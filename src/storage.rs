//! Explicit screenshot persistence and XDG Pictures resolution.

use chrono::{Duration, Local};
use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const USER_DIRS_FILENAME: &str = "user-dirs.dirs";
const SCREENSHOTS_DIRECTORY: &str = "Screenshots";
const MAX_FILENAME_ATTEMPTS: u32 = 1_000_000;
const TEMPORARY_FILENAME_ATTEMPTS: u32 = 128;
const ESCAPED_DOLLAR: char = '\u{e000}';

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Save an image using an optional user-configured output directory.
///
/// A configured value is interpreted without shell expansion: `~/Downloads`
/// is resolved against `HOME`, while absolute and relative paths are used as
/// supplied.  An empty or invalid home-relative value falls back to the
/// historical XDG Pictures/Screenshots directory.
pub fn save_image_with_directory(
    image: &crate::image::RgbaImage,
    configured_directory: Option<&str>,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    save_image_in(&screenshot_directory_for(configured_directory), image)
}

/// Write an image to a private temporary PNG for an external viewer.
///
/// The caller owns the returned path and is responsible for removing it after
/// the viewer no longer needs it.  The file is created with exclusive access
/// so a concurrent capture cannot reuse the same name.
pub fn save_temporary_image(
    image: &crate::image::RgbaImage,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    // Encode before touching the filesystem so invalid image data cannot leave
    // an empty temporary file behind.
    let encoded = image.to_png()?;
    let directory = env::temp_dir();

    for _ in 0..TEMPORARY_FILENAME_ATTEMPTS {
        let sequence = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let filename = format!(".snipchord-preview-{}-{}.png", std::process::id(), sequence);
        let destination = directory.join(filename);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = match options.open(&destination) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let result = (|| {
            file.write_all(&encoded)?;
            file.sync_all()
        })();
        drop(file);

        if let Err(error) = result {
            let _ = fs::remove_file(&destination);
            return Err(error.into());
        }
        return Ok(destination);
    }

    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique temporary preview filename",
    )
    .into())
}

/// Return the directory used for saved screenshots without creating it,
/// honoring an optional configured output directory.
pub fn screenshot_directory_for(configured_directory: Option<&str>) -> PathBuf {
    if let Some(directory) = configured_directory.and_then(resolve_configured_directory) {
        return directory;
    }

    let home = env::var_os("HOME").map(PathBuf::from);
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".config")));

    let pictures = config_home
        .and_then(|root| read_pictures_directory(&user_dirs_path(&root), home.as_deref()))
        .or_else(|| home.map(|path| path.join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("Pictures"));

    pictures.join(SCREENSHOTS_DIRECTORY)
}

fn resolve_configured_directory(value: &str) -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    resolve_configured_directory_from(value, home.as_deref())
}

fn resolve_configured_directory_from(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if value == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    if value.starts_with('~') {
        // Do not guess at ~user or otherwise invoke shell-like expansion.
        return None;
    }

    Some(PathBuf::from(value))
}

/// Save an image in an explicit directory, creating that directory on demand.
pub fn save_image_in(
    directory: &Path,
    image: &crate::image::RgbaImage,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    // Encode before touching the filesystem so an invalid image cannot create
    // an empty directory or file.
    let encoded = image.to_png()?;
    fs::create_dir_all(directory)?;

    let first_timestamp = Local::now();
    for offset in 0..MAX_FILENAME_ATTEMPTS {
        let timestamp = first_timestamp + Duration::microseconds(i64::from(offset));
        let filename = format!(
            "Screenshot {}.{:06}.png",
            timestamp.format("%Y-%m-%d at %H.%M.%S"),
            timestamp.timestamp_subsec_micros()
        );
        let destination = directory.join(filename);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };

        let result = (|| {
            file.write_all(&encoded)?;
            file.sync_all()
        })();
        drop(file);

        if let Err(error) = result {
            let _ = fs::remove_file(&destination);
            return Err(error.into());
        }
        return Ok(destination);
    }

    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique screenshot filename",
    )
    .into())
}

fn user_dirs_path(config_home: &Path) -> PathBuf {
    config_home.join(USER_DIRS_FILENAME)
}

