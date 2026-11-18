//! capture_series: timed frames of a window or the desktop.

use super::*;

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
    pub(super) state: Arc<ToolState>,
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
