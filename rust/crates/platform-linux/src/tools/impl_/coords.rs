//! Coordinate frames: desktop and window pixels, screenshot regions, element
//! centers, and the point resolver of the pointer tools.

use super::*;

/// `native_pixels:true`: coordinates or a region are in the target's own
/// pixels (full-size desktop pixels, or the window's native pixels), not in
/// the screenshot the caller was given.
pub(super) fn native_pixels_requested(args: &Value) -> bool {
    args.get("native_pixels").and_then(Value::as_bool) == Some(true)
}

/// Schema of the `native_pixels` flag.
pub(super) fn native_pixels_schema(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

/// Map a region read off a desktop image back to full-size desktop pixels,
/// rounded to whole pixels and kept inside what the image shows.
pub(super) fn desktop_region_from_image(
    region: pincer_core::image_utils::PixelRect,
    frame: &DesktopImageFrame,
) -> Result<pincer_core::image_utils::PixelRect, String> {
    let (image_w, image_h) = frame.image;
    if !region.fits_within(image_w, image_h) {
        return Err(format!(
            "region {},{} {}x{} does not fit the {image_w}x{image_h} get_desktop_state image \
             it is read from (needs x+width <= {image_w} and y+height <= {image_h}); pass \
             native_pixels:true for full-size desktop pixels",
            region.x, region.y, region.width, region.height
        ));
    }
    let shows = frame.shows;
    let axis = |start: u32, length: u32, image: u32, shows_start: u32, shows_len: u32| {
        let scale = f64::from(shows_len) / f64::from(image);
        let lo = (f64::from(shows_start) + f64::from(start) * scale).round() as u32;
        let hi = (f64::from(shows_start) + f64::from(start + length) * scale).round() as u32;
        let lo = lo.clamp(shows_start, shows_start + shows_len - 1);
        let hi = hi.clamp(lo + 1, shows_start + shows_len);
        (lo, hi - lo)
    };
    let (x, width) = axis(region.x, region.width, image_w, shows.x, shows.width);
    let (y, height) = axis(region.y, region.height, image_h, shows.y, shows.height);
    Ok(pincer_core::image_utils::PixelRect {
        x,
        y,
        width,
        height,
    })
}

/// Map a region read off a window's get_window_state screenshot to its
/// native pixels (`scale` native pixels per screenshot pixel), rounded to
/// whole pixels and kept inside `native` when that size is known.
pub(super) fn window_region_from_screenshot(
    region: pincer_core::image_utils::PixelRect,
    scale: f64,
    native: Option<(u32, u32)>,
) -> pincer_core::image_utils::PixelRect {
    let axis = |start: u32, length: u32, limit: Option<u32>| {
        let mut lo = (f64::from(start) * scale).round().max(0.0) as u32;
        let mut hi = (f64::from(start + length) * scale).round().max(0.0) as u32;
        if let Some(limit) = limit.filter(|limit| *limit > 0) {
            lo = lo.min(limit - 1);
            hi = hi.min(limit);
        }
        (lo, hi.max(lo + 1) - lo)
    };
    let (x, width) = axis(region.x, region.width, native.map(|size| size.0));
    let (y, height) = axis(region.y, region.height, native.map(|size| size.1));
    pincer_core::image_utils::PixelRect {
        x,
        y,
        width,
        height,
    }
}
