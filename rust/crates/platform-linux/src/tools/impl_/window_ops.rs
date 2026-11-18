//! Operations on running apps and their windows: kill_app, invoke_menu,
//! set_window_frame and bring_to_front.

use super::*;

// ── kill_app ──────────────────────────────────────────────────────────────────

/// Plain words for a `kill_app` observation a caller could misread: a
/// "zombie" reads as "not dead", but it has exited and only waits for its
/// parent to collect its exit status (desc-test round 1).
pub(super) fn kill_observation_note(observation: &str) -> &'static str {
    match observation {
        "zombie" => {
            "; the process has exited and only waits for its parent to reap it, so ps may \
             list it as <defunct> for a moment"
        }
        _ => "",
    }
}

/// What `kill_app` adds after it ended an isolated browser that
/// `browser_prepare` launched.
const KILLED_PREPARED_BROWSER_NOTE: &str = "; it was the isolated browser browser_prepare \
     launched, so Pincer collected its exit and ended that browser (call browser_prepare \
     again for a new one)";

pub struct KillAppTool {
    /// Holds the isolated browsers `browser_prepare` launched, whose exit
    /// only the engine's own child handle may collect.
    pub(super) browsers: Arc<pincer_core::browser::BrowserEngine>,
}
static KILL_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for KillAppTool {
    fn def(&self) -> &ToolDef {
        KILL_DEF.get_or_init(|| ToolDef {
            name: "kill_app".into(),
            description: "Force-terminate a process by pid with SIGKILL and confirm that it \
                exited. Unsaved work is lost, so close the app the normal way first: its Quit or \
                Close menu item through invoke_menu (e.g. [\"File\",\"Quit\"]), or hotkey \
                [\"alt\",\"F4\"] on its window; use kill_app when that does not end it. In \
                standard mode it terminates only a process Pincer launched (launch_app, \
                browser_prepare), from the session that launched it or, once that session has \
                ended, from any session; any other pid is refused with \
                foreign_process_termination_denied."
                .into(),
            input_schema: json!({"type":"object","required":["pid"],"properties":{
                "pid":{"type":"integer","description":"PID of the process to terminate."}
            },"additionalProperties":false}),
            read_only: false,
            destructive: true,
            idempotent: true,
            open_world: false,
        })
    }

    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if adapter_id != "process_control" {
            return Ok(None);
        }
        use pincer_core::browser::platform::BrowserPlatform;
        let pid = args
            .get("pid")
            .and_then(Value::as_i64)
            .filter(|pid| *pid > 0)
            .ok_or_else(|| "kill_app requires a positive integer pid".to_owned())?;
        let fingerprint = crate::browser_platform::LinuxBrowserPlatform::default()
            .process_fingerprint(pid)
            .await
            .map_err(|error| error.message)?;
        Ok(Some(json!({
            "kind": "process_instance",
            "fingerprint": fingerprint,
        })))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Some(expected) = args.get("_protected_process_fingerprint") {
            let current = match self
                .protected_resource_scope("process_control", &args)
                .await
            {
                Ok(Some(scope)) => scope["fingerprint"].clone(),
                Ok(None) => Value::Null,
                Err(message) => return kill_app_stale_process_refusal(message),
            };
            if current != *expected {
                return kill_app_stale_process_refusal(
                    "the process identity changed at the termination boundary".to_owned(),
                );
            }
        }
        let pid_i = match args.get("pid").and_then(|v| v.as_i64()) {
            Some(p) if p > 0 && p <= i32::MAX as i64 => p as i32,
            Some(_) => {
                return ToolResult::error("kill_app: `pid` must be a positive integer".to_string())
            }
            None => {
                return ToolResult::error(
                    "kill_app: missing required integer field `pid`".to_string(),
                )
            }
        };
        // Read the instance before signaling so the confirmation below can
        // tell its exit from a later process that reused the pid.
        let before = match crate::proc_fs::read_process_stat(pid_i as u32) {
            Ok(Some(before)) => before,
            Ok(None) => {
                return kill_app_failure(
                    pid_i,
                    "process_not_found",
                    format!("kill_app: pid {pid_i} does not exist; no signal was sent."),
                )
            }
            Err(error) => {
                return kill_app_failure(
                    pid_i,
                    "process_identity_unavailable",
                    format!(
                        "kill_app: cannot read /proc/{pid_i}/stat to verify termination: \
                         {error}; no signal was sent."
                    ),
                )
            }
        };
        // SAFETY: libc::kill is a thin syscall wrapper, no thread-safety concerns.
        let rc = unsafe { libc::kill(pid_i, libc::SIGKILL) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return kill_app_failure(
                pid_i,
                "signal_failed",
                format!(
                    "kill_app: kill(pid={pid_i}, SIGKILL) failed: {err}. \
                     The process may not exist, or the daemon lacks permission to signal it."
                ),
            );
        }
        // An accepted signal is not an exit: a sandbox (gVisor) can accept
        // SIGKILL for a process that keeps running. Report success only once
        // the same instance is gone.
        let deadline = tokio::time::Instant::now() + KILL_CONFIRM_TIMEOUT;
        loop {
            let now = crate::proc_fs::read_process_stat(pid_i as u32);
            if let Some(mut observation) = now
                .as_ref()
                .ok()
                .and_then(|now| crate::proc_fs::process_exit_observation(before, *now))
            {
                let mut note = kill_observation_note(observation);
                // An isolated browser from browser_prepare is this server's
                // child: it stays a zombie until the browser engine reaps it.
                if observation == "zombie" {
                    let browsers = self.browsers.clone();
                    let released = tokio::task::spawn_blocking(move || {
                        browsers.release_exited_browser(i64::from(pid_i), KILL_CONFIRM_TIMEOUT)
                    })
                    .await
                    .unwrap_or(false);
                    if released {
                        if let Ok(after) = crate::proc_fs::read_process_stat(pid_i as u32) {
                            observation = crate::proc_fs::process_exit_observation(before, after)
                                .unwrap_or(observation);
                        }
                        note = KILLED_PREPARED_BROWSER_NOTE;
                    }
                }
                return ToolResult::text(format!(
                    "✅ Terminated pid {pid_i} with SIGKILL (confirmed: {observation}){note}."
                ))
                .with_structured(json!({
                    "status": "terminated",
                    "effect": "confirmed",
                    "pid": pid_i,
                    "terminated": true,
                    "observation": observation,
                }));
            }
            if tokio::time::Instant::now() >= deadline {
                let detail = match now {
                    Err(error) => format!("/proc/{pid_i}/stat became unreadable: {error}"),
                    Ok(_) => format!(
                        "the same process was still alive {} ms later",
                        KILL_CONFIRM_TIMEOUT.as_millis()
                    ),
                };
                let mut result = kill_app_failure(
                    pid_i,
                    "termination_unconfirmed",
                    format!("kill_app: SIGKILL was accepted for pid {pid_i}, but {detail}."),
                );
                if let Some(structured) = result.structured_content.as_mut() {
                    structured["effect"] = json!("suspected_noop");
                }
                return result;
            }
            tokio::time::sleep(KILL_CONFIRM_INTERVAL).await;
        }
    }
}

