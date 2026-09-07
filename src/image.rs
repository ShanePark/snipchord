//! Pixel storage, X11 capture conversion, and clipboard image encoders.
//!
//! The X11 server is allowed to choose its byte order, scanline padding, and
//! channel masks.  [`x11rb::image::Image`] exposes those details, so this
//! module converts through `PixelLayout` instead of assuming the usual
//! little-endian BGRX layout.  The rest of the application only sees a small,
//! owned, row-major RGBA image.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use x11rb::connection::Connection;
use x11rb::errors::{ParseError, ReplyError};
use x11rb::image::{Image as X11Image, PixelLayout};
use x11rb::protocol::xproto::{Setup, VisualClass, Visualid, Visualtype};

/// Fallible result used by the image and capture helpers.
pub type ImageResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Errors raised before an image can be encoded or displayed.
#[derive(Debug)]
pub enum ImageError {
    /// A zero-sized image or an allocation whose byte length overflowed.
    InvalidDimensions,
    /// The X11 visual returned by `GetImage` was not present in the setup.
    MissingVisual(Visualid),
    /// X11 images from indexed or grayscale visuals need a colormap and are
    /// intentionally outside this capture path.
    UnsupportedVisualClass(VisualClass),
    /// The visual masks do not describe a supported TrueColor image.
    InvalidPixelLayout(ParseError),
    /// An X11 request failed.
    X11(ReplyError),
    /// PNG encoding failed.
    Png(png::EncodingError),
}

impl fmt::Display for ImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => formatter.write_str("invalid image dimensions"),
            Self::MissingVisual(visual) => {
                write!(
                    formatter,
                    "X11 visual {visual} is missing from the server setup"
                )
            }
            Self::UnsupportedVisualClass(class) => {
                write!(formatter, "unsupported X11 visual class {class:?}")
            }
            Self::InvalidPixelLayout(error) => {
                write!(formatter, "invalid X11 pixel layout: {error}")
            }
            Self::X11(error) => write!(formatter, "X11 capture failed: {error}"),
            Self::Png(error) => write!(formatter, "PNG encoding failed: {error}"),
        }
    }
}

impl Error for ImageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPixelLayout(error) => Some(error),
            Self::X11(error) => Some(error),
            Self::Png(error) => Some(error),
            _ => None,
        }
    }
}

/// An owned row-major image with one byte each for red, green, blue, and
/// alpha.  Alpha is retained in memory and in PNG output; BMP output follows
/// the broadly supported 24-bit BGR format and therefore omits alpha.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RgbaImage {
    width: u32,
    height: u32,
    pixels: Arc<Vec<u8>>,
}

impl RgbaImage {
    /// Allocate a transparent image of the requested dimensions.
    pub fn new(width: u32, height: u32) -> ImageResult<Self> {
        if width == 0 || height == 0 {
            return Err(Box::new(ImageError::InvalidDimensions));
        }
        let pixel_count = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or_else(|| {
                Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>
            })?;
        let byte_count = pixel_count.checked_mul(4).ok_or_else(|| {
            Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>
        })?;
        Ok(Self {
            width,
            height,
            pixels: Arc::new(vec![0; byte_count]),
        })
    }

