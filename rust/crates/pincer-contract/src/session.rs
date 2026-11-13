// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use crate::{
    CursorAction, CursorSemantics, EndSessionInput, EndSessionOutput, GetSessionInput,
    ListSessionsInput, ListSessionsOutput, Platform, SchemaMode, SessionOutput, StartSessionInput,
    StartSessionOutput, ToolAnnotations, ToolContract, ToolInput, ToolOutput,
};

const ALL_PLATFORMS: [Platform; 3] = [Platform::Macos, Platform::Windows, Platform::Linux];

pub fn contracts() -> Vec<ToolContract> {
    vec![start(), get(), list(), end()]
}

fn contract<I: ToolInput, O: ToolOutput>(
    name: &str,
    description: &str,
    capabilities: &[&str],
    annotations: ToolAnnotations,
) -> ToolContract {
    assert_eq!(name, I::TOOL_NAME, "typed input is bound to the wrong tool");
    ToolContract {
        name: name.into(),
        description: description.into(),
        platforms: ALL_PLATFORMS.to_vec(),
        aliases: Vec::new(),
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        annotations,
        schema_mode: SchemaMode::CanonicalRuntime,
        cursor_semantics: Some(CursorSemantics::new(CursorAction::System)),
        input_schema: I::input_schema(),
        success_output_schema: Some(O::output_schema()),
        error_output_schema: None,
        output_validator: crate::validate_typed_output::<O>,
    }
}

fn start() -> ToolContract {
    contract::<StartSessionInput, StartSessionOutput>(
        "start_session",
        "Optional: create or return a session before acting. For multi-call work, use a short `session` label and repeat it on every call; omitted, the connection's default session is used. Actions create or reuse a named session on their own, so call this only to set the initial cursor theme or motion, or to revive a name ended with end_session (ordinary actions never revive those). A session idle for 30 min ends, and its next call restarts it fresh. Idempotent.",
        &["session.lifecycle.start"],
        ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn get() -> ToolContract {
    contract::<GetSessionInput, SessionOutput>(
        "get_session",
        "Read one session's status, without window content: `state` (active or ending), `cursor_visible` (its agent cursor is drawn on screen right now: false before its first action and after its idle timeout, 15000 ms by default, even while get_agent_cursor_state reports it `enabled`), `recording_active`, `idle_seconds`, `expires_in_seconds` and `transport`. Omit `session` for this connection's default session. An ended session is refused with `session_ended` (start_session with the same label starts it again); a label never started on this connection is refused with `session_not_started`.",
        &["session.lifecycle.read"],
        ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn list() -> ToolContract {
    contract::<ListSessionsInput, ListSessionsOutput>(
        "list_sessions",
        "List this connection's active sessions, with the same fields as get_session. Ended sessions are left out, and other connections' sessions are never shown. Returns up to `limit` (default 50) per call; pass `next_cursor` back as `cursor` for the rest.",
        &["session.lifecycle.list"],
        ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn end() -> ToolContract {
    contract::<EndSessionInput, EndSessionOutput>(
        "end_session",
        "End a session and run its cleanup once: hide its agent cursor, stop a recording it started (finalizing the video), drop the window snapshots it took and its consent grants, and close isolated browsers it opened with browser_prepare. Apps it started with launch_app keep running; close them with kill_app before ending if they should not outlive it. Omit `session` to end this connection's default session. Ending an ended session succeeds again. Afterward, calls naming the session are refused with `session_ended` until start_session starts it again.",
        &["session.lifecycle.end"],
        ToolAnnotations {
            read_only: false,
            destructive: true,
            idempotent: true,
            open_world: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_description_explains_direct_naming_revival_and_idle_restart() {
        let description = start().description;
        assert!(description.contains("short `session` label"));
        assert!(description.contains("repeat it on every call"));
        assert!(description.contains("connection's default session"));
        assert!(description.contains("revive a name ended with end_session"));
        assert!(description.contains("ordinary actions never revive those"));
        assert!(description.contains("idle for 30 min ends, and its next call restarts it fresh"));
    }

    #[test]
    fn ended_sessions_are_described_as_refused_or_left_out() {
        // Measured on Linux at 3eefd3e: get_session on an ended label refused
        // with `session_ended`, list_sessions dropped it, and an app started
        // with launch_app in the session was still running after end_session.
        let get = get().description;
        assert!(get.contains("refused with `session_ended`"));
        assert!(get.contains("`session_not_started`"));
        assert!(list().description.contains("Ended sessions are left out"));
        let end = end().description;
        assert!(end.contains("Apps it started with launch_app keep running"));
        assert!(end.contains("close isolated browsers it opened with browser_prepare"));
        assert!(end.contains("refused with `session_ended`"));
    }
}
