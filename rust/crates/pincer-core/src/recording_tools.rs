//! Platform-independent recording / replay tools.
//!
//! Registered on all platforms via `ToolRegistry::register_recording_tools()`.
//!
//! - `start_recording`     — enable trajectory recording to disk (+ optional video)
//! - `stop_recording`      — disable trajectory recording, finalize video
//! - `get_recording_state` — query current recording state
//! - `replay_trajectory`   — replay a previously recorded trajectory
//!
//! Renamed from the older `set_recording(enabled: bool)` toggle in
//! `02f1f033..` — see `JOURNAL_VIDEO.md` for rationale. The split makes
//! the verbs match the CLI subcommand names (`pincer recording start|
//! stop|status`) and removes the "is this a setting write?" ambiguity of
//! the old `set_*` name.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    protocol::ToolResult,
    recording::{RecordingSession, RecordingState},
    tool::{Tool, ToolDef, ToolRegistry},
};

pub type ReplayRegistrySlot = Arc<Mutex<Weak<ToolRegistry>>>;

// ── start_recording ──────────────────────────────────────────────────────────

pub struct StartRecordingTool {
    session: Arc<RecordingSession>,
}

impl StartRecordingTool {
    pub fn new(session: Arc<RecordingSession>) -> Self {
        Self { session }
    }
}