    /// Construct an image from owned RGBA bytes.
    pub fn from_rgba(width: u32, height: u32, pixels: Vec<u8>) -> ImageResult<Self> {
        if width == 0 || height == 0 {
            return Err(Box::new(ImageError::InvalidDimensions));
        }
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| {
                Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>
            })?;
        if pixels.len() != expected {
            return Err(Box::new(ImageError::InvalidDimensions));
        }
        Ok(Self {
            width,
            height,
            pixels: Arc::new(pixels),
        })
    }

    /// Construct an image by repeating one RGBA color.
    pub fn solid_rgba(width: u32, height: u32, color: [u8; 4]) -> ImageResult<Self> {
        let mut image = Self::new(width, height)?;
        for pixel in image.pixels_mut().chunks_mut(4) {
            pixel.copy_from_slice(&color);
        }
        Ok(image)
    }

    /// Construct an opaque solid-color image.
    pub fn solid(width: u32, height: u32, color: [u8; 3]) -> ImageResult<Self> {
        Self::solid_rgba(width, height, [color[0], color[1], color[2], 255])
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the complete row-major RGBA byte slice.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Mutably borrow the complete row-major RGBA byte slice.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        Arc::make_mut(&mut self.pixels).as_mut_slice()
    }

    /// Crop a rectangle, clamping it to the source image like the original
    /// GdkPixbuf implementation.  `None` means that the clamped rectangle is
    /// empty.
    pub fn crop(&self, x: i32, y: i32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let left = i64::from(x).max(0).min(i64::from(self.width));
        let top = i64::from(y).max(0).min(i64::from(self.height));
        let right = (i64::from(x) + i64::from(width))
            .max(left)
            .min(i64::from(self.width));
        let bottom = (i64::from(y) + i64::from(height))
            .max(top)
            .min(i64::from(self.height));
        if right <= left || bottom <= top {
            return None;
        }

        let output_width = u32::try_from(right - left).ok()?;
        let output_height = u32::try_from(bottom - top).ok()?;
        let mut output = Self::new(output_width, output_height).ok()?;
        let source_width = usize::try_from(self.width).ok()?;
        let left = usize::try_from(left).ok()?;
        let top = usize::try_from(top).ok()?;
        let copy_width = usize::try_from(output_width).ok()?;
        let row_bytes = copy_width.checked_mul(4)?;
        for row in 0..usize::try_from(output_height).ok()? {
            let source_offset =
                (top + row).checked_mul(source_width)?.checked_mul(4)? + left.checked_mul(4)?;
            let output_offset = row.checked_mul(row_bytes)?;
            output.pixels_mut()[output_offset..output_offset + row_bytes]
                .copy_from_slice(&self.pixels[source_offset..source_offset + row_bytes]);
        }
        Some(output)
    }

    /// Encode this image as an 8-bit RGBA PNG.
    pub fn to_png(&self) -> ImageResult<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| Box::new(ImageError::Png(error)) as Box<dyn Error + Send + Sync>)?;
        writer
            .write_image_data(&self.pixels)
            .map_err(|error| Box::new(ImageError::Png(error)) as Box<dyn Error + Send + Sync>)?;
        drop(writer);
        Ok(bytes)
    }

    /// Encode this image as a standard 24-bit bottom-up Windows BMP.
    ///
    /// The clipboard MIME target is `image/bmp`, which is conventionally a
    /// complete BMP file rather than a bare DIB.  Rows are padded to a 4-byte
    /// boundary and channels are emitted in BGR order.
    pub fn to_bmp(&self) -> ImageResult<Vec<u8>> {
        let width = usize::try_from(self.width).map_err(|_| ImageError::InvalidDimensions)?;
        let height = usize::try_from(self.height).map_err(|_| ImageError::InvalidDimensions)?;
        if width == 0
            || height == 0
            || self.width > i32::MAX as u32
            || self.height > i32::MAX as u32
        {
            return Err(Box::new(ImageError::InvalidDimensions));
        }
        let unpadded_row = width.checked_mul(3).ok_or_else(|| {
            Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>
        })?;
        let row_stride = unpadded_row
            .checked_add(3)
            .map(|value| value & !3)
            .ok_or_else(|| {
                Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>
            })?;
        let pixel_bytes = row_stride.checked_mul(height).ok_or_else(|| {
            Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>
        })?;
        let file_size = 54usize.checked_add(pixel_bytes).ok_or_else(|| {
            Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>
        })?;
        let file_size_u32 = u32::try_from(file_size)
            .map_err(|_| Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>)?;
        let pixel_bytes_u32 = u32::try_from(pixel_bytes)
            .map_err(|_| Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>)?;

        let mut bytes = Vec::with_capacity(file_size);
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&file_size_u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&54u32.to_le_bytes());
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&(self.width as i32).to_le_bytes());
        bytes.extend_from_slice(&(self.height as i32).to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&24u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&pixel_bytes_u32.to_le_bytes());
        // 96 DPI is a conservative, widely understood default.
        bytes.extend_from_slice(&3780i32.to_le_bytes());
        bytes.extend_from_slice(&3780i32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        for row in (0..height).rev() {
            let source_row = row * width * 4;
            for x in 0..width {
                let source = source_row + x * 4;
                bytes.push(self.pixels[source + 2]);
                bytes.push(self.pixels[source + 1]);
                bytes.push(self.pixels[source]);
            }
            bytes.resize(bytes.len() + row_stride - unpadded_row, 0);
        }
        debug_assert_eq!(bytes.len(), file_size);
        Ok(bytes)
    }
}