/// How long `kill_app` waits for the signaled process to exit, and how often
/// it re-reads `/proc/<pid>/stat` meanwhile.
const KILL_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const KILL_CONFIRM_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

fn kill_app_failure(pid: i32, code: &str, message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "failed",
        "pid": pid,
        "terminated": false,
        "code": code,
        "message": message,
    }))
}

fn kill_app_stale_process_refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "refused",
        "refusal": {
            "code": "protected_resource_scope_stale",
            "message": message,
        }
    }))
}

// ── invoke_menu (Linux) ──────────────────────────────────────────────────────

pub struct InvokeMenuTool;

static INVOKE_MENU_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn normalized_menu_path(path: Vec<String>) -> Result<Vec<String>, String> {
    if path.is_empty() || path.len() > 16 {
        return Err("invoke_menu: path must contain between 1 and 16 segments".into());
    }
    path.into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let segment = segment.trim();
            if segment.is_empty() {
                Err(format!("invoke_menu: path segment {index} is empty"))
            } else {
                Ok(segment.to_owned())
            }
        })
        .collect()
}

/// The invoke_menu success result: what ran, then what became of the menu
/// Pincer opened to reach the item (closed, or still open with how to close
/// it), and whether the app answered the final action
/// ([`crate::atspi::native::MenuInvoked::unacknowledged`]).
pub(super) fn invoke_menu_result(
    route: &str,
    cleanup: &crate::atspi::native::MenuCleanup,
    unacknowledged: bool,
    pid: u32,
    window_id: u64,
    delivery_mode: &str,
) -> ToolResult {
    use crate::atspi::native::MenuCleanup;
    let mut text = format!(
        "Resolved the live native menu path and dispatched its final accessibility action \
         ({route})"
    );
    let mut structured = json!({
        "path": "ax",
        "verified": false,
        "effect": "unverifiable",
        "delivery_mode": delivery_mode,
    });
    match cleanup {
        MenuCleanup::NoneOpen => {}
        MenuCleanup::Closed { menu } => {
            text.push_str(&format!(
                ", then closed the {menu} menu it had opened to reach the item"
            ));
            structured["menu_closed"] = json!(menu);
        }
        MenuCleanup::StillOpen { menu } => {
            let popup = crate::input::popup_of_pid(pid);
            text.push_str(&format!(
                ". The {menu} menu Pincer opened to reach the item is still open{}, so pid {pid} \
                 holds a keyboard grab until it closes: press_key escape on window {window_id} \
                 closes it (with delivery_mode foreground if a background press is refused)",
                popup
                    .as_ref()
                    .map(|popup| format!(" ({})", popup.describe()))
                    .unwrap_or_default()
            ));
            structured["menu_left_open"] = json!(menu);
            if let Some(popup) = popup {
                structured["popup"] = popup.to_json();
            }
        }
    }
    if unacknowledged {
        text.push_str(&format!(
            ". The app did not answer the final action within {} s, as when the item opens a \
             modal dialog: check list_windows before calling again",
            crate::atspi::native::CALL_TIMEOUT.as_secs()
        ));
        structured["acknowledged"] = json!(false);
    }
    text.push_str("; verify the command's semantic effect from fresh state.");
    ToolResult::text(text).with_structured(structured)
}

