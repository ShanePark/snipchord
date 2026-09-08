//! Application orchestration and the resident X11 event loop.
//!
//! The UI owns X11 windows and pointer interaction, while this module owns
//! process lifetime, capture buffers, settings, storage, and the clipboard.
//! Keeping those responsibilities separate lets a hotkey invocation reuse the
//! same X connection without making the UI depend on a toolkit runtime.

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::CURRENT_TIME;

use crate::clipboard::Clipboard;
use crate::geometry::Rect;
use crate::image::{capture_root_with_size, RgbaImage};
use crate::settings::{read_settings, write_settings, Settings};
use crate::shortcuts;
use crate::storage;
use crate::tray::TrayRuntime;
use crate::ui::Ui;
use crate::window_capture::{WindowPicker, WindowTarget};
use crate::x11::{Instance, InstanceClaim, X11Context};

/// Errors returned by application operations share the same bound used by the
/// low-level X11, image, and clipboard modules.
pub type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

enum PreviewCompletion {
    Prepared(storage::PreparedPreview),
    SavedElsewhere,
}

type PreviewPreparationResult = Result<PreviewCompletion, Box<dyn Error + Send + Sync>>;

struct PreviewPreparation {
    generation: u64,
    result: Receiver<PreviewPreparationResult>,
    handle: JoinHandle<()>,
}

const COMMAND_REGION: u32 = 1;
const COMMAND_FULLSCREEN: u32 = 2;
const COMMAND_PREFERENCES: u32 = 3;
const COMMAND_QUIT: u32 = 4;
const COMMAND_DEMO: u32 = 5;
const COMMAND_REGION_CLIPBOARD: u32 = 6;
const COMMAND_REGION_SAVE: u32 = 7;
const COMMAND_FULLSCREEN_CLIPBOARD: u32 = 8;
const COMMAND_FULLSCREEN_SAVE: u32 = 9;
const COMMAND_ABOUT: u32 = 10;

/// The project's public source repository, shown by the tray About item and
/// the native About window.
pub const REPOSITORY_URL: &str = "https://github.com/ShanePark/snipchord";

/// User-visible actions emitted by the native UI.
///
/// The UI reports intent here instead of writing files or claiming the
/// clipboard itself. This keeps actions from a resident command and actions
/// from a button on the preview identical.
#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    None,
    /// The UI read only the accepted crop from its server-side frozen
    /// pixmap. This keeps normal region selection from downloading the whole
    /// desktop before the user has chosen a rectangle.
    Captured(RgbaImage),
    Selected(Rect),
    /// A window selected after entering window-selection mode with Space.
    ///
    /// Keep the sampled XID and geometry until the application commits the
    /// image.  This gives Composite a chance to replace an occluded client
    /// area while retaining the frozen-root crop as a safe fallback.
    SelectedWindow(WindowTarget),
    Cancelled,
    Close,
    /// The user clicked the completed capture thumbnail.
    OpenPreview,
    /// The user clicked the repository link in About.
    OpenRepository,
    SettingsChanged(Settings),
}

/// Capture/command mode requested by one invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Region,
    Fullscreen,
    RegionClipboard,
    RegionSave,
    FullscreenClipboard,
    FullscreenSave,
    Preferences,
    About,
    Daemon,
    Quit,
    Demo,
}

/// Destination requested by a capture command.
///
/// `Legacy` preserves the original behavior for existing commands: copy to
/// the clipboard and additionally save when the preference is enabled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Legacy,
    Clipboard,
    Save,
}

impl Mode {
    fn capture(self) -> Option<(bool, OutputMode)> {
        match self {
            Self::Region => Some((false, OutputMode::Legacy)),
            Self::Fullscreen => Some((true, OutputMode::Legacy)),
            Self::RegionClipboard => Some((false, OutputMode::Clipboard)),
            Self::RegionSave => Some((false, OutputMode::Save)),
            Self::FullscreenClipboard => Some((true, OutputMode::Clipboard)),
            Self::FullscreenSave => Some((true, OutputMode::Save)),
            _ => None,
        }
    }
}

impl Mode {
    fn command(self) -> Option<u32> {
        match self {
            Self::Region => Some(COMMAND_REGION),
            Self::Fullscreen => Some(COMMAND_FULLSCREEN),
            Self::RegionClipboard => Some(COMMAND_REGION_CLIPBOARD),
            Self::RegionSave => Some(COMMAND_REGION_SAVE),
            Self::FullscreenClipboard => Some(COMMAND_FULLSCREEN_CLIPBOARD),
            Self::FullscreenSave => Some(COMMAND_FULLSCREEN_SAVE),
            Self::Preferences => Some(COMMAND_PREFERENCES),
            Self::About => Some(COMMAND_ABOUT),
            Self::Daemon | Self::Quit => None,
            Self::Demo => Some(COMMAND_DEMO),
        }
    }
}

/// Result of parsing one command line, including informational requests that
/// do not need an X11 connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedCommand {
    Help,
    Version,
    Mode(Mode),
    ConfigureOutputDirectory(PathBuf),
}

