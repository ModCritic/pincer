//! `run_steps` v2 against a fake two-screen app: steps that name their
//! target, waits, checks and the end-of-batch summary.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef, ToolRegistry},
};

/// What the fake app shows.
#[derive(Default)]
struct Ui {
    screen: u8,
    email: String,
    dialog_open: bool,
    /// A transient popup (no useful controls) stacked on top.
    bubble_open: bool,
    /// Reads left before the "Finish" button appears on screen 2.
    finish_delay_reads: u32,
    snapshot: u32,
    /// Element labels of the latest read, by element index.
    last_labels: Vec<(String, String)>,
    /// The window of each current snapshot, as a platform snapshot store
    /// keeps it: a newer read of a window replaces its snapshot.
    snapshot_windows: Vec<(String, u64)>,
    calls: Vec<(String, Value)>,
}

impl Ui {
    fn elements(&mut self, window_id: u64) -> Vec<(&'static str, String, Option<String>)> {
        if window_id == 9 {
            return vec![("AXButton", "Done".into(), None)];
        }
        if window_id == 8 {
            return vec![
                ("AXButton", "Discard".into(), None),
                ("AXButton", "Keep".into(), None),
            ];
        }
        match self.screen {
            0 => vec![
                ("AXTextField", "Email".into(), Some(self.email.clone())),
                ("AXButton", "Next".into(), None),
                ("AXButton", "OK".into(), None),
                ("AXButton", "OK".into(), None),
            ],
            _ => {
                let mut rows = vec![("AXButton", "Back".into(), None)];
                if self.finish_delay_reads == 0 {
                    rows.push(("AXButton", "Finish".into(), None));
                } else {
                    self.finish_delay_reads -= 1;
                }
                rows
            }
        }
    }
}

struct Fake {
    def: ToolDef,
    ui: Arc<Mutex<Ui>>,
}

#[async_trait]
impl Tool for Fake {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let mut ui = self.ui.lock().unwrap();
        let name = self.def.name.clone();
        ui.calls.push((name.clone(), args.clone()));
        match name.as_str() {
            "list_windows" => {
                let mut windows = vec![json!({
                    "pid": 42, "window_id": 7, "app_name": "Demo", "title": "Main",
                    "z_index": 1, "is_on_screen": true,
                    "bounds": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 600.0}
                })];
                if ui.dialog_open {
                    windows.push(json!({
                        "pid": 42, "window_id": 8, "app_name": "Demo", "title": "Unsaved changes",
                        "z_index": 2, "is_on_screen": true,
                        "bounds": {"x": 100.0, "y": 100.0, "width": 300.0, "height": 200.0}
                    }));
                }
                if ui.bubble_open {
                    windows.push(json!({
                        "pid": 42, "window_id": 9, "app_name": "Demo", "title": "Bookmark added",
                        "z_index": 3, "is_on_screen": true,
                        "bounds": {"x": 500.0, "y": 0.0, "width": 200.0, "height": 100.0}
                    }));
                }
                ToolResult::text("windows").with_structured(json!({ "windows": windows }))
            }
            "list_apps" => ToolResult::text("apps").with_structured(json!({
                "apps": [{"pid": 42, "name": "Demo", "bundle_id": "com.example.demo", "running": true, "active": false}]
            })),
            "get_window_state" => {
                let window_id = args["window_id"].as_u64().unwrap_or(7);
                ui.snapshot += 1;
                let snapshot = ui.snapshot;
                ui.snapshot_windows.retain(|(_, window)| *window != window_id);
                ui.snapshot_windows
                    .push((format!("s{snapshot:08x}"), window_id));
                let rows = ui.elements(window_id);
                let mut markdown = String::from("- [0] AXWindow \"Demo\"\n");
                let mut elements = Vec::new();
                ui.last_labels.clear();
                for (offset, (role, label, value)) in rows.iter().enumerate() {
                    let index = offset + 1;
                    markdown.push_str(&format!("  - [{index}] {role} \"{label}\"\n"));
                    let mut element = json!({
                        "element_index": index,
                        "element_token": format!("s{snapshot:08x}:{index}"),
                        "role": role,
                        "depth": 1,
                        "label": label,
                        "frame": {"x": 0, "y": 0, "w": 10, "h": 10},
                    });
                    if let Some(value) = value.as_ref().filter(|value| !value.is_empty()) {
                        element["value"] = json!(value);
                    }
                    elements.push(element);
                    ui.last_labels
                        .push((format!("s{snapshot:08x}:{index}"), label.clone()));
                }
                if ui.screen == 1 && window_id == 7 {
                    markdown.push_str("  - AXStaticText \"Step 2 of 2\"\n");
                }
                ToolResult::text(markdown.clone()).with_structured(json!({
                    "pid": 42,
                    "window_id": window_id,
                    "snapshot_id": format!("s{snapshot:08x}"),
                    "elements": elements,
                    "tree_markdown": markdown,
                }))
            }
            "click" | "set_value" => {
                let token = args["element_token"].as_str().unwrap_or_default();
                let label = ui
                    .last_labels
                    .iter()
                    .find(|(candidate, _)| candidate == token)
                    .map(|(_, label)| label.clone());
                let Some(label) = label else {
                    return ToolResult::error(format!("{name}: stale or unknown element_token {token}"));
                };
                match (name.as_str(), label.as_str()) {
                    ("set_value", "Email") => {
                        ui.email = args["value"].as_str().unwrap_or_default().to_owned()
                    }
                    ("click", "Next") => ui.screen = 1,
                    ("click", "Back") => ui.dialog_open = true,
                    ("click", "Keep") | ("click", "Discard") => ui.dialog_open = false,
                    _ => {}
                }
                ToolResult::text(format!("{name} {label} done"))
            }
            other => ToolResult::text(format!("{other} done")),
        }
    }
}

