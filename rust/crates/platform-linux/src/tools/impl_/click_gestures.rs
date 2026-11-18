//! double_click and right_click, which hand the call to click's pipeline.

use super::*;

// ── double_click ──────────────────────────────────────────────────────────────

pub struct DoubleClickTool {
    pub(super) state: Arc<ToolState>,
}
static DCLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for DoubleClickTool {
    fn def(&self) -> &ToolDef {
        DCLICK_DEF.get_or_init(|| ToolDef {
            name: "double_click".into(),
            description: "Double-click at (x,y) or at the center of an element_token's bounds. \
                Background delivery on X11 uses the session's virtual pointer (path mpx_pointer) \
                when it can, else synthetic XSendEvent (path x11_xsendevent), which some \
                toolkits read as a single click: check `path` and verify the result. Where no \
                focus-free pointer route exists (a GTK3 canvas or LibreOffice, for example) \
                background delivery is refused with background_unavailable: retry with \
                delivery_mode foreground. In background, a \
                point covered by another application's window is refused \
                (target_occluded), one covered by this app's own dialog is retargeted to it \
                (retargeted_to). No focus steal. Pass pid + window_id + x,y, or an element_token \
                (pid optional). \
                After a zoom call, pass from_zoom=true to auto-translate zoom-image coords.".into(),
            input_schema: json!({"type":"object","required":[],"properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id": cursor_id_schema(),
                "pid":{"type":"integer","description":"Target process ID. Required unless scope is \"desktop\" or element_token is given (the token carries the pid)."},
                "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                "x":{"type":"number","description":"Pixel X in the window's get_window_state screenshot (0..screenshot_width). Pass scope:\"desktop\" for get_desktop_state pixels."},
                "y":{"type":"number","description":"Pixel Y in the window's get_window_state screenshot (0..screenshot_height); see x."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Default \"window\". \"desktop\" with x,y and no pid/window_id double-clicks at get_desktop_state screen coordinates."},
                "coordinate_frame": coordinate_frame_schema(),
                "element_token": pincer_core::tool_schema::element_token_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        gesture_click(&self.state, args, ClickKind::Double).await
    }
}

// ── right_click ───────────────────────────────────────────────────────────────

pub struct RightClickTool {
    pub(super) state: Arc<ToolState>,
}
static RCLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for RightClickTool {
    fn def(&self) -> &ToolDef {
        RCLICK_DEF.get_or_init(|| ToolDef {
            name: "right_click".into(),
            description: "Right-click at window pixel (x, y) or an element_token. Pass pid + \
                window_id + x,y, or an element_token (pid optional); after a zoom, from_zoom=true \
                maps zoom-image coordinates \
                back. Background delivery does not steal focus; where no focus-free pointer \
                route exists (a GTK3 canvas or LibreOffice, for example) it is refused with \
                background_unavailable: retry with delivery_mode foreground. In background, a point \
                covered by another app's \
                window is refused (target_occluded), one covered by this app's own dialog goes to \
                the dialog (retargeted_to), and one outside the window is refused \
                (point_outside_window); results report window_point and screen_point. When Pincer \
                can identify the context menu it opened (in either delivery mode), the result \
                includes `popup: {window_id, \
                bounds, title}`: read it with get_window_state(pid, window_id) and click its \
                items by element_token. Without `popup`, a menu may still be open even though \
                window screenshots never show popup menus; press Escape on the window to close \
                it.".into(),
            input_schema: json!({"type":"object","required":[],"properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id": cursor_id_schema(),
                "pid":{"type":"integer","description":"Target process ID. Required unless scope is \"desktop\" or element_token is given (the token carries the pid)."},
                "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                "x":{"type":"number","description":"Pixel X in the window's get_window_state screenshot (0..screenshot_width). Pass scope:\"desktop\" for get_desktop_state pixels."},
                "y":{"type":"number","description":"Pixel Y in the window's get_window_state screenshot (0..screenshot_height); see x."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Default \"window\". \"desktop\" with x,y and no pid/window_id right-clicks at get_desktop_state screen coordinates."},
                "coordinate_frame": coordinate_frame_schema(),
                "element_token": pincer_core::tool_schema::element_token_schema(),
                "modifier": pincer_core::tool_schema::modifier_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        gesture_click(&self.state, args, ClickKind::Right).await
    }
}
