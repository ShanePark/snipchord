//! Fast X11 window discovery for the Space-before-drag capture mode.
//!
//! The root image is frozen before the selection overlay appears. This module
//! resolves the current window geometry without issuing another root capture,
//! then gives the caller a rectangle to crop from that frozen image. When the
//! Composite extension is available, the selected WM frame or client pixmap
//! replaces the corresponding pixels so other windows cannot leak into the
//! result. A direct query-tree and frozen-root fallback keep the feature useful
//! on bare Xvfb and on lightweight WMs without EWMH or Composite support.

use std::collections::HashSet;

use x11rb::connection::Connection;
use x11rb::protocol::composite::{ConnectionExt as CompositeConnectionExt, Redirect};
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, MapState, Window, WindowClass};

use crate::geometry::Rect;
use crate::image::{capture_drawable, RgbaImage};
use crate::x11::X11Result;

const PROPERTY_CHUNK_LONGS: u32 = 4096;

/// A root-coordinate pointer position.
pub type RootPoint = (i32, i32);

/// The four signed decoration margins applied around a client window.
///
/// Positive values expand the client rectangle, as in _NET_FRAME_EXTENTS.
/// Negative values inset it, which is how a GTK client-side shadow is removed
/// from the visible opaque bounds. The values are pixels in left, right, top,
/// bottom order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameExtents {
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}

impl FrameExtents {
    const ZERO: Self = Self {
        left: 0,
        right: 0,
        top: 0,
        bottom: 0,
    };

    fn from_values(values: &[u32]) -> Option<Self> {
        if values.len() < 4 || values[..4].iter().any(|value| *value > i32::MAX as u32) {
            return None;
        }
        Some(Self {
            left: values[0] as i32,
            right: values[1] as i32,
            top: values[2] as i32,
            bottom: values[3] as i32,
        })
    }

    fn outer_rect(self, client: Rect) -> Rect {
        let x = i64::from(client.x) - i64::from(self.left);
        let y = i64::from(client.y) - i64::from(self.top);
        let width = i64::from(client.width) + i64::from(self.left) + i64::from(self.right);
        let height = i64::from(client.height) + i64::from(self.top) + i64::from(self.bottom);
        Rect {
            x: clamp_i32(x),
            y: clamp_i32(y),
            width: clamp_u32(width.max(0)),
            height: clamp_u32(height.max(0)),
        }
    }

    fn subtract(self, other: Self) -> Self {
        Self {
            left: self.left - other.left,
            right: self.right - other.right,
            top: self.top - other.top,
            bottom: self.bottom - other.bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FrameProperties {
    /// Window-manager frame margins. These are added to client geometry.
    net: Option<FrameExtents>,
    /// GTK client-side shadow margins. These are removed from the opaque
    /// bounds after the WM frame has been applied.
    gtk: Option<FrameExtents>,
}

impl FrameProperties {
    fn effective(self, fallback: FrameExtents) -> FrameExtents {
        self.net
            .unwrap_or(fallback)
            .subtract(self.gtk.unwrap_or(FrameExtents::ZERO))
    }
}

/// Why a selected window's geometry was found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeometrySource {
    /// A client from _NET_CLIENT_LIST_STACKING (or its list fallback).
    Ewmh,
    /// A direct child of the root found through QueryTree.
    QueryTree,
}

/// A visible window under the pointer, with its geometry sampled at pick time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowTarget {
    pub window: Window,
    /// Root window used when revalidating the sampled client geometry before
    /// reading a Composite pixmap.
    pub root: Window,
    /// The root-relative client geometry used to place a Composite client
    /// pixmap over the frozen outer crop.
    pub client_rect: Rect,
    /// The reparenting WM frame, when the client has one. A frame pixmap
    /// includes the frame and descendants when the WM redirects it.
    pub frame_window: Option<Window>,
    /// The root-relative geometry of frame_window, when discoverable.
    pub frame_rect: Option<Rect>,
    /// The client plus decoration rectangle in root coordinates. It may extend
    /// beyond the root edges until clipped_to_root is called.
    pub rect: Rect,
    pub frame_extents: FrameExtents,
    pub source: GeometrySource,
}

impl WindowTarget {
    /// Clip the sampled window rectangle to the captured root image.
    pub fn clipped_to_root(self, root_width: u32, root_height: u32) -> Option<Rect> {
        clip_to_root(self.rect, root_width, root_height)
    }
}

/// Clip a root-coordinate rectangle to the dimensions of a frozen root image.
pub fn clip_to_root(rect: Rect, root_width: u32, root_height: u32) -> Option<Rect> {
    if root_width == 0 || root_height == 0 || rect.width == 0 || rect.height == 0 {
        return None;
    }
    let left = i64::from(rect.x).max(0);
    let top = i64::from(rect.y).max(0);
    let right = (i64::from(rect.x) + i64::from(rect.width)).min(i64::from(root_width));
    let bottom = (i64::from(rect.y) + i64::from(rect.height)).min(i64::from(root_height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect {
        x: clamp_i32(left),
        y: clamp_i32(top),
        width: clamp_u32(right - left),
        height: clamp_u32(bottom - top),
    })
}

/// Which backing store produced a selected-window image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowCaptureSource {
    /// The WM frame's redirected Composite pixmap, including descendants.
    CompositeFrame,
    /// The client's redirected pixmap over the frozen outer crop.
    CompositeClient,
    /// The visible pixels in the root image captured before the overlay.
    FrozenRoot,
}

/// The result of capturing a selected window, including the fallback path
/// used when Composite is unavailable or the window changed during capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowCapture {
    pub image: RgbaImage,
    pub source: WindowCaptureSource,
}