struct Harness {
    registry: Arc<ToolRegistry>,
    ui: Arc<Mutex<Ui>>,
}

impl Harness {
    fn new() -> Self {
        Self::build(false)
    }

    /// A harness whose registry can say which window an element token
    /// names, as the Linux platform's snapshot store does.
    fn with_token_windows() -> Self {
        Self::build(true)
    }

    fn build(token_windows: bool) -> Self {
        let ui = Arc::new(Mutex::new(Ui::default()));
        let mut registry = ToolRegistry::new();
        if token_windows {
            let ui = ui.clone();
            registry.set_element_token_window_resolver(Arc::new(move |token| {
                let (snapshot, _) = token.split_once(':')?;
                let ui = ui.lock().unwrap();
                ui.snapshot_windows
                    .iter()
                    .find(|(id, _)| id == snapshot)
                    .map(|(_, window)| (42, *window))
            }));
        }
        for name in super::BATCHABLE_TOOLS.iter().copied().chain([
            "get_window_state",
            "list_windows",
            "list_apps",
        ]) {
            registry.register(Box::new(Fake {
                def: ToolDef {
                    name: name.into(),
                    description: "fake".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "pid": {"type": "integer"},
                            "window_id": {"type": "integer"},
                            "element_token": {"type": "string"},
                            "value": {"type": "string"},
                            "key": {"type": "string"},
                            "x": {"type": "number"},
                            "y": {"type": "number"}
                        }
                    }),
                    read_only: matches!(name, "get_window_state" | "list_windows" | "list_apps"),
                    destructive: false,
                    idempotent: false,
                    open_world: false,
                },
                ui: ui.clone(),
            }));
        }
        registry.register_session_tools();
        let registry = Arc::new(registry);
        registry.init_self_weak();
        Self { registry, ui }
    }

    async fn run(&self, args: Value) -> ToolResult {
        self.registry.invoke(super::RUN_STEPS_TOOL, args).await
    }

    fn calls(&self, tool: &str) -> Vec<Value> {
        self.ui
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, args)| args.clone())
            .collect()
    }
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn one_batch_crosses_screens_by_name() {
    let harness = Harness::new();
    let result = harness
        .run(json!({
            "steps": [
                {"set_value": {"app": "Demo", "role": "textfield", "name": "email", "value": "ada@example.com"},
                 "expect": {"role": "textfield", "name": "Email", "value": "ada@example.com"}},
                {"click": {"role": "button", "name": "Next"}, "expect": {"text": "Step 2"}},
                {"tool": "click", "args": {"role": "button", "name": "Back"},
                 "expect": {"app": "Demo", "window": "Unsaved", "role": "button", "name": "Keep"}},
                {"click": {"role": "button", "name": "Keep"},
                 "expect": {"name": "Keep", "gone": true}}
            ],
            "observe": true
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["executed"], 4);
    // The Keep button lives in the dialog window that opened on top: the
    // app-level target found it there without a window id.
    assert_eq!(structured["steps"][3]["target"]["window"]["window_id"], 8);
    assert!(
        text(&result).contains("on [2] AXButton \"Keep\""),
        "{}",
        text(&result)
    );

    let clicks = harness.calls("click");
    assert_eq!(clicks.len(), 3);
    assert_eq!(clicks[0]["pid"], 42);
    assert_eq!(clicks[0]["window_id"], 7);
    assert!(
        clicks[0].get("role").is_none(),
        "selector keys never reach the tool"
    );
    assert!(clicks[0]["element_token"].as_str().unwrap().ends_with(":2"));
    assert_eq!(harness.ui.lock().unwrap().email, "ada@example.com");

    // Lookup reads ask for full_output, which `since` never remembers; the
    // end observation is the one diff read, of the last window used.
    let reads = harness.calls("get_window_state");
    let observed = reads.last().unwrap();
    assert_eq!(observed["since"], "latest");
    // Lookup reads are full_output, which `since` never remembers; the only
    // other reads are one plain baseline per window the batch looked in.
    let baselines: Vec<u64> = reads[..reads.len() - 1]
        .iter()
        .filter(|read| read["full_output"] != true)
        .map(|read| read["window_id"].as_u64().unwrap())
        .collect();
    assert_eq!(baselines, [7, 8], "{reads:?}");
}

#[tokio::test]
async fn several_matches_fail_with_the_candidates_and_nth_picks_one() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "OK"}},
            {"click": {"role": "button", "name": "Next"}}
        ]}))
        .await;
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["failed_step"], 0);
    assert_eq!(structured["steps"][0]["phase"], "find");
    assert_eq!(structured["steps"][0]["code"], "ambiguous");
    let message = text(&result);
    assert!(message.contains("2 elements match"), "{message}");
    assert!(message.contains("nth 1: [4] AXButton \"OK\""), "{message}");
    assert!(harness.calls("click").is_empty(), "nothing clicked");

    let result = harness
        .run(json!({"steps": [{"click": {"app": "Demo", "role": "button", "name": "OK", "nth": 1}}]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(harness.calls("click")[0]["element_token"]
        .as_str()
        .unwrap()
        .ends_with(":4"));
}

#[tokio::test]
async fn a_missing_target_waits_then_names_the_nearest_elements() {
    let harness = Harness::new();
    let started = std::time::Instant::now();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "com.example.demo", "role": "button", "name": "Submit"}, "timeout_ms": 600}
        ]}))
        .await;
    let elapsed = started.elapsed();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("1. click ERROR (find): no button \"Submit\""),
        "{message}"
    );
    assert!(
        message.contains("nearest: [2] AXButton \"Next\""),
        "{message}"
    );
    assert!(message.contains("waited"), "{message}");
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(2_000), "{elapsed:?}");
}

