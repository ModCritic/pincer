//! Recording captures (the before and after screenshots and accessibility
//! walks of a turn) run on the blocking pool, outside the process-wide input
//! lock.
//!
//! The capture hooks are process-global, so this file runs in its own test
//! binary with deliberately slow fake hooks.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pincer_core::protocol::ToolResult;
use pincer_core::recording::{set_budgeted_ax_snapshot_fn, set_screenshot_fn, StateCaptureBudget};
use pincer_core::tool::{Tool, ToolDef, ToolRegistry};
use serde_json::{json, Value};

/// How long each fake capture blocks its thread.
const CAPTURE_DELAY: Duration = Duration::from_millis(150);
/// The recorded app; captures of other pids return at once.
const SLOW_PID: i64 = 5101;
/// The app an unrecorded click acts on meanwhile.
const OTHER_PID: i64 = 5102;

/// The tests share the process-wide input lock; run them one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn install_hooks() {
    let png = pincer_core::image_utils::encode_rgba_to_png(&[255; 16], 2, 2).unwrap();
    set_screenshot_fn(move |_, pid| {
        if pid == Some(SLOW_PID) {
            std::thread::sleep(CAPTURE_DELAY);
        }
        Some(png.clone())
    });
    set_budgeted_ax_snapshot_fn(|_, pid, _budget: StateCaptureBudget| {
        if pid == Some(SLOW_PID) {
            std::thread::sleep(CAPTURE_DELAY);
        }
        Some(br#"{"tree_markdown":"- frame"}"#.to_vec())
    });
}

struct FakeClick {
    def: ToolDef,
}

#[async_trait]
impl Tool for FakeClick {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        ToolResult::text("Posted click (background; not driver-verified).")
    }
}

fn registry(recording_to: Option<&std::path::Path>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FakeClick {
        def: ToolDef {
            name: "click".into(),
            description: "fake click".into(),
            input_schema: json!({"type": "object"}),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        },
    }));
    if let Some(directory) = recording_to {
        registry
            .recording
            .start_with_state_budget(
                directory.to_str().unwrap(),
                false,
                None,
                Some(StateCaptureBudget { timeout_ms: 1_000 }),
            )
            .unwrap();
    }
    registry
}

fn click(pid: i64) -> Value {
    json!({"pid": pid, "window_id": 7, "x": 1, "y": 1})
}

/// On a single-threaded runtime, other tasks keep running while a recorded
/// call captures its before and after evidence.
#[tokio::test(flavor = "current_thread")]
async fn a_recorded_call_leaves_the_async_worker_free_while_it_captures() {
    let _serial = SERIAL.lock().await;
    install_hooks();
    let directory = tempfile::tempdir().unwrap();
    let registry = registry(Some(directory.path()));

    let ticks = Arc::new(AtomicUsize::new(0));
    let ticker = tokio::spawn({
        let ticks = ticks.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    // Let the ticker start.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let before = ticks.load(Ordering::SeqCst);
    let started = Instant::now();
    let result = registry.invoke("click", click(SLOW_PID)).await;
    let elapsed = started.elapsed();
    let during = ticks.load(Ordering::SeqCst) - before;
    ticker.abort();

    assert_ne!(result.is_error, Some(true), "{result:?}");
    // Four captures of 150 ms each: two screenshots, two walks.
    assert!(elapsed >= CAPTURE_DELAY * 4, "{elapsed:?}");
    // A blocked worker ticks at most once or twice, between captures.
    assert!(
        during >= 30,
        "the ticker ran {during} times in {elapsed:?} while the call captured"
    );
    let turn = directory.path().join("turn-00001");
    for name in [
        "before.png",
        "before_state.json",
        "screenshot.png",
        "app_state.json",
        "action.json",
    ] {
        assert!(turn.join(name).is_file(), "{name}");
    }
    registry.recording.stop_owner(None).unwrap();
}

/// A recorded call takes its evidence outside the process-wide input lock,
/// so another runtime's click does not wait for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_call_captures_outside_the_input_lock() {
    let _serial = SERIAL.lock().await;
    install_hooks();
    let directory = tempfile::tempdir().unwrap();
    let recorded = Arc::new(registry(Some(directory.path())));
    let other = registry(None);

    let first = tokio::spawn({
        let recorded = recorded.clone();
        async move { recorded.invoke("click", click(SLOW_PID)).await }
    });
    // The recorded click is now taking its before screenshot.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    let result = other.invoke("click", click(OTHER_PID)).await;
    let waited = started.elapsed();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(
        waited < Duration::from_millis(100),
        "an unrecorded click waited {waited:?} for another call's capture"
    );

    let recorded_result = first.await.unwrap();
    assert_ne!(recorded_result.is_error, Some(true), "{recorded_result:?}");
    assert!(directory.path().join("turn-00001/action.json").is_file());
    recorded.recording.stop_owner(None).unwrap();
}
