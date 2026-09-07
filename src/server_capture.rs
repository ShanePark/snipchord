//! Server-side frozen root capture used by the selection overlay.
//!
//! The usual root capture path first transfers the complete desktop to the
//! client with `GetImage`, converts every pixel in Rust, and then uploads the
//! same image again for the overlay. On a large desktop that makes the first
//! frame wait for two full-screen transfers. [`ServerCapture`] keeps the
//! frozen desktop in one X11 pixmap instead: `CopyArea` takes the snapshot
//! while the screen is still visible and the overlay presents that pixmap
//! directly. The client reads only the selected rectangle after the user
//! commits it.

use std::error::Error;

use x11rb::connection::Connection;
use x11rb::image::Image as X11Image;
use x11rb::protocol::xproto::{
    self, ConnectionExt as XProtoConnectionExt, CreateGCAux, Pixmap, SubwindowMode,
};

use crate::image::{from_x11, ImageError, ImageResult, RgbaImage};
use crate::x11::{X11Context, X11Result};

/// A server-side frozen root image.
///
/// The pixmap remains owned by this value until [`destroy`](Self::destroy) is
/// called. X11 resources cannot be freed from a Rust `Drop` implementation
/// because the live connection is borrowed from the application, so callers
/// must explicitly destroy the value on every path that leaves selection
/// mode.
pub struct ServerCapture {
    snapshot: Pixmap,
    width: u16,
    height: u16,
    root_visual: xproto::Visualid,
}

impl ServerCapture {
    /// Freeze the current root window into a server pixmap without reading any
    /// pixels back to the client. The synchronous `CopyArea` check establishes
    /// the ordering boundary before the overlay is mapped.
    pub fn begin(context: &mut X11Context) -> X11Result<Option<Self>> {
        let width = context.width();
        let height = context.height();
        if width == 0 || height == 0 {
            return Ok(None);
        }

        let snapshot = context.create_pixmap(width, height)?;
        let gc = match context.create_gc(
            snapshot,
            &CreateGCAux::new()
                .subwindow_mode(SubwindowMode::INCLUDE_INFERIORS)
                .graphics_exposures(0u32),
        ) {
            Ok(gc) => gc,
            Err(error) => {
                free_pixmap(context, snapshot);
                return Err(error);
            }
        };

        // CopyArea is ordered with the later map request on the same X11
        // connection. The server therefore freezes the visible root before
        // the overlay can cover it, without a client-side full-screen readback
        // or a second full-screen presentation pixmap.
        let copy_cookie =
            match context
                .conn
                .copy_area(context.root(), snapshot, gc, 0, 0, 0, 0, width, height)
            {
                Ok(cookie) => cookie,
                Err(error) => {
                    free_gc(context, gc);
                    free_pixmap(context, snapshot);
                    return Err(error.into());
                }
            };
        let copy_result = copy_cookie.check();
        free_gc(context, gc);
        if let Err(error) = copy_result {
            free_pixmap(context, snapshot);
            return Err(error.into());
        }
        if let Err(error) = context.conn.flush() {
            free_pixmap(context, snapshot);
            return Err(error.into());
        }

        Ok(Some(Self {
            snapshot,
            width,
            height,
            root_visual: context.visual(),
        }))
    }

    /// Pixmap containing the frozen root image.
    pub fn snapshot(&self) -> Pixmap {
        self.snapshot
    }

    /// Dimensions of the server pixmap in root pixels.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Read only the committed rectangle from the frozen server snapshot.
    ///
    /// This is the only method in this module that performs `GetImage`; the
    /// full root was never transferred to the client during overlay startup.
    pub fn read_region(
        &self,
        context: &X11Context,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> ImageResult<RgbaImage> {
        let Some((x, y, width, height)) = clamp_region(
            x,
            y,
            width,
            height,
            u32::from(self.width),
            u32::from(self.height),
        ) else {
            return Err(Box::new(ImageError::InvalidDimensions));
        };
        let raw_width = u16::try_from(width).map_err(|_| ImageError::InvalidDimensions)?;
        let raw_height = u16::try_from(height).map_err(|_| ImageError::InvalidDimensions)?;
        let raw_x = i16::try_from(x).map_err(|_| ImageError::InvalidDimensions)?;
        let raw_y = i16::try_from(y).map_err(|_| ImageError::InvalidDimensions)?;
        let (raw, visual_id) = X11Image::get(
            &context.conn,
            self.snapshot,
            raw_x,
            raw_y,
            raw_width,
            raw_height,
        )
        .map_err(|error| Box::new(ImageError::X11(error)) as Box<dyn Error + Send + Sync>)?;
        let visual = find_visual(context, visual_id)
            .or_else(|| find_visual(context, self.root_visual))
            .ok_or_else(|| {
                Box::new(ImageError::MissingVisual(visual_id)) as Box<dyn Error + Send + Sync>
            })?;
        from_x11(&raw, visual)
    }

    /// Release the server pixmap and continue cleanup after a request error.
    pub fn destroy(self, context: &X11Context) -> X11Result<()> {
        let result = free_pixmap_checked(context, self.snapshot);
        context.conn.flush()?;
        result
    }
}

fn find_visual(context: &X11Context, visual_id: xproto::Visualid) -> Option<xproto::Visualtype> {
    context
        .screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == visual_id)
        .copied()
}

fn clamp_region(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    root_width: u32,
    root_height: u32,
) -> Option<(i32, i32, u32, u32)> {
    if width == 0 || height == 0 || root_width == 0 || root_height == 0 {
        return None;
    }
    let left = i64::from(x).max(0).min(i64::from(root_width));
    let top = i64::from(y).max(0).min(i64::from(root_height));
    let right = i64::from(x)
        .saturating_add(i64::from(width))
        .max(left)
        .min(i64::from(root_width));
    let bottom = i64::from(y)
        .saturating_add(i64::from(height))
        .max(top)
        .min(i64::from(root_height));
    if right <= left || bottom <= top {
        return None;
    }
    Some((
        i32::try_from(left).ok()?,
        i32::try_from(top).ok()?,
        u32::try_from(right - left).ok()?,
        u32::try_from(bottom - top).ok()?,
    ))
}

fn free_gc(context: &X11Context, gc: xproto::Gcontext) {
    let _ = context.conn.free_gc(gc);
}

fn free_pixmap(context: &X11Context, pixmap: Pixmap) {
    let _ = context.conn.free_pixmap(pixmap);
}

fn free_pixmap_checked(context: &X11Context, pixmap: Pixmap) -> X11Result<()> {
    Ok(context.conn.free_pixmap(pixmap)?.check()?)
}

#[cfg(test)]
mod tests {
    use super::clamp_region;

    #[test]
    fn clamp_region_keeps_selection_inside_snapshot() {
        assert_eq!(
            clamp_region(-20, 10, 80, 100, 100, 100),
            Some((0, 10, 60, 90))
        );
        assert_eq!(clamp_region(100, 100, 1, 1, 100, 100), None);
    }

    #[test]
    fn clamp_region_rejects_empty_dimensions() {
        assert_eq!(clamp_region(0, 0, 0, 10, 100, 100), None);
        assert_eq!(clamp_region(0, 0, 10, 0, 100, 100), None);
    }
}