/// Cached atoms and protocol policy used for repeated window picks.
///
/// Construct this once when the resident app starts. Atom requests are
/// pipelined in Self::new, so the hot Space path only performs property,
/// attribute, and geometry requests.
pub struct WindowPicker {
    atoms: WindowAtoms,
    composite_available: bool,
}

#[derive(Clone, Copy)]
struct WindowAtoms {
    client_list_stacking: Atom,
    client_list: Atom,
    frame_extents: Atom,
    gtk_frame_extents: Atom,
    window_type: Atom,
    atom_type: Atom,
    cardinal_type: Atom,
    dock_type: Atom,
    desktop_type: Atom,
    panel_type: Atom,
    notification_type: Atom,
    tooltip_type: Atom,
    splash_type: Atom,
    dnd_type: Atom,
}

impl WindowPicker {
    /// Intern the EWMH atoms needed by window discovery.
    pub fn new<C: Connection>(conn: &C) -> X11Result<Self> {
        // Queue every request before waiting for any reply. This keeps the
        // one-time setup from becoming a series of avoidable round trips.
        let cookies = [
            b"_NET_CLIENT_LIST_STACKING".as_slice(),
            b"_NET_CLIENT_LIST".as_slice(),
            b"_NET_FRAME_EXTENTS".as_slice(),
            b"_GTK_FRAME_EXTENTS".as_slice(),
            b"_NET_WM_WINDOW_TYPE".as_slice(),
            b"_NET_WM_WINDOW_TYPE_DOCK".as_slice(),
            b"_NET_WM_WINDOW_TYPE_DESKTOP".as_slice(),
            b"_NET_WM_WINDOW_TYPE_PANEL".as_slice(),
            b"_NET_WM_WINDOW_TYPE_NOTIFICATION".as_slice(),
            b"_NET_WM_WINDOW_TYPE_TOOLTIP".as_slice(),
            b"_NET_WM_WINDOW_TYPE_SPLASH".as_slice(),
            b"_NET_WM_WINDOW_TYPE_DND".as_slice(),
        ]
        .into_iter()
        .map(|name| conn.intern_atom(false, name))
        .collect::<Result<Vec<_>, _>>()?;
        let atoms = cookies
            .into_iter()
            .map(|cookie| Ok(cookie.reply()?.atom))
            .collect::<X11Result<Vec<_>>>()?;
        let [client_list_stacking, client_list, frame_extents, gtk_frame_extents, window_type, dock_type, desktop_type, panel_type, notification_type, tooltip_type, splash_type, dnd_type] =
            atoms
                .try_into()
                .map_err(|_| "X11 atom setup returned the wrong number of atoms")?;
        let composite_available = conn
            .composite_query_version(0, 4)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.major_version > 0 || reply.minor_version >= 2);