/// The action record's evidence for a menu path that resolved.
fn menu_evidence(unacknowledged: bool) -> &'static str {
    if unacknowledged {
        "Every menu hop resolved uniquely; the final action was sent but not answered in time"
    } else {
        "Every menu hop resolved uniquely and AT-SPI accepted the final action"
    }
}

/// A refused invoke_menu call. The top-level `code` lets the text result
/// print the refusal once with `[code: ...]`, instead of the message a
/// second time inside `refusal` (a long label list would show twice).
fn menu_refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "refused",
        "code": "menu_path_unavailable",
        "refusal": { "code": "menu_path_unavailable", "message": message }
    }))
}

#[async_trait]
impl Tool for InvokeMenuTool {
    fn def(&self) -> &ToolDef {
        INVOKE_MENU_DEF.get_or_init(|| {
            let contract =
                pincer_contract::tool_contract("invoke_menu").expect("invoke_menu contract");
            ToolDef {
                name: contract.name,
                description: contract.description,
                input_schema: contract.input_schema,
                read_only: contract.annotations.read_only,
                destructive: contract.annotations.destructive,
                idempotent: contract.annotations.idempotent,
                open_world: contract.annotations.open_world,
            }
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::action_record::{
            ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
            EvidenceKind, RequestedDelivery,
        };

        // Menu paths are resolved and fired through AT-SPI, which needs no
        // window activation on X11: the default is background (focus-free,
        // under the focus guard). `delivery_mode:"foreground"` is the explicit
        // escalation that activates the window first, as every other tool.
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let mut args = args;
        if let Some(object) = args.as_object_mut() {
            // Not part of the closed InvokeMenuInput contract; consumed above.
            object.remove("delivery_mode");
        }
        let input: InvokeMenuInput = match parse_typed_input("invoke_menu", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let path = match normalized_menu_path(input.path) {
            Ok(path) => path,
            Err(error) => return menu_refusal(error),
        };
        let pid = input.pid;
        let window_id = input.window_id;
        if !crate::wayland::list_windows_dispatch(Some(pid))
            .iter()
            .any(|window| window.xid == window_id && window.pid == Some(pid))
        {
            return menu_refusal("invoke_menu: window_id does not belong to pid".into());
        }

        if !crate::wayland::is_wayland() && !delivery.is_foreground() {
            let outcome = tokio::task::spawn_blocking(move || {
                crate::input::focus_guard::guarded(Some(pid), || {
                    crate::atspi::native::invoke_menu_path_in(pid, window_id, &path)
                })
            })
            .await;
            return match outcome {
                Ok(Ok((invoked, guard))) => attach_focus_guard_opts(
                    invoke_menu_result(
                        "delivery_mode=background, focus untouched",
                        &invoked.cleanup,
                        invoked.unacknowledged,
                        pid,
                        window_id,
                        "background",
                    )
                    .with_action_record(
                        ActionExecutionRecord::builder(
                            ActionEffect::Unverifiable,
                            ActionTransport::LinuxAtSpiAction,
                            RequestedDelivery::Background,
                        )
                        .actual_delivery(ActualDelivery::Background)
                        .evidence(ActionEvidence {
                            kind: EvidenceKind::NativeApiResult,
                            detail: menu_evidence(invoked.unacknowledged).into(),
                        })
                        .build()
                        .expect("invoke_menu record is valid"),
                    ),
                    guard.as_ref(),
                    // A menu left open is named by the cleanup sentence.
                    !matches!(
                        invoked.cleanup,
                        crate::atspi::native::MenuCleanup::StillOpen { .. }
                    ),
                ),
                Ok(Err(error)) => menu_refusal(format!("invoke_menu: {error}")),
                Err(error) => menu_refusal(format!("invoke_menu: blocking task failed: {error}")),
            };
        }

        let activation = if crate::wayland::is_wayland() {
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::activate_window_for_input_target(window_id, Some(pid))
            })
            .await;
            match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => {
                    return menu_refusal(format!(
                        "invoke_menu: target window could not be activated: {error}"
                    ))
                }
                Err(error) => {
                    return menu_refusal(format!("invoke_menu: activation task failed: {error}"))
                }
            }
        } else {
            match tokio::task::spawn_blocking(move || {
                crate::input::x11_activate_window_persistent(window_id)
            })
            .await
            {
                Ok(Ok(prior)) => Some(prior),
                Ok(Err(error)) => {
                    return menu_refusal(format!(
                        "invoke_menu: target window could not be activated: {error}"
                    ))
                }
                Err(error) => {
                    return menu_refusal(format!("invoke_menu: activation task failed: {error}"))
                }
            }
        };

        let outcome = tokio::task::spawn_blocking(move || {
            let result = crate::atspi::native::invoke_menu_path_in(pid, window_id, &path);
            if let Some(Some(prior_window)) = activation {
                let _ = crate::input::x11_activate_window_persistent(prior_window);
            }
            result
        })
        .await;

        match outcome {
            Ok(Ok(invoked)) => invoke_menu_result(
                "delivery_mode=foreground",
                &invoked.cleanup,
                invoked.unacknowledged,
                pid,
                window_id,
                "foreground",
            )
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::LinuxAtSpiAction,
                    RequestedDelivery::Foreground,
                )
                .actual_delivery(ActualDelivery::Foreground)
                .evidence(ActionEvidence {
                    kind: EvidenceKind::NativeApiResult,
                    detail: menu_evidence(invoked.unacknowledged).into(),
                })
                .build()
                .expect("invoke_menu record is valid"),
            ),
            Ok(Err(error)) => menu_refusal(format!("invoke_menu: {error}")),
            Err(error) => menu_refusal(format!("invoke_menu: blocking task failed: {error}")),
        }
    }
}