/// Capture the root using dimensions that the caller already obtained from
/// an authoritative root-geometry query.  This avoids a second synchronous
/// `GetGeometry` round trip in the hotkey path.
pub fn capture_root_with_size<C: Connection>(
    conn: &C,
    screen_num: usize,
    width: u16,
    height: u16,
) -> ImageResult<RgbaImage> {
    if width == 0 || height == 0 {
        return Err(Box::new(ImageError::InvalidDimensions));
    }
    let (root, root_visual) = conn
        .setup()
        .roots
        .get(screen_num)
        .map(|screen| (screen.root, screen.root_visual))
        .ok_or_else(|| Box::new(ImageError::InvalidDimensions) as Box<dyn Error + Send + Sync>)?;
    capture_drawable(conn, root, 0, 0, width, height, root_visual)
}

/// Read an arbitrary X11 drawable rectangle into an owned RGBA image.
///
/// The reply's visual is authoritative for windows and pixmaps that carry
/// one. Some server-side drawables report visual id zero, so callers provide
/// the visual used when the drawable was created as a fallback. This helper
/// lets the native selection path keep the full desktop on the X server and
/// download only the accepted rectangle after the pointer is released.
pub fn capture_drawable<C: Connection>(
    conn: &C,
    drawable: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    fallback_visual: Visualid,
) -> ImageResult<RgbaImage> {
    if width == 0 || height == 0 {
        return Err(Box::new(ImageError::InvalidDimensions));
    }
    let (raw, visual_id) = X11Image::get(conn, drawable, x, y, width, height)
        .map_err(|error| Box::new(ImageError::X11(error)) as Box<dyn Error + Send + Sync>)?;
    let visual = find_visual(conn.setup(), visual_id)
        .or_else(|| find_visual(conn.setup(), fallback_visual))
        .ok_or_else(|| {
            Box::new(ImageError::MissingVisual(visual_id)) as Box<dyn Error + Send + Sync>
        })?;
    from_x11(&raw, visual)
}

/// Convert an x11rb native image using the supplied visual description.
///
/// This function is intentionally public and independent of a live X11
/// connection so format tests can exercise 24/32-bit data, unusual channel
/// masks, big-endian replies and padded scanlines deterministically.
pub fn from_x11(raw: &X11Image<'_>, visual: Visualtype) -> ImageResult<RgbaImage> {
    // DirectColor pixels are colormap indices.  Decoding them as masked RGB
    // values would silently produce incorrect screenshots; resolving the
    // colormap belongs in a separate capture path.
    if visual.class != VisualClass::TRUE_COLOR {
        return Err(Box::new(ImageError::UnsupportedVisualClass(visual.class)));
    }
    let layout = PixelLayout::from_visual_type(visual).map_err(|error| {
        Box::new(ImageError::InvalidPixelLayout(error)) as Box<dyn Error + Send + Sync>
    })?;
    // The root visual on the Linux desktops this app targets is almost
    // always the conventional 8-bit RGB layout.  `Image::get_pixel` plus
    // three mask decoders is correct for every X11 layout, but it is a hot
    // loop over every screen pixel.  Keep the generic path below for unusual
    // visuals while using a row-wise byte shuffle for the common packed form.
    if let Some(image) = fast_common_rgb(raw, visual) {
        return Ok(image);
    }
    from_x11_with_layout(raw, layout)
}