static START_REC_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for StartRecordingTool {
    fn def(&self) -> &ToolDef {
        START_REC_DEF.get_or_init(|| ToolDef {
            name: "start_recording".into(),
            description: "Start trajectory recording: each later call to a tool that changes \
                something (click, type_text, press_key, scroll, set_value, launch_app, and so \
                on, including the steps inside run_steps and run_script) writes one turn folder \
                under `output_dir`, numbered `turn-00001/`, `turn-00002/`, ... from 1 on each \
                start. Read-only tools, the recording tools, start_session and end_session are \
                not recorded. Without `session`, every call on this connection is recorded, \
                named sessions included; other connections are not.\n\n\
                Each turn folder holds `action.json` (`tool`, `arguments` exactly as called, \
                pid and window_id included, `result_summary`, `result_error`, `timestamp`, and \
                `click_point` for clicks), `before.png`/`after.png` (target-window \
                screenshots), `before_state.json`/`after_state.json` (accessibility tree before \
                and after), `evidence.json` (what was captured and why anything is missing) \
                and, for dispatched clicks, `click.png` (before.png with a red marker at the \
                click point). `app_state.json` and `screenshot.png` are copies of \
                after_state.json and after.png. Beside the turn folders: `session.json` (a \
                monotonic start stamp, video and cursor-sample status), `cursor.jsonl` (pointer samples; always \
                empty on Linux) and, with `record_video`, `recording.mp4`.\n\n\
                Recording lasts until stop_recording, the end of the session that started it, \
                or a daemon restart. Replay it with replay_trajectory."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["output_dir"],
                "properties": {
                    "output_dir": {
                        "type": "string",
                        "description": "Absolute or ~-rooted directory where turn folders \
                            and (when enabled) the video file are written."
                    },
                    "record_video": {
                        "type": "boolean",
                        "description": "Also capture the main display to <output_dir>/recording.mp4 \
                            (H.264, 30 fps, no audio). Default false. macOS uses \
                            ScreenCaptureKit (macOS 15.0+); macOS 26 may show a one-time \
                            capture consent, which `pincer permissions grant` answers ahead. \
                            Windows and Linux need ffmpeg on PATH; if it is missing or fails, \
                            per-turn capture still runs and the session's `last_error` says \
                            why."
                    },
                    "state_timeout_ms": {
                        "type": "integer",
                        "minimum": crate::tool_schema::TIMEOUT_MS_MIN,
                        "maximum": crate::tool_schema::TIMEOUT_MS_MAX,
                        "default": crate::recording::TURN_STATE_TIMEOUT_MS_DEFAULT,
                        "description": "Time budget in ms for each before/after accessibility walk \
                            (same default and bounds as get_window_state's timeout_ms). A cut \
                            walk records the partial tree, marked truncated in evidence.json."
                    },
                    "include_accessibility_tree": {
                        "type": "boolean",
                        "default": true,
                        "description": "Default true. False skips the before/after accessibility \
                            walks; screenshots, click markers and action.json are still \
                            recorded (state classified `state_capture_disabled`)."
                    },
                    "session": crate::tool_schema::session_schema()
                },
                "additionalProperties": false
            }),
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use crate::tool_args::ArgsExt;
        let output_dir = args.opt_str("output_dir");
        if output_dir.as_deref().map(str::is_empty).unwrap_or(true) {
            return ToolResult::error("`output_dir` is required.");
        }
        let record_video = args.bool_or("record_video", false);
        // Daemon-injected ownership key (absent for one-shot CLI / anonymous
        // sessions). Stamps the recording so a session-scoped teardown
        // (session_end) only stops the recording its own session started.
        let owner = args.opt_str("_session_id");

        let state_budget = args.bool_or("include_accessibility_tree", true).then(|| {
            crate::recording::StateCaptureBudget {
                timeout_ms: crate::tool_schema::resolve_timeout_ms(args.get("state_timeout_ms")),
            }
        });

        match self.session.start_with_state_budget(
            output_dir.as_deref().unwrap(),
            record_video,
            owner.as_deref(),
            state_budget,
        ) {
            Ok(()) => {
                let state = self.session.current_state();
                // When the caller asked for video and it failed (e.g. macOS
                // ffmpeg TCC prompt deadlock), surface the actual error
                // prominently — the per-turn capture still runs, but the
                // caller deserves to know the mp4 won't materialize.
                let video_failed = record_video && !state.video_active;
                let video_note = if record_video && state.video_active {
                    " (video → recording.mp4)".to_string()
                } else if video_failed {
                    let err = state.last_error.clone().unwrap_or_else(|| "unknown".into());
                    let hint = if crate::video_ffmpeg::find_ffmpeg().is_none() {
                        "\n\nffmpeg was not found. Call install_ffmpeg (then again with \
                         confirm=true) to install it, then restart recording."
                    } else {
                        ""
                    };
                    format!("\n\n⚠️ Video capture failed (per-turn JSON+screenshot still running):\n{err}{hint}")
                } else {
                    String::new()
                };
                let msg = format!(
                    "✅ Recording started -> {}{}",
                    state.output_dir.as_deref().unwrap_or("?"),
                    video_note
                );
                ToolResult::text(msg).with_structured(recording_state_json(&state))
            }
            Err(e) => ToolResult::error(format!("Failed to start recording: {e}")),
        }
    }
}

// ── stop_recording ───────────────────────────────────────────────────────────

pub struct StopRecordingTool {
    session: Arc<RecordingSession>,
}

impl StopRecordingTool {
    pub fn new(session: Arc<RecordingSession>) -> Self {
        Self { session }
    }
}

