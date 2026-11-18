//! Hyprland input: the isolated background worker, foreground delivery and
//! the results they report.

use super::*;

pub(super) fn isolated_hyprland_background(delivery: crate::input::delivery::DeliveryMode) -> bool {
    !delivery.is_foreground() && crate::wayland::hyprland_input::enabled()
}

pub(super) fn hyprland_foreground(delivery: crate::input::delivery::DeliveryMode) -> bool {
    delivery.is_foreground()
        && crate::wayland::wayland_input_enabled()
        && crate::wayland::hyprland::is_session()
}

pub(super) fn isolated_hyprland_refusal(detail: impl Into<String>) -> ToolResult {
    let detail = detail.into();
    ToolResult::error(format!("background_unavailable: {detail}")).with_structured(json!({
        "ok": false, "code": "background_unavailable", "reason": "unsupported_operation",
        "detail": detail, "route": "synthetic_events", "verified": false, "effect": "refused"
    }))
}

pub(super) fn isolated_hyprland_result(result: anyhow::Result<Value>) -> ToolResult {
    hyprland_input_result(result, false)
}

pub(super) fn hyprland_input_result(result: anyhow::Result<Value>, foreground: bool) -> ToolResult {
    use pincer_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let record = |effect| {
        ActionExecutionRecord::builder(
            effect,
            if foreground {
                ActionTransport::LinuxHyprlandForegroundInput
            } else {
                ActionTransport::LinuxHyprlandIsolatedInput
            },
            if foreground {
                RequestedDelivery::Foreground
            } else {
                RequestedDelivery::Background
            },
        )
    };
    let actual = if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    };
    let unavailable = if foreground {
        "foreground_unavailable"
    } else {
        "background_unavailable"
    };
    match result {
        Ok(value) if value["ok"] == true => ToolResult::text(
            "Dispatched exact-target Hyprland input; application effect is unverifiable.",
        )
        .with_action_record(
            record(ActionEffect::Unverifiable)
                .actual_delivery(actual)
                .build()
                .expect("isolated input record is valid"),
        ),
        Ok(mut value) => {
            if foreground && value["code"] == "foreground_partial_unknown" {
                return hyprland_input_result(
                    Err(crate::wayland::hyprland_input::unknown_dispatch(
                        anyhow::anyhow!(
                            "{}",
                            value["detail"]
                                .as_str()
                                .unwrap_or("foreground acquisition may have changed focus")
                        ),
                        value["delivery"]["delivered_count"]
                            .as_u64()
                            .and_then(|count| u32::try_from(count).ok())
                            .unwrap_or(0),
                    )),
                    true,
                );
            }
            let code = value["code"]
                .as_str()
                .unwrap_or("protocol_error")
                .to_owned();
            let detail = value["detail"]
                .as_str()
                .unwrap_or("input refused")
                .to_owned();
            value["reason"] = json!(code);
            value["code"] = json!(unavailable);
            value["route"] = json!(if foreground {
                "global_input"
            } else {
                "synthetic_events"
            });
            value["verified"] = json!(false);
            let delivered = value["delivery"]["delivered_count"]
                .as_u64()
                .and_then(|count| u32::try_from(count).ok())
                .filter(|count| *count > 0);
            let outcome = if value["effect"] == "partial"
                && delivered.is_some_and(|count| foreground || count == 1)
            {
                record(ActionEffect::Partial)
                    .actual_delivery(actual)
                    .delivered_count(delivered.unwrap())
            } else {
                value["effect"] = json!("refused");
                value
                    .as_object_mut()
                    .expect("protocol reply is an object")
                    .remove("delivery");
                record(ActionEffect::Refused)
            }
            .detail(&detail)
            .build()
            .expect("isolated input outcome is valid");
            ToolResult::error(format!("{unavailable} ({code}): {detail}"))
                .with_structured(value)
                .with_action_record(outcome)
        }
        Err(error) if error.is::<crate::wayland::hyprland_input::LaneBusy>() => {
            hyprland_input_result(
                Ok(json!({
                    "ok": false, "code": "lane_busy", "detail": error.to_string()
                })),
                foreground,
            )
        }
        Err(error) if error.is::<crate::wayland::hyprland_input::ActionCancelled>() => {
            hyprland_input_result(
                Ok(json!({
                    "ok": false, "code": "cancelled", "detail": error.to_string()
                })),
                foreground,
            )
        }
        Err(error) => {
            let count = error
                .downcast_ref::<crate::wayland::hyprland_input::DispatchUnknown>()
                .map(|error| error.acknowledged_phases)
                .filter(|count| *count > 0);
            let effect = if count.is_some() {
                ActionEffect::Partial
            } else {
                ActionEffect::Unverifiable
            };
            let mut outcome = record(effect)
                .actual_delivery(ActualDelivery::Unknown)
                .detail(error.to_string());
            if let Some(count) = count {
                outcome = outcome.delivered_count(count);
            }
            let outcome = outcome
                .build()
                .expect("unknown delivery preserves acknowledged progress");
            let mut value = serde_json::to_value(outcome.public_result().unwrap()).unwrap();
            value["ok"] = json!(false);
            value["code"] = json!(unavailable);
            value["reason"] = json!("transport_or_protocol_error");
            value["detail"] = json!(error.to_string());
            ToolResult::error(format!("{unavailable}: {error}"))
                .with_structured(value)
                .with_action_record(outcome)
        }
    }
}

