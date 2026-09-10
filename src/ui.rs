//! Lightweight X11 UI surfaces.
//!
//! The UI deliberately uses only X11 core protocol requests.  A screenshot is uploaded once to
//! a server-side pixmap; redraws then copy that pixmap and draw a small number of primitives.  The
//! application owns the event loop and the captured `RgbaImage`; this module owns transient window
//! state and turns input into [`UiAction`] values.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::cmp::{max, min};
use std::ffi::CString;
use std::io;
use std::os::raw::{c_int, c_uchar};
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::image::{BitsPerPixel, Image as X11Image, ImageOrder, ScanlinePad};
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::render::{self, ConnectionExt as RenderConnectionExt};
use x11rb::protocol::shape::{
    ConnectionExt as ShapeConnectionExt, SK as ShapeKind, SO as ShapeOperation,
};
use x11rb::protocol::xproto::{
    self, CapStyle, ClipOrdering, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask,
    FillStyle, Font, Gcontext, GrabMode, GrabStatus, InputFocus, JoinStyle, Keycode, Pixmap,
    PropMode, Rectangle, StackMode, Window,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::CURRENT_TIME;

use crate::app::UiAction;
use crate::geometry::Rect;
use crate::image::RgbaImage;
use crate::server_capture::ServerCapture;
use crate::settings::Settings;
use crate::shortcuts::ShortcutList;
use crate::window_capture::{WindowPicker, WindowTarget};
use crate::x11::{encode_rgb, X11Context, X11Result};

const PREVIEW_TIMEOUT: Duration = Duration::from_secs(2);
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);
// Keep the transient preview close to the size of macOS's screenshot thumbnail.  The window
// grows with the screenshot's aspect ratio but never becomes a second preview/editor surface.
const PREVIEW_MAX_WIDTH: u32 = 220;
const PREVIEW_MAX_HEIGHT: u32 = 140;
// The thumbnail itself is the surface.  A five-pixel neutral frame leaves the complete image
// inside the rounded X11 shape, so high-contrast pixels at the source corners are not cut off.
const PREVIEW_BORDER: i32 = 5;
const PREVIEW_RADIUS: u16 = 8;
// A global shortcut can still own the keyboard for a short period after it
// has delivered the capture command. Keep the pointer responsive and retry
// keyboard ownership on the event-loop timer instead of blocking selection
// setup on a second synchronous grab attempt.
const KEYBOARD_GRAB_RETRY: Duration = Duration::from_millis(8);

// The settings surfaces deliberately stay compact enough to feel like tray
// popovers while leaving room for a real two-column layout.  The left rail
// gives the read-only shortcut list a clear home instead of pushing it below
// the capture options.
const PREFERENCES_WIDTH: u16 = 620;
const PREFERENCES_HEIGHT: u16 = 480;
const ABOUT_WIDTH: u16 = 440;
const ABOUT_HEIGHT: u16 = 220;

const PREFERENCES_SIDEBAR_WIDTH: i32 = 176;
const PREFERENCES_HEADER_HEIGHT: i32 = 72;
const PREFERENCES_CONTENT_X: i32 = 208;
const PREFERENCES_CONTENT_WIDTH: u16 = 376;

const GENERAL_TAB_Y: i32 = 104;
const SHORTCUTS_TAB_Y: i32 = 152;
const TAB_HEIGHT: i32 = 40;

const GENERAL_PREVIEW_Y: i32 = 160;
const GENERAL_SAVE_Y: i32 = 244;

const XK_SPACE: u32 = 0x20;
const XK_RETURN: u32 = 0xff0d;
const XK_KP_ENTER: u32 = 0xff8d;
const XK_ESCAPE: u32 = 0xff1b;

/// A root-window coordinate in X11 pixels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// A monitor rectangle in root-window coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MonitorRect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && i64::from(point.x) < i64::from(self.x) + i64::from(self.width)
            && i64::from(point.y) < i64::from(self.y) + i64::from(self.height)
    }
}

/// The small image view needed by the X11 uploader.  `RgbaImage` can be passed directly to the
/// public methods below; this type is useful to callers that have a borrowed buffer.
#[derive(Clone, Copy, Debug)]
pub struct RgbaView<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

impl<'a> RgbaView<'a> {
    pub fn new(width: u32, height: u32, pixels: &'a [u8]) -> Self {
        Self {
            width,
            height,
            pixels,
        }
    }

    fn from_image(image: &'a RgbaImage) -> Self {
        Self::new(image.width(), image.height(), image.pixels())
    }

    fn validate(self) -> X11Result<()> {
        let expected = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| invalid("invalid image dimensions"))?;
        if self.pixels.len() != expected {
            return Err(invalid("RGBA image buffer has the wrong length"));
        }
        Ok(())
    }
}

struct ServerImage {
    pixmap: Pixmap,
    width: u16,
    height: u16,
    /// Native-selection pixmaps are owned by `ServerCapture`; uploaded
    /// legacy/preview pixmaps are owned directly by this wrapper.
    owned: bool,
}

struct SelectionState {
    window: Window,
    /// Complete selection frame assembled off-screen before it is presented. Keeping the
    /// intermediate copies out of the mapped window prevents the selection backdrop and the
    /// restored selection from becoming visible as separate frames while the pointer moves.
    frame: Pixmap,
    frame_picture: Option<render::Picture>,
    snapshot: ServerImage,
    backdrop: ServerImage,
    /// When present, `snapshot` and `backdrop` are borrowed X11 pixmaps owned by
    /// this native capture. The client-upload fallback leaves this unset and
    /// owns the two `ServerImage` pixmaps directly.
    native_capture: Option<ServerCapture>,
    /// Window that owns the active pointer/keyboard grabs.  The fast path
    /// keeps the grabs on the root while the server-side snapshot is prepared;
    /// queued events therefore arrive with `event == grab_window` even before
    /// the visible selection surface is ready.
    grab_window: Window,
    previous_focus: Window,
    pointer: Point,
    anchor: Option<Point>,
    rect: Option<Rect>,
    moving: bool,
    resize_offset: Point,
    last_pointer: Point,
    window_pick: bool,
    window_target: Option<WindowTarget>,
    /// Logical Space state used to ignore repeated KeyPress events while the key is held.
    space_down: bool,
    pointer_grabbed: bool,
    keyboard_grabbed: bool,
    keyboard_retry_at: Option<Instant>,
}

struct PreviewState {
    window: Window,
    thumbnail: ServerImage,
    width: u16,
    height: u16,
    deadline: Option<Instant>,
}

struct PreferencesState {
    window: Window,
    settings: Settings,
    shortcuts: ShortcutList,
    tab: PreferencesTab,
    previous_focus: Window,
    wm_protocols: u32,
    wm_delete_window: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreferencesTab {
    General,
    Shortcuts,
}

struct AboutState {
    window: Window,
    previous_focus: Window,
    wm_protocols: u32,
    wm_delete_window: u32,
}

struct NoticeState {
    window: Window,
    width: u16,
    height: u16,
    title: String,
    body: String,
    deadline: Instant,
}

/// Root grabs installed before the expensive server snapshot is prepared.
/// Keeping them on the root avoids an input handoff while early events are
/// queued in the X11 connection.
struct CaptureCursor {
    pointer_grabbed: bool,
    keyboard_grabbed: bool,
}

struct Keycodes {
    escape: Keycode,
    space: Keycode,
    enter: Keycode,
    keypad_enter: Keycode,
}

struct Resources {
    copy_gc: Gcontext,
    selection_shadow_gc: Gcontext,
    outline_gc: Gcontext,
    window_pick_gc: Option<Gcontext>,
    window_pick_stipple: Option<Pixmap>,
    card_gc: Gcontext,
    panel_gc: Gcontext,
    panel_border_gc: Gcontext,
    primary_gc: Gcontext,
    frame_gc: Gcontext,
    button_gc: Gcontext,
    text_gc: Gcontext,
    font: Font,
    cursor: Option<xproto::Cursor>,
    cursor_font: Option<Font>,
    card_pixel: u32,
    frame_pixel: u32,
    window_pick_cursor: Option<xproto::Cursor>,
    window_pick_render: Option<WindowPickRender>,
    xft: OnceCell<Option<XftResources>>,
}

/// Optional anti-aliased text resources for the small settings surfaces.
///
/// The capture overlay deliberately stays on the protocol-only core-font path. Keeping Xft
/// isolated here gives Preferences and About a proportional system font without introducing a
/// toolkit or changing the latency-sensitive capture path. Xft uses its own Xlib connection
/// because x11rb does not expose the underlying `Display*` handle.
struct XftResources {
    xlib: x11_dl::xlib::Xlib,
    xft: x11_dl::xft::Xft,
    display: *mut x11_dl::xlib::Display,
    visual: *mut x11_dl::xlib::Visual,
    colormap: x11_dl::xlib::Colormap,
    title_font: *mut x11_dl::xft::XftFont,
    body_font: *mut x11_dl::xft::XftFont,
    small_font: *mut x11_dl::xft::XftFont,
    primary: x11_dl::xft::XftColor,
    secondary: x11_dl::xft::XftColor,
    muted: x11_dl::xft::XftColor,
    button: x11_dl::xft::XftColor,
    primary_allocated: bool,
    secondary_allocated: bool,
    muted_allocated: bool,
    button_allocated: bool,
}

#[derive(Clone, Copy)]
enum XftTextStyle {
    Title,
    Body,
    Small,
    Button,
}

impl XftResources {
    fn try_new() -> Option<Self> {
        let xlib = x11_dl::xlib::Xlib::open().ok()?;
        let xft = x11_dl::xft::Xft::open().ok()?;
        let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
        if display.is_null() {
            return None;
        }
        let screen = unsafe { (xlib.XDefaultScreen)(display) };
        let visual = unsafe { (xlib.XDefaultVisual)(display, screen) };
        if visual.is_null() {
            unsafe {
                (xlib.XCloseDisplay)(display);
            }
            return None;
        }
        let colormap = unsafe { (xlib.XDefaultColormap)(display, screen) };
        let mut resources = Self {
            xlib,
            xft,
            display,
            visual,
            colormap,
            title_font: ptr::null_mut(),
            body_font: ptr::null_mut(),
            small_font: ptr::null_mut(),
            primary: empty_xft_color(),
            secondary: empty_xft_color(),
            muted: empty_xft_color(),
            button: empty_xft_color(),
            primary_allocated: false,
            secondary_allocated: false,
            muted_allocated: false,
            button_allocated: false,
        };

        resources.title_font = resources.open_font("Sans:style=Bold:size=18")?;
        resources.body_font = resources.open_font("Sans:size=13")?;
        resources.small_font = resources.open_font("Sans:size=11")?;
        if let Some(color) = resources.alloc_color("#f2f5fa") {
            resources.primary = color;
            resources.primary_allocated = true;
        }
        if let Some(color) = resources.alloc_color("#aeb9c9") {
            resources.secondary = color;
            resources.secondary_allocated = true;
        }
        if let Some(color) = resources.alloc_color("#7d899b") {
            resources.muted = color;
            resources.muted_allocated = true;
        }
        if let Some(color) = resources.alloc_color("#f2f5fa") {
            resources.button = color;
            resources.button_allocated = true;
        }
        if !(resources.primary_allocated
            && resources.secondary_allocated
            && resources.muted_allocated
            && resources.button_allocated)
        {
            return None;
        }
        Some(resources)
    }

    fn open_font(&self, pattern: &str) -> Option<*mut x11_dl::xft::XftFont> {
        let pattern = CString::new(pattern).ok()?;
        let font = unsafe {
            (self.xft.XftFontOpenName)(
                self.display,
                (self.xlib.XDefaultScreen)(self.display),
                pattern.as_ptr(),
            )
        };
        (!font.is_null()).then_some(font)
    }

    fn alloc_color(&self, name: &str) -> Option<x11_dl::xft::XftColor> {
        let Ok(name) = CString::new(name) else {
            return None;
        };
        let mut color = empty_xft_color();
        unsafe {
            (self.xft.XftColorAllocName)(
                self.display,
                self.visual,
                self.colormap,
                name.as_ptr(),
                &mut color,
            )
            .ne(&0)
            .then_some(color)
        }
    }

    fn draw_text(&self, drawable: Window, x: i32, baseline: i32, label: &str, style: XftTextStyle) {
        let (font, color) = match style {
            XftTextStyle::Title => (self.title_font, &self.primary),
            XftTextStyle::Body => (self.body_font, &self.primary),
            XftTextStyle::Small => (self.small_font, &self.secondary),
            XftTextStyle::Button => (self.body_font, &self.button),
        };
        if font.is_null() {
            return;
        }
        let bytes = label.as_bytes();
        let length = bytes.len().min(c_int::MAX as usize) as c_int;
        let draw = unsafe {
            (self.xft.XftDrawCreate)(
                self.display,
                drawable as x11_dl::xlib::Drawable,
                self.visual,
                self.colormap,
            )
        };
        if draw.is_null() {
            return;
        }
        unsafe {
            (self.xft.XftDrawStringUtf8)(
                draw,
                color,
                font,
                x,
                baseline,
                bytes.as_ptr() as *const c_uchar,
                length,
            );
            (self.xft.XftDrawDestroy)(draw);
            (self.xlib.XFlush)(self.display);
        }
    }

