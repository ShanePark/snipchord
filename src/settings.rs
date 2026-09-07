//! Small, atomic, per-user preferences.
//!
//! Reading settings never creates the configuration directory.  Writes use a
//! same-directory temporary file followed by an atomic rename so a process
//! interrupted during serialization cannot leave a partial JSON document at
//! the settings path.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const SETTINGS_DIRECTORY: &str = "snipchord";
const SETTINGS_FILENAME: &str = "settings.json";

/// User preferences persisted by SnipChord.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Settings {
    pub save_automatically: bool,
    pub show_preview: bool,
    /// Optional directory for saved screenshots.  `None` preserves the
    /// historical XDG Pictures/Screenshots resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_directory: Option<String>,
}

/// The defaults used when the settings file is absent or invalid.
pub const DEFAULTS: Settings = Settings {
    save_automatically: false,
    show_preview: true,
    output_directory: None,
};

impl Default for Settings {
    fn default() -> Self {
        DEFAULTS
    }
}

/// Resolve the user's settings file using the XDG configuration convention.
pub fn config_path() -> PathBuf {
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config_path_for(config_home)
}

/// Build a settings path below an explicit XDG configuration directory.
pub fn config_path_for(config_home: impl AsRef<Path>) -> PathBuf {
    config_home
        .as_ref()
        .join(SETTINGS_DIRECTORY)
        .join(SETTINGS_FILENAME)
}

/// Read the user's settings, falling back to [`DEFAULTS`] on any read or
/// parse error.
pub fn read_settings() -> Settings {
    read_settings_from(config_path())
}

/// Read settings from an explicit path, preserving known boolean values and
/// ignoring unknown or incorrectly typed options.
pub fn read_settings_from(path: impl AsRef<Path>) -> Settings {
    try_read_settings_from(path).unwrap_or_default()
}

/// Fallible counterpart to [`read_settings_from`], useful when a caller needs
/// to distinguish a missing/corrupt file from a valid settings document.
pub fn try_read_settings_from(path: impl AsRef<Path>) -> io::Result<Settings> {
    let raw = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;

    let mut settings = Settings::default();
    if let Some(object) = value.as_object() {
        if let Some(value) = object.get("save_automatically").and_then(Value::as_bool) {
            settings.save_automatically = value;
        }
        if let Some(value) = object.get("show_preview").and_then(Value::as_bool) {
            settings.show_preview = value;
        }
        if let Some(value) = object.get("output_directory").and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                settings.output_directory = Some(value.to_owned());
            }
        }
    }
    Ok(settings)
}

/// Atomically write the user's settings file.
pub fn write_settings(settings: &Settings) -> io::Result<()> {
    write_settings_to(config_path(), settings)
}

/// Atomically write settings to an explicit path, creating its parent
/// directory when necessary.
pub fn write_settings_to(path: impl AsRef<Path>, settings: &Settings) -> io::Result<()> {
    let target = path.as_ref();
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let encoded = serde_json::to_string_pretty(settings)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
    let (mut temporary, temporary_path) = create_temporary_file(parent)?;

    let result = (|| {
        temporary.write_all(encoded.as_bytes())?;
        temporary.write_all(b"\n")?;
        temporary.sync_all()?;
        // Closing before rename also keeps this function portable to systems
        // that do not allow replacing an open file.
        drop(temporary);
        fs::rename(&temporary_path, target)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

fn create_temporary_file(parent: &Path) -> io::Result<(File, PathBuf)> {
    for _ in 0..128 {
        let sequence = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".settings-{}-{}.tmp", std::process::id(), sequence);
        let path = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique temporary settings file",
    ))
}

#[cfg(test)]
mod tests {
    use super::{config_path_for, read_settings_from, write_settings_to, Settings, DEFAULTS};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            for _ in 0..128 {
                let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "snipchord-settings-test-{}-{}",
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

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_match_python_preferences() {
        assert_eq!(
            Settings::default(),
            Settings {
                save_automatically: false,
                show_preview: true,
                output_directory: None,
            }
        );
        assert_eq!(Settings::default(), DEFAULTS);
    }

    #[test]
    fn config_path_uses_the_xdg_root() {
        assert_eq!(
            config_path_for("/tmp/example-config"),
            PathBuf::from("/tmp/example-config/snipchord/settings.json")
        );
    }

    #[test]
    fn missing_settings_fall_back_without_creating_files() {
        let directory = TempDirectory::new();
        let path = directory.path().join("nested/settings.json");
        assert_eq!(read_settings_from(&path), DEFAULTS);
        assert!(!path.exists());
        assert!(!path.parent().expect("settings parent").exists());
    }

    #[test]
    fn round_trip_creates_parent_and_writes_pretty_json() {
        let directory = TempDirectory::new();
        let path = directory.path().join("nested/settings.json");
        let expected = Settings {
            save_automatically: true,
            show_preview: false,
            output_directory: None,
        };

        write_settings_to(&path, &expected).expect("write settings");
        assert_eq!(read_settings_from(&path), expected);
        assert_eq!(
            fs::read_to_string(&path).expect("read settings"),
            "{\n  \"save_automatically\": true,\n  \"show_preview\": false\n}\n"
        );
        assert_eq!(
            fs::read_dir(path.parent().expect("settings parent"))
                .expect("list settings")
                .count(),
            1
        );
    }

    #[test]
    fn corrupt_or_non_object_settings_fall_back() {
        let directory = TempDirectory::new();
        let path = directory.path().join("settings.json");

        fs::write(&path, "not JSON").expect("write corrupt settings");
        assert_eq!(read_settings_from(&path), DEFAULTS);

        fs::write(&path, "[]").expect("write non-object settings");
        assert_eq!(read_settings_from(&path), DEFAULTS);
    }

    #[test]
    fn only_boolean_known_options_are_loaded() {
        let directory = TempDirectory::new();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{"show_preview":"false","save_automatically":true,"extra":1}"#,
        )
        .expect("write settings");

        assert_eq!(
            read_settings_from(&path),
            Settings {
                save_automatically: true,
                show_preview: true,
                output_directory: None,
            }
        );
    }

    #[test]
    fn configured_output_directory_round_trips_and_ignores_blank_values() {
        let directory = TempDirectory::new();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{"output_directory":" ~/Downloads ","show_preview":false}"#,
        )
        .expect("write settings");

        let loaded = read_settings_from(&path);
        assert_eq!(loaded.output_directory.as_deref(), Some("~/Downloads"));
        assert!(!loaded.show_preview);

        let empty_path = directory.path().join("empty.json");
        fs::write(&empty_path, r#"{"output_directory":"   "}"#)
            .expect("write empty output directory");
        assert_eq!(read_settings_from(&empty_path).output_directory, None);

        write_settings_to(&empty_path, &loaded).expect("write configured settings");
        assert!(fs::read_to_string(&empty_path)
            .expect("read configured settings")
            .contains("\"output_directory\": \"~/Downloads\""));
    }
}
