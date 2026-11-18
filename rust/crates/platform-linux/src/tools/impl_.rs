//! Real Linux tool implementations (compiled only on Linux).

use async_trait::async_trait;
use pincer_contract::{
    ClickButton, DragInput, GetCursorPositionInput, GetDesktopStateInput, GetScreenSizeInput,
    HotkeyInput, InvokeMenuInput, MoveCursorInput, PressKeyInput, ScrollInput, TypeTextInput,
};
use pincer_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
    tool_args::{parse_legacy_click_input, parse_typed_input, parse_typed_projection, ArgsExt},
    window_target::{PidOnlyWindowTargetGuard, WindowTargetCandidate, WindowTargetCandidates},
};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::atspi::Snapshots;
use crate::input::{
    quote_untrusted, quoted_title, untrusted_token, SHOWN_COMMAND_CHARS, SHOWN_NAME_CHARS,
};
use crate::input::modifier::is_modifier;
use cursor_overlay::CursorRegistry;

mod click;
mod click_gestures;
mod coords;
mod cursor_reveal;
mod discovery;
mod drag;
mod held_button;
mod hyprland_route;
mod input_common;
mod keyboard_route;
mod keys;
mod launch;
mod pointer_route;
mod scroll;
mod set_value;
mod state;
mod text;
mod toolkit;
mod window_ops;
mod window_state;
mod window_target;

use self::click::*;
use self::click_gestures::*;
use self::coords::*;
use self::cursor_reveal::*;
use self::discovery::*;
use self::drag::*;
use self::held_button::*;
use self::hyprland_route::*;
use self::input_common::*;
use self::keyboard_route::*;
use self::keys::*;
use self::launch::*;
use self::pointer_route::*;
use self::scroll::*;
use self::set_value::*;
use self::state::*;
use self::text::*;
use self::toolkit::*;
use self::window_ops::*;
use self::window_state::*;
use self::window_target::*;

#[cfg(test)]
mod pid_window_resolver_tests;

use pincer_core::snapshot_store::ZoomContext;

#[cfg(test)]
mod region_frame_tests;

#[cfg(test)]
mod list_windows_tests;

#[cfg(test)]
mod untrusted_text_tests;

#[cfg(test)]
mod megapixel_cap_tests;

#[cfg(test)]
mod get_window_state_actions_tests;

mod appimage;
mod launch_child;
use launch_child::{LaunchExit, LaunchedChild};

#[cfg(test)]
mod launch_app_tests;
#[cfg(test)]
mod launch_scope_tests;

#[cfg(test)]
mod launch_resolution_tests;

#[cfg(test)]
mod pointer_argument_tests;

#[cfg(test)]
mod held_button_live_tests;

#[cfg(test)]
mod type_text_readback_tests;

#[cfg(test)]
mod click_delegation_tests;

#[cfg(test)]
mod press_key_tests;

#[cfg(test)]
mod set_value_tests;

// ── get_screen_size ───────────────────────────────────────────────────────────

