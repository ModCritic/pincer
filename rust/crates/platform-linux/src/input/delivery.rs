//! Shared delivery-mode logic for pincer Linux input tools.
//!
//! Mirrors macOS `tools::DeliveryMode` and Windows `input::delivery`: each
//! input tool accepts an optional `delivery_mode` field with exactly two modes
//! — the agent-selected rung of the best-effort-background ladder, passed per
//! call (never a stored setting):
//!
//! - `background` (DEFAULT) — inject without activating/raising the target.
//!   - **X11**: XTEST / XSendEvent / XInput2 MPX master-pointer no-focus-steal
//!     path (see `input::mod`). Lands on a backgrounded window without stealing
//!     focus — the direct analogue of macOS CGEvent / Windows PostMessage.
//!   - **Wayland**: libei via xdg-desktop-portal (`wayland::libei`). Wayland's
//!     security model has NO arbitrary per-window background targeting — libei
//!     injects to the compositor's input focus, so "background" cannot aim at a
//!     specific non-focused window the way X11/macOS/Windows can (this is a
//!     platform constraint, reported honestly, like macOS pixel input being
//!     driver-unverifiable). When no libei backend is available
//!     (`PORTAL_INPUT_ENABLED == false`) the tool returns a structured
//!     `background_unavailable` error so the caller can escalate to foreground.
//!
//! - `foreground` — activate the target first, then inject. On X11 the target
//!   stays active afterwards (see `input/foreground.rs`: restoring the prior
//!   window would close a menu the action just opened).
//!   - **X11**: EWMH `_NET_ACTIVE_WINDOW` client message (the `wmctrl -a`
//!     equivalent) raise + activate.
//!   - **Wayland**: compositor-specific activate where supported; else the
//!     libei-to-focus path with an honest note.
//!   The agent's explicit last resort: it moves focus to the target. Matches the macOS / Windows `foreground` rung.

use serde_json::Value;

/// The shared per-call delivery mode; see [`pincer_core::delivery`].
pub use pincer_core::delivery::DeliveryMode;

/// JSON-schema fragment for the `delivery_mode` field. Include this in every
/// input tool's `input_schema.properties.delivery_mode`. Two modes, matching
/// the macOS / Windows surface.
pub fn delivery_mode_schema() -> Value {
    // Source the SHAPE (`type` + `enum`) from the shared canon so it can't drift
    // from macOS / Windows, while keeping the Linux-specific Wayland/X11 prose
    // (the gate compares shape only — descriptions may vary per platform). The
    // `default` advertisement is additive on top of the canon shape.
    let mut v = pincer_core::tool_schema::delivery_mode_schema_with(
        "\"background\" (default) acts without activating or raising the window; on \
         Wayland it reaches only the focused window. Targets that cannot take \
         background input (Chromium/Electron, WebKitGTK, toolkits reachable only \
         through the focused widget, Wayland without libei) return \
         background_unavailable. \"foreground\" activates and raises the window (the \
         pointer may move) and acts; the window stays active afterwards. Use it only \
         when a background attempt is refused or unverifiable.",
    );
    v["default"] = serde_json::json!("background");
    v
}

/// Reason a `background` delivery cannot be performed on Wayland.
#[derive(Copy, Clone, Debug)]
pub enum BackgroundUnavailable {
    /// No libei backend (built without `portal-input`, or the portal session
    /// was denied / unavailable). Input has no actuator at all.
    NoLibeiBackend,
    /// X11/Chromium does not accept synthetic pointer or keyboard input
    /// addressed to an occluded, unfocused renderer without briefly moving
    /// focus, which background delivery forbids.
    ChromiumInput,
    /// The remaining backend can only inject into the globally focused widget.
    FocusedInputOnly,
    /// WebKitGTK rejects synthetic XSendEvent input and no real target-addressed
    /// pointer backend is available in this session.
    WebKitSyntheticInput,
}

impl BackgroundUnavailable {
    fn code(self) -> &'static str {
        match self {
            Self::NoLibeiBackend => "background_unavailable",
            Self::ChromiumInput => "background_unavailable",
            Self::FocusedInputOnly => "background_unavailable",
            Self::WebKitSyntheticInput => "background_unavailable",
        }
    }
    fn detail(self) -> &'static str {
        match self {
            Self::NoLibeiBackend => {
                "no libei input backend on this Wayland compositor (built without \
                 portal-input, or the xdg-desktop-portal RemoteDesktop session was \
                 unavailable/denied): synthetic input has no actuator"
            }
            Self::ChromiumInput => {
                "Chromium/Electron does not accept pointer or keyboard input \
                 addressed to an occluded, unfocused renderer through X11 \
                 background injection"
            }
            Self::FocusedInputOnly => {
                "the requested target has no focus-free input backend; the remaining input route can only deliver to the globally focused widget"
            }
            Self::WebKitSyntheticInput => {
                "WebKitGTK rejects synthetic XSendEvent input and this session has no real target-addressed pointer backend"
            }
        }
    }
}

/// Build the structured `background_unavailable` error returned when a
/// `delivery_mode:"background"` injection has no Wayland backend. Mirrors the
/// Windows silent-drop error so callers branch on `code` identically.
pub fn background_unavailable_error(
    reason: BackgroundUnavailable,
) -> pincer_core::protocol::ToolResult {
    let detail = reason.detail();
    pincer_core::delivery::background_unavailable_result(
        format!(
            "Background delivery is not available: {detail}. Retry this action with \
             delivery_mode:\"foreground\"; Pincer will activate the target window, \
             confirm it holds the input focus, and deliver real input to it."
        ),
        reason.code(),
        "background input is unavailable on this surface; retry this \
         action with delivery_mode:\"foreground\".",
        serde_json::json!({ "detail": detail }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_input_refusal_does_not_claim_an_x11_backend() {
        let result = background_unavailable_error(BackgroundUnavailable::FocusedInputOnly);
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["code"], "background_unavailable");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured["escalation"]["recommended"], "foreground");
        assert_eq!(
            structured["suggestion"],
            pincer_core::delivery::FOREGROUND_RETRY_SUGGESTION
        );
        let detail = structured["detail"].as_str().unwrap();
        assert!(detail.contains("globally focused widget"));
        assert!(!detail.contains("XTest"));
        assert!(!detail.contains("X11"));
        let text = result
            .content
            .iter()
            .filter_map(|content| match content {
                pincer_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("XTest"));
        assert!(!text.contains("X11"));
        assert!(text.contains("delivery_mode:\"foreground\""));
    }

    #[test]
    fn delivery_mode_schema_advertises_two_modes() {
        let s = delivery_mode_schema();
        let en: Vec<&str> = s["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(en, vec!["background", "foreground"]);
        assert_eq!(s["default"], "background");
        let description = s["description"]
            .as_str()
            .expect("delivery_mode description");
        assert!(!description.contains("bring_to_front"));
    }
}