#[derive(Debug, Eq, PartialEq)]
pub struct CliError {
    message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CliError {}

/// Parse the process arguments.  `args` includes the executable name, as
/// returned by [`std::env::args_os`].
pub fn parse_args(args: &[OsString]) -> Result<ParsedCommand, CliError> {
    // Match the old command's friendly behavior: informational flags are
    // handled before connecting to X11, even if another flag is present.
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        return Ok(ParsedCommand::Help);
    }
    if args.iter().skip(1).any(|arg| arg == "--version") {
        return Ok(ParsedCommand::Version);
    }

    let mut capture_kind = None;
    let mut output = OutputMode::Legacy;
    let mut output_flag = None;
    let mut action = None;
    let mut output_directory = None;
    let mut end_of_options = false;
    let mut arguments = args.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if end_of_options {
            return Err(CliError::new(format!(
                "unexpected argument `{}`",
                argument.to_string_lossy()
            )));
        }
        if argument == "--" {
            end_of_options = true;
            continue;
        }

        if let Some(value) = argument
            .to_str()
            .and_then(|value| value.strip_prefix("--save-dir="))
        {
            if value.is_empty() || value.starts_with('-') {
                return Err(CliError::new("`--save-dir` requires a directory path"));
            }
            if output_directory.replace(PathBuf::from(value)).is_some() {
                return Err(CliError::new("`--save-dir` may only be specified once"));
            }
            continue;
        }

        let candidate = match argument.to_str() {
            Some("--region") => {
                if capture_kind.replace(false).is_some() {
                    return Err(CliError::new(
                        "the options `--region` and `--fullscreen` cannot be used together",
                    ));
                }
                continue;
            }
            Some("--fullscreen") => {
                if capture_kind.replace(true).is_some() {
                    return Err(CliError::new(
                        "the options `--region` and `--fullscreen` cannot be used together",
                    ));
                }
                continue;
            }
            Some("--clipboard") => {
                if output_flag.replace("--clipboard").is_some() {
                    return Err(CliError::new(
                        "the options `--clipboard` and `--save` cannot be used together",
                    ));
                }
                output = OutputMode::Clipboard;
                continue;
            }
            Some("--save") => {
                if output_flag.replace("--save").is_some() {
                    return Err(CliError::new(
                        "the options `--clipboard` and `--save` cannot be used together",
                    ));
                }
                output = OutputMode::Save;
                continue;
            }
            Some("--save-dir") => {
                let Some(value) = arguments.next() else {
                    return Err(CliError::new("`--save-dir` requires a directory path"));
                };
                if value.is_empty() || value.to_string_lossy().starts_with('-') {
                    return Err(CliError::new("`--save-dir` requires a directory path"));
                }
                if output_directory.replace(PathBuf::from(value)).is_some() {
                    return Err(CliError::new("`--save-dir` may only be specified once"));
                }
                continue;
            }
            Some("--preferences") => Mode::Preferences,
            Some("--about") => Mode::About,
            Some("--daemon") => Mode::Daemon,
            Some("--quit") => Mode::Quit,
            Some("--demo") => Mode::Demo,
            Some(value) => {
                return Err(CliError::new(format!("unrecognized option `{value}`")));
            }
            None => {
                return Err(CliError::new(format!(
                    "unrecognized option `{}`",
                    argument.to_string_lossy()
                )));
            }
        };

        if let Some(previous) = action {
            return Err(CliError::new(format!(
                "the options `{}` and `{}` cannot be used together",
                mode_name(previous),
                mode_name(candidate)
            )));
        }
        action = Some(candidate);
    }

    if let Some(path) = output_directory {
        if action.is_some() || capture_kind.is_some() || output_flag.is_some() {
            return Err(CliError::new(
                "`--save-dir` is a configuration command and cannot be combined with capture options",
            ));
        }
        return Ok(ParsedCommand::ConfigureOutputDirectory(path));
    }

    let action = action.unwrap_or(Mode::Region);
    if output_flag.is_some() && action != Mode::Region && action != Mode::Fullscreen {
        return Err(CliError::new(
            "output options can only be used with a capture command",
        ));
    }
    if action == Mode::Demo && output_flag.is_some() {
        return Err(CliError::new(
            "`--demo` cannot be combined with an output option",
        ));
    }
    if action != Mode::Region && action != Mode::Fullscreen {
        if capture_kind.is_some() {
            return Err(CliError::new(
                "capture options cannot be combined with this command",
            ));
        }
        if output_flag.is_some() {
            return Err(CliError::new(
                "output options can only be used with a capture command",
            ));
        }
        return Ok(ParsedCommand::Mode(action));
    }

    let fullscreen = capture_kind.unwrap_or(action == Mode::Fullscreen);
    let mode = match (fullscreen, output) {
        (false, OutputMode::Legacy) => Mode::Region,
        (true, OutputMode::Legacy) => Mode::Fullscreen,
        (false, OutputMode::Clipboard) => Mode::RegionClipboard,
        (false, OutputMode::Save) => Mode::RegionSave,
        (true, OutputMode::Clipboard) => Mode::FullscreenClipboard,
        (true, OutputMode::Save) => Mode::FullscreenSave,
    };
    Ok(ParsedCommand::Mode(mode))
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Region => "--region",
        Mode::Fullscreen => "--fullscreen",
        Mode::RegionClipboard => "--region --clipboard",
        Mode::RegionSave => "--region --save",
        Mode::FullscreenClipboard => "--fullscreen --clipboard",
        Mode::FullscreenSave => "--fullscreen --save",
        Mode::Preferences => "--preferences",
        Mode::About => "--about",
        Mode::Daemon => "--daemon",
        Mode::Quit => "--quit",
        Mode::Demo => "--demo",
    }
}