pub(super) fn isolated_hyprland_task_error(error: tokio::task::JoinError, acknowledged: bool) -> ToolResult {
    isolated_hyprland_result(Err(crate::wayland::hyprland_input::unknown_dispatch(
        error.into(),
        u32::from(acknowledged),
    )))
}

pub(super) fn spawn_isolated_hyprland(
    args: &Value,
    work: impl FnOnce(crate::wayland::hyprland_input::ActionCancellation) -> anyhow::Result<Value>
        + Send
        + 'static,
) -> Result<
    (
        crate::wayland::hyprland_input::CancelOnDrop,
        tokio::task::JoinHandle<anyhow::Result<Value>>,
    ),
    ToolResult,
> {
    use pincer_core::session;
    let owner = named_session_cursor_key(args)
        .ok_or_else(|| isolated_hyprland_refusal("authenticated lifecycle required"))?;
    let transport_owner = args
        .get("_transport_session_id")
        .and_then(Value::as_str)
        .unwrap_or(&owner);
    let snapshot = session::session_snapshot(&owner, transport_owner, std::time::Duration::ZERO)
        .ok_or_else(|| isolated_hyprland_refusal("admitted lifecycle required"))?;
    // Retain admission before spawning: aborting the async caller must not
    // complete session teardown while its native worker still owns input.
    let lifecycle = session::begin_session_dispatch(
        &owner,
        snapshot.public_label.as_deref(),
        transport_owner,
        snapshot.implicit,
        snapshot.transport,
        snapshot.client_kind,
    )
    .map_err(isolated_hyprland_refusal)?;
    let (guard, cancellation) = crate::wayland::hyprland_input::ActionCancellation::invocation();
    let dispatch = tokio::task::spawn_blocking(move || {
        let _lifecycle = lifecycle;
        work(cancellation)
    });
    Ok((guard, dispatch))
}

pub(super) async fn isolated_hyprland_action(
    args: &Value,
    pid: u32,
    xid: u64,
    action: crate::wayland::hyprland_input::Action,
) -> ToolResult {
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute(owner, pid, xid, action, cancellation)
    }) {
        Ok(dispatch) => dispatch,
        Err(refusal) => return refusal,
    };
    match dispatch.await {
        Ok(result) => isolated_hyprland_result(result),
        Err(error) => isolated_hyprland_task_error(error, false),
    }
}

pub(super) fn foreground_hyprland_refusal(detail: impl Into<String>) -> ToolResult {
    hyprland_input_result(
        Ok(json!({
            "ok": false, "code": "unsupported_operation", "detail": detail.into()
        })),
        true,
    )
}

pub(super) async fn foreground_hyprland_action(
    args: &Value,
    pid: u32,
    xid: u64,
    action: crate::wayland::hyprland_input::Action,
) -> ToolResult {
    if !crate::wayland::hyprland_input::enabled() {
        return foreground_hyprland_refusal("production Hyprland input plugin is unavailable");
    }
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute_foreground(owner, pid, xid, action, cancellation)
    }) {
        Ok(dispatch) => dispatch,
        Err(_) => return foreground_hyprland_refusal("authenticated admitted lifecycle required"),
    };
    hyprland_input_result(
        match dispatch.await {
            Ok(result) => result,
            Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                error.into(),
                0,
            )),
        },
        true,
    )
}

