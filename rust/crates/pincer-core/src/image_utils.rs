//! Cross-platform PNG / JPEG / crosshair / resize helpers.
//!
//! These functions are pure consumers of the [`image`] crate with no
//! platform-specific dependencies, so they were perfect candidates for
//! deduplication. Until 2026-05 they lived as near-identical copies in
//! `platform-{macos,windows,linux}/src/capture.rs`.
//!
//! Each platform's `capture.rs` still owns its own
//! `screenshot_window_bytes` / `screenshot_display_bytes` (CGImage on
//! macOS, BitBlt+PrintWindow on Windows, XGetImage / ImageMagick
//! `import` on Linux). Everything downstream of "I have RGBA pixels" —
//! PNG encoding, JPEG encoding, downscaling to a max long edge, drawing
//! a crosshair, reading width/height from an IHDR — is here.
//!
//! No public API removed: the platform `capture.rs` modules re-export
//! the same function names so existing callers keep compiling.

use anyhow::{anyhow, bail, Result};
use image::{
    ColorType, DynamicImage, ExtendedColorType, ImageBuffer, ImageDecoder, ImageEncoder,
    ImageFormat, RgbImage, RgbaImage,
};

// ── Captured image ────────────────────────────────────────────────────────

/// A screenshot as its capture backend produced it. A backend that converts
/// the frame itself hands over RGBA pixels, so callers resize or crop them and
/// encode a PNG once; a backend that only gives a PNG (a compositor helper,
/// ImageMagick) keeps its bytes. Every method gives the same result for an
/// `Rgba` image as the PNG helpers below give for the PNG of those pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapturedImage {
    Png(Vec<u8>),
    Rgba(RgbaImage),
}

impl CapturedImage {
    /// No PNG bytes, or no pixels.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Png(png) => png.is_empty(),
            Self::Rgba(image) => image.width() == 0 || image.height() == 0,
        }
    }

    pub fn dimensions(&self) -> Result<(u32, u32)> {
        match self {
            Self::Png(png) => png_dimensions(png),
            Self::Rgba(image) => Ok(image.dimensions()),
        }
    }

    /// [`resize_png_if_needed`], size rounding included.
    pub fn fit_long_edge(self, max_dim: u32) -> Result<Self> {
        match self {
            Self::Png(png) => {
                if max_dim == 0 {
                    return Ok(Self::Png(png));
                }
                let (w, h) = png_dimensions(&png)?;
                if w <= max_dim && h <= max_dim {
                    return Ok(Self::Png(png));
                }
                resize_png_if_needed(&png, max_dim).map(Self::Png)
            }
            Self::Rgba(image) => Ok(Self::Rgba(resize_rgba8_if_needed(image, max_dim))),
        }
    }

    /// Resample to exactly `new_w`x`new_h` with Lanczos3 in one pass; the
    /// same size returns the image as it is. A PNG keeps its color type, as
    /// in [`resize_png_if_needed`]. Two size limits in a row (see
    /// [`fit_long_edge_dimensions`]) go through here once instead of
    /// resampling twice.
    pub fn resize_to(self, new_w: u32, new_h: u32) -> Result<Self> {
        if self.dimensions()? == (new_w, new_h) {
            return Ok(self);
        }
        match self {
            Self::Png(png) => resize_png_to(&png, new_w, new_h).map(Self::Png),
            Self::Rgba(image) => Ok(Self::Rgba(lanczos3_resize_rgba8(&image, new_w, new_h))),
        }
    }

    /// [`crop_png`].
    pub fn crop(self, rect: PixelRect) -> Result<Self> {
        match self {
            Self::Png(png) => crop_png(&png, rect).map(Self::Png),
            Self::Rgba(image) => crop_rgba8(&image, rect).map(Self::Rgba),
        }
    }

    /// The pixels, decoding a PNG with [`decode_png_to_rgba8`].
    pub fn into_rgba8(self) -> Result<RgbaImage> {
        match self {
            Self::Png(png) => decode_png_to_rgba8(&png),
            Self::Rgba(image) => Ok(image),
        }
    }

    /// The image in its own color type, decoding a PNG the way
    /// `image::load_from_memory_with_format(.., Png)` does.
    pub fn into_dynamic(self) -> Result<DynamicImage> {
        match self {
            Self::Png(png) => Ok(image::load_from_memory_with_format(&png, ImageFormat::Png)?),
            Self::Rgba(image) => Ok(DynamicImage::ImageRgba8(image)),
        }
    }

    pub fn into_png(self) -> Result<Vec<u8>> {
        self.into_png_and_rgba().map(|(png, _)| png)
    }

    /// The PNG, plus the pixels it was just encoded from when there were any
    /// (see `CaptureService::publish_encoded_rgba`).
    pub fn into_png_and_rgba(self) -> Result<(Vec<u8>, Option<RgbaImage>)> {
        match self {
            Self::Png(png) => Ok((png, None)),
            Self::Rgba(image) => {
                let png = encode_rgba_to_png(image.as_raw(), image.width(), image.height())?;
                Ok((png, Some(image)))
            }
        }
    }
}

// ── PNG → JPEG ────────────────────────────────────────────────────────────

/// Convert raw PNG bytes to JPEG at the given quality (1-95).
///
/// Strips alpha (RGBA → RGB) since JPEG doesn't carry alpha. Quality is
/// clamped to the encoder's accepted range by the underlying `image`
/// crate.
pub fn png_bytes_to_jpeg(png_bytes: &[u8], quality: u8) -> Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(png_bytes);
    let decoder = image::codecs::png::PngDecoder::new(cursor)?;
    let (w, h) = decoder.dimensions();
    let color = decoder.color_type();
    let mut buf = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buf)?;

    // RGBA → RGB if needed (drop the alpha channel — JPEG can't store it).
    let rgb_buf: Vec<u8> = if color == ColorType::Rgba8 {
        buf.chunks_exact(4)
            .flat_map(|px| [px[0], px[1], px[2]])
            .collect()
    } else {
        buf
    };

    let mut jpeg_bytes = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, quality);
    enc.encode(&rgb_buf, w, h, ColorType::Rgb8.into())?;
    Ok(jpeg_bytes)
}