    fn sync(&self) {
        unsafe {
            (self.xlib.XSync)(self.display, 0);
        }
    }
}

impl Drop for XftResources {
    fn drop(&mut self) {
        unsafe {
            if self.primary_allocated {
                (self.xft.XftColorFree)(
                    self.display,
                    self.visual,
                    self.colormap,
                    &mut self.primary,
                );
            }
            if self.secondary_allocated {
                (self.xft.XftColorFree)(
                    self.display,
                    self.visual,
                    self.colormap,
                    &mut self.secondary,
                );
            }
            if self.muted_allocated {
                (self.xft.XftColorFree)(self.display, self.visual, self.colormap, &mut self.muted);
            }
            if self.button_allocated {
                (self.xft.XftColorFree)(self.display, self.visual, self.colormap, &mut self.button);
            }
            if !self.title_font.is_null() {
                (self.xft.XftFontClose)(self.display, self.title_font);
            }
            if !self.body_font.is_null() {
                (self.xft.XftFontClose)(self.display, self.body_font);
            }
            if !self.small_font.is_null() {
                (self.xft.XftFontClose)(self.display, self.small_font);
            }
            if !self.display.is_null() {
                (self.xlib.XCloseDisplay)(self.display);
            }
        }
    }
}

fn empty_xft_color() -> x11_dl::xft::XftColor {
    x11_dl::xft::XftColor {
        pixel: 0,
        color: x11_dl::xrender::XRenderColor {
            red: 0,
            green: 0,
            blue: 0,
            alpha: 0,
        },
    }
}

struct WindowPickRender {
    source: render::Picture,
    destination_format: render::Pictformat,
}

/// X11 core-protocol selection overlay, preview, and preferences windows.
pub struct Ui {
    width: u16,
    height: u16,
    resources: Resources,
    keys: Keycodes,
    shape_supported: bool,
    window_picker: WindowPicker,
    selection: Option<SelectionState>,
    preview: Option<PreviewState>,
    preferences: Option<PreferencesState>,
    about: Option<AboutState>,
    notice: Option<NoticeState>,
    capture_cursor: Option<CaptureCursor>,
}

impl Ui {
    /// Prepare reusable server-side GCs and the colors used by the overlay.  No window is created
    /// until a surface is requested, so the resident process can stay idle without a visible
    /// artifact.
    pub fn new(context: &X11Context) -> X11Result<Self> {
        if context.width() == 0
            || context.height() == 0
            || context.width() > i16::MAX as u16
            || context.height() > i16::MAX as u16
        {
            return Err(invalid(
                "X11 root dimensions are outside UI coordinate range",
            ));
        }

        let font = context.alloc_id()?;
        context.conn.open_font(font, b"fixed")?.check()?;

        let cursor_font = context.alloc_id()?;
        let cursor = match context.conn.open_font(cursor_font, b"cursor") {
            Ok(cookie) => {
                if cookie.check().is_err() {
                    None
                } else {
                    let id = context.alloc_id()?;
                    let glyph_cursor_ok = match context.conn.create_glyph_cursor(
                        id,
                        cursor_font,
                        cursor_font,
                        34,
                        35,
                        0xffff,
                        0xffff,
                        0xffff,
                        0,
                        0,
                        0,
                    ) {
                        Ok(cookie) => cookie.check().is_ok(),
                        Err(_) => false,
                    };
                    if glyph_cursor_ok {
                        Some(id)
                    } else {
                        let _ = context.conn.close_font(cursor_font);
                        None
                    }
                }
            }
            _ => None,
        };
        let cursor_font = cursor.map(|_| cursor_font);
        // A window-pick cursor makes the Space mode distinct from the ordinary
        // crosshair.  If the server cannot create the small bitmap cursor, the
        // crosshair remains a safe fallback.
        let window_pick_cursor = create_camera_cursor(context).ok();
        let window_pick_render = create_window_pick_render(context).ok();

        let card_pixel = alloc_color(context, 0x191d, 0x252b, 0x353f);
        let panel_pixel = alloc_color(context, 0x2026, 0x333f, 0x4d4d);
        let panel_border_pixel = alloc_color(context, 0x3038, 0x464f, 0x606e);
        let primary_pixel = alloc_color(context, 0x3b6f, 0x9f9f, 0xd2d2);
        let frame_pixel = alloc_color(context, 0x5a5e, 0x666c, 0x787e);
        let button_pixel = alloc_color(context, 0x2d36, 0x454f, 0x636f);
        let window_pick_pixel = alloc_color(context, 0x3c80, 0x9fff, 0xffff);
        let copy_gc =
            context.create_gc(context.root(), &CreateGCAux::new().graphics_exposures(0u32))?;
        // Keep a dark under-stroke behind the white selection line so the rectangle remains
        // readable over both light and dark desktop content.
        let selection_shadow_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(card_pixel)
                .background(card_pixel)
                .line_width(3u32)
                .cap_style(CapStyle::ROUND)
                .join_style(JoinStyle::ROUND)
                .graphics_exposures(0u32),
        )?;
        let outline_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(context.screen.white_pixel)
                .background(card_pixel)
                .line_width(1u32)
                .cap_style(CapStyle::ROUND)
                .join_style(JoinStyle::ROUND)
                .graphics_exposures(0u32),
        )?;
        // X11 core has no alpha compositing in a normal window.  A sparse
        // stipple gives the hovered window a light blue tint while preserving
        // the frozen pixels underneath it.  The screenshot path never reads
        // this decorated frame, so the tint cannot leak into the result.
        let (window_pick_gc, window_pick_stipple) =
            create_window_pick_resources(context, window_pick_pixel).unwrap_or((None, None));
        let card_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(card_pixel)
                .background(card_pixel)
                .graphics_exposures(0u32),
        )?;
        let panel_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(panel_pixel)
                .background(card_pixel)
                .graphics_exposures(0u32),
        )?;
        let panel_border_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(panel_border_pixel)
                .background(card_pixel)
                .line_width(1u32)
                .graphics_exposures(0u32),
        )?;
        let primary_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(primary_pixel)
                .background(card_pixel)
                .graphics_exposures(0u32),
        )?;
        let frame_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(frame_pixel)
                .background(frame_pixel)
                .graphics_exposures(0u32),
        )?;
        let button_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(button_pixel)
                .background(card_pixel)
                .graphics_exposures(0u32),
        )?;
        let text_gc = context.create_gc(
            context.root(),
            &CreateGCAux::new()
                .foreground(context.screen.white_pixel)
                .background(card_pixel)
                .font(font)
                .graphics_exposures(0u32),
        )?;
        // Query once while the resident process is starting.  Avoid a round trip on every
        // screenshot; the extension cannot disappear during this connection's lifetime.
        let shape_supported = context
            .conn
            .extension_information(x11rb::protocol::shape::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();
        let window_picker = WindowPicker::new(&context.conn)?;

        Ok(Self {
            width: context.width(),
            height: context.height(),
            resources: Resources {
                copy_gc,
                selection_shadow_gc,
                outline_gc,
                window_pick_gc,
                window_pick_stipple,
                card_gc,
                panel_gc,
                panel_border_gc,
                primary_gc,
                frame_gc,
                button_gc,
                text_gc,
                font,
                cursor,
                cursor_font,
                card_pixel,
                frame_pixel,
                window_pick_cursor,
                window_pick_render,
                xft: OnceCell::new(),
            },
            keys: resolve_keycodes(context),
            shape_supported,
            window_picker,
            selection: None,
            preview: None,
            preferences: None,
            about: None,
            notice: None,
            capture_cursor: None,
        })
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some() || self.capture_cursor.is_some()
    }

    /// Install the capture cursor before the potentially expensive root
    /// snapshot starts.  The pointer and keyboard grabs stay on the root for
    /// the whole fast path: no grab transfer or mapped input window can lose a
    /// click that arrives while the server is preparing the frozen pixmaps.
    pub fn prepare_capture_cursor(&mut self, context: &X11Context) -> X11Result<()> {
        self.close_capture_cursor(context)?;
        self.capture_cursor = Some(CaptureCursor {
            pointer_grabbed: false,
            keyboard_grabbed: false,
        });

        let pointer_grab_cookie = match context.conn.grab_pointer(
            false,
            context.root(),
            selection_grab_event_mask(),
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            self.resources.cursor.unwrap_or(x11rb::NONE),
            CURRENT_TIME,
        ) {
            Ok(cookie) => cookie,
            Err(error) => return self.fail_capture_cursor(context, error.into()),
        };
        if let Some(cursor) = self.capture_cursor.as_mut() {
            cursor.pointer_grabbed = true;
        }
        let keyboard_grab_cookie = match context.conn.grab_keyboard(
            false,
            context.root(),
            CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        ) {
            Ok(cookie) => cookie,
            Err(error) => return self.fail_capture_cursor(context, error.into()),
        };
        if let Some(cursor) = self.capture_cursor.as_mut() {
            cursor.keyboard_grabbed = true;
        }

        // Flush the grabs before waiting for their replies.
        // The cursor therefore reaches the server as soon as this method
        // starts, ahead of the full-screen capture work in the caller.
        if let Err(error) = context.flush() {
            return self.fail_capture_cursor(context, error);
        }
        let pointer = match pointer_grab_cookie.reply() {
            Ok(reply) => reply,
            Err(error) => return self.fail_capture_cursor(context, error.into()),
        };
        if pointer.status != GrabStatus::SUCCESS {
            return self
                .fail_capture_cursor(context, invalid("the X11 pointer is already grabbed"));
        }
        let keyboard = match keyboard_grab_cookie.reply() {
            Ok(reply) => reply,
            Err(error) => return self.fail_capture_cursor(context, error.into()),
        };
        if keyboard.status != GrabStatus::SUCCESS {
            // A global shortcut may still be releasing its own keyboard grab
            // while this method runs.  Keep the pointer grab and the visible
            // crosshair alive; the normal selection setup gets one more
            // chance to acquire the keyboard after the snapshot is ready.
            if let Some(cursor) = self.capture_cursor.as_mut() {
                cursor.keyboard_grabbed = false;
            }
        }
        Ok(())
    }

    /// Release the early capture input grabs.  The method is public so
    /// the application can explicitly retire it on the fallback capture path
    /// as well as after native selection setup.
    pub fn close_capture_cursor(&mut self, context: &X11Context) -> X11Result<()> {
        let Some(cursor) = self.capture_cursor.take() else {
            return Ok(());
        };
        cleanup_capture_cursor(context, cursor)
    }

    fn fail_capture_cursor(
        &mut self,
        context: &X11Context,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> X11Result<()> {
        if let Err(cleanup_error) = self.close_capture_cursor(context) {
            eprintln!("snipchord: capture cursor cleanup: {cleanup_error}");
        }
        Err(error)
    }

    /// Start the full-screen input grab and selection overlay at the current root pointer.
    /// `image` is borrowed only while it is converted to a server pixmap; the application can
    /// keep the same image for cropping.
    pub fn start_selection(&mut self, context: &X11Context, image: &RgbaImage) -> X11Result<()> {
        let width = match u16::try_from(image.width()) {
            Ok(width) => width,
            Err(_) => {
                let _ = self.close_capture_cursor(context);
                return Err(invalid("capture width exceeds X11 limits"));
            }
        };
        let height = match u16::try_from(image.height()) {
            Ok(height) => height,
            Err(_) => {
                let _ = self.close_capture_cursor(context);
                return Err(invalid("capture height exceeds X11 limits"));
            }
        };
        if width == 0 || height == 0 || width > i16::MAX as u16 || height > i16::MAX as u16 {
            let _ = self.close_capture_cursor(context);
            return Err(invalid(
                "capture dimensions are outside UI coordinate range",
            ));
        }
        self.width = width;
        self.height = height;
        let (x, y) = match context.pointer_position() {
            Ok(point) => point,
            Err(error) => {
                let _ = self.close_capture_cursor(context);
                return Err(error);
            }
        };
        let result = self.begin_selection_at(context, image, Point { x, y });
        if result.is_err() {
            let _ = self.close_capture_cursor(context);
        }
        result
    }

    /// Start the selection overlay using server-owned pixmaps prepared by
    /// [`ServerCapture`]. The full desktop stays in the X server until the
    /// user commits a rectangle, so the hotkey path does not download and
    /// re-upload a large multi-monitor image.
    pub fn start_selection_native(
        &mut self,
        context: &X11Context,
        capture: ServerCapture,
    ) -> X11Result<()> {
        let (capture_width, capture_height) = capture.size();
        if capture_width == 0
            || capture_height == 0
            || capture_width > i16::MAX as u16
            || capture_height > i16::MAX as u16
        {
            let _ = capture.destroy(context);
            let _ = self.close_capture_cursor(context);
            return Err(invalid(
                "capture dimensions are outside UI coordinate range",
            ));
        }
        if self.selection.is_some() {
            let _ = capture.destroy(context);
            let _ = self.close_capture_cursor(context);
            return Ok(());
        }
        let (x, y) = match context.pointer_position() {
            Ok(point) => point,
            Err(error) => {
                let _ = capture.destroy(context);
                let _ = self.close_capture_cursor(context);
                return Err(error);
            }
        };
        if let Err(error) = self.close_preview(context) {
            let _ = capture.destroy(context);
            let _ = self.close_capture_cursor(context);
            return Err(error);
        }
        if let Err(error) = self.close_preferences(context) {
            let _ = capture.destroy(context);
            let _ = self.close_capture_cursor(context);
            return Err(error);
        }
        if let Err(error) = self.close_about(context) {
            let _ = capture.destroy(context);
            let _ = self.close_capture_cursor(context);
            return Err(error);
        }
        self.width = capture_width;
        self.height = capture_height;
        let result = self.begin_selection_with_surfaces(
            context,
            ServerImage {
                pixmap: capture.snapshot(),
                width: capture_width,
                height: capture_height,
                owned: false,
            },
            ServerImage {
                // The no-dim selection surface intentionally reuses the
                // frozen snapshot. `native_capture` owns and frees the
                // pixmap once; this borrowed alias must never free it.
                pixmap: capture.snapshot(),
                width: capture_width,
                height: capture_height,
                owned: false,
            },
            Some(capture),
            Point { x, y },
        );
        if result.is_err() {
            // The setup function owns the native pixmaps, but failures before
            // its SelectionState is installed can still leave the early root
            // grab marker active. Release that marker on every public error
            // path just like the client-upload fallback does.
            let _ = self.close_capture_cursor(context);
        }
        result
    }

    /// Internal form retained for tests and multi-monitor callers that already know the pointer.
    fn begin_selection_at(
        &mut self,
        context: &X11Context,
        image: &RgbaImage,
        pointer: Point,
    ) -> X11Result<()> {
        if self.selection.is_some() {
            return Ok(());
        }
        self.close_preview(context)?;
        self.close_preferences(context)?;
        self.close_about(context)?;

        let snapshot = upload_pixmap(context, RgbaView::from_image(image))?;
        // Keep the live desktop bright while the user selects. The immutable
        // snapshot is borrowed a second time for the frame background; only
        // the owned `snapshot` entry frees this pixmap during cleanup.
        let backdrop = ServerImage {
            pixmap: snapshot.pixmap,
            width: snapshot.width,
            height: snapshot.height,
            owned: false,
        };
        let result = self.begin_selection_with_surfaces(context, snapshot, backdrop, None, pointer);
        if result.is_err() {
            let _ = self.close_capture_cursor(context);
        }
        result
    }

    fn begin_selection_with_surfaces(
        &mut self,
        context: &X11Context,
        snapshot: ServerImage,
        backdrop: ServerImage,
        native_capture: Option<ServerCapture>,
        pointer: Point,
    ) -> X11Result<()> {
        // When the caller prepared the capture cursor first, the root already
        // owns the pointer grab and (usually) the keyboard grab. Keep that root
        // grab in place while the visible window is installed, so queued
        // ButtonPress/Motion events cannot fall into a handoff gap.
        let capture_cursor_active = self.capture_cursor.is_some();
        // Query after the root grab is installed.  The request is issued
        // before mapping the visible overlay, so it records the focus that
        // must be restored without delaying the early cursor/grab path.
        let previous_focus_cookie = match context.conn.get_input_focus() {
            Ok(cookie) => cookie,
            Err(error) => {
                cleanup_capture_surfaces(context, &snapshot, &backdrop, native_capture);
                return Err(error.into());
            }
        };
        let frame = match context.create_pixmap(self.width, self.height) {
            Ok(frame) => frame,
            Err(error) => {
                cleanup_capture_surfaces(context, &snapshot, &backdrop, native_capture);
                return Err(error);
            }
        };
        let frame_picture = self.create_frame_picture(context, frame);
        let mut aux = CreateWindowAux::new()
            .override_redirect(1u32)
            .background_pixmap(backdrop.pixmap)
            .event_mask(selection_event_mask());
        if let Some(cursor) = self.resources.cursor {
            aux = aux.cursor(cursor);
        }
        let window = match context.create_window(self.width, self.height, &aux) {
            Ok(window) => window,
            Err(error) => {
                cleanup_capture_surfaces(context, &snapshot, &backdrop, native_capture);
                if let Some(picture) = frame_picture {
                    let _ = context.conn.render_free_picture(picture);
                }
                let _ = context.conn.free_pixmap(frame);
                return Err(error);
            }
        };

        let pointer = self.clamp_point(pointer);
        // Early capture grabs target the root window, so events queued while
        // the snapshot was being prepared retain `event == root`.  The normal
        // path grabs the newly-created overlay directly.
        let grab_window = if self.capture_cursor.is_some() {
            context.root()
        } else {
            window
        };
        let bright_snapshot_surface = snapshot.pixmap == backdrop.pixmap;
        self.selection = Some(SelectionState {
            window,
            frame,
            frame_picture,
            snapshot,
            backdrop,
            native_capture,
            previous_focus: x11rb::NONE,
            grab_window,
            pointer,
            anchor: None,
            rect: None,
            moving: false,
            resize_offset: Point::default(),
            last_pointer: pointer,
            window_pick: false,
            window_target: None,
            space_down: false,
            pointer_grabbed: self
                .capture_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.pointer_grabbed),
            keyboard_grabbed: self
                .capture_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.keyboard_grabbed),
            keyboard_retry_at: None,
        });

        // From here the selection state owns the active root grabs. The
        // helper's cleanup flags are cleared so an error cannot ungrab the
        // same root grab twice through both state and helper cleanup.
        if capture_cursor_active {
            self.handoff_capture_cursor();
        }

        // Queue the map and stacking requests as one batch. On the fast path
        // the root grabs and crosshair are already active; the ordinary path
        // queues its grabs and focus change below before waiting for replies.
        let map_cookie = match context.conn.map_window(window) {
            Ok(cookie) => cookie,
            Err(error) => return self.fail_selection_setup(context, error.into()),
        };
        let pointer_grab_cookie = if capture_cursor_active {
            None
        } else {
            let cookie = match context.conn.grab_pointer(
                false,
                window,
                selection_grab_event_mask(),
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                self.resources.cursor.unwrap_or(x11rb::NONE),
                CURRENT_TIME,
            ) {
                Ok(cookie) => cookie,
                Err(error) => return self.fail_selection_setup(context, error.into()),
            };
            // The request may have reached the server even when a later reply
            // or status check fails. Mark it as owned now so cleanup always
            // queues the matching ungrab on every setup error path.
            if let Some(state) = self.selection.as_mut() {
                state.pointer_grabbed = true;
            }
            Some(cookie)
        };
        let keyboard_grab_cookie = if capture_cursor_active {
            if self
                .selection
                .as_ref()
                .is_some_and(|state| state.keyboard_grabbed)
            {
                None
            } else {
                // The early pointer grab is sufficient for drag selection.
                // The shortcut's own keyboard grab commonly remains active
                // until its key-release event, so defer this grab and let the
                // event loop retry without delaying the first visible frame.
                if let Some(state) = self.selection.as_mut() {
                    state.keyboard_retry_at = Some(Instant::now() + KEYBOARD_GRAB_RETRY);
                }
                None
            }
        } else {
            let cookie = match context.conn.grab_keyboard(
                false,
                if capture_cursor_active {
                    context.root()
                } else {
                    window
                },
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            ) {
                Ok(cookie) => cookie,
                Err(error) => return self.fail_selection_setup(context, error.into()),
            };
            if let Some(state) = self.selection.as_mut() {
                state.keyboard_grabbed = true;
            }
            Some(cookie)
        };
        let focus_cookie = if capture_cursor_active {
            None
        } else {
            match context
                .conn
                .set_input_focus(InputFocus::NONE, window, CURRENT_TIME)
            {
                Ok(cookie) => Some(cookie),
                Err(error) => return self.fail_selection_setup(context, error.into()),
            }
        };
        if let Err(error) = context.flush() {
            return self.fail_selection_setup(context, error);
        }

        // Keep a reply barrier after the map request.  x11rb can otherwise
        // mistake an input event whose sequence is the map request for the
        // end of its checked-void scan and wait forever in `map_cookie.check`.
        // A later reply makes the ordering explicit while retaining the map
        // error check below.
        let map_error_barrier = match context.conn.get_input_focus() {
            Ok(cookie) => cookie,
            Err(error) => return self.fail_selection_setup(context, error.into()),
        };

        let previous_focus = match previous_focus_cookie.reply() {
            Ok(reply) => reply.focus,
            Err(error) => return self.fail_selection_setup(context, error.into()),
        };
        if let Some(state) = self.selection.as_mut() {
            state.previous_focus = previous_focus;
        }

        // The pointer and keyboard replies are now already in flight while
        // the mapped window is becoming visible.  Their ownership flags were
        // set when the requests were queued so cleanup can always release a
        // grab if a later reply or setup request fails.
        if let Some(cookie) = pointer_grab_cookie {
            let grab = match cookie.reply() {
                Ok(reply) => reply,
                Err(error) => return self.fail_selection_setup(context, error.into()),
            };
            if grab.status != GrabStatus::SUCCESS {
                return self
                    .fail_selection_setup(context, invalid("the X11 pointer is already grabbed"));
            }
        }
        if let Some(cookie) = keyboard_grab_cookie {
            let keyboard = match cookie.reply() {
                Ok(reply) => reply,
                Err(error) => return self.fail_selection_setup(context, error.into()),
            };
            if keyboard.status != GrabStatus::SUCCESS {
                return self
                    .fail_selection_setup(context, invalid("the X11 keyboard is already grabbed"));
            }
        }
        if let Err(error) = map_error_barrier.reply() {
            return self.fail_selection_setup(context, error.into());
        }
        if let Err(error) = map_cookie.check() {
            return self.fail_selection_setup(context, error.into());
        }
        if let Some(cookie) = focus_cookie {
            if let Err(error) = cookie.check() {
                return self.fail_selection_setup(context, error.into());
            }
        }
        // With the bright no-dim surface the mapped window already paints
        // directly from the frozen snapshot background. Avoid copying the
        // entire desktop into the off-screen frame and back for this initial
        // decoration-free state; later drag/window-pick redraws still use the
        // frame so their outlines remain atomic.
        if !bright_snapshot_surface {
            if let Err(error) = self.redraw_selection(context) {
                return self.fail_selection_setup(context, error);
            }
        }
        if let Err(error) = context.flush() {
            return self.fail_selection_setup(context, error);
        }
        Ok(())
    }

    fn retry_selection_keyboard(&mut self, context: &X11Context) -> X11Result<()> {
        let Some((grab_window, retry_at)) = self
            .selection
            .as_ref()
            .map(|state| (state.grab_window, state.keyboard_retry_at))
        else {
            return Ok(());
        };
        let Some(retry_at) = retry_at else {
            return Ok(());
        };
        if retry_at > Instant::now() {
            return Ok(());
        }

        let cookie = context.conn.grab_keyboard(
            false,
            grab_window,
            CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )?;
        context.flush()?;
        let keyboard = cookie.reply()?;
        if let Some(state) = self.selection.as_mut() {
            if keyboard.status == GrabStatus::SUCCESS {
                state.keyboard_grabbed = true;
                state.keyboard_retry_at = None;
            } else {
                state.keyboard_retry_at = Some(Instant::now() + KEYBOARD_GRAB_RETRY);
            }
        }
        Ok(())
    }

    fn fail_selection_setup(
        &mut self,
        context: &X11Context,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> X11Result<()> {
        if let Err(cleanup_error) = self.close_selection(context) {
            eprintln!("snipchord: selection cleanup: {cleanup_error}");
        }
        if let Err(cleanup_error) = self.close_capture_cursor(context) {
            eprintln!("snipchord: capture cursor cleanup: {cleanup_error}");
        }
        Err(error)
    }

    fn handoff_capture_cursor(&mut self) {
        // The selection state now owns the active root grabs.  There is no
        // helper window to destroy on this path, so simply drop the marker;
        // cleanup releases each grab exactly once through `SelectionState`.
        let _ = self.capture_cursor.take();
    }

    /// Show a transient lower-right image thumbnail at the current root pointer. The application
    /// retains `image` for the clipboard/output pipeline and only passes borrowed pixels here.
    pub fn show_preview(
        &mut self,
        context: &X11Context,
        image: &RgbaImage,
        demo: bool,
        saved: Option<&Path>,
    ) -> X11Result<()> {
        let (x, y) = context.pointer_position()?;
        let monitors = context.monitor_rects()?;
        let saved_name = saved
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned());
        self.begin_preview_at(
            context,
            image,
            Point { x, y },
            &monitors
                .into_iter()
                .map(|(x, y, width, height)| MonitorRect::new(x, y, width, height))
                .collect::<Vec<_>>(),
            demo,
            saved_name.as_deref(),
        )
    }

    /// Internal form retained for callers that already have monitor geometry.
    fn begin_preview_at(
        &mut self,
        context: &X11Context,
        image: &RgbaImage,
        pointer: Point,
        monitors: &[MonitorRect],
        _demo: bool,
        _saved_name: Option<&str>,
    ) -> X11Result<()> {
        self.close_selection(context)?;
        self.close_preview(context)?;
        self.close_preferences(context)?;
        self.close_about(context)?;

        let (thumbnail, thumb_width, thumb_height) = make_thumbnail(image)?;
        let thumbnail = upload_pixmap(context, RgbaView::from_image(&thumbnail))?;
        let outer_padding = PREVIEW_BORDER;
        let width = u16::try_from(i32::from(thumb_width) + outer_padding * 2)
            .map_err(|_| invalid("preview width is too large"))?;
        let height = u16::try_from(i32::from(thumb_height) + outer_padding * 2)
            .map_err(|_| invalid("preview height is too large"))?;
        let fallback = MonitorRect::new(0, 0, u32::from(self.width), u32::from(self.height));
        let monitor = workarea_for(context, monitor_for(pointer, monitors, fallback));
        let x = monitor.x + max(0, monitor.width as i32 - i32::from(width) - 24);
        let y = monitor.y + max(0, monitor.height as i32 - i32::from(height) - 24);
        let aux = CreateWindowAux::new()
            .override_redirect(1u32)
            .background_pixel(self.resources.frame_pixel)
            .event_mask(preview_event_mask());
        let window = match context.create_window(width, height, &aux) {
            Ok(window) => window,
            Err(error) => {
                free_server_image(context, &thumbnail);
                return Err(error);
            }
        };
        self.preview = Some(PreviewState {
            window,
            thumbnail,
            width,
            height,
            deadline: Some(Instant::now() + PREVIEW_TIMEOUT),
        });
        let configure_result: X11Result<()> = (|| {
            context
                .conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .x(x)
                        .y(y)
                        .stack_mode(StackMode::ABOVE),
                )?
                .check()?;
            Ok(())
        })();
        if let Err(error) = configure_result {
            return self.fail_preview_setup(context, error);
        }
        if let Err(error) =
            shape_preview_window(context, window, width, height, self.shape_supported)
        {
            return self.fail_preview_setup(context, error);
        }
        if let Err(error) = self.redraw_preview(context) {
            return self.fail_preview_setup(context, error);
        }
        if let Err(error) = context.conn.map_window(window)?.check() {
            return self.fail_preview_setup(context, error.into());
        }
        if let Err(error) = context.flush() {
            return self.fail_preview_setup(context, error);
        }
        Ok(())
    }

    fn fail_preview_setup(
        &mut self,
        context: &X11Context,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> X11Result<()> {
        if let Err(cleanup_error) = self.close_preview(context) {
            eprintln!("snipchord: preview cleanup: {cleanup_error}");
        }
        Err(error)
    }

    /// Show or update the Preferences window.
    ///
    /// Shortcut values are captured when the window is opened.  They are
    /// intentionally display-only: changing them belongs to the desktop's
    /// keyboard settings, not to this small tray surface.
    pub fn begin_preferences(
        &mut self,
        context: &X11Context,
        settings: Settings,
        shortcuts: ShortcutList,
    ) -> X11Result<()> {
        self.sync_root_geometry(context)?;
        self.close_selection(context)?;
        self.close_preview(context)?;
        self.close_about(context)?;
        let width = PREFERENCES_WIDTH;
        let height = PREFERENCES_HEIGHT;
        let (x, y) = dialog_origin(context, self.width, self.height, width, height);
        if let Some(preferences) = self.preferences.as_mut() {
            let window = preferences.window;
            preferences.settings = settings;
            preferences.shortcuts = shortcuts;
            let result: X11Result<()> = (|| {
                set_dialog_window_hints(context, window, x, y, width, height)?;
                context.conn.map_window(window)?.check()?;
                context
                    .conn
                    .configure_window(
                        window,
                        &xproto::ConfigureWindowAux::new()
                            .x(x)
                            .y(y)
                            .stack_mode(StackMode::ABOVE),
                    )?
                    .check()?;
                Ok(())
            })();
            result?;
            // A managed X11 window is not necessarily viewable when MapWindow
            // returns: the window manager may still be processing the
            // MapRequest.  SetInputFocus rejects such a window with BadMatch.
            // The MapNotify handler below focuses it once it is actually
            // mapped; this branch only needs to refocus an already-viewable
            // preferences window.
            if context
                .conn
                .get_window_attributes(window)?
                .reply()?
                .map_state
                == xproto::MapState::VIEWABLE
            {
                self.focus_preferences(context)?;
            }
            self.redraw_preferences(context)?;
            context.flush()?;
            return Ok(());
        }
        let aux = CreateWindowAux::new()
            .background_pixel(self.resources.card_pixel)
            .event_mask(preferences_event_mask());
        let previous_focus = context.conn.get_input_focus()?.reply()?.focus;
        let wm_protocols = context.intern_atom(b"WM_PROTOCOLS")?;
        let wm_delete_window = context.intern_atom(b"WM_DELETE_WINDOW")?;
        let window = context.create_window(width, height, &aux)?;
        self.preferences = Some(PreferencesState {
            window,
            settings,
            shortcuts,
            tab: PreferencesTab::General,
            previous_focus,
            wm_protocols,
            wm_delete_window,
        });
        let wm_name: u32 = xproto::AtomEnum::WM_NAME.into();
        let string_atom: u32 = xproto::AtomEnum::STRING.into();
        if let Err(error) =
            context.change_property8(window, wm_name, string_atom, b"SnipChord Preferences")
        {
            return self.fail_preferences_setup(context, error);
        }
        let atom: u32 = xproto::AtomEnum::ATOM.into();
        let protocol_result: X11Result<()> = (|| {
            context
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    window,
                    wm_protocols,
                    atom,
                    &[wm_delete_window],
                )?
                .check()?;
            Ok(())
        })();
        if let Err(error) = protocol_result {
            return self.fail_preferences_setup(context, error);
        }
        let configure_result: X11Result<()> = (|| {
            set_dialog_window_hints(context, window, x, y, width, height)?;
            context
                .conn
                .configure_window(window, &xproto::ConfigureWindowAux::new().x(x).y(y))?
                .check()?;
            context.conn.map_window(window)?.check()?;
            Ok(())
        })();
        if let Err(error) = configure_result {
            return self.fail_preferences_setup(context, error);
        }
        if let Err(error) = self.redraw_preferences(context) {
            return self.fail_preferences_setup(context, error);
        }
        if let Err(error) = context.flush() {
            return self.fail_preferences_setup(context, error);
        }
        Ok(())
    }

    /// Focus a preferences window only after the X server reports it as
    /// viewable.  Managed windows can reject an earlier SetInputFocus with
    /// BadMatch while the window manager is still handling MapRequest.
    fn focus_preferences(&self, context: &X11Context) -> X11Result<()> {
        let Some(preferences) = self.preferences.as_ref() else {
            return Ok(());
        };
        if context
            .conn
            .get_window_attributes(preferences.window)?
            .reply()?
            .map_state
            != xproto::MapState::VIEWABLE
        {
            return Ok(());
        }
        context
            .conn
            .set_input_focus(InputFocus::NONE, preferences.window, CURRENT_TIME)?
            .check()?;
        context.flush()?;
        Ok(())
    }

    fn fail_preferences_setup(
        &mut self,
        context: &X11Context,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> X11Result<()> {
        if let Err(cleanup_error) = self.close_preferences(context) {
            eprintln!("snipchord: preferences cleanup: {cleanup_error}");
        }
        Err(error)
    }

    pub fn show_preferences(
        &mut self,
        context: &X11Context,
        settings: Settings,
        shortcuts: ShortcutList,
    ) -> X11Result<()> {
        self.begin_preferences(context, settings, shortcuts)
    }

    /// Show or update the small About window.
    pub fn begin_about(&mut self, context: &X11Context) -> X11Result<()> {
        self.sync_root_geometry(context)?;
        self.close_selection(context)?;
        self.close_preview(context)?;
        self.close_preferences(context)?;
        let width = ABOUT_WIDTH;
        let height = ABOUT_HEIGHT;
        let (x, y) = dialog_origin(context, self.width, self.height, width, height);
        if self.about.is_some() {
            let window = self.about.as_ref().expect("about state exists").window;
            set_dialog_window_hints(context, window, x, y, width, height)?;
            context.conn.map_window(window)?.check()?;
            context
                .conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .x(x)
                        .y(y)
                        .stack_mode(StackMode::ABOVE),
                )?
                .check()?;
            if context
                .conn
                .get_window_attributes(window)?
                .reply()?
                .map_state
                == xproto::MapState::VIEWABLE
            {
                self.focus_about(context)?;
            }
            self.redraw_about(context)?;
            context.flush()?;
            return Ok(());
        }

        let aux = CreateWindowAux::new()
            .background_pixel(self.resources.card_pixel)
            .event_mask(about_event_mask());
        let previous_focus = context.conn.get_input_focus()?.reply()?.focus;
        let wm_protocols = context.intern_atom(b"WM_PROTOCOLS")?;
        let wm_delete_window = context.intern_atom(b"WM_DELETE_WINDOW")?;
        let window = context.create_window(width, height, &aux)?;
        self.about = Some(AboutState {
            window,
            previous_focus,
            wm_protocols,
            wm_delete_window,
        });

        let wm_name: u32 = xproto::AtomEnum::WM_NAME.into();
        let string_atom: u32 = xproto::AtomEnum::STRING.into();
        if let Err(error) =
            context.change_property8(window, wm_name, string_atom, b"About SnipChord")
        {
            return self.fail_about_setup(context, error);
        }
        let atom: u32 = xproto::AtomEnum::ATOM.into();
        let protocol_result: X11Result<()> = (|| {
            context
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    window,
                    wm_protocols,
                    atom,
                    &[wm_delete_window],
                )?
                .check()?;
            Ok(())
        })();
        if let Err(error) = protocol_result {
            return self.fail_about_setup(context, error);
        }
        let configure_result: X11Result<()> = (|| {
            set_dialog_window_hints(context, window, x, y, width, height)?;
            context
                .conn
                .configure_window(window, &xproto::ConfigureWindowAux::new().x(x).y(y))?
                .check()?;
            context.conn.map_window(window)?.check()?;
            Ok(())
        })();
        if let Err(error) = configure_result {
            return self.fail_about_setup(context, error);
        }
        if let Err(error) = self.redraw_about(context) {
            return self.fail_about_setup(context, error);
        }
        if let Err(error) = context.flush() {
            return self.fail_about_setup(context, error);
        }
        Ok(())
    }

    /// Alias for callers that use the same verb as the preferences command.
    pub fn show_about(&mut self, context: &X11Context) -> X11Result<()> {
        self.begin_about(context)
    }

    fn focus_about(&self, context: &X11Context) -> X11Result<()> {
        let Some(about) = self.about.as_ref() else {
            return Ok(());
        };
        if context
            .conn
            .get_window_attributes(about.window)?
            .reply()?
            .map_state
            != xproto::MapState::VIEWABLE
        {
            return Ok(());
        }
        context
            .conn
            .set_input_focus(InputFocus::NONE, about.window, CURRENT_TIME)?
            .check()?;
        context.flush()?;
        Ok(())
    }

    fn fail_about_setup(
        &mut self,
        context: &X11Context,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> X11Result<()> {
        if let Err(cleanup_error) = self.close_about(context) {
            eprintln!("snipchord: about cleanup: {cleanup_error}");
        }
        Err(error)
    }

    pub fn close_selection(&mut self, context: &X11Context) -> X11Result<()> {
        if let Some(state) = self.selection.take() {
            cleanup_selection(context, state, true)?;
        }
        self.close_capture_cursor(context)?;
        Ok(())
    }

    pub fn close_preview(&mut self, context: &X11Context) -> X11Result<()> {
        if let Some(state) = self.preview.take() {
            cleanup_preview(context, &state, true)?;
        }
        Ok(())
    }

    pub fn close_preferences(&mut self, context: &X11Context) -> X11Result<()> {
        if let Some(state) = self.preferences.take() {
            cleanup_preferences(context, &state, true)?;
        }
        Ok(())
    }

    pub fn close_about(&mut self, context: &X11Context) -> X11Result<()> {
        if let Some(state) = self.about.take() {
            cleanup_about(context, &state, true)?;
        }
        Ok(())
    }

    /// Show a short native notification card for failures and completion messages.  The card is
    /// intentionally independent of the preview so an error can remain visible while the last
    /// screenshot is still available for Save or Copy.
    pub fn show_notice(&mut self, context: &X11Context, title: &str, body: &str) -> X11Result<()> {
        self.sync_root_geometry(context)?;
        self.close_notice(context)?;
        let width = 380u16;
        let height = 96u16;
        let x = i32::from(self.width).saturating_sub(i32::from(width) + 24);
        let y = i32::from(self.height).saturating_sub(i32::from(height) + 24);
        let aux = CreateWindowAux::new()
            .override_redirect(1u32)
            .background_pixel(self.resources.card_pixel)
            .event_mask(notice_event_mask());
        let window = context.create_window(width, height, &aux)?;
        self.notice = Some(NoticeState {
            window,
            width,
            height,
            title: truncate_text(title, 56),
            body: truncate_text(body, 72),
            deadline: Instant::now() + NOTICE_TIMEOUT,
        });
        let wm_name: u32 = xproto::AtomEnum::WM_NAME.into();
        let string_atom: u32 = xproto::AtomEnum::STRING.into();
        if let Err(error) =
            context.change_property8(window, wm_name, string_atom, b"SnipChord Notice")
        {
            return self.fail_notice_setup(context, error);
        }
        let configure_result: X11Result<()> = (|| {
            context
                .conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .x(x)
                        .y(y)
                        .stack_mode(StackMode::ABOVE),
                )?
                .check()?;
            context.conn.map_window(window)?.check()?;
            Ok(())
        })();
        if let Err(error) = configure_result {
            return self.fail_notice_setup(context, error);
        }
        if let Err(error) = self.redraw_notice(context) {
            return self.fail_notice_setup(context, error);
        }
        if let Err(error) = context.flush() {
            return self.fail_notice_setup(context, error);
        }
        Ok(())
    }

    fn fail_notice_setup(
        &mut self,
        context: &X11Context,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) -> X11Result<()> {
        if let Err(cleanup_error) = self.close_notice(context) {
            eprintln!("snipchord: notice cleanup: {cleanup_error}");
        }
        Err(error)
    }

    pub fn close_notice(&mut self, context: &X11Context) -> X11Result<()> {
        if let Some(state) = self.notice.take() {
            cleanup_notice(context, &state, true)?;
        }
        Ok(())
    }

    /// Hide transient surfaces before a new root capture.  The caller can use the return value to
    /// give the compositor a short opportunity to repaint the desktop underneath them.
    pub fn close_for_capture(&mut self, context: &X11Context) -> bool {
        let mut closed = false;
        if self.preview.is_some() {
            let _ = self.close_preview(context);
            closed = true;
        }
        if self.preferences.is_some() {
            let _ = self.close_preferences(context);
            closed = true;
        }
        if self.about.is_some() {
            let _ = self.close_about(context);
            closed = true;
        }
        if self.notice.is_some() {
            let _ = self.close_notice(context);
            closed = true;
        }
        closed
    }

    /// Return the timer deadline used by the application event loop.
    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.selection
                .as_ref()
                .and_then(|selection| selection.keyboard_retry_at),
            self.preview.as_ref().and_then(|preview| preview.deadline),
            self.notice.as_ref().map(|notice| notice.deadline),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn tick(&mut self, context: &X11Context) -> X11Result<()> {
        self.retry_selection_keyboard(context)?;
        let now = Instant::now();
        if self
            .preview
            .as_ref()
            .and_then(|preview| preview.deadline)
            .is_some_and(|deadline| deadline <= now)
        {
            self.close_preview(context)?;
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.deadline <= now)
        {
            self.close_notice(context)?;
        }
        Ok(())
    }

    /// Dispatch one X11 event and return at most one application action.
    pub fn handle_event(
        &mut self,
        context: &X11Context,
        event: &Event,
    ) -> X11Result<Option<UiAction>> {
        match event {
            Event::MapNotify(map) if self.window_is_preferences(map.window) => {
                // With a reparenting window manager, MapWindow only queues a
                // request.  Wait for MapNotify before claiming focus so the
                // preferences window is viewable and SetInputFocus cannot
                // fail with BadMatch.
                self.focus_preferences(context)?;
                Ok(None)
            }
            Event::MapNotify(map) if self.window_is_about(map.window) => {
                self.focus_about(context)?;
                Ok(None)
            }
            Event::Expose(expose) if self.window_is_selection(expose.window) => {
                if expose.count == 0 {
                    self.redraw_selection(context)?;
                }
                Ok(None)
            }
            Event::Expose(expose) if self.window_is_preview(expose.window) => {
                if expose.count == 0 {
                    self.redraw_preview(context)?;
                }
                Ok(None)
            }
            Event::Expose(expose) if self.window_is_preferences(expose.window) => {
                if expose.count == 0 {
                    self.redraw_preferences(context)?;
                }
                Ok(None)
            }
            Event::Expose(expose) if self.window_is_about(expose.window) => {
                if expose.count == 0 {
                    self.redraw_about(context)?;
                }
                Ok(None)
            }
            Event::Expose(expose) if self.window_is_notice(expose.window) => {
                if expose.count == 0 {
                    self.redraw_notice(context)?;
                }
                Ok(None)
            }
            Event::ButtonPress(button) if self.window_is_selection_event(button.event) => {
                self.selection_button_press(context, button)
            }
            Event::ButtonRelease(button) if self.window_is_selection_event(button.event) => {
                self.selection_button_release(context, button)
            }
            Event::MotionNotify(motion) if self.window_is_selection_event(motion.event) => {
                self.selection_motion(context, motion)
            }
            Event::KeyPress(key) if self.window_is_selection_event(key.event) => {
                self.selection_key_press(context, key)
            }
            Event::KeyRelease(key) if self.window_is_selection_event(key.event) => {
                self.selection_key_release(context, key)
            }
            Event::KeyPress(key) if self.window_is_preferences(key.event) => {
                if key.detail == self.keys.escape {
                    self.close_preferences(context)?;
                    Ok(Some(UiAction::Close))
                } else {
                    Ok(None)
                }
            }
            Event::ButtonPress(button) if self.window_is_preferences(button.event) => {
                self.preferences_button_press(context, button)
            }
            Event::KeyPress(key) if self.window_is_about(key.event) => {
                if key.detail == self.keys.escape {
                    self.close_about(context)?;
                    Ok(Some(UiAction::Close))
                } else {
                    Ok(None)
                }
            }
            Event::ButtonPress(button) if self.window_is_about(button.event) => {
                self.about_button_press(context, button)
            }
            Event::ButtonPress(button) if self.window_is_preview(button.event) => {
                if button.detail == 1 {
                    self.close_preview(context)?;
                    Ok(Some(UiAction::OpenPreview))
                } else {
                    Ok(None)
                }
            }
            Event::ClientMessage(message)
                if self.window_is_preferences(message.window) && self.is_wm_delete(message) =>
            {
                self.close_preferences(context)?;
                Ok(Some(UiAction::Close))
            }
            Event::ClientMessage(message)
                if self.window_is_about(message.window) && self.is_wm_delete(message) =>
            {
                self.close_about(context)?;
                Ok(Some(UiAction::Close))
            }
            Event::ClientMessage(message)
                if self.window_is_selection(message.window)
                    || self.window_is_preview(message.window)
                    || self.window_is_notice(message.window) =>
            {
                if self.window_is_selection(message.window) {
                    self.close_selection(context)?;
                } else if self.window_is_preview(message.window) {
                    self.close_preview(context)?;
                } else {
                    self.close_notice(context)?;
                }
                Ok(Some(UiAction::Close))
            }
            Event::DestroyNotify(destroy) if self.window_is_selection(destroy.window) => {
                if let Some(state) = self.selection.take() {
                    cleanup_selection(context, state, false)?;
                }
                self.close_capture_cursor(context)?;
                Ok(Some(UiAction::Cancelled))
            }
            Event::DestroyNotify(destroy) if self.window_is_preview(destroy.window) => {
                if let Some(state) = self.preview.take() {
                    cleanup_preview(context, &state, false)?;
                }
                Ok(Some(UiAction::Close))
            }
            Event::DestroyNotify(destroy) if self.window_is_preferences(destroy.window) => {
                if let Some(state) = self.preferences.take() {
                    cleanup_preferences(context, &state, false)?;
                }
                Ok(Some(UiAction::Close))
            }
            Event::DestroyNotify(destroy) if self.window_is_about(destroy.window) => {
                if let Some(state) = self.about.take() {
                    cleanup_about(context, &state, false)?;
                }
                Ok(Some(UiAction::Close))
            }
            Event::DestroyNotify(destroy) if self.window_is_notice(destroy.window) => {
                if let Some(state) = self.notice.take() {
                    cleanup_notice(context, &state, false)?;
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Free all server resources owned by the UI. Call this before dropping the X11 context.
    pub fn shutdown(&mut self, context: &X11Context) -> X11Result<()> {
        self.close_selection(context)?;
        self.close_preview(context)?;
        self.close_preferences(context)?;
        self.close_about(context)?;
        self.close_notice(context)?;
        self.close_capture_cursor(context)?;
        // Close the optional Xft connection before freeing the core X11 resources. It is lazy so
        // normal screenshot startup never pays for font discovery, and `take` also handles the
        // case where Preferences/About were never opened.
        drop(self.resources.xft.take());
        let resources = &self.resources;
        context.conn.free_gc(resources.copy_gc)?.check()?;
        context
            .conn
            .free_gc(resources.selection_shadow_gc)?
            .check()?;
        context.conn.free_gc(resources.outline_gc)?.check()?;
        if let Some(gc) = resources.window_pick_gc {
            context.conn.free_gc(gc)?.check()?;
        }
        if let Some(stipple) = resources.window_pick_stipple {
            context.conn.free_pixmap(stipple)?.check()?;
        }
        context.conn.free_gc(resources.card_gc)?.check()?;
        context.conn.free_gc(resources.panel_gc)?.check()?;
        context.conn.free_gc(resources.panel_border_gc)?.check()?;
        context.conn.free_gc(resources.primary_gc)?.check()?;
        context.conn.free_gc(resources.frame_gc)?.check()?;
        context.conn.free_gc(resources.button_gc)?.check()?;
        context.conn.free_gc(resources.text_gc)?.check()?;
        context.conn.close_font(resources.font)?.check()?;
        if let Some(cursor) = resources.window_pick_cursor {
            context.conn.free_cursor(cursor)?.check()?;
        }
        if let Some(render_resources) = resources.window_pick_render.as_ref() {
            context
                .conn
                .render_free_picture(render_resources.source)?
                .check()?;
        }
        if let Some(cursor_font) = resources.cursor_font {
            if let Some(cursor) = resources.cursor {
                context.conn.free_cursor(cursor)?.check()?;
            }
            context.conn.close_font(cursor_font)?.check()?;
        }
        context.flush()?;
        Ok(())
    }

    fn window_is_selection(&self, window: Window) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|state| state.window == window)
    }

    fn window_is_selection_event(&self, window: Window) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|state| state.window == window || state.grab_window == window)
    }

    fn window_is_preview(&self, window: Window) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|state| state.window == window)
    }

    fn window_is_preferences(&self, window: Window) -> bool {
        self.preferences
            .as_ref()
            .is_some_and(|state| state.window == window)
    }

    fn window_is_about(&self, window: Window) -> bool {
        self.about
            .as_ref()
            .is_some_and(|state| state.window == window)
    }

    fn window_is_notice(&self, window: Window) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|state| state.window == window)
    }

    fn is_wm_delete(&self, message: &xproto::ClientMessageEvent) -> bool {
        if let Some(preferences) = self.preferences.as_ref() {
            return message.format == 32
                && message.type_ == preferences.wm_protocols
                && message.data.as_data32()[0] == preferences.wm_delete_window;
        }
        self.about.as_ref().is_some_and(|about| {
            message.format == 32
                && message.type_ == about.wm_protocols
                && message.data.as_data32()[0] == about.wm_delete_window
        })
    }

    fn sync_root_geometry(&mut self, context: &X11Context) -> X11Result<()> {
        let geometry = context.conn.get_geometry(context.root())?.reply()?;
        if geometry.width == 0
            || geometry.height == 0
            || geometry.width > i16::MAX as u16
            || geometry.height > i16::MAX as u16
        {
            return Err(invalid(
                "X11 root dimensions are outside UI coordinate range",
            ));
        }
        self.width = geometry.width;
        self.height = geometry.height;
        Ok(())
    }

    fn clamp_point(&self, point: Point) -> Point {
        Point {
            x: point.x.clamp(0, i32::from(self.width.saturating_sub(1))),
            y: point.y.clamp(0, i32::from(self.height.saturating_sub(1))),
        }
    }

    /// Refresh the window under the pointer while the user is in Space-before-drag mode.  The
    /// picker walks the frozen X11 stacking list and excludes this full-screen selection window,
    /// so the overlay never selects itself.
    fn update_window_target(&mut self, context: &X11Context) -> X11Result<()> {
        let Some(state) = self.selection.as_ref() else {
            return Ok(());
        };
        if !state.window_pick {
            return Ok(());
        }
        let pointer = state.pointer;
        let excluded = [state.window];
        let target = self.window_picker.pick_at_pointer(
            &context.conn,
            context.root(),
            (pointer.x, pointer.y),
            &excluded,
        )?;
        if let Some(state) = self.selection.as_mut() {
            state.window_target = target;
        }
        Ok(())
    }

    fn create_frame_picture(&self, context: &X11Context, frame: Pixmap) -> Option<render::Picture> {
        let render_resources = self.resources.window_pick_render.as_ref()?;
        let picture = context.alloc_id().ok()?;
        let result: X11Result<()> = (|| {
            context
                .conn
                .render_create_picture(
                    picture,
                    frame,
                    render_resources.destination_format,
                    &render::CreatePictureAux::new(),
                )?
                .check()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = context.conn.render_free_picture(picture);
            None
        } else {
            Some(picture)
        }
    }

    fn set_selection_cursor(
        &self,
        context: &X11Context,
        requested_cursor: Option<xproto::Cursor>,
    ) -> X11Result<()> {
        let Some(cursor) = requested_cursor.or(self.resources.cursor) else {
            return Ok(());
        };
        let Some(state) = self.selection.as_ref() else {
            return Ok(());
        };
        // ChangeActivePointerGrab updates the cursor immediately even when the
        // early fast path still owns the grab on the root window. Updating the
        // selection window as well keeps the cursor correct after the normal
        // grab handoff path.
        if state.pointer_grabbed {
            context
                .conn
                .change_active_pointer_grab(cursor, CURRENT_TIME, selection_grab_event_mask())?
                .check()?;
        }
        context
            .conn
            .change_window_attributes(
                state.window,
                &xproto::ChangeWindowAttributesAux::new().cursor(cursor),
            )?
            .check()?;
        context.flush()?;
        Ok(())
    }

    fn selection_button_press(
        &mut self,
        context: &X11Context,
        event: &xproto::ButtonPressEvent,
    ) -> X11Result<Option<UiAction>> {
        if event.detail == 3 {
            self.close_selection(context)?;
            return Ok(Some(UiAction::Cancelled));
        }
        if event.detail != 1 {
            return Ok(None);
        }
        // Pointer grabs installed before the selection window is mapped deliver
        // queued events with coordinates relative to the temporary grab target.
        // Root coordinates are stable for both that path and the normal overlay
        // window, including multi-monitor roots with a non-zero origin.
        let point = self.clamp_point(Point {
            x: i32::from(event.root_x),
            y: i32::from(event.root_y),
        });
        if let Some(state) = self.selection.as_mut() {
            state.pointer = point;
            if !state.window_pick {
                state.anchor = Some(point);
                state.last_pointer = point;
                state.resize_offset = Point::default();
                state.rect = Some(Rect::between(
                    point.x as f64,
                    point.y as f64,
                    point.x as f64,
                    point.y as f64,
                ));
                state.moving = false;
            }
        }
        if self
            .selection
            .as_ref()
            .is_some_and(|state| state.window_pick)
        {
            self.update_window_target(context)?;
        }
        self.redraw_selection(context)?;
        Ok(None)
    }

    fn selection_motion(
        &mut self,
        context: &X11Context,
        event: &xproto::MotionNotifyEvent,
    ) -> X11Result<Option<UiAction>> {
        let point = self.clamp_point(Point {
            x: i32::from(event.root_x),
            y: i32::from(event.root_y),
        });
        let width = self.width;
        let height = self.height;
        let window_pick = self
            .selection
            .as_ref()
            .is_some_and(|state| state.window_pick);
        let mut needs_redraw = false;
        if let Some(state) = self.selection.as_mut() {
            state.pointer = point;
            if window_pick {
                state.anchor = None;
                state.rect = None;
                state.moving = false;
            } else if let Some(anchor) = state.anchor {
                needs_redraw = true;
                if state.moving {
                    if let Some(rect) = state.rect {
                        state.rect = Some(rect.moved(
                            f64::from(point.x - state.last_pointer.x),
                            f64::from(point.y - state.last_pointer.y),
                            u32::from(self.width),
                            u32::from(self.height),
                        ));
                    }
                } else {
                    let end = clamp_point_to(
                        Point {
                            x: point.x.saturating_add(state.resize_offset.x),
                            y: point.y.saturating_add(state.resize_offset.y),
                        },
                        width,
                        height,
                    );
                    state.rect = Some(Rect::between(
                        anchor.x as f64,
                        anchor.y as f64,
                        end.x as f64,
                        end.y as f64,
                    ));
                }
            }
            state.last_pointer = point;
        }
        if window_pick {
            let previous_target = self
                .selection
                .as_ref()
                .and_then(|state| state.window_target);
            self.update_window_target(context)?;
            let current_target = self
                .selection
                .as_ref()
                .and_then(|state| state.window_target);
            needs_redraw = previous_target != current_target;
        }
        if needs_redraw {
            self.redraw_selection(context)?;
        }
        Ok(None)
    }

    fn selection_button_release(
        &mut self,
        context: &X11Context,
        event: &xproto::ButtonReleaseEvent,
    ) -> X11Result<Option<UiAction>> {
        if event.detail != 1 {
            return Ok(None);
        }
        // Use root coordinates so motion queued while the fast grab target is
        // active continues the drag correctly after the overlay is installed.
        let point = self.clamp_point(Point {
            x: i32::from(event.root_x),
            y: i32::from(event.root_y),
        });
        if self
            .selection
            .as_ref()
            .is_some_and(|state| state.window_pick)
        {
            if let Some(state) = self.selection.as_mut() {
                state.pointer = point;
            }
            self.update_window_target(context)?;
            let target = self
                .selection
                .as_ref()
                .and_then(|state| state.window_target);
            if let Some(target) = target {
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|state| state.native_capture.is_some())
                {
                    let image = self.read_native_window(context, target)?;
                    self.close_selection(context)?;
                    return Ok(Some(UiAction::Captured(image)));
                }
                self.close_selection(context)?;
                return Ok(Some(UiAction::SelectedWindow(target)));
            }
            // A click without a target is the same no-op gesture as a click
            // without a non-zero region: finish the transient capture instead
            // of leaving the pointer grabbed until the user drags again.
            self.close_selection(context)?;
            return Ok(Some(UiAction::Cancelled));
        }
        let width = self.width;
        let height = self.height;
        let mut selected = None;
        if let Some(state) = self.selection.as_mut() {
            state.pointer = point;
            if let Some(anchor) = state.anchor {
                if !state.moving {
                    let end = clamp_point_to(
                        Point {
                            x: point.x.saturating_add(state.resize_offset.x),
                            y: point.y.saturating_add(state.resize_offset.y),
                        },
                        width,
                        height,
                    );
                    state.rect = Some(Rect::between(
                        anchor.x as f64,
                        anchor.y as f64,
                        end.x as f64,
                        end.y as f64,
                    ));
                }
                if state.rect.is_some_and(|rect| rect.valid()) {
                    selected = state.rect;
                } else {
                    state.anchor = None;
                    state.rect = None;
                }
            }
        }
        if let Some(rect) = selected {
            if let Some(image) = self.read_native_region(context, rect)? {
                self.close_selection(context)?;
                return Ok(Some(UiAction::Captured(image)));
            }
            self.close_selection(context)?;
            return Ok(Some(UiAction::Selected(rect)));
        }
        // A press/release at one point is an explicit cancel gesture.  Keeping
        // the overlay alive here made a fast habitual click appear to do
        // nothing and left the grab installed until a later Escape.
        self.close_selection(context)?;
        Ok(Some(UiAction::Cancelled))
    }

    fn read_native_region(&self, context: &X11Context, rect: Rect) -> X11Result<Option<RgbaImage>> {
        let Some(capture) = self
            .selection
            .as_ref()
            .and_then(|state| state.native_capture.as_ref())
        else {
            return Ok(None);
        };
        Ok(Some(capture.read_region(
            context,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
        )?))
    }

    fn read_native_window(
        &self,
        context: &X11Context,
        target: WindowTarget,
    ) -> X11Result<RgbaImage> {
        let capture = self
            .selection
            .as_ref()
            .and_then(|state| state.native_capture.as_ref())
            .ok_or_else(|| invalid("native selection capture is unavailable"))?;
        let (width, height) = capture.size();
        let rect = target
            .clipped_to_root(u32::from(width), u32::from(height))
            .ok_or_else(|| invalid("selected window is outside the captured root"))?;
        // Window selection must use the same frame that was frozen when the
        // shortcut was pressed.  A live Composite read here would reintroduce
        // animation or movement after the overlay was shown, which is exactly
        // what the frozen selection surface is meant to prevent.
        capture.read_region(context, rect.x, rect.y, rect.width, rect.height)
    }

    fn selection_key_press(
        &mut self,
        context: &X11Context,
        event: &xproto::KeyPressEvent,
    ) -> X11Result<Option<UiAction>> {
        if event.detail == self.keys.escape {
            self.close_selection(context)?;
            return Ok(Some(UiAction::Cancelled));
        }
        if event.detail == self.keys.space {
            let space_down = self
                .selection
                .as_ref()
                .is_some_and(|state| state.space_down);
            if space_down {
                return Ok(None);
            }
            if let Some(state) = self.selection.as_mut() {
                state.space_down = true;
            }
            let already_picking_window = self
                .selection
                .as_ref()
                .is_some_and(|state| state.window_pick);
            if already_picking_window {
                self.set_selection_cursor(context, self.resources.cursor)?;
                if let Some(state) = self.selection.as_mut() {
                    state.window_pick = false;
                    state.window_target = None;
                }
                self.redraw_selection(context)?;
                return Ok(None);
            }
            let window_pick = self
                .selection
                .as_ref()
                .is_some_and(|state| state.anchor.is_none() && state.rect.is_none());
            if window_pick {
                self.set_selection_cursor(context, self.resources.window_pick_cursor)?;
            }
            if let Some(state) = self.selection.as_mut() {
                if window_pick {
                    state.window_pick = true;
                } else if state.anchor.is_some() && state.rect.is_some_and(|rect| rect.valid()) {
                    state.moving = true;
                }
            }
            if window_pick {
                self.update_window_target(context)?;
            }
            self.redraw_selection(context)?;
            return Ok(None);
        }
        if (event.detail == self.keys.enter || event.detail == self.keys.keypad_enter)
            && self
                .selection
                .as_ref()
                .and_then(|state| state.rect)
                .is_some_and(|rect| rect.valid())
        {
            let rect = self
                .selection
                .as_ref()
                .and_then(|state| state.rect)
                .expect("valid selection rect");
            if let Some(image) = self.read_native_region(context, rect)? {
                self.close_selection(context)?;
                return Ok(Some(UiAction::Captured(image)));
            }
            self.close_selection(context)?;
            return Ok(Some(UiAction::Selected(rect)));
        }
        Ok(None)
    }

    fn selection_key_release(
        &mut self,
        context: &X11Context,
        event: &xproto::KeyReleaseEvent,
    ) -> X11Result<Option<UiAction>> {
        if event.detail != self.keys.space {
            return Ok(None);
        }
        // Core X11 can encode auto-repeat as a synthetic KeyRelease/KeyPress pair.
        // QueryKeymap distinguishes that release from a real edge while preserving
        // Space-held region moves and preventing window-pick mode from toggling.
        let space_still_down = context
            .conn
            .query_keymap()
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| {
                let index = usize::from(event.detail) / 8;
                let mask = 1u8 << (event.detail % 8);
                reply.keys.get(index).is_some_and(|keys| keys & mask != 0)
            })
            .unwrap_or(false);
        if space_still_down {
            if let Some(state) = self.selection.as_mut() {
                state.space_down = true;
            }
            return Ok(None);
        }
        if let Some(state) = self.selection.as_mut() {
            state.space_down = false;
        }
        if self
            .selection
            .as_ref()
            .is_some_and(|state| state.window_pick)
        {
            return Ok(None);
        }
        if let Some(state) = self.selection.as_mut() {
            state.moving = false;
            if let Some(rect) = state.rect.filter(|rect| rect.valid()) {
                let anchor = Point {
                    x: if state.pointer.x >= rect.x + (rect.width as i32 / 2) {
                        rect.x
                    } else {
                        rect.x + rect.width as i32
                    },
                    y: if state.pointer.y >= rect.y + (rect.height as i32 / 2) {
                        rect.y
                    } else {
                        rect.y + rect.height as i32
                    },
                };
                let corner = Point {
                    x: if anchor.x == rect.x {
                        rect.x + rect.width as i32
                    } else {
                        rect.x
                    },
                    y: if anchor.y == rect.y {
                        rect.y + rect.height as i32
                    } else {
                        rect.y
                    },
                };
                state.anchor = Some(anchor);
                state.resize_offset = Point {
                    x: corner.x - state.pointer.x,
                    y: corner.y - state.pointer.y,
                };
                state.last_pointer = state.pointer;
            }
        }
        self.redraw_selection(context)?;
        Ok(None)
    }

    fn preferences_button_press(
        &mut self,
        context: &X11Context,
        event: &xproto::ButtonPressEvent,
    ) -> X11Result<Option<UiAction>> {
        if event.detail != 1 {
            return Ok(None);
        }
        let (x, y) = (i32::from(event.event_x), i32::from(event.event_y));
        let mut action = None;
        let mut redraw = false;
        if let Some(preferences) = self.preferences.as_mut() {
            if (16..=160).contains(&x) && (GENERAL_TAB_Y..GENERAL_TAB_Y + TAB_HEIGHT).contains(&y) {
                if preferences.tab != PreferencesTab::General {
                    preferences.tab = PreferencesTab::General;
                    redraw = true;
                }
            } else if (16..=160).contains(&x)
                && (SHORTCUTS_TAB_Y..SHORTCUTS_TAB_Y + TAB_HEIGHT).contains(&y)
            {
                if preferences.tab != PreferencesTab::Shortcuts {
                    preferences.tab = PreferencesTab::Shortcuts;
                    redraw = true;
                }
            } else if preferences.tab == PreferencesTab::General
                && (GENERAL_PREVIEW_Y..GENERAL_PREVIEW_Y + 72).contains(&y)
            {
                preferences.settings.show_preview = !preferences.settings.show_preview;
                action = Some(UiAction::SettingsChanged(preferences.settings.clone()));
                redraw = true;
            } else if preferences.tab == PreferencesTab::General
                && (GENERAL_SAVE_Y..GENERAL_SAVE_Y + 72).contains(&y)
            {
                preferences.settings.save_automatically = !preferences.settings.save_automatically;
                action = Some(UiAction::SettingsChanged(preferences.settings.clone()));
                redraw = true;
            }
        }
        match action.as_ref() {
            Some(UiAction::SettingsChanged(_)) => self.redraw_preferences(context)?,
            _ => {}
        }
        if redraw && action.is_none() {
            self.redraw_preferences(context)?;
        }
        Ok(action)
    }

    fn about_button_press(
        &mut self,
        _context: &X11Context,
        event: &xproto::ButtonPressEvent,
    ) -> X11Result<Option<UiAction>> {
        if event.detail != 1 {
            return Ok(None);
        }
        let x = i32::from(event.event_x);
        let y = i32::from(event.event_y);
        if (136..=184).contains(&y) && (28..=412).contains(&x) {
            return Ok(Some(UiAction::OpenRepository));
        }
        Ok(None)
    }

    fn redraw_selection(&self, context: &X11Context) -> X11Result<()> {
        let Some(state) = self.selection.as_ref() else {
            return Ok(());
        };
        let drawable = state.window;
        let frame = state.frame;
        let width = min(state.snapshot.width, self.width);
        let height = min(state.snapshot.height, self.height);
        context
            .conn
            .copy_area(
                state.backdrop.pixmap,
                frame,
                self.resources.copy_gc,
                0,
                0,
                0,
                0,
                width,
                height,
            )?
            .check()?;
        if state.window_pick {
            if let Some(target) = state.window_target {
                if let Some(rectangle) = bounded_rectangle(target.rect, self.width, self.height) {
                    context
                        .conn
                        .copy_area(
                            state.snapshot.pixmap,
                            frame,
                            self.resources.copy_gc,
                            rectangle.x,
                            rectangle.y,
                            rectangle.x,
                            rectangle.y,
                            rectangle.width,
                            rectangle.height,
                        )?
                        .check()?;
                    if let Some(picture) = state.frame_picture {
                        let source = self
                            .resources
                            .window_pick_render
                            .as_ref()
                            .map(|resources| resources.source)
                            .unwrap_or(x11rb::NONE);
                        if source != x11rb::NONE {
                            context
                                .conn
                                .render_composite(
                                    render::PictOp::OVER,
                                    source,
                                    x11rb::NONE,
                                    picture,
                                    0,
                                    0,
                                    0,
                                    0,
                                    rectangle.x,
                                    rectangle.y,
                                    rectangle.width,
                                    rectangle.height,
                                )?
                                .check()?;
                        }
                    } else if let Some(highlight_gc) = self.resources.window_pick_gc {
                        context
                            .conn
                            .poly_fill_rectangle(frame, highlight_gc, &[rectangle])?
                            .check()?;
                    }
                    context
                        .conn
                        .poly_rectangle(frame, self.resources.selection_shadow_gc, &[rectangle])?
                        .check()?;
                    context
                        .conn
                        .poly_rectangle(frame, self.resources.outline_gc, &[rectangle])?
                        .check()?;
                }
            }
        } else if let Some(rect) = state.rect.filter(|rect| rect.valid()) {
            if let Some(rectangle) = bounded_rectangle(rect, self.width, self.height) {
                context
                    .conn
                    .copy_area(
                        state.snapshot.pixmap,
                        frame,
                        self.resources.copy_gc,
                        rectangle.x,
                        rectangle.y,
                        rectangle.x,
                        rectangle.y,
                        rectangle.width,
                        rectangle.height,
                    )?
                    .check()?;
                context
                    .conn
                    .poly_rectangle(frame, self.resources.selection_shadow_gc, &[rectangle])?
                    .check()?;
                context
                    .conn
                    .poly_rectangle(frame, self.resources.outline_gc, &[rectangle])?
                    .check()?;
                let corners = corner_points(rectangle);
                let shadow_handles = corners.map(|(x, y)| Rectangle {
                    x: clamp_i16(x - 3),
                    y: clamp_i16(y - 3),
                    width: 6,
                    height: 6,
                });
                context
                    .conn
                    .poly_fill_rectangle(
                        frame,
                        self.resources.selection_shadow_gc,
                        &shadow_handles,
                    )?
                    .check()?;
                let handles = corners.map(|(x, y)| Rectangle {
                    x: clamp_i16(x - 2),
                    y: clamp_i16(y - 2),
                    width: 4,
                    height: 4,
                });
                context
                    .conn
                    .poly_fill_rectangle(frame, self.resources.outline_gc, &handles)?
                    .check()?;
            }
        }
        context
            .conn
            .copy_area(
                frame,
                drawable,
                self.resources.copy_gc,
                0,
                0,
                0,
                0,
                width,
                height,
            )?
            .check()?;
        context.flush()?;
        Ok(())
    }

    fn redraw_preview(&self, context: &X11Context) -> X11Result<()> {
        let Some(preview) = self.preview.as_ref() else {
            return Ok(());
        };
        let drawable = preview.window;
        // SHAPE clips the outer corners on composited X11 desktops. Paint the same neutral frame
        // into the complete fallback rectangle first so servers without SHAPE do not expose black
        // corner holes while retaining the rounded surface wherever clipping is available.
        fill_rect(
            context,
            drawable,
            self.resources.frame_gc,
            0,
            0,
            preview.width,
            preview.height,
        )?;
        fill_rounded_rect(
            context,
            drawable,
            self.resources.frame_gc,
            Rectangle {
                x: 0,
                y: 0,
                width: preview.width,
                height: preview.height,
            },
            PREVIEW_RADIUS,
        )?;
        let image_x = PREVIEW_BORDER;
        let image_y = PREVIEW_BORDER;
        context
            .conn
            .copy_area(
                preview.thumbnail.pixmap,
                drawable,
                self.resources.copy_gc,
                0,
                0,
                clamp_i16(image_x),
                clamp_i16(image_y),
                preview.thumbnail.width,
                preview.thumbnail.height,
            )?
            .check()?;
        context.flush()?;
        Ok(())
    }

    fn redraw_preferences(&self, context: &X11Context) -> X11Result<()> {
        let Some(preferences) = self.preferences.as_ref() else {
            return Ok(());
        };
        let drawable = preferences.window;
        let width = PREFERENCES_WIDTH;
        let height = PREFERENCES_HEIGHT;
        fill_rect(
            context,
            drawable,
            self.resources.card_gc,
            0,
            0,
            width,
            height,
        )?;
        // Header and rail use the same restrained dark palette as the capture
        // overlay.  The rail is a real navigation region, rather than a
        // decorative border, so the second page remains easy to discover.
        fill_rect(
            context,
            drawable,
            self.resources.panel_gc,
            0,
            0,
            width,
            u16::try_from(PREFERENCES_HEADER_HEIGHT).unwrap_or(72),
        )?;
        fill_rect(
            context,
            drawable,
            self.resources.card_gc,
            0,
            PREFERENCES_HEADER_HEIGHT,
            PREFERENCES_SIDEBAR_WIDTH as u16,
            height.saturating_sub(PREFERENCES_HEADER_HEIGHT as u16),
        )?;
        fill_rect(
            context,
            drawable,
            self.resources.panel_border_gc,
            PREFERENCES_SIDEBAR_WIDTH,
            PREFERENCES_HEADER_HEIGHT,
            1,
            height.saturating_sub(PREFERENCES_HEADER_HEIGHT as u16),
        )?;
        context.flush()?;
        draw_surface_text(
            context,
            &self.resources,
            drawable,
            28,
            34,
            "Preferences",
            XftTextStyle::Title,
        )?;
        draw_surface_text(
            context,
            &self.resources,
            drawable,
            28,
            56,
            "Capture behavior and keyboard shortcuts.",
            XftTextStyle::Small,
        )?;

        draw_preferences_tab(
            context,
            &self.resources,
            drawable,
            GENERAL_TAB_Y,
            "General",
            preferences.tab == PreferencesTab::General,
        )?;
        draw_preferences_tab(
            context,
            &self.resources,
            drawable,
            SHORTCUTS_TAB_Y,
            "Shortcuts",
            preferences.tab == PreferencesTab::Shortcuts,
        )?;

        match preferences.tab {
            PreferencesTab::General => {
                draw_surface_text(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X,
                    116,
                    "Capture behavior",
                    XftTextStyle::Title,
                )?;
                draw_surface_text(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X,
                    140,
                    "Choose what happens after each capture.",
                    XftTextStyle::Small,
                )?;
                draw_preference_panel(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X,
                    GENERAL_PREVIEW_Y,
                    PREFERENCES_CONTENT_WIDTH,
                    72,
                )?;
                draw_surface_text(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X + 18,
                    GENERAL_PREVIEW_Y + 28,
                    "Floating preview",
                    XftTextStyle::Body,
                )?;
                draw_surface_text(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X + 18,
                    GENERAL_PREVIEW_Y + 50,
                    "Show a preview after capture.",
                    XftTextStyle::Small,
                )?;
                draw_toggle(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X + i32::from(PREFERENCES_CONTENT_WIDTH) - 52 - 16,
                    GENERAL_PREVIEW_Y + 23,
                    preferences.settings.show_preview,
                )?;

                draw_preference_panel(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X,
                    GENERAL_SAVE_Y,
                    PREFERENCES_CONTENT_WIDTH,
                    72,
                )?;
                draw_surface_text(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X + 18,
                    GENERAL_SAVE_Y + 28,
                    "Save screenshots",
                    XftTextStyle::Body,
                )?;
                draw_surface_text(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X + 18,
                    GENERAL_SAVE_Y + 50,
                    "Save captures as PNG files.",
                    XftTextStyle::Small,
                )?;
                draw_toggle(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X + i32::from(PREFERENCES_CONTENT_WIDTH) - 52 - 16,
                    GENERAL_SAVE_Y + 23,
                    preferences.settings.save_automatically,
                )?;
                draw_surface_text(
                    context,
                    &self.resources,
                    drawable,
                    PREFERENCES_CONTENT_X,
                    350,
                    "Changes apply to the next capture.",
                    XftTextStyle::Small,
                )?;
            }
            PreferencesTab::Shortcuts => {
                redraw_shortcuts(context, &self.resources, drawable, &preferences.shortcuts)?;
            }
        }
        sync_surface_text(&self.resources);
        context.flush()?;
        Ok(())
    }

    fn redraw_about(&self, context: &X11Context) -> X11Result<()> {
        let Some(about) = self.about.as_ref() else {
            return Ok(());
        };
        let drawable = about.window;
        let width = ABOUT_WIDTH;
        let height = ABOUT_HEIGHT;
        fill_rect(
            context,
            drawable,
            self.resources.card_gc,
            0,
            0,
            width,
            height,
        )?;
        fill_rect(context, drawable, self.resources.primary_gc, 28, 28, 4, 40)?;
        context.flush()?;
        draw_surface_text(
            context,
            &self.resources,
            drawable,
            46,
            52,
            "About SnipChord",
            XftTextStyle::Title,
        )?;
        let version = format!("Version {}", env!("CARGO_PKG_VERSION"));
        draw_surface_text(
            context,
            &self.resources,
            drawable,
            46,
            76,
            &version,
            XftTextStyle::Small,
        )?;
        draw_surface_text(
            context,
            &self.resources,
            drawable,
            46,
            124,
            "A lightweight X11 screenshot tool.",
            XftTextStyle::Body,
        )?;
        let github_mark = upload_github_mark(context)?;
        context
            .conn
            .copy_area(
                github_mark.pixmap,
                drawable,
                self.resources.copy_gc,
                0,
                0,
                46,
                145,
                github_mark.width,
                github_mark.height,
            )?
            .check()?;
        free_server_image_checked(context, &github_mark)?;
        draw_surface_text(
            context,
            &self.resources,
            drawable,
            80,
            164,
            "GitHub",
            XftTextStyle::Button,
        )?;
        // A quiet underline makes the row read as a link without turning the
        // About window into a second button bar.
        fill_rect(context, drawable, self.resources.primary_gc, 80, 170, 48, 1)?;
        sync_surface_text(&self.resources);
        context.flush()?;
        Ok(())
    }

    fn redraw_notice(&self, context: &X11Context) -> X11Result<()> {
        let Some(notice) = self.notice.as_ref() else {
            return Ok(());
        };
        let drawable = notice.window;
        fill_rect(
            context,
            drawable,
            self.resources.card_gc,
            0,
            0,
            notice.width,
            notice.height,
        )?;
        draw_text(context, &self.resources, drawable, 18, 30, &notice.title)?;
        draw_text(context, &self.resources, drawable, 18, 58, &notice.body)?;
        context
            .conn
            .poly_rectangle(
                drawable,
                self.resources.outline_gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: notice.width.saturating_sub(1),
                    height: notice.height.saturating_sub(1),
                }],
            )?
            .check()?;
        context.flush()?;
        Ok(())
    }
}

