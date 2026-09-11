//! Raw XI2 hotkey handling for shortcuts that GNOME normally dispatches.
//!
//! GNOME custom shortcuts are normally delivered as a process launch.  An
//! application menu can hold an X11 pointer/keyboard grab while the shortcut
//! is pressed, however, which leaves the launched process unable to claim the
//! input needed by the selection overlay.  XI2 raw key events are delivered
//! independently of those client grabs, so the resident process can start the
//! capture from the same X connection.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use x11_dl::keysym;
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{
    ConnectionExt as XInputConnectionExt, Device, EventMask, KeyEventFlags, XIEventMask,
};
use x11rb::protocol::xproto::{
    ConnectionExt as XProtoConnectionExt, Keycode, Keysym, MappingNotifyEvent,
};
use x11rb::protocol::Event;

use crate::app::Mode;
use crate::shortcuts;
use crate::x11::{X11Context, X11Result};

const SHIFT_MASK: u16 = 1 << 0;
const LOCK_MASK: u16 = 1 << 1;
const CONTROL_MASK: u16 = 1 << 2;
const MOD1_MASK: u16 = 1 << 3;
const MOD2_MASK: u16 = 1 << 4;
const MOD3_MASK: u16 = 1 << 5;
const MOD4_MASK: u16 = 1 << 6;
const MOD5_MASK: u16 = 1 << 7;

const XI2_MAJOR: u16 = 2;
const XI2_MINIMUM_MINOR: u16 = 1;
const SETTINGS_POLL_INTERVAL: Duration = Duration::from_secs(2);
const DUPLICATE_WINDOW: Duration = Duration::from_millis(1_000);

type ConfiguredBindings = Vec<(Vec<String>, String)>;

/// A raw listener is optional.  X11 servers without XI2 2.1 continue to use
/// the existing GNOME process-launch path, while a supported server returns a
/// listener that can react to keys even when another client owns the grab.
pub struct Hotkeys {
    bindings: Vec<Binding>,
    keymap: KeyMap,
    pressed: HashSet<Keycode>,
    fired: Vec<(Mode, Keycode)>,
    configured: ConfiguredBindings,
    settings_watcher: SettingsWatcher,
    mapping_dirty: bool,
    /// Raw and process-launch actions are retained briefly so either one can
    /// consume the other when the two paths race for the same physical key.
    last_raw: Option<(Mode, Instant)>,
    last_external: Option<(Mode, Instant)>,
}

#[derive(Clone, Debug)]
struct Binding {
    mode: Mode,
    keycodes: HashSet<Keycode>,
    modifiers: u16,
}

