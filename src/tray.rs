//! StatusNotifierItem support for the resident application.
//!
//! The capture UI owns the X11 connection and must keep its event loop on the
//! main thread.  The tray service therefore runs in ksni's own thread and
//! invokes the existing command-line entry point for menu actions.  Those
//! invocations already know how to address the resident X11 instance, so tray
//! callbacks stay short and do not need to share X11 state with this module.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;

/// Actions exposed by the status-icon menu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Preferences,
    About,
    Quit,
}

impl TrayAction {
    /// The existing command-line arguments understood by the resident
    /// singleton.  Keeping these in one table makes menu wiring testable and
    /// prevents a second command protocol from growing in the tray module.
    pub const fn args(self) -> &'static [&'static str] {
        match self {
            Self::Preferences => &["--preferences"],
            Self::About => &["--about"],
            Self::Quit => &["--quit"],
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Preferences => "Preferences",
            Self::About => "About SnipChord",
            Self::Quit => "Quit SnipChord",
        }
    }
}

const MENU_ACTIONS: &[TrayAction] = &[TrayAction::Preferences, TrayAction::About, TrayAction::Quit];

/// A non-blocking owner for the background D-Bus startup and service handle.
pub struct TrayRuntime {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TrayRuntime {
    /// Start tray registration without delaying the initial capture command.
    ///
    /// D-Bus may be unavailable in a headless invocation or while the desktop
    /// session is still starting.  Either case is non-fatal to screenshot
    /// capture, so startup errors are reported and the caller continues.
    pub fn start() -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let service_thread = match thread::Builder::new()
            .name("snipchord-tray-startup".to_owned())
            .spawn(move || {
                let tray = (|| {
                    let executable = env::current_exe()
                        .map_err(|error| format!("could not locate executable: {error}"))?;
                    Ok::<_, String>(SnipChordTray::new(executable))
                })();
                let tray = match tray {
                    Ok(tray) => tray,
                    Err(error) => {
                        eprintln!("snipchord: tray unavailable: {error}");
                        return;
                    }
                };
                let handle = match tray
                    .assume_sni_available(true)
                    .spawn()
                    .map_err(|error| error.to_string())
                {
                    Ok(handle) => handle,
                    Err(error) => {
                        eprintln!("snipchord: tray unavailable: {error}");
                        return;
                    }
                };

                // Keep the handle on this worker so the main event loop never
                // waits for a D-Bus response during startup or shutdown.
                while !worker_stop.load(Ordering::Acquire) {
                    thread::park();
                }
                handle.shutdown().wait();
            }) {
            Ok(thread) => thread,
            Err(error) => {
                eprintln!("snipchord: could not start tray thread: {error}");
                return None;
            }
        };

        Some(Self {
            stop,
            thread: Some(service_thread),
        })
    }

    /// Ask the service thread to stop without waiting on D-Bus.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            // Dropping a JoinHandle detaches the worker.  This keeps a broken
            // or unresponsive session bus from delaying application exit;
            // the worker still shuts down its service when it observes stop.
        }
    }
}

impl Drop for TrayRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.as_ref() {
            thread.thread().unpark();
        }
    }
}

struct SnipChordTray {
    executable: PathBuf,
}

impl SnipChordTray {
    fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
}

impl ksni::Tray for SnipChordTray {
    // Keep activation focused on the menu so the compact panel item does not
    // trigger an immediate capture before the user chooses a destination.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "snipchord".to_owned()
    }

    fn title(&self) -> String {
        "SnipChord".to_owned()
    }

    fn icon_name(&self) -> String {
        "snipchord".to_owned()
    }

    fn icon_theme_path(&self) -> String {
        icon_theme_path(&self.executable).display().to_string()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let mut menu = Vec::with_capacity(MENU_ACTIONS.len());
        for action in MENU_ACTIONS.iter().copied() {
            let executable = self.executable.clone();
            menu.push(
                StandardItem {
                    label: action.label().to_owned(),
                    activate: Box::new(move |_| launch_command(&executable, action.args())),
                    ..Default::default()
                }
                .into(),
            );
        }
        menu
    }
}

fn icon_theme_path(executable: &Path) -> PathBuf {
    if let Some(prefix) = executable
        .parent()
        .filter(|directory| directory.file_name().is_some_and(|name| name == "bin"))
        .and_then(Path::parent)
    {
        // The installer keeps the icon beside the selected executable's
        // prefix, including when --prefix points somewhere other than
        // ~/.local.  This makes the SNI lookup follow the actual install.
        return prefix.join("share/icons/hicolor/scalable/apps");
    }

    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from(".local/share"));
    data_home.join("icons/hicolor/scalable/apps")
}

fn launch_command(executable: &Path, arguments: &[&str]) {
    let child = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            if let Err(error) = thread::Builder::new()
                .name("snipchord-tray-command".to_owned())
                .spawn(move || {
                    let _ = child.wait();
                })
            {
                eprintln!("snipchord: could not reap tray command: {error}");
            }
        }
        Err(error) => eprintln!("snipchord: could not run tray command: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{icon_theme_path, TrayAction, MENU_ACTIONS};
    use std::path::Path;

    #[test]
    fn actions_map_to_existing_singleton_commands() {
        assert_eq!(TrayAction::Preferences.args(), &["--preferences"]);
        assert_eq!(TrayAction::About.args(), &["--about"]);
        assert_eq!(TrayAction::Quit.args(), &["--quit"]);
    }

    #[test]
    fn menu_order_keeps_settings_actions_before_quit() {
        assert_eq!(
            MENU_ACTIONS,
            &[TrayAction::Preferences, TrayAction::About, TrayAction::Quit,]
        );
    }

    #[test]
    fn icon_path_follows_a_custom_bin_prefix() {
        assert_eq!(
            icon_theme_path(Path::new("/opt/snipchord/bin/snipchord")),
            Path::new("/opt/snipchord/share/icons/hicolor/scalable/apps")
        );
    }

    #[test]
    fn icon_path_falls_back_to_user_theme_for_uninstalled_binary() {
        assert_eq!(
            icon_theme_path(Path::new("target/release/snipchord")),
            std::env::var_os("XDG_DATA_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| std::path::PathBuf::from(home).join(".local/share"))
                })
                .unwrap_or_else(|| std::path::PathBuf::from(".local/share"))
                .join("icons/hicolor/scalable/apps")
        );
    }
}