fn invalid(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(io::Error::new(
        io::ErrorKind::InvalidInput,
        message.to_owned(),
    ))
}

fn selection_event_mask() -> EventMask {
    EventMask::EXPOSURE
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::STRUCTURE_NOTIFY
}

fn selection_grab_event_mask() -> EventMask {
    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION
}

fn preview_event_mask() -> EventMask {
    EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY
}

fn preferences_event_mask() -> EventMask {
    EventMask::EXPOSURE
        | EventMask::BUTTON_PRESS
        | EventMask::KEY_PRESS
        | EventMask::STRUCTURE_NOTIFY
}

fn about_event_mask() -> EventMask {
    EventMask::EXPOSURE
        | EventMask::BUTTON_PRESS
        | EventMask::KEY_PRESS
        | EventMask::STRUCTURE_NOTIFY
}

fn notice_event_mask() -> EventMask {
    EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY
}

fn alloc_color(context: &X11Context, red: u16, green: u16, blue: u16) -> u32 {
    match context
        .conn
        .alloc_color(context.screen.default_colormap, red, green, blue)
    {
        Ok(cookie) => cookie
            .reply()
            .map(|reply| reply.pixel)
            .unwrap_or(context.screen.black_pixel),
        Err(_) => context.screen.black_pixel,
    }
}