pub(super) async fn foreground_hyprland_scroll(
    args: &Value,
    pid: u32,
    xid: u64,
    actions: Vec<crate::wayland::hyprland_input::Action>,
) -> ToolResult {
    if !crate::wayland::hyprland_input::enabled() {
        return foreground_hyprland_refusal("production Hyprland input plugin is unavailable");
    }
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute_foreground_scroll(
            owner,
            pid,
            xid,
            actions,
            cancellation,
        )
    }) {
        Ok(dispatch) => dispatch,
        Err(_) => return foreground_hyprland_refusal("authenticated admitted lifecycle required"),
    };
    hyprland_input_result(
        match dispatch.await {
            Ok(result) => result,
            Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                error.into(),
                0,
            )),
        },
        true,
    )
}

pub(super) fn foreground_hyprland_key(
    key: String,
    mut modifiers: Vec<String>,
) -> Result<crate::wayland::hyprland_input::Action, ToolResult> {
    let parts: Vec<&str> = key.split('+').collect();
    let key = if parts.len() > 1 && key != "+" {
        if parts[..parts.len() - 1]
            .iter()
            .any(|part| !is_modifier(part))
            || parts.last() == Some(&"")
        {
            return Err(foreground_hyprland_refusal("invalid modified key chord"));
        }
        modifiers.extend(
            parts[..parts.len() - 1]
                .iter()
                .map(|part| part.to_lowercase()),
        );
        parts.last().unwrap().to_string()
    } else {
        key
    };
    Ok(crate::wayland::hyprland_input::Action::Key { key, modifiers })
}

pub(super) async fn focus_hyprland_foreground(
    state: &Arc<ToolState>,
    args: &Value,
    pid: u32,
    xid: u64,
    element: Option<usize>,
    pixel: Option<(f64, f64)>,
) -> Result<(), ToolResult> {
    if !crate::wayland::hyprland_input::enabled() {
        return Err(foreground_hyprland_refusal(
            "production Hyprland input plugin is unavailable",
        ));
    }
    if let Some((x, y)) = pixel {
        return focus_by_pixel(
            state,
            pid,
            Some(xid),
            (x, y),
            true,
            args,
            args.bool_or("from_zoom", false),
        )
        .await;
    }
    if let Some(index) = element {
        let activation = foreground_hyprland_action(
            args,
            pid,
            xid,
            crate::wayland::hyprland_input::Action::Activate,
        )
        .await;
        if activation.is_error == Some(true) {
            return Err(activation);
        }
        match tokio::task::spawn_blocking(move || crate::atspi::focus_element(pid, xid, index))
            .await
        {
            Ok(Ok(true)) => {}
            _ => return Err(foreground_hyprland_refusal("AT-SPI child focus failed")),
        }
    }
    Ok(())
}

#[cfg(test)]
#[tokio::test]
async fn isolated_worker_retains_lifecycle_until_native_work_exits() {
    use pincer_core::session::{self, SessionClientKind, SessionTransport};
    let sid = "isolated-worker-lifecycle-test";
    let owner = "isolated-worker-transport-test";
    let caller = session::begin_session_dispatch(
        sid,
        None,
        owner,
        true,
        SessionTransport::McpStdio,
        SessionClientKind::Mcp,
    )
    .unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (cancellation, dispatch) = spawn_isolated_hyprland(
        &json!({"_session_id":sid,"_transport_session_id":owner}),
        move |_| {
            started.send(()).unwrap();
            released.recv().unwrap();
            Ok(json!({"ok":true}))
        },
    )
    .unwrap();
    ready.await.unwrap();
    drop(cancellation);
    drop(caller);
    assert!(session::end_session_for_owner(sid, owner));
    assert!(session::is_session_ending(sid));
    assert!(!session::is_session_ended(sid));
    release.send(()).unwrap();
    dispatch.await.unwrap().unwrap();
    assert!(session::is_session_ended(sid));
}