        Ok(Self {
            atoms: WindowAtoms {
                client_list_stacking,
                client_list,
                frame_extents,
                gtk_frame_extents,
                window_type,
                atom_type: AtomEnum::ATOM.into(),
                cardinal_type: AtomEnum::CARDINAL.into(),
                dock_type,
                desktop_type,
                panel_type,
                notification_type,
                tooltip_type,
                splash_type,
                dnd_type,
            },
            composite_available,
        })
    }

    /// Find the topmost visible client window containing pointer.
    ///
    /// The returned rectangle is sampled while the selection overlay is
    /// active and is intended to crop the root image captured just before the
    /// overlay was shown. A window moved or resized in between can therefore
    /// make the geometry and frozen pixels differ slightly. Pass the overlay,
    /// preview, and any other SnipChord windows in excluded so a resident UI
    /// surface cannot win the hit test.
    ///
    /// The crop contains the pixels present in the frozen root image. X11
    /// root capture cannot reconstruct parts of a window that were occluded
    /// by another window at capture time.
    pub fn pick_at_pointer<C: Connection>(
        &self,
        conn: &C,
        root: Window,
        pointer: RootPoint,
        excluded: &[Window],
    ) -> X11Result<Option<WindowTarget>> {
        let excluded: HashSet<Window> = excluded.iter().copied().collect();

        // _NET_CLIENT_LIST_STACKING is bottom-to-top, so reverse it before
        // testing. A few WMs only publish _NET_CLIENT_LIST; try it before
        // falling back to the core tree.
        for property in [self.atoms.client_list_stacking, self.atoms.client_list] {
            let Some(windows) = read_window_list(conn, root, property, AtomEnum::WINDOW.into())?
            else {
                continue;
            };
            for window in windows.into_iter().rev() {
                if let Some(target) =
                    self.candidate(conn, root, window, pointer, &excluded, GeometrySource::Ewmh)
                {
                    return Ok(Some(target));
                }
            }
        }

        // Bare X servers and small WMs often omit all EWMH lists. Root
        // children are also bottom-to-top in QueryTree, so reverse those.
        let tree = conn.query_tree(root)?.reply()?;
        for window in tree.children.into_iter().rev() {
            if let Some(target) = self.candidate(
                conn,
                root,
                window,
                pointer,
                &excluded,
                GeometrySource::QueryTree,
            ) {
                return Ok(Some(target));
            }
        }
        Ok(None)
    }

    fn candidate<C: Connection>(
        &self,
        conn: &C,
        root: Window,
        window: Window,
        pointer: RootPoint,
        excluded: &HashSet<Window>,
        source: GeometrySource,
    ) -> Option<WindowTarget> {
        if window == 0 || window == root || excluded.contains(&window) {
            return None;
        }

        // Stale EWMH entries are normal during a close animation. Ignore a
        // failed candidate and continue down the stack rather than making a
        // hotkey invocation fail because one window disappeared mid-pick.
        let attributes = conn.get_window_attributes(window).ok()?.reply().ok()?;
        if attributes.map_state != MapState::VIEWABLE
            || attributes.class == WindowClass::INPUT_ONLY
            || self.is_panel_window(conn, window)
        {
            return None;
        }

        let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
        if geometry.width == 0 || geometry.height == 0 {
            return None;
        }
        let translated = conn
            .translate_coordinates(window, root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        if !translated.same_screen {
            return None;
        }

        let client = Rect {
            x: i32::from(translated.dst_x),
            y: i32::from(translated.dst_y),
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        };
        let fallback_border = i32::from(geometry.border_width);
        let decorations = self.decorations(conn, window).unwrap_or_default();
        let frame_extents = decorations.effective(FrameExtents {
            left: fallback_border,
            right: fallback_border,
            top: fallback_border,
            bottom: fallback_border,
        });
        let rect = frame_extents.outer_rect(client);
        if contains(rect, pointer) {
            let (frame_window, frame_rect) = top_level_frame(conn, root, window)
                .map(|(frame_window, frame_rect)| (Some(frame_window), Some(frame_rect)))
                .unwrap_or((None, None));
            Some(WindowTarget {
                window,
                root,
                client_rect: client,
                frame_window,
                frame_rect,
                rect,
                frame_extents,
                source,
            })
        } else {
            None
        }
    }

    fn decorations<C: Connection>(&self, conn: &C, window: Window) -> X11Result<FrameProperties> {
        let net = read_property32(
            conn,
            window,
            self.atoms.frame_extents,
            self.atoms.cardinal_type,
        )?
        .as_deref()
        .and_then(FrameExtents::from_values);
        let gtk = read_property32(
            conn,
            window,
            self.atoms.gtk_frame_extents,
            self.atoms.cardinal_type,
        )?
        .as_deref()
        .and_then(FrameExtents::from_values);
        Ok(FrameProperties { net, gtk })
    }

    fn is_panel_window<C: Connection>(&self, conn: &C, window: Window) -> bool {
        let Ok(Some(types)) =
            read_property32(conn, window, self.atoms.window_type, self.atoms.atom_type)
        else {
            return false;
        };
        [
            self.atoms.dock_type,
            self.atoms.desktop_type,
            self.atoms.panel_type,
            self.atoms.notification_type,
            self.atoms.tooltip_type,
            self.atoms.splash_type,
            self.atoms.dnd_type,
        ]
        .into_iter()
        .any(|ignored| types.contains(&ignored))
    }

    /// Capture one selected window without exposing the desktop overlay.
    ///
    /// Composite is attempted only after the pointer is released. A WM frame
    /// pixmap is preferred because Composite storage includes its descendants;
    /// if the frame is unavailable, the client pixmap is copied over the
    /// corresponding area of the pre-overlay root crop. Every Composite,
    /// geometry, visual, and resize mismatch falls back to that root crop.
    pub fn capture_target<C: Connection>(
        &self,
        conn: &C,
        target: WindowTarget,
        frozen_root: &RgbaImage,
        fallback_visual: u32,
    ) -> X11Result<WindowCapture> {
        let rect = target
            .clipped_to_root(frozen_root.width(), frozen_root.height())
            .ok_or_else(|| invalid("selected window is outside the captured root"))?;
        let crop = frozen_root
            .crop(rect.x, rect.y, rect.width, rect.height)
            .ok_or_else(|| invalid("selected window crop is empty"))?;
        self.capture_target_from_frozen_crop(conn, target, &crop, (rect.x, rect.y), fallback_visual)
    }

    /// Capture a target when the caller already read only its frozen-root crop.
    ///
    /// `frozen_crop` must be the root snapshot rectangle whose top-left root
    /// coordinate is `frozen_origin`. This is the native selection path's
    /// bridge to a server-side snapshot: it avoids downloading the complete
    /// desktop while preserving the same Composite and visible-pixel
    /// fallbacks as [`Self::capture_target`].
    pub fn capture_target_from_frozen_crop<C: Connection>(
        &self,
        conn: &C,
        target: WindowTarget,
        frozen_crop: &RgbaImage,
        frozen_origin: RootPoint,
        fallback_visual: u32,
    ) -> X11Result<WindowCapture> {
        let fallback = || {
            Ok(WindowCapture {
                image: frozen_crop.clone(),
                source: WindowCaptureSource::FrozenRoot,
            })
        };

        if !self.composite_available {
            return fallback();
        }

        // The root crop is already clipped to the screen. Keep the Composite
        // frame result at the same bounds, including windows that touch or
        // extend beyond a root edge, so frame and fallback paths have the
        // same output geometry.
        let frozen_rect = Rect {
            x: frozen_origin.0,
            y: frozen_origin.1,
            width: frozen_crop.width(),
            height: frozen_crop.height(),
        };
        let clipped_target = intersect_rect(target.rect, frozen_rect);
        let Some(clipped_target) = clipped_target else {
            return fallback();
        };

        // The target was sampled while the pointer moved over the live
        // desktop. Revalidate the client root position and size immediately
        // before naming its pixmap; otherwise a move with unchanged
        // dimensions would paste current pixels at stale coordinates.
        if current_client_rect(conn, target.root, target.window) != Some(target.client_rect) {
            return fallback();
        }

        if let Some(frame_window) = target.frame_window {
            if let Some(image) = named_window_pixmap(conn, frame_window, fallback_visual) {
                if let Some(frame_rect) = target.frame_rect {
                    let dimensions_match =
                        frame_rect.width == image.width() && frame_rect.height == image.height();
                    let frame_geometry_matches =
                        current_frame_rect(conn, target.root, frame_window) == Some(frame_rect);
                    let client_geometry_matches =
                        current_client_rect(conn, target.root, target.window)
                            == Some(target.client_rect);
                    if dimensions_match && frame_geometry_matches && client_geometry_matches {
                        if let Some(image) = crop_frame_image(&image, frame_rect, clipped_target) {
                            if image.width() == clipped_target.width
                                && image.height() == clipped_target.height
                            {
                                return Ok(WindowCapture {
                                    image,
                                    source: WindowCaptureSource::CompositeFrame,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Revalidate again after the frame attempt and before using the client
        // pixmap. The server may have processed a configure request between
        // the two reads; any mismatch is safer as the frozen visible crop.
        if current_client_rect(conn, target.root, target.window) != Some(target.client_rect) {
            return fallback();
        }
        let Some(client_image) = named_window_pixmap(conn, target.window, fallback_visual) else {
            return fallback();
        };
        if client_image.width() != target.client_rect.width
            || client_image.height() != target.client_rect.height
        {
            return fallback();
        }
        if current_client_rect(conn, target.root, target.window) != Some(target.client_rect) {
            return fallback();
        }

        let mut image = frozen_crop.clone();
        if !overlay_client_image(
            &mut image,
            frozen_origin,
            &client_image,
            (target.client_rect.x, target.client_rect.y),
        ) {
            return fallback();
        }
        Ok(WindowCapture {
            image,
            source: WindowCaptureSource::CompositeClient,
        })
    }
}

fn read_window_list<C: Connection>(
    conn: &C,
    root: Window,
    property: Atom,
    window_type: Atom,
) -> X11Result<Option<Vec<Window>>> {
    Ok(read_property32(conn, root, property, window_type)?
        .map(|values| values.into_iter().filter(|window| *window != 0).collect()))
}

fn named_window_pixmap<C: Connection>(
    conn: &C,
    window: Window,
    fallback_visual: u32,
) -> Option<RgbaImage> {
    let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
    if geometry.width == 0 || geometry.height == 0 {
        return None;
    }
    // `from_x11` intentionally treats every visual as opaque RGBA. A depth32
    // Composite pixmap may instead be premultiplied ARGB, where forcing alpha
    // to 255 turns transparent shadow pixels into black fringes. Keep the
    // bounded path safe by using the frozen visible crop for every depth32
    // window until alpha-aware decoding/compositing exists.
    if !composite_depth_supported(geometry.depth) {
        return None;
    }
    // GetImage reports visual zero for many pixmaps. Prefer the selected
    // window's visual for that case so ARGB client windows are decoded with
    // their own channel masks instead of the root visual's masks.
    let capture_visual = conn
        .get_window_attributes(window)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|attributes| attributes.visual)
        .filter(|visual| *visual != 0)
        .unwrap_or(fallback_visual);
    let read_named = || {
        let pixmap = conn.generate_id().ok()?;
        let name_cookie = conn.composite_name_window_pixmap(window, pixmap).ok()?;
        if name_cookie.check().is_err() {
            let _ = conn.free_pixmap(pixmap);
            return None;
        }
        let image = capture_drawable(
            conn,
            pixmap,
            0,
            0,
            geometry.width,
            geometry.height,
            capture_visual,
        )
        .ok();
        if let Ok(cookie) = conn.free_pixmap(pixmap) {
            let _ = cookie.check();
        }
        image
    };

    // A compositor normally redirects client windows already. Bare Xvfb and
    // lightweight WMs often do not, in which case NameWindowPixmap has no
    // backing store to name. Temporarily redirect this one window and retry;
    // failure (including an existing manual redirect owned by another client)
    // keeps the frozen-root fallback intact.
    if let Some(image) = read_named() {
        return Some(image);
    }
    let redirected = conn
        .composite_redirect_window(window, Redirect::MANUAL)
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .is_some();
    if !redirected {
        return None;
    }
    let image = read_named();
    if let Ok(cookie) = conn.composite_unredirect_window(window, Redirect::MANUAL) {
        let _ = cookie.check();
    }
    image
}

fn composite_depth_supported(depth: u8) -> bool {
    depth != 32
}

fn root_window_geometry<C: Connection>(
    conn: &C,
    root: Window,
    window: Window,
) -> Option<(Rect, i32)> {
    let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
    if geometry.width == 0 || geometry.height == 0 {
        return None;
    }
    let translated = conn
        .translate_coordinates(window, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    if !translated.same_screen {
        return None;
    }
    Some((
        Rect {
            x: i32::from(translated.dst_x),
            y: i32::from(translated.dst_y),
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        },
        i32::from(geometry.border_width),
    ))
}

fn current_client_rect<C: Connection>(conn: &C, root: Window, window: Window) -> Option<Rect> {
    root_window_geometry(conn, root, window).map(|(rect, _)| rect)
}

fn current_frame_rect<C: Connection>(conn: &C, root: Window, window: Window) -> Option<Rect> {
    root_window_geometry(conn, root, window).map(|(rect, border)| {
        FrameExtents {
            left: border,
            right: border,
            top: border,
            bottom: border,
        }
        .outer_rect(rect)
    })
}

/// Crop a frame pixmap into the target's root-coordinate rectangle.
///
/// The pixmap origin is the sampled frame origin. Intersecting first handles
/// GTK opaque insets and a partially visible frame without asking
/// `RgbaImage::crop` to silently clamp a mismatched rectangle.
fn crop_frame_image(image: &RgbaImage, frame_rect: Rect, target_rect: Rect) -> Option<RgbaImage> {
    if image.width() != frame_rect.width || image.height() != frame_rect.height {
        return None;
    }
    let left = i64::from(frame_rect.x).max(i64::from(target_rect.x));
    let top = i64::from(frame_rect.y).max(i64::from(target_rect.y));
    let right = (i64::from(frame_rect.x) + i64::from(frame_rect.width))
        .min(i64::from(target_rect.x) + i64::from(target_rect.width));
    let bottom = (i64::from(frame_rect.y) + i64::from(frame_rect.height))
        .min(i64::from(target_rect.y) + i64::from(target_rect.height));
    if right <= left || bottom <= top {
        return None;
    }
    let source_x = i32::try_from(left - i64::from(frame_rect.x)).ok()?;
    let source_y = i32::try_from(top - i64::from(frame_rect.y)).ok()?;
    let width = u32::try_from(right - left).ok()?;
    let height = u32::try_from(bottom - top).ok()?;
    image.crop(source_x, source_y, width, height)
}

fn intersect_rect(first: Rect, second: Rect) -> Option<Rect> {
    let left = i64::from(first.x).max(i64::from(second.x));
    let top = i64::from(first.y).max(i64::from(second.y));
    let right = (i64::from(first.x) + i64::from(first.width))
        .min(i64::from(second.x) + i64::from(second.width));
    let bottom = (i64::from(first.y) + i64::from(first.height))
        .min(i64::from(second.y) + i64::from(second.height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect {
        x: clamp_i32(left),
        y: clamp_i32(top),
        width: clamp_u32(right - left),
        height: clamp_u32(bottom - top),
    })
}

fn top_level_frame<C: Connection>(
    conn: &C,
    root: Window,
    window: Window,
) -> Option<(Window, Rect)> {
    let mut current = window;
    for _ in 0..64 {
        let tree = conn.query_tree(current).ok()?.reply().ok()?;
        if tree.parent == 0 || tree.parent == root {
            return window_root_rect(conn, root, current).map(|rect| (current, rect));
        }
        current = tree.parent;
    }
    None
}

fn window_root_rect<C: Connection>(conn: &C, root: Window, window: Window) -> Option<Rect> {
    current_frame_rect(conn, root, window)
}

fn overlay_client_image(
    outer: &mut RgbaImage,
    outer_origin: RootPoint,
    client: &RgbaImage,
    client_origin: RootPoint,
) -> bool {
    let outer_left = i64::from(outer_origin.0);
    let outer_top = i64::from(outer_origin.1);
    let outer_right = outer_left + i64::from(outer.width());
    let outer_bottom = outer_top + i64::from(outer.height());
    let client_left = i64::from(client_origin.0);
    let client_top = i64::from(client_origin.1);
    let client_right = client_left + i64::from(client.width());
    let client_bottom = client_top + i64::from(client.height());
    let left = outer_left.max(client_left);
    let top = outer_top.max(client_top);
    let right = outer_right.min(client_right);
    let bottom = outer_bottom.min(client_bottom);
    if right <= left || bottom <= top {
        return false;
    }

    let Some(copy_width) = usize::try_from(right - left).ok() else {
        return false;
    };
    let Some(copy_height) = usize::try_from(bottom - top).ok() else {
        return false;
    };
    let Some(outer_x) = usize::try_from(left - outer_left).ok() else {
        return false;
    };
    let Some(outer_y) = usize::try_from(top - outer_top).ok() else {
        return false;
    };
    let Some(client_x) = usize::try_from(left - client_left).ok() else {
        return false;
    };
    let Some(client_y) = usize::try_from(top - client_top).ok() else {
        return false;
    };
    let Some(outer_width) = usize::try_from(outer.width()).ok() else {
        return false;
    };
    let Some(client_width) = usize::try_from(client.width()).ok() else {
        return false;
    };
    let Some(row_bytes) = copy_width.checked_mul(4) else {
        return false;
    };
    let source = client.pixels();
    let destination = outer.pixels_mut();
    for row in 0..copy_height {
        let Some(source_offset) = (client_y + row)
            .checked_mul(client_width)
            .and_then(|offset| offset.checked_add(client_x))
            .and_then(|offset| offset.checked_mul(4))
        else {
            return false;
        };
        let Some(destination_offset) = (outer_y + row)
            .checked_mul(outer_width)
            .and_then(|offset| offset.checked_add(outer_x))
            .and_then(|offset| offset.checked_mul(4))
        else {
            return false;
        };
        destination[destination_offset..destination_offset + row_bytes]
            .copy_from_slice(&source[source_offset..source_offset + row_bytes]);
    }
    true
}

fn invalid(message: &'static str) -> Box<dyn std::error::Error + Send + Sync> {
    message.into()
}

/// Read a format-32 property in bounded chunks. GetProperty offsets are
/// measured in 32-bit units, so the next request advances by value_len for
/// the format we accept.
fn read_property32<C: Connection>(
    conn: &C,
    window: Window,
    property: Atom,
    expected_type: Atom,
) -> X11Result<Option<Vec<u32>>> {
    let mut offset = 0u32;
    let mut values = Vec::new();
    loop {
        let reply = conn
            .get_property(
                false,
                window,
                property,
                expected_type,
                offset,
                PROPERTY_CHUNK_LONGS,
            )?
            .reply()?;
        if reply.type_ == u32::from(AtomEnum::NONE) || reply.format == 0 {
            return Ok(None);
        }
        if reply.format != 32 || reply.type_ != expected_type {
            return Ok(None);
        }
        let chunk: Vec<u32> = reply.value32().into_iter().flatten().collect();
        let consumed = reply.value_len;
        values.extend(chunk);
        if reply.bytes_after == 0 || consumed == 0 {
            return Ok(Some(values));
        }
        let next = offset.saturating_add(consumed);
        if next == offset {
            return Ok(Some(values));
        }
        offset = next;
    }
}

fn contains(rect: Rect, point: RootPoint) -> bool {
    point.0 >= rect.x
        && point.1 >= rect.y
        && i64::from(point.0) < i64::from(rect.x) + i64::from(rect.width)
        && i64::from(point.1) < i64::from(rect.y) + i64::from(rect.height)
}

fn clamp_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn clamp_u32(value: i64) -> u32 {
    value.clamp(0, i64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::{
        clip_to_root, contains, overlay_client_image, FrameExtents, FrameProperties,
        GeometrySource, WindowTarget,
    };
    use crate::geometry::Rect;
    use crate::image::RgbaImage;

    #[test]
    fn frame_extents_expand_client_to_outer_window() {
        let client = Rect {
            x: 100,
            y: 80,
            width: 400,
            height: 300,
        };
        assert_eq!(
            FrameExtents {
                left: 8,
                right: 9,
                top: 32,
                bottom: 10,
            }
            .outer_rect(client),
            Rect {
                x: 92,
                y: 48,
                width: 417,
                height: 342,
            }
        );
    }

    #[test]
    fn gtk_shadow_is_inset_after_wm_frame_is_applied() {
        let effective = FrameProperties {
            net: Some(FrameExtents {
                left: 8,
                right: 9,
                top: 32,
                bottom: 10,
            }),
            gtk: Some(FrameExtents {
                left: 3,
                right: 4,
                top: 5,
                bottom: 6,
            }),
        }
        .effective(FrameExtents::ZERO);

        assert_eq!(
            effective.outer_rect(Rect {
                x: 100,
                y: 80,
                width: 400,
                height: 300,
            }),
            Rect {
                x: 95,
                y: 53,
                width: 410,
                height: 331,
            }
        );
    }

    #[test]
    fn gtk_shadow_without_wm_frame_insets_client_bounds() {
        let effective = FrameProperties {
            net: None,
            gtk: Some(FrameExtents {
                left: 4,
                right: 6,
                top: 8,
                bottom: 10,
            }),
        }
        .effective(FrameExtents::ZERO);

        assert_eq!(
            effective.outer_rect(Rect {
                x: 100,
                y: 80,
                width: 400,
                height: 300,
            }),
            Rect {
                x: 104,
                y: 88,
                width: 390,
                height: 282,
            }
        );
    }

    #[test]
    fn outer_window_is_clipped_to_frozen_root() {
        let target = WindowTarget {
            window: 7,
            root: 1,
            client_rect: Rect {
                x: -20,
                y: -10,
                width: 120,
                height: 90,
            },
            frame_window: None,
            frame_rect: None,
            rect: Rect {
                x: -20,
                y: -10,
                width: 120,
                height: 90,
            },
            frame_extents: FrameExtents::ZERO,
            source: GeometrySource::Ewmh,
        };
        assert_eq!(
            target.clipped_to_root(80, 60),
            Some(Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 60,
            })
        );
    }

    #[test]
    fn rectangles_touching_right_or_bottom_edge_do_not_hit() {
        let rect = Rect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        };
        assert!(contains(rect, (10, 10)));
        assert!(contains(rect, (29, 29)));
        assert!(!contains(rect, (30, 29)));
        assert!(!contains(rect, (29, 30)));
    }

    #[test]
    fn fully_offscreen_windows_have_no_capture_rect() {
        assert_eq!(
            clip_to_root(
                Rect {
                    x: 100,
                    y: 100,
                    width: 20,
                    height: 20,
                },
                80,
                80,
            ),
            None
        );
    }

    #[test]
    fn client_pixmap_overlay_replaces_only_the_visible_intersection() {
        let mut outer = RgbaImage::solid_rgba(4, 3, [1, 2, 3, 4]).unwrap();
        let client = RgbaImage::solid_rgba(3, 2, [9, 8, 7, 6]).unwrap();

        assert!(overlay_client_image(
            &mut outer,
            (10, 20),
            &client,
            (11, 20)
        ));

        let expected = [
            [1, 2, 3, 4],
            [9, 8, 7, 6],
            [9, 8, 7, 6],
            [9, 8, 7, 6],
            [1, 2, 3, 4],
            [9, 8, 7, 6],
            [9, 8, 7, 6],
            [9, 8, 7, 6],
            [1, 2, 3, 4],
            [1, 2, 3, 4],
            [1, 2, 3, 4],
            [1, 2, 3, 4],
        ];
        assert_eq!(outer.pixels(), expected.concat());
    }

    #[test]
    fn client_pixmap_overlay_rejects_non_overlapping_images() {
        let mut outer = RgbaImage::solid_rgba(2, 2, [1, 2, 3, 4]).unwrap();
        let client = RgbaImage::solid_rgba(2, 2, [9, 8, 7, 6]).unwrap();

        assert!(!overlay_client_image(&mut outer, (0, 0), &client, (3, 3)));
        assert_eq!(
            outer.pixels(),
            &[1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4]
        );
    }

    #[test]
    fn frame_pixmap_crop_uses_root_offset_and_target_insets() {
        let mut pixels = Vec::new();
        for y in 0..4u8 {
            for x in 0..6u8 {
                pixels.extend_from_slice(&[x, y, 0, 255]);
            }
        }
        let frame = RgbaImage::from_rgba(6, 4, pixels).unwrap();
        let cropped = super::crop_frame_image(
            &frame,
            Rect {
                x: 100,
                y: 50,
                width: 6,
                height: 4,
            },
            Rect {
                x: 102,
                y: 51,
                width: 3,
                height: 2,
            },
        )
        .unwrap();

        assert_eq!(cropped.width(), 3);
        assert_eq!(cropped.height(), 2);
        assert_eq!(
            cropped.pixels(),
            &[2, 1, 0, 255, 3, 1, 0, 255, 4, 1, 0, 255, 2, 2, 0, 255, 3, 2, 0, 255, 4, 2, 0, 255,]
        );
    }

    #[test]
    fn frame_pixmap_crop_rejects_non_overlapping_target() {
        let frame = RgbaImage::solid_rgba(4, 4, [1, 2, 3, 255]).unwrap();
        assert_eq!(
            super::crop_frame_image(
                &frame,
                Rect {
                    x: 100,
                    y: 100,
                    width: 4,
                    height: 4,
                },
                Rect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
            ),
            None
        );
    }

    #[test]
    fn frame_pixmap_crop_uses_the_clipped_root_target() {
        let frame = RgbaImage::solid_rgba(10, 10, [1, 2, 3, 255]).unwrap();
        let target = Rect {
            x: -2,
            y: -2,
            width: 10,
            height: 10,
        };
        let frozen_bounds = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        let clipped = super::intersect_rect(target, frozen_bounds).unwrap();
        let cropped = super::crop_frame_image(
            &frame,
            Rect {
                x: -2,
                y: -2,
                width: 10,
                height: 10,
            },
            clipped,
        )
        .unwrap();
        assert_eq!((cropped.width(), cropped.height()), (8, 8));
    }

    #[test]
    fn depth32_composite_pixmaps_use_frozen_fallback() {
        assert!(super::composite_depth_supported(24));
        assert!(!super::composite_depth_supported(32));
    }
}