/// Decode the common TrueColor 24/32-bit RGB visual without calling
/// `Image::get_pixel` and `PixelLayout::decode` once per pixel.  X11 permits
/// both byte orders and both packed widths, so the fast path handles all four
/// combinations and leaves uncommon masks to the general decoder.
fn fast_common_rgb(raw: &X11Image<'_>, visual: Visualtype) -> Option<RgbaImage> {
    if visual.red_mask != 0x00ff_0000
        || visual.green_mask != 0x0000_ff00
        || visual.blue_mask != 0x0000_00ff
    {
        return None;
    }

    let bytes_per_pixel = match raw.bits_per_pixel() {
        x11rb::image::BitsPerPixel::B24 => 3,
        x11rb::image::BitsPerPixel::B32 => 4,
        _ => return None,
    };
    let width = usize::from(raw.width());
    let height = usize::from(raw.height());
    let bits_per_row = width.checked_mul(bytes_per_pixel)?.checked_mul(8)?;
    let pad_bits = usize::from(raw.scanline_pad());
    let stride = bits_per_row
        .checked_add(pad_bits - 1)?
        .checked_div(pad_bits)?
        .checked_mul(pad_bits / 8)?;
    let source = raw.data();
    let required = stride.checked_mul(height)?;
    if source.len() < required {
        return None;
    }
    let mut pixels = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    for row in 0..height {
        let source_row = &source[row * stride..row * stride + width * bytes_per_pixel];
        let output_row = &mut pixels[row * width * 4..(row + 1) * width * 4];
        for x in 0..width {
            let source_offset = x * bytes_per_pixel;
            let output_offset = x * 4;
            match (bytes_per_pixel, raw.byte_order()) {
                (3, x11rb::image::ImageOrder::LsbFirst) => {
                    output_row[output_offset..output_offset + 3].copy_from_slice(&[
                        source_row[source_offset + 2],
                        source_row[source_offset + 1],
                        source_row[source_offset],
                    ]);
                }
                (3, x11rb::image::ImageOrder::MsbFirst) => {
                    output_row[output_offset..output_offset + 3]
                        .copy_from_slice(&source_row[source_offset..source_offset + 3]);
                }
                (4, x11rb::image::ImageOrder::LsbFirst) => {
                    output_row[output_offset..output_offset + 3].copy_from_slice(&[
                        source_row[source_offset + 2],
                        source_row[source_offset + 1],
                        source_row[source_offset],
                    ]);
                }
                (4, x11rb::image::ImageOrder::MsbFirst) => {
                    output_row[output_offset..output_offset + 3].copy_from_slice(&[
                        source_row[source_offset + 1],
                        source_row[source_offset + 2],
                        source_row[source_offset + 3],
                    ]);
                }
                _ => unreachable!("fast path only admits 24/32-bit images"),
            }
            output_row[output_offset + 3] = 255;
        }
    }
    RgbaImage::from_rgba(raw.width().into(), raw.height().into(), pixels).ok()
}

/// Convert an x11rb image with an already validated pixel layout.
pub fn from_x11_with_layout(raw: &X11Image<'_>, layout: PixelLayout) -> ImageResult<RgbaImage> {
    let mut output = RgbaImage::new(u32::from(raw.width()), u32::from(raw.height()))?;
    let output_width = usize::from(raw.width());
    let output_pixels = output.pixels_mut();
    for y in 0..raw.height() {
        for x in 0..raw.width() {
            let (red, green, blue) = layout.decode(raw.get_pixel(x, y));
            let offset = (usize::from(y) * output_width + usize::from(x)) * 4;
            output_pixels[offset..offset + 4].copy_from_slice(&[
                (red >> 8) as u8,
                (green >> 8) as u8,
                (blue >> 8) as u8,
                255,
            ]);
        }
    }
    Ok(output)
}