fn create_window_pick_render(context: &X11Context) -> X11Result<WindowPickRender> {
    let formats = context.conn.render_query_pict_formats()?.reply()?;
    let destination_format = formats
        .screens
        .iter()
        .flat_map(|screen| screen.depths.iter())
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual == context.visual())
        .map(|visual| visual.format)
        .ok_or_else(|| invalid("X11 Render has no format for the root visual"))?;
    let source = context.alloc_id()?;
    let result: X11Result<()> = (|| {
        context
            .conn
            .render_create_solid_fill(
                source,
                render::Color {
                    // Render compositing uses premultiplied source channels. These are the
                    // #3c9fff tint channels multiplied by the 0x5000 (~31%) alpha below.
                    red: 0x12e8,
                    green: 0x3200,
                    blue: 0x5000,
                    alpha: 0x5000,
                },
            )?
            .check()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = context.conn.render_free_picture(source);
        return Err(error);
    }
    Ok(WindowPickRender {
        source,
        destination_format,
    })
}

fn create_window_pick_resources(
    context: &X11Context,
    foreground: u32,
) -> X11Result<(Option<Gcontext>, Option<Pixmap>)> {
    let stipple = create_bitmap_pixmap(context, 4, 4, &[0x01, 0x02, 0x04, 0x08])?;
    let gc = match context.create_gc(
        context.root(),
        &CreateGCAux::new()
            .foreground(foreground)
            .background(foreground)
            .fill_style(FillStyle::STIPPLED)
            .stipple(stipple)
            .tile_stipple_x_origin(0)
            .tile_stipple_y_origin(0)
            .graphics_exposures(0u32),
    ) {
        Ok(gc) => gc,
        Err(error) => {
            let _ = context.conn.free_pixmap(stipple);
            return Err(error);
        }
    };
    Ok((Some(gc), Some(stipple)))
}

