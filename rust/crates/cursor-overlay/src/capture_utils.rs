//! Shared image-manipulation utilities for the `zoom` tool.
//!
//! All three platform crates depend on cursor-overlay, so this is the natural
//! home for a pure-Rust crop + JPEG-encode helper.

use image::{DynamicImage, GenericImageView};

/// Result of a crop-and-encode operation; carries coordinate metadata so callers
/// can translate zoom-image pixel coordinates back to full-window pixel coordinates.
pub struct CropResult {
    pub jpeg_bytes: Vec<u8>,
    /// Output image width in pixels.
    pub out_w: u32,
    /// Output image height in pixels.
    pub out_h: u32,
    /// X of the padded crop origin in full-window pixel space.
    pub origin_x: f64,
    /// Y of the padded crop origin in full-window pixel space.
    pub origin_y: f64,
    /// Width of the padded crop in full-window pixel space, before any resize.
    pub crop_w: u32,
    /// Height of the padded crop in full-window pixel space, before any resize.
    pub crop_h: u32,
    /// Inverse of the resize scale: `cw / out_w`.
    /// Multiply a zoom-image X coordinate by this to get the full-window offset from origin.
    /// Equal to 1.0 when no downscale was applied.
    pub scale_inv: f64,
}

impl CropResult {
    /// Convert a zoom-image coordinate `(px, py)` back to full-window pixel coordinates.
    pub fn zoom_to_window(&self, px: f64, py: f64) -> (f64, f64) {
        (
            self.origin_x + px * self.scale_inv,
            self.origin_y + py * self.scale_inv,
        )
    }
}

/// Decode a PNG, crop to the given region with 20% padding on all sides,
/// optionally scale down to `max_width`, and re-encode as JPEG.
pub fn crop_png_to_jpeg(
    png_bytes: &[u8],
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    max_width: u32,
) -> anyhow::Result<CropResult> {
    let img = image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png)?;
    crop_image_to_jpeg(&img, x1, y1, x2, y2, max_width)
}

/// [`crop_png_to_jpeg`] for an image that is already decoded.
pub fn crop_image_to_jpeg(
    img: &DynamicImage,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    max_width: u32,
) -> anyhow::Result<CropResult> {
    let (img_w, img_h) = img.dimensions();

    // Region dimensions (at least 1px to avoid divide-by-zero).
    let rw = (x2 - x1).max(1.0);
    let rh = (y2 - y1).max(1.0);

    // 20% outward padding on each side.
    let pad_x = rw * 0.20;
    let pad_y = rh * 0.20;

    let cx1 = ((x1 - pad_x).max(0.0) as u32).min(img_w);
    let cy1 = ((y1 - pad_y).max(0.0) as u32).min(img_h);
    let cx2 = ((x2 + pad_x).min(img_w as f64) as u32).min(img_w);
    let cy2 = ((y2 + pad_y).min(img_h as f64) as u32).min(img_h);

    let cw = cx2.saturating_sub(cx1).max(1);
    let ch = cy2.saturating_sub(cy1).max(1);

    let cropped = img.crop_imm(cx1, cy1, cw, ch);

    // Scale down if the crop is wider than max_width.
    let out: DynamicImage = if cw > max_width {
        let scale = max_width as f64 / cw as f64;
        let new_h = ((ch as f64) * scale).round().max(1.0) as u32;
        pincer_core::image_utils::lanczos3_resize_exact(&cropped, max_width, new_h)
    } else {
        cropped
    };

    let (out_w, out_h) = out.dimensions();
    let scale_inv = cw as f64 / out_w as f64; // 1.0 if no resize

    // Encode as JPEG.
    let mut buf = Vec::new();
    out.write_to(
        &mut std::io::Cursor::new(&mut buf),
        image::ImageFormat::Jpeg,
    )?;

    Ok(CropResult {
        jpeg_bytes: buf,
        out_w,
        out_h,
        origin_x: cx1 as f64,
        origin_y: cy1 as f64,
        crop_w: cw,
        crop_h: ch,
        scale_inv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_and_png_input_give_the_same_jpeg() {
        let image = image::RgbaImage::from_fn(900, 400, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8, 255])
        });
        let png = pincer_core::image_utils::encode_rgba_to_png(image.as_raw(), 900, 400).unwrap();
        // Wide enough to be scaled down to 500 pixels.
        let from_png = crop_png_to_jpeg(&png, 10.0, 20.0, 700.0, 300.0, 500).unwrap();
        let decoded = DynamicImage::ImageRgba8(image);
        let from_image = crop_image_to_jpeg(&decoded, 10.0, 20.0, 700.0, 300.0, 500).unwrap();
        assert_eq!(from_png.out_w, 500);
        assert_eq!(from_png.jpeg_bytes, from_image.jpeg_bytes);
        assert_eq!(
            (from_png.out_h, from_png.origin_x, from_png.scale_inv),
            (from_image.out_h, from_image.origin_x, from_image.scale_inv)
        );
    }
}
