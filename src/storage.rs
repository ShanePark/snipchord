//! Explicit screenshot persistence and XDG Pictures resolution.

use chrono::{Duration, Local};
use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const USER_DIRS_FILENAME: &str = "user-dirs.dirs";
const SCREENSHOTS_DIRECTORY: &str = "Screenshots";
const MAX_FILENAME_ATTEMPTS: u32 = 1_000_000;
const TEMPORARY_FILENAME_ATTEMPTS: u32 = 128;
const PREVIEW_CACHE_LIMIT: usize = 5;
const PREVIEW_CACHE_DIRECTORY: &str = "snipchord/previews";
const PREVIEW_FILENAME_PREFIX: &str = ".snipchord-preview-";
const PREVIEW_STAGING_SUFFIX: &str = ".tmp";
const ESCAPED_DOLLAR: char = '\u{e000}';

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A complete PNG that is waiting for the application to publish it into the
/// preview cache.  The staging file is kept in the same directory as the
/// destination so publication can use an atomic rename.
pub struct PreparedPreview {
    staging_path: PathBuf,
}

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

/// Return the private cache directory used for thumbnail previews.
///
/// The directory follows XDG cache conventions and is created with mode 0700
/// by the first writer.  It contains only SnipChord-owned preview files; the
/// user's configured screenshot directory is never used for this cache.
pub fn preview_cache_directory() -> PathBuf {
    let cache_home = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".cache"))
        })
        .unwrap_or_else(|| env::temp_dir().join("snipchord"));
    cache_home.join(PREVIEW_CACHE_DIRECTORY)
}

/// Encode an image into a private, unpublished preview file in the cache.
///
/// The worker may perform this operation away from the X11 event loop.  The
/// returned staging file is not visible to preview consumers until
/// [`publish_temporary_image`] atomically renames it to its final `.png`
/// name.
pub fn prepare_temporary_image(
    image: &crate::image::RgbaImage,
) -> Result<PreparedPreview, Box<dyn Error + Send + Sync>> {
    let directory = ensure_preview_cache_directory()?;
    prepare_temporary_image_in(&directory, image)
}

/// Publish a prepared preview and return its stable cache path.
pub fn publish_temporary_image(
    prepared: PreparedPreview,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    let directory = prepared.staging_path.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "preview staging path has no parent",
        )
    })?;
    let destination = allocate_preview_destination(directory)?;
    let result = fs::rename(&prepared.staging_path, &destination);
    if let Err(error) = result {
        let _ = fs::remove_file(&prepared.staging_path);
        return Err(error.into());
    }
    Ok(destination)
}

/// Remove stale staging files and retain only the five newest published
/// SnipChord preview PNGs.  This is intended for startup, before any new
/// worker is launched.
pub fn cleanup_preview_cache() -> Result<(), Box<dyn Error + Send + Sync>> {
    let directory = ensure_preview_cache_directory()?;
    remove_staging_files(&directory)?;
    rotate_preview_cache_in(&directory)
}

/// Rotate already-published preview PNGs after a new file is published.
/// Staging files are deliberately left alone because other preparation
/// workers may still be writing them.
pub fn rotate_preview_cache() -> Result<(), Box<dyn Error + Send + Sync>> {
    let directory = ensure_preview_cache_directory()?;
    rotate_preview_cache_in(&directory)
}

fn ensure_preview_cache_directory() -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    let directory = preview_cache_directory();
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

fn prepare_temporary_image_in(
    directory: &Path,
    image: &crate::image::RgbaImage,
) -> Result<PreparedPreview, Box<dyn Error + Send + Sync>> {
    // Encode before touching the filesystem so invalid image data cannot leave
    // an empty staging file behind.
    let encoded = image.to_png()?;

    for _ in 0..TEMPORARY_FILENAME_ATTEMPTS {
        let sequence = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staging = directory.join(format!(
            "{PREVIEW_FILENAME_PREFIX}stage-{}-{sequence}{PREVIEW_STAGING_SUFFIX}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = match options.open(&staging) {
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
            let _ = fs::remove_file(&staging);
            return Err(error.into());
        }
        return Ok(PreparedPreview {
            staging_path: staging,
        });
    }

    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique temporary preview filename",
    )
    .into())
}

fn allocate_preview_destination(directory: &Path) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    for _ in 0..TEMPORARY_FILENAME_ATTEMPTS {
        let sequence = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let destination = directory.join(format!(
            "{PREVIEW_FILENAME_PREFIX}{timestamp:020}-{}-{sequence}.png",
            std::process::id()
        ));
        if !destination.exists() {
            return Ok(destination);
        }
    }
    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique published preview filename",
    )
    .into())
}

fn remove_staging_files(directory: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let is_staging = entry.file_type()?.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(PREVIEW_FILENAME_PREFIX)
                        && name.ends_with(PREVIEW_STAGING_SUFFIX)
                });
        if is_staging {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn rotate_preview_cache_in(directory: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut published = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let is_preview = entry.file_type()?.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(PREVIEW_FILENAME_PREFIX) && name.ends_with(".png")
                });
        if is_preview {
            published.push(path);
        }
    }

    // The filename starts with a fixed-width nanosecond timestamp and a
    // per-process counter, so lexical order is publication order even when
    // metadata timestamp precision is coarse.
    published.sort_unstable();
    let remove_count = published.len().saturating_sub(PREVIEW_CACHE_LIMIT);
    for path in published.into_iter().take(remove_count) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
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
        parse_pictures_line, prepare_temporary_image_in, publish_temporary_image,
        read_pictures_directory, resolve_configured_directory_from, rotate_preview_cache_in,
        save_image_in, save_image_with_directory, screenshot_directory_for,
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
        let directory = TempDirectory::new();
        let prepared = prepare_temporary_image_in(directory.path(), &image)
            .expect("prepare temporary preview");
        assert!(prepared.staging_path.is_file());
        let path = publish_temporary_image(prepared).expect("publish temporary preview");

        assert!(path.starts_with(directory.path()));
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".snipchord-preview-")));
        assert_eq!(
            fs::read(&path).expect("read temporary preview")[..8],
            [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        fs::remove_file(path).expect("remove temporary preview");
    }

    #[test]
    fn preview_rotation_keeps_only_the_newest_five_owned_pngs() {
        let directory = TempDirectory::new();
        for index in 0..7 {
            fs::write(
                directory.path().join(format!(
                    ".snipchord-preview-000000000000000000{index}-test.png"
                )),
                [0x89, b'P', b'N', b'G'],
            )
            .expect("write preview fixture");
        }
        fs::write(
            directory.path().join("unrelated.png"),
            [0x89, b'P', b'N', b'G'],
        )
        .expect("write unrelated fixture");

        rotate_preview_cache_in(directory.path()).expect("rotate preview cache");

        let mut remaining: Vec<_> = fs::read_dir(directory.path())
            .expect("read preview cache")
            .map(|entry| entry.expect("read preview entry").file_name())
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), 6);
        assert!(remaining.iter().any(|name| name == "unrelated.png"));
        for index in 0..2 {
            assert!(!remaining.iter().any(|name| {
                name.to_string_lossy()
                    .contains(&format!("000000000000000000{index}-test"))
            }));
        }
        for index in 2..7 {
            assert!(remaining.iter().any(|name| {
                name.to_string_lossy()
                    .contains(&format!("000000000000000000{index}-test"))
            }));
        }
    }
}
