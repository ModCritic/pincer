// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use crate::{
    GetAgentCursorStateInput, GetAgentCursorStateOutput, Platform, SchemaMode,
    SetAgentCursorEnabledInput, SetAgentCursorEnabledOutput, SetAgentCursorMotionInput,
    SetAgentCursorMotionOutput, SetAgentCursorThemeInput, SetAgentCursorThemeOutput,
    ToolAnnotations, ToolContract, ToolInput, ToolOutput,
};

const ALL_PLATFORMS: [Platform; 3] = [Platform::Macos, Platform::Windows, Platform::Linux];

pub fn contracts() -> Vec<ToolContract> {
    vec![
        // The Linux sentence describes a deliberate rule, not a bug: the
        // Linux pointer and keyboard paths re-enable the session cursor
        // (`overlay_glide_to_target_for` and
        // `reveal_pointer_action_target_for` in platform-linux), "a hidden
        // cursor must not make pointer or keyboard control invisible". The
        // macOS and Windows action paths send no SetEnabled(true), so there a
        // hide lasts until `enabled:true` (read in source, not run).
        contract::<SetAgentCursorEnabledInput, SetAgentCursorEnabledOutput>(
            "set_agent_cursor_enabled",
            "Show or hide the agent cursor owned by a session. On Linux, `false` lasts until the session's next desktop pointer or keyboard action, which shows the cursor again.",
            &["agent_cursor.set_enabled"],
            false,
        ),
        contract::<SetAgentCursorMotionInput, SetAgentCursorMotionOutput>(
            "set_agent_cursor_motion",
            "Configure the movement style, timing, effects and visibility timing for a session cursor. Fields left out keep their value. An effect at `default` (where every effect starts) follows the style, so changing `style` can turn it on or off; `on` or `off` pins it. The result says which effects are in use and which are pinned.",
            &["agent_cursor.set_motion"],
            false,
        ),
        contract::<SetAgentCursorThemeInput, SetAgentCursorThemeOutput>(
            "set_agent_cursor_theme",
            "Select an installed cursor theme for a session. get_agent_cursor_state's `installed_themes` lists the ids; an unknown id is refused with that list.",
            &["agent_cursor.set_theme"],
            false,
        ),
        contract::<GetAgentCursorStateInput, GetAgentCursorStateOutput>(
            "get_agent_cursor_state",
            "Return the session cursor's state. `enabled`: whether set_agent_cursor_enabled lets it be drawn; an enabled cursor still hides after `motion.idle_hide_ms` without activity (15000 ms by default), and get_session's `cursor_visible` says whether it is drawn right now. `position`: where it was last drawn, in screen coordinates (desktop pixels; points on macOS), null until it first moves. `installed_themes`: the ids set_agent_cursor_theme accepts. `visual_state`: the animation playing, for the kind of tool call in `requested_action` and `resolved_action` (`observe` for reads such as get_window_state; `click`, `drag`, `scroll`, `text`, `key`; `navigate` for move_cursor and browser_navigate; `app` for launch_app, list_windows, invoke_menu and the like; `record`; `system` for sessions, config and diagnostics; `idle` once it ends; the cursor tools themselves start none), with `modifiers` (empty while idle, so a text result leaves it out) naming its delivery (`background`, `foreground`) and how the call named its target (`ax` element token, `pixel` x/y, `browser`, else `desktop`), its `phase` and `frame`, and `preempted_count`, the animations a newer call cut short. `motion`: the motion settings and the effects in use; the result also says which effects are pinned (set on or off), while the others follow the style.",
            &["agent_cursor.state"],
            true,
        ),
    ]
}

fn contract<I: ToolInput, O: ToolOutput>(
    name: &str,
    description: &str,
    capabilities: &[&str],
    read_only: bool,
) -> ToolContract {
    assert_eq!(name, I::TOOL_NAME, "typed input is bound to the wrong tool");
    ToolContract {
        name: name.into(),
        description: description.into(),
        platforms: ALL_PLATFORMS.to_vec(),
        aliases: Vec::new(),
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        annotations: ToolAnnotations {
            read_only,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
        schema_mode: SchemaMode::CanonicalRuntime,
        cursor_semantics: None,
        input_schema: I::input_schema(),
        success_output_schema: Some(O::output_schema()),
        error_output_schema: None,
        output_validator: crate::validate_typed_output::<O>,
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        CursorEffectSetting, CursorMotionStyle, SetAgentCursorMotionInput, StartSessionInput,
        ToolInput,
    };
    use serde_json::json;

    #[test]
    fn motion_style_schema_lists_public_names_and_accepts_lab_ids() {
        let schema = SetAgentCursorMotionInput::input_schema();
        let names: Vec<&str> = schema["properties"]["style"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert_eq!(names, CursorMotionStyle::ALL.map(CursorMotionStyle::as_str));
        for (alias, style) in [
            ("dc-signature-arc", CursorMotionStyle::SignatureArc),
            ("dc-spring-settle", CursorMotionStyle::SpringSettle),
            ("dc-magnetic", CursorMotionStyle::Magnetic),
            ("dc-comet-swoop", CursorMotionStyle::CometSwoop),
            ("adaptive-auto", CursorMotionStyle::Adaptive),
            ("dubins-glide", CursorMotionStyle::Classic),
        ] {
            let input: SetAgentCursorMotionInput =
                serde_json::from_value(json!({"session": "s", "style": alias})).unwrap();
            assert_eq!(input.style, Some(style));
        }
        assert!(serde_json::from_value::<SetAgentCursorMotionInput>(
            json!({"session": "s", "effects": {"sparkle": true}})
        )
        .is_err());
    }

    #[test]
    fn explicit_null_effect_reset_survives_typed_tool_inputs() {
        let set: SetAgentCursorMotionInput = serde_json::from_value(json!({
            "session": "s",
            "effects": {"trail": null, "glow": true}
        }))
        .unwrap();
        let effects = set.effects.as_ref().expect("effects object");
        assert_eq!(effects.trail, Some(CursorEffectSetting::Default));
        assert_eq!(effects.glow, Some(CursorEffectSetting::On));
        assert_eq!(effects.magnet, None);
        assert_eq!(
            serde_json::to_value(&set).unwrap()["effects"],
            json!({"trail": "default", "glow": "on"})
        );

        let started: StartSessionInput = serde_json::from_value(json!({
            "session": "s",
            "cursor_motion": {"effects": {"trail": null}}
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&started).unwrap()["cursor_motion"]["effects"],
            json!({"trail": "default"})
        );
    }

    #[test]
    fn unset_effects_are_omitted_on_the_wire() {
        let input: SetAgentCursorMotionInput =
            serde_json::from_value(json!({"session": "s", "effects": {"trail": false}})).unwrap();
        assert_eq!(
            serde_json::to_value(&input).unwrap()["effects"],
            json!({"trail": "off"})
        );
    }
}