#[tokio::test]
async fn a_named_target_that_appears_late_is_waited_for() {
    let harness = Harness::new();
    harness.ui.lock().unwrap().screen = 1;
    harness.ui.lock().unwrap().finish_delay_reads = 2;
    let result = harness
        .run(json!({"steps": [
            {"wait_for": {"app": "Demo", "text": "Step 2"}},
            {"click": {"role": "button", "name": "Finish"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.calls("click").len(), 1);
}

#[tokio::test]
async fn a_failed_expect_stops_the_batch_and_says_what_was_found() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"set_value": {"app": "Demo", "role": "textfield", "name": "Email", "value": "bob"},
             "expect": {"name": "Email", "value": "ada", "timeout_ms": 300}},
            {"click": {"role": "button", "name": "Next"}}
        ], "observe": true}))
        .await;
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["failed_step"], 0);
    assert_eq!(structured["steps"][0]["phase"], "expect");
    assert_eq!(structured["steps"][0]["code"], "unexpected_state");
    let message = text(&result);
    assert!(message.contains("value=\"bob\""), "{message}");
    assert!(harness.calls("click").is_empty(), "later steps never run");
    // The failed batch is still observed, in the window it was working in.
    assert_eq!(structured["observation"]["ok"], true);
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );
}

#[tokio::test]
async fn a_check_only_step_and_gone_on_a_closed_window() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"expect": {"app": "Demo", "window": "Unsaved", "name": "Keep", "gone": true, "timeout_ms": 0}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(harness.calls("click").is_empty());
}

