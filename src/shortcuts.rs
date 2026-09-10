//! Read-only descriptions of the keyboard shortcuts managed by the installer.
//!
//! SnipChord does not own a shortcut daemon.  When the optional GNOME setup
//! has been used, the bindings live in the desktop's custom-keybinding
//! settings, so Preferences reads those entries for display.  A missing
//! `gsettings` service (for example in a non-GNOME session) falls back to the
//! same defaults used by `tools/install.py`.

use std::path::Path;
use std::process::Command;

const MEDIA_KEYS_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const CUSTOM_KEYBINDINGS_KEY: &str = "custom-keybindings";
const CUSTOM_KEYBINDING_SCHEMA: &str =
    "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";

/// One shortcut shown in the read-only Preferences list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShortcutBinding {
    /// Stable label for the action represented by the binding.
    pub name: &'static str,
    /// Human-readable key combination, for example `Ctrl+Alt+Shift+4`.
    pub binding: String,
    /// Short action description shown beside the key combination.
    pub action: &'static str,
    /// Whether this value came from the desktop's current settings or the
    /// installer defaults.
    pub source: ShortcutSource,
}

/// Indicates where a shortcut value came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutSource {
    Configured,
    Default,
    Unassigned,
}

/// The shortcut snapshot loaded when Preferences opens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShortcutList {
    pub bindings: Vec<ShortcutBinding>,
    /// True when the desktop settings service could not be read and the
    /// displayed values are the installer's fallback defaults.
    pub using_defaults: bool,
}

struct ShortcutDefinition {
    name: &'static str,
    action: &'static str,
    arguments: &'static [&'static str],
    default_binding: &'static str,
}

const DEFINITIONS: &[ShortcutDefinition] = &[
    ShortcutDefinition {
        name: "Region to clipboard",
        action: "Capture a selected region and copy it",
        arguments: &["--region", "--clipboard"],
        default_binding: "<Control><Alt><Shift>4",
    },
    ShortcutDefinition {
        name: "Region to file",
        action: "Capture a selected region and save it",
        arguments: &["--region", "--save"],
        default_binding: "<Alt><Shift>4",
    },
    ShortcutDefinition {
        name: "Full desktop to clipboard",
        action: "Capture the full desktop and copy it",
        arguments: &["--fullscreen", "--clipboard"],
        default_binding: "<Control><Alt><Shift>3",
    },
    ShortcutDefinition {
        name: "Full desktop to file",
        action: "Capture the full desktop and save it",
        arguments: &["--fullscreen", "--save"],
        default_binding: "<Alt><Shift>3",
    },
];

/// Read the installed SnipChord bindings when GNOME settings are available.
///
/// The installer identifies its four entries by their exact executable
/// arguments.  That lets this function show a binding changed in GNOME's
/// settings while ignoring unrelated custom shortcuts.  If the settings
/// service is unavailable, the installer defaults are returned and marked as
/// such.  If the service is readable but an action has no managed entry, it
/// is shown as unassigned instead of implying that the default is active.
pub fn configured_shortcuts() -> ShortcutList {
    let Some(configured) = read_configured_shortcuts() else {
        return ShortcutList {
            bindings: DEFINITIONS
                .iter()
                .map(|definition| ShortcutBinding {
                    name: definition.name,
                    binding: format_binding(definition.default_binding),
                    action: definition.action,
                    source: ShortcutSource::Default,
                })
                .collect(),
            using_defaults: true,
        };
    };

    ShortcutList {
        bindings: DEFINITIONS
            .iter()
            .map(|definition| {
                configured
                    .iter()
                    .find(|(arguments, _)| *arguments == definition.arguments)
                    .and_then(|(_, binding)| {
                        Some(ShortcutBinding {
                            name: definition.name,
                            binding: format_binding_for_display(binding)?,
                            action: definition.action,
                            source: ShortcutSource::Configured,
                        })
                    })
                    .unwrap_or(ShortcutBinding {
                        name: definition.name,
                        binding: "Not assigned".to_owned(),
                        action: definition.action,
                        source: ShortcutSource::Unassigned,
                    })
            })
            .collect(),
        using_defaults: false,
    }
}

fn read_configured_shortcuts() -> Option<Vec<(&'static [&'static str], String)>> {
    Some(
        configured_capture_shortcuts()?
            .into_iter()
            .filter_map(|(arguments, binding)| {
                definition_for(&arguments).map(|definition| (definition.arguments, binding))
            })
            .collect(),
    )
}

/// Read the exact commands managed by the installer together with their
/// current GNOME binding strings.
///
/// Preferences only needs the definition that a command represents.  The raw
/// listener also needs to distinguish the legacy `--region` command from the
/// newer explicit `--region --clipboard` command, so this crate-visible view
/// deliberately keeps the command arguments as configured.
pub(crate) fn configured_capture_shortcuts() -> Option<Vec<(Vec<String>, String)>> {
    let raw_paths = gsettings_get(MEDIA_KEYS_SCHEMA, CUSTOM_KEYBINDINGS_KEY)?;
    let paths = parse_gvariant_strings(&raw_paths)?;
    let mut configured = Vec::new();

    for path in paths {
        let schema = format!("{CUSTOM_KEYBINDING_SCHEMA}:{path}");
        let command_value = gsettings_get(&schema, "command")?;
        let command = parse_gvariant_strings(&command_value)?.into_iter().next()?;
        let Some(arguments) = command_arguments(&command) else {
            continue;
        };
        if definition_for(&arguments).is_none() {
            continue;
        }
        let binding_value = gsettings_get(&schema, "binding")?;
        let binding = parse_gvariant_strings(&binding_value)?.into_iter().next()?;
        configured.push((arguments, binding));
    }
    Some(configured)
}