fn create_camera_cursor(context: &X11Context) -> X11Result<xproto::Cursor> {
    // A small monochrome camera silhouette keeps the window-pick state
    // recognizable on X11 servers that do not provide a themed camera cursor.
    // The hotspot is centered because the selected target is resolved from
    // root coordinates, independently of the cursor bitmap.
    const MASK: [u16; 16] = [
        0, 0, 0, 0x03c0, 0x1ff8, 0x1ff8, 0x1ff8, 0x1ff8, 0x1ff8, 0x1ff8, 0x1ff8, 0x1ff8, 0x0ff0, 0,
        0, 0,
    ];
    const SOURCE: [u16; 16] = [
        0, 0, 0, 0x03c0, 0x1ff8, 0x1e78, 0x1c38, 0x1c38, 0x1c38, 0x1e78, 0x1ff8, 0x1ff8, 0x0ff0, 0,
        0, 0,
    ];
    let source = create_bitmap_pixmap(context, 16, 16, &bitmap_rows(&SOURCE))?;
    let mask = match create_bitmap_pixmap(context, 16, 16, &bitmap_rows(&MASK)) {
        Ok(mask) => mask,
        Err(error) => {
            let _ = context.conn.free_pixmap(source);
            return Err(error);
        }
    };
    let cursor = context.alloc_id()?;
    let create_result = (|| {
        context
            .conn
            .create_cursor(cursor, source, mask, 0xffff, 0xffff, 0xffff, 0, 0, 0, 8, 8)?
            .check()?;
        Ok(())
    })();
    let _ = context.conn.free_pixmap(source);
    let _ = context.conn.free_pixmap(mask);
    create_result.map(|()| cursor)
}