// ── set_window_frame (Linux) ─────────────────────────────────────────────────

pub struct SetWindowFrameTool;

static SET_WINDOW_FRAME_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetWindowFrameTool {
    fn def(&self) -> &ToolDef {
        SET_WINDOW_FRAME_DEF.get_or_init(|| {
            let contract = pincer_contract::tool_contract("set_window_frame")
                .expect("set_window_frame contract");
            ToolDef {
                name: contract.name,
                // X11 geometry is the client window's (checked under KWin,
                // 2026-11-11: a frame set at y=100 left the 28 px title bar
                // at y=72..100, per _NET_FRAME_EXTENTS).
                description: format!(
                    "{} On Linux the frame is the client area, as in list_windows bounds: \
                     the window manager's title bar and borders lie outside it, so the title \
                     bar sits above y. On X11 a maximized or full-screen window is restored \
                     first. The result gives the frame read back as WxH+X+Y.",
                    contract.description
                ),
                input_schema: contract.input_schema,
                read_only: contract.annotations.read_only,
                destructive: contract.annotations.destructive,
                idempotent: contract.annotations.idempotent,
                open_world: contract.annotations.open_world,
            }
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_contract::SetWindowFrameInput;
        use pincer_core::action_record::{
            effect_from_value_readback, ActionEvidence, ActionExecutionRecord, ActionTransport,
            ActualDelivery, EvidenceKind, RequestedDelivery,
        };

        let input: SetWindowFrameInput =
            match pincer_core::tool_args::parse_typed_input("set_window_frame", args) {
                Ok(input) => input,
                Err(result) => return result,
            };
        if crate::wayland::is_wayland() {
            return ToolResult::error(
                "set_window_frame: this Wayland compositor exposes no protocol that permits a client to set another top-level window's geometry",
            );
        }
        let values = [input.x, input.y, input.width, input.height];
        if values.iter().any(|value| !value.is_finite())
            || input.width <= 0.0
            || input.height <= 0.0
        {
            return ToolResult::error(
                "set_window_frame: x/y must be finite and width/height must be finite positive numbers",
            );
        }
        let to_i32 = |name: &str, value: f64| -> Result<i32, String> {
            let rounded = value.round();
            if rounded < i32::MIN as f64 || rounded > i32::MAX as f64 {
                Err(format!("{name} is out of range for an X11 coordinate"))
            } else {
                Ok(rounded as i32)
            }
        };
        let to_u32 = |name: &str, value: f64| -> Result<u32, String> {
            let rounded = value.round();
            if rounded < 1.0 || rounded > u32::MAX as f64 {
                Err(format!("{name} is out of range for an X11 size"))
            } else {
                Ok(rounded as u32)
            }
        };
        let x = match to_i32("x", input.x) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let y = match to_i32("y", input.y) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let width = match to_u32("width", input.width) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let height = match to_u32("height", input.height) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let window_id = input.window_id;
        let pid = input.pid;
        let outcome = tokio::task::spawn_blocking(move || {
            let before = crate::x11::list_windows(Some(pid))
                .into_iter()
                .find(|window| window.xid == window_id)
                .map(|window| (window.x, window.y, window.width, window.height));
            let (observed, confirmed, mutation_error) =
                crate::x11::set_window_frame(window_id, pid, x, y, width, height)
                    .map_err(|error| error.to_string())?;
            let observed_frame =
                observed.map(|window| (window.x, window.y, window.width, window.height));
            let changed = observed_frame.is_some_and(|observed| before != Some(observed));
            Ok::<_, String>((observed_frame, confirmed, changed, mutation_error))
        })
        .await;
        let (observed, confirmed, changed, mutation_error) = match outcome {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return ToolResult::error(format!("set_window_frame: {error}")),
            Err(error) => {
                return ToolResult::error(format!(
                    "set_window_frame: blocking task failed: {error}"
                ));
            }
        };
        let effect = effect_from_value_readback(confirmed, changed, observed.is_some());
        let requested = (x, y, width, height);
        let mut record = ActionExecutionRecord::builder(
            effect,
            ActionTransport::LinuxX11ConfigureWindow,
            RequestedDelivery::NotApplicable,
        )
        .actual_delivery(ActualDelivery::NotApplicable)
        .detail(format!(
            "requested={requested:?} observed={observed:?} mutation_error={mutation_error:?}"
        ));
        if observed.is_some() {
            record = record.evidence(ActionEvidence {
                kind: EvidenceKind::ValueReadback,
                detail: if confirmed {
                    "X11 geometry readback matched the requested frame".into()
                } else {
                    "X11 geometry readback did not match the requested frame".into()
                },
            });
        }
        ToolResult::text(set_window_frame_text(requested, observed, confirmed))
            .with_action_record(record.build().expect("set_window_frame record is valid"))
    }
}

/// set_window_frame's text, with the frame read back in list_windows' own
/// `WxH+X+Y` form so the caller needs no list_windows call to see it (round
/// 2 of the first-call test).
pub(super) fn set_window_frame_text(
    requested: (i32, i32, u32, u32),
    observed: Option<(i32, i32, u32, u32)>,
    confirmed: bool,
) -> String {
    let frame = |(x, y, width, height): (i32, i32, u32, u32)| format!("{width}x{height}+{x}+{y}");
    match observed {
        Some(observed) if confirmed => format!(
            "Set and verified the window frame: it reads back as {}.",
            frame(observed)
        ),
        Some(observed) => format!(
            "The window frame did not settle at the requested geometry: asked for {}, it \
             reads back as {}.",
            frame(requested),
            frame(observed)
        ),
        None => "The native window mutation was attempted, but its resulting frame could not \
                 be read back."
            .to_owned(),
    }
}

// ── bring_to_front (Linux) ───────────────────────────────────────────────────

pub struct BringToFrontTool;

static BTF_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for BringToFrontTool {
    fn def(&self) -> &ToolDef {
        BTF_DEF.get_or_init(|| ToolDef {
            name: "bring_to_front".into(),
            description:
                // Screenshot advice checked on KWin X11, 2026-11-11: with
                // compositing on, a covered window's capture showed only its
                // own pixels; with compositing blocked
                // (_KDE_NET_WM_BLOCK_COMPOSITING) it showed the window over
                // it, and a minimized window's capture failed (BadMatch)
                // until bring_to_front restored it.
                "Activate a window and raise it above the others, and leave it that way, \
                 so later input lands on it. On X11 this sends _NET_ACTIVE_WINDOW, which \
                 window managers such as KWin answer by raising the window (and restoring \
                 a minimized one); the result reads the stacking order back: whether the \
                 window is now active and on top, and any overlapping window still above \
                 it (a keep-above window, for example). \
                 This deliberately breaks the no-foreground contract and is not part of \
                 the normal input ladder. For an ordinary `background_unavailable` \
                 response, retry only the refused action with \
                 `delivery_mode:\"foreground\"`; the input tool activates the window \
                 itself. Use `bring_to_front` for a surface that must stay in front \
                 across several calls, such as a remote desktop session, or before \
                 reading the screenshot of a minimized window (its capture fails) or, \
                 with no compositing window manager running, of a covered one (its \
                 capture shows the window over it). Wayland: activates through a \
                 target-addressable compositor adapter (wlroots foreign-toplevel or the \
                 GNOME Shell helper) and refuses when the compositor offers no safe \
                 adapter."
                .into(),
            input_schema: serde_json::json!({
                "type":"object","required":["pid"],"properties":{
                    "pid":{"type":"integer","description":"Process ID of the app to activate."},
                    "window_id":{"type":"integer","description":"X11 window id (xid) to activate. If omitted, the first window of `pid` is used."}
                },"additionalProperties":false
            }),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let pid = args.u64_or("pid", 0) as u32;
        if crate::wayland::is_wayland() {
            let window_id = match args.opt_u64("window_id") {
                Some(window_id) => window_id,
                None => match crate::wayland::list_windows_dispatch(Some(pid)).first() {
                    Some(window) => window.xid,
                    None => {
                        return ToolResult::error(format!(
                            "bring_to_front: no window_id given and no Wayland windows found for pid {pid}."
                        ))
                    }
                },
            };
            if crate::wayland::wayland_input_enabled() && crate::wayland::hyprland::is_session() {
                if args.opt_u64("window_id").is_none() {
                    return foreground_hyprland_refusal("an exact window_id is required");
                }
                return foreground_hyprland_action(
                    &args,
                    pid,
                    window_id,
                    crate::wayland::hyprland_input::Action::Activate,
                )
                .await;
            }
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::activate_window_for_input_target(window_id, Some(pid))
            })
            .await;
            return match result {
                Ok(Ok(())) => {
                    ToolResult::text(format!("Brought Wayland window {window_id} to front."))
                        .with_structured(serde_json::json!({
                            "window_id": window_id,
                            "platform": "linux",
                            "session": "wayland",
                        }))
                }
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        // X11: resolve the target xid (window_id, else first window for pid).
        let xid = match args.opt_u64("window_id") {
            Some(x) => x,
            None => {
                let windows =
                    tokio::task::spawn_blocking(move || crate::x11::list_windows(Some(pid)))
                        .await
                        .unwrap_or_default();
                match windows.first() {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                        "bring_to_front: no window_id given and no windows found for pid {pid}."
                    ))
                    }
                }
            }
        };
        let r = tokio::task::spawn_blocking(move || {
            let before = StackingOutcome::read(xid);
            let prior = crate::input::x11_activate_window_persistent(xid)?;
            Ok::<_, anyhow::Error>((prior, before, StackingOutcome::settle(xid)))
        })
        .await;
        match r {
            Ok(Ok((prior, before, after))) => bring_to_front_result(xid, prior, before, after),
            Ok(Err(e)) => ToolResult::error(format!("bring_to_front failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Where a window sits after bring_to_front: whether the window manager
/// reports it active, and the on-screen windows stacked above it
/// (`_NET_CLIENT_LIST_STACKING`, bottom to top) that overlap it.
#[derive(Debug, Clone, PartialEq)]
struct StackingOutcome {
    /// `_NET_ACTIVE_WINDOW`; `None` without an EWMH window manager.
    active: Option<u64>,
    /// No on-screen window is stacked above it at all.
    top_of_stack: bool,
    /// On-screen windows above it whose bounds intersect its own.
    covered_by: Vec<(u64, String)>,
}

impl StackingOutcome {
    /// Blocking. `None` when the window is not in the stacking list.
    fn read(xid: u64) -> Option<Self> {
        let windows = crate::x11::list_windows(None);
        Self::from_windows(xid, &windows, crate::x11::active_window())
    }

    fn from_windows(
        xid: u64,
        windows: &[crate::x11::WindowInfo],
        active: Option<u64>,
    ) -> Option<Self> {
        let target = windows.iter().find(|w| w.xid == xid)?;
        let z = target.z_index?;
        let overlaps = |w: &crate::x11::WindowInfo| {
            w.x < target.x + target.width as i32
                && target.x < w.x + w.width as i32
                && w.y < target.y + target.height as i32
                && target.y < w.y + w.height as i32
        };
        let above: Vec<&crate::x11::WindowInfo> = windows
            .iter()
            .filter(|w| w.is_on_screen && w.xid != xid && w.z_index.is_some_and(|wz| wz > z))
            .collect();
        Some(Self {
            active,
            top_of_stack: above.is_empty(),
            covered_by: above
                .into_iter()
                .filter(|w| overlaps(w))
                .map(|w| (w.xid, w.title.clone()))
                .collect(),
        })
    }

    fn settled(&self, xid: u64) -> bool {
        self.active.is_none_or(|active| active == xid) && self.covered_by.is_empty()
    }

    /// The outcome once the window manager has acted on the request: polled
    /// until the window is active and uncovered, for up to a second (KWin
    /// restacks within a few milliseconds of the client message).
    fn settle(xid: u64) -> Option<Self> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let outcome = Self::read(xid);
            if outcome.as_ref().is_none_or(|o| o.settled(xid))
                || std::time::Instant::now() >= deadline
            {
                return outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

/// `5 "Kate", 6`: window ids with their titles escaped and quoted
/// (`quoted_title`).
pub(super) fn describe_windows(windows: &[(u64, String)]) -> String {
    windows
        .iter()
        .map(|(id, title)| {
            if title.is_empty() {
                id.to_string()
            } else {
                format!("{id} {}", quoted_title(title))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// bring_to_front's X11 result: what the stacking order says now, and
/// whether anything changed, so a call on a window that was already in
/// front still has something to check.
fn bring_to_front_result(
    xid: u64,
    prior: Option<u64>,
    before: Option<StackingOutcome>,
    after: Option<StackingOutcome>,
) -> ToolResult {
    let Some(after) = after else {
        return ToolResult::text(format!(
            "Asked the window manager to activate window {xid} (X11 _NET_ACTIVE_WINDOW), but \
             it is not in the stacking list, so whether it is in front cannot be read."
        ))
        .with_structured(json!({
            "window_id": xid,
            "prior_active": prior,
            "platform": "linux",
        }));
    };
    let active = after.active.is_none_or(|active| active == xid);
    let on_top = after.covered_by.is_empty();
    let unchanged = before.as_ref() == Some(&after) && prior == Some(xid);
    let structured = json!({
        "window_id": xid,
        "prior_active": prior,
        "active": active,
        "top_of_stack": after.top_of_stack,
        "covered_by": after.covered_by.iter().map(|(id, title)| json!({"window_id": id, "title": title})).collect::<Vec<_>>(),
        "raised": before.as_ref().is_some_and(|b| !b.covered_by.is_empty()) && on_top,
        "platform": "linux",
    });
    if !active {
        return ToolResult::error(format!(
            "bring_to_front: asked the window manager to activate window {xid}, but after 1 s \
             the active window is still {}. The window may be minimized, on another desktop, \
             or blocked by a modal dialog (target that dialog instead), or the window \
             manager's focus-stealing prevention kept it back.",
            after
                .active
                .map(|a| a.to_string())
                .unwrap_or_else(|| "unset".to_owned())
        ))
        .with_structured(structured);
    }
    let position = if on_top {
        "the active window with no other window over it".to_owned()
    } else {
        format!(
            "the active window, but these windows are stacked above it and overlap it: {}",
            describe_windows(&after.covered_by)
        )
    };
    let change = if unchanged {
        " It already was before this call; nothing changed.".to_owned()
    } else {
        match before.as_ref().filter(|b| !b.covered_by.is_empty()) {
            Some(b) if on_top => {
                format!(" Before, it was under {}.", describe_windows(&b.covered_by))
            }
            _ => String::new(),
        }
    };
    // "is now" only when this call changed something (round 6 read "is now
    // ... It already was" as a contradiction).
    let now = if unchanged { "" } else { "now " };
    ToolResult::text(format!(
        "✅ Window {xid} is {now}{position} (X11 _NET_ACTIVE_WINDOW).{change}"
    ))
    .with_structured(structured)
}

#[cfg(test)]
#[test]
fn stacking_outcome_names_overlapping_windows_above_the_target() {
    let window = |xid, z, x, y, on_screen| crate::x11::WindowInfo {
        xid,
        pid: Some(1),
        app_name: String::new(),
        title: format!("w{xid}"),
        is_on_screen: on_screen,
        z_index: Some(z),
        x,
        y,
        width: 100,
        height: 100,
    };
    // Bottom to top: target, an overlapping window, a distant one, a
    // minimized overlapping one.
    let windows = [
        window(10, 0, 0, 0, true),
        window(11, 1, 50, 50, true),
        window(12, 2, 500, 500, true),
        window(13, 3, 10, 10, false),
    ];
    let outcome = StackingOutcome::from_windows(10, &windows, Some(11)).unwrap();
    assert!(!outcome.top_of_stack);
    assert_eq!(outcome.covered_by, vec![(11, "w11".to_owned())]);
    assert!(!outcome.settled(10));

    let top = StackingOutcome::from_windows(12, &windows, Some(12)).unwrap();
    assert!(top.top_of_stack && top.covered_by.is_empty() && top.settled(12));
    // Without an EWMH active window, an uncovered window counts as settled.
    let no_wm = StackingOutcome::from_windows(12, &windows, None).unwrap();
    assert!(no_wm.settled(12));
    assert!(StackingOutcome::from_windows(99, &windows, None).is_none());

    let result = bring_to_front_result(10, Some(11), Some(outcome.clone()), Some(outcome));
    assert_eq!(result.is_error, Some(true), "not active must be refused");
}
