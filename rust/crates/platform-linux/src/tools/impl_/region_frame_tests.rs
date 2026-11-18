//! Region and coordinate inputs are read off the image the caller was given
//! and mapped back to the target's own pixels.

use super::*;
use pincer_core::image_utils::PixelRect;

fn rect(x: u32, y: u32, width: u32, height: u32) -> PixelRect {
    PixelRect {
        x,
        y,
        width,
        height,
    }
}

fn frame(image: (u32, u32), shows: PixelRect) -> DesktopImageFrame {
    DesktopImageFrame {
        generation: 1,
        image,
        shows,
    }
}

#[test]
fn a_capped_desktop_image_maps_back_to_full_size_pixels() {
    let capped = frame((960, 540), rect(0, 0, 1920, 1080));
    assert_eq!(
        desktop_region_from_image(rect(100, 50, 200, 100), &capped).unwrap(),
        rect(200, 100, 400, 200)
    );
    // The whole image is the whole desktop.
    assert_eq!(
        desktop_region_from_image(rect(0, 0, 960, 540), &capped).unwrap(),
        rect(0, 0, 1920, 1080)
    );
    // An uncapped full capture is the identity.
    let full = frame((1920, 1080), rect(0, 0, 1920, 1080));
    assert_eq!(
        desktop_region_from_image(rect(7, 9, 11, 13), &full).unwrap(),
        rect(7, 9, 11, 13)
    );
}

#[test]
fn a_region_image_maps_through_its_offset_and_scale() {
    // A 400x300 image of the desktop rectangle 1000,500 800x600.
    let cropped = frame((400, 300), rect(1000, 500, 800, 600));
    assert_eq!(
        desktop_region_from_image(rect(10, 20, 100, 50), &cropped).unwrap(),
        rect(1020, 540, 200, 100)
    );
    assert_eq!(
        desktop_region_from_image(rect(0, 0, 400, 300), &cropped).unwrap(),
        rect(1000, 500, 800, 600)
    );
}

#[test]
fn rounding_stays_inside_what_the_image_shows() {
    // 1366/1000 is not a whole ratio; the far edge must not pass 1366.
    let odd = frame((1000, 562), rect(0, 0, 1366, 768));
    let mapped = desktop_region_from_image(rect(999, 561, 1, 1), &odd).unwrap();
    assert!(mapped.x + mapped.width <= 1366, "{mapped:?}");
    assert!(mapped.y + mapped.height <= 768, "{mapped:?}");
    assert!(mapped.width >= 1 && mapped.height >= 1);
}

#[test]
fn a_region_outside_the_image_is_refused_in_image_pixels() {
    let capped = frame((960, 540), rect(0, 0, 1920, 1080));
    let error = desktop_region_from_image(rect(900, 0, 100, 10), &capped).unwrap_err();
    assert!(error.contains("960x540"), "{error}");
    assert!(error.contains("native_pixels"), "{error}");
}

#[test]
fn window_regions_scale_to_native_pixels_and_clamp_to_the_window() {
    assert_eq!(
        window_region_from_screenshot(rect(100, 40, 200, 80), 1.25, Some((1920, 1080))),
        rect(125, 50, 250, 100)
    );
    // An edge-to-edge region whose scaled edge rounds past the window.
    let mapped = window_region_from_screenshot(rect(0, 0, 1568, 883), 1.2245, Some((1920, 1081)));
    assert_eq!(mapped, rect(0, 0, 1920, 1081));
    assert_eq!(
        window_region_from_screenshot(rect(3, 4, 5, 6), 1.0, None),
        rect(3, 4, 5, 6)
    );
}

#[test]
fn the_latest_desktop_image_of_the_session_sets_the_region_frame() {
    let state = ToolState::new();
    let session = json!({"_session_id": "region-frame-a"});
    // No image yet: the region is full-size desktop pixels.
    assert_eq!(
        state
            .desktop_region_arg(&session, rect(10, 10, 20, 20))
            .unwrap(),
        (rect(10, 10, 20, 20), false)
    );
    state.record_desktop_image(&session, (960, 540), rect(0, 0, 1920, 1080));
    assert_eq!(
        state
            .desktop_region_arg(&session, rect(10, 10, 20, 20))
            .unwrap(),
        (rect(20, 20, 40, 40), true)
    );
    // native_pixels keeps the old frame.
    let native = json!({"_session_id": "region-frame-a", "native_pixels": true});
    assert_eq!(
        state.desktop_region_arg(&native, rect(10, 10, 20, 20)).unwrap(),
        (rect(10, 10, 20, 20), false)
    );
    // Another session never read that image.
    let other = json!({"_session_id": "region-frame-b"});
    assert_eq!(
        state.desktop_region_arg(&other, rect(10, 10, 20, 20)).unwrap(),
        (rect(10, 10, 20, 20), false)
    );
    // An ended session's image does not carry over to its next generation.
    state.capture_service.retire_session_id("region-frame-a");
    assert_eq!(
        state
            .desktop_region_arg(&session, rect(10, 10, 20, 20))
            .unwrap(),
        (rect(10, 10, 20, 20), false)
    );
}

#[test]
fn parallel_drag_points_scale_from_screenshot_pixels() {
    assert_eq!(
        scale_points(vec![(10.0, 20.0), (30.5, 4.0)], 2.0),
        [(20.0, 40.0), (61.0, 8.0)]
    );
    let state = ToolState::new();
    let native = json!({"_session_id": "drag-frame", "native_pixels": true});
    assert_eq!(parallel_drag_scale(&state, &native, 77).unwrap(), 1.0);
    // Without a screenshot of the window there is no frame to map from.
    let refusal = parallel_drag_scale(&state, &json!({"_session_id": "drag-frame"}), 77)
        .expect_err("no screenshot context");
    assert_eq!(
        refusal.structured_content.as_ref().unwrap()["code"],
        "screenshot_context_missing"
    );
}

/// The three inputs that used to take native pixels by default now offer
/// them only behind `native_pixels`.
#[test]
fn native_pixel_inputs_need_the_flag() {
    let state = ToolState::new();
    let desktop = GetDesktopStateTool {
        state: state.clone(),
    };
    let series = CaptureSeriesTool {
        state: state.clone(),
    };
    let parallel = ParallelMouseDragTool { state };
    for def in [desktop.def(), series.def(), parallel.def()] {
        let flag = &def.input_schema["properties"]["native_pixels"];
        assert_eq!(flag["type"], "boolean", "{}", def.name);
    }
    for def in [desktop.def(), series.def()] {
        let region = def.input_schema["properties"]["region"]["description"]
            .as_str()
            .unwrap();
        assert!(region.contains("latest get_desktop_state image"), "{region}");
    }
    let from_x = parallel.def().input_schema["properties"]["drags"]["items"]["properties"]
        ["from_x"]["description"]
        .as_str()
        .unwrap();
    assert!(from_x.contains("screenshot pixels"), "{from_x}");
}