fn create_bitmap_pixmap(
    context: &X11Context,
    width: u16,
    height: u16,
    data: &[u8],
) -> X11Result<Pixmap> {
    let image = X11Image::new(
        width,
        height,
        ScanlinePad::Pad8,
        1,
        BitsPerPixel::B1,
        ImageOrder::LsbFirst,
        Cow::Borrowed(data),
    )?;
    let pixmap = context.alloc_id()?;
    if let Err(error) = context
        .conn
        .create_pixmap(1, pixmap, context.root(), width, height)?
        .check()
    {
        let _ = context.conn.free_pixmap(pixmap);
        return Err(error.into());
    }
    let gc = match context.create_gc(pixmap, &CreateGCAux::new().graphics_exposures(0u32)) {
        Ok(gc) => gc,
        Err(error) => {
            let _ = context.conn.free_pixmap(pixmap);
            return Err(error);
        }
    };
    let upload_result = context.put_image(&image, pixmap, gc);
    let free_gc_result = (|| {
        context.conn.free_gc(gc)?.check()?;
        Ok(())
    })();
    if let Err(error) = upload_result.and(free_gc_result) {
        let _ = context.conn.free_pixmap(pixmap);
        return Err(error);
    }
    Ok(pixmap)
}

fn bitmap_rows(rows: &[u16; 16]) -> [u8; 32] {
    let mut data = [0u8; 32];
    for (index, row) in rows.iter().copied().enumerate() {
        data[index * 2] = row as u8;
        data[index * 2 + 1] = (row >> 8) as u8;
    }
    data
}