/// Find a visual in all screens and allowed depth descriptions.
pub fn find_visual(setup: &Setup, visual: Visualid) -> Option<Visualtype> {
    setup
        .roots
        .iter()
        .flat_map(|screen| screen.allowed_depths.iter())
        .flat_map(|depth| depth.visuals.iter())
        .find(|candidate| candidate.visual_id == visual)
        .copied()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{from_x11, RgbaImage};
    use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
    use x11rb::protocol::xproto::{VisualClass, Visualtype};

    fn true_color_visual() -> Visualtype {
        Visualtype {
            visual_id: 7,
            class: VisualClass::TRUE_COLOR,
            bits_per_rgb_value: 8,
            colormap_entries: 256,
            red_mask: 0x00ff_0000,
            green_mask: 0x0000_ff00,
            blue_mask: 0x0000_00ff,
        }
    }

    #[test]
    fn decodes_big_endian_padded_24_bit_rows() {
        // Two pixels per row occupy six bytes and are padded to eight.  The
        // MsbFirst byte order stores the most significant channel first.
        let raw = Image::new(
            2,
            2,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B24,
            ImageOrder::MsbFirst,
            Cow::Borrowed(&[
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0xaa, 0xbb, // row 0 + pad
                0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xcc, 0xdd, // row 1 + pad
            ]),
        )
        .expect("raw image layout is valid");

        let converted = from_x11(&raw, true_color_visual()).expect("true-color conversion works");
        assert_eq!(&converted.pixels()[..4], &[0x11, 0x22, 0x33, 255]);
        assert_eq!(&converted.pixels()[4..8], &[0x44, 0x55, 0x66, 255]);
        assert_eq!(&converted.pixels()[8..12], &[0x77, 0x88, 0x99, 255]);
        assert_eq!(&converted.pixels()[12..16], &[0xaa, 0xbb, 0xcc, 255]);
    }

    #[test]
    fn decodes_little_endian_32_bit_rgb_rows_without_alpha() {
        let raw = Image::new(
            2,
            1,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B32,
            ImageOrder::LsbFirst,
            Cow::Borrowed(&[
                0x33, 0x22, 0x11, 0xee, // RGB = 11, 22, 33
                0x66, 0x55, 0x44, 0xdd, // RGB = 44, 55, 66
            ]),
        )
        .expect("raw image layout is valid");

        let converted = from_x11(&raw, true_color_visual()).expect("true-color conversion works");
        assert_eq!(
            converted.pixels(),
            &[0x11, 0x22, 0x33, 255, 0x44, 0x55, 0x66, 255]
        );
    }

    #[test]
    fn rejects_direct_color_without_colormap_resolution() {
        let raw = Image::new(
            1,
            1,
            ScanlinePad::Pad32,
            24,
            BitsPerPixel::B24,
            ImageOrder::LsbFirst,
            Cow::Borrowed(&[0, 0, 0, 0]),
        )
        .expect("raw image layout is valid");
        let mut visual = true_color_visual();
        visual.class = VisualClass::DIRECT_COLOR;
        assert!(from_x11(&raw, visual).is_err());
    }

    #[test]
    fn bmp_rows_are_bottom_up_and_padded() {
        let image = RgbaImage::from_rgba(1, 2, vec![10, 20, 30, 255, 40, 50, 60, 255])
            .expect("image dimensions match");
        let bmp = image.to_bmp().expect("BMP encoding works");
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(u32::from_le_bytes(bmp[2..6].try_into().unwrap()), 62);
        assert_eq!(u32::from_le_bytes(bmp[10..14].try_into().unwrap()), 54);
        // Bottom-up row first: second RGBA pixel, then its one-byte pad.
        assert_eq!(&bmp[54..58], &[60, 50, 40, 0]);
        assert_eq!(&bmp[58..62], &[30, 20, 10, 0]);
    }

    #[test]
    fn cloned_images_share_until_mutated() {
        let mut original = RgbaImage::solid(2, 1, [1, 2, 3]).expect("image dimensions are valid");
        let clone = original.clone();
        original.pixels_mut()[..4].copy_from_slice(&[9, 8, 7, 255]);

        assert_eq!(&clone.pixels()[..4], &[1, 2, 3, 255]);
        assert_eq!(&original.pixels()[..4], &[9, 8, 7, 255]);
    }
}
