use super::{capture_admission_error, normalize_desktop_capture_for_action_frame};
use pincer_core::image_utils::CapturedImage;

fn png(width: u32, height: u32) -> CapturedImage {
    let rgba = vec![0x7f; (width * height * 4) as usize];
    CapturedImage::Png(
        pincer_core::image_utils::encode_rgba_to_png(&rgba, width, height).expect("encode fixture"),
    )
}

#[test]
fn native_wayland_capture_is_resized_to_the_reported_action_frame() {
    let (normalized, width, height, scale) =
        normalize_desktop_capture_for_action_frame(png(3200, 2000), 1600, 1000)
            .expect("normalize 2x capture");

    assert_eq!((width, height), (1600, 1000));
    assert_eq!(normalized.dimensions().unwrap(), (1600, 1000));
    assert!(matches!(normalized, CapturedImage::Png(_)));
    assert_eq!(scale, 2.0);

    // Pixels at the action frame's size are passed through untouched.
    let pixels = CapturedImage::Rgba(image::RgbaImage::new(16, 10));
    let (same, width, height, scale) =
        normalize_desktop_capture_for_action_frame(pixels.clone(), 16, 10)
            .expect("normalize 1x capture");
    assert_eq!((same, width, height, scale), (pixels, 16, 10, 1.0));
}

#[test]
fn nonuniform_capture_mapping_fails_instead_of_distorting_coordinates() {
    let error = normalize_desktop_capture_for_action_frame(png(3200, 2000), 1600, 1200)
        .expect_err("nonuniform mapping must fail closed");
    assert!(error.to_string().contains("cannot be mapped uniformly"));
}

#[test]
fn capture_refusal_exposes_specific_code_and_refused_effect() {
    let refusal = capture_admission_error(anyhow::Error::new(
        pincer_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch,
    ));
    let structured = refusal.structured_content.unwrap();
    assert_eq!(structured["code"], "capture_frame_mismatch");
    assert_eq!(structured["effect"], "refused");
}