fn read_pictures_directory(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .find_map(|line| parse_pictures_line(line, home))
}

fn parse_pictures_line(line: &str, home: Option<&Path>) -> Option<PathBuf> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, raw_value) = line.split_once('=')?;
    if key.trim() != "XDG_PICTURES_DIR" {
        return None;
    }

    let value = parse_user_dirs_value(raw_value)?;
    let path = expand_home(&value, home)?;
    path.is_absolute().then_some(path)
}

fn expand_home(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = value.strip_prefix("${HOME}") {
        if (rest.is_empty() || rest.starts_with('/')) && !rest.contains('$') {
            return home
                .map(|path| path.join(restore_escaped(rest.trim_start_matches('/'))))
                .filter(|path| path.is_absolute());
        }
    }
    if let Some(rest) = value.strip_prefix("$HOME") {
        if (rest.is_empty() || rest.starts_with('/')) && !rest.contains('$') {
            return home
                .map(|path| path.join(restore_escaped(rest.trim_start_matches('/'))))
                .filter(|path| path.is_absolute());
        }
    }
    if value.contains('$') {
        return None;
    }
    Some(PathBuf::from(restore_escaped(value)))
}

fn parse_user_dirs_value(raw_value: &str) -> Option<String> {
    let value = raw_value.trim();
    if let Some(value) = value.strip_prefix('"') {
        let mut decoded = String::new();
        let mut escaped = false;
        let mut closing_quote = None;

        for (index, character) in value.char_indices() {
            if escaped {
                match character {
                    '"' | '\\' | '\x60' => decoded.push(character),
                    '$' => decoded.push(ESCAPED_DOLLAR),
                    _ => {
                        decoded.push('\\');
                        decoded.push(character);
                    }
                }
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                closing_quote = Some(index);
                break;
            } else {
                decoded.push(character);
            }
        }

        let closing_quote = closing_quote?;
        if escaped {
            return None;
        }
        let trailing = &value[closing_quote + 1..];
        if !trailing.trim().is_empty() && !trailing.trim_start().starts_with('#') {
            return None;
        }
        return (!decoded.is_empty()).then_some(decoded);
    }

    let value = value.split('#').next()?.trim();
    (!value.is_empty()).then_some(value.to_owned())
}

fn restore_escaped(value: &str) -> String {
    value.replace(ESCAPED_DOLLAR, "$")
}

