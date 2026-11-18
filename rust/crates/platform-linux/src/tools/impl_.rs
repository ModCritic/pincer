//! Real Linux tool implementations (compiled only on Linux).

use async_trait::async_trait;
use pincer_contract::{
    ClickButton, DragInput, GetCursorPositionInput, GetDesktopStateInput, GetScreenSizeInput,
    HotkeyInput, InvokeMenuInput, MoveCursorInput, PressKeyInput, ScrollInput, TypeTextInput,
};
use pincer_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
    tool_args::{parse_legacy_click_input, parse_typed_input, parse_typed_projection, ArgsExt},
    window_target::{PidOnlyWindowTargetGuard, WindowTargetCandidate, WindowTargetCandidates},
};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::atspi::Snapshots;
use crate::input::{
    quote_untrusted, quoted_title, untrusted_token, SHOWN_COMMAND_CHARS, SHOWN_NAME_CHARS,
};
use crate::input::modifier::is_modifier;
use cursor_overlay::CursorRegistry;

mod capture_series;
mod click;
mod click_gestures;
mod config;
mod coords;
mod cursor_reveal;
mod cursor_tools;
mod desktop;
mod discovery;
mod drag;
mod held_button;
mod hyprland_route;
mod input_common;
mod keyboard_route;
mod keys;
mod launch;
mod pointer_route;
mod scroll;
mod set_value;
mod state;
mod text;
mod toolkit;
mod window_ops;
mod window_state;
mod window_target;
mod zoom;

use self::capture_series::*;
use self::click::*;
use self::click_gestures::*;
use self::config::*;
use self::coords::*;
use self::cursor_reveal::*;
use self::cursor_tools::*;
use self::desktop::*;
use self::discovery::*;
use self::drag::*;
use self::held_button::*;
use self::hyprland_route::*;
use self::input_common::*;
use self::keyboard_route::*;
use self::keys::*;
use self::launch::*;
use self::pointer_route::*;
use self::scroll::*;
use self::set_value::*;
use self::state::*;
use self::text::*;
use self::toolkit::*;
use self::window_ops::*;
use self::window_state::*;
use self::window_target::*;
use self::zoom::*;

#[cfg(test)]
mod pid_window_resolver_tests;

use pincer_core::snapshot_store::ZoomContext;

#[cfg(test)]
mod region_frame_tests;

#[cfg(test)]
mod list_windows_tests;

#[cfg(test)]
mod untrusted_text_tests;

#[cfg(test)]
mod megapixel_cap_tests;

#[cfg(test)]
mod get_window_state_actions_tests;

mod appimage;
mod launch_child;
use launch_child::{LaunchExit, LaunchedChild};

#[cfg(test)]
mod launch_app_tests;
#[cfg(test)]
mod launch_scope_tests;

#[cfg(test)]
mod launch_resolution_tests;

#[cfg(test)]
mod pointer_argument_tests;

#[cfg(test)]
mod held_button_live_tests;

#[cfg(test)]
mod type_text_readback_tests;

#[cfg(test)]
mod click_delegation_tests;

#[cfg(test)]
mod press_key_tests;

#[cfg(test)]
mod set_value_tests;

// ── registry ─────────────────────────────────────────────────────────────────