// ── Downscale ─────────────────────────────────────────────────────────────

/// The screenshot long-edge limits that apply to one capture.
///
/// Fields are named because both sources are `u32`-valued, and positional
/// arguments invite swapping the per-call override with the ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageDimensionLimits {
    /// The session or global `max_image_dimension` ceiling. `0` means no limit.
    pub configured: u32,
    /// The per-call `max_image_dimension` override.
    pub max_image_dimension: Option<u32>,
}

impl ImageDimensionLimits {
    /// Resolve the `max_dim` to pass to [`resize_png_if_needed`].
    ///
    /// The per-call override wins outright, including `0` for native
    /// resolution; otherwise the configured ceiling applies. Returns `0` only
    /// when nothing imposes a limit.
    pub fn resolve(self) -> u32 {
        self.max_image_dimension.unwrap_or(self.configured)
    }
}

/// Downscale `png_bytes` so neither dimension exceeds `max_dim`.
///
/// `max_dim == 0` is treated as "no cap"; the original bytes are
/// returned unchanged. Aspect ratio is preserved. The resampler is
/// Lanczos3 (see [`lanczos3_resize_rgba8`]).
pub fn resize_png_if_needed(png_bytes: &[u8], max_dim: u32) -> Result<Vec<u8>> {
    if max_dim == 0 {
        return Ok(png_bytes.to_vec());
    }
    let (w, h) = png_dimensions(png_bytes)?;
    if w <= max_dim && h <= max_dim {
        return Ok(png_bytes.to_vec());
    }
    let (new_w, new_h) = png_fit_dimensions(w, h, max_dim);
    resize_png_to(png_bytes, new_w, new_h)
}

/// Decode a PNG, resample it to `new_w`x`new_h` with Lanczos3 and encode it
/// again in its own color type (RGBA or RGB).
fn resize_png_to(png_bytes: &[u8], new_w: u32, new_h: u32) -> Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(png_bytes);
    let decoder = image::codecs::png::PngDecoder::new(cursor)?;
    let (w, h) = decoder.dimensions();
    let color = decoder.color_type();
    let mut buf = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buf)?;

    match color {
        ColorType::Rgba8 => {
            let image: RgbaImage =
                ImageBuffer::from_raw(w, h, buf).ok_or_else(|| anyhow!("invalid RGBA buffer"))?;
            let resized = lanczos3_resize_rgba8(&image, new_w, new_h);
            encode_rgba_to_png(resized.as_raw(), new_w, new_h)
        }
        ColorType::Rgb8 => {
            let image: RgbImage =
                ImageBuffer::from_raw(w, h, buf).ok_or_else(|| anyhow!("invalid RGB buffer"))?;
            let resized = lanczos3_resize_rgb8(&image, new_w, new_h);
            let mut out = Vec::new();
            image::codecs::png::PngEncoder::new(&mut out).write_image(
                resized.as_raw(),
                new_w,
                new_h,
                ExtendedColorType::Rgb8,
            )?;
            Ok(out)
        }
        _ => bail!("unsupported color type for resize: {color:?}"),
    }
}

/// [`resize_png_if_needed`] for RGBA pixels: the same output size and pixels,
/// with no PNG on either side.
pub fn resize_rgba8_if_needed(image: RgbaImage, max_dim: u32) -> RgbaImage {
    let (w, h) = image.dimensions();
    if max_dim == 0 || (w <= max_dim && h <= max_dim) {
        return image;
    }
    let (new_w, new_h) = png_fit_dimensions(w, h, max_dim);
    lanczos3_resize_rgba8(&image, new_w, new_h)
}

/// The size [`CapturedImage::fit_long_edge`] gives a `w`x`h` image: the same
/// size when `max_dim` is 0 or both edges already fit, otherwise the
/// rounding of [`png_fit_dimensions`].
pub fn fit_long_edge_dimensions(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    if max_dim == 0 || (w <= max_dim && h <= max_dim) {
        return (w, h);
    }
    png_fit_dimensions(w, h, max_dim)
}

/// The size [`resize_png_if_needed`] resizes a `w`x`h` image to when one edge
/// exceeds `max_dim`. It used to call `DynamicImage::resize(new_w, new_h, ..)`,
/// which fits the image inside that box again (image 0.25.10,
/// `math::utils::resize_dimensions` with `fill = false`), so a short edge that
/// rounded down can shrink the long edge too. That second step is kept here so
/// the output size does not change.
fn png_fit_dimensions(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    let scale = (max_dim as f64) / (w.max(h) as f64);
    let new_w = (w as f64 * scale).round() as u32;
    let new_h = (h as f64 * scale).round() as u32;
    if (new_w, new_h) == (w, h) {
        return (w, h);
    }
    let ratio = f64::min(
        f64::from(new_w) / f64::from(w),
        f64::from(new_h) / f64::from(h),
    );
    // image clamps a result above u32::MAX; a downscale never gets there.
    let fit_w = ((f64::from(w) * ratio).round() as u64).max(1);
    let fit_h = ((f64::from(h) * ratio).round() as u64).max(1);
    (fit_w as u32, fit_h as u32)
}

// ── Lanczos3 resampling ───────────────────────────────────────────────────

/// `image::imageops::resize(image, new_w, new_h, FilterType::Lanczos3)`,
/// byte for byte, in plain loops the compiler can vectorize.
pub fn lanczos3_resize_rgba8(image: &RgbaImage, new_w: u32, new_h: u32) -> RgbaImage {
    let (w, h) = image.dimensions();
    let pixels = &image.as_raw()[..w as usize * h as usize * 4];
    ImageBuffer::from_raw(
        new_w,
        new_h,
        lanczos3_resize::<4>(pixels, w, h, new_w, new_h),
    )
    .expect("the resize fills new_w x new_h pixels")
}