#[cfg(test)]
mod tests {
    use super::{
        parse_pictures_line, read_pictures_directory, resolve_configured_directory_from,
        save_image_in, save_image_with_directory, save_temporary_image, screenshot_directory_for,
    };
    use crate::image::RgbaImage;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            for _ in 0..128 {
                let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "snipchord-storage-test-{}-{}",
                    std::process::id(),
                    sequence
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create temporary test directory: {error}"),
                }
            }
            panic!("could not allocate a temporary test directory");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_image() -> RgbaImage {
        RgbaImage::solid(2, 2, [0x35, 0xa7, 0xe8]).expect("create sample image")
    }

    #[test]
    fn expands_home_in_user_dirs_file() {
        assert_eq!(
            parse_pictures_line(
                r#"XDG_PICTURES_DIR="$HOME/Pictures""#,
                Some(Path::new("/home/test"))
            ),
            Some(PathBuf::from("/home/test/Pictures"))
        );
        assert_eq!(
            parse_pictures_line(
                r#"XDG_PICTURES_DIR="${HOME}/Screens""#,
                Some(Path::new("/home/test"))
            ),
            Some(PathBuf::from("/home/test/Screens"))
        );
    }

    #[test]
    fn rejects_relative_or_unresolved_picture_paths() {
        assert_eq!(
            parse_pictures_line(
                "XDG_PICTURES_DIR=\"Pictures\"",
                Some(Path::new("/home/test"))
            ),
            None
        );
        assert_eq!(
            parse_pictures_line("XDG_PICTURES_DIR=\"$HOME/Pictures\"", None),
            None
        );
        assert_eq!(
            parse_pictures_line("XDG_PICTURES_DIR=\"/tmp/$PICTURES\"", None),
            None
        );
        assert_eq!(
            parse_pictures_line("XDG_PICTURES_DIR=\"/home/test/Pictures\" trailing", None),
            None
        );
    }

    #[test]
    fn decodes_standard_user_dirs_escapes_without_shell_execution() {
        assert_eq!(
            parse_pictures_line(r#"XDG_PICTURES_DIR="/home/test/My\"Pictures""#, None),
            Some(PathBuf::from("/home/test/My\"Pictures"))
        );
        assert_eq!(
            parse_pictures_line(r#"XDG_PICTURES_DIR="/home/test/My\\Pictures""#, None),
            Some(PathBuf::from("/home/test/My\\Pictures"))
        );
        assert_eq!(
            parse_pictures_line(r#"XDG_PICTURES_DIR="/home/test/Cash\$Pictures""#, None),
            Some(PathBuf::from("/home/test/Cash$Pictures"))
        );
        let escaped_backtick = format!("XDG_PICTURES_DIR=\"/home/test/Tick\\{}Pictures\"", '\x60');
        assert_eq!(
            parse_pictures_line(&escaped_backtick, None),
            Some(PathBuf::from(format!("/home/test/Tick{}Pictures", '\x60')))
        );
    }

    #[test]
    fn reads_only_the_pictures_assignment() {
        let directory = TempDirectory::new();
        let config = directory.path().join("user-dirs.dirs");
        fs::write(
            &config,
            "# generated\nXDG_DOCUMENTS_DIR=\"$HOME/Documents\"\nXDG_PICTURES_DIR=\"$HOME/Images\"\n",
        )
        .expect("write user dirs");

        assert_eq!(
            read_pictures_directory(&config, Some(Path::new("/home/test"))),
            Some(PathBuf::from("/home/test/Images"))
        );
    }

    #[test]
    fn resolves_configured_output_directory_without_shell_expansion() {
        let home = Path::new("/home/test");
        assert_eq!(
            resolve_configured_directory_from("~/Downloads", Some(home)),
            Some(PathBuf::from("/home/test/Downloads"))
        );
        assert_eq!(
            resolve_configured_directory_from("/tmp/snips", Some(home)),
            Some(PathBuf::from("/tmp/snips"))
        );
        assert_eq!(
            resolve_configured_directory_from("relative/snips", Some(home)),
            Some(PathBuf::from("relative/snips"))
        );
        assert_eq!(
            resolve_configured_directory_from("~other/snips", Some(home)),
            None
        );
        assert_eq!(resolve_configured_directory_from("~/Downloads", None), None);
        assert_eq!(
            screenshot_directory_for(Some("   ")),
            screenshot_directory_for(None)
        );
    }

    #[test]
    fn configured_output_directory_is_used_for_saves() {
        let directory = TempDirectory::new();
        let configured = directory.path().join("Downloads");
        let configured_text = configured.to_str().expect("configured path is UTF-8");

        let saved = save_image_with_directory(&sample_image(), Some(configured_text))
            .expect("save configured image");
        assert_eq!(saved.parent(), Some(configured.as_path()));
        assert!(saved.is_file());
    }

    #[test]
    fn explicit_save_writes_png_and_keeps_unique_names() {
        let directory = TempDirectory::new();
        let save_directory = directory.path().join("Screenshots");
        let image = sample_image();

        let first = save_image_in(&save_directory, &image).expect("save first image");
        let second = save_image_in(&save_directory, &image).expect("save second image");

        assert_ne!(first, second);
        assert_eq!(
            fs::read(&first).expect("read first")[..8],
            [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        );
        assert_eq!(
            fs::read_dir(&save_directory)
                .expect("list screenshots")
                .count(),
            2
        );
        assert!(first
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let timestamp = name
                    .strip_prefix("Screenshot ")
                    .and_then(|name| name.strip_suffix(".png"));
                timestamp.is_some_and(|timestamp| timestamp.len() == 29)
            }));
    }

    #[test]
    fn temporary_preview_writes_private_png() {
        let image = sample_image();
        let path = save_temporary_image(&image).expect("save temporary preview");

        assert!(path.starts_with(std::env::temp_dir()));
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".snipchord-preview-")));
        assert_eq!(
            fs::read(&path).expect("read temporary preview")[..8],
            [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        );

        fs::remove_file(path).expect("remove temporary preview");
    }
}