pub const HELP_TEXT: &str = "SnipChord — select, capture, carry on.\n\
Usage: snipchord [--region | --fullscreen] [--clipboard | --save]\n\
       snipchord [--preferences | --about | --daemon | --demo | --quit]\n\
       snipchord --save-dir PATH\n\
X11 only. Default capture: region, clipboard (plus automatic save when enabled).\n\
--clipboard and --save select one explicit destination. --demo uses a synthetic desktop.\n\
--save-dir persists the directory used by --save and automatic saves.\n\
Selection: drag to capture, Space to move or choose a window, Esc or right click to cancel.";

/// Main entry point used by `src/main.rs`.  It returns a process exit status
/// instead of calling `exit`, which keeps argument and settings logic easy to
/// exercise in unit tests.
pub fn run(args: &[OsString]) -> i32 {
    match parse_args(args) {
        Ok(ParsedCommand::Help) => {
            println!("{HELP_TEXT}");
            0
        }
        Ok(ParsedCommand::Version) => {
            println!("SnipChord {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Ok(ParsedCommand::ConfigureOutputDirectory(path)) => {
            match configure_output_directory(&path) {
                Ok(directory) => {
                    println!("Screenshot directory: {}", directory.display());
                    0
                }
                Err(error) => {
                    eprintln!("snipchord: could not save screenshot directory: {error}");
                    1
                }
            }
        }
        Ok(ParsedCommand::Mode(mode)) => match launch(mode) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("snipchord: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("snipchord: {error}");
            eprintln!("Try `snipchord --help` for usage.");
            2
        }
    }
}

/// Persist the output directory without opening an X11 connection.  This is
/// intentionally a standalone command so it also works before a graphical
/// session is available and updates the settings read by a resident daemon on
/// its next capture.
fn configure_output_directory(path: &std::path::Path) -> AppResult<PathBuf> {
    let value = path.to_string_lossy().trim().to_owned();
    if value.is_empty() {
        return Err("screenshot directory cannot be empty".into());
    }
    let mut settings = read_settings();
    settings.output_directory = Some(value);
    write_settings(&settings)?;
    Ok(storage::screenshot_directory_for(
        settings.output_directory.as_deref(),
    ))
}

fn launch(mode: Mode) -> AppResult<()> {
    let context = X11Context::connect()?;

    // `--quit` is intentionally checked without claiming the selection.  A
    // machine with no resident process should remain unchanged after a quit
    // command.
    if mode == Mode::Quit {
        if let Some(owner) = context.instance_owner()? {
            context.send_instance_command(owner, COMMAND_QUIT)?;
        }
        return Ok(());
    }

    match context.claim_instance()? {
        InstanceClaim::Existing(owner) => {
            if let Some(command) = mode.command() {
                context.send_instance_command(owner, command)?;
            }
            Ok(())
        }
        InstanceClaim::Owner(instance) => {
            let settings = read_settings();
            let mut app = App::new(context, instance, settings)?;
            app.run(mode)
        }
    }
}

/// Resident process state.  There is one `App` per claimed X11 instance and
/// one X connection for the entire lifetime of that resident process.
pub struct App {
    context: X11Context,
    instance: Option<Instance>,
    clipboard: Clipboard,
    ui: Ui,
    window_picker: WindowPicker,
    settings: Settings,
    image: Option<RgbaImage>,
    saved_path: Option<PathBuf>,
    preview_preparations: Vec<PreviewPreparation>,
    preview_completions: BTreeMap<u64, PreviewPreparationResult>,
    preview_generation: u64,
    next_preview_generation: u64,
    next_preview_publication: u64,
    preview_ready: Option<(u64, PathBuf)>,
    pending_preview_open: Option<u64>,
    pending_image: Option<RgbaImage>,
    pending_output: Option<OutputMode>,
    current_demo: bool,
    tray: Option<TrayRuntime>,
    running: bool,
}

impl App {
    pub fn new(context: X11Context, instance: Instance, settings: Settings) -> AppResult<Self> {
        let clipboard = match Clipboard::new(&context.conn, instance.window) {
            Ok(clipboard) => clipboard,
            Err(error) => {
                let _ = instance.release(&context);
                return Err(error);
            }
        };
        let window_picker = match WindowPicker::new(&context.conn) {
            Ok(window_picker) => window_picker,
            Err(error) => {
                let mut clipboard = clipboard;
                let _ = clipboard.release(&context.conn);
                let _ = instance.release(&context);
                return Err(error);
            }
        };
        let ui = match Ui::new(&context) {
            Ok(ui) => ui,
            Err(error) => {
                let mut clipboard = clipboard;
                let _ = clipboard.release(&context.conn);
                let _ = instance.release(&context);
                return Err(error);
            }
        };

        if let Err(error) = storage::cleanup_preview_cache() {
            eprintln!("snipchord: preview cache cleanup: {error}");
        }

        Ok(Self {
            context,
            instance: Some(instance),
            clipboard,
            ui,
            window_picker,
            settings,
            image: None,
            saved_path: None,
            preview_preparations: Vec::new(),
            preview_completions: BTreeMap::new(),
            preview_generation: 0,
            next_preview_generation: 0,
            next_preview_publication: 1,
            preview_ready: None,
            pending_preview_open: None,
            pending_image: None,
            pending_output: None,
            current_demo: false,
            tray: None,
            running: true,
        })
    }

    /// Execute the initial command and remain resident until `--quit` or a UI
    /// quit action arrives.  Cleanup runs even when an X11 request fails.
    pub fn run(&mut self, mode: Mode) -> AppResult<()> {
        // D-Bus startup runs independently of the initial capture.  A tray is
        // useful for a resident daemon, but a missing or late panel must never
        // delay the first screenshot.
        self.tray = TrayRuntime::start();
        let result = self.dispatch_mode(mode).and_then(|()| {
            if self.running {
                self.event_loop()
            } else {
                Ok(())
            }
        });
        self.shutdown(result)
    }

    fn dispatch_mode(&mut self, mode: Mode) -> AppResult<()> {
        if let Some((fullscreen, output)) = mode.capture() {
            return self.begin_capture(fullscreen, false, output);
        }
        match mode {
            Mode::Region
            | Mode::Fullscreen
            | Mode::RegionClipboard
            | Mode::RegionSave
            | Mode::FullscreenClipboard
            | Mode::FullscreenSave => unreachable!("capture modes were handled above"),
            Mode::Preferences => self.show_preferences(),
            Mode::About => self.show_about(),
            Mode::Daemon => Ok(()),
            Mode::Quit => {
                self.running = false;
                Ok(())
            }
            Mode::Demo => self.begin_capture(false, true, OutputMode::Legacy),
        }
    }

    fn invalidate_preview_generation(&mut self) {
        self.preview_ready = None;
    }

    fn begin_capture(&mut self, fullscreen: bool, demo: bool, output: OutputMode) -> AppResult<()> {
        if self.pending_image.is_some() || self.ui.has_selection() {
            return Ok(());
        }
        // A new capture invalidates the current ready path.  An earlier click
        // remains associated with its own generation so it can still open the
        // correct image if that worker finishes after this capture starts.
        self.invalidate_preview_generation();

        // Install the cursor and input grabs before any
        // geometry/configuration reads or full-screen capture work.  On a
        // large desktop the server-side snapshot can take a noticeable
        // amount of time; grabbing the pointer first makes the crosshair
        // appear at the moment the shortcut is handled and keeps an eager
        // click away from the application underneath.  Native selection
        // setup adopts these grabs once its real surface is ready.
        if !fullscreen {
            if let Err(error) = self.ui.prepare_capture_cursor(&self.context) {
                // A pre-existing external grab should not prevent the
                // established fallback path from attempting its normal
                // transactional selection setup.
                eprintln!("snipchord: early capture input unavailable: {error}");
            } else if env::var_os("SNIPCHORD_BENCHMARK_READY").is_some() {
                // stderr is unbuffered when the daemon is launched by the
                // benchmark harness, so the readiness marker cannot sit in
                // stdout's block buffer until a later capture completes.
                eprintln!("selection_input_ready");
            }
        }

        // A resident daemon keeps its X11 connection warm, so reload the
        // small settings file before each command. This makes a standalone
        // `--save-dir` invocation effective without restarting the daemon.
        self.settings = read_settings();

        if let Err(error) = self.context.refresh_root_geometry() {
            let cleanup = self.ui.close_capture_cursor(&self.context);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => {
                    Err(format!("{error}; capture input cleanup failed: {cleanup_error}").into())
                }
            };
        }

        // Hide an existing preview/preferences window before GetImage so the
        // capture contains the desktop underneath. Give the compositor one
        // frame to retire the transient surface without adding the old
        // multi-frame delay to every repeated hotkey capture.
        if self.ui.close_for_capture(&self.context) {
            if let Err(error) = self.context.flush() {
                let cleanup = self.ui.close_capture_cursor(&self.context);
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(format!(
                        "{error}; capture input cleanup failed: {cleanup_error}"
                    )
                    .into()),
                };
            }
            thread::sleep(Duration::from_millis(16));
        }

        if !fullscreen && !demo {
            // Keep the complete desktop in X11 pixmaps while the user drags.
            // The first client-side GetImage is deferred until acceptance, so
            // the hotkey only pays for CopyArea and the small overlay setup.
            match crate::server_capture::ServerCapture::begin(&mut self.context) {
                Ok(Some(capture)) => {
                    self.pending_image = None;
                    self.pending_output = Some(output);
                    self.current_demo = false;
                    match self.ui.start_selection_native(&self.context, capture) {
                        Ok(()) => return Ok(()),
                        Err(error) => {
                            self.pending_output = None;
                            eprintln!("snipchord: native selection unavailable: {error}");
                            // Fall through to the established client-side
                            // path if grabs/window setup reject the fast path.
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("snipchord: native selection unavailable: {error}");
                }
            }
        }

        let image_result = if demo {
            RgbaImage::solid(
                u32::from(self.context.width()),
                u32::from(self.context.height()),
                [0x36, 0x50, 0x70],
            )
        } else {
            capture_root_with_size(
                &self.context.conn,
                self.context.screen_num,
                self.context.width(),
                self.context.height(),
            )
        };
        let image = match image_result {
            Ok(image) => image,
            Err(error) => {
                let cleanup = self.ui.close_capture_cursor(&self.context);
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(format!(
                        "{error}; capture input cleanup failed: {cleanup_error}"
                    )
                    .into()),
                };
            }
        };

        if fullscreen {
            self.complete_capture(image, demo, output)
        } else {
            self.pending_image = Some(image.clone());
            self.pending_output = Some(output);
            self.current_demo = demo;
            let result = self.ui.start_selection(&self.context, &image);
            if result.is_err() {
                self.pending_image = None;
                self.pending_output = None;
                let _ = self.ui.close_capture_cursor(&self.context);
            }
            result
        }
    }

    fn complete_selection(&mut self, rect: Rect) -> AppResult<()> {
        let Some(full_image) = self.pending_image.take() else {
            return Ok(());
        };
        let output = self.pending_output.take().unwrap_or(OutputMode::Legacy);
        if !rect.valid() {
            return Ok(());
        }

        // X11 root pixels and the selection overlay share desktop pixel
        // coordinates in this X11-only implementation.  Keep the conversion
        // explicit so a future mixed-scale UI can change this at one boundary.
        let pixel_rect = rect.pixels(1.0, 1.0);
        let Some(image) = full_image.crop(
            pixel_rect.x,
            pixel_rect.y,
            pixel_rect.width,
            pixel_rect.height,
        ) else {
            return Ok(());
        };
        self.complete_capture(image, self.current_demo, output)
    }

    fn complete_window_selection(&mut self, target: WindowTarget) -> AppResult<()> {
        let Some(full_image) = self.pending_image.take() else {
            return Ok(());
        };
        let output = self.pending_output.take().unwrap_or(OutputMode::Legacy);
        let captured = self.window_picker.capture_target(
            &self.context.conn,
            target,
            &full_image,
            self.context.visual(),
        )?;
        self.complete_capture(captured.image, self.current_demo, output)
    }

    fn complete_capture(
        &mut self,
        image: RgbaImage,
        demo: bool,
        output: OutputMode,
    ) -> AppResult<()> {
        // A saved path belongs to exactly one captured image.  Clear it before
        // any work for the new capture so a later Save cannot reuse an older
        // screenshot's filename.
        self.saved_path = None;
        let mut saved = None;
        if !demo {
            match output {
                OutputMode::Legacy | OutputMode::Clipboard => {
                    if let Err(error) =
                        self.clipboard
                            .set_image(&self.context.conn, &image, CURRENT_TIME)
                    {
                        self.notify("Could not copy screenshot", &error.to_string());
                        return Ok(());
                    }
                }
                OutputMode::Save => match storage::save_image_with_directory(
                    &image,
                    self.settings.output_directory.as_deref(),
                ) {
                    Ok(path) => saved = Some(path),
                    Err(error) => {
                        self.notify("Could not save screenshot", &error.to_string());
                        return Ok(());
                    }
                },
            }
            if output == OutputMode::Legacy && self.settings.save_automatically {
                match storage::save_image_with_directory(
                    &image,
                    self.settings.output_directory.as_deref(),
                ) {
                    Ok(path) => saved = Some(path),
                    Err(error) => {
                        self.notify("Copied, but file could not be saved", &error.to_string())
                    }
                }
            }
        }

        self.image = Some(image);
        self.current_demo = demo;
        self.saved_path = saved;
        self.preview_generation = self.next_preview_generation.wrapping_add(1);
        self.next_preview_generation = self.preview_generation;
        self.invalidate_preview_generation();

        if self.saved_path.is_some() {
            self.preview_completions.insert(
                self.preview_generation,
                Ok(PreviewCompletion::SavedElsewhere),
            );
        } else if let Some(image) = self.image.as_ref().cloned() {
            if let Err(error) = self.start_preview_preparation(self.preview_generation, image) {
                self.preview_completions
                    .insert(self.preview_generation, Err(error));
            }
        }
        self.publish_preview_completions();

        let (image_width, image_height) = self
            .image
            .as_ref()
            .map(|image| (image.width(), image.height()))
            .expect("capture image was just stored");
        if self.settings.show_preview || demo {
            self.ui.show_preview(
                &self.context,
                self.image.as_ref().expect("capture image was just stored"),
                demo,
                self.saved_path.as_deref(),
            )?;
        } else if !demo {
            let (title, body) = match output {
                OutputMode::Save => ("Screenshot saved", "PNG saved to the configured directory"),
                OutputMode::Clipboard | OutputMode::Legacy => {
                    ("Screenshot copied", "Ready to paste · PNG + BMP")
                }
            };
            self.notify(title, body);
        }

        println!(
            "capture_complete width={} height={} demo={demo}",
            image_width, image_height
        );
        Ok(())
    }

    fn start_preview_preparation(&mut self, generation: u64, image: RgbaImage) -> AppResult<()> {
        let (sender, result) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("snipchord-preview-png".to_owned())
            .spawn(move || {
                let result =
                    storage::prepare_temporary_image(&image).map(PreviewCompletion::Prepared);
                let _ = sender.send(result);
            })
            .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)?;
        self.preview_preparations.push(PreviewPreparation {
            generation,
            result,
            handle,
        });
        Ok(())
    }

    fn poll_preview_preparations(&mut self) {
        let mut completed = Vec::new();
        let mut finished = Vec::new();
        for (index, preparation) in self.preview_preparations.iter_mut().enumerate() {
            match preparation.result.try_recv() {
                Ok(result) => {
                    completed.push((preparation.generation, result));
                    finished.push(index);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    completed.push((
                        preparation.generation,
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "preview preparation stopped before publishing",
                        )
                        .into()),
                    ));
                    finished.push(index);
                }
            }
        }

        for index in finished.into_iter().rev() {
            let preparation = self.preview_preparations.remove(index);
            let _ = preparation.handle.join();
        }
        for (generation, result) in completed {
            self.preview_completions.insert(generation, result);
        }
        self.publish_preview_completions();
    }

    fn publish_preview_completions(&mut self) {
        loop {
            let generation = self.next_preview_publication;
            let Some(completion) = self.preview_completions.remove(&generation) else {
                break;
            };
            match completion {
                Ok(PreviewCompletion::SavedElsewhere) => {}
                Ok(PreviewCompletion::Prepared(prepared)) => {
                    match storage::publish_temporary_image(prepared) {
                        Ok(path) => {
                            if let Err(error) = storage::rotate_preview_cache() {
                                eprintln!("snipchord: preview cache rotation: {error}");
                            }
                            if self.preview_generation == generation {
                                self.preview_ready = Some((generation, path.clone()));
                            }
                            if self.pending_preview_open == Some(generation) {
                                self.pending_preview_open = None;
                                if let Err(error) = open_with_default_image_viewer(&path) {
                                    self.notify("Could not open screenshot", &error.to_string());
                                }
                            }
                        }
                        Err(error) => self.preview_preparation_failed(generation, error),
                    }
                }
                Err(error) => self.preview_preparation_failed(generation, error),
            }
            self.next_preview_publication = self.next_preview_publication.wrapping_add(1);
        }
    }

    fn preview_preparation_failed(&mut self, generation: u64, error: Box<dyn Error + Send + Sync>) {
        if self.pending_preview_open == Some(generation) {
            self.pending_preview_open = None;
            self.notify("Could not open screenshot", &error.to_string());
        } else {
            eprintln!("snipchord: preview preparation: {error}");
        }
    }

    fn show_preferences(&mut self) -> AppResult<()> {
        // Opening preferences cancels an in-progress selection in the UI.  Do
        // the matching application-side cleanup or the next capture remains
        // blocked by the stale full-screen image.
        self.pending_image = None;
        self.pending_output = None;
        let shortcuts = shortcuts::configured_shortcuts();
        self.ui
            .show_preferences(&self.context, self.settings.clone(), shortcuts)
    }

    fn show_about(&mut self) -> AppResult<()> {
        // Opening an informational surface cancels any pending command state;
        // the UI owns the transient X11 windows and closes overlapping ones.
        self.pending_image = None;
        self.pending_output = None;
        self.ui.show_about(&self.context)
    }

    fn dispatch_event(&mut self, event: Event) -> AppResult<()> {
        // Clipboard requestor failures are expected when a client exits while
        // a transfer is in flight.  The clipboard module cleans up the
        // transfer and the resident app remains usable, so report and ignore
        // those errors here.
        match self.clipboard.handle_event(&self.context.conn, &event) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                eprintln!("snipchord: clipboard transfer: {error}");
                return Ok(());
            }
        }

        if let Some(command) = self
            .instance
            .as_ref()
            .and_then(|instance| self.context.instance_command(instance, &event))
        {
            match command {
                COMMAND_REGION => self.run_resident_action(
                    |app| app.begin_capture(false, false, OutputMode::Legacy),
                    "Capture failed",
                ),
                COMMAND_FULLSCREEN => self.run_resident_action(
                    |app| app.begin_capture(true, false, OutputMode::Legacy),
                    "Capture failed",
                ),
                COMMAND_REGION_CLIPBOARD => self.run_resident_action(
                    |app| app.begin_capture(false, false, OutputMode::Clipboard),
                    "Capture failed",
                ),
                COMMAND_REGION_SAVE => self.run_resident_action(
                    |app| app.begin_capture(false, false, OutputMode::Save),
                    "Capture failed",
                ),
                COMMAND_FULLSCREEN_CLIPBOARD => self.run_resident_action(
                    |app| app.begin_capture(true, false, OutputMode::Clipboard),
                    "Capture failed",
                ),
                COMMAND_FULLSCREEN_SAVE => self.run_resident_action(
                    |app| app.begin_capture(true, false, OutputMode::Save),
                    "Capture failed",
                ),
                COMMAND_PREFERENCES => self.run_resident_action(
                    |app| app.show_preferences(),
                    "Could not open preferences",
                ),
                COMMAND_ABOUT => {
                    self.run_resident_action(|app| app.show_about(), "Could not open About")
                }
                COMMAND_QUIT => self.running = false,
                COMMAND_DEMO => self.run_resident_action(
                    |app| app.begin_capture(false, true, OutputMode::Legacy),
                    "Capture failed",
                ),
                _ => {}
            }
            return Ok(());
        }

        if let Some(action) = self.ui.handle_event(&self.context, &event)? {
            if action != UiAction::None {
                self.handle_action(action)?;
            }
        }
        Ok(())
    }

    fn handle_action(&mut self, action: UiAction) -> AppResult<()> {
        match action {
            UiAction::None => Ok(()),
            UiAction::Captured(image) => {
                self.pending_image = None;
                let output = self.pending_output.take().unwrap_or(OutputMode::Legacy);
                self.complete_capture(image, self.current_demo, output)
            }
            UiAction::Selected(rect) => self.complete_selection(rect),
            UiAction::SelectedWindow(target) => self.complete_window_selection(target),
            UiAction::Cancelled => {
                self.pending_image = None;
                self.pending_output = None;
                Ok(())
            }
            UiAction::Close => {
                // The UI uses Close for WM_DELETE on every transient surface,
                // including the selection overlay.  A closed selection must
                // release its captured root image as well.
                self.pending_image = None;
                self.pending_output = None;
                Ok(())
            }
            UiAction::OpenPreview => {
                if let Err(error) = self.open_preview() {
                    self.notify("Could not open screenshot", &error.to_string());
                }
                Ok(())
            }
            UiAction::OpenRepository => {
                if let Err(error) = open_with_default_browser(REPOSITORY_URL) {
                    self.notify("Could not open GitHub", &error.to_string());
                }
                Ok(())
            }
            UiAction::SettingsChanged(settings) => {
                self.settings = settings;
                if let Err(error) = write_settings(&self.settings) {
                    self.notify("Could not save preferences", &error.to_string());
                }
                Ok(())
            }
        }
    }

    fn run_resident_action<F>(&mut self, action: F, title: &str)
    where
        F: FnOnce(&mut Self) -> AppResult<()>,
    {
        if let Err(error) = action(self) {
            self.notify(title, &error.to_string());
        }
    }

    fn notify(&mut self, title: &str, body: &str) {
        // Native X11 mode intentionally has no desktop-bus dependency. Keep
        // failures visible to a launcher if the notice surface itself cannot
        // be created.
        if let Err(error) = self.ui.show_notice(&self.context, title, body) {
            eprintln!("snipchord: {title}: {body} ({error})");
        }
    }

    fn open_preview(&mut self) -> AppResult<()> {
        if let Some(path) = self.saved_path.clone().filter(|path| path.is_file()) {
            let path = fs::canonicalize(path)?;
            return open_with_default_image_viewer(&path).map_err(Into::into);
        }

        if let Some((generation, path)) = self.preview_ready.clone() {
            if generation == self.preview_generation && path.is_file() {
                return open_with_default_image_viewer(&path).map_err(Into::into);
            }
        }

        let preview_is_pending = self
            .preview_preparations
            .iter()
            .any(|preparation| preparation.generation == self.preview_generation)
            || self
                .preview_completions
                .contains_key(&self.preview_generation);
        if preview_is_pending {
            // The thumbnail may be clicked before the worker has published its
            // file.  Keep the click associated with this exact capture and
            // let the event loop open it after publication.
            self.pending_preview_open = Some(self.preview_generation);
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no prepared screenshot preview is available",
        )
        .into())
    }

    fn event_loop(&mut self) -> AppResult<()> {
        while self.running {
            self.poll_preview_preparations();
            self.drain_events()?;
            if !self.running {
                break;
            }
            self.ui.tick(&self.context)?;
            self.clipboard.tick(&self.context.conn)?;
            self.poll_preview_preparations();
            if !self.running {
                break;
            }
            // `Ui::tick` may complete a deferred X11 keyboard-grab reply.
            // x11rb can queue input events read while waiting for that reply,
            // so drain once more before falling into a raw poll. Otherwise a
            // successful retry with no future deadline could leave Esc or a
            // queued drag asleep in the connection buffer indefinitely.
            self.drain_events()?;
            if !self.running {
                break;
            }

            let deadline = earliest_deadline(
                earliest_deadline(self.ui.next_deadline(), self.clipboard.next_deadline()),
                (!self.preview_preparations.is_empty())
                    .then(|| Instant::now() + Duration::from_millis(8)),
            );
            if deadline.is_some_and(|when| when <= Instant::now()) {
                continue;
            }
            self.wait_for_x11(deadline)?;
        }
        Ok(())
    }

    fn drain_events(&mut self) -> AppResult<()> {
        while let Some(event) = self.context.conn.poll_for_event()? {
            self.dispatch_event(event)?;
            if !self.running {
                break;
            }
        }
        Ok(())
    }

    fn wait_for_x11(&self, deadline: Option<Instant>) -> AppResult<()> {
        let timeout = poll_timeout_ms(deadline);
        let mut descriptor = libc::pollfd {
            fd: self.context.conn.stream().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(Box::new(error));
        }
        Ok(())
    }

    fn finish_preview_preparations(&mut self) {
        self.pending_preview_open = None;
        let preparations = std::mem::take(&mut self.preview_preparations);
        for preparation in preparations {
            let _ = preparation.handle.join();
            match preparation.result.try_recv() {
                Ok(result) => {
                    self.preview_completions
                        .insert(preparation.generation, result);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    self.preview_completions.insert(
                        preparation.generation,
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "preview preparation stopped before publishing",
                        )
                        .into()),
                    );
                }
            }
        }
        self.publish_preview_completions();
    }

    fn shutdown(&mut self, primary: AppResult<()>) -> AppResult<()> {
        self.finish_preview_preparations();
        if let Some(tray) = self.tray.take() {
            tray.shutdown();
        }
        let ui_result = self.ui.shutdown(&self.context);
        let clipboard_result = self.clipboard.release(&self.context.conn);
        let instance_result = self
            .instance
            .take()
            .map(|instance| instance.release(&self.context));

        primary
            .and(ui_result)
            .and(clipboard_result)
            .and(instance_result.unwrap_or(Ok(())))
    }
}