#[tokio::test]
async fn bad_v2_steps_are_rejected_before_anything_runs() {
    let harness = Harness::new();
    for (step, needle) in [
        (json!({"click": {"pid": 1}, "args": {}}), "drop `args`"),
        (json!({"click": {}, "press_key": {}}), "one action per step"),
        (
            json!({"drag": {"app": "Demo", "name": "x"}}),
            "drag takes coordinates",
        ),
        (
            json!({"click": {"app": "Demo", "name": "x", "element_token": "s1:1"}}),
            "not both",
        ),
        (json!({"click": {"name": "Next"}}), "no window"),
        (
            json!({"wait_for": {"app": "Demo"}}),
            "needs `role`, `name` or `text`",
        ),
        (json!({"expect": []}), "1 to 4 checks"),
        (json!({}), "empty step"),
        (
            json!({"click": {"app": "Demo", "name": "x"}, "timeout_ms": 99999}),
            "timeout_ms",
        ),
        (json!({"wait": {"app": "Demo"}}), "cannot run in a batch"),
    ] {
        let result = harness
            .run(json!({"steps": [{"press_key": {"key": "a"}}, step.clone()]}))
            .await;
        assert_eq!(result.is_error, Some(true), "{step}");
        let message = text(&result);
        assert!(message.contains("step 2"), "{step}: {message}");
        assert!(message.contains(needle), "{step}: {message}");
    }
    assert!(harness.calls("press_key").is_empty());
    assert!(harness.calls("list_windows").is_empty());
}

#[tokio::test]
async fn later_steps_inherit_the_window() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "textfield", "name": "Email"}},
            {"press_key": {"key": "tab"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let pressed = &harness.calls("press_key")[0];
    assert_eq!(pressed["pid"], 42);
    assert_eq!(pressed["window_id"], 7);
}

#[tokio::test]
async fn a_popup_on_top_does_not_hide_the_main_window() {
    let harness = Harness::new();
    harness.ui.lock().unwrap().bubble_open = true;
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Next"}, "expect": {"text": "Step 2"}}
        ], "observe": true}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["steps"][0]["target"]["window"]["window_id"], 7);
    // The observation follows the window the batch acted in, not the popup.
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );

    // A miss names the window with the most elements, and says how many
    // windows were searched.
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Publish"}, "timeout_ms": 0}
        ]}))
        .await;
    let message = text(&result);
    assert!(message.contains("searched 2 windows"), "{message}");
    assert!(message.contains("window 7"), "{message}");
}

#[tokio::test]
async fn a_check_only_step_names_the_window_to_observe() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [{"expect": {"app": "Demo", "text": "Email", "timeout_ms": 0}}], "observe": true}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(
        result.structured_content.unwrap()["observation"]["ok"],
        true
    );
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );
}

#[tokio::test]
async fn a_window_the_caller_never_read_gets_a_baseline_read() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"set_value": {"app": "Demo", "role": "textfield", "name": "Email", "value": "a@b.c"}}
        ], "observe": true}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let reads = harness.calls("get_window_state");
    // No earlier read of window 7 is remembered: first a plain read with the
    // observation's budget (remembered by `since`), then the full lookup
    // read, then the end diff.
    assert_eq!(reads[0]["max_elements"], 250);
    assert!(reads[0].get("full_output").is_none());
    assert_eq!(reads[1]["full_output"], true);
    let last = reads.last().unwrap();
    assert_eq!(last["since"], "latest");
    assert_eq!(last["max_elements"], 250);
    assert_eq!(reads.len(), 3, "one baseline per window, not per poll");

    // Without observe there is no baseline read.
    let before = harness.calls("get_window_state").len();
    harness
        .run(json!({"steps": [{"click": {"app": "Demo", "role": "button", "name": "Next"}}]}))
        .await;
    assert_eq!(harness.calls("get_window_state").len(), before + 1);
}

#[tokio::test]
async fn the_end_observation_diffs_against_the_callers_last_read() {
    // The caller read window 9 with the observation's view: that read is the
    // baseline, so the batch takes none of its own and the end diff names a
    // snapshot the caller has seen (A/B test, 2026-11-03).
    crate::window_state_view::remember_for_test(
        "s0000f301",
        42,
        9,
        "- [0] AXWindow \"Bookmark added\"\n  - [1] AXButton \"Done\"\n",
    );
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"pid": 42, "window_id": 9, "role": "button", "name": "Done"}}
        ], "observe": true}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let reads = harness.calls("get_window_state");
    assert_eq!(reads.len(), 2, "lookup and end read only: {reads:?}");
    assert_eq!(reads[0]["full_output"], true);
    assert_eq!(reads[1]["since"], "latest");
}