#[derive(Clone, Debug, Default)]
struct KeyMap {
    keysyms: HashMap<Keycode, Vec<Keysym>>,
    modifier_masks: HashMap<Keycode, u16>,
    ignored_modifiers: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedBinding {
    modifiers: Vec<String>,
    key: String,
}

struct SettingsWatcher {
    receiver: Receiver<Option<ConfiguredBindings>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SettingsWatcher {
    fn start() -> Self {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("snipchord-hotkey-settings".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    let mut remaining = SETTINGS_POLL_INTERVAL;
                    while !remaining.is_zero() {
                        if thread_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let step = remaining.min(Duration::from_millis(50));
                        thread::sleep(step);
                        remaining = remaining.saturating_sub(step);
                    }
                    if thread_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if sender
                        .send(shortcuts::configured_capture_shortcuts())
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .ok();
        Self {
            receiver,
            stop,
            handle,
        }
    }

    fn latest(&self) -> Option<Option<ConfiguredBindings>> {
        let mut latest = None;
        loop {
            match self.receiver.try_recv() {
                Ok(value) => latest = Some(value),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        latest
    }
}

impl Drop for SettingsWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Hotkeys {
    /// Initialize XI2 raw key selection and load the current GNOME-managed
    /// screenshot bindings. `Ok(None)` means XI2 is unavailable or too old;
    /// callers should keep the established process-launch path in that case.
    pub fn new(context: &X11Context) -> X11Result<Option<Self>> {
        let version = match context
            .conn
            .xinput_xi_query_version(XI2_MAJOR, XI2_MINIMUM_MINOR)
        {
            Ok(cookie) => match cookie.reply() {
                Ok(version) => version,
                Err(error) => {
                    eprintln!("snipchord: raw hotkeys unavailable: {error}");
                    return Ok(None);
                }
            },
            Err(error) => {
                eprintln!("snipchord: raw hotkeys unavailable: {error}");
                return Ok(None);
            }
        };

        if version.major_version < XI2_MAJOR
            || (version.major_version == XI2_MAJOR && version.minor_version < XI2_MINIMUM_MINOR)
        {
            eprintln!(
                "snipchord: raw hotkeys require XI2.1 (server has XI2.{}.{})",
                version.major_version, version.minor_version
            );
            return Ok(None);
        }

        let masks = [EventMask {
            deviceid: Device::ALL_MASTER.into(),
            mask: vec![XIEventMask::RAW_KEY_PRESS | XIEventMask::RAW_KEY_RELEASE],
        }];
        let select_result = context
            .conn
            .xinput_xi_select_events(context.root(), &masks)
            .map_err(|error| error.to_string())
            .and_then(|cookie| cookie.check().map_err(|error| error.to_string()));
        if let Err(error) = select_result {
            eprintln!("snipchord: raw hotkeys unavailable: {error}");
            return Ok(None);
        }
        context.conn.flush()?;

        let configured = shortcuts::configured_capture_shortcuts().unwrap_or_default();
        let mut hotkeys = Self {
            bindings: Vec::new(),
            keymap: KeyMap::default(),
            pressed: HashSet::new(),
            fired: Vec::new(),
            configured,
            settings_watcher: SettingsWatcher::start(),
            mapping_dirty: true,
            last_raw: None,
            last_external: None,
        };
        hotkeys.reload(context)?;
        hotkeys.pressed = query_pressed_keys(context).unwrap_or_default();
        Ok(Some(hotkeys))
    }

    /// Process one event from the resident app's normal X11 queue.
    ///
    /// The returned mode is ready for the app's existing capture dispatcher.
    /// Raw events do not contain modifier state, so the listener reconstructs
    /// it from the raw press/release stream and the current keyboard mapping.
    pub fn handle_event(&mut self, event: &Event) -> Option<Mode> {
        match event {
            Event::XinputRawKeyPress(event) => {
                let keycode = u8::try_from(event.detail).ok()?;
                if !self.pressed.insert(keycode) {
                    // XI2 servers generally report autorepeat as another raw
                    // press without a release. Never fire a second capture
                    // while the physical shortcut remains held.
                    return None;
                }

                let modifiers = self.current_modifiers();
                let matching = self
                    .bindings
                    .iter()
                    .find(|binding| {
                        !self.fired.iter().any(|(mode, fired_keycode)| {
                            *mode == binding.mode && *fired_keycode == keycode
                        }) && binding.keycodes.contains(&keycode)
                            && binding.matches_modifiers(modifiers, self.keymap.ignored_modifiers)
                    })
                    .map(|binding| binding.mode);
                let Some(mode) = matching else {
                    return None;
                };

                self.fired.push((mode, keycode));
                if self.last_external.is_some_and(|(previous, when)| {
                    previous == mode && when.elapsed() <= DUPLICATE_WINDOW
                }) {
                    self.last_external = None;
                    return None;
                }
                self.last_raw = Some((mode, Instant::now()));
                Some(mode)
            }
            Event::XinputRawKeyRelease(event) => {
                if is_key_repeat(event.flags) {
                    // Some XInput servers expose autorepeat as a synthetic
                    // release/press pair. Keep the original key down and its
                    // fired marker intact until the real release arrives.
                    return None;
                }
                if let Ok(keycode) = u8::try_from(event.detail) {
                    self.pressed.remove(&keycode);
                    // Releasing any key permits a fresh complete chord. This
                    // also handles a modifier being released before the final
                    // key's release on keyboards with rollover.
                    self.fired.clear();
                }
                None
            }
            Event::MappingNotify(MappingNotifyEvent { .. }) => {
                self.mapping_dirty = true;
                None
            }
            Event::XinputDeviceChanged(_) | Event::XinputHierarchy(_) => {
                self.mapping_dirty = true;
                None
            }
            _ => None,
        }
    }

    /// Reload settings and keyboard mapping when the event loop reaches its
    /// periodic maintenance point. A settings change never installs an
    /// implicit default: only configured, non-empty bindings are listened to.
    pub fn tick(&mut self, context: &X11Context) -> X11Result<()> {
        if self.mapping_dirty {
            self.reload(context)?;
        }
        if let Some(configured) = self.settings_watcher.latest() {
            self.configured = configured.unwrap_or_default();
            self.rebuild_bindings();
        }
        if self
            .last_raw
            .is_some_and(|(_, when)| when.elapsed() > DUPLICATE_WINDOW)
        {
            self.last_raw = None;
        }
        if self
            .last_external
            .is_some_and(|(_, when)| when.elapsed() > DUPLICATE_WINDOW)
        {
            self.last_external = None;
        }
        Ok(())
    }

    /// Reconcile modifier state with the server's authoritative key state
    /// after the event queue has been drained.
    ///
    /// Raw release events can be lost while a foreign client owns a keyboard
    /// grab.  Keeping a stale modifier in `pressed` would make a later plain
    /// `Shift+4` look like the configured `Alt+Shift+4` binding.  Callers do
    /// this only after processing queued events so a valid chord is still
    /// reconstructed from its raw press sequence before the snapshot wins.
    /// Keep non-modifier keys owned by the raw stream: a new key press may
    /// already be represented in the server snapshot while its raw event is
    /// still queued on this connection.
    pub fn synchronize_pressed(&mut self, context: &X11Context) {
        let Some(pressed) = query_pressed_keys(context) else {
            return;
        };
        self.reconcile_pressed(pressed);
    }

    fn reconcile_pressed(&mut self, pressed: HashSet<Keycode>) {
        let mut changed = false;
        for keycode in self.keymap.modifier_masks.keys() {
            let was_pressed = self.pressed.contains(keycode);
            let is_pressed = pressed.contains(keycode);
            if was_pressed == is_pressed {
                continue;
            }
            changed = true;
            if is_pressed {
                self.pressed.insert(*keycode);
            } else {
                self.pressed.remove(keycode);
            }
        }
        if changed {
            self.fired.clear();
        }
    }

    /// Force a keyboard mapping reload and re-apply the current settings.
    pub fn reload(&mut self, context: &X11Context) -> X11Result<()> {
        self.keymap = KeyMap::load(context)?;
        self.rebuild_bindings();
        self.mapping_dirty = false;
        Ok(())
    }

    fn rebuild_bindings(&mut self) {
        self.bindings = self
            .configured
            .clone()
            .into_iter()
            .filter_map(|(arguments, binding)| {
                let mode = mode_for_arguments(&arguments)?;
                match Binding::parse(mode, &binding, &self.keymap) {
                    Some(binding) => Some(binding),
                    None => {
                        if !binding.trim().is_empty() {
                            eprintln!(
                                "snipchord: ignoring unsupported screenshot binding `{binding}`"
                            );
                        }
                        None
                    }
                }
            })
            .collect();
    }

    /// Return true when a GNOME-launched copy of a raw action should be
    /// ignored. A mode that was not recently started by raw input is left
    /// untouched, preserving direct CLI and ordinary GNOME invocations.
    pub fn suppress_duplicate(&mut self, mode: Mode) -> bool {
        if self
            .last_raw
            .is_some_and(|(previous, when)| previous == mode && when.elapsed() <= DUPLICATE_WINDOW)
        {
            self.last_raw = None;
            true
        } else {
            self.last_external = Some((mode, Instant::now()));
            false
        }
    }

    fn current_modifiers(&self) -> u16 {
        self.pressed
            .iter()
            .filter_map(|keycode| self.keymap.modifier_masks.get(keycode))
            .fold(0, |state, mask| state | mask)
    }
}

impl Binding {
    fn parse(mode: Mode, text: &str, keymap: &KeyMap) -> Option<Self> {
        let parsed = parse_binding(text)?;
        let target = keysym_for_name(&parsed.key)?;
        let keycodes = keymap
            .keysyms
            .iter()
            .filter_map(|(keycode, symbols)| symbols.contains(&target).then_some(*keycode))
            .collect::<HashSet<_>>();
        if keycodes.is_empty() {
            return None;
        }

        let modifiers = parsed.modifiers.iter().try_fold(0u16, |mask, modifier| {
            keymap
                .modifier_mask_for_name(modifier)
                .map(|modifier_mask| mask | modifier_mask)
        })?;
        Some(Self {
            mode,
            keycodes,
            modifiers,
        })
    }

    fn matches_modifiers(&self, current: u16, ignored: u16) -> bool {
        let current = current & !ignored;
        let required = self.modifiers & !ignored;
        current == required
    }
}

impl KeyMap {
    fn load(context: &X11Context) -> X11Result<Self> {
        let first = context.conn.setup().min_keycode;
        let count = context
            .conn
            .setup()
            .max_keycode
            .saturating_sub(first)
            .saturating_add(1);
        let mapping = context.conn.get_keyboard_mapping(first, count)?.reply()?;
        let stride = usize::from(mapping.keysyms_per_keycode);
        if stride == 0 {
            return Err("X11 keyboard mapping has no keysyms per keycode".into());
        }

        let mut keysyms = HashMap::new();
        for (index, symbols) in mapping.keysyms.chunks(stride).enumerate() {
            let Some(offset) = u8::try_from(index).ok() else {
                continue;
            };
            let keycode = first.saturating_add(offset);
            keysyms.insert(keycode, symbols.to_vec());
        }

        let modifier_reply = context.conn.get_modifier_mapping()?.reply()?;
        let stride = usize::from(modifier_reply.keycodes_per_modifier());
        let mut modifier_masks = HashMap::new();
        if stride != 0 {
            for (index, keycode) in modifier_reply.keycodes.iter().enumerate() {
                if *keycode == 0 {
                    continue;
                }
                let mask_index = index / stride;
                if mask_index < 8 {
                    let mask = 1u16 << mask_index;
                    modifier_masks
                        .entry(*keycode)
                        .and_modify(|value| *value |= mask)
                        .or_insert(mask);
                }
            }
        }

        // Core modifier slots are authoritative for Shift, Control and Lock,
        // but adding the symbol-derived masks makes the listener tolerate
        // servers that expose an incomplete modifier map during startup.
        let mut ignored_modifiers = LOCK_MASK;
        for (keycode, symbols) in &keysyms {
            let mut mask = modifier_masks.get(keycode).copied().unwrap_or(0);
            if contains_any(symbols, &[keysym::XK_Shift_L, keysym::XK_Shift_R]) {
                mask |= SHIFT_MASK;
            }
            if contains_any(symbols, &[keysym::XK_Control_L, keysym::XK_Control_R]) {
                mask |= CONTROL_MASK;
            }
            if contains_any(symbols, &[keysym::XK_Caps_Lock, keysym::XK_Shift_Lock]) {
                mask |= LOCK_MASK;
            }
            if contains_any(symbols, &[keysym::XK_Num_Lock]) {
                ignored_modifiers |= mask;
            }
            if mask != 0 {
                modifier_masks.insert(*keycode, mask);
            }
        }

        Ok(Self {
            keysyms,
            modifier_masks,
            ignored_modifiers,
        })
    }

    fn modifier_mask_for_name(&self, name: &str) -> Option<u16> {
        let normalized = name.to_ascii_lowercase();
        match normalized.as_str() {
            "shift" => Some(SHIFT_MASK),
            "control" | "ctrl" | "primary" => Some(CONTROL_MASK),
            "lock" | "capslock" => Some(LOCK_MASK),
            "alt" | "meta" => self
                .mask_for_symbols(&[keysym::XK_Alt_L, keysym::XK_Alt_R])
                .or_else(|| self.mask_for_symbols(&[keysym::XK_Meta_L, keysym::XK_Meta_R]))
                .or(Some(MOD1_MASK)),
            "super" | "win" | "logo" | "hyper" => self
                .mask_for_symbols(&[keysym::XK_Super_L, keysym::XK_Super_R])
                .or_else(|| self.mask_for_symbols(&[keysym::XK_Win_L, keysym::XK_Win_R]))
                .or_else(|| self.mask_for_symbols(&[keysym::XK_Hyper_L, keysym::XK_Hyper_R]))
                .or(Some(MOD4_MASK)),
            "mod1" => Some(MOD1_MASK),
            "mod2" => Some(MOD2_MASK),
            "mod3" => Some(MOD3_MASK),
            "mod4" => Some(MOD4_MASK),
            "mod5" => Some(MOD5_MASK),
            _ => None,
        }
    }

    fn mask_for_symbols(&self, target: &[Keysym]) -> Option<u16> {
        self.keysyms.iter().find_map(|(keycode, symbols)| {
            contains_any(symbols, target)
                .then(|| self.modifier_masks.get(keycode).copied())
                .flatten()
        })
    }
}

fn contains_any(values: &[Keysym], targets: &[Keysym]) -> bool {
    values.iter().any(|value| targets.contains(value))
}

fn is_key_repeat(flags: KeyEventFlags) -> bool {
    u32::from(flags) & u32::from(KeyEventFlags::KEY_REPEAT) != 0
}

fn query_pressed_keys(context: &X11Context) -> Option<HashSet<Keycode>> {
    let Ok(cookie) = context.conn.query_keymap() else {
        return None;
    };
    let Ok(reply) = cookie.reply() else {
        return None;
    };
    let mut pressed = HashSet::new();
    for (byte_index, byte) in reply.keys.iter().enumerate() {
        for bit in 0..8 {
            if byte & (1 << bit) != 0 {
                let code = byte_index.saturating_mul(8).saturating_add(bit);
                if let Ok(code) = u8::try_from(code) {
                    pressed.insert(code);
                }
            }
        }
    }
    Some(pressed)
}

fn mode_for_arguments(arguments: &[String]) -> Option<Mode> {
    match arguments {
        [region] if region == "--region" => Some(Mode::Region),
        [region, clipboard] if region == "--region" && clipboard == "--clipboard" => {
            Some(Mode::RegionClipboard)
        }
        [region, save] if region == "--region" && save == "--save" => Some(Mode::RegionSave),
        [fullscreen] if fullscreen == "--fullscreen" => Some(Mode::Fullscreen),
        [fullscreen, clipboard] if fullscreen == "--fullscreen" && clipboard == "--clipboard" => {
            Some(Mode::FullscreenClipboard)
        }
        [fullscreen, save] if fullscreen == "--fullscreen" && save == "--save" => {
            Some(Mode::FullscreenSave)
        }
        _ => None,
    }
}

fn parse_binding(value: &str) -> Option<ParsedBinding> {
    let mut modifiers = Vec::new();
    let mut remainder = value.trim();
    while let Some(stripped) = remainder.strip_prefix('<') {
        let end = stripped.find('>')?;
        let modifier = &stripped[..end];
        if modifier.is_empty() {
            return None;
        }
        modifiers.push(modifier.to_owned());
        remainder = &stripped[end + 1..];
    }
    let key = remainder.trim();
    (!key.is_empty() && !key.contains('<') && !key.contains('>')).then(|| ParsedBinding {
        modifiers,
        key: key.to_owned(),
    })
}

fn keysym_for_name(value: &str) -> Option<Keysym> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16).ok();
    }
    if value.chars().count() == 1 {
        let character = value.chars().next()? as u32;
        return (character <= 0xFF)
            .then_some(character)
            .or_else(|| (character <= 0x10_FFFF).then_some(0x01_00_00_00 | character));
    }

    let lower = value.to_ascii_lowercase();
    let symbol = match lower.as_str() {
        "backspace" => keysym::XK_BackSpace,
        "tab" => keysym::XK_Tab,
        "return" | "enter" => keysym::XK_Return,
        "escape" | "esc" => keysym::XK_Escape,
        "delete" => keysym::XK_Delete,
        "home" => keysym::XK_Home,
        "left" => keysym::XK_Left,
        "up" => keysym::XK_Up,
        "right" => keysym::XK_Right,
        "down" => keysym::XK_Down,
        "prior" | "page_up" => keysym::XK_Page_Up,
        "next" | "page_down" => keysym::XK_Page_Down,
        "end" => keysym::XK_End,
        "print" | "printscreen" | "sys_req" => keysym::XK_Print,
        "insert" => keysym::XK_Insert,
        "menu" => keysym::XK_Menu,
        "pause" => keysym::XK_Pause,
        "scroll_lock" => keysym::XK_Scroll_Lock,
        "num_lock" => keysym::XK_Num_Lock,
        "space" => keysym::XK_space,
        "numbersign" => keysym::XK_numbersign,
        "percent" => keysym::XK_percent,
        "plus" => keysym::XK_plus,
        "minus" => keysym::XK_minus,
        "equal" => keysym::XK_equal,
        "comma" => keysym::XK_comma,
        "period" => keysym::XK_period,
        "slash" => keysym::XK_slash,
        "backslash" => keysym::XK_backslash,
        "bracketleft" => keysym::XK_bracketleft,
        "bracketright" => keysym::XK_bracketright,
        "grave" | "quoteleft" => keysym::XK_grave,
        "kp_enter" => keysym::XK_KP_Enter,
        "kp_space" => keysym::XK_KP_Space,
        "kp_add" => keysym::XK_KP_Add,
        "kp_subtract" => keysym::XK_KP_Subtract,
        "kp_multiply" => keysym::XK_KP_Multiply,
        "kp_divide" => keysym::XK_KP_Divide,
        "xf86audiomute" => keysym::XF86XK_AudioMute,
        "xf86audioraisevolume" => keysym::XF86XK_AudioRaiseVolume,
        "xf86audiolowervolume" => keysym::XF86XK_AudioLowerVolume,
        "xf86audioplay" => keysym::XF86XK_AudioPlay,
        "xf86audiostop" => keysym::XF86XK_AudioStop,
        "xf86audionext" => keysym::XF86XK_AudioNext,
        "xf86audioprev" => keysym::XF86XK_AudioPrev,
        "xf86monbrightnessup" => keysym::XF86XK_MonBrightnessUp,
        "xf86monbrightnessdown" => keysym::XF86XK_MonBrightnessDown,
        _ => {
            if let Some(number) = lower
                .strip_prefix('f')
                .and_then(|number| number.parse::<u32>().ok())
            {
                if (1..=35).contains(&number) {
                    keysym::XK_F1 + number - 1
                } else {
                    return None;
                }
            } else if let Some(number) = lower
                .strip_prefix("kp_")
                .and_then(|number| number.parse::<u32>().ok())
            {
                if number <= 9 {
                    keysym::XK_KP_0 + number
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
    };
    Some(symbol)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use x11rb::protocol::xinput::{self, KeyEventFlags};
    use x11rb::protocol::Event;

    use super::{is_key_repeat, keysym_for_name, mode_for_arguments, parse_binding, Mode};

    #[test]
    fn parses_gnome_binding_tokens() {
        assert_eq!(
            parse_binding("<Control><Alt><Shift>4"),
            Some(super::ParsedBinding {
                modifiers: vec!["Control".to_owned(), "Alt".to_owned(), "Shift".to_owned()],
                key: "4".to_owned(),
            })
        );
        assert!(parse_binding("<Control>").is_none());
        assert!(parse_binding("<Control><>4").is_none());
    }

    #[test]
    fn resolves_common_gnome_keysyms() {
        assert_eq!(keysym_for_name("3"), Some(0x33));
        assert_eq!(keysym_for_name("numbersign"), Some(0x23));
        assert_eq!(keysym_for_name("Print"), Some(0xFF61));
        assert_eq!(keysym_for_name("F12"), Some(0xFFC9));
        assert_eq!(keysym_for_name("KP_7"), Some(0xFFB7));
    }

    #[test]
    fn maps_only_managed_capture_arguments() {
        assert_eq!(
            mode_for_arguments(&["--region".to_owned(), "--clipboard".to_owned()]),
            Some(Mode::RegionClipboard)
        );
        assert_eq!(
            mode_for_arguments(&["--fullscreen".to_owned(), "--save".to_owned()]),
            Some(Mode::FullscreenSave)
        );
        assert_eq!(mode_for_arguments(&["--preferences".to_owned()]), None);
    }

    #[test]
    fn recognizes_xi_key_repeat_flag() {
        assert!(is_key_repeat(KeyEventFlags::KEY_REPEAT));
        assert!(!is_key_repeat(KeyEventFlags::default()));
    }

    #[test]
    fn repeat_release_does_not_rearm_a_fired_binding() {
        let keycode: super::Keycode = 42;
        let mode = Mode::Region;
        let mut keysyms = HashMap::new();
        keysyms.insert(keycode, vec![0x33]);
        let mut hotkeys = super::Hotkeys {
            bindings: vec![super::Binding {
                mode,
                keycodes: HashSet::from([keycode]),
                modifiers: 0,
            }],
            keymap: super::KeyMap {
                keysyms,
                modifier_masks: HashMap::new(),
                ignored_modifiers: 0,
            },
            pressed: HashSet::new(),
            fired: Vec::new(),
            configured: Vec::new(),
            settings_watcher: super::SettingsWatcher::start(),
            mapping_dirty: false,
            last_raw: None,
            last_external: None,
        };

        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                keycode,
                KeyEventFlags::default()
            ))),
            Some(mode)
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyRelease(raw_key(
                keycode,
                KeyEventFlags::KEY_REPEAT,
            ))),
            None
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                keycode,
                KeyEventFlags::KEY_REPEAT,
            ))),
            None
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyRelease(raw_key(
                keycode,
                KeyEventFlags::default(),
            ))),
            None
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                keycode,
                KeyEventFlags::default(),
            ))),
            Some(mode)
        );
    }

    #[test]
    fn authoritative_pressed_state_prevents_stale_modifiers_from_matching_shift_four() {
        let alt: super::Keycode = 64;
        let control: super::Keycode = 37;
        let shift: super::Keycode = 50;
        let key: super::Keycode = 13;
        let alt_mode = Mode::RegionSave;
        let control_alt_mode = Mode::RegionClipboard;
        let mut keysyms = HashMap::new();
        keysyms.insert(key, vec![0x34]);
        let mut hotkeys = super::Hotkeys {
            bindings: vec![
                super::Binding {
                    mode: alt_mode,
                    keycodes: HashSet::from([key]),
                    modifiers: super::SHIFT_MASK | super::MOD1_MASK,
                },
                super::Binding {
                    mode: control_alt_mode,
                    keycodes: HashSet::from([key]),
                    modifiers: super::SHIFT_MASK | super::MOD1_MASK | super::CONTROL_MASK,
                },
            ],
            keymap: super::KeyMap {
                keysyms,
                modifier_masks: HashMap::from([
                    (alt, super::MOD1_MASK),
                    (control, super::CONTROL_MASK),
                    (shift, super::SHIFT_MASK),
                ]),
                ignored_modifiers: 0,
            },
            pressed: HashSet::new(),
            fired: Vec::new(),
            configured: Vec::new(),
            settings_watcher: super::SettingsWatcher::start(),
            mapping_dirty: false,
            last_raw: None,
            last_external: None,
        };

        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                control,
                KeyEventFlags::default()
            ))),
            None
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                alt,
                KeyEventFlags::default()
            ))),
            None
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                shift,
                KeyEventFlags::default()
            ))),
            None
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                key,
                KeyEventFlags::default()
            ))),
            Some(control_alt_mode)
        );

        // The Alt and Control releases were missed while a foreign client
        // owned the grab. Releasing the other keys leaves stale modifiers in
        // the raw tracker.
        hotkeys.handle_event(&Event::XinputRawKeyRelease(raw_key(
            key,
            KeyEventFlags::default(),
        )));
        hotkeys.handle_event(&Event::XinputRawKeyRelease(raw_key(
            shift,
            KeyEventFlags::default(),
        )));
        assert_eq!(hotkeys.pressed, HashSet::from([control, alt]));

        // The post-queue X11 snapshot is authoritative and clears the stale
        // modifier before the next plain Shift+4 is considered.
        hotkeys.reconcile_pressed(HashSet::new());
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                shift,
                KeyEventFlags::default()
            ))),
            None
        );
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                key,
                KeyEventFlags::default()
            ))),
            None
        );

        hotkeys.handle_event(&Event::XinputRawKeyRelease(raw_key(
            key,
            KeyEventFlags::default(),
        )));
        hotkeys.handle_event(&Event::XinputRawKeyRelease(raw_key(
            shift,
            KeyEventFlags::default(),
        )));

        // A valid Alt+Shift chord remains valid after synchronization when
        // its modifiers and target are physically held. The target is left
        // to the raw stream so its queued press still fires once.
        hotkeys.reconcile_pressed(HashSet::from([alt, shift, key]));
        assert!(!hotkeys.pressed.contains(&key));
        assert_eq!(
            hotkeys.handle_event(&Event::XinputRawKeyPress(raw_key(
                key,
                KeyEventFlags::default()
            ))),
            Some(alt_mode)
        );
    }

    fn raw_key(detail: super::Keycode, flags: KeyEventFlags) -> xinput::RawKeyPressEvent {
        xinput::RawKeyPressEvent {
            response_type: 0,
            extension: 0,
            sequence: 0,
            length: 0,
            event_type: xinput::RAW_KEY_PRESS_EVENT,
            deviceid: 0,
            time: 0,
            detail: u32::from(detail),
            sourceid: 0,
            flags,
            valuator_mask: Vec::new(),
            axisvalues: Vec::new(),
            axisvalues_raw: Vec::new(),
        }
    }
}