fn definition_for(arguments: &[String]) -> Option<&'static ShortcutDefinition> {
    DEFINITIONS.iter().find(|definition| {
        if definition.arguments == arguments {
            return true;
        }
        let legacy_arguments = match definition.name {
            "Region to clipboard" => ["--region"].as_slice(),
            "Full desktop to clipboard" => ["--fullscreen"].as_slice(),
            _ => return false,
        };
        arguments == legacy_arguments
    })
}

fn gsettings_get(schema: &str, key: &str) -> Option<String> {
    let output = Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn command_arguments(command: &str) -> Option<Vec<String>> {
    let parts = shell_words(command)?;
    if parts.len() < 2 || Path::new(&parts[0]).file_name()? != "snipchord" {
        return None;
    }
    Some(parts[1..].to_vec())
}

/// Parse the small shell subset produced by `tools/install.py`'s `shlex.quote`.
/// This handles quoted executable paths and arguments without invoking a shell.
fn shell_words(value: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut had_content = false;

    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            had_content = true;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    current.push(character);
                    had_content = true;
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    escaped = true;
                } else {
                    current.push(character);
                    had_content = true;
                }
            }
            Some(_) => unreachable!(),
            None => match character {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(character),
                character if character.is_whitespace() => {
                    if had_content {
                        words.push(std::mem::take(&mut current));
                        had_content = false;
                    }
                }
                character => {
                    current.push(character);
                    had_content = true;
                }
            },
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if had_content {
        words.push(current);
    }
    Some(words)
}

/// Parse a GVariant string or string-array using the subset emitted by
/// `gsettings`.  Values are single-quoted and use backslash escapes.
fn parse_gvariant_strings(value: &str) -> Option<Vec<String>> {
    let value = value.trim().strip_prefix("@as ").unwrap_or(value).trim();
    if value == "[]" {
        return Some(Vec::new());
    }
    let mut strings = Vec::new();
    let mut chars = value.chars().peekable();
    let mut saw_malformed = false;
    while let Some(character) = chars.next() {
        if character != '\'' && character != '"' {
            continue;
        }
        let quote = character;
        let mut current = String::new();
        let mut escaped = false;
        let mut closed = false;
        for character in chars.by_ref() {
            if escaped {
                current.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote {
                closed = true;
                break;
            } else {
                current.push(character);
            }
        }
        if closed && !escaped {
            strings.push(current);
        } else {
            saw_malformed = true;
            break;
        }
    }
    if saw_malformed || (strings.is_empty() && !value.is_empty()) {
        None
    } else {
        Some(strings)
    }
}

fn format_binding(binding: &str) -> String {
    let mut labels = Vec::new();
    let mut rest = binding;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start + 1..].find('>') else {
            break;
        };
        let end = start + 1 + end;
        let label = match &rest[start + 1..end] {
            "Primary" | "Control" => "Ctrl",
            "Shift" => "Shift",
            "Alt" => "Alt",
            "Super" => "Super",
            modifier => modifier,
        };
        labels.push(label.to_owned());
        rest = &rest[end + 1..];
    }
    if !rest.is_empty() {
        labels.push(match rest {
            "numbersign" => "#".to_owned(),
            "percent" => "%".to_owned(),
            "space" => "Space".to_owned(),
            key => key.to_owned(),
        });
    }
    if labels.is_empty() {
        binding.to_owned()
    } else {
        labels.join("+")
    }
}

fn format_binding_for_display(binding: &str) -> Option<String> {
    let formatted = format_binding(binding);
    (!formatted.trim().is_empty()).then_some(formatted)
}

#[cfg(test)]
mod tests {
    use super::{
        command_arguments, format_binding, format_binding_for_display, parse_gvariant_strings,
        shell_words,
    };

    #[test]
    fn parses_gvariant_string_arrays() {
        assert_eq!(
            parse_gvariant_strings("['/one/', '/two/']"),
            Some(vec!["/one/".to_owned(), "/two/".to_owned()])
        );
        assert_eq!(
            parse_gvariant_strings("'Ctrl+4'"),
            Some(vec!["Ctrl+4".to_owned()])
        );
        assert_eq!(
            parse_gvariant_strings("[\"/one/\", \"/two/\"]"),
            Some(vec!["/one/".to_owned(), "/two/".to_owned()])
        );
        assert_eq!(parse_gvariant_strings("@as []"), Some(Vec::new()));
        assert_eq!(parse_gvariant_strings("['unterminated"), None);
    }

    #[test]
    fn parses_installer_commands_without_a_shell() {
        assert_eq!(
            command_arguments("'/home/shane/.local/bin/snipchord' --region --clipboard"),
            Some(vec!["--region".to_owned(), "--clipboard".to_owned()])
        );
        assert_eq!(
            command_arguments("/usr/bin/other --region --clipboard"),
            None
        );
        assert_eq!(
            shell_words("'/tmp/snip chord/snipchord' --region --save"),
            Some(vec![
                "/tmp/snip chord/snipchord".to_owned(),
                "--region".to_owned(),
                "--save".to_owned()
            ])
        );
    }

    #[test]
    fn formats_gnome_bindings_for_display() {
        assert_eq!(format_binding("<Control><Alt><Shift>4"), "Ctrl+Alt+Shift+4");
        assert_eq!(
            format_binding("<Primary><Alt><Shift>numbersign"),
            "Ctrl+Alt+Shift+#"
        );
    }

    #[test]
    fn empty_binding_is_treated_as_unassigned() {
        assert_eq!(format_binding_for_display(""), None);
        assert_eq!(
            format_binding_for_display("<Control><Alt><Shift>4"),
            Some("Ctrl+Alt+Shift+4".to_owned())
        );
    }
}