fn resolve_keycodes(context: &X11Context) -> Keycodes {
    let fallback = Keycodes {
        escape: 9,
        space: 65,
        enter: 36,
        keypad_enter: 104,
    };
    let first = context.conn.setup().min_keycode;
    let count = context
        .conn
        .setup()
        .max_keycode
        .saturating_sub(first)
        .saturating_add(1);
    let Ok(cookie) = context.conn.get_keyboard_mapping(first, count) else {
        return fallback;
    };
    let Ok(reply) = cookie.reply() else {
        return fallback;
    };
    let stride = usize::from(reply.keysyms_per_keycode);
    if stride == 0 {
        return fallback;
    }
    let mut result = fallback;
    for (index, symbols) in reply.keysyms.chunks(stride).enumerate() {
        let code = first.saturating_add(index as u8);
        for symbol in symbols {
            match *symbol {
                XK_ESCAPE => result.escape = code,
                XK_SPACE => result.space = code,
                XK_RETURN => result.enter = code,
                XK_KP_ENTER => result.keypad_enter = code,
                _ => {}
            }
        }
    }
    result
}

fn upload_pixmap(context: &X11Context, image: RgbaView<'_>) -> X11Result<ServerImage> {
    image.validate()?;
    let width =
        u16::try_from(image.width).map_err(|_| invalid("image width exceeds X11 limits"))?;
    let height =
        u16::try_from(image.height).map_err(|_| invalid("image height exceeds X11 limits"))?;
    let native = native_image(context, image)?;
    let pixmap = context.create_pixmap(width, height)?;
    let gc = match context.create_gc(pixmap, &CreateGCAux::new().graphics_exposures(0u32)) {
        Ok(gc) => gc,
        Err(error) => {
            let _ = context.conn.free_pixmap(pixmap);
            return Err(error);
        }
    };
    if let Err(error) = context.put_image(&native, pixmap, gc) {
        let _ = context.conn.free_gc(gc);
        let _ = context.conn.free_pixmap(pixmap);
        return Err(error);
    }
    let free_gc_result: X11Result<()> = (|| {
        context.conn.free_gc(gc)?.check()?;
        Ok(())
    })();
    if let Err(error) = free_gc_result {
        let _ = context.conn.free_pixmap(pixmap);
        return Err(error);
    }
    Ok(ServerImage {
        pixmap,
        width,
        height,
        owned: true,
    })
}

fn native_image(context: &X11Context, image: RgbaView<'_>) -> X11Result<X11Image<'static>> {
    let format = context.format_for_depth(context.depth())?;
    let scanline_pad: ScanlinePad = format
        .scanline_pad
        .try_into()
        .map_err(|_| invalid("unsupported X11 scanline padding"))?;
    let bits_per_pixel: BitsPerPixel = format
        .bits_per_pixel
        .try_into()
        .map_err(|_| invalid("unsupported X11 pixel format"))?;
    let byte_order: ImageOrder = context
        .conn
        .setup()
        .image_byte_order
        .try_into()
        .map_err(|_| invalid("unsupported X11 byte order"))?;
    let bpp = usize::from(bits_per_pixel);
    if bpp < 8 || bpp % 8 != 0 || bpp > 32 {
        return Err(invalid("unsupported X11 bits per pixel"));
    }
    let bytes_per_pixel = bpp / 8;
    let pad_bits = usize::from(u8::from(scanline_pad));
    let row_bits = usize::from(
        u16::try_from(image.width).map_err(|_| invalid("image width exceeds X11 limits"))?,
    ) * bpp;
    let stride = row_bits.saturating_add(pad_bits.saturating_sub(1)) / pad_bits * (pad_bits / 8);
    let mut data = vec![
        0u8;
        stride
            * usize::from(
                u16::try_from(image.height)
                    .map_err(|_| invalid("image height exceeds X11 limits"))?
            )
    ];
    let width = usize::try_from(image.width).map_err(|_| invalid("image width is too large"))?;
    let height = usize::try_from(image.height).map_err(|_| invalid("image height is too large"))?;
    // Most X11 desktops use an 8-bit RGB layout packed into 24/32-bit BGRX words.  Detect that
    // layout once and use straight byte stores for the large selection snapshot uploads; the
    // general mask-aware path below remains available for unusual visuals and test fixtures.
    let common_rgb = (bpp == 24 || bpp == 32)
        && encode_rgb(context.root_layout, [0xff, 0, 0]) == 0x00ff0000
        && encode_rgb(context.root_layout, [0, 0xff, 0]) == 0x0000ff00
        && encode_rgb(context.root_layout, [0, 0, 0xff]) == 0x000000ff;
    if common_rgb {
        for y in 0..height {
            let source_row = y * width * 4;
            let destination_row = y * stride;
            for x in 0..width {
                let source = source_row + x * 4;
                let destination = destination_row + x * bytes_per_pixel;
                match (byte_order, bytes_per_pixel) {
                    (ImageOrder::LsbFirst, 4) => {
                        data[destination] = image.pixels[source + 2];
                        data[destination + 1] = image.pixels[source + 1];
                        data[destination + 2] = image.pixels[source];
                    }
                    (ImageOrder::LsbFirst, 3) => {
                        data[destination] = image.pixels[source + 2];
                        data[destination + 1] = image.pixels[source + 1];
                        data[destination + 2] = image.pixels[source];
                    }
                    (ImageOrder::MsbFirst, 4) => {
                        data[destination + 1] = image.pixels[source];
                        data[destination + 2] = image.pixels[source + 1];
                        data[destination + 3] = image.pixels[source + 2];
                    }
                    (ImageOrder::MsbFirst, 3) => {
                        data[destination] = image.pixels[source];
                        data[destination + 1] = image.pixels[source + 1];
                        data[destination + 2] = image.pixels[source + 2];
                    }
                    _ => unreachable!("common X11 RGB layout has 24 or 32 bits per pixel"),
                }
            }
        }
    } else {
        for y in 0..height {
            for x in 0..width {
                let source = (y * width + x) * 4;
                let pixel = encode_rgb(
                    context.root_layout,
                    [
                        image.pixels[source],
                        image.pixels[source + 1],
                        image.pixels[source + 2],
                    ],
                );
                let bytes = match byte_order {
                    ImageOrder::LsbFirst => pixel.to_le_bytes(),
                    ImageOrder::MsbFirst => pixel.to_be_bytes(),
                };
                let destination = y * stride + x * bytes_per_pixel;
                if matches!(byte_order, ImageOrder::LsbFirst) {
                    data[destination..destination + bytes_per_pixel]
                        .copy_from_slice(&bytes[..bytes_per_pixel]);
                } else {
                    data[destination..destination + bytes_per_pixel]
                        .copy_from_slice(&bytes[4 - bytes_per_pixel..]);
                }
            }
        }
    }
    Ok(X11Image::new(
        u16::try_from(image.width).map_err(|_| invalid("image width exceeds X11 limits"))?,
        u16::try_from(image.height).map_err(|_| invalid("image height exceeds X11 limits"))?,
        scanline_pad,
        context.depth(),
        bits_per_pixel,
        byte_order,
        Cow::Owned(data),
    )?)
}

fn make_thumbnail(image: &RgbaImage) -> X11Result<(RgbaImage, u16, u16)> {
    if image.width() == 0 || image.height() == 0 {
        return Err(invalid("cannot preview an empty image"));
    }
    let scale = (PREVIEW_MAX_WIDTH as f64 / image.width() as f64)
        .min(PREVIEW_MAX_HEIGHT as f64 / image.height() as f64)
        .min(1.0);
    let width = max(1, (image.width() as f64 * scale).round() as u32);
    let height = max(1, (image.height() as f64 * scale).round() as u32);
    let mut output = vec![
        0u8;
        usize::try_from(
            width
                .checked_mul(height)
                .ok_or_else(|| invalid("thumbnail dimensions overflow"))?
        )
        .map_err(|_| invalid("thumbnail is too large"))?
            * 4
    ];
    for y in 0..height {
        let source_y = min(image.height() - 1, y * image.height() / height);
        for x in 0..width {
            let source_x = min(image.width() - 1, x * image.width() / width);
            let source = (usize::try_from(source_y * image.width() + source_x)
                .map_err(|_| invalid("thumbnail source overflow"))?)
                * 4;
            let target = (usize::try_from(y * width + x)
                .map_err(|_| invalid("thumbnail target overflow"))?)
                * 4;
            output[target..target + 4].copy_from_slice(&image.pixels()[source..source + 4]);
        }
    }
    let thumbnail = RgbaImage::from_rgba(width, height, output)?;
    Ok((
        thumbnail,
        u16::try_from(width).map_err(|_| invalid("thumbnail width is too large"))?,
        u16::try_from(height).map_err(|_| invalid("thumbnail height is too large"))?,
    ))
}

fn cleanup_selection(
    context: &X11Context,
    state: SelectionState,
    destroy_window: bool,
) -> X11Result<()> {
    let mut first_error = None;
    if state.pointer_grabbed {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.ungrab_pointer(CURRENT_TIME)?.check()?;
                Ok(())
            })(),
        );
    }
    if state.keyboard_grabbed {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.ungrab_keyboard(CURRENT_TIME)?.check()?;
                Ok(())
            })(),
        );
    }
    if state.pointer_grabbed || state.keyboard_grabbed {
        remember_error(
            &mut first_error,
            restore_input_focus(context, state.previous_focus),
        );
    }
    if destroy_window {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.destroy_window(state.window)?.check()?;
                Ok(())
            })(),
        );
    }
    if let Some(capture) = state.native_capture {
        remember_error(&mut first_error, capture.destroy(context));
    } else {
        remember_error(
            &mut first_error,
            free_server_image_checked(context, &state.snapshot),
        );
        remember_error(
            &mut first_error,
            free_server_image_checked(context, &state.backdrop),
        );
    }
    if let Some(picture) = state.frame_picture {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.render_free_picture(picture)?.check()?;
                Ok(())
            })(),
        );
    }
    remember_error(
        &mut first_error,
        (|| {
            context.conn.free_pixmap(state.frame)?.check()?;
            Ok(())
        })(),
    );
    remember_error(&mut first_error, context.flush());
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn cleanup_capture_cursor(context: &X11Context, cursor: CaptureCursor) -> X11Result<()> {
    let mut first_error = None;
    if cursor.pointer_grabbed {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.ungrab_pointer(CURRENT_TIME)?.check()?;
                Ok(())
            })(),
        );
    }
    if cursor.keyboard_grabbed {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.ungrab_keyboard(CURRENT_TIME)?.check()?;
                Ok(())
            })(),
        );
    }
    remember_error(&mut first_error, context.flush());
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn cleanup_capture_surfaces(
    context: &X11Context,
    snapshot: &ServerImage,
    backdrop: &ServerImage,
    native_capture: Option<ServerCapture>,
) {
    if let Some(capture) = native_capture {
        let _ = capture.destroy(context);
    } else {
        free_server_image(context, snapshot);
        free_server_image(context, backdrop);
    }
}

fn remember_error(
    first_error: &mut Option<Box<dyn std::error::Error + Send + Sync>>,
    result: X11Result<()>,
) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

/// Closing a tray-launched dialog can race the panel destroying the window
/// that owned focus before the dialog opened.  X11 reports that stale target
/// as BadWindow or BadMatch; both are safe to recover from by returning focus
/// to the pointer root.  Keep all other errors visible to the caller.
fn is_stale_focus_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let Some(error) = error.downcast_ref::<x11rb::errors::ReplyError>() else {
        return false;
    };
    matches!(
        error,
        x11rb::errors::ReplyError::X11Error(error)
            if matches!(
                error.error_kind,
                x11rb::protocol::ErrorKind::Window | x11rb::protocol::ErrorKind::Match
            )
    )
}

fn restore_input_focus(context: &X11Context, focus: Window) -> X11Result<()> {
    let (revert_to, target) = if focus == x11rb::NONE {
        (InputFocus::NONE, x11rb::NONE)
    } else if focus == u32::from(InputFocus::POINTER_ROOT) {
        (
            InputFocus::POINTER_ROOT,
            u32::from(InputFocus::POINTER_ROOT),
        )
    } else {
        (InputFocus::NONE, focus)
    };
    let result = context
        .conn
        .set_input_focus(revert_to, target, CURRENT_TIME)?
        .check();
    match result {
        Ok(()) => Ok(()),
        Err(error) if is_stale_focus_error(&error) => {
            // The panel's transient focus owner disappeared.  Falling back
            // to the pointer root keeps the resident daemon alive and gives
            // the next tray dialog a valid focus target.
            context
                .conn
                .set_input_focus(
                    InputFocus::POINTER_ROOT,
                    u32::from(InputFocus::POINTER_ROOT),
                    CURRENT_TIME,
                )?
                .check()?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn cleanup_preview(
    context: &X11Context,
    state: &PreviewState,
    destroy_window: bool,
) -> X11Result<()> {
    let mut first_error = None;
    if destroy_window {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.destroy_window(state.window)?.check()?;
                Ok(())
            })(),
        );
    }
    remember_error(
        &mut first_error,
        free_server_image_checked(context, &state.thumbnail),
    );
    remember_error(&mut first_error, context.flush());
    first_error.map_or(Ok(()), Err)
}

fn cleanup_preferences(
    context: &X11Context,
    state: &PreferencesState,
    destroy_window: bool,
) -> X11Result<()> {
    let mut first_error = None;
    if destroy_window {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.destroy_window(state.window)?.check()?;
                Ok(())
            })(),
        );
    }
    remember_error(
        &mut first_error,
        restore_input_focus(context, state.previous_focus),
    );
    remember_error(&mut first_error, context.flush());
    first_error.map_or(Ok(()), Err)
}

fn cleanup_about(context: &X11Context, state: &AboutState, destroy_window: bool) -> X11Result<()> {
    let mut first_error = None;
    if destroy_window {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.destroy_window(state.window)?.check()?;
                Ok(())
            })(),
        );
    }
    remember_error(
        &mut first_error,
        restore_input_focus(context, state.previous_focus),
    );
    remember_error(&mut first_error, context.flush());
    first_error.map_or(Ok(()), Err)
}

fn cleanup_notice(
    context: &X11Context,
    state: &NoticeState,
    destroy_window: bool,
) -> X11Result<()> {
    let mut first_error = None;
    if destroy_window {
        remember_error(
            &mut first_error,
            (|| {
                context.conn.destroy_window(state.window)?.check()?;
                Ok(())
            })(),
        );
    }
    remember_error(&mut first_error, context.flush());
    first_error.map_or(Ok(()), Err)
}

fn free_server_image(context: &X11Context, image: &ServerImage) {
    if image.owned {
        let _ = context.conn.free_pixmap(image.pixmap);
    }
}

fn free_server_image_checked(context: &X11Context, image: &ServerImage) -> X11Result<()> {
    if image.owned {
        context.conn.free_pixmap(image.pixmap)?.check()?;
    }
    Ok(())
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut output = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        output.push('…');
    }
    output
}

/// Center a transient dialog on the monitor that contains the current pointer.
///
/// Tray actions are invoked from the panel, so the pointer monitor is the useful
/// anchor for the dialog.  Using the monitor workarea keeps the panel/taskbar
/// out of the centered bounds and also behaves sensibly when the pointer is on
/// a secondary monitor.
fn dialog_origin(
    context: &X11Context,
    root_width: u16,
    root_height: u16,
    width: u16,
    height: u16,
) -> (i32, i32) {
    let fallback = MonitorRect::new(0, 0, u32::from(root_width), u32::from(root_height));
    let pointer = context
        .pointer_position()
        .map(|(x, y)| Point { x, y })
        .unwrap_or(Point {
            x: i32::from(root_width) / 2,
            y: i32::from(root_height) / 2,
        });
    let monitors = context
        .monitor_rects()
        .unwrap_or_else(|_| vec![(fallback.x, fallback.y, fallback.width, fallback.height)])
        .into_iter()
        .map(|(x, y, width, height)| MonitorRect::new(x, y, width, height))
        .collect::<Vec<_>>();
    let monitor = workarea_for(context, monitor_for(pointer, &monitors, fallback));
    let available_width = i32::try_from(monitor.width).unwrap_or(i32::MAX);
    let available_height = i32::try_from(monitor.height).unwrap_or(i32::MAX);
    let x = monitor.x + ((available_width - i32::from(width)).max(0) / 2);
    let y = monitor.y + ((available_height - i32::from(height)).max(0) / 2);
    (x, y)
}