pub fn build_registry_with_provider(
    compat: bool,
    provider: Option<std::sync::Arc<dyn pincer_core::consent::ProtectedConsentProvider>>,
) -> ToolRegistry {
    let mut r = ToolRegistry::new_with_protected_consent_provider(provider);
    let state = ToolState::new_with_capture_service(r.capture_service());
    if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let recording_state = Arc::downgrade(&state);
        r.recording
            .set_pixel_point_fn(move |args, window_id, pid, mut x, mut y| {
                let state = recording_state.upgrade()?;
                if let Some(pid) = pid {
                    let pid = u32::try_from(pid).ok()?;
                    // Match ClickTool's screenshot and zoom conversion before
                    // translating to the recording's full-output image.
                    if args.bool_or("from_zoom", false) {
                        (x, y) = state
                            .zoom_context(args, pid, args.opt_u64("window_id"))
                            .ok()?
                            .zoom_to_window(x, y);
                    } else if let Ok(ratio) =
                        screenshot_scale(&state, args, pid, args.opt_u64("window_id"))
                    {
                        x *= ratio;
                        y *= ratio;
                    }
                }
                crate::recording_hooks::hyprland_pixel_recording_point(window_id, pid, x, y)
            });
    }
    let cursor_outcome_reader = {
        let cursor_registry = state.cursor_registry.clone();
        pincer_core::session::register_scoped_cursor_outcome_reader(std::sync::Arc::new(
            move |session_id| {
                let state = cursor_registry.get(session_id);
                let motion_customized = state.is_some()
                    && crate::overlay::current_motion_for(session_id)
                        != cursor_overlay::MotionConfig::default();
                let active_cursor_count = cursor_registry
                    .all_states()
                    .iter()
                    .filter(|state| state.config.cursor_id != "default")
                    .count()
                    .max(1);
                match state {
                    Some(state) => pincer_core::session::bounded_cursor_outcome(
                        true,
                        state.config.enabled,
                        crate::overlay::is_visible_for_session(session_id),
                        Some(state.config.theme_id.as_str()),
                        motion_customized,
                        active_cursor_count,
                    ),
                    None => pincer_core::session::bounded_cursor_outcome(
                        false,
                        false,
                        false,
                        None,
                        false,
                        active_cursor_count,
                    ),
                }
            },
        ))
    };
    let session_end_hook = {
        let cursor_registry = state.cursor_registry.clone();
        let state_for_session_end = state.clone();
        pincer_core::session::register_scoped_fallible_session_end_hook(
            "linux_input",
            move |session_id| {
                if crate::wayland::is_wayland() {
                    release_mouse_hold_for_session(
                        &state_for_session_end,
                        session_id,
                        |cursor_id, held| {
                            crate::wayland::persistent_vptr::release(cursor_id, held.button)
                        },
                    )?;
                } else {
                    // A foreground hold is a real XTest button on the core
                    // pointer: release it so the user's pointer is not left
                    // pressed. Best effort; the hold is dropped either way.
                    let _ = release_mouse_hold_for_session(
                        &state_for_session_end,
                        session_id,
                        |_, held| {
                            if held.foreground {
                                crate::input::send_button_xtest_desktop(None, held.button, false)
                            } else {
                                Ok(())
                            }
                        },
                    );
                    clear_mouse_hold_after_release(&state_for_session_end, session_id);
                }

                state_for_session_end
                    .snapshots
                    .retire_session_screenshots(session_id);
                cursor_registry.remove(session_id);
                crate::overlay::remove_cursor(session_id.to_owned());
                crate::input::forget_master_pointer(session_id);
                crate::wayland::hyprland_input::cleanup_session(session_id);
                Ok(())
            },
        )
    };
    let session_revive_hook =
        pincer_core::session::register_scoped_session_revive_hook(move |session_id| {
            crate::overlay::revive_cursor(session_id.to_owned());
        });
    r.retain_cursor_outcome_reader(cursor_outcome_reader);
    r.retain_session_end_hook(session_end_hook);
    r.retain_session_revive_hook(session_revive_hook);
    if let Some(runtime_scope) = pincer_core::tool::current_dispatch_runtime_scope() {
        let prefix = format!("__pincer_runtime_{runtime_scope}:");
        let cursor_registry = state.cursor_registry.clone();
        let capture_service = state.capture_service.clone();
        r.retain_runtime_cleanup(move || {
            crate::capture_action_frame::retire_runtime(&capture_service);
            crate::wayland::hyprland_input::cleanup_runtime(&prefix);
            for cursor in cursor_registry
                .all_states()
                .into_iter()
                .filter(|cursor| cursor.config.cursor_id.starts_with(&prefix))
            {
                cursor_registry.remove(&cursor.config.cursor_id);
                crate::overlay::remove_cursor(cursor.config.cursor_id);
            }
        });
    }
    r.register(Box::new(ListAppsTool));
    r.register(Box::new(ListWindowsTool));
    r.register(Box::new(GetWindowStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(pincer_core::expectation::VerifyStateTool::new(
        std::sync::Arc::new(pincer_core::expectation::ToolObservationProvider::new(
            std::sync::Arc::new(ListWindowsTool),
            std::sync::Arc::new(GetWindowStateTool {
                state: state.clone(),
            }),
        )),
    )));
    r.register(Box::new(LaunchAppTool));
    // AppImage run folders a crash or an earlier driver left behind.
    #[cfg(not(test))]
    if let Some(root) = appimage::run_root() {
        let _ = std::thread::Builder::new()
            .name("pincer-appimage-sweep".into())
            .spawn(move || {
                appimage::sweep_run_dirs(&root);
            });
    }
    let browser_engine = pincer_core::browser::BrowserEngine::new_with_runtime_services(
        Arc::new(crate::browser_platform::LinuxBrowserPlatform::new(
            state.cursor_registry.clone(),
        )),
        r.approval_broker(),
        r.protected_resource_ownership(),
    );
    r.register(Box::new(KillAppTool {
        browsers: browser_engine.clone(),
    }));
    let pid_window_candidates: PidWindowGuardParts = (
        Arc::new(pid_window_target_candidates),
        desktop_point_window_resolver(),
        pid_fallback_window_resolver(),
        snapshot_window_resolver(state.clone()),
        token_pid_resolver(state.clone()),
    );
    // run_steps gives a step that carries only an element_token the window
    // the token was read from.
    let token_state = state.clone();
    r.set_element_token_window_resolver(Arc::new(move |token| {
        let (pid, window_id) = token_state.snapshots.window_for_token(token)?;
        Some((i64::from(pid), window_id))
    }));
    r.register(pid_window_guarded(BringToFrontTool, &pid_window_candidates));
    r.register(Box::new(SetWindowFrameTool));
    r.register(Box::new(InvokeMenuTool));
    r.register(pid_window_guarded(
        ClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DoubleClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        RightClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonDownTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseDragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonUpTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    // Offered only where it can run: every call is refused elsewhere (KDE
    // Plasma X11, XWayland, Xvfb, no /dev/uinput, native Wayland without
    // the cua-compositor socket), and it has no foreground fallback. The
    // probe opens no device.
    match parallel_drag_unavailable_reason(
        crate::wayland::is_inject_mode(),
        crate::wayland::is_wayland(),
        crate::input::real_pointer_unavailable_reason,
    ) {
        None => r.register(Box::new(ParallelMouseDragTool {
            state: state.clone(),
        })),
        Some(reason) => tracing::debug!("parallel_mouse_drag not offered: {reason}"),
    }
    r.register(pid_window_guarded(
        TypeTextTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        PressKeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        HotkeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        SetValueTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        ScrollTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    pincer_core::clipboard::register_clipboard_tools(
        &mut r,
        Arc::new(crate::clipboard::LinuxClipboard::new()),
    );
    // `screenshot` removed - see the matching comment in
    // platform-windows/src/tools/impl_.rs::build_registry. Canonical
    // screenshot path is `get_window_state` (it returns one by default).
    let _ = compat;
    r.register(Box::new(GetScreenSizeTool));
    r.register(Box::new(GetDesktopStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetCursorPositionTool));
    r.register(Box::new(MoveCursorTool {
        state: state.clone(),
    }));
    r.register(Box::new(MoveAlongPathTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorEnabledV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorMotionV2Tool));
    r.register(Box::new(GetAgentCursorStateV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorThemeTool {
        state: state.clone(),
    }));
    r.register(Box::new(CheckPermissionsTool));
    // `health_report` — single-call cross-platform driver diagnostics.
    // Stable schema_version="1" contract for downstream consumers. Linux skips
    // tcc_* and bundle_identity with "not applicable on Linux".
    r.register(Box::new(pincer_core::health_report::HealthReportTool::new(
        std::sync::Arc::new(crate::health_report::LinuxHealthProvider),
    )));
    r.register(Box::new(GetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetAccessibilityTreeTool));
    r.register(Box::new(ZoomTool {
        state: state.clone(),
    }));
    r.register(Box::new(CaptureSeriesTool {
        state: state.clone(),
    }));
    // `type_text_chars` needs no tool here: pincer-core's registry resolves
    // it to `type_text` at invoke time and keeps it out of tools/list.
    pincer_core::browser::register_browser_tools(&browser_engine, &mut r);
    r.register_recording_tools();
    r.register_session_tools();
    r
}

#[cfg(test)]
mod click_button_schema_tests;

#[cfg(test)]
mod pid_window_target_tests;

#[cfg(test)]
mod session_cursor_target_tests;

#[cfg(test)]
mod display_only_tests;
#[cfg(test)]
mod since_screenshot_default_tests;
#[cfg(test)]
mod window_capture_dimension_tests;

#[cfg(test)]
mod desktop_capture_frame_tests;

#[cfg(test)]
mod background_budget_tests;

#[cfg(test)]
mod background_keyboard_route_tests;

#[cfg(test)]
mod visibility_tests;

#[cfg(test)]
mod result_wording_tests;
