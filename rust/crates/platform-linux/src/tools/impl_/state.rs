//! State the tools share: the driver config, element snapshots, capture service,
//! agent cursors and held mouse buttons.

use super::*;

// ── DriverConfig ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DriverConfig {
    pub max_image_dimension: u32,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            max_image_dimension: 1568,
        }
    }
}

/// Load `DriverConfig` from `~/.pincer/config.json`, falling back to
/// defaults for any missing/malformed keys. Called once at `ToolState`
/// construction (i.e. on every fresh `pincer call` process) so that a
/// prior `set_config max_image_dimension=...` survives across stateless
/// one-shot invocations, matching the macOS daemon's startup load. Keys are
/// read one at a time, so unknown keys in the file (such as a `capture_mode`
/// saved by an older build) are ignored.
pub fn load_driver_config() -> DriverConfig {
    let mut cfg = DriverConfig::default();
    if let Some(v) = pip_preview::read_config_value("max_image_dimension").and_then(|v| v.as_u64())
    {
        if let Ok(v32) = u32::try_from(v) {
            cfg.max_image_dimension = v32;
        }
    }
    cfg
}

pub struct ToolState {
    pub snapshots: Arc<Snapshots>,
    pub cursor_registry: Arc<CursorRegistry>,
    pub capture_service: Arc<pincer_core::capture_runtime::CaptureService>,
    pub mouse_hold: std::sync::Mutex<std::collections::HashMap<String, MouseHoldState>>,
    pub config: Arc<RwLock<DriverConfig>>,
    /// Each session's latest get_desktop_state image, for a later desktop
    /// `region` read off it (keyed by `_session_id`).
    desktop_images: std::sync::Mutex<std::collections::HashMap<String, DesktopImageFrame>>,
}

/// What a get_desktop_state image shows: its size and the full-size desktop
/// rectangle it was scaled from, so a region read off the image maps back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DesktopImageFrame {
    /// The capture binding's session generation: an ended and restarted
    /// session does not inherit the image.
    pub(super) generation: u64,
    /// The image the caller was given, in its own pixels.
    pub(super) image: (u32, u32),
    /// The part of the desktop it shows, in full-size desktop pixels.
    pub(super) shows: pincer_core::image_utils::PixelRect,
}

impl ToolState {
    /// Remember the get_desktop_state image this call returned.
    pub(super) fn record_desktop_image(
        &self,
        args: &Value,
        image: (u32, u32),
        shows: pincer_core::image_utils::PixelRect,
    ) {
        let (Some(session), Ok(binding)) = (
            args.get("_session_id").and_then(Value::as_str),
            self.capture_service.binding_from_args(args),
        ) else {
            return;
        };
        self.desktop_images.lock().unwrap().insert(
            session.to_owned(),
            DesktopImageFrame {
                generation: binding.session_generation(),
                image,
                shows,
            },
        );
    }

    /// The calling session's latest get_desktop_state image, if it has one.
    fn desktop_image(&self, args: &Value) -> Option<DesktopImageFrame> {
        let session = args.get("_session_id").and_then(Value::as_str)?;
        let generation = self
            .capture_service
            .binding_from_args(args)
            .ok()?
            .session_generation();
        self.desktop_images
            .lock()
            .unwrap()
            .get(session)
            .copied()
            .filter(|frame| frame.generation == generation)
    }

    /// A desktop `region` argument in full-size desktop pixels. By default
    /// it was read off the session's latest get_desktop_state image and is
    /// mapped back; `native_pixels:true` (or no earlier image, whose frame
    /// would be the full-size one) takes it as it is. `Ok` also says
    /// whether it was mapped.
    pub(super) fn desktop_region_arg(
        &self,
        args: &Value,
        region: pincer_core::image_utils::PixelRect,
    ) -> Result<(pincer_core::image_utils::PixelRect, bool), String> {
        if native_pixels_requested(args) {
            return Ok((region, false));
        }
        match self.desktop_image(args) {
            Some(frame) => desktop_region_from_image(region, &frame).map(|rect| (rect, true)),
            None => Ok((region, false)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MouseHoldState {
    pub pid: u32,
    pub xid: u64,
    pub button: u8,
    pub x: f64,
    pub y: f64,
    /// Pressed with `delivery_mode:"foreground"` on X11: a real XTest button
    /// on the core pointer, so motion and release go the same way.
    pub foreground: bool,
}

impl ToolState {
    /// Test fixture: production registries share one capture service.
    #[cfg(test)]
    pub fn new() -> Arc<Self> {
        Self::new_with_capture_service(Arc::new(
            pincer_core::capture_runtime::CaptureService::default(),
        ))
    }

    pub(super) fn new_with_capture_service(
        capture_service: Arc<pincer_core::capture_runtime::CaptureService>,
    ) -> Arc<Self> {
        let snapshots = Arc::new(Snapshots::new());
        pincer_core::snapshot_store::register_runtime_store(&snapshots);
        Arc::new(Self {
            snapshots,
            cursor_registry: Arc::new(CursorRegistry::new()),
            capture_service,
            mouse_hold: std::sync::Mutex::new(Default::default()),
            config: Arc::new(RwLock::new(load_driver_config())),
            desktop_images: std::sync::Mutex::new(Default::default()),
        })
    }

    pub(super) fn zoom_context(
        &self,
        args: &Value,
        pid: u32,
        window_id: Option<u64>,
    ) -> Result<ZoomContext, ToolResult> {
        self.snapshots.zoom(
            pid as i32,
            window_id,
            args.get("_session_id").and_then(Value::as_str),
        )
    }
}

pub(super) fn screenshot_scale(
    state: &ToolState,
    args: &Value,
    pid: u32,
    window_id: Option<u64>,
) -> Result<f64, ToolResult> {
    state
        .snapshots
        .screenshot_scale(pid as i32, window_id, args)
}