#[cfg(test)]
#[test]
fn isolated_background_routes_do_not_reprobe_availability_before_primary_fallback() {
    // Structural regression for the availability-loss race: each tool keeps
    // the decision that bypassed background refusal until its returning
    // native branch. A second probe here could select primary-seat input.
    let source = concat!(
        include_str!("click.rs"),
        include_str!("../impl_.rs"),
        include_str!("scroll.rs"),
        include_str!("click_gestures.rs"),
        include_str!("drag.rs"),
        include_str!("held_button.rs"),
    );
    for (start, end) in [
        ("impl Tool for ClickTool {", "impl Tool for TypeTextTool {"),
        (
            "impl Tool for ScrollTool {",
            "impl Tool for DoubleClickTool {",
        ),
        (
            "impl Tool for DragTool {",
            "impl Tool for MouseButtonDownTool {",
        ),
    ] {
        let body = source
            .rsplit_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        let invoke = body.split_once("async fn invoke").unwrap().1;
        assert_eq!(
            invoke
                .matches("isolated_hyprland_background(delivery)")
                .count(),
            1,
            "{start}"
        );
        let native = invoke.split_once("if isolated_background {").unwrap().1;
        assert!(native.contains("return match dispatch.await"), "{start}");
    }
}

#[cfg(test)]
#[test]
fn type_text_routes_isolated_background_before_generic_wayland_refusal() {
    let source = include_str!("../impl_.rs");
    let invoke = source
        .rsplit_once("impl Tool for TypeTextTool {")
        .unwrap()
        .1
        .split_once("impl Tool for PressKeyTool {")
        .unwrap()
        .0
        .split_once("async fn invoke")
        .unwrap()
        .1;
    let isolated = invoke
        .find("isolated_hyprland_background(delivery)")
        .expect("type_text must select isolated Hyprland background delivery");
    let generic = invoke
        .find("unavailable_wayland_focused_input_background")
        .expect("type_text must retain the generic Wayland refusal");
    assert!(
        isolated < generic,
        "the plugin route must run before the generic focused-input refusal"
    );
    assert!(invoke.contains("execute_background_text"));
}

#[cfg(test)]
#[test]
fn isolated_hyprland_refused_partial_and_unknown_outcomes_stay_distinct() {
    let busy = isolated_hyprland_result(Err(crate::wayland::hyprland_input::LaneBusy.into()));
    let content = busy.structured_content.as_ref().unwrap();
    assert_eq!(content["reason"], "lane_busy");
    assert_eq!(content["effect"], "refused");
    assert!(content.get("delivery").is_none());
    assert!(busy
        .action_record
        .unwrap()
        .public_result()
        .unwrap()
        .delivery
        .is_none());

    let refused = isolated_hyprland_result(Ok(
        json!({"ok":false,"code":"stale_target","detail":"stale_target"}),
    ));
    assert_eq!(
        refused.structured_content.as_ref().unwrap()["effect"],
        "refused"
    );
    let public = refused.action_record.unwrap().public_result().unwrap();
    assert!(public.delivery.is_none());
    public.validate_invariants().unwrap();

    let partial = isolated_hyprland_result(Ok(
        json!({"ok":false,"code":"cancelled","detail":"cancelled",
        "effect":"partial","delivery":{"mode":"background","delivered_count":1}}),
    ));
    let public = partial.action_record.unwrap().public_result().unwrap();
    assert_eq!(public.effect, pincer_contract::ActionEffect::Partial);
    assert_eq!(public.delivery.unwrap().delivered_count, Some(1));

    let unknown = isolated_hyprland_result(Err(anyhow::anyhow!("connection lost")));
    let public = unknown.action_record.unwrap().public_result().unwrap();
    assert_eq!(public.effect, pincer_contract::ActionEffect::Unverifiable);
    assert_eq!(
        public.delivery.as_ref().unwrap().mode,
        pincer_contract::ActionDeliveryMode::Unknown
    );
    assert_eq!(public.delivery.unwrap().delivered_count, None);

    let unknown_after_start = isolated_hyprland_result(Err(
        crate::wayland::hyprland_input::unknown_dispatch(anyhow::anyhow!("connection lost"), 1),
    ));
    let public = unknown_after_start
        .action_record
        .unwrap()
        .public_result()
        .unwrap();
    public.validate_invariants().unwrap();
    assert_eq!(public.effect, pincer_contract::ActionEffect::Partial);
    let delivery = public.delivery.unwrap();
    assert_eq!(delivery.mode, pincer_contract::ActionDeliveryMode::Unknown);
    assert_eq!(delivery.delivered_count, Some(1));
}

