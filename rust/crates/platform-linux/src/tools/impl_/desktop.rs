//! get_screen_size and get_desktop_state, and the desktop action frame they
//! report.

use super::*;

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
pub(super) fn x11_screen_size() -> anyhow::Result<(u32, u32)> {
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
pub(super) fn normalize_desktop_capture_for_action_frame(
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
pub(super) struct DesktopActionFrame {
    pub(super) image: pincer_core::image_utils::CapturedImage,
    pub(super) width: u32,
    pub(super) height: u32,
    /// Capture pixels per action-frame pixel before normalization.
    scale_factor: f64,
    overlay_capture: cursor_overlay::capture_exclusion::AgentOverlayCapture,
    /// The Hyprland layout the image was composed from.
    hyprland_frame: Option<crate::wayland::hyprland::DesktopFrame>,
}

pub(super) fn capture_desktop_action_frame() -> anyhow::Result<DesktopActionFrame> {
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
    pub(super) state: Arc<ToolState>,
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
pub(super) fn desktop_window_lines(windows: &[crate::x11::WindowInfo], frame_scale: f64) -> String {
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