pub struct GetScreenSizeTool;
static GSS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetScreenSizeTool {
    fn def(&self) -> &ToolDef {
        GSS_DEF.get_or_init(|| ToolDef {
            name: "get_screen_size".into(),
            description: "Return the desktop size and a scale factor. On X11 this is the \
                pixel size of the whole X screen (every monitor) with scale_factor 1.0; on \
                Hyprland it is the logical desktop frame of the powered monitors with the \
                output scale, plus a per-monitor list."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema()
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if let Err(result) = parse_typed_input::<GetScreenSizeInput>("get_screen_size", args) {
            return result;
        }
        let result = tokio::task::spawn_blocking(|| {
            if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
                // Shared manifest admission needs content-free display
                // metadata even when this native desktop has no X11 DISPLAY.
                // The adapter attests the IPC/Wayland compositor peer and
                // refuses layouts its logical frame cannot represent: no
                // powered output, or a rotated powered output.
                // Scaled outputs report logical pixels plus the output scale.
                // The size and the monitor list come from one snapshot.
                let (frame, monitors) = crate::wayland::hyprland::screen_report()?;
                return Ok((frame.width, frame.height, frame.scale, Some(monitors)));
            }
            // X11 reports pixel dimensions; scale factor on X11 is not
            // well-defined per-monitor, so report 1.0 (matches DPI-unaware
            // assumption). Other Wayland compositors retain their existing
            // limitation; do not infer native metadata support there.
            let (w, h) = x11_screen_size()?;
            Ok::<(u32, u32, f64, Option<Value>), anyhow::Error>((w, h, 1.0, None))
        })
        .await;
        match result {
            Ok(Ok((w, h, scale, Some(monitors)))) => {
                // Tell the agent which monitors are actually on: the desktop
                // frame covers powered monitors only and changes as they turn
                // on or off, so re-read it after any display change.
                let describe = |m: &Value| {
                    let name = m["name"].as_str().unwrap_or("?");
                    match (
                        m["powered"].as_bool(),
                        m["frame_x"].as_i64(),
                        m["frame_y"].as_i64(),
                    ) {
                        (Some(true), Some(x), Some(y)) => format!(
                            "{name} {}x{} on, at ({x},{y}) in the desktop frame",
                            m["width"], m["height"]
                        ),
                        _ => format!("{name} off (standby), not in the desktop frame"),
                    }
                };
                let lines: Vec<String> = monitors
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(describe)
                    .collect();
                ToolResult::text(format!(
                    "✅ Desktop frame: {w}x{h} logical pixels @ {scale}x, spanning every monitor that is on.\n{}",
                    lines.join("\n")
                ))
                .with_structured(json!({
                    "width": w, "height": h, "scale_factor": scale, "monitors": monitors,
                }))
            }
            // The Windows wording: X11 sizes are pixels, not macOS points.
            Ok(Ok((w, h, scale, None))) => ToolResult::text(format!(
                "✅ Main display: {w}x{h} pixels @ {scale}x (the whole X screen, every monitor)"
            ))
            .with_structured(json!({ "width": w, "height": h, "scale_factor": scale })),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Read the true X11 root-window size in pixels: (width, height).
/// Shared by `get_screen_size` and `get_desktop_state`.
fn x11_screen_size() -> anyhow::Result<(u32, u32)> {
    use x11rb::connection::Connection;
    use x11rb::rust_connection::RustConnection;
    let (conn, screen_num) = RustConnection::connect(None)
        .map_err(|e| anyhow::anyhow!("{e}{}", crate::no_display_hint()))?;
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];
    let w = screen.width_in_pixels as u32;
    let h = screen.height_in_pixels as u32;
    // WSLg / headless XWayland quirk: the X server connects but the
    // root screen advertises a 0-px geometry until a real output is
    // attached. Returning {width:0,height:0} here would propagate a
    // success with zero dimensions to the client, which then either
    // divides by zero when scaling or feeds the value into `int(...)`
    // after the missing key collapses to None. Fail loudly with an
    // actionable, typed error instead (never emit a 0/null where the
    // client expects a usable int). See issue #2005.
    if w == 0 || h == 0 {
        anyhow::bail!(
            "X11 connected but reports a 0x0 root screen — no usable \
             display geometry.{}",
            crate::no_display_hint()
        );
    }
    Ok((w, h))
}

/// Put the desktop image in the exact coordinate frame consumed by desktop
/// actions. Native Wayland capture buffers may use backing pixels while the
/// compositor's pointer protocol and reported screen geometry use logical
/// pixels. Returning the backing image alongside logical dimensions violates
/// the screenshot-to-action contract and makes every vision-grounded action
/// miss by the output scale.
fn normalize_desktop_capture_for_action_frame(
    image: pincer_core::image_utils::CapturedImage,
    action_width: u32,
    action_height: u32,
) -> anyhow::Result<(pincer_core::image_utils::CapturedImage, u32, u32, f64)> {
    if action_width == 0 || action_height == 0 {
        anyhow::bail!("desktop action frame is empty: {action_width}x{action_height}");
    }

    let (capture_width, capture_height) = image.dimensions()?;
    let scale_x = f64::from(capture_width) / f64::from(action_width);
    let scale_y = f64::from(capture_height) / f64::from(action_height);
    if (scale_x - scale_y).abs() > 0.01 {
        anyhow::bail!(
            "desktop capture {capture_width}x{capture_height} cannot be mapped uniformly to \
             action frame {action_width}x{action_height} (scale {scale_x:.4}x{scale_y:.4})"
        );
    }

    if capture_width == action_width && capture_height == action_height {
        return Ok((image, action_width, action_height, 1.0));
    }

    let png = image.into_png()?;
    let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?;
    let resized = image.resize_exact(
        action_width,
        action_height,
        image::imageops::FilterType::Lanczos3,
    );
    let mut encoded = std::io::Cursor::new(Vec::new());
    resized.write_to(&mut encoded, image::ImageFormat::Png)?;
    Ok((
        pincer_core::image_utils::CapturedImage::Png(encoded.into_inner()),
        action_width,
        action_height,
        scale_x,
    ))
}

/// One full-display capture in the desktop action coordinate frame. Shared by
/// `get_desktop_state` and `capture_series`.
struct DesktopActionFrame {
    image: pincer_core::image_utils::CapturedImage,
    width: u32,
    height: u32,
    /// Capture pixels per action-frame pixel before normalization.
    scale_factor: f64,
    overlay_capture: cursor_overlay::capture_exclusion::AgentOverlayCapture,
    /// The Hyprland layout the image was composed from.
    hyprland_frame: Option<crate::wayland::hyprland::DesktopFrame>,
}

fn capture_desktop_action_frame() -> anyhow::Result<DesktopActionFrame> {
    // Capture the full display at native size first. When the
    // compositor consumes logical input coordinates, normalize the
    // image below so screenshot pixels still land exactly.
    //
    // The agent reads this image, so the Driver's own cursor and
    // session pill are hidden around the grab (or the limitation is
    // reported) instead of being baked over the controls it reads.
    // Hyprland: read the desktop frame once. The screenshot, its action
    // size and the window geometry below all use it.
    let hyprland_plan = if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        Some(crate::wayland::hyprland::desktop_capture_plan()?)
    } else {
        None
    };
    let (native_image, overlay_capture) =
        cursor_overlay::capture_exclusion::capture_excluding_overlays(
            &crate::overlay_capture::OverlayExcluder,
            |hidden| {
                // A multi-monitor layout is composed from the frame
                // read above, so the image is exactly that frame.
                let image = match &hyprland_plan {
                    Some(plan) if plan.composite => pincer_core::image_utils::CapturedImage::Png(
                        crate::wayland::hyprland::compose_desktop_capture(&plan.frame)?,
                    ),
                    _ => crate::capture::screenshot_display_image()?,
                };
                Ok::<_, anyhow::Error>(crate::overlay_capture::verify_hidden_image_at(
                    image,
                    hidden,
                    (0, 0),
                ))
            },
        )?;
    let (native_w, native_h) = native_image.dimensions()?;
    // True screen size. On a pure-Wayland session (native backend
    // opted in, no X11 DISPLAY) the capture above came from the
    // wlroots `zwlr_screencopy` cascade, whose full-display buffer is
    // the whole output at native (physical) pixels, so the PNG
    // dimensions ARE the true screen size. Querying the X11 root
    // window here would fail with "$DISPLAY variable not set" and
    // abort the tool even though the screenshot already succeeded.
    // Only fall back to the X11 root-window geometry off Wayland, so
    // the X11 / XWayland path is unchanged. See #2017 / Sway testing.
    let hyprland_frame = hyprland_plan.map(|plan| plan.frame);
    let (screen_w, screen_h) = if let Some(frame) = &hyprland_frame {
        (frame.width, frame.height)
    } else if crate::wayland::is_wayland() {
        crate::capture_action_frame::desktop_action_dimensions((native_w, native_h))?
    } else {
        x11_screen_size()?
    };
    let (image, width, height, scale_factor) =
        normalize_desktop_capture_for_action_frame(native_image, screen_w, screen_h)?;
    Ok(DesktopActionFrame {
        image,
        width,
        height,
        scale_factor,
        overlay_capture,
        hyprland_frame,
    })
}

// ── get_desktop_state ─────────────────────────────────────────────────────────

pub struct GetDesktopStateTool {
    state: Arc<ToolState>,
}
static GDS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetDesktopStateTool {
    fn def(&self) -> &ToolDef {
        GDS_DEF.get_or_init(|| ToolDef {
            name: "get_desktop_state".into(),
            description: "Capture the full display in the desktop action coordinate frame. \
                Use the returned PNG directly as the coordinate source for actions whose target is \
                {kind:\"desktop\",display_id:\"primary\"}. No AT-SPI walk. Also lists the \
                visible windows with pid, window_id, z_index and geometry, labeled \
                `screenshot px` (this image) and `desktop px` (list_windows and set_window_frame). The \
                agent cursor is hidden for the capture; `agent_overlay_capture` says how, and \
                `excluded` means it is not in the image.".into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "screenshot_out_file":{"type":"string","description":"Write PNG here instead of base64."},
                "max_image_dimension": pincer_core::tool_schema::desktop_max_image_dimension_schema(),
                "region": pincer_core::capture_series::region_schema(
                    "Optional crop, in pixels of this session's latest get_desktop_state image (mapped back to the desktop for you, through its frame_scale and region; full-size desktop pixels when there is no earlier image). Must lie inside that image. Applied before max_image_dimension."
                ),
                "native_pixels": native_pixels_schema("true: region is in full-size desktop pixels (list_windows bounds; physical screen pixels on X11) instead. Default false.")
            },"additionalProperties":false}),
            read_only: true, destructive: false, idempotent: false, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // `region` is a Linux addition outside the portable contract type.
        let input = match parse_typed_projection::<GetDesktopStateInput>("get_desktop_state", &args)
        {
            Ok(input) => input,
            Err(result) => return result,
        };
        let invalid = |message: String| {
            ToolResult::error(format!("get_desktop_state: invalid arguments: {message}"))
                .with_structured(json!({
                    "code": "invalid_arguments",
                    "tool": "get_desktop_state",
                    "detail": message,
                }))
        };
        let requested_region = match pincer_core::capture_series::parse_region(args.get("region"))
        {
            Ok(region) => region,
            Err(message) => return invalid(message),
        };
        // The region is read off the image the caller was given; crop in
        // full-size desktop pixels.
        let (region, mapped_from) = match requested_region {
            None => (None, None),
            Some(requested) => match self.state.desktop_region_arg(&args, requested) {
                Ok((region, mapped)) => (Some(region), mapped.then_some(requested)),
                Err(message) => return invalid(message),
            },
        };
        let record_args = args.clone();
        let capture_args = args;
        let out_file = input.screenshot_out_file;
        let max_image_dimension = input.max_image_dimension;
        let capture_service = self.state.capture_service.clone();

        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let DesktopActionFrame {
                image,
                width: screen_w,
                height: screen_h,
                scale_factor,
                overlay_capture,
                hyprland_frame,
            } = capture_desktop_action_frame()?;
            // Optional crop, in action-frame pixels, before the cap.
            let (image, shot_w, shot_h) = match region {
                Some(region) => {
                    pincer_core::capture_series::check_region(region, (screen_w, screen_h))
                        .map_err(|message| anyhow::anyhow!(message))?;
                    (image.crop(region)?, region.width, region.height)
                }
                None => (image, screen_w, screen_h),
            };
            // Opt-in long-edge cap: the full-size capture is the desktop
            // action frame; a capped image is mapped back at dispatch
            // (pincer_core::desktop_capture_scale) and by its capture_id.
            let (image, shot_w, shot_h) = match max_image_dimension.filter(|cap| *cap > 0) {
                Some(cap) if shot_w.max(shot_h) > cap => {
                    let image = image.fit_long_edge(cap)?;
                    let (w, h) = image.dimensions()?;
                    (image, w, h)
                }
                _ => (image, shot_w, shot_h),
            };
            let (png, encoded_from) = image.into_png_and_rgba()?;
            // Optional: write PNG to disk instead of returning base64.
            let written = if let Some(path) = out_file.as_deref() {
                std::fs::write(path, &png)?;
                Some(path.to_string())
            } else {
                None
            };
            use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
            let b64 = if written.is_some() {
                None
            } else {
                Some(B64.encode(&png))
            };
            // Which pid owns which window is what the agent needs before its
            // first click; without it the first action guesses `pid: 1`.
            let mut windows = crate::wayland::list_windows_dispatch(None);
            windows
                .retain(|w| w.is_on_screen && w.pid.is_some_and(crate::proc_fs::is_process_live));
            if let Some(frame) = &hyprland_frame {
                // Window geometry is in layout coordinates; screenshot pixel
                // (0, 0) is the frame origin. Refuse rather than pair this
                // capture with a layout that changed meanwhile.
                anyhow::ensure!(
                    crate::wayland::hyprland::desktop_frame()? == *frame,
                    "the Hyprland monitor layout changed during get_desktop_state; call it again"
                );
                frame.rebase_windows(&mut windows);
            }
            let capture_id = match region {
                Some(region) => crate::capture_action_frame::publish_desktop_region(
                    &capture_service,
                    &capture_args,
                    &png,
                    encoded_from.as_ref(),
                    (shot_w, shot_h),
                    (screen_w, screen_h),
                    region,
                )?,
                None => crate::capture_action_frame::publish_desktop(
                    &capture_service,
                    &capture_args,
                    &png,
                    encoded_from.as_ref(),
                    (shot_w, shot_h),
                    (screen_w, screen_h),
                )?,
            };
            Ok((
                b64,
                shot_w,
                shot_h,
                screen_w,
                screen_h,
                scale_factor,
                written,
                windows,
                capture_id,
                overlay_capture,
            ))
        })
        .await;

        match result {
            Ok(Ok((
                b64_opt,
                shot_w,
                shot_h,
                screen_w,
                screen_h,
                scale_factor,
                written,
                windows,
                capture_id,
                overlay_capture,
            ))) => {
                self.state.record_desktop_image(
                    &record_args,
                    (shot_w, shot_h),
                    region.unwrap_or(pincer_core::image_utils::PixelRect {
                        x: 0,
                        y: 0,
                        width: screen_w,
                        height: screen_h,
                    }),
                );
                // Desktop pixels per screenshot pixel: the crop's width when a
                // region was taken, the whole screen's otherwise.
                let source_w = region.map_or(screen_w, |region| region.width);
                let frame_scale = if shot_w > 0 {
                    f64::from(source_w) / f64::from(shot_w)
                } else {
                    1.0
                };
                let mut content = Vec::new();
                let mut structured = json!({
                    "platform": "linux",
                    "display": "primary",
                    "screenshot_width": shot_w,
                    "screenshot_height": shot_h,
                    "screen_width": screen_w,
                    "screen_height": screen_h,
                    "scale_factor": scale_factor,
                    "frame_scale": frame_scale,
                    "screenshot_mime_type": "image/png",
                    "windows": windows.iter().map(window_record_json).collect::<Vec<_>>(),
                    "capture_id": capture_id,
                    "agent_overlay_capture": overlay_capture,
                });
                if let Some(region) = region {
                    structured["region"] = json!({
                        "x": region.x, "y": region.y,
                        "width": region.width, "height": region.height,
                    });
                    if let Some(requested) = mapped_from {
                        structured["region_requested"] = json!({
                            "x": requested.x, "y": requested.y,
                            "width": requested.width, "height": requested.height,
                            "frame": "previous_screenshot",
                        });
                    }
                } else if (frame_scale - 1.0).abs() > 0.001 {
                    // Capped: the uncapped capture is the action frame.
                    structured["screenshot_original_width"] = json!(screen_w);
                    structured["screenshot_original_height"] = json!(screen_h);
                }
                if let Some(b64) = b64_opt {
                    content.push(pincer_core::protocol::Content::image_png(b64));
                }
                // A cropped image is not the desktop frame, so window
                // geometry stays in full-size desktop pixels.
                let window_lines = desktop_window_lines(
                    &windows,
                    if region.is_some() { 1.0 } else { frame_scale },
                );
                let mut frame_note = if let Some(region) = region {
                    let from = mapped_from.map_or_else(String::new, |requested| {
                        format!(
                            " (your {},{} {}x{} in the previous screenshot)",
                            requested.x, requested.y, requested.width, requested.height
                        )
                    });
                    format!(
                        "; region {},{} {}x{} of the desktop{from}: screenshot pixel (px,py) is \
                         desktop ({} + px*{frame_scale:.2}, {} + py*{frame_scale:.2}); window \
                         positions below are full-size desktop pixels",
                        region.x, region.y, region.width, region.height, region.x, region.y
                    )
                } else if (frame_scale - 1.0).abs() > 0.001 {
                    format!(
                        "; x/y for scope:\"desktop\" actions are pixels of THIS screenshot \
                         (mapped ×{frame_scale:.2} back to the screen automatically)"
                    )
                } else {
                    String::new()
                };
                cursor_overlay::capture_exclusion::append_summary_note(
                    &mut frame_note,
                    &overlay_capture,
                );
                if let Some(path) = written {
                    structured["screenshot_file_path"] = json!(path);
                    content.push(pincer_core::protocol::Content::text(format!(
                        "✅ Desktop screenshot {shot_w}x{shot_h} written to {path} (screen {screen_w}x{screen_h}{frame_note}){window_lines}"
                    )));
                } else {
                    content.push(pincer_core::protocol::Content::text(format!(
                        "✅ Desktop screenshot {shot_w}x{shot_h} (screen {screen_w}x{screen_h}{frame_note}){window_lines}"
                    )));
                }
                ToolResult {
                    content,
                    is_error: None,
                    structured_content: Some(structured),
                    action_record: None,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Capture error: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Text block naming every visible window with the `pid` / `window_id` pair
/// that every other tool takes, so the model's first action targets a real
/// process instead of guessing. Each line names its unit: the geometry in
/// this screenshot's pixels when they differ from the desktop's, then in
/// desktop pixels as the structured `windows` records hold it, and the
/// z_index, so the line says the whole record. Titles and app names are
/// app text, escaped and quoted as in `window_record_line`.
fn desktop_window_lines(windows: &[crate::x11::WindowInfo], frame_scale: f64) -> String {
    let px = |v: i32| (f64::from(v) / frame_scale).round() as i32;
    let sz = |v: u32| (f64::from(v) / frame_scale).round() as u32;
    if windows.is_empty() {
        return "\nVisible windows: none (use list_windows / launch_app).".to_owned();
    }
    let scaled = (frame_scale - 1.0).abs() > 0.001;
    let mut out =
        String::from("\nVisible windows (use these pid + window_id values in every tool call):");
    for w in windows {
        let title = if w.title.is_empty() {
            "(no title)".to_owned()
        } else {
            quoted_title(&w.title)
        };
        let desktop = format!("{}x{}+{}+{} desktop px", w.width, w.height, w.x, w.y);
        let geometry = if scaled {
            format!(
                "{}x{} at ({},{}) screenshot px, {desktop}",
                sz(w.width),
                sz(w.height),
                px(w.x),
                px(w.y)
            )
        } else {
            desktop
        };
        out.push_str(&format!(
            "\n- pid={} window_id={} {} {geometry} z={}{}{}",
            w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            w.xid,
            title,
            w.z_index
                .map(|z| z.to_string())
                .unwrap_or_else(|| "?".into()),
            if w.app_name.is_empty() {
                String::new()
            } else {
                format!(" app={}", untrusted_token(&w.app_name, SHOWN_NAME_CHARS))
            },
            if w.is_on_screen {
                ""
            } else {
                " is_on_screen=false"
            },
        ));
    }
    out.push_str("\n→ get_window_state(pid, window_id) lists clickable elements; click/type_text take the same pid (+ window_id for x,y).");
    out
}

// ── get_cursor_position ───────────────────────────────────────────────────────

pub struct GetCursorPositionTool;
static GCP_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetCursorPositionTool {
    fn def(&self) -> &ToolDef {
        GCP_DEF.get_or_init(|| ToolDef {
            name: "get_cursor_position".into(),
            description:
                "Return the pointer position in desktop pixels (origin top-left). On Wayland \
                 compositors other than Hyprland it is only the last position this process moved \
                 the pointer to (`source: \"synthetic\"`)."
                    .into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema()
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if let Err(result) =
            parse_typed_input::<GetCursorPositionInput>("get_cursor_position", args)
        {
            return result;
        }
        // Hyprland reports the real pointer over IPC; return it in the
        // desktop-frame coordinates that get_desktop_state and desktop actions use.
        if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
            let position = tokio::task::spawn_blocking(|| -> anyhow::Result<(i32, i32)> {
                let (x, y) = crate::wayland::hyprland::cursor_position()?;
                Ok(crate::wayland::hyprland::desktop_frame()?
                    .from_layout(x.round() as i32, y.round() as i32))
            })
            .await;
            // The synthetic registry below holds layout coordinates, not the
            // desktop frame, so a failed compositor query is an error here.
            return match position {
                Ok(Ok((x, y))) => {
                    ToolResult::text(format!("✅ Cursor at ({x}, {y}) in the desktop frame"))
                        .with_structured(json!({ "x": x, "y": y, "source": "compositor" }))
                }
                Ok(Err(e)) => ToolResult::error(format!(
                    "Could not read the Hyprland pointer position: {e:#}"
                )),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }
        // Native Wayland: there's no protocol for clients to query the real
        // global cursor position. Fall back to the synthetic registry that
        // records every `motion_absolute` this process emits.
        if crate::wayland::is_wayland() {
            return match crate::wayland::last_synth_cursor_pos() {
                Some((x, y)) => ToolResult::text(
                    format!("✅ Cursor at ({x}, {y}) (synthetic — last move_cursor in this process)")
                ).with_structured(json!({
                    "x": x, "y": y, "source": "synthetic"
                })),
                None => ToolResult::text(
                    "Cursor position unknown on Wayland — no move_cursor has been issued in this process yet.".to_string()
                ).with_structured(json!({ "source": "synthetic", "available": false })),
            };
        }
        let result = tokio::task::spawn_blocking(|| {
            use x11rb::connection::Connection;
            use x11rb::protocol::xproto::ConnectionExt as _;
            let (conn, screen_num) = crate::x11::query_connection()?;
            let root = conn.setup().roots[screen_num].root;
            let reply = conn.query_pointer(root)?.reply()?;
            Ok::<(i32, i32), anyhow::Error>((reply.root_x as i32, reply.root_y as i32))
        })
        .await;
        match result {
            // Text format matches Swift `GetCursorPositionTool` 1:1.
            Ok(Ok((x, y))) => ToolResult::text(format!("✅ Cursor at ({x}, {y})"))
                .with_structured(json!({ "x": x, "y": y, "source": "x11" })),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── move_cursor ───────────────────────────────────────────────────────────────

pub struct MoveCursorTool {
    state: Arc<ToolState>,
}

static MCURSOR_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorControlScope {
    Agent,
    Desktop,
}

fn cursor_control_scope(args: &Value) -> CursorControlScope {
    match args.get("scope").and_then(Value::as_str) {
        Some("desktop") => CursorControlScope::Desktop,
        _ => CursorControlScope::Agent,
    }
}

#[async_trait]
impl Tool for MoveCursorTool {
    fn def(&self) -> &ToolDef {
        MCURSOR_DEF.get_or_init(|| ToolDef {
            name: "move_cursor".into(),
            description: "Move the synthetic agent cursor without changing the user's pointer. Only an explicit scope=desktop request moves the real OS pointer in get_desktop_state coordinates.".into(),
            input_schema: json!({"type":"object","required":["x","y"],"properties":{
                "x":{"type":"number","description":"Destination X in screen pixels (list_windows bounds, full-size get_desktop_state pixels) in both scopes. Unlike click, drag and the other window pointer tools, move_cursor never takes window screenshot pixels, and a window `target` does not convert x,y."},
                "y":{"type":"number","description":"Destination Y, in the same space as x."},
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Cursor instance to move when no `session` is given; with `session`, the session label names the cursor. Default 'default'."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"What moves: \"window\" (default) only the agent cursor overlay, leaving the user's pointer alone; \"desktop\" the real OS pointer. A `target` of kind window or desktop selects the same scope."}
            },"additionalProperties":false}),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        if cursor_control_scope(&args) == CursorControlScope::Desktop {
            let input = match parse_typed_projection::<MoveCursorInput>("move_cursor", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (x, y) = (input.x, input.y);
            let xi = x.round() as i32;
            let yi = y.round() as i32;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland {
                "wayland_desktop"
            } else {
                "xtest_desktop"
            };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            // Glide the agent cursor to where the real pointer is about to
            // jump. The overlay draws in layout coordinates, like the input.
            let (overlay_x, overlay_y) = space.to_layout(xi, yi);
            reveal_pointer_action_for(
                &self.state,
                &resolve_cursor_key(&args),
                f64::from(overlay_x),
                f64::from(overlay_y),
                false,
            )
            .await;
            let result = if wayland {
                tokio::task::spawn_blocking(move || {
                    crate::wayland::move_cursor_desktop(&space, xi, yi)
                })
                .await
            } else {
                tokio::task::spawn_blocking(move || crate::input::send_move_xtest_desktop(xi, yi))
                    .await
            };
            return match result {
                Ok(Ok(())) => {
                    // On X11 the server reports the pointer XTest just moved;
                    // Wayland gives clients no such query.
                    let read_back = if crate::wayland::is_wayland() {
                        None
                    } else {
                        tokio::task::spawn_blocking(current_pointer_position)
                            .await
                            .ok()
                            .flatten()
                            .map(|(rx, ry)| (rx.round() as i32, ry.round() as i32))
                    };
                    let mut structured = json!({
                        "scope":"desktop","path":path,"x":xi,"y":yi,"effect":"unverifiable"
                    });
                    let text = match read_back {
                        Some((rx, ry)) if (rx, ry) == (xi, yi) => {
                            structured["effect"] = json!("confirmed");
                            structured["evidence"] = json!([{
                                "kind": "value_readback",
                                "detail": format!("the X server reports the pointer at ({rx}, {ry})"),
                            }]);
                            format!("Moved the real desktop pointer to ({xi}, {yi}); it reads back there.")
                        }
                        Some((rx, ry)) => {
                            structured["read_back"] = json!({"x": rx, "y": ry});
                            format!(
                                "Sent the real desktop pointer to ({xi}, {yi}), but it reads back \
                                 at ({rx}, {ry}); a point off the screen stops at its edge."
                            )
                        }
                        None => format!("Moved the real desktop pointer to ({xi}, {yi})."),
                    };
                    ToolResult::text(text).with_structured(structured)
                }
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let x = args.f64_or("x", 0.0);
        let y = args.f64_or("y", 0.0);
        let cursor_id = resolve_cursor_key(&args);
        // End pointing upper-left (45°) — matches Swift's
        // `AgentCursor.animateAndWait(endAngleDegrees: 45)` convention so the
        // overlay arrow settles to the natural macOS-style pose.
        // Use the acknowledged animation path so a first-ever move seeds and
        // displays the session cursor just as reliably as a coordinate click.
        reveal_pointer_action_for(&self.state, &cursor_id, x, y, false).await;
        ToolResult::text(format!(
            "Agent cursor '{cursor_id}' moved to ({x:.1}, {y:.1}); the user pointer was unchanged."
        ))
    }
}

// ── move_along_path ───────────────────────────────────────────────────────────

/// Glides the real pointer along a sampled path (see `crate::input::path`).
/// On X11, delivery is XTest, the route `move_cursor` scope=desktop and
/// foreground drags use; the session's MPX virtual pointer is never used,
/// because the point of this tool is the visible system pointer. On Wayland
/// it is the route of `move_cursor` scope=desktop (the wlroots virtual
/// pointer, else libei), for desktop scope only. Both share the timing of
/// `crate::input::run_pointer_path`.
pub struct MoveAlongPathTool {
    state: Arc<ToolState>,
}

static MOVE_ALONG_PATH_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn path_point_schema(description: &str) -> Value {
    json!({
        "type": "array",
        "items": {"type": "number"},
        "minItems": 2,
        "maxItems": 2,
        "description": description
    })
}

fn move_along_path_schema() -> Value {
    use crate::input::path::{
        MAX_DURATION_MS, MAX_POINTS, MAX_SAMPLES, MAX_SEGMENTS, MAX_SPACING_PX, MIN_SPACING_PX,
    };
    let point = |description: &str| path_point_schema(description);
    let segment = |properties: Value, required: Value, description: &str| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
            "description": description
        })
    };
    json!({"type":"object","properties":{
        "session": pincer_core::tool_schema::session_schema(),
        "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"\"window\" (default): window_id's get_window_state screenshot pixels; the window is raised first. \"desktop\": get_desktop_state pixels, no pid/window_id."},
        "pid":{"type":"integer","description":"Target process ID. Required for window scope."},
        "window_id":{"type":"integer","description":"Target window ID. Required for window scope."},
        "start": point("First point [x, y] of `segments`."),
        "segments":{"type":"array","minItems":1,"maxItems":MAX_SEGMENTS,"description":"Pieces after `start`, each with one key: line, quad, cubic, or arc (from the current point around center, which sets the radius).","items":{
            "type":"object",
            "minProperties":1,
            "maxProperties":1,
            "additionalProperties":false,
            "properties":{
                "line": segment(json!({"to": point("End point.")}), json!(["to"]), "Straight line to `to`."),
                "quad": segment(json!({"control": point("Control point."), "to": point("End point.")}), json!(["control","to"]), "Quadratic Bezier curve."),
                "cubic": segment(json!({"control1": point("First control point."), "control2": point("Second control point."), "to": point("End point.")}), json!(["control1","control2","to"]), "Cubic Bezier curve."),
                "arc": segment(json!({"center": point("Center of the arc."), "sweep_deg": {"type":"number","description":"Degrees to turn; positive is clockwise on screen."}}), json!(["center","sweep_deg"]), "Circular arc around `center`.")
            }
        }},
        "points":{"type":"array","minItems":2,"maxItems":MAX_POINTS,"items": point("Point [x, y]."),"description":"Polyline [[x, y], ...], instead of segments."},
        "smooth":{"type":"boolean","description":"With `points`: a Catmull-Rom curve through the points instead of straight spans. Default false."},
        "parametric":{"type":"object","required":["x","y","t_from","t_to"],"additionalProperties":false,"description":"x(t), y(t) for t from t_from to t_to (circles, loops, spirals). Supports + - * / ^, sin, cos, tan, sqrt, abs, exp, ln, pi, e.","properties":{
            "x":{"type":"string","description":"x(t), e.g. \"1280+300*cos(t)\"."},
            "y":{"type":"string","description":"y(t), e.g. \"720+300*sin(t)\"."},
            "t_from":{"type":"number","description":"First value of t."},
            "t_to":{"type":"number","description":"Last value of t."}
        }},
        // The shared `button` shape (left/right/middle); leaving it out is the
        // hover-only glide, and "none" is accepted as the same thing.
        "button":{"type":"string","enum":["left","right","middle"],"description":"Button held from the first point to the last. Omit to only move."},
        "click_at_end":{"type":"string","enum":["left","right","middle","double"],"description":"Click at the last point after the glide. Only without `button`."},
        "duration_ms":{"type":"integer","minimum":0,"maximum":MAX_DURATION_MS,"default":600,"description":"Glide time in ms. Default 600."},
        "easing":{"type":"string","enum":["linear","ease_in","ease_out","ease_in_out"],"default":"ease_in_out","description":"Speed profile; linear is even. Default ease_in_out."},
        "spacing_px":{"type":"number","minimum":MIN_SPACING_PX,"maximum":MAX_SPACING_PX,"default":4,"description":format!("Max distance between samples, in path pixels. Default 4; at most {MAX_SAMPLES} samples.")}
    },"additionalProperties":false})
}

fn move_along_path_refusal(detail: impl Into<String>) -> ToolResult {
    // Parser messages arrive with their own full stop.
    let detail = detail.into().trim_end_matches('.').to_owned();
    ToolResult::error(format!("move_along_path: {detail}. No input was sent.")).with_structured(
        json!({
            "code": "invalid_arguments",
            "tool": "move_along_path",
            "effect": "refused",
            "detail": detail,
        }),
    )
}

fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[async_trait]
impl Tool for MoveAlongPathTool {
    fn def(&self) -> &ToolDef {
        MOVE_ALONG_PATH_DEF.get_or_init(|| ToolDef {
            name: "move_along_path".into(),
            description: "Move the real pointer along a path (segments, a polyline, or a \
                parametric x(t), y(t)), optionally holding a button to draw or drag, or clicking \
                at the end. On Wayland, only scope \"desktop\" without pid/window_id works."
                .into(),
            input_schema: move_along_path_schema(),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use crate::input::path;
        let wayland = crate::wayland::wayland_input_enabled();
        let scope = args.opt_str("scope");
        if let Some(other) = scope
            .as_deref()
            .filter(|scope| !matches!(*scope, "window" | "desktop"))
        {
            return move_along_path_refusal(format!(
                "scope must be \"window\" or \"desktop\", not \"{other}\""
            ));
        }
        // Dispatch rewrites scope:"desktop" plus pid/window_id into
        // coordinate_frame:"desktop": desktop pixels, delivered to that window.
        let desktop_pixels = scope.as_deref() == Some("desktop") || desktop_frame_requested(&args);
        let target = match (args.opt_u64("pid"), args.opt_u64("window_id")) {
            (None, None) => None,
            (Some(pid), Some(xid)) => match u32::try_from(pid) {
                Ok(pid) => Some((pid, xid)),
                Err(_) => return move_along_path_refusal("pid is out of range"),
            },
            _ => return move_along_path_refusal("pid and window_id go together"),
        };
        if wayland && target.is_some() {
            // Window pixels need the window's screen origin and a verified
            // way to raise it, which Wayland compositors do not reliably
            // give, so only desktop pixels are accepted.
            return ToolResult::error(
                "move_along_path on Wayland only takes scope \"desktop\" without pid or \
                 window_id, in get_desktop_state pixels. No input was sent.",
            )
            .with_structured(json!({
                "code": "unsupported_platform",
                "tool": "move_along_path",
                "effect": "refused",
            }));
        }
        if target.is_none() && !desktop_pixels {
            return move_along_path_refusal(
                "window scope needs pid and window_id (window-local screenshot pixels); \
                 pass scope \"desktop\" without them for get_desktop_state pixels",
            );
        }
        let request = match path::PathRequest::from_args(&args) {
            Ok(request) => request,
            Err(error) => return move_along_path_refusal(error.to_string()),
        };
        let sampled = {
            let request = request.clone();
            match tokio::task::spawn_blocking(move || path::sample(&request)).await {
                Ok(Ok(sampled)) => sampled,
                Ok(Err(error)) => return move_along_path_refusal(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
        };

        // Map the samples to screen pixels: one affine map for the whole
        // path, so arcs and parametric curves keep their shape.
        let (scale, origin) = if let Some((pid, xid)) = target {
            match tokio::task::spawn_blocking(move || window_target_matches(pid, xid, None)).await {
                Ok(true) => {}
                Ok(false) => {
                    return move_along_path_refusal(format!(
                        "window_id {xid} does not belong to pid {pid}"
                    ))
                }
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
            let scale = if desktop_pixels {
                let (fx, fy) = pincer_core::desktop_capture_scale::desktop_scale_factor(&args)
                    .unwrap_or((1.0, 1.0));
                (fx, fy)
            } else {
                match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                    Ok(scale) => (scale, scale),
                    Err(refusal) => return refusal,
                }
            };
            let origin = if desktop_pixels {
                (0.0, 0.0)
            } else {
                match tokio::task::spawn_blocking(move || window_local_to_screen(xid, 0.0, 0.0))
                    .await
                {
                    Ok(Ok(origin)) => origin,
                    Ok(Err(error)) => return ToolResult::error(format!("{error:#}")),
                    Err(error) => return ToolResult::error(format!("Task error: {error}")),
                }
            };
            (scale, origin)
        } else {
            (
                pincer_core::desktop_capture_scale::desktop_scale_factor(&args)
                    .unwrap_or((1.0, 1.0)),
                (0.0, 0.0),
            )
        };
        let screen: Vec<(f64, f64)> = sampled
            .points
            .iter()
            .map(|(x, y)| (origin.0 + x * scale.0, origin.1 + y * scale.1))
            .collect();
        // On Wayland the backend checks the samples against its own pointer
        // area, before it sends any input.
        let mut screen_width = None;
        if !wayland {
            let (width, height) = match tokio::task::spawn_blocking(x11_screen_size).await {
                Ok(Ok(size)) => size,
                Ok(Err(error)) => return ToolResult::error(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            };
            if let Err(error) = path::check_on_screen(&screen, width, height) {
                return move_along_path_refusal(error.to_string());
            }
            screen_width = Some(width);
        }

        let points: Vec<(i32, i32)> = screen
            .iter()
            .map(|(x, y)| (x.round() as i32, y.round() as i32))
            .collect();
        let space = match desktop_input_space().await {
            Ok(space) => space,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        // The agent cursor follows the real pointer in the overlay's layout
        // coordinates (the same as screen pixels on X11).
        let overlay_points: Vec<(f64, f64)> = points
            .iter()
            .map(|&(x, y)| {
                let (lx, ly) = space.to_layout(x, y);
                (f64::from(lx), f64::from(ly))
            })
            .collect();
        let cursor_id = resolve_cursor_key(&args);
        if let Some((_, xid)) = target {
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::PinAbove(xid),
            );
        }
        // Glide to the start before the real pointer jumps there.
        reveal_pointer_action_for(
            &self.state,
            &cursor_id,
            overlay_points[0].0,
            overlay_points[0].1,
            false,
        )
        .await;

        let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let gesture = crate::input::PointerPathGesture {
            points,
            offsets_ms: sampled.offsets_ms.clone(),
            button: request.button,
            click_at_end: request.click_at_end.map(path::EndClick::x_button_and_count),
            started: Some(started_tx),
        };
        let budget = std::time::Duration::from_millis(request.duration_ms + 15_000);
        let run = async move {
            match target {
                // Like move_cursor and drag on Wayland: libei can wait for
                // portal consent, which the X11 budget does not allow for.
                None if wayland => {
                    tokio::task::spawn_blocking(move || {
                        crate::wayland::pointer_path_desktop(&space, &gesture)
                    })
                    .await
                }
                None => {
                    spawn_blocking_bounded("move_along_path", budget, move || {
                        crate::input::send_pointer_path_xtest(&gesture)
                    })
                    .await
                }
                Some((pid, xid)) => {
                    spawn_blocking_bounded("move_along_path", budget, move || {
                        // The report is not used (the result is always
                        // "unverifiable"), so skip the post-path observation:
                        // with no window change it polled for up to 800 ms.
                        crate::input::with_x11_foreground_opts(
                            xid,
                            crate::input::ForegroundOptions::pointer()
                                .for_pid(pid)
                                .without_observation(),
                            || crate::input::send_pointer_path_xtest(&gesture),
                        )
                        .map(|(elapsed, _report)| elapsed)
                    })
                    .await
                }
            }
        };
        let follow = follow_overlay_path_with(
            |command| crate::overlay::send_command_for(cursor_id.clone(), command),
            &overlay_points,
            &sampled.offsets_ms,
            request.button.is_some(),
            eased_jump_px(screen_width),
            started_rx,
        );
        let result = if crate::overlay::renderer_configured() {
            tokio::pin!(run);
            tokio::pin!(follow);
            tokio::select! {
                // The route returns once its last sample is sent, which can be
                // a frame before the follow's own clock reaches the end: let
                // the follow finish so the agent cursor's glide is drawn in
                // full. A route that fails mid-path ends the follow with it.
                result = &mut run => {
                    if matches!(&result, Ok(Ok(_))) {
                        follow.await;
                    }
                    result
                }
                () = &mut follow => run.await,
            }
        } else {
            run.await
        };
        if matches!(&result, Ok(Ok(_))) {
            let end = *overlay_points.last().expect("sampled path is non-empty");
            self.state
                .cursor_registry
                .update_position(&cursor_id, end.0, end.1);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::track_pointer_command(end.0, end.1),
            );
            if request.click_at_end.is_some() {
                emit_cursor_hook(&cursor_id, end.0, end.1, true);
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::ClickPulse { x: end.0, y: end.1 },
                );
            }
        }
        let elapsed = match result {
            Ok(Ok(elapsed)) => elapsed,
            Ok(Err(error)) => {
                if let Some(refused) = error.downcast_ref::<crate::wayland::PointerPathRefused>() {
                    return move_along_path_refusal(refused.to_string());
                }
                return input_error_result(error);
            }
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };

        let first = sampled.points[0];
        let last = *sampled.points.last().expect("sampled path is non-empty");
        let duration_ms = elapsed.as_millis() as u64;
        let stats = json!({
            "segments": sampled.segment_count,
            "samples": sampled.points.len(),
            "length_px": round_tenth(sampled.length_px),
            "duration_ms": duration_ms,
            "start": [round_tenth(first.0), round_tenth(first.1)],
            "end": [round_tenth(last.0), round_tenth(last.1)],
        });
        let held = match request.button {
            Some(1) => " holding the left button",
            Some(2) => " holding the middle button",
            Some(3) => " holding the right button",
            _ => "",
        };
        let click = match request.click_at_end {
            Some(path::EndClick::Left) => ", then left-clicked",
            Some(path::EndClick::Right) => ", then right-clicked",
            Some(path::EndClick::Middle) => ", then middle-clicked",
            Some(path::EndClick::Double) => ", then double-clicked",
            None => "",
        };
        let windowed = target.is_some();
        // One line with every stat, floats to one decimal as JSON writes
        // them, so the text result does not repeat `stats` as JSON.
        ToolResult::text(format!(
            "Moved the real pointer along the path{held} in {duration_ms} ms{click}: \
             {:.1} px from ({:.1}, {:.1}) to ({:.1}, {:.1}) in the coordinates passed, {} \
             segment(s), {} samples.",
            round_tenth(sampled.length_px),
            round_tenth(first.0),
            round_tenth(first.1),
            round_tenth(last.0),
            round_tenth(last.1),
            sampled.segment_count,
            sampled.points.len(),
        ))
        .with_structured(json!({
            "scope": if windowed { "window" } else { "desktop" },
            "path": if windowed {
                "x11_xtest_fg"
            } else if wayland {
                "wayland_desktop"
            } else {
                "xtest_desktop"
            },
            "delivery_mode": if windowed { "foreground" } else { "not_applicable" },
            "effect": "unverifiable",
            "stats": stats,
        }))
    }
}

// ── set_agent_cursor_enabled ──────────────────────────────────────────────────

/// The `renderer` value and a text suffix for an agent-cursor reply. Without a
/// renderer (XWayland mode) the setters still store the value, but nothing on
/// screen changes, and the reply has to say so.
fn cursor_renderer_report() -> (Value, String) {
    match crate::overlay::active_renderer() {
        Some(renderer) => (json!(renderer), String::new()),
        None => (
            Value::Null,
            format!(
                " Nothing changes on screen: {}.",
                crate::overlay::missing_renderer_reason()
            ),
        ),
    }
}

pub struct SetAgentCursorEnabledV2Tool {
    state: Arc<ToolState>,
}

static CURSOR_ENABLED_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetAgentCursorEnabledV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_ENABLED_V2_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_enabled"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let enabled = match args.get("enabled").and_then(Value::as_bool) {
            Some(value) => value,
            None => return ToolResult::error("Missing required boolean field `enabled`."),
        };
        let session = resolve_cursor_key(&args);
        self.state.cursor_registry.set_enabled(&session, enabled);
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetEnabled(enabled),
        );
        let (renderer, note) = cursor_renderer_report();
        // Pointer and keyboard actions turn the cursor back on (see
        // `overlay_glide_to_target_for`), so the reply says how long a hide
        // lasts; round 2's Opus tester expected it to last until
        // `enabled:true`.
        ToolResult::text(format!(
            "Agent cursor for session '{session}' {}.{note}",
            if enabled {
                "enabled"
            } else {
                "disabled until the session's next desktop pointer or keyboard action, which \
                 shows it again"
            }
        ))
        .with_structured(json!({"session":session,"enabled":enabled,"renderer":renderer}))
    }
}

pub struct SetAgentCursorMotionV2Tool;

static CURSOR_MOTION_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn cursor_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_i64().map(|integer| integer as f64))
    })
}

#[async_trait]
impl Tool for SetAgentCursorMotionV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_MOTION_V2_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_motion"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = resolve_cursor_key(&args);
        let current = crate::overlay::current_motion_for(&session);
        let motion = current.with_overrides(
            cursor_number(args.get("start_handle")),
            cursor_number(args.get("end_handle")),
            cursor_number(args.get("arc_size")),
            cursor_number(args.get("arc_flow")),
            cursor_number(args.get("spring")),
            cursor_number(args.get("glide_duration_ms")),
            cursor_number(args.get("dwell_after_click_ms")),
            cursor_number(args.get("idle_hide_ms")),
            None,
            cursor_number(args.get("turn_radius")),
        );
        let motion = match motion.with_style_args(&args) {
            Ok(motion) => motion,
            Err(message) => return ToolResult::error(message),
        };
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetMotion(motion.clone()),
        );
        let (renderer, note) = cursor_renderer_report();
        ToolResult::text(format!(
            "Agent cursor motion updated for session '{session}': style {}, timing {}.{note}\n{}",
            motion.style.as_str(),
            motion.timing.as_str(),
            motion.effects_summary(&current),
        ))
        .with_structured(
            json!({"session":session,"motion":motion.output_json(),"renderer":renderer}),
        )
    }
}

pub struct SetAgentCursorThemeTool {
    state: Arc<ToolState>,
}

static CURSOR_THEME_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn cursor_reduced_motion(value: Option<&str>) -> cursor_overlay::ReducedMotion {
    match value {
        Some("on") => cursor_overlay::ReducedMotion::On,
        Some("off") => cursor_overlay::ReducedMotion::Off,
        _ => cursor_overlay::ReducedMotion::Auto,
    }
}

#[async_trait]
impl Tool for SetAgentCursorThemeTool {
    fn def(&self) -> &ToolDef {
        CURSOR_THEME_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_theme"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(theme_id) = args.get("theme_id").and_then(Value::as_str) else {
            return ToolResult::error("Missing required string field `theme_id`.");
        };
        let session = resolve_cursor_key(&args);
        let resolved_theme = match cursor_overlay::resolve_theme_selection(theme_id) {
            Ok(theme) => theme,
            Err(error) => {
                return ToolResult::error(format!(
                    "Cursor theme '{theme_id}' cannot be selected: {error:#}. Installed theme \
                     ids: {}.",
                    installed_cursor_themes().join(", ")
                ));
            }
        };
        let (version, profile) = resolved_theme
            .as_deref()
            .map(|theme| (theme.version.as_str(), theme.profile.as_str()))
            .unwrap_or((
                cursor_overlay::DEFAULT_THEME_VERSION,
                cursor_overlay::THEME_PROFILE,
            ));
        let reduced_motion =
            cursor_reduced_motion(args.get("reduced_motion").and_then(Value::as_str));
        self.state
            .cursor_registry
            .update_config(&session, |config| {
                config.theme_id = theme_id.to_owned();
                config.reduced_motion = reduced_motion;
            });
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetTheme {
                theme_id: theme_id.to_owned(),
                reduced_motion,
            },
        );
        let (renderer, note) = cursor_renderer_report();
        ToolResult::text(format!(
            "Agent cursor theme for session '{session}' set to '{theme_id}'.{note}"
        ))
        .with_structured(json!({"session":session,"theme":{
            "id":theme_id,
            "version":version,
            "profile":profile,
            "reduced_motion":reduced_motion,
            "fallback":null
        },"renderer":renderer}))
    }
}