fn open_with_default_image_viewer(path: &std::path::Path) -> io::Result<()> {
    let mut child = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    thread::Builder::new()
        .name("snipchord-xdg-open".to_owned())
        .spawn(move || {
            let _ = child.wait();
        })
        .map(|_| ())
}

fn open_with_default_browser(url: &str) -> io::Result<()> {
    let mut child = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    thread::Builder::new()
        .name("snipchord-xdg-open-url".to_owned())
        .spawn(move || {
            let _ = child.wait();
        })
        .map(|_| ())
}

fn earliest_deadline(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

fn poll_timeout_ms(deadline: Option<Instant>) -> i32 {
    let Some(deadline) = deadline else {
        return -1;
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return 0;
    }
    // Round up so a sub-millisecond deadline cannot turn into a busy loop.
    let millis = remaining.as_millis().saturating_add(1);
    i32::try_from(millis).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{parse_args, Mode, ParsedCommand};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_mode_defaults_to_region() {
        assert_eq!(
            parse_args(&args(&["snipchord"])),
            Ok(ParsedCommand::Mode(Mode::Region))
        );
    }

    #[test]
    fn informational_flags_do_not_need_x11() {
        assert_eq!(
            parse_args(&args(&["snipchord", "--help", "--bogus"])),
            Ok(ParsedCommand::Help)
        );
        assert_eq!(
            parse_args(&args(&["snipchord", "--version"])),
            Ok(ParsedCommand::Version)
        );
    }

    #[test]
    fn modes_are_mutually_exclusive() {
        assert!(parse_args(&args(&["snipchord", "--demo", "--fullscreen"])).is_err());
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert!(parse_args(&args(&["snipchord", "--unknown"])).is_err());
    }

    #[test]
    fn explicit_destinations_are_parsed_independently_from_capture_shape() {
        assert_eq!(
            parse_args(&args(&["snipchord", "--region", "--clipboard"])),
            Ok(ParsedCommand::Mode(Mode::RegionClipboard))
        );
        assert_eq!(
            parse_args(&args(&["snipchord", "--save"])),
            Ok(ParsedCommand::Mode(Mode::RegionSave))
        );
        assert_eq!(
            parse_args(&args(&["snipchord", "--fullscreen", "--clipboard"])),
            Ok(ParsedCommand::Mode(Mode::FullscreenClipboard))
        );
        assert_eq!(
            parse_args(&args(&["snipchord", "--fullscreen", "--save"])),
            Ok(ParsedCommand::Mode(Mode::FullscreenSave))
        );
    }

    #[test]
    fn destination_flags_are_mutually_exclusive() {
        assert!(parse_args(&args(&["snipchord", "--clipboard", "--save"])).is_err());
        assert!(parse_args(&args(&["snipchord", "--region", "--fullscreen"])).is_err());
        assert!(parse_args(&args(&["snipchord", "--preferences", "--save"])).is_err());
        assert!(parse_args(&args(&["snipchord", "--about", "--save"])).is_err());
        assert!(parse_args(&args(&["snipchord", "--demo", "--clipboard"])).is_err());
    }

    #[test]
    fn about_mode_is_parsed_without_capture_options() {
        assert_eq!(
            parse_args(&args(&["snipchord", "--about"])),
            Ok(ParsedCommand::Mode(Mode::About))
        );
    }

    #[test]
    fn save_directory_is_a_display_free_configuration_command() {
        assert_eq!(
            parse_args(&args(&["snipchord", "--save-dir", "~/Downloads"])),
            Ok(ParsedCommand::ConfigureOutputDirectory(PathBuf::from(
                "~/Downloads"
            )))
        );
        assert_eq!(
            parse_args(&args(&["snipchord", "--save-dir=~/Pictures"])),
            Ok(ParsedCommand::ConfigureOutputDirectory(PathBuf::from(
                "~/Pictures"
            )))
        );
        assert!(parse_args(&args(&["snipchord", "--save-dir"])).is_err());
        assert!(parse_args(&args(&["snipchord", "--save-dir", "--save"])).is_err());
        assert!(parse_args(&args(&["snipchord", "--save-dir=--save"])).is_err());
        assert!(parse_args(&args(&["snipchord", "--save-dir", "/tmp", "--save"])).is_err());
    }
}