/// Tell a reparenting WM both the requested position and the fixed client size.
/// The explicit ConfigureWindow request remains useful for simple WMs and test
/// fixtures, while these ICCCM hints let GNOME honor the same geometry when it
/// manages the window during MapRequest.
fn set_dialog_window_hints(
    context: &X11Context,
    window: Window,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
) -> X11Result<()> {
    let size = (i32::from(width), i32::from(height));
    WmSizeHints {
        position: Some((WmSizeHintsSpecification::ProgramSpecified, x, y)),
        size: Some((WmSizeHintsSpecification::ProgramSpecified, size.0, size.1)),
        min_size: Some(size),
        max_size: Some(size),
        ..WmSizeHints::new()
    }
    .set_normal_hints(&context.conn, window)?
    .check()?;
    Ok(())
}

fn monitor_for(pointer: Point, monitors: &[MonitorRect], fallback: MonitorRect) -> MonitorRect {
    monitors
        .iter()
        .copied()
        .find(|monitor| monitor.contains(pointer))
        .or_else(|| monitors.first().copied())
        .unwrap_or(fallback)
}

fn workarea_for(context: &X11Context, monitor: MonitorRect) -> MonitorRect {
    let atom = match context.intern_atom(b"_NET_WORKAREA") {
        Ok(atom) => atom,
        Err(_) => return monitor,
    };
    let cardinal: u32 = xproto::AtomEnum::CARDINAL.into();
    let Ok(cookie) = context
        .conn
        .get_property(false, context.root(), atom, cardinal, 0, 4)
    else {
        return monitor;
    };
    let Ok(reply) = cookie.reply() else {
        return monitor;
    };
    let Some(mut values) = reply.value32() else {
        return monitor;
    };
    let Some(x) = values.next().and_then(|value| i32::try_from(value).ok()) else {
        return monitor;
    };
    let Some(y) = values.next().and_then(|value| i32::try_from(value).ok()) else {
        return monitor;
    };
    let Some(width) = values.next().filter(|value| *value != 0) else {
        return monitor;
    };
    let Some(height) = values.next().filter(|value| *value != 0) else {
        return monitor;
    };
    let workarea = MonitorRect::new(x, y, width, height);
    intersect_monitor(monitor, workarea).unwrap_or(monitor)
}

fn intersect_monitor(first: MonitorRect, second: MonitorRect) -> Option<MonitorRect> {
    let left = first.x.max(second.x);
    let top = first.y.max(second.y);
    let right = (i64::from(first.x) + i64::from(first.width))
        .min(i64::from(second.x) + i64::from(second.width));
    let bottom = (i64::from(first.y) + i64::from(first.height))
        .min(i64::from(second.y) + i64::from(second.height));
    if right <= i64::from(left) || bottom <= i64::from(top) {
        return None;
    }
    Some(MonitorRect::new(
        left,
        top,
        u32::try_from(right - i64::from(left)).ok()?,
        u32::try_from(bottom - i64::from(top)).ok()?,
    ))
}

fn bounded_rectangle(rect: Rect, width: u16, height: u16) -> Option<Rectangle> {
    let x = rect.x.clamp(0, i32::from(width));
    let y = rect.y.clamp(0, i32::from(height));
    if x >= i32::from(width) || y >= i32::from(height) {
        return None;
    }
    let available_width = i32::from(width) - x;
    let available_height = i32::from(height) - y;
    Some(Rectangle {
        x: clamp_i16(x),
        y: clamp_i16(y),
        width: min(rect.width, available_width as u32).max(1) as u16,
        height: min(rect.height, available_height as u32).max(1) as u16,
    })
}

fn corner_points(rect: Rectangle) -> [(i32, i32); 4] {
    let x = i32::from(rect.x);
    let y = i32::from(rect.y);
    let right = x + i32::from(rect.width);
    let bottom = y + i32::from(rect.height);
    [(x, y), (right, y), (x, bottom), (right, bottom)]
}

fn fill_rect(
    context: &X11Context,
    drawable: Window,
    gc: Gcontext,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
) -> X11Result<()> {
    let width = min(width, i16::MAX as u16);
    let height = min(height, i16::MAX as u16);
    context
        .conn
        .poly_fill_rectangle(
            drawable,
            gc,
            &[Rectangle {
                x: clamp_i16(x),
                y: clamp_i16(y),
                width,
                height,
            }],
        )?
        .check()?;
    Ok(())
}

/// Paint a rounded rectangle using one X11 core request. Core X11 has no alpha channel, so the
/// preview uses a minimal frame and lets the SHAPE extension clip the image corners. The rows
/// also keep the fallback (when SHAPE is unavailable) visually coherent without a compositor-
/// specific transparent window.
fn fill_rounded_rect(
    context: &X11Context,
    drawable: Window,
    gc: Gcontext,
    bounds: Rectangle,
    radius: u16,
) -> X11Result<()> {
    let width = i32::from(bounds.width);
    let height = i32::from(bounds.height);
    if width <= 0 || height <= 0 {
        return Ok(());
    }
    let radius = i32::from(radius).min(width / 2).min(height / 2);
    if radius == 0 {
        return fill_rect(
            context,
            drawable,
            gc,
            i32::from(bounds.x),
            i32::from(bounds.y),
            bounds.width,
            bounds.height,
        );
    }

    let mut rows = Vec::with_capacity(usize::try_from(height).unwrap_or(0));
    let radius_squared = f64::from(radius * radius);
    for row in 0..height {
        let distance = if row < radius {
            radius - row - 1
        } else if row >= height - radius {
            row - (height - radius)
        } else {
            0
        };
        let inset = if distance == 0 {
            0
        } else {
            let horizontal = (radius_squared - f64::from(distance * distance)).sqrt();
            (f64::from(radius) - horizontal).ceil() as i32
        };
        let row_width = width - inset * 2;
        if row_width > 0 {
            rows.push(Rectangle {
                x: clamp_i16(i32::from(bounds.x) + inset),
                y: clamp_i16(i32::from(bounds.y) + row),
                width: u16::try_from(row_width).map_err(|_| invalid("rounded row is too wide"))?,
                height: 1,
            });
        }
    }
    context
        .conn
        .poly_fill_rectangle(drawable, gc, &rows)?
        .check()?;
    Ok(())
}

/// Give the transient thumbnail a native rounded shape where the X11 Shape extension is
/// available.  SHAPE is optional on older X servers, so the ordinary rectangular preview remains
/// a safe fallback.
fn shape_preview_window(
    context: &X11Context,
    window: Window,
    width: u16,
    height: u16,
    shape_supported: bool,
) -> X11Result<()> {
    if !shape_supported {
        return Ok(());
    }
    let bounds = Rectangle {
        x: 0,
        y: 0,
        width,
        height,
    };
    let rows = rounded_shape_rows(bounds, PREVIEW_RADIUS);
    context
        .conn
        .shape_rectangles(
            ShapeOperation::SET,
            ShapeKind::BOUNDING,
            ClipOrdering::Y_SORTED,
            window,
            0,
            0,
            &rows,
        )?
        .check()?;
    // Keep the input shape aligned with the visible rounded image so a click on
    // the thumbnail reaches the application without making transparent corners
    // into a mouse target.
    context
        .conn
        .shape_rectangles(
            ShapeOperation::SET,
            ShapeKind::INPUT,
            ClipOrdering::Y_SORTED,
            window,
            0,
            0,
            &rows,
        )?
        .check()?;
    Ok(())
}

fn rounded_shape_rows(bounds: Rectangle, radius: u16) -> Vec<Rectangle> {
    let width = i32::from(bounds.width);
    let height = i32::from(bounds.height);
    let radius = i32::from(radius).min(width / 2).min(height / 2);
    if radius == 0 {
        return vec![bounds];
    }
    let radius_squared = f64::from(radius * radius);
    let mut rows = Vec::with_capacity(usize::try_from(height).unwrap_or(0));
    for row in 0..height {
        let distance = if row < radius {
            radius - row - 1
        } else if row >= height - radius {
            row - (height - radius)
        } else {
            0
        };
        let inset = if distance == 0 {
            0
        } else {
            let horizontal = (radius_squared - f64::from(distance * distance)).sqrt();
            (f64::from(radius) - horizontal).ceil() as i32
        };
        let row_width = width - inset * 2;
        if row_width > 0 {
            rows.push(Rectangle {
                x: clamp_i16(i32::from(bounds.x) + inset),
                y: clamp_i16(i32::from(bounds.y) + row),
                width: u16::try_from(row_width).unwrap_or(u16::MAX),
                height: 1,
            });
        }
    }
    rows
}

fn draw_text(
    context: &X11Context,
    resources: &Resources,
    drawable: Window,
    x: i32,
    baseline: i32,
    label: &str,
) -> X11Result<()> {
    let bytes = label.as_bytes();
    let length = min(bytes.len(), 255);
    context
        .conn
        .image_text8(
            drawable,
            resources.text_gc,
            clamp_i16(x),
            clamp_i16(baseline),
            &bytes[..length],
        )?
        .check()?;
    Ok(())
}

fn draw_surface_text(
    context: &X11Context,
    resources: &Resources,
    drawable: Window,
    x: i32,
    baseline: i32,
    label: &str,
    style: XftTextStyle,
) -> X11Result<()> {
    if let Some(xft) = resources.xft.get_or_init(XftResources::try_new).as_ref() {
        xft.draw_text(drawable, x, baseline, label, style);
    } else {
        draw_text(context, resources, drawable, x, baseline, label)?;
    }
    Ok(())
}

fn sync_surface_text(resources: &Resources) {
    if let Some(xft) = resources.xft.get().and_then(Option::as_ref) {
        xft.sync();
    }
}

fn draw_preferences_tab(
    context: &X11Context,
    resources: &Resources,
    drawable: Window,
    y: i32,
    label: &str,
    active: bool,
) -> X11Result<()> {
    let bounds = Rectangle {
        x: 16,
        y: clamp_i16(y),
        width: 144,
        height: TAB_HEIGHT as u16,
    };
    if active {
        fill_rounded_rect(context, drawable, resources.button_gc, bounds, 8)?;
        fill_rect(context, drawable, resources.primary_gc, 16, y + 4, 3, 32)?;
    }
    draw_surface_text(
        context,
        resources,
        drawable,
        32,
        y + 26,
        label,
        if active {
            XftTextStyle::Button
        } else {
            XftTextStyle::Small
        },
    )?;
    Ok(())
}

fn redraw_shortcuts(
    context: &X11Context,
    resources: &Resources,
    drawable: Window,
    shortcuts: &ShortcutList,
) -> X11Result<()> {
    draw_surface_text(
        context,
        resources,
        drawable,
        PREFERENCES_CONTENT_X,
        116,
        "Keyboard shortcuts",
        XftTextStyle::Title,
    )?;
    draw_surface_text(
        context,
        resources,
        drawable,
        PREFERENCES_CONTENT_X,
        140,
        if shortcuts.using_defaults {
            "Default shortcuts (desktop settings unavailable)."
        } else {
            "Read-only shortcuts from your desktop settings."
        },
        XftTextStyle::Small,
    )?;

    for (index, binding) in shortcuts.bindings.iter().take(4).enumerate() {
        let y = 160 + (index as i32 * 64);
        draw_preference_panel(
            context,
            resources,
            drawable,
            PREFERENCES_CONTENT_X,
            y,
            PREFERENCES_CONTENT_WIDTH,
            56,
        )?;
        draw_surface_text(
            context,
            resources,
            drawable,
            PREFERENCES_CONTENT_X + 16,
            y + 19,
            binding.name,
            XftTextStyle::Body,
        )?;
        draw_key_chip(
            context,
            resources,
            drawable,
            PREFERENCES_CONTENT_X + 16,
            y + 25,
            &binding.binding,
        )?;
    }
    if shortcuts.bindings.is_empty() {
        draw_preference_panel(
            context,
            resources,
            drawable,
            PREFERENCES_CONTENT_X,
            160,
            PREFERENCES_CONTENT_WIDTH,
            56,
        )?;
        draw_surface_text(
            context,
            resources,
            drawable,
            PREFERENCES_CONTENT_X + 16,
            191,
            "No shortcuts configured",
            XftTextStyle::Small,
        )?;
    }
    draw_surface_text(
        context,
        resources,
        drawable,
        PREFERENCES_CONTENT_X,
        444,
        "Read-only shortcut reference.",
        XftTextStyle::Small,
    )?;
    Ok(())
}

fn draw_key_chip(
    context: &X11Context,
    resources: &Resources,
    drawable: Window,
    x: i32,
    y: i32,
    label: &str,
) -> X11Result<()> {
    let bounds = Rectangle {
        x: clamp_i16(x),
        y: clamp_i16(y),
        width: 176,
        height: 26,
    };
    fill_rounded_rect(context, drawable, resources.button_gc, bounds, 6)?;
    draw_surface_text(
        context,
        resources,
        drawable,
        x + 10,
        y + 18,
        label,
        XftTextStyle::Small,
    )?;
    Ok(())
}

const GITHUB_MARK_PNG: &[u8] = include_bytes!("../assets/github-mark.png");

fn upload_github_mark(context: &X11Context) -> X11Result<ServerImage> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(GITHUB_MARK_PNG));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| invalid(&format!("could not decode GitHub mark: {error}")))?;
    let mut pixels = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut pixels)
        .map_err(|error| invalid(&format!("could not decode GitHub mark: {error}")))?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(invalid("GitHub mark is not an RGBA image"));
    }
    upload_pixmap(
        context,
        RgbaView::new(
            u32::from(info.width),
            u32::from(info.height),
            &pixels[..info.buffer_size()],
        ),
    )
}

fn draw_preference_panel(
    context: &X11Context,
    resources: &Resources,
    drawable: Window,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
) -> X11Result<()> {
    // Draw the border as a rounded outer fill and inset the panel by one
    // pixel.  A core X11 rectangle outline is square at the corners and was
    // the source of the visibly broken edges in the previous switches/cards.
    fill_rounded_rect(
        context,
        drawable,
        resources.panel_border_gc,
        Rectangle {
            x: clamp_i16(x),
            y: clamp_i16(y),
            width,
            height,
        },
        10,
    )?;
    if width > 2 && height > 2 {
        fill_rounded_rect(
            context,
            drawable,
            resources.panel_gc,
            Rectangle {
                x: clamp_i16(x + 1),
                y: clamp_i16(y + 1),
                width: width - 2,
                height: height - 2,
            },
            9,
        )?;
    }
    Ok(())
}

fn draw_toggle(
    context: &X11Context,
    resources: &Resources,
    drawable: Window,
    x: i32,
    y: i32,
    checked: bool,
) -> X11Result<()> {
    let track = Rectangle {
        x: clamp_i16(x),
        y: clamp_i16(y),
        width: 52,
        height: 26,
    };
    fill_rounded_rect(
        context,
        drawable,
        if checked {
            resources.primary_gc
        } else {
            resources.button_gc
        },
        track,
        13,
    )?;
    let thumb_x = if checked { x + 29 } else { x + 3 };
    fill_rounded_rect(
        context,
        drawable,
        if checked {
            resources.outline_gc
        } else {
            resources.frame_gc
        },
        Rectangle {
            x: clamp_i16(thumb_x),
            y: clamp_i16(y + 3),
            width: 20,
            height: 20,
        },
        10,
    )?;
    Ok(())
}

fn clamp_i16(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn clamp_point_to(point: Point, width: u16, height: u16) -> Point {
    Point {
        x: point.x.clamp(0, i32::from(width.saturating_sub(1))),
        y: point.y.clamp(0, i32::from(height.saturating_sub(1))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_image_corners_stay_inside_rounded_window_shape() {
        let border = u16::try_from(PREVIEW_BORDER).expect("preview border fits u16");
        let outer = Rectangle {
            x: 0,
            y: 0,
            width: u16::try_from(PREVIEW_MAX_WIDTH + u32::from(border) * 2)
                .expect("preview width fits u16"),
            height: u16::try_from(PREVIEW_MAX_HEIGHT + u32::from(border) * 2)
                .expect("preview height fits u16"),
        };
        let rows = rounded_shape_rows(outer, PREVIEW_RADIUS);
        let image_right = i32::from(border) + i32::try_from(PREVIEW_MAX_WIDTH).unwrap();
        let image_bottom = usize::from(border) + usize::try_from(PREVIEW_MAX_HEIGHT).unwrap();

        assert!(rows.first().expect("top row").width < outer.width);
        assert!(rows.last().expect("bottom row").width < outer.width);
        for row in &rows[usize::from(border)..image_bottom] {
            let left = i32::from(row.x);
            let right = left + i32::from(row.width);
            assert!(left <= i32::from(border));
            assert!(right >= image_right);
        }
    }
}