pub struct GetAgentCursorStateV2Tool {
    state: Arc<ToolState>,
}

static CURSOR_STATE_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetAgentCursorStateV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_STATE_V2_DEF.get_or_init(|| canonical_cursor_def("get_agent_cursor_state"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = resolve_cursor_key(&args);
        let enabled = crate::overlay::is_enabled_for(&session);
        let motion = crate::overlay::current_motion_for(&session);
        let (theme_id, version, profile, fallback, visual) =
            crate::overlay::current_theme_state_for(&session).unwrap_or_else(|| {
                (
                    cursor_overlay::DEFAULT_THEME_ID.into(),
                    cursor_overlay::DEFAULT_THEME_VERSION.into(),
                    cursor_overlay::THEME_PROFILE.into(),
                    None,
                    cursor_overlay::CursorVisualState::default(),
                )
            });
        let modifiers: Vec<&str> = [
            visual.delivery.map(|value| value.as_str()),
            visual.target.map(|value| value.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        // The last point this session's cursor was placed at, `null` until
        // it first moves (same registry source as macOS).
        let position = self
            .state
            .cursor_registry
            .get(&session)
            .and_then(|cursor| cursor.x.zip(cursor.y))
            .map(|(x, y)| json!({"x": x, "y": y}));
        // Without a renderer nothing applies the setters' commands, so the
        // motion and theme below are the defaults, not what was last set.
        let (renderer, note) = cursor_renderer_report();
        let installed_themes = installed_cursor_themes();
        let drawn_at = match &position {
            Some(point) => format!(
                "last drawn at ({}, {}) in desktop pixels",
                point["x"], point["y"]
            ),
            None => "not drawn yet (position null)".to_owned(),
        };
        // The effects line says which effects are pinned even when none is:
        // the text renderer leaves an empty `motion.pinned_effects` out.
        let text = format!(
            "Agent cursor state for session '{session}'.{note}\n\
             enabled={enabled}: {}; get_session's cursor_visible says whether it is drawn right \
             now (it hides after motion.idle_hide_ms idle).\n\
             Position: {drawn_at}.\n\
             Installed themes (ids for set_agent_cursor_theme): {}.\n\
             {}",
            if enabled {
                "it may be drawn"
            } else {
                "hidden by set_agent_cursor_enabled until the session's next desktop pointer or \
                 keyboard action"
            },
            installed_themes.join(", "),
            motion.effects_summary(&motion),
        );
        ToolResult::text(text).with_structured(json!({
            "session":session,
            "renderer":renderer,
            "enabled":enabled,
            "position":position,
            "installed_themes":installed_themes,
            "theme":{
                "id":theme_id,
                "version":version,
                "profile":profile,
                "reduced_motion":visual.reduced_motion,
                "fallback":fallback
            },
            "visual_state":{
                "requested_action":visual.requested_action,
                "resolved_action":visual.resolved_action,
                "modifiers":modifiers,
                "phase":visual.phase(),
                "frame":visual.frame(),
                "preempted_count":visual.preempted_count
            },
            "motion":motion.output_json()
        }))
    }
}

fn canonical_cursor_def(name: &str) -> ToolDef {
    let contract = pincer_contract::tool_contract(name)
        .unwrap_or_else(|| panic!("missing canonical cursor contract for {name}"));
    ToolDef::from_contract(&contract)
}

/// The ids set_agent_cursor_theme accepts: the built-in default plus the
/// theme store's files, as `pincer cursor-theme list` reads them. An
/// unreadable store still leaves the built-in.
fn installed_cursor_themes() -> Vec<String> {
    cursor_overlay::list_installed_themes()
        .unwrap_or_else(|_| vec![cursor_overlay::DEFAULT_THEME_ID.to_owned()])
}

// ── check_permissions ─────────────────────────────────────────────────────────

pub struct CheckPermissionsTool;
static PERMS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for CheckPermissionsTool {
    fn def(&self) -> &ToolDef {
        PERMS_DEF.get_or_init(|| ToolDef {
            name: "check_permissions".into(),
            description: "Report whether X11, a Wayland session, and the AT-SPI accessibility \
                bus are reachable, with a fix hint for each failure. Linux has no permission \
                prompts. Fields: `x11`, `wayland` (this is a Wayland session), \
                `wayland_enabled` (Pincer's experimental Wayland backend is on), `atspi`, \
                `xsend_event`. On a working X11 session Wayland is not applicable, not a \
                failure.".into(),
            input_schema: json!({"type":"object","properties":{"prompt":{"type":"boolean","description":"Ignored: Linux has no permission prompts."}},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        // Check X11 connectivity (required for window enumeration and input injection).
        let x11_ok = tokio::task::spawn_blocking(|| {
            x11rb::rust_connection::RustConnection::connect(None).is_ok()
        })
        .await
        .unwrap_or(false);

        // Check AT-SPI: not merely "is there a session bus?" but "does
        // org.a11y.Bus actually answer on it?" — the previous env-var-or-
        // /run/user heuristic false-passed exactly the headless/container case
        // (/run/user exists, but no a11y bus → empty trees). Probe for real.
        let dbus_address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        let bus_ok = tokio::task::spawn_blocking(crate::health_report::probe_a11y_bus)
            .await
            .unwrap_or(false);
        // The bus name can have an owner while no registry runs, and then
        // every read comes back without a tree (round 8 of the first-call
        // test), so ask the registry itself.
        let registry = if bus_ok {
            tokio::task::spawn_blocking(crate::health_report::probe_atspi_registry)
                .await
                .unwrap_or_else(|error| Err(error.to_string()))
        } else {
            Err("org.a11y.Bus is not reachable".to_owned())
        };
        let atspi_ok = registry.is_ok();

        let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
        let atspi_status = if let (true, Err(why)) = (bus_ok, &registry) {
            format!(
                "❌ org.a11y.Bus reachable, but the AT-SPI registry does not answer: {why}. \
                 Window reads will return no element tree; start at-spi2-registryd on the \
                 accessibility bus"
            )
        } else if atspi_ok {
            match &dbus_address {
                Some(a) => format!(
                    "✅ org.a11y.Bus reachable and the registry answers (DBUS_SESSION_BUS_ADDRESS={a})"
                ),
                None => "✅ org.a11y.Bus reachable and the registry answers".to_string(),
            }
        } else if dbus_address.is_none() {
            "❌ no session bus (DBUS_SESSION_BUS_ADDRESS unset and none auto-discovered) — \
             AT-SPI trees will be empty; start the daemon inside the desktop session"
                .to_string()
        } else {
            format!(
                "❌ session bus present but org.a11y.Bus has no owner: {}",
                crate::a11y::no_bus_advice(crate::a11y::current_desktop().as_deref())
            )
        };
        let status_text = format!(
            "X11 display: {}\nWayland: {}\nAT-SPI (D-Bus): {}\nXSendEvent injection: {}",
            if x11_ok { "✅ connected" } else { "❌ DISPLAY not set or X11 unavailable" },
            match &wayland_display {
                Some(s) if crate::wayland::wayland_enabled() =>
                    format!("✅ native Wayland session (WAYLAND_DISPLAY={s}) — experimental backend ENABLED"),
                Some(s) => format!(
                    "⚠️  native Wayland session (WAYLAND_DISPLAY={s}) — experimental backend OFF; \
                     set {}=1 to enable it",
                    crate::wayland::ENABLE_WAYLAND_ENV
                ),
                // X11 serves every tool here, so no Wayland is no failure.
                None if x11_ok => "➖ not applicable (an X11 session)".to_string(),
                None => "❌ not a Wayland session".to_string(),
            },
            atspi_status,
            if x11_ok { "✅ available" } else { "❌ requires X11" }
        );
        ToolResult::text(status_text)
            .with_structured(json!({ "x11": x11_ok, "wayland": wayland_display.is_some(), "wayland_enabled": crate::wayland::wayland_enabled(), "atspi": atspi_ok, "dbus_session_bus_address": dbus_address, "xsend_event": x11_ok }))
    }
}

#[cfg(test)]
#[tokio::test]
async fn check_permissions_accepts_the_status_prompt_argument() {
    let result = build_registry_with_provider(false, None)
        .invoke("check_permissions", json!({ "prompt": false }))
        .await;
    let wire = serde_json::to_value(&result).unwrap();
    assert!(wire["structuredContent"].get("x11").is_some(), "{wire}");
}

// ── get_config ────────────────────────────────────────────────────────────────

pub struct GetConfigTool {
    state: Arc<ToolState>,
}
static GCFG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetConfigTool {
    fn def(&self) -> &ToolDef {
        GCFG_DEF.get_or_init(|| ToolDef {
            name: "get_config".into(),
            // The text result marks each setting `(saved)` or `(built-in
            // default)`, so the text renderer drops the structured
            // `saved_keys` list as already said (it prints it only when an
            // effect is saved); naming it here sent both round-2 testers
            // looking for it.
            description: "Return the current pincer configuration: each setting's value and \
                whether it is saved in ~/.pincer/config.json (by set_config) or a built-in \
                default. Cursor effects not saved follow the motion style."
                .into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        let max_image_dimension = self.state.config.read().unwrap().max_image_dimension;
        let (pip_enabled, pip_geometry) = pip_preview::read_pip_keys_from_file();
        let motion = cursor_overlay::motion_defaults::read_saved();
        let badges = cursor_overlay::badge_mode::read_saved();
        // A key counts as saved when ~/.pincer/config.json holds a value its
        // loader accepts; otherwise the built-in default applies. These are
        // the checks `load_driver_config`, `read_pip_keys_from_file` and the
        // cursor loaders make.
        let file = pip_preview::read_config_value;
        let saved_dimension = file("max_image_dimension")
            .and_then(|value| value.as_u64())
            .is_some_and(|value| u32::try_from(value).is_ok());
        let saved_pip = file("experimental_pip").is_some_and(|value| value.is_boolean());
        let saved_badges = file("cursor")
            .and_then(|cursor| cursor.get("badges").cloned())
            .is_some_and(|value| {
                matches!(cursor_overlay::badge_mode::parse_value(&value), Ok(Some(_)))
            });
        let saved_effects: Vec<(&str, bool)> = [
            ("trail", motion.effects.trail),
            ("glow", motion.effects.glow),
            ("magnet", motion.effects.magnet),
            ("ripple", motion.effects.ripple),
            ("squish", motion.effects.squish),
        ]
        .into_iter()
        .filter_map(|(name, value)| Some((name, value?)))
        .collect();

        let mut saved_keys = Vec::new();
        let mut mark = |key: String, saved: bool| {
            if saved {
                saved_keys.push(key);
                "saved"
            } else {
                "built-in default"
            }
        };
        let dimension_source = mark("max_image_dimension".into(), saved_dimension);
        let style_source = mark("cursor.motion.style".into(), motion.style.is_some());
        let timing_source = mark("cursor.motion.timing".into(), motion.timing.is_some());
        for (name, _) in &saved_effects {
            mark(format!("cursor.motion.effects.{name}"), true);
        }
        let badges_source = mark("cursor.badges".into(), saved_badges);
        let pip_source = mark("experimental_pip".into(), saved_pip);
        if pip_geometry.is_some() {
            mark("experimental_pip_geometry".into(), true);
        }

        let motion_json = motion.config_json();
        let effects = if saved_effects.is_empty() {
            "no effect saved, each follows the style".to_owned()
        } else {
            let pinned: Vec<String> = saved_effects
                .iter()
                // set_config's vocabulary (on/off), so a model can copy it back.
                .map(|(name, value)| format!("{name}={}", if *value { "on" } else { "off" }))
                .collect();
            format!("{} (saved), the rest follow the style", pinned.join(", "))
        };
        let mut lines = vec![
            format!(
                "pincer configuration (version {}, platform linux). \"saved\" values come from \
                 ~/.pincer/config.json; the others are built-in defaults.",
                env!("CARGO_PKG_VERSION")
            ),
            format!("- max_image_dimension={max_image_dimension} ({dimension_source})"),
            format!(
                "- cursor.motion.style={} ({style_source}), cursor.motion.timing={} \
                 ({timing_source}), cursor.motion.effects: {effects}",
                motion_json["style"].as_str().unwrap_or_default(),
                motion_json["timing"].as_str().unwrap_or_default(),
            ),
            format!("- cursor.badges={} ({badges_source})", badges.as_str()),
            format!("- experimental_pip={pip_enabled} ({pip_source})"),
        ];
        if let Some(geometry) = &pip_geometry {
            lines.push(format!("- experimental_pip_geometry={geometry} (saved)"));
        }
        ToolResult::text(lines.join("\n")).with_structured(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "source_sha": option_env!("PINCER_SOURCE_SHA"),
            "platform": "linux",
            "max_image_dimension": max_image_dimension,
            "cursor": {
                "motion": motion_json,
                "badges": badges.as_str(),
            },
            "experimental_pip": pip_enabled,
            "experimental_pip_geometry": pip_geometry,
            "saved_keys": saved_keys,
        }))
    }
}

// ── set_config ────────────────────────────────────────────────────────────────

fn with_cursor_motion_config_properties(mut schema: Value) -> Value {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.extend(cursor_overlay::motion_defaults::config_schema_properties());
        properties.extend(cursor_overlay::badge_mode::config_schema_properties());
    }
    schema
}

pub struct SetConfigTool {
    state: Arc<ToolState>,
}
static SCFG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetConfigTool {
    fn def(&self) -> &ToolDef {
        SCFG_DEF.get_or_init(|| ToolDef {
            name: "set_config".into(),
            description: "Update the driver configuration. max_image_dimension, the long-edge \
                cap of get_window_state screenshots, takes effect immediately (get_desktop_state \
                ignores it and takes its own max_image_dimension). The result echoes each key this call set as `key=value`; \
                get_config shows the rest.\n\n\
                Pass each setting as its own field: `{\"max_image_dimension\": 0}`.\n\n\
                The experimental_pip keys persist to ~/.pincer/config.json and apply on the \
                next daemon restart; the Linux PiP backend is a stub, so they show nothing here.\n\n\
                `cursor.motion.style`, `cursor.motion.timing` and `cursor.motion.effects.<name>` \
                (`on`, `off`, or `default`) save the default cursor motion for sessions started afterwards. \
                `cursor.motion: \"default\"` clears every saved cursor motion default.\n\n\
                `cursor.badges` (`minimal` default, `verbose`, or `off`) sets the agent cursor badge \
                for every cursor at once.".into(),
            input_schema: with_cursor_motion_config_properties(json!({"type":"object","properties":{
                "max_image_dimension":{"type":"integer","description":"Long-edge cap in pixels for get_window_state screenshots (0 = no limit); get_desktop_state ignores it."},
                "experimental_pip":{"type":"boolean","description":"Enable the experimental PiP preview window (applies next restart; Linux backend stubbed)."},
                "experimental_pip_geometry":{"type":"string","description":"PiP window size + optional position in `WxH` or `WxH+X+Y` form."}
            },"additionalProperties":false})),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        if args.get("capture_scope").is_some() {
            return ToolResult::error(
                "config key 'capture_scope' is retired; select a window or desktop target on each action",
            )
            .with_structured(json!({
                "code": "config_key_retired",
                "key": "capture_scope",
                "replacement": "action.target",
            }));
        }
        let motion_keys = match cursor_overlay::motion_defaults::apply_config_args(&args) {
            Ok(keys) => keys,
            Err(message) => return ToolResult::error(message),
        };
        let badge_key = match cursor_overlay::badge_mode::apply_config_args(&args) {
            Ok(key) => key,
            Err(message) => return ToolResult::error(message),
        };
        let mut cfg = self.state.config.write().unwrap();
        // The result names each key this call set with its new value, in
        // the text and the structured echo, and nothing else: at 3eefd3e a
        // style change answered `cursor.motion.style` with no value and
        // then the unrelated max_image_dimension and experimental_pip.
        let mut updated = serde_json::Map::new();
        let as_set = |value: &Value| match value {
            Value::Null => "default".to_owned(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let mut parts: Vec<String> = Vec::new();
        for key in &motion_keys {
            let value = args.get(key).cloned().unwrap_or(Value::Null);
            let note = if key == cursor_overlay::motion_defaults::KEY_PREFIX {
                "cleared every saved cursor motion default; applies to sessions started from now on"
            } else {
                "applies to sessions started from now on"
            };
            parts.push(format!("{key}={} ({note})", as_set(&value)));
            updated.insert(key.clone(), value);
        }
        if let Some(key) = badge_key {
            let value = args.get(&key).cloned().unwrap_or(Value::Null);
            parts.push(format!("{key}={} (applies now)", as_set(&value)));
            updated.insert(key, value);
        }
        if let Some(dim) = args.opt_u64("max_image_dimension") {
            cfg.max_image_dimension = dim as u32;
            if let Err(e) = pip_preview::write_config_key("max_image_dimension", Value::from(dim)) {
                tracing::warn!("set_config: failed to persist max_image_dimension: {e}");
            }
            parts.push(format!("max_image_dimension={dim}"));
            updated.insert("max_image_dimension".into(), Value::from(dim));
        }
        if let Some(enabled) = args.get("experimental_pip").and_then(|v| v.as_bool()) {
            if let Err(e) = pip_preview::write_config_key("experimental_pip", Value::Bool(enabled))
            {
                return ToolResult::error(format!("failed to persist experimental_pip: {e}"));
            }
            parts.push(format!("experimental_pip={enabled} (next restart)"));
            updated.insert("experimental_pip".into(), Value::Bool(enabled));
        }
        if let Some(geom) = args.opt_str("experimental_pip_geometry") {
            if pip_preview::PipGeometry::parse(&geom).is_none() {
                return ToolResult::error(format!(
                    "experimental_pip_geometry `{geom}` is not a valid WxH or WxH+X+Y string"
                ));
            }
            if let Err(e) = pip_preview::write_config_key(
                "experimental_pip_geometry",
                Value::String(geom.clone()),
            ) {
                return ToolResult::error(format!(
                    "failed to persist experimental_pip_geometry: {e}"
                ));
            }
            parts.push(format!("experimental_pip_geometry={geom} (next restart)"));
            updated.insert("experimental_pip_geometry".into(), Value::String(geom));
        }
        let msg = if parts.is_empty() {
            "Config unchanged (no known parameters).".to_owned()
        } else {
            format!("Config updated: {}", parts.join(", "))
        };
        ToolResult::text(msg).with_structured(Value::Object(updated))
    }
}

// ── zoom ──────────────────────────────────────────────────────────────────────

pub struct ZoomTool {
    state: Arc<ToolState>,
}
static ZOOM_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ZoomTool {
    fn def(&self) -> &ToolDef {
        ZOOM_DEF.get_or_init(|| ToolDef {
            name: "zoom".into(),
            description: "Capture a cropped JPEG of a window region from (x1,y1) to (x2,y2), in \
                pixels of this window's most recent get_window_state screenshot, padded by 20% \
                of the region's width and height on each side (clamped to the window). Output \
                is at most 500 px wide and never enlarged past the screenshot's own pixels. \
                Corners, not a size: x1,y1 is the top-left and x2,y2 the bottom-right (x,y,width,height is also accepted and converted). The result echoes the padded region the image actually shows, in the same screenshot pixels.\n\n\
                After a zoom, pass from_zoom=true to click, double_click, right_click, drag or \
                a mouse_button / mouse_drag call to auto-translate coordinates back to \
                full-window space. Coordinate actions return \
                `screenshot_context_missing` when no current snapshot contains a \
                screenshot owned by this session. `from_zoom` actions return \
                `zoom_context_missing` when the zoom was never created or was replaced; call \
                `get_window_state`, then `zoom`, again on the same connection.".into(),
            input_schema: json!({
                "type":"object","required":["window_id","x1","y1","x2","y2"],"properties":{
                    "window_id":{"type":"integer","description":"Window id of the window captured by get_window_state."},
                    "pid":{"type":"integer","description":"Optional target pid. When omitted, the driver resolves the unique current snapshot for this session and window."},
                    "x1":{"type":"number","description":"Left edge of the region in window-local screenshot pixels."},
                    "y1":{"type":"number","description":"Top edge of the region in window-local screenshot pixels."},
                    "x2":{"type":"number","description":"Right edge of the region in window-local screenshot pixels."},
                    "y2":{"type":"number","description":"Bottom edge of the region in window-local screenshot pixels."}
                },"additionalProperties":false
            }),
            read_only: true, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let xid = match args.require_u64("window_id") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let requested_pid = match args.get("pid") {
            None => None,
            Some(value) => match value.as_u64().and_then(|pid| i32::try_from(pid).ok()) {
                Some(pid) => Some(pid),
                None => return ToolResult::error("pid must be a positive 32-bit integer"),
            },
        };
        let session_id = args.opt_str("_session_id");
        let (pid, screenshot) = match self.state.snapshots.screenshot_context_for_zoom(
            requested_pid,
            xid,
            session_id.as_deref(),
        ) {
            Ok(context) => context,
            Err(refusal) => return refusal,
        };
        let x1 = match args.opt_f64("x1") {
            Some(v) => v,
            None => return ToolResult::error("Missing x1"),
        };
        let y1 = match args.opt_f64("y1") {
            Some(v) => v,
            None => return ToolResult::error("Missing y1"),
        };
        let x2 = match args.opt_f64("x2") {
            Some(v) => v,
            None => return ToolResult::error("Missing x2"),
        };
        let y2 = match args.opt_f64("y2") {
            Some(v) => v,
            None => return ToolResult::error("Missing y2"),
        };
        if x2 <= x1 || y2 <= y1 {
            return ToolResult::error("x2 must be > x1 and y2 must be > y1");
        }

        let requested = (x1, y1, x2, y2);
        let scale = screenshot.scale;
        let (x1, y1, x2, y2) = (x1 * scale, y1 * scale, x2 * scale, y2 * scale);
        let state = self.state.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Route through the Wayland-aware window capture dispatcher so
            // pure-Wayland sessions surface a typed "per-window capture not
            // supported yet" error instead of accidentally calling the
            // X11-only path with a foreign-toplevel id.
            let image = crate::wayland::screenshot_window_image_dispatch(xid)?.into_dynamic()?;
            cursor_overlay::capture_utils::crop_image_to_jpeg(&image, x1, y1, x2, y2, 500)
        })
        .await;

        match result {
            Ok(Ok(crop)) => {
                if let Err(refusal) = state.snapshots.set_zoom(
                    pid,
                    session_id.as_deref(),
                    ZoomContext {
                        screenshot,
                        origin_x: crop.origin_x,
                        origin_y: crop.origin_y,
                        scale_inv: crop.scale_inv,
                    },
                ) {
                    return refusal;
                }
                use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
                let b64 = B64.encode(&crop.jpeg_bytes);
                let (w, h) = (crop.out_w, crop.out_h);
                // The padded crop the image shows, back in the caller's
                // screenshot pixels (the crop ran on the native window).
                let padded = (
                    crop.origin_x / scale,
                    crop.origin_y / scale,
                    (crop.origin_x + f64::from(crop.crop_w)) / scale,
                    (crop.origin_y + f64::from(crop.crop_h)) / scale,
                );
                let (rx1, ry1, rx2, ry2) = requested;
                let (px1, py1, px2, py2) = padded;
                use pincer_core::protocol::Content;
                ToolResult {
                    content: vec![
                        Content::image_jpeg(b64),
                        Content::text(format!(
                            "Zoom of ({rx1:.0},{ry1:.0})-({rx2:.0},{ry2:.0}): the image shows \
                             ({px1:.0},{py1:.0})-({px2:.0},{py2:.0}) with the padding, in \
                             screenshot pixels, as a {w}x{h} px JPEG."
                        )),
                    ],
                    is_error: None,
                    // Surface 7: `mime_type` mirrors the MCP image part's `mimeType`
                    // onto the structured payload (additive — `format` stays).
                    structured_content: Some(json!({
                        "width": w, "height": h, "format": "jpeg",
                        "mime_type": "image/jpeg",
                        "shown_region": {"x1": px1, "y1": py1, "x2": px2, "y2": py2},
                    })),
                    action_record: None,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Zoom failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── capture_series ────────────────────────────────────────────────────────────

/// The whole desktop, in the desktop action frame `get_desktop_state` uses.
struct DesktopFrameSource;

impl pincer_core::capture_series::FrameSource for DesktopFrameSource {
    fn frame_size(&self) -> anyhow::Result<Option<(u32, u32)>> {
        // On X11 the desktop action frame is the root window. Wayland frames
        // are measured on the first capture.
        if crate::wayland::is_wayland() {
            Ok(None)
        } else {
            x11_screen_size().map(Some)
        }
    }

    fn capture(
        &self,
        region: Option<pincer_core::image_utils::PixelRect>,
    ) -> anyhow::Result<pincer_core::capture_series::SourceFrame> {
        if let (Some(region), false) = (region, crate::wayland::is_wayland()) {
            // X11 maps the root window 1:1 onto the action frame, so read
            // only the region from it instead of the whole screen.
            let frame_size = x11_screen_size()?;
            let origin = (i32::try_from(region.x)?, i32::try_from(region.y)?);
            let (image, _) = cursor_overlay::capture_exclusion::capture_excluding_overlays(
                &crate::overlay_capture::OverlayExcluder,
                |hidden| {
                    let rgba = crate::capture::screenshot_root_region_rgba(
                        origin.0,
                        origin.1,
                        region.width,
                        region.height,
                    )?;
                    Ok::<_, anyhow::Error>(crate::overlay_capture::verify_hidden_image_at(
                        pincer_core::image_utils::CapturedImage::Rgba(rgba),
                        hidden,
                        origin,
                    ))
                },
            )?;
            return Ok(pincer_core::capture_series::SourceFrame {
                image,
                origin: (region.x, region.y),
                frame_size,
            });
        }
        let frame = capture_desktop_action_frame()?;
        Ok(pincer_core::capture_series::SourceFrame {
            image: frame.image,
            origin: (0, 0),
            frame_size: (frame.width, frame.height),
        })
    }

    fn label(&self) -> String {
        "desktop".into()
    }
}

/// One window at native size, captured the way `get_window_state` does.
struct WindowFrameSource {
    pid: u32,
    xid: u64,
    /// The native size when a probe already measured it.
    known_size: Option<(u32, u32)>,
}

impl WindowFrameSource {
    fn grab(&self) -> anyhow::Result<pincer_core::image_utils::CapturedImage> {
        let overlays = if crate::wayland::is_wayland() {
            WindowOverlays::default()
        } else {
            window_overlays(self.pid, self.xid)
        };
        capture_window_image(self.xid, self.pid, &overlays).map_err(|error| {
            if !crate::wayland::is_wayland() && !crate::x11::window_exists(self.xid) {
                anyhow::anyhow!(
                    "Window target pid {}, window_id {} is stale or no longer running; refresh list_windows.",
                    self.pid,
                    self.xid
                )
            } else {
                error
            }
        })
    }
}

impl pincer_core::capture_series::FrameSource for WindowFrameSource {
    fn frame_size(&self) -> anyhow::Result<Option<(u32, u32)>> {
        if let Some(size) = self.known_size {
            return Ok(Some(size));
        }
        // Only a capture tells the exact native size, so probe once.
        self.grab()?.dimensions().map(Some)
    }

    fn capture(
        &self,
        _region: Option<pincer_core::image_utils::PixelRect>,
    ) -> anyhow::Result<pincer_core::capture_series::SourceFrame> {
        let image = self.grab()?;
        let frame_size = image.dimensions()?;
        Ok(pincer_core::capture_series::SourceFrame {
            image,
            origin: (0, 0),
            frame_size,
        })
    }

    fn label(&self) -> String {
        format!("window {} of pid {}", self.xid, self.pid)
    }
}

pub struct CaptureSeriesTool {
    state: Arc<ToolState>,
}
static CAPTURE_SERIES_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for CaptureSeriesTool {
    fn def(&self) -> &ToolDef {
        CAPTURE_SERIES_DEF.get_or_init(|| ToolDef {
            name: pincer_core::capture_series::TOOL_NAME.into(),
            description: "Take screenshots on a schedule: `count` frames `interval_ms` apart \
                (default 5 x 1000 ms) or at `offsets_ms` from the start, all timed from one \
                start so delays never accumulate. Captures the desktop, or one window with \
                pid + window_id. `region` crops each frame, then `max_image_dimension` caps \
                its long edge. A region is in the pixels of the image you read it off: this \
                session's latest get_desktop_state image for the desktop, the window's latest \
                get_window_state screenshot for a window (with native_pixels:true, full-size \
                desktop or native window pixels). `return_frames`: all, changed (differs from the last kept frame in more than \
                `change_threshold` of its pixels) or last. Inline replies carry at most \
                `max_inline_images`; `output:\"files\"` writes new frame-0001.png, ... into \
                `out_dir` instead. Each frame reports t_ms, and late when the previous capture \
                overran its slot. `stop_after_unchanged_ms` ends the series early once no frame \
                has differed from the one before it for that long. At most 120 frames within \
                600000 ms."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "pid":{"type":"integer","description":"Window owner pid; with window_id, capture that window. Omit both for the desktop."},
                "window_id":{"type":"integer","description":"Window id from list_windows, with pid."},
                "count":{"type":"integer","minimum":1,"maximum":120,"description":"Frames to take (default 5). Not with offsets_ms."},
                "interval_ms":{"type":"integer","minimum":0,"maximum":60000,"description":"Time between frame starts (default 1000; 0 = back to back). Not with offsets_ms."},
                "offsets_ms":{"type":"array","items":{"type":"integer","minimum":0},"minItems":1,"maxItems":120,"description":"Strictly increasing capture times in ms from the start, instead of count and interval_ms."},
                "region": pincer_core::capture_series::region_schema(
                    "Crop {x,y,width,height} in pixels of this session's latest get_desktop_state image (desktop), or of the window's latest get_window_state screenshot (pid/window_id); mapped to the frame for you. Must lie inside that image."
                ),
                "native_pixels": native_pixels_schema("true: region is in full-size desktop pixels (physical screen pixels on X11), or native window pixels with pid/window_id, instead. Default false."),
                "max_image_dimension":{"type":"integer","minimum":0,"description":"Long-edge cap applied after the crop (0 or omitted: none)."},
                "format":{"type":"string","enum":["png","jpeg"],"description":"Image format (default png)."},
                "quality":{"type":"integer","minimum":1,"maximum":100,"description":"JPEG quality (default 80)."},
                "output":{"type":"string","enum":["inline","files"],"description":"inline (default) returns the images; files writes them to out_dir."},
                "out_dir":{"type":"string","description":"Absolute path of an existing directory, not a symlink. Frame files that already exist are never overwritten; the call refuses instead."},
                "return_frames":{"type":"string","enum":["all","changed","last"],"description":"Which frames to return (default all)."},
                "max_inline_images":{"type":"integer","minimum":1,"maximum":30,"description":"Most images in an inline reply (default 10). For changed, the first and the latest frames are kept."},
                "change_threshold":{"type":"number","minimum":0,"maximum":1,"description":"Fraction of pixels that must differ for a frame to count as changed (default 0.001), measured on the cropped, capped frame."},
                "stop_after_unchanged_ms":{"type":"integer","minimum":0,"maximum":600000,"description":"End early once no frame has differed from the previous frame (per change_threshold) for this long. The first frame starts the clock and any change restarts it. The reply then reports stopped_early and stop_reason."}
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: false,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::capture_series::{self, FrameSource, SeriesRequest};
        let mut request = match SeriesRequest::parse(&args) {
            Ok(request) => request,
            Err(message) => return capture_series::invalid_arguments(&message),
        };
        let source: Arc<dyn FrameSource> = match (args.get("pid"), args.get("window_id")) {
            (None, None) => {
                if let Some(region) = request.region {
                    match self.state.desktop_region_arg(&args, region) {
                        Ok((region, _)) => request.region = Some(region),
                        Err(message) => return capture_series::invalid_arguments(&message),
                    }
                }
                Arc::new(DesktopFrameSource)
            }
            (Some(pid), Some(window_id)) => {
                let Some(pid) = pid
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .filter(|pid| *pid > 0)
                else {
                    return capture_series::invalid_arguments(
                        "`pid` must be a positive 32-bit integer",
                    );
                };
                let Some(xid) = window_id.as_u64().filter(|xid| *xid > 0) else {
                    return capture_series::invalid_arguments(
                        "`window_id` must be a positive integer",
                    );
                };
                let current = tokio::task::spawn_blocking(move || {
                    let meta = crate::wayland::list_windows_dispatch(Some(pid))
                        .into_iter()
                        .find(|w| w.xid == xid);
                    crate::proc_fs::is_process_live(pid)
                        && window_target_matches(pid, xid, meta.as_ref())
                })
                .await
                .unwrap_or(false);
                if !current {
                    return ToolResult::error(format!(
                        "Window target pid {pid}, window_id {xid} is stale or no longer running; refresh list_windows."
                    ));
                }
                let mut source = WindowFrameSource {
                    pid,
                    xid,
                    known_size: None,
                };
                if let Some(region) = request.region.filter(|_| !native_pixels_requested(&args)) {
                    // Screenshot pixels of the window's latest get_window_state
                    // read; the series crops native window pixels.
                    let scale = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                        Ok(scale) => scale,
                        Err(refusal) => return refusal,
                    };
                    let probe = WindowFrameSource {
                        pid,
                        xid,
                        known_size: None,
                    };
                    let native = match tokio::task::spawn_blocking(move || probe.frame_size()).await
                    {
                        Ok(Ok(size)) => size,
                        Ok(Err(error)) => return ToolResult::error(format!("{error:#}")),
                        Err(error) => return ToolResult::error(format!("Task error: {error}")),
                    };
                    source.known_size = native;
                    request.region = Some(window_region_from_screenshot(region, scale, native));
                }
                Arc::new(source)
            }
            _ => {
                return capture_series::invalid_arguments(
                    "pass both pid and window_id for a window, or neither for the desktop",
                )
            }
        };
        capture_series::run(request, source).await
    }
}

// ── registry ─────────────────────────────────────────────────────────────────

pub fn build_registry_with_provider(
    compat: bool,
    provider: Option<std::sync::Arc<dyn pincer_core::consent::ProtectedConsentProvider>>,
) -> ToolRegistry {
    let mut r = ToolRegistry::new_with_protected_consent_provider(provider);
    let state = ToolState::new_with_capture_service(r.capture_service());
    if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let recording_state = Arc::downgrade(&state);
        r.recording
            .set_pixel_point_fn(move |args, window_id, pid, mut x, mut y| {
                let state = recording_state.upgrade()?;
                if let Some(pid) = pid {
                    let pid = u32::try_from(pid).ok()?;
                    // Match ClickTool's screenshot and zoom conversion before
                    // translating to the recording's full-output image.
                    if args.bool_or("from_zoom", false) {
                        (x, y) = state
                            .zoom_context(args, pid, args.opt_u64("window_id"))
                            .ok()?
                            .zoom_to_window(x, y);
                    } else if let Ok(ratio) =
                        screenshot_scale(&state, args, pid, args.opt_u64("window_id"))
                    {
                        x *= ratio;
                        y *= ratio;
                    }
                }
                crate::recording_hooks::hyprland_pixel_recording_point(window_id, pid, x, y)
            });
    }
    let cursor_outcome_reader = {
        let cursor_registry = state.cursor_registry.clone();
        pincer_core::session::register_scoped_cursor_outcome_reader(std::sync::Arc::new(
            move |session_id| {
                let state = cursor_registry.get(session_id);
                let motion_customized = state.is_some()
                    && crate::overlay::current_motion_for(session_id)
                        != cursor_overlay::MotionConfig::default();
                let active_cursor_count = cursor_registry
                    .all_states()
                    .iter()
                    .filter(|state| state.config.cursor_id != "default")
                    .count()
                    .max(1);
                match state {
                    Some(state) => pincer_core::session::bounded_cursor_outcome(
                        true,
                        state.config.enabled,
                        crate::overlay::is_visible_for_session(session_id),
                        Some(state.config.theme_id.as_str()),
                        motion_customized,
                        active_cursor_count,
                    ),
                    None => pincer_core::session::bounded_cursor_outcome(
                        false,
                        false,
                        false,
                        None,
                        false,
                        active_cursor_count,
                    ),
                }
            },
        ))
    };
    let session_end_hook = {
        let cursor_registry = state.cursor_registry.clone();
        let state_for_session_end = state.clone();
        pincer_core::session::register_scoped_fallible_session_end_hook(
            "linux_input",
            move |session_id| {
                if crate::wayland::is_wayland() {
                    release_mouse_hold_for_session(
                        &state_for_session_end,
                        session_id,
                        |cursor_id, held| {
                            crate::wayland::persistent_vptr::release(cursor_id, held.button)
                        },
                    )?;
                } else {
                    // A foreground hold is a real XTest button on the core
                    // pointer: release it so the user's pointer is not left
                    // pressed. Best effort; the hold is dropped either way.
                    let _ = release_mouse_hold_for_session(
                        &state_for_session_end,
                        session_id,
                        |_, held| {
                            if held.foreground {
                                crate::input::send_button_xtest_desktop(None, held.button, false)
                            } else {
                                Ok(())
                            }
                        },
                    );
                    clear_mouse_hold_after_release(&state_for_session_end, session_id);
                }

                state_for_session_end
                    .snapshots
                    .retire_session_screenshots(session_id);
                cursor_registry.remove(session_id);
                crate::overlay::remove_cursor(session_id.to_owned());
                crate::input::forget_master_pointer(session_id);
                crate::wayland::hyprland_input::cleanup_session(session_id);
                Ok(())
            },
        )
    };
    let session_revive_hook =
        pincer_core::session::register_scoped_session_revive_hook(move |session_id| {
            crate::overlay::revive_cursor(session_id.to_owned());
        });
    r.retain_cursor_outcome_reader(cursor_outcome_reader);
    r.retain_session_end_hook(session_end_hook);
    r.retain_session_revive_hook(session_revive_hook);
    if let Some(runtime_scope) = pincer_core::tool::current_dispatch_runtime_scope() {
        let prefix = format!("__pincer_runtime_{runtime_scope}:");
        let cursor_registry = state.cursor_registry.clone();
        let capture_service = state.capture_service.clone();
        r.retain_runtime_cleanup(move || {
            crate::capture_action_frame::retire_runtime(&capture_service);
            crate::wayland::hyprland_input::cleanup_runtime(&prefix);
            for cursor in cursor_registry
                .all_states()
                .into_iter()
                .filter(|cursor| cursor.config.cursor_id.starts_with(&prefix))
            {
                cursor_registry.remove(&cursor.config.cursor_id);
                crate::overlay::remove_cursor(cursor.config.cursor_id);
            }
        });
    }
    r.register(Box::new(ListAppsTool));
    r.register(Box::new(ListWindowsTool));
    r.register(Box::new(GetWindowStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(pincer_core::expectation::VerifyStateTool::new(
        std::sync::Arc::new(pincer_core::expectation::ToolObservationProvider::new(
            std::sync::Arc::new(ListWindowsTool),
            std::sync::Arc::new(GetWindowStateTool {
                state: state.clone(),
            }),
        )),
    )));
    r.register(Box::new(LaunchAppTool));
    // AppImage run folders a crash or an earlier driver left behind.
    #[cfg(not(test))]
    if let Some(root) = appimage::run_root() {
        let _ = std::thread::Builder::new()
            .name("pincer-appimage-sweep".into())
            .spawn(move || {
                appimage::sweep_run_dirs(&root);
            });
    }
    let browser_engine = pincer_core::browser::BrowserEngine::new_with_runtime_services(
        Arc::new(crate::browser_platform::LinuxBrowserPlatform::new(
            state.cursor_registry.clone(),
        )),
        r.approval_broker(),
        r.protected_resource_ownership(),
    );
    r.register(Box::new(KillAppTool {
        browsers: browser_engine.clone(),
    }));
    let pid_window_candidates: PidWindowGuardParts = (
        Arc::new(pid_window_target_candidates),
        desktop_point_window_resolver(),
        pid_fallback_window_resolver(),
        snapshot_window_resolver(state.clone()),
        token_pid_resolver(state.clone()),
    );
    // run_steps gives a step that carries only an element_token the window
    // the token was read from.
    let token_state = state.clone();
    r.set_element_token_window_resolver(Arc::new(move |token| {
        let (pid, window_id) = token_state.snapshots.window_for_token(token)?;
        Some((i64::from(pid), window_id))
    }));
    r.register(pid_window_guarded(BringToFrontTool, &pid_window_candidates));
    r.register(Box::new(SetWindowFrameTool));
    r.register(Box::new(InvokeMenuTool));
    r.register(pid_window_guarded(
        ClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DoubleClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        RightClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonDownTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseDragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonUpTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    // Offered only where it can run: every call is refused elsewhere (KDE
    // Plasma X11, XWayland, Xvfb, no /dev/uinput, native Wayland without
    // the cua-compositor socket), and it has no foreground fallback. The
    // probe opens no device.
    match parallel_drag_unavailable_reason(
        crate::wayland::is_inject_mode(),
        crate::wayland::is_wayland(),
        crate::input::real_pointer_unavailable_reason,
    ) {
        None => r.register(Box::new(ParallelMouseDragTool {
            state: state.clone(),
        })),
        Some(reason) => tracing::debug!("parallel_mouse_drag not offered: {reason}"),
    }
    r.register(pid_window_guarded(
        TypeTextTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        PressKeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        HotkeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        SetValueTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        ScrollTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    pincer_core::clipboard::register_clipboard_tools(
        &mut r,
        Arc::new(crate::clipboard::LinuxClipboard::new()),
    );
    // `screenshot` removed - see the matching comment in
    // platform-windows/src/tools/impl_.rs::build_registry. Canonical
    // screenshot path is `get_window_state` (it returns one by default).
    let _ = compat;
    r.register(Box::new(GetScreenSizeTool));
    r.register(Box::new(GetDesktopStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetCursorPositionTool));
    r.register(Box::new(MoveCursorTool {
        state: state.clone(),
    }));
    r.register(Box::new(MoveAlongPathTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorEnabledV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorMotionV2Tool));
    r.register(Box::new(GetAgentCursorStateV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorThemeTool {
        state: state.clone(),
    }));
    r.register(Box::new(CheckPermissionsTool));
    // `health_report` — single-call cross-platform driver diagnostics.
    // Stable schema_version="1" contract for downstream consumers. Linux skips
    // tcc_* and bundle_identity with "not applicable on Linux".
    r.register(Box::new(pincer_core::health_report::HealthReportTool::new(
        std::sync::Arc::new(crate::health_report::LinuxHealthProvider),
    )));
    r.register(Box::new(GetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetAccessibilityTreeTool));
    r.register(Box::new(ZoomTool {
        state: state.clone(),
    }));
    r.register(Box::new(CaptureSeriesTool {
        state: state.clone(),
    }));
    // `type_text_chars` needs no tool here: pincer-core's registry resolves
    // it to `type_text` at invoke time and keeps it out of tools/list.
    pincer_core::browser::register_browser_tools(&browser_engine, &mut r);
    r.register_recording_tools();
    r.register_session_tools();
    r
}

#[cfg(test)]
mod click_button_schema_tests;

#[cfg(test)]
mod pid_window_target_tests;

#[cfg(test)]
mod session_cursor_target_tests;

#[cfg(test)]
mod display_only_tests;
#[cfg(test)]
mod since_screenshot_default_tests;
#[cfg(test)]
mod window_capture_dimension_tests;

#[cfg(test)]
mod desktop_capture_frame_tests;

#[cfg(test)]
mod background_budget_tests;

#[cfg(test)]
mod background_keyboard_route_tests;

#[cfg(test)]
mod visibility_tests;

#[cfg(test)]
mod result_wording_tests;