/// [`lanczos3_resize_rgba8`] for RGB pixels.
pub fn lanczos3_resize_rgb8(image: &RgbImage, new_w: u32, new_h: u32) -> RgbImage {
    let (w, h) = image.dimensions();
    let pixels = &image.as_raw()[..w as usize * h as usize * 3];
    ImageBuffer::from_raw(
        new_w,
        new_h,
        lanczos3_resize::<3>(pixels, w, h, new_w, new_h),
    )
    .expect("the resize fills new_w x new_h pixels")
}

/// `DynamicImage::resize_exact(new_w, new_h, FilterType::Lanczos3)`: the
/// plain loops for 8-bit RGB and RGBA, the image crate for anything else.
pub fn lanczos3_resize_exact(image: &DynamicImage, new_w: u32, new_h: u32) -> DynamicImage {
    match image {
        DynamicImage::ImageRgba8(image) => {
            DynamicImage::ImageRgba8(lanczos3_resize_rgba8(image, new_w, new_h))
        }
        DynamicImage::ImageRgb8(image) => {
            DynamicImage::ImageRgb8(lanczos3_resize_rgb8(image, new_w, new_h))
        }
        other => other.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3),
    }
}

/// image 0.25.10's `sinc` and `lanczos3_kernel` (src/imageops/sample.rs).
fn lanczos3_kernel(x: f32) -> f32 {
    fn sinc(t: f32) -> f32 {
        let a = t * std::f32::consts::PI;
        if t == 0.0 {
            1.0
        } else {
            a.sin() / a
        }
    }
    if x.abs() < 3.0 {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// Normalized Lanczos3 weights for resampling `src` pixels to `dst` along one
/// axis. Output pixel `i` reads input pixels `left[i]..` with the weights in
/// `weights[start[i]..start[i + 1]]`.
struct Lanczos3Axis {
    left: Vec<usize>,
    start: Vec<usize>,
    weights: Vec<f32>,
}

impl Lanczos3Axis {
    /// The window and weights image's `vertical_sample` and
    /// `horizontal_sample` compute, in the same f32 steps and order.
    fn new(src: u32, dst: u32) -> Self {
        let ratio = src as f32 / dst as f32;
        let sratio = if ratio < 1.0 { 1.0 } else { ratio };
        let src_support = 3.0 * sratio;
        let mut axis = Self {
            left: Vec::with_capacity(dst as usize),
            start: Vec::with_capacity(dst as usize + 1),
            weights: Vec::new(),
        };
        for out in 0..dst {
            let input = (out as f32 + 0.5) * ratio;
            let left = ((input - src_support).floor() as i64).clamp(0, i64::from(src) - 1);
            let right = ((input + src_support).ceil() as i64).clamp(left + 1, i64::from(src));
            let center = input - 0.5;
            let start = axis.weights.len();
            let mut sum = 0.0f32;
            for i in left..right {
                let weight = lanczos3_kernel((i as u32 as f32 - center) / sratio);
                axis.weights.push(weight);
                sum += weight;
            }
            for weight in &mut axis.weights[start..] {
                *weight /= sum;
            }
            axis.left.push(left as usize);
            axis.start.push(start);
        }
        axis.start.push(axis.weights.len());
        axis
    }

    fn taps(&self, out: usize) -> (usize, &[f32]) {
        (
            self.left[out],
            &self.weights[self.start[out]..self.start[out + 1]],
        )
    }
}

/// image's float-to-u8 step: clamp to 0..=255, then round half away from
/// zero. `f32::round` is a libm call and `as u8` a scalar conversion per
/// channel, so this stays in float operations the compiler can vectorize:
/// adding and subtracting 2^23 rounds a value in 0..=255 to the nearest
/// integer (ties to even), stepping back where that went up gives the floor,
/// `v - floor` is exact, and `x + 2^23` holds an integer `x` in its low
/// mantissa bits.
fn lanczos3_round_u8(v: f32) -> u8 {
    const TWO_POW_23: f32 = 8_388_608.0;
    let v = if v < 0.0 { 0.0 } else { v };
    let v = if v > 255.0 { 255.0 } else { v };
    let nearest = (v + TWO_POW_23) - TWO_POW_23;
    let floor = if nearest > v { nearest - 1.0 } else { nearest };
    let rounded = if v - floor >= 0.5 { floor + 1.0 } else { floor };
    (rounded + TWO_POW_23).to_bits() as u8
}

/// `image::imageops::resize(.., Lanczos3)` for 8-bit pixels with `N`
/// interleaved channels. Like image, it resamples columns into an f32 buffer
/// first and rows second, summing each output value over the same inputs in
/// the same order, so every result rounds to the same byte.
fn lanczos3_resize<const N: usize>(src: &[u8], w: u32, h: u32, new_w: u32, new_h: u32) -> Vec<u8> {
    let (sw, dw, dh) = (w as usize, new_w as usize, new_h as usize);
    assert_eq!(src.len(), sw * h as usize * N, "pixel buffer size");
    if w == 0 || h == 0 || new_w == 0 || new_h == 0 {
        return vec![0; dw * dh * N];
    }
    if (new_w, new_h) == (w, h) {
        return src.to_vec();
    }

    let src_row = sw * N;
    let mut columns = vec![0f32; src_row * dh];
    let vertical = Lanczos3Axis::new(h, new_h);
    for (y, row) in columns.chunks_exact_mut(src_row).enumerate() {
        let (top, weights) = vertical.taps(y);
        for (i, &weight) in weights.iter().enumerate() {
            let line = &src[(top + i) * src_row..(top + i + 1) * src_row];
            for (acc, &value) in row.iter_mut().zip(line) {
                *acc += f32::from(value) * weight;
            }
        }
    }

    let mut out = vec![0u8; dw * dh * N];
    let horizontal = Lanczos3Axis::new(w, new_w);
    for (row, line) in out
        .chunks_exact_mut(dw * N)
        .zip(columns.chunks_exact(src_row))
    {
        for (x, pixel) in row.chunks_exact_mut(N).enumerate() {
            let (left, weights) = horizontal.taps(x);
            let taps = &line[left * N..(left + weights.len()) * N];
            let mut acc = [0f32; N];
            for (&weight, input) in weights.iter().zip(taps.chunks_exact(N)) {
                let input: &[f32; N] = input.try_into().expect("chunks of N");
                for c in 0..N {
                    acc[c] += input[c] * weight;
                }
            }
            for c in 0..N {
                pixel[c] = lanczos3_round_u8(acc[c]);
            }
        }
    }
    out
}

// ── Crosshair ─────────────────────────────────────────────────────────────

/// Draw a red crosshair at pixel (cx, cy) on a PNG and write it to `path`.
/// Used by `click`'s `debug_image_out` param to verify coordinate spaces.
/// The crosshair uses top-left-origin coords matching the click tool's
/// convention.
///
/// Tilde-expands `~` in `path` to `$HOME` (or `%USERPROFILE%` via the same
/// env-var name on Windows).
pub fn write_crosshair_png(png_bytes: &[u8], cx: f64, cy: f64, path: &str) -> Result<()> {
    let mut img = decode_png_to_rgba8(png_bytes)?;
    validate_crosshair_point(&img, cx, cy)?;
    draw_crosshair(&mut img, cx, cy);

    let path = if let Some(rest) = path.strip_prefix('~') {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map_err(|_| {
                anyhow::anyhow!(
                    "Cannot expand `~` in path {path:?}: neither HOME nor USERPROFILE is set"
                )
            })?;
        if home.is_empty() {
            anyhow::bail!("Cannot expand `~` in path {path:?}: HOME/USERPROFILE is empty");
        }
        format!("{home}{rest}")
    } else {
        path.to_owned()
    };
    img.save_with_format(&path, ImageFormat::Png)?;
    Ok(())
}

/// Draw a red crosshair at pixel (cx, cy) and return the modified PNG bytes.
/// Used by the recording subsystem's click-marker callback to produce
/// `click.png` without a temp file.
pub fn crosshair_png_bytes(png_bytes: &[u8], cx: f64, cy: f64) -> Result<Vec<u8>> {
    let mut img = decode_png_to_rgba8(png_bytes)?;
    validate_crosshair_point(&img, cx, cy)?;
    draw_crosshair(&mut img, cx, cy);

    let mut out = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)?;
    Ok(out)
}

fn validate_crosshair_point(
    img: &ImageBuffer<image::Rgba<u8>, Vec<u8>>,
    cx: f64,
    cy: f64,
) -> Result<()> {
    if !cx.is_finite()
        || !cy.is_finite()
        || cx < 0.0
        || cy < 0.0
        || cx >= f64::from(img.width())
        || cy >= f64::from(img.height())
    {
        bail!("click point is outside the retained image");
    }
    Ok(())
}

/// Decode an RGB8 or RGBA8 PNG to an RGBA8 image buffer.
pub fn decode_png_to_rgba8(png_bytes: &[u8]) -> Result<ImageBuffer<image::Rgba<u8>, Vec<u8>>> {
    let cursor = std::io::Cursor::new(png_bytes);
    let decoder = image::codecs::png::PngDecoder::new(cursor)?;
    let (w, h) = decoder.dimensions();
    let color = decoder.color_type();
    let mut buf = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buf)?;

    let img: DynamicImage = match color {
        ColorType::Rgba8 => DynamicImage::ImageRgba8(
            ImageBuffer::from_raw(w, h, buf).ok_or_else(|| anyhow!("invalid RGBA buffer"))?,
        ),
        ColorType::Rgb8 => DynamicImage::ImageRgb8(
            ImageBuffer::from_raw(w, h, buf).ok_or_else(|| anyhow!("invalid RGB buffer"))?,
        ),
        _ => bail!("unsupported PNG color type: {color:?}"),
    };
    Ok(img.to_rgba8())
}