static STOP_REC_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for StopRecordingTool {
    fn def(&self) -> &ToolDef {
        STOP_REC_DEF.get_or_init(|| ToolDef {
            name: "stop_recording".into(),
            description: "Stop trajectory recording, whichever session started it, and \
                finalize the mp4 when video was on. Returns the folder (`output_dir`), how many \
                turn folders were written (`turns_written`: `turn-00001` up to that number) \
                and, with video, `last_video_path`. With nothing recording it changes nothing \
                and says so (`stopped: false`)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        // Manual stop is unconditional — `None` requester tears down whatever
        // recording is active. Session-scoped teardown is driven by the
        // registry-owned session-end hook, which calls `stop_owner(sid)`.
        let stopped = self.session.stop_owner_reporting(None);
        let state = self.session.current_state();
        // The result describes the recording that just ended. The recorder's
        // `next_turn` has already reset to 1 for the next start, so it is left
        // out: shown beside a stop, it read like an empty recording.
        let mut structured = json!({
            "recording": false,
            "enabled": false,
            "stopped": stopped.is_some(),
            "video_active": state.video_active,
            "last_error": state.last_error,
        });
        let Some(stopped) = stopped else {
            return ToolResult::text("Recording was not running; nothing to stop.")
                .with_structured(structured);
        };
        structured["output_dir"] = json!(stopped.output_dir);
        structured["turns_written"] = json!(stopped.turns);
        structured["last_video_path"] = json!(stopped.video_path);
        let turns = match stopped.turns {
            0 => "no turn folders".to_owned(),
            1 => "1 turn folder (turn-00001)".to_owned(),
            n => format!("{n} turn folders (turn-00001 to turn-{n:05})"),
        };
        let summary = format!("{turns} in {}", stopped.output_dir);
        if let Some(error) = stopped.video_error {
            return ToolResult::error(format!(
                "Recording stopped with {summary}, but the video could not be finalized: {error}"
            ))
            .with_structured(structured);
        }
        let video_note = stopped
            .video_path
            .as_deref()
            .map(|path| format!(" Video: {path}."))
            .unwrap_or_default();
        ToolResult::text(format!("✅ Recording stopped: {summary}.{video_note}"))
            .with_structured(structured)
    }
}

// ── get_recording_state ───────────────────────────────────────────────────────

pub struct GetRecordingStateTool {
    session: Arc<RecordingSession>,
}

impl GetRecordingStateTool {
    pub fn new(session: Arc<RecordingSession>) -> Self {
        Self { session }
    }
}

static GET_REC_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for GetRecordingStateTool {
    fn def(&self) -> &ToolDef {
        GET_REC_DEF.get_or_init(|| ToolDef {
            name: "get_recording_state".into(),
            // Description ported from Swift `GetRecordingStateTool.swift`.
            description: "Report the current trajectory recorder state: whether recording \
                is enabled, the output directory (when enabled), and the 1-based counter \
                for the next turn folder that will be written. Counter increments on every \
                recorded action tool call and resets to 1 each time recording is \
                (re-)enabled.\n\n\
                Pure read-only.".into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        let state = self.session.current_state();
        // Match Swift text format 1:1:
        //   "✅ recording: enabled output_dir=<path> next_turn=<N>"
        //   "✅ recording: disabled"
        let summary = if state.enabled {
            format!(
                "recording: enabled output_dir={} next_turn={}",
                state.output_dir.as_deref().unwrap_or("?"),
                state.next_turn
            )
        } else {
            "recording: disabled".to_owned()
        };
        ToolResult::text(format!("✅ {summary}")).with_structured(recording_state_json(&state))
    }
}

// ── replay_trajectory ─────────────────────────────────────────────────────────

pub struct ReplayTrajectoryTool {
    registry: ReplayRegistrySlot,
}

impl ReplayTrajectoryTool {
    pub fn new(registry: ReplayRegistrySlot) -> Self {
        Self { registry }
    }
}

static REPLAY_DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for ReplayTrajectoryTool {
    fn def(&self) -> &ToolDef {
        REPLAY_DEF.get_or_init(|| ToolDef {
            name: "replay_trajectory".into(),
            // The caveats below were measured on Linux at 3eefd3e: a replay
            // in a new session refused every pixel click until
            // get_window_state had read that window, and a relaunched app
            // (new pid) failed every turn.
            // Turns recorded as failed are skipped by default: at bfc42c0 a
            // recording of a refused background click followed by its
            // foreground retry stopped the replay at the refusal, so the
            // retry that had worked never ran.
            description: "Replay a recording: call the tool in each `turn-NNNNN/action.json` \
                with its recorded `arguments`, in folder order. Turns recorded as failed \
                (`result_error: true`, such as a refusal before its retry) are skipped unless \
                `include_failed_turns` is true. Returns attempted, succeeded, failed and \
                skipped counts, each turn's result, and the first failure. Stops at the first \
                failure unless `stop_on_error` is false.\n\n\
                Turns replay against the recorded pid and window_id:\n\
                - If the app was relaunched (new pid), every turn fails. Edit `pid` and \
                `window_id` in each action.json's `arguments` to the new values, or reset the \
                app instead of relaunching it.\n\
                - A pixel click (`x`, `y`) is refused until this session has a screenshot of \
                its window: call get_window_state(pid, window_id) on each recorded window \
                first.\n\
                - An `element_token` names a row of one snapshot in one session, so a recorded \
                token click fails or hits whatever row has that token now. Record pixel clicks \
                or keys for replay.\n\n\
                With recording on, the replay is itself recorded into the current output \
                folder, so two runs can be diffed."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["dir"],
                "properties": {
                    "dir":           { "type": "string",  "description": "Trajectory directory previously written by `start_recording`. Absolute or ~-rooted." },
                    "delay_ms":      { "type": "integer", "minimum": 0, "maximum": 10000, "description": "Milliseconds to sleep between turns, for human-observable pacing. Default 500." },
                    "stop_on_error": { "type": "boolean", "description": "Default true: stop at the first turn that fails now. False replays every turn." },
                    "include_failed_turns": { "type": "boolean", "description": "Default false. True also replays turns recorded with `result_error: true`." }
                },
                "additionalProperties": false
            }),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // Swift error wording 1:1.
        let dir_str = match args.get("dir").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v.to_owned(),
            _ => return ToolResult::error("Missing required string field `dir`."),
        };
        use crate::tool_args::ArgsExt;
        let delay_ms = args.u64_or("delay_ms", 500).min(10_000);
        let stop_on_error = args.bool_or("stop_on_error", true);
        let include_failed_turns = args.bool_or("include_failed_turns", false);

        // Expand ~/
        let dir = {
            let p = std::path::PathBuf::from(&dir_str);
            if let Some(relative) = dir_str.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    std::path::PathBuf::from(home).join(relative)
                } else {
                    p
                }
            } else {
                p
            }
        };

        if !dir.exists() {
            return ToolResult::error(format!(
                "Trajectory directory does not exist: {}",
                dir.display()
            ));
        }

        // Collect and sort turn-NNNNN directories.
        let mut turn_dirs: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_dir()
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.starts_with("turn-"))
                                .unwrap_or(false)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        turn_dirs.sort();

        if turn_dirs.is_empty() {
            return ToolResult::error(format!(
                "No turn-NNNNN folders found under {}",
                dir.display()
            ));
        }

        let registry = match self.registry.lock().unwrap().upgrade() {
            Some(r) => r,
            None => {
                return ToolResult::error("Replay not available: registry not initialised yet.")
            }
        };

        let mut attempted = 0u32;
        let mut succeeded = 0u32;
        let mut failed = 0u32;
        let mut turns_json = Vec::new();
        let mut first_failure: Option<(String, String, String)> = None;
        let mut skipped_turns: Vec<String> = Vec::new();

        for turn_dir in &turn_dirs {
            let turn_name = turn_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_owned();

            let action_path = turn_dir.join("action.json");
            let (tool_name, tool_args, recorded_error) = match parse_action_json(&action_path) {
                Ok(v) => v,
                Err(e) => {
                    failed += 1;
                    if first_failure.is_none() {
                        first_failure =
                            Some((turn_name.clone(), "action.json".into(), e.to_string()));
                    }
                    turns_json.push(json!({
                        "turn": turn_name,
                        "ok": false,
                        "parse_error": e.to_string()
                    }));
                    if stop_on_error {
                        break;
                    }
                    continue;
                }
            };

            if recorded_error && !include_failed_turns {
                turns_json.push(json!({
                    "turn": turn_name,
                    "tool": &tool_name,
                    "skipped": "recorded as failed",
                }));
                skipped_turns.push(turn_name);
                continue;
            }

            attempted += 1;
            let result = registry.invoke(&tool_name, tool_args).await;
            let is_err = result.is_error.unwrap_or(false);
            let summary = result
                .content
                .iter()
                .find_map(|c| {
                    if let crate::protocol::Content::Text { text, .. } = c {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .unwrap_or("")
                .to_owned();

            turns_json.push(json!({
                "turn": turn_name,
                "tool": &tool_name,
                "ok": !is_err,
                "result_summary": &summary,
            }));

            if is_err {
                failed += 1;
                if first_failure.is_none() {
                    first_failure = Some((turn_name.clone(), tool_name.clone(), summary));
                }
                if stop_on_error {
                    break;
                }
            } else {
                succeeded += 1;
            }

            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let skipped = skipped_turns.len();
        let mut summary_text = format!(
            "replay {dir_name}: attempted={attempted} succeeded={succeeded} failed={failed} \
             skipped={skipped}"
        );
        if let Some((ref turn, ref tool, _)) = first_failure {
            summary_text.push_str(&format!(" first_failure={turn}:{tool}"));
        }
        if skipped > 0 {
            summary_text.push_str(&format!(
                "\nSkipped turns recorded as failed: {}. include_failed_turns:true replays them.",
                skipped_turns.join(", ")
            ));
        }

        let mut structured = json!({
            "directory": dir.to_string_lossy(),
            "attempted": attempted,
            "succeeded": succeeded,
            "failed": failed,
            "skipped": skipped,
            "stop_on_error": stop_on_error,
            "turns": turns_json,
        });
        if let Some((turn, tool, error)) = first_failure {
            structured["first_failure"] = json!({ "turn": turn, "tool": tool, "error": error });
        }

        ToolResult::text(summary_text).with_structured(structured)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn recording_state_json(state: &RecordingState) -> Value {
    json!({
        // "recording" mirrors the Swift field name for parity.
        "recording": state.enabled,
        "enabled": state.enabled,
        "output_dir": state.output_dir,
        "next_turn": state.next_turn,
        "last_error": state.last_error,
        "video_active": state.video_active,
        "last_video_path": state.last_video_path,
        // Session that owns the live recording (the daemon-injected
        // `_session_id`), or null when started anonymously. Informational; the
        // proxy-exit teardown drives ownership via the daemon `session_end`
        // signal rather than reading this back.
        "owner": state.owner,
    })
}

/// The tool, its recorded arguments, and whether the call failed when it was
/// recorded. A missing `result_error` (older or hand-written turns) counts as
/// not failed, so such turns still replay.
fn parse_action_json(path: &std::path::Path) -> anyhow::Result<(String, Value, bool)> {
    if !path.exists() {
        anyhow::bail!("Missing action.json");
    }
    let text = std::fs::read_to_string(path)?;
    let obj: Value = serde_json::from_str(&text)?;
    let tool = obj
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("action.json missing 'tool' string field"))?
        .to_owned();
    let tool_args = obj
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let recorded_error = obj
        .get("result_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((tool, tool_args, recorded_error))
}

// ── install_ffmpeg ────────────────────────────────────────────────────────────
//
// Confirmation-gated installer for the ffmpeg binary that the Linux/Windows
// video backend shells out to. Called without `confirm` it only REPORTS the
// command it would run (read-only preview); `confirm: true` runs it. Marked
// destructive + open_world so conforming MCP clients also gate it behind a
// human approval. ffmpeg is invoked as a separate process, never linked.

pub struct InstallFfmpegTool;
static INSTALL_FFMPEG_DEF: OnceLock<ToolDef> = OnceLock::new();

/// `found` as the file that runs: `find_ffmpeg` answers the bare name
/// `ffmpeg` when it starts from PATH, so look it up in PATH's directories
/// in order, as the process spawner's PATH search does (execvp on Unix; on
/// Windows with `.exe` added). A name no PATH entry holds stays as it is.
fn resolved_on_path(found: std::path::PathBuf) -> std::path::PathBuf {
    let dirs = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    resolved_in(found, dirs)
}

fn resolved_in(found: std::path::PathBuf, dirs: Vec<std::path::PathBuf>) -> std::path::PathBuf {
    if found.components().count() != 1 {
        return found;
    }
    let names = if cfg!(windows) {
        vec![found.with_extension("exe"), found.clone()]
    } else {
        vec![found.clone()]
    };
    dirs.iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| is_executable_file(candidate))
        .unwrap_or(found)
}

#[cfg(all(test, unix))]
mod ffmpeg_path_tests {
    use super::resolved_in;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// install_ffmpeg at 3eefd3e answered `path: ffmpeg`.
    #[test]
    fn a_bare_name_resolves_to_the_first_executable_on_path() {
        let root = tempfile::tempdir().unwrap();
        let [plain, first, second] = ["plain", "first", "second"].map(|dir| {
            let dir = root.path().join(dir);
            std::fs::create_dir(&dir).unwrap();
            dir
        });
        std::fs::write(plain.join("ffmpeg"), "").unwrap();
        for dir in [&first, &second] {
            let file = dir.join("ffmpeg");
            std::fs::write(&file, "").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let found = resolved_in(PathBuf::from("ffmpeg"), vec![plain, first.clone(), second]);
        assert_eq!(found, first.join("ffmpeg"));
        assert_eq!(
            resolved_in(PathBuf::from("ffmpeg"), vec![]),
            PathBuf::from("ffmpeg")
        );
        let absolute = PathBuf::from("/opt/ffmpeg/bin/ffmpeg");
        assert_eq!(resolved_in(absolute.clone(), vec![first]), absolute);
    }
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[async_trait]
impl Tool for InstallFfmpegTool {
    fn def(&self) -> &ToolDef {
        INSTALL_FFMPEG_DEF.get_or_init(|| ToolDef {
            name: "install_ffmpeg".into(),
            description: "Install the ffmpeg binary used by start_recording's video \
                capture (Linux/Windows; macOS records natively and needs no ffmpeg). \
                Two-step and confirmed: called without `confirm` it only reports the \
                exact install command for this platform's package manager; pass \
                `confirm: true` to actually run it. No-op if ffmpeg is already on PATH. \
                ffmpeg is run as a separate process, never linked into the driver."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "confirm":{"type":"boolean","description":"Run the install command. Without it, only the planned command is reported."}
            },"additionalProperties":false}),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use crate::tool_args::ArgsExt;

        if let Some(path) = crate::video_ffmpeg::find_ffmpeg().map(resolved_on_path) {
            return ToolResult::text(format!(
                "✅ ffmpeg already available ({}). Nothing to install.",
                path.display()
            ))
            .with_structured(json!({
                "installed": true, "ran": false, "path": path.display().to_string()
            }));
        }

        let Some(plan) = crate::ffmpeg_install::install_plan() else {
            return ToolResult::error(
                "ffmpeg is not installed and no supported package manager was found to \
                 install it automatically. Install ffmpeg manually and put it on PATH \
                 (Linux: apt/dnf/pacman/zypper/apk/snap; macOS: `brew install ffmpeg`; \
                 Windows: `winget install Gyan.FFmpeg`).",
            );
        };

        if !args.bool_or("confirm", false) {
            return ToolResult::text(format!(
                "ffmpeg is not installed. To install it via {}, re-call install_ffmpeg \
                 with confirm=true.\n\nCommand that will run:\n  {}",
                plan.manager,
                plan.display()
            ))
            .with_structured(json!({
                "installed": false, "ran": false,
                "manager": plan.manager, "command": plan.display()
            }));
        }

        let display = plan.display();
        let result =
            tokio::task::spawn_blocking(move || crate::ffmpeg_install::run_install(&plan)).await;
        match result {
            Ok(Ok((cmd_ok, output))) => match crate::video_ffmpeg::find_ffmpeg() {
                Some(path) => ToolResult::text(format!("✅ ffmpeg installed via `{display}`."))
                    .with_structured(json!({
                        "installed": true, "ran": true,
                        "command": display, "path": path.display().to_string()
                    })),
                None => ToolResult::error(format!(
                    "Ran the install command but ffmpeg is still not found.\n\
                     Command: {display}\ncommand_succeeded={cmd_ok}\nOutput tail:\n{output}"
                )),
            },
            Ok(Err(e)) => {
                ToolResult::error(format!("ffmpeg install failed: {e}\nCommand: {display}"))
            }
            Err(e) => ToolResult::error(format!("install task error: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_recording_resolves_the_per_turn_state_budget() {
        let session = Arc::new(RecordingSession::new());
        let tool = StartRecordingTool::new(session.clone());
        let properties = &tool.def().input_schema["properties"];
        assert_eq!(properties["state_timeout_ms"]["default"], 1000);
        assert_eq!(properties["include_accessibility_tree"]["default"], true);

        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        for (args, expected) in [
            (json!({"output_dir": dir}), Some(1000)),
            (
                json!({"output_dir": dir, "state_timeout_ms": 250}),
                Some(250),
            ),
            // Clamped to the shared timeout_ms bounds.
            (json!({"output_dir": dir, "state_timeout_ms": 1}), Some(100)),
            (
                json!({"output_dir": dir, "include_accessibility_tree": false}),
                None,
            ),
        ] {
            let result = tool.invoke(args.clone()).await;
            assert_ne!(result.is_error, Some(true), "{args}");
            assert_eq!(
                session.state_budget().map(|budget| budget.timeout_ms),
                expected,
                "{args}"
            );
            session.stop_owner(None).unwrap();
        }
    }

    #[tokio::test]
    async fn stop_recording_reports_the_turns_it_wrote_not_the_reset_counter() {
        // At 3eefd3e a stop right after three recorded turns answered
        // `enabled: false, next_turn: 1`, which read like an empty recording.
        let session = Arc::new(RecordingSession::new());
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().to_str().unwrap();
        session.start(dir, false, None).unwrap();
        // Off the (pid 1, window 2) target whose screenshot phase another test counts.
        let args = json!({"pid": 51, "window_id": 52, "x": 3, "y": 4});
        for _ in 0..3 {
            let pending = session
                .begin_turn(
                    "click",
                    &args,
                    crate::recording::now_ms(),
                    crate::recording::RecordingCaller::default(),
                )
                .expect("an anonymous recording keeps every call");
            session.finish_turn(pending, "clicked");
        }

        let stop = StopRecordingTool::new(session.clone());
        let stopped = stop.invoke(json!({})).await;
        assert_ne!(stopped.is_error, Some(true));
        let structured = stopped.structured_content.as_ref().unwrap();
        assert_eq!(structured["stopped"], true);
        assert_eq!(structured["turns_written"], 3);
        assert_eq!(structured["output_dir"], dir);
        assert!(structured.get("next_turn").is_none());
        let text = format!("{:?}", stopped.content);
        assert!(
            text.contains("3 turn folders (turn-00001 to turn-00003)"),
            "{text}"
        );
        assert!(directory.path().join("turn-00003/action.json").exists());

        let again = stop.invoke(json!({})).await;
        assert_ne!(again.is_error, Some(true));
        assert_eq!(again.structured_content.as_ref().unwrap()["stopped"], false);
        assert!(format!("{:?}", again.content).contains("nothing to stop"));
    }
}