#[tokio::test]
async fn a_trailing_get_window_state_in_shorthand_is_the_observation() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Next"}},
            {"get_window_state": {}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["total"], 1, "the read is not a step");
    assert_eq!(structured["observation"]["ok"], true);
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );
}

/// A token-only step names the window its token was read from: the step's
/// checks read that window, later steps inherit it, and `observe` reads it,
/// with no pid/window_id or app anywhere in the batch.
#[tokio::test]
async fn a_token_only_step_acts_checks_and_observes_in_its_tokens_window() {
    let harness = Harness::with_token_windows();
    let read = harness
        .registry
        .invoke("get_window_state", json!({"pid": 42, "window_id": 7}))
        .await;
    let snapshot = read.structured_content.unwrap()["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = harness
        .run(json!({
            "steps": [
                {"set_value": {"element_token": format!("{snapshot}:1"), "value": "ada@example.com"},
                 "expect": [{"role": "textfield", "name": "Email", "value": "ada@example.com"}]},
                {"click": {"role": "button", "name": "Next"}, "expect": {"text": "Step 2"}}
            ],
            "observe": {}
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let set = harness.calls("set_value");
    assert_eq!(
        (set[0]["pid"].clone(), set[0]["window_id"].clone()),
        (json!(42), json!(7))
    );
    assert_eq!(
        harness.ui.lock().unwrap().screen,
        1,
        "the inherited window held Next"
    );
    let observed = harness.calls("get_window_state").last().cloned().unwrap();
    assert_eq!(
        (observed["pid"].clone(), observed["window_id"].clone()),
        (json!(42), json!(7))
    );
    assert_eq!(observed["since"], "latest");
}

/// A token read from a dialog acts in that dialog even when the batch names
/// the main window: the token is the step's own target, so it wins over the
/// batch default like a step's own pid/window_id does.
#[tokio::test]
async fn a_tokens_own_window_beats_the_batch_window() {
    let harness = Harness::with_token_windows();
    harness.ui.lock().unwrap().dialog_open = true;
    let read = harness
        .registry
        .invoke("get_window_state", json!({"pid": 42, "window_id": 8}))
        .await;
    let snapshot = read.structured_content.unwrap()["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = harness
        .run(json!({
            "pid": 42, "window_id": 7,
            "steps": [
                {"click": {"element_token": format!("{snapshot}:2")},
                 "expect": [{"role": "button", "name": "Discard"}]}
            ]
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let click = harness.calls("click")[0].clone();
    assert_eq!(click["window_id"], 8, "{click}");
    assert!(
        !harness.ui.lock().unwrap().dialog_open,
        "Keep closed the dialog"
    );
}

/// `wait_for` runs before the step's action, and looks in the window the
/// step names, not only in one an earlier step named.
#[tokio::test]
async fn wait_for_looks_in_the_window_its_own_step_names() {
    let harness = Harness::new();
    harness.ui.lock().unwrap().dialog_open = true;
    let result = harness
        .run(json!({"steps": [
            {"click": {"pid": 42, "window_id": 8, "role": "button", "name": "Keep"},
             "wait_for": {"role": "button", "name": "Discard"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(!harness.ui.lock().unwrap().dialog_open);
}

/// The report's flow: step 1 opens a modal dialog, step 2 types into it
/// with desktop scope. The typing step takes no window from the batch, its
/// check finds the field in the dialog on top (the app's frontmost window),
/// and `observe` reads that dialog.
#[tokio::test]
async fn a_desktop_scope_step_checks_the_apps_windows_and_observes_where_its_check_held() {
    let harness = Harness::new();
    let result = harness
        .run(json!({
            "pid": 42, "window_id": 7, "delivery_mode": "foreground",
            "steps": [
                {"click": {"role": "button", "name": "Next"}},
                {"click": {"role": "button", "name": "Back"}},
                {"type_text": {"text": "x", "scope": "desktop"},
                 "expect": [{"role": "button", "name": "Keep"}]}
            ],
            "observe": {}
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let typed = harness.calls("type_text")[0].clone();
    for key in ["pid", "window_id", "delivery_mode"] {
        assert!(typed.get(key).is_none(), "desktop step got {key}: {typed}");
    }
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(
        structured["steps"][2]["expect"][0]["ok"], true,
        "{structured}"
    );
    let observed = harness.calls("get_window_state").last().cloned().unwrap();
    assert_eq!(observed["window_id"], 8, "{observed}");
}