/// Internal: draw the red ring + horizontal + vertical arms in place.
fn draw_crosshair(img: &mut ImageBuffer<image::Rgba<u8>, Vec<u8>>, cx: f64, cy: f64) {
    let w = img.width();
    let h = img.height();

    let ring_r = (w as f64 / 80.0).max(6.0) as i32;
    let arm_len = (w as f64 / 40.0).max(12.0) as i32;
    let line_w = ((w as f64 / 400.0).max(1.5)) as i32;
    let red = image::Rgba([255u8, 26, 26, 242]);
    let cx = cx as i32;
    let cy = cy as i32;

    // Horizontal + vertical arms.
    for lw in 0..=line_w {
        let off = lw - line_w / 2;
        for dx in -arm_len..=arm_len {
            if let Some(p) = img.get_pixel_mut_checked(
                (cx + dx).clamp(0, w as i32 - 1) as u32,
                (cy + off).clamp(0, h as i32 - 1) as u32,
            ) {
                *p = red;
            }
        }
        for dy in -arm_len..=arm_len {
            if let Some(p) = img.get_pixel_mut_checked(
                (cx + off).clamp(0, w as i32 - 1) as u32,
                (cy + dy).clamp(0, h as i32 - 1) as u32,
            ) {
                *p = red;
            }
        }
    }

    // Stroked ring.
    let steps = (ring_r * 12).max(48) as usize;
    for i in 0..steps {
        let theta = 2.0 * std::f64::consts::PI * i as f64 / steps as f64;
        let rx = (cx as f64 + ring_r as f64 * theta.cos()) as i32;
        let ry = (cy as f64 + ring_r as f64 * theta.sin()) as i32;
        for lw in 0..=line_w {
            let off = lw - line_w / 2;
            for dx in 0..=1 {
                let fx = (rx + off + dx).clamp(0, w as i32 - 1) as u32;
                let fy = (ry + off).clamp(0, h as i32 - 1) as u32;
                *img.get_pixel_mut(fx, fy) = red;
            }
        }
    }
}