#[cfg(test)]
#[test]
fn experimental_pending_grant_is_an_error_with_operator_context() {
    let result = isolated_hyprland_result(Ok(json!({
        "ok": false, "code": "permission_required", "detail": "external approval pending",
        "epoch": "epoch", "challenge": "challenge", "target": "target", "revision": 3
    })));
    assert_eq!(result.is_error, Some(true));
    let value = result.structured_content.unwrap();
    assert_eq!(value["code"], "background_unavailable");
    assert_eq!(value["reason"], "permission_required");
    assert_eq!(value["challenge"], "challenge");
    assert_eq!(value["target"], "target");
    assert_eq!(value["revision"], 3);
    assert_eq!(value["verified"], false);
}

#[cfg(test)]
#[test]
fn experimental_dispatch_does_not_claim_application_success() {
    let result = isolated_hyprland_result(Ok(json!({
        "ok": true, "effect": "unverifiable", "route": "synthetic_events"
    })));
    assert_ne!(result.is_error, Some(true));
    let public = result.action_record.unwrap().public_result().unwrap();
    public.validate_invariants().unwrap();
    let value = serde_json::to_value(public).unwrap();
    assert_eq!(value["effect"], "unverifiable");
    assert_eq!(value["route"], "synthetic_events");
    assert!(value.get("verified").is_none());
}

#[cfg(test)]
#[test]
fn foreground_hyprland_reports_foreground_and_preserves_partial_progress() {
    use pincer_core::action_record::{ActionTransport, RequestedDelivery};
    for (reply, expected_effect, count) in [
        (json!({"ok":true}), "unverifiable", None),
        (
            json!({"ok":false,"code":"stale_target","detail":"stale"}),
            "refused",
            None,
        ),
        (
            json!({"ok":false,"code":"cancelled","detail":"cancelled","effect":"partial",
            "delivery":{"mode":"foreground","delivered_count":3}}),
            "partial",
            Some(3),
        ),
    ] {
        let result = hyprland_input_result(Ok(reply), true);
        let record = result.action_record.unwrap();
        assert_eq!(
            record.transport,
            ActionTransport::LinuxHyprlandForegroundInput
        );
        assert_eq!(record.requested_delivery, RequestedDelivery::Foreground);
        let public = record.public_result().unwrap();
        public.validate_invariants().unwrap();
        let value = serde_json::to_value(public).unwrap();
        assert_eq!(value["route"], "global_input");
        assert_eq!(value["effect"], expected_effect);
        if expected_effect == "refused" {
            assert!(value.get("delivery").is_none());
        } else {
            assert_eq!(value["delivery"]["mode"], "foreground");
            assert_eq!(value["delivery"]["delivered_count"].as_u64(), count);
        }
    }
    for reply in [
        Err(crate::wayland::hyprland_input::unknown_dispatch(
            anyhow::anyhow!("lost"),
            2,
        )),
        Ok(json!({"ok":false,"code":"foreground_partial_unknown","detail":"focus changed"})),
    ] {
        let unknown = hyprland_input_result(reply, true);
        let value =
            serde_json::to_value(unknown.action_record.unwrap().public_result().unwrap()).unwrap();
        assert_eq!(value["delivery"]["mode"], "unknown");
        assert_ne!(value["effect"], "refused");
    }
}

#[cfg(test)]
#[test]
fn foreground_hyprland_chords_preserve_all_modifiers() {
    let action = foreground_hyprland_key("Ctrl+Shift+H".into(), vec!["alt".into()]).unwrap();
    match action {
        crate::wayland::hyprland_input::Action::Key { key, modifiers } => {
            assert_eq!(key, "H");
            assert_eq!(modifiers, ["alt", "ctrl", "shift"]);
        }
        _ => panic!("expected a key chord"),
    }
    assert!(foreground_hyprland_key("bogus+H".into(), vec![]).is_err());
}