// ── Crop ──────────────────────────────────────────────────────────────────

/// A pixel rectangle inside an image: top-left corner plus size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    /// Whether the rectangle is non-empty and lies inside a
    /// `width`x`height` image.
    pub fn fits_within(&self, width: u32, height: u32) -> bool {
        self.width > 0
            && self.height > 0
            && u64::from(self.x) + u64::from(self.width) <= u64::from(width)
            && u64::from(self.y) + u64::from(self.height) <= u64::from(height)
    }
}

/// Copy `rect` out of an RGBA image. A rectangle that does not fit is an
/// error, never a silent clamp.
pub fn crop_rgba8(
    image: &ImageBuffer<image::Rgba<u8>, Vec<u8>>,
    rect: PixelRect,
) -> Result<ImageBuffer<image::Rgba<u8>, Vec<u8>>> {
    let (w, h) = image.dimensions();
    if !rect.fits_within(w, h) {
        bail!(
            "crop {},{} {}x{} does not fit the {w}x{h} image",
            rect.x,
            rect.y,
            rect.width,
            rect.height
        );
    }
    Ok(image::imageops::crop_imm(image, rect.x, rect.y, rect.width, rect.height).to_image())
}

/// Crop a PNG to `rect` and re-encode it as PNG.
pub fn crop_png(png_bytes: &[u8], rect: PixelRect) -> Result<Vec<u8>> {
    let cropped = crop_rgba8(&decode_png_to_rgba8(png_bytes)?, rect)?;
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(cropped)
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)?;
    Ok(out)
}

/// Downscale an RGBA image so its long edge is at most `max_dim`, with the
/// same size rounding and Lanczos3 resampler as [`resize_png_if_needed`].
/// `max_dim == 0` or an image that already fits is returned unchanged.
pub fn fit_rgba8_long_edge(
    image: ImageBuffer<image::Rgba<u8>, Vec<u8>>,
    max_dim: u32,
) -> ImageBuffer<image::Rgba<u8>, Vec<u8>> {
    let (w, h) = image.dimensions();
    if max_dim == 0 || (w <= max_dim && h <= max_dim) {
        return image;
    }
    let scale = f64::from(max_dim) / f64::from(w.max(h));
    let new_w = ((f64::from(w) * scale).round() as u32).max(1);
    let new_h = ((f64::from(h) * scale).round() as u32).max(1);
    lanczos3_resize_rgba8(&image, new_w, new_h)
}

// ── PNG dimensions ────────────────────────────────────────────────────────

/// Parse width and height from a PNG file's IHDR chunk.
///
/// Pure byte-slice parsing — does not depend on the `image` crate's
/// decoder. Used by `resize_png_if_needed`'s fast-path size check.
pub fn png_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    // PNG signature: 8 bytes.
    // Then IHDR chunk: 4-byte length, 4-byte "IHDR", 4-byte width, 4-byte height.
    if data.len() < 24 {
        bail!("PNG data too small");
    }
    const PNG_SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if data[..8] != PNG_SIG {
        bail!("not a PNG: signature mismatch");
    }
    if &data[12..16] != b"IHDR" {
        bail!("not a PNG: missing IHDR chunk");
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Ok((w, h))
}

// ── Raw RGBA → PNG (used by Windows + Linux capture paths) ────────────────

/// Encode raw RGBA bytes (top-down, row-major) as a PNG.
///
/// Uses the `image` crate's encoder rather than the hand-rolled
/// uncompressed-PNG path that Windows + Linux previously carried — that
/// path was a workspace-local optimization that traded ~5x larger output
/// for less code, but the `image` crate's encoder is fast enough (it
/// already ships in every consumer of `image`, no new code paths) and
/// the smaller output saves more bytes downstream than the encode cost.
///
/// Caller guarantees `rgba.len() == w * h * 4`.
pub fn encode_rgba_to_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    if rgba.len() as u64 != (w as u64) * (h as u64) * 4 {
        bail!(
            "encode_rgba_to_png: buffer size {} != w({w}) * h({h}) * 4",
            rgba.len()
        );
    }
    // The encoder `DynamicImage::write_to(.., ImageFormat::Png)` uses, with
    // its default settings, so the bytes are the same without copying the
    // pixels into an image first.
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(
        rgba,
        w,
        h,
        ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// Encode raw BGRA bytes (top-down, row-major) as a PNG.
///
/// Windows GDI gives us BGRA; we swap channels in-place then defer to
/// [`encode_rgba_to_png`]. Caller guarantees the buffer's size invariant.
pub fn encode_bgra_to_png(bgra: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut rgba = bgra.to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2); // B ↔ R
    }
    encode_rgba_to_png(&rgba, w, h)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_dimension_override_wins_over_the_ceiling() {
        // (configured, max_image_dimension) -> max_dim
        for (configured, canonical, expected) in [
            // The per-call override wins outright, including 0 = native.
            (1568, Some(800), 800),
            (800, Some(1568), 1568),
            (1568, Some(0), 0),
            (0, Some(768), 768),
            // No per-call limit passes the ceiling through.
            (1600, None, 1600),
            (0, None, 0),
        ] {
            let limits = ImageDimensionLimits {
                configured,
                max_image_dimension: canonical,
            };
            assert_eq!(limits.resolve(), expected, "{limits:?}");
        }
    }

    #[test]
    fn crop_png_copies_exactly_the_rectangle() {
        // 4x3 image where each pixel encodes its own x and y.
        let mut rgba = Vec::new();
        for y in 0..3u8 {
            for x in 0..4u8 {
                rgba.extend_from_slice(&[x, y, 0, 255]);
            }
        }
        let png = encode_rgba_to_png(&rgba, 4, 3).unwrap();
        let rect = PixelRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let cropped = decode_png_to_rgba8(&crop_png(&png, rect).unwrap()).unwrap();
        assert_eq!(cropped.dimensions(), (2, 2));
        assert_eq!(cropped.get_pixel(0, 0).0, [1, 1, 0, 255]);
        assert_eq!(cropped.get_pixel(1, 1).0, [2, 2, 0, 255]);
    }

    #[test]
    fn crop_refuses_rectangles_outside_the_image() {
        let image = ImageBuffer::from_pixel(4, 3, image::Rgba([0u8, 0, 0, 255]));
        for rect in [
            PixelRect {
                x: 3,
                y: 0,
                width: 2,
                height: 1,
            },
            PixelRect {
                x: 0,
                y: 2,
                width: 1,
                height: 2,
            },
            PixelRect {
                x: 0,
                y: 0,
                width: 0,
                height: 1,
            },
            PixelRect {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 1,
            },
        ] {
            assert!(crop_rgba8(&image, rect).is_err(), "{rect:?}");
        }
        let whole = PixelRect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        assert_eq!(crop_rgba8(&image, whole).unwrap().dimensions(), (4, 3));
    }

    #[test]
    fn fit_long_edge_matches_resize_png_rounding() {
        let image = ImageBuffer::from_pixel(300, 100, image::Rgba([9u8, 9, 9, 255]));
        assert_eq!(
            fit_rgba8_long_edge(image.clone(), 0).dimensions(),
            (300, 100)
        );
        assert_eq!(
            fit_rgba8_long_edge(image.clone(), 300).dimensions(),
            (300, 100)
        );
        assert_eq!(
            fit_rgba8_long_edge(image.clone(), 200).dimensions(),
            (200, 67)
        );
        let png = encode_rgba_to_png(image.as_raw(), 300, 100).unwrap();
        let resized = resize_png_if_needed(&png, 200).unwrap();
        assert_eq!(png_dimensions(&resized).unwrap(), (200, 67));
    }

    /// Deterministic pixel noise with `channels` per pixel; alpha (the 4th
    /// channel) varies too, so it is resampled like the others.
    fn noise(w: u32, h: u32, channels: usize, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2_654_435_761).max(1);
        (0..w as usize * h as usize * channels)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                // Mix smooth gradients with hard edges, like a screenshot.
                if (i / channels) % 7 < 3 {
                    (i % 251) as u8
                } else {
                    (state >> 24) as u8
                }
            })
            .collect()
    }

    const RESIZE_CASES: [(u32, u32, u32, u32); 14] = [
        (64, 40, 54, 34),    // shrink, the window screenshot's ratio
        (37, 23, 13, 9),     // odd sizes, strong shrink
        (31, 17, 30, 16),    // shrink by one pixel
        (40, 30, 1, 1),      // to a single pixel
        (1, 1, 5, 3),        // from a single pixel
        (1, 40, 1, 7),       // one pixel wide
        (40, 1, 9, 1),       // one pixel high
        (9, 7, 9, 3),        // one axis only
        (5, 3, 17, 11),      // upscale
        (12, 50, 29, 23),    // upscale one axis, shrink the other
        (16, 16, 16, 16),    // same size
        (300, 200, 7, 5),    // very strong shrink
        (8, 8, 0, 4),        // empty result
        (1600, 10, 1356, 8), // the screenshot's width
    ];

    #[test]
    fn lanczos3_resize_matches_image_crate_byte_for_byte() {
        let filter = image::imageops::FilterType::Lanczos3;
        for (seed, (w, h, nw, nh)) in RESIZE_CASES.into_iter().enumerate() {
            let rgba: RgbaImage = ImageBuffer::from_raw(w, h, noise(w, h, 4, seed as u32)).unwrap();
            let expected = image::imageops::resize(&rgba, nw, nh, filter);
            let actual = lanczos3_resize_rgba8(&rgba, nw, nh);
            assert_eq!(
                actual.dimensions(),
                expected.dimensions(),
                "{w}x{h} -> {nw}x{nh}"
            );
            assert!(
                actual.as_raw() == expected.as_raw(),
                "RGBA {w}x{h} -> {nw}x{nh} differs from image::imageops::resize"
            );

            let rgb: RgbImage = ImageBuffer::from_raw(w, h, noise(w, h, 3, seed as u32)).unwrap();
            let expected = image::imageops::resize(&rgb, nw, nh, filter);
            let actual = lanczos3_resize_rgb8(&rgb, nw, nh);
            assert!(
                actual.as_raw() == expected.as_raw(),
                "RGB {w}x{h} -> {nw}x{nh} differs from image::imageops::resize"
            );
        }
        // A buffer longer than the image, which ImageBuffer allows.
        let mut long = noise(6, 4, 4, 99);
        long.extend_from_slice(&[7; 12]);
        let rgba: RgbaImage = ImageBuffer::from_raw(6, 4, long).unwrap();
        assert_eq!(
            lanczos3_resize_rgba8(&rgba, 4, 3),
            image::imageops::resize(&rgba, 4, 3, filter)
        );
    }

    #[test]
    fn lanczos3_resize_exact_matches_dynamic_image() {
        let rgba: RgbaImage = ImageBuffer::from_raw(23, 19, noise(23, 19, 4, 5)).unwrap();
        let gray = DynamicImage::ImageRgba8(rgba.clone()).to_luma8();
        for image in [
            DynamicImage::ImageRgba8(rgba.clone()),
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).to_rgb8()),
            DynamicImage::ImageLuma8(gray),
        ] {
            let expected = image.resize_exact(10, 13, image::imageops::FilterType::Lanczos3);
            assert_eq!(lanczos3_resize_exact(&image, 10, 13), expected);
        }
    }

    #[test]
    fn lanczos3_rounding_matches_f32_round() {
        let expected = |v: f32| v.clamp(0.0, 255.0).round() as u8;
        // Every eighth from -2 to 258 and the floats right around each.
        let mut v = -2.0f32;
        while v < 258.0 {
            let mut near = v;
            for _ in 0..4 {
                near = near.next_down();
            }
            for _ in 0..9 {
                assert_eq!(lanczos3_round_u8(near), expected(near), "{near}");
                near = near.next_up();
            }
            v += 0.125;
        }
        // A spread of other values, the tiny and huge ones included.
        let mut bits = 0x2545_f491u32;
        for _ in 0..100_000 {
            bits ^= bits << 13;
            bits ^= bits >> 17;
            bits ^= bits << 5;
            let v = f32::from_bits(bits);
            if v.is_finite() {
                assert_eq!(lanczos3_round_u8(v), expected(v), "{v}");
            }
        }
        assert_eq!(lanczos3_round_u8(0.499_999_97), 0);
        assert_eq!(lanczos3_round_u8(254.5), 255);
        assert_eq!(lanczos3_round_u8(-0.0), 0);
    }

    /// What `resize_png_if_needed` did before the plain-loop resize.
    fn resize_png_with_image_crate(png: &[u8], max_dim: u32) -> Vec<u8> {
        let (w, h) = png_dimensions(png).unwrap();
        if max_dim == 0 || (w <= max_dim && h <= max_dim) {
            return png.to_vec();
        }
        let scale = (max_dim as f64) / (w.max(h) as f64);
        let new_w = (w as f64 * scale).round() as u32;
        let new_h = (h as f64 * scale).round() as u32;
        let resized = image::load_from_memory_with_format(png, ImageFormat::Png)
            .unwrap()
            .resize(new_w, new_h, image::imageops::FilterType::Lanczos3);
        let mut out = Vec::new();
        resized
            .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn png_and_rgba_resizes_match_the_image_crate_path() {
        // (w, h, max_dim): 1000x333 at 100 comes out 99x33, not 100x33,
        // because DynamicImage::resize fits the rounded box again.
        for (w, h, max_dim) in [
            (160, 100, 136),
            (1000, 333, 100),
            (333, 1000, 100),
            (301, 7, 50),
            (2000, 1, 10),
            (40, 30, 0),
            (40, 30, 40),
        ] {
            let rgba = noise(w, h, 4, w ^ h);
            let png = encode_rgba_to_png(&rgba, w, h).unwrap();
            let expected = resize_png_with_image_crate(&png, max_dim);
            assert!(
                resize_png_if_needed(&png, max_dim).unwrap() == expected,
                "RGBA PNG {w}x{h} at {max_dim}"
            );
            let image = RgbaImage::from_raw(w, h, rgba).unwrap();
            let fitted = CapturedImage::Rgba(image).fit_long_edge(max_dim).unwrap();
            assert!(
                fitted.into_png().unwrap() == expected,
                "RGBA pixels {w}x{h} at {max_dim}"
            );

            let rgb = noise(w, h, 3, w ^ h);
            let mut rgb_png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut rgb_png)
                .write_image(&rgb, w, h, ExtendedColorType::Rgb8)
                .unwrap();
            assert!(
                resize_png_if_needed(&rgb_png, max_dim).unwrap()
                    == resize_png_with_image_crate(&rgb_png, max_dim),
                "RGB PNG {w}x{h} at {max_dim}"
            );
        }
        assert_eq!(png_fit_dimensions(1000, 333, 100), (99, 33));
    }

    #[test]
    fn resize_to_the_fitted_size_matches_fit_long_edge() {
        for (w, h, max_dim) in [
            (160, 100, 136),
            (1000, 333, 100),
            (333, 1000, 100),
            (301, 7, 50),
            (40, 30, 0),
            (40, 30, 40),
        ] {
            let (new_w, new_h) = fit_long_edge_dimensions(w, h, max_dim);

            let image = RgbaImage::from_raw(w, h, noise(w, h, 4, w ^ h)).unwrap();
            let rgba = CapturedImage::Rgba(image.clone());
            let fitted = rgba.clone().fit_long_edge(max_dim).unwrap();
            assert_eq!(fitted.dimensions().unwrap(), (new_w, new_h));
            assert_eq!(
                rgba.resize_to(new_w, new_h).unwrap(),
                fitted,
                "RGBA {w}x{h} at {max_dim}"
            );

            let png = CapturedImage::Png(encode_rgba_to_png(image.as_raw(), w, h).unwrap());
            assert_eq!(
                png.clone().resize_to(new_w, new_h).unwrap(),
                png.fit_long_edge(max_dim).unwrap(),
                "RGBA PNG {w}x{h} at {max_dim}"
            );

            let mut rgb_png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut rgb_png)
                .write_image(&noise(w, h, 3, w ^ h), w, h, ExtendedColorType::Rgb8)
                .unwrap();
            let rgb_png = CapturedImage::Png(rgb_png);
            assert_eq!(
                rgb_png.clone().resize_to(new_w, new_h).unwrap(),
                rgb_png.fit_long_edge(max_dim).unwrap(),
                "RGB PNG {w}x{h} at {max_dim}"
            );
        }
    }

    #[test]
    fn two_limits_resample_once_from_the_source() {
        // The second limit is applied to the first one's size, but the
        // pixels come from one pass over the original.
        let image = RgbaImage::from_raw(160, 100, noise(160, 100, 4, 5)).unwrap();
        let (w1, h1) = fit_long_edge_dimensions(160, 100, 150);
        let (w2, h2) = fit_long_edge_dimensions(w1, h1, 120);
        let once = CapturedImage::Rgba(image.clone())
            .resize_to(w2, h2)
            .unwrap();
        let twice = CapturedImage::Rgba(image.clone())
            .fit_long_edge(150)
            .unwrap()
            .fit_long_edge(120)
            .unwrap();
        assert_eq!(once.dimensions().unwrap(), twice.dimensions().unwrap());
        assert_eq!(
            once,
            CapturedImage::Rgba(lanczos3_resize_rgba8(&image, w2, h2))
        );
        assert_ne!(once, twice);
    }

    #[test]
    fn encode_rgba_to_png_matches_dynamic_image_write_to() {
        let rgba = noise(37, 21, 4, 3);
        let mut expected = Vec::new();
        DynamicImage::ImageRgba8(RgbaImage::from_raw(37, 21, rgba.clone()).unwrap())
            .write_to(&mut std::io::Cursor::new(&mut expected), ImageFormat::Png)
            .unwrap();
        assert_eq!(encode_rgba_to_png(&rgba, 37, 21).unwrap(), expected);
    }

    #[test]
    fn captured_rgba_matches_its_png() {
        let image = RgbaImage::from_raw(30, 20, noise(30, 20, 4, 8)).unwrap();
        let png = encode_rgba_to_png(image.as_raw(), 30, 20).unwrap();
        let from_png = CapturedImage::Png(png.clone());
        let from_rgba = CapturedImage::Rgba(image.clone());
        assert_eq!(from_rgba.dimensions().unwrap(), (30, 20));
        assert!(!from_rgba.is_empty() && !from_png.is_empty());
        assert!(CapturedImage::Png(Vec::new()).is_empty());

        let rect = PixelRect {
            x: 3,
            y: 4,
            width: 11,
            height: 9,
        };
        assert_eq!(
            from_rgba.clone().crop(rect).unwrap().into_png().unwrap(),
            crop_png(&png, rect).unwrap()
        );
        assert_eq!(
            from_png.clone().fit_long_edge(12).unwrap(),
            CapturedImage::Png(resize_png_if_needed(&png, 12).unwrap())
        );
        assert_eq!(from_png.clone().fit_long_edge(30).unwrap(), from_png);
        assert_eq!(
            from_rgba.clone().into_dynamic().unwrap(),
            from_png.clone().into_dynamic().unwrap()
        );
        assert_eq!(from_png.into_rgba8().unwrap(), image);
        let (encoded, pixels) = from_rgba.into_png_and_rgba().unwrap();
        assert_eq!(encoded, png);
        assert_eq!(pixels, Some(image));
    }

    #[test]
    fn png_dimensions_round_trip() {
        // Build a tiny 3x2 RGBA image then read its dimensions back.
        let rgba = vec![0xFFu8; 3 * 2 * 4];
        let png = encode_rgba_to_png(&rgba, 3, 2).expect("encode");
        let (w, h) = png_dimensions(&png).expect("dimensions");
        assert_eq!((w, h), (3, 2));
    }

    #[test]
    fn png_dimensions_rejects_non_png() {
        assert!(png_dimensions(&[0; 100]).is_err());
        assert!(png_dimensions(b"not a PNG file at all").is_err());
        assert!(png_dimensions(&[]).is_err());
    }

    #[test]
    fn resize_no_op_when_within_bound() {
        let rgba = vec![0u8; 10 * 10 * 4];
        let png = encode_rgba_to_png(&rgba, 10, 10).unwrap();
        let resized = resize_png_if_needed(&png, 100).unwrap();
        // Returned bytes should match the input exactly (no re-encode).
        assert_eq!(resized, png);
    }

    #[test]
    fn resize_no_op_when_max_dim_zero() {
        let rgba = vec![0u8; 100 * 100 * 4];
        let png = encode_rgba_to_png(&rgba, 100, 100).unwrap();
        let resized = resize_png_if_needed(&png, 0).unwrap();
        assert_eq!(resized, png);
    }

    #[test]
    fn resize_downscales_to_long_edge() {
        // 200x100 → max_dim=50 → 50x25 (long edge dictates).
        let rgba = vec![0u8; 200 * 100 * 4];
        let png = encode_rgba_to_png(&rgba, 200, 100).unwrap();
        let resized = resize_png_if_needed(&png, 50).unwrap();
        let (w, h) = png_dimensions(&resized).unwrap();
        assert_eq!(w, 50);
        assert_eq!(h, 25);
    }

    #[test]
    fn png_to_jpeg_strips_alpha() {
        let rgba = vec![0x80u8; 4 * 4 * 4];
        let png = encode_rgba_to_png(&rgba, 4, 4).unwrap();
        let jpeg = png_bytes_to_jpeg(&png, 80).unwrap();
        // JPEG file signature: FF D8 FF.
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn crosshair_returns_valid_png() {
        let rgba = vec![0u8; 40 * 40 * 4];
        let png = encode_rgba_to_png(&rgba, 40, 40).unwrap();
        let marked = crosshair_png_bytes(&png, 20.0, 20.0).unwrap();
        let (w, h) = png_dimensions(&marked).unwrap();
        assert_eq!((w, h), (40, 40));
    }

    #[test]
    fn crosshair_rejects_invalid_centers_instead_of_clamping_to_an_edge() {
        let png = encode_rgba_to_png(&vec![0u8; 40 * 40 * 4], 40, 40).unwrap();
        for point in [
            (-1.0, 20.0),
            (20.0, -1.0),
            (40.0, 20.0),
            (20.0, 40.0),
            (f64::NAN, 20.0),
            (20.0, f64::INFINITY),
        ] {
            assert!(crosshair_png_bytes(&png, point.0, point.1).is_err());
        }
        assert!(crosshair_png_bytes(&png, 0.0, 0.0).is_ok());
        assert!(crosshair_png_bytes(&png, 39.0, 39.0).is_ok());
    }

    #[test]
    fn bgra_to_png_swaps_channels() {
        // 1x1 BGRA pixel: B=10, G=20, R=30, A=40.
        let bgra = vec![10u8, 20, 30, 40];
        let png = encode_bgra_to_png(&bgra, 1, 1).unwrap();
        // Decode and check the RGBA bytes have R↔B swapped.
        let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(&png)).unwrap();
        let mut decoded = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut decoded).unwrap();
        assert_eq!(decoded, vec![30u8, 20, 10, 40]); // RGBA: R=30, G=20, B=10, A=40
    }
}
