# Security audit, 2026-11-13

Scope: the Rust workspace under `rust/` at `643be26` (11/13/26), with the skill pack
(`rust/Skills/pincer`), install scripts and the Linux helper peers. The top priority was prompt
injection: Pincer's results reach a model, and an attacker controls much of what Pincer reads (a
window title, a file name, an accessible name, page text, the clipboard, a dialog). The rest of the
attack surface came second: launch_app and its arguments, file paths, the daemon socket and HTTP
transports, the run_script sandbox, authorization and consent, browser profiles and CDP, unsafe FFI,
resource exhaustion and secrets in output.

Not covered line by line: the macOS and Windows platform crates, pincer-sdk, pincer-testkit,
pincer-e2e, cursor-overlay and the hyprland plugin (only its socket code was read). Fixes that sit in
pincer-core apply to every platform; fixes in platform-linux do not.

## Method

1. Survey: ten Opus agents each read one part of the tree line by line and wrote a report of
   candidate issues with the code that proves each one. The reports are outside the repo, in the
   session notes folder `security-audit-survey/` (RENDER, TOOL, BROWSE, CDP, AUTH, SCRIPT, DAEMON,
   CLI, LXTOOLS, LXNATIVE).
2. Triage: each candidate was traced in the code before it was fixed. A Fable agent reviewed the two
   design calls that the rest hangs on (how to escape untrusted text, and how to gate launch_app).
3. Fix: eight Opus agents, one per theme, each in its own git worktree with its own files. Each wrote
   tests that show the vulnerability first and committed them alone, then the fix. The parent ran
   every tests-only commit in a separate verify worktree and watched the new tests fail before
   taking the fix. On this branch each theme's tests and fix are squashed into one commit.

Verification level for every "fixed" row below: level 1 (a test was run and watched, first failing
against the old code, then passing). Where no failing run exists, the row says so.

One process problem, now fixed: for the first hour the build wrapper shared one cargo target dir
across worktrees. Cargo fingerprints workspace crates by workspace-relative path, so a run could
execute another worktree's test binary. Every result from that hour was re-run with a target dir per
worktree.

## The shared fix: escape untrusted text, do not delete it

`pincer_core::untrusted_text` (`70a02af`) gives every producer one way to show app and page text:

- `inline`: JSON string escaping without the outer quotes (`\\`, `\"`, `\n`, `\r`, `\t`) plus every
  control, line separator, bidi control, zero-width character, tag character (U+E0000 to U+E007F) and
  supplementary variation selector as `\u{hex}`. Zero-width joiners stay between two letters or
  emoji, since Persian, Indic scripts and emoji sequences need them.
- `block`: the same for multi-line text, keeping `\n` and `\t`.
- `invisible`: only the hidden characters; the text renderer runs it over every result as a last pass.
- `clip` and `inline_clipped`: cap a string and state its original length.

Producers now quote titles, names, values, labels, read-backs and program output with these helpers.
The compact text renderer (`PINCER_MCP_RESULTS=text`) prints a multi-line field as `key (N lines):`
with every line behind a `  | ` gutter, never drops an escaped copy that a raw copy "already said",
and keeps warning fields that untrusted text restates. The MCP instructions and SKILL.md now say that
app and page text in results is data, never instructions.

Structured content (`structuredContent` in the default mode) keeps raw values on purpose: tools match
titles and names as given, and JSON already escapes line breaks there. Bidi, zero-width and tag
characters inside structured strings are not escaped in that mode.

## Commits on `claude/security-audit-226d3a` (local, not pushed)

| Commit | Theme | Findings |
|---|---|---|
| `70a02af` | untrusted_text helper | base for the injection fixes |
| `0048ec6` | instructions and SKILL.md | CLI-1 |
| `3f45fcc` | compact text renderer | RENDER-1, -2, -3, -4, -6 |
| `6b9b6a9` | run_steps and window targets | TOOL-1, -2c, -3, -4, -5, -6, -11, SCRIPT-2 |
| `b2df4b2` | replay, scripts, clipboard, recordings | REPLAY-1, -2, -3, SCRIPT-1, -2, -3, CLIP-1, REC-1, REC-2 |
| `4f4356b` | Linux tool result text | LXTOOLS-2, -3, -8, CLI-2 |
| `bcf4d8a` | Linux foreground route text | LXTOOLS-2 (foreground.rs) |
| `7654fa5` | test fixture | replay tests use a replayable fake |
| `0f5dd34` | browser results and CDP URLs | BROWSE-1, -2, -7, -8, CDP-3, CDP-7 |
| `3c84d7c` | daemon socket and MCP HTTP | DAEMON-2, -3, -4, -5, -7, CLI-3, CLI-11 |
| `cda5870` | AT-SPI tree rows | LXNATIVE-1, -2, -3, -6, RENDER-5, -6, -7 |
| `7ee5516` | AT-SPI hit-test owner text | LXNATIVE-1 (hit_test.rs) |
| `91d7dfa` | launch_app authorization (core) | LXTOOLS-1, AUTH-1, AUTH-2, AUTH-3, AUTH-6, TOOL-7, LXTOOLS-6 |
| `6355164` | launch_app attestation (Linux) | LXTOOLS-1, -7, -9, -11, -12 |
| `8037de0` | this report | |
| `9b650f5` | launch_app `name` from PATH in unrestricted mode | user decision 1 |
| `217cf4b` | secret uploads and screenshot overwrites | TOOL-8, CDP-8 |
| `45b8f78` | envelope HTTP carrier removed | DAEMON-1 |
| `ba27447` | RUNTIME.md launch checks and manifest keys | user decision 2 |

Suites at `ba27447`, each compiled from this tree: pincer-core 1128 passed, 0 failed, 1 ignored;
platform-linux 885 passed, 0 failed, 12 ignored; pincer 168 passed, 0 failed. The macOS
and Windows crates were not built (no toolchain for them here); none of their files changed, but
they call pincer-core and were not compiled against these changes.

## Findings

Severity is the survey's, after triage. Status: fixed, partly fixed, open (with the reason), or not
real. Test names are the ones that failed first and pass now.

### Result rendering (RENDER)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| RENDER-1 | medium | fixed | `a_multi_line_field_prints_every_line_escaped_under_its_line_count`, `a_long_multi_line_field_is_cut_with_a_stated_remainder`, `a_multi_line_evidence_detail_prints_as_a_block`, `object_keys_inside_a_value_are_quoted_like_strings` |
| RENDER-2 | medium | fixed | renderer half: `a_record_whose_title_breaks_its_line_keeps_its_escaped_copy`, `a_restated_string_with_a_hidden_character_or_line_break_is_kept`; producer half in the LXTOOLS, TOOL, LXNATIVE rows |
| RENDER-3 | low | fixed | `protected_fields_survive_a_key_value_restatement` |
| RENDER-4 | low | partly fixed | `degraded`, `degraded_reason`, `escalation` are protected (`degraded_and_escalation_fields_survive_a_title_that_quotes_them`); `suggestion`, `refusal`, `next_call` and `verified: false` can still be hidden by restating text |
| RENDER-5 | low | fixed | rows escape quotes and parsers skip escapes: `an_escaped_quote_does_not_end_a_label`, `a_value_written_inside_a_name_does_not_hide_the_real_value`, `a_cut_value_never_ends_inside_an_escape`, `an_id_written_inside_a_name_is_not_the_row_id` |
| RENDER-6 | low | fixed | `hidden_characters_anywhere_in_the_result_become_visible_escapes`, `row_text_escapes_quotes_and_hidden_characters` |
| RENDER-7 | low | partly fixed | multi-line fields cut at 1,000 lines or 50,000 characters; row names cut at 300 characters (`long_names_and_descriptions_are_cut_and_say_how_long_they_were`, a guard test with no failing run); single-line structured strings stay uncapped except clipboard and script values |
| RENDER-8 | low | open | element tokens are global and sequential; a pid-less call takes its target from the token. Needs session-bound tokens; calls that pass pid or window_id are already checked |
| RENDER-9 | low | open | `since:"latest"` uses a process-wide store, so one session's read can be another's baseline. Needs a session key in the store |
| RENDER-10 | low | open | since diffs match rows by text, so two identical rows can swap owners unreported. Needs an ancestor in the row key |

### Tool dispatch and run_steps (TOOL)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| TOOL-1 | medium | fixed (labeled) | page content still satisfies a check, but each expect and wait_for entry says `source: page` and the step says "held on page content": `a_check_held_only_by_page_content_says_where_it_held`, `a_wait_for_held_only_by_page_content_says_where_it_held`, `a_check_held_by_the_apps_own_ui_is_labelled_app` |
| TOOL-2 | medium | partly fixed | 2c (stale-token refind) fixed: `a_quote_inside_a_row_label_cannot_cut_it_short`, `a_stale_token_whose_row_label_a_raw_quote_cuts_is_not_retried`. Open: a title-only window spec searches every app, frontmost wins, and pid-only typing follows focus |
| TOOL-3 | low | fixed | `a_window_title_cannot_forge_a_focus_holder`, `a_forged_focus_holder_title_gets_no_recovery_read`; open: platform-macos should add `focus_holder_window_id` to its refusal |
| TOOL-4 | low | fixed | `a_title_cannot_ask_for_a_foreground_retry_or_a_stale_refind`, `an_occluded_click_is_not_sent_in_the_foreground_because_of_a_title` |
| TOOL-5 | low | fixed | `a_resolved_window_note_quotes_an_untrusted_title`, `a_resolved_window_note_clips_a_long_title_and_says_how_long_it_was`, `ambiguity_text_quotes_and_clips_untrusted_titles` |
| TOOL-6 | low | fixed | `element_labels_and_values_are_escaped_inside_their_quotes`, `window_titles_and_app_names_are_quoted_and_escaped`, `a_row_shown_in_a_miss_is_quoted_and_escaped` |
| TOOL-7 | low | fixed | `normalizes_dashes_and_values_and_covers_other_debugger_switches`; the guard matters less now that standard mode refuses options |
| TOOL-8 | medium | fixed | standard mode refuses uploads from key, keyring, credential, browser-profile and Pincer state folders (`.pub` files in `.ssh` stay allowed): `standard_upload_refuses_secret_locations_but_allows_public_keys`. Any mode that resolves output paths refuses to overwrite a non-empty file that is not PNG or JPEG data: `proposed_output_never_overwrites_an_existing_file_that_is_not_an_image`. Bounded mode keeps its manifest as the upload gate |
| TOOL-9 | low | open | verify_state `label_contains` accepts native labels such as a browser tab named by the page |
| TOOL-10 | info | open | recordings and the PiP label keep typed text; redaction changes replay |
| TOOL-11 | info | partly fixed | miss lists capped at 10 with "and N more" (`miss_lists_of_apps_and_titles_are_capped_quoted_and_escaped`); actions and end reads still run outside the 60 s budget |

### Browser results (BROWSE)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| BROWSE-1 | medium | fixed | `supplemented_role_attribute_cannot_forge_outline_lines`, `outline_quotes_values_like_names` |
| BROWSE-2 | medium | fixed | `dialog_inspect_escapes_page_text_and_names_the_opener_origin` (with CDP-7) |
| BROWSE-3 | medium | open | trusted clicks do not check what is under the point, and `opacity:0` overlays pass the occlusion test. Needs a live hit test before delivery |
| BROWSE-4 | low | open | a main-world visibility probe gives the page a callback just before typing, and browser_type does not re-check focus |
| BROWSE-5 | medium | open | hidden page text reaches the model unmarked |
| BROWSE-6 | low | open | `opacity:0.95` counts as hidden |
| BROWSE-7 | low | fixed | `page_text_bounds_state_the_original_length`, `page_labels_urls_and_titles_in_results_are_capped` |
| BROWSE-8 | low | fixed | `semantic_text_escapes_controls_bidi_and_tag_characters` |
| BROWSE-9 | low | open | third-party frame content has no origin label |
| BROWSE-10 | info | open | predictable snapshot and dialog ids |
| BROWSE-11 | info | open | browser_navigate allows loopback and private hosts; conflicts with a no-local-network rule. Suggested fix: refuse them unless configured |

### Browser infrastructure (CDP)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| CDP-1 | medium | open | the consent automation can click Allow on another local client's remote-debugging prompt. Needs the prompt bound to Pincer's own connection |
| CDP-2 | medium | open, contained here | the isolated browser listens on an unauthenticated loopback port (`--remote-debugging-port=0`). On this machine every `pincer mcp` runs under the `unshare -rn` wrapper in its own network namespace (checked 2026-11-13: five running servers, five namespaces, none the host's, no separate daemon), so only Pincer and the apps it launched can reach the port. Without the wrapper the risk stands. Full fix: `--remote-debugging-pipe` and a pipe transport |
| CDP-3 | low | fixed | `userinfo_and_portless_urls_are_rejected`, `the_dialer_reads_the_host_after_the_userinfo` (a guard test) |
| CDP-4 | low | open | a download keeps the page's file name and extension in any approved directory |
| CDP-5 | low | open | the browser-wide download window lets other tabs drop files |
| CDP-6 | low | open | the setup-page proof does not exclude web content |
| CDP-7 | low | fixed | with BROWSE-2; `cut_dialog_text_states_its_original_length` |
| CDP-8 | low | fixed | with TOOL-8 |
| CDP-9 | low | open | the restart-required path leaves remote debugging on in the real profile |
| CDP-10 to CDP-12 | info | open | nested-reply stalls, the root-owned check skipped with a pid, a stale DevToolsActivePort |

### Sessions, authorization and consent (AUTH)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| AUTH-1 | high | fixed | `launch_app_rules_do_not_admit_unconstrained_launch_arguments`: a constrained launch_app rule no longer admits an unconstrained name, bundle_id, launch_path, additional_arguments or urls. Rules for other tools keep their meaning |
| AUTH-2 | high | fixed | `launch_resources_require_a_declared_launch_app`, `bounded_launch_runs_only_declared_launch_apps`, `capability_manifest_scopes_native_menu_invocation`: bounded launches need an app declared with `launch: true` (options also need the new `launch_options: true`), and invoke_menu goes through the desktop input adapter |
| AUTH-3 | medium | partly fixed | `policy_runs_again_on_canonical_file_paths`: the policy runs again on resolved paths. Open: an unanchored `pattern` still matches anywhere in the argument |
| AUTH-4 | low | open | manifest app identity accepts app-controlled `bundle_id` and `launch_path` |
| AUTH-5 | low | open | manifest pids are not bound to a process start time |
| AUTH-6 | low | fixed | `browser_url_refusals_name_only_the_scheme` |
| AUTH-7 | low | open | consent runs before lifecycle admission; matters only where a consent provider exists (the Linux daemon has none) |
| AUTH-8 | low | open | session labels are one first-come namespace with unbounded tombstones |
| AUTH-9 | low | open | any session can stop another session's recording |
| AUTH-10 | low | open | an ended session's launched apps become killable by every agent |
| AUTH-11 | low | open | the "never target Pincer's own pid" rule covers some tools only |
| AUTH-12 | low | open | origin-scoped manifests still allow zoom on the browser window |
| AUTH-13 to AUTH-16 | info | open | delivery ceiling ignored, manifest approval not bound to a hash, README overstates fail-open, labels echoed raw |

### Scripts, replay and recordings (SCRIPT)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| REPLAY-1 | high | fixed | `replay_refuses_turns_that_are_not_single_actions`: only single actions replay; anything else is refused as `not_replayable` before it runs |
| REPLAY-2 | low | fixed | `replay_never_runs_a_nested_replay` |
| REPLAY-3 | low | fixed | `replay_summary_escapes_names_from_the_trajectory` |
| SCRIPT-1 | low | fixed | `console_and_uncaught_errors_cannot_write_lines_of_their_own` |
| SCRIPT-2 | low | fixed | `call_rows_escape_labels_and_op_names`; locator half in TOOL-6 |
| SCRIPT-3 | low | fixed | `a_large_return_value_is_cut_and_says_so` (64 KiB, `value_truncated`) |
| SCRIPT-4 | info | open | `launch_app` is in the script's read list; the launch adapter now gates it like a direct call |
| CLIP-1 | low | fixed | `a_huge_clipboard_text_is_cut_and_says_so` (1 MiB, `text_truncated`) |
| REC-1 | low | partly fixed | folders 0700, files 0600 (`recording_output_is_private_to_the_user`); typed text and set values are still stored |
| REC-2 | low | fixed | `recording_writes_never_follow_a_planted_symlink`, `recording_refuses_a_turn_folder_that_is_a_symlink`; open: `cursor_sampler.rs` and ffmpeg's `recording.mp4` still follow links and take the umask |
| VIDEO-1 | low | open | a relative `output_dir` reaches ffmpeg as given (unrestricted mode or the CLI) |
| REC-RENDER-1 | info | open | `pincer recording render` lets ffmpeg auto-detect an untrusted folder's format |

### Daemon, sockets and MCP envelope (DAEMON)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| DAEMON-1 | high when enabled | fixed (removed) | the envelope HTTP carrier authenticated no caller by design (it relied on upstream's Fleet route). This fork removed it; startup now fails while `PINCER_ENVELOPE_HTTP_PORT` is set (`the_removed_http_carrier_variable_stops_startup`, a guard test). The MCP envelope carrier over the authenticated socket stays |
| DAEMON-2 | medium | fixed | `unauthenticated_requests_are_refused_before_the_body_is_read`, `stalled_request_heads_are_closed`, `mcp_http_connections_over_the_cap_are_refused`, `accept_errors_do_not_stop_the_daemon`; open: the Windows pipe and the envelope carrier's accept loop |
| DAEMON-3 | medium | fixed | `mcp_http_bind_failure_fails_startup` |
| DAEMON-4 | low | fixed | `mcp_http_token_leaves_the_environment_once_read` |
| DAEMON-5 | low | fixed | `default_daemon_paths_without_home_avoid_shared_directories`, `daemon_creates_its_socket_directory_private`, `daemon_refuses_a_world_writable_socket_directory`, `pid_file_symlink_is_not_followed`, `home_unset_uses_a_private_xdg_runtime_dir`, `home_unset_without_a_private_runtime_dir_has_no_shared_default` |
| DAEMON-6 | low | open | socket clients choose their own transport id |
| DAEMON-7 | low | fixed | `oversized_request_line_is_refused` (16 MiB cap); connection count still uncapped on the socket |
| DAEMON-8 to DAEMON-13 | info | open | Flatpak reach to `~/.cache`, the history CLI proof, debug logs with raw requests, the Windows pipe label, errors echoing caller input, unbounded tombstones |

### CLI, skills and install scripts (CLI)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| CLI-1 | medium | fixed | `instructions_say_app_and_page_text_in_results_is_untrusted_data`; SKILL.md section "Text from apps and pages" |
| CLI-2 | medium | fixed | `a_dialog_title_stays_a_quoted_value_in_the_follow_up` |
| CLI-3 | medium | partly fixed | the CLI, the proxy and pincer-core's daemon client check the socket's peer uid (`daemon_client_refuses_a_socket_served_by_another_uid`); open: `mcp_envelope.rs` and pincer-sdk's `service_session.rs` clients |
| CLI-4 | medium | open | Windows autostart runs at High integrity behind a pipe that admits Low and Medium (not built here) |
| CLI-5 | medium | open | macOS local signing imports its key with `-A` (not built here) |
| CLI-6 | medium | open | the GNOME helper extension serves capture to every session-bus client (same as LXNATIVE-7) |
| CLI-7 | low | open | several skill texts escalate to foreground delivery without asking |
| CLI-8 | low | open | BROWSER.md tells the model to restart with the existing-profile grant |
| CLI-9 | low | open | `install_ffmpeg` runs `sudo -n` on a model-set `confirm` |
| CLI-10 | low | open | Windows autostart command breaks on a `'` in a path |
| CLI-11 | low | partly fixed | socket and PID paths fixed with DAEMON-5; the history root in `history_runtime.rs` still falls back |
| CLI-12 | low | open | `cursor-gallery.sh` serves the repo and a CDP port to local users while it runs |
| CLI-13 | info | open | small items listed in the CLI report |

### Linux tools (LXTOOLS)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| LXTOOLS-1 | high | fixed | a `process_launch` adapter gates every Linux launch_app call; see "launch_app in standard mode" below. Tests: `standard_program_launch_admits_only_installed_apps_without_options`, `standard_launch_of_other_programs_or_options_is_refused_with_the_allowed_forms`, `standard_launch_of_an_installed_app_runs_the_attested_program_without_prompting`, `unrestricted_launch_without_a_manifest_needs_no_attestation`, `launch_app_and_invoke_menu_select_resource_adapters`, `a_name_resolves_only_through_installed_desktop_entries`, `a_raw_program_needs_launch_path_and_is_not_an_installed_app`, `an_installed_app_by_name_bundle_id_or_its_launch_path_attests_its_entry`, `options_are_dash_arguments_or_any_argument_to_a_terminal`, `the_launch_runs_exactly_the_attested_program`, `urls_alone_open_through_xdg_open_with_private_stdio`. Open: macOS and Windows launch_app attest nothing yet |
| LXTOOLS-2 | medium | fixed | `a_window_title_cannot_forge_a_list_windows_line`, `a_window_title_cannot_forge_a_desktop_state_line`, `a_process_argv_cannot_forge_a_process_line`, `a_long_window_title_is_cut_with_its_length_named`, `bring_to_front_names_covering_windows_as_quoted_values`, `a_covering_title_cannot_ask_for_foreground_delivery`, `a_retargeted_press_names_its_window_as_a_quoted_value`, `a_popup_title_is_one_quoted_value`, `an_occluding_window_title_is_one_quoted_value`, `a_foreign_window_names_its_titles_as_quoted_values`, `a_window_title_in_the_summary_is_one_quoted_value`, `a_window_title_in_a_window_change_is_one_quoted_value`, `a_focus_holder_class_and_process_name_cannot_add_text` |
| LXTOOLS-3 | medium | fixed | `a_set_value_read_back_is_escaped_and_cut`, `a_combo_option_and_read_back_are_escaped`, `a_missing_option_list_cuts_long_labels`, `a_read_back_escapes_quotes_and_invisible_characters`. Not real in part: `no_such_option_result` already escaped through Debug formatting; only its cap was missing |
| LXTOOLS-4 | medium | open | no guard on typing into terminals, and the pty route can pick the wrong shell. Needs a terminal check and a one-pty rule |
| LXTOOLS-5 | medium | open | with the GNOME helper, `app_name` is the window title, so a page title can match `app:"KeePassXC"`. Needs the helper to report a real app id |
| LXTOOLS-6 | low | fixed | with TOOL-7 |
| LXTOOLS-7 | low | fixed | `a_launched_programs_output_is_fenced_and_escaped`; open: display names in the exited and hand-off texts are not escaped |
| LXTOOLS-8 | low | partly fixed | `an_exec_line_with_quotes_is_escaped_in_its_quoted_value`, `a_desktop_entry_cannot_forge_list_apps_fields`; open: a process is merged into an installed app by argv[0] basename |
| LXTOOLS-9 | low | fixed | `a_new_window_is_adopted_only_by_a_whole_name` |
| LXTOOLS-10 | low | open | trusts X11 `WM_TRANSIENT_FOR` and the `Pincer.` title prefix |
| LXTOOLS-11 | low | fixed | `launched_programs_do_not_inherit_pincer_variables` (every `PINCER_*` variable is removed for children) |
| LXTOOLS-12 | low | fixed | `urls_alone_open_through_xdg_open_with_private_stdio` |
| LXTOOLS-13 | info | open | get_window_state and get_desktop_state say `read_only` but can write a screenshot file |
| LXTOOLS-14 | info | open | listings have no record cap; capping the text alone would make the renderer reprint the list |
| LXTOOLS-15 | info | open | kill_app pid race, a FIFO as launch_path, Exec quoting |

### Linux native layers (LXNATIVE)

| ID | Sev | Status | Evidence or reason |
|---|---|---|---|
| LXNATIVE-1 | medium | fixed | `row_text_escapes_quotes_and_hidden_characters`, `hit_description_escapes_the_app_strings`, `a_click_summary_cannot_be_forged_by_the_element`, `owner_description_escapes_the_app_strings`, `labels_from_the_app_cannot_break_the_message_or_flood_it`; guard tests with no failing run (they call helpers the fix adds): `app_strings_cannot_add_rows_or_forge_row_structure`, `passive_rows_escape_their_role_and_text`, `the_x11_fallback_row_escapes_the_window_title` |
| LXNATIVE-2 | medium | partly fixed | rows say `off-screen` (`an_off_screen_element_says_so_in_its_row`, guard test); a failed state read still counts as showing |
| LXNATIVE-3 | medium | partly fixed | the main walk skips nodes owned by another app (`a_node_owned_by_another_application_is_not_walked`, guard test, not tried on real apps); the menu and hit-test walkers still follow foreign children |
| LXNATIVE-4 | medium | open | the process-name fallback trusts `/proc/<pid>/comm` |
| LXNATIVE-5 | medium | open | X11 ownership prefers client-set `_NET_WM_PID` over X-Resource. On X11 any client can already inject input, so the gain is limited |
| LXNATIVE-6 | medium | fixed, no failing run | `NActions` capped at 64, loops stop at the deadline, child lists checked against ChildCount (`app_reported_action_counts_are_capped`, `action_name_reads_stop_at_the_cap_and_at_the_deadline`, `a_child_list_far_longer_than_its_count_is_not_walked`). These test new helpers; the old loops needed a live AT-SPI peer to fail, so no red run exists |
| LXNATIVE-7 | medium | open | the GNOME helper extension has no caller check (needs a sender allowlist in extension.js; testable in the GNOME sandbox) |
| LXNATIVE-8 | medium, high as root | open | session-bus discovery trusts any process's environment and zbus accepts `unixexec:`. Matters when Pincer runs outside a session |
| LXNATIVE-9 | low | open | XAUTHORITY discovery trusts any process named like an X server |
| LXNATIVE-10 | low | partly fixed | clipboard text capped in pincer-core (CLIP-1); the INCR size still comes from the selection owner |
| LXNATIVE-11 | low | open | Sway title lookup matches by prefix both ways |
| LXNATIVE-12 | low | open | the AT-SPI window list trusts app names, has no frame cap, and ids can collide |
| LXNATIVE-13, LXNATIVE-14 | info | open | persistent accessibility flags; unchecked extent arithmetic |

## launch_app in standard mode

This is the one fix that changes what works by default, so it is its own pair of commits
(`91d7dfa`, `6355164`) and can be dropped on its own.

- Standard mode starts an installed desktop app by `name`, `bundle_id`, or the exact `launch_path`
  that list_apps reports, with file or URL targets. `urls` alone open through xdg-open.
- It refuses, with `program_launch_denied` and a message naming the forms that run: any other
  program, any argument or url starting with `-`, any argument at all to a terminal emulator (kitty
  and foot run positional arguments as commands), and a url that is a `.desktop` file or an
  executable.
- `name` resolves only through installed apps in standard and bounded mode. In unrestricted mode
  without a manifest (the only case where the registry skips launch attestation), a `name` that is no
  installed app falls back to a program on PATH, as before (`9b650f5`,
  `an_unattested_launch_runs_a_name_from_path_and_attestation_still_refuses_it`).
- Bounded mode needs the app declared with `launch: true`, and a new manifest key
  `launch_options: true` before a launch may pass options.
- The launch runs exactly the attested executable (argv[0] kept) and refuses with
  `launch_target_changed` if the plan changed after approval.

Effect on the tool-test harness: it runs `pincer mcp` in standard mode, and its briefs launch
KWrite, Dolphin and Pincer Test Pad by `name`. All three have desktop entries in a slot (`slot.sh`
copies `testpad.desktop` in), so they should keep working; not yet run against this build. A dev
build launched by a raw `launch_path` in standard mode is refused.

## Decisions made by the user (2026-11-13)

1. launch_app: keep the standard-mode change; restore the PATH fallback for `name` in unrestricted
   mode only (`9b650f5`).
2. `launch_options: true`: keep it and document it (`ba27447`, RUNTIME.md).
3. Uploads and overwrites: refuse secret locations in standard mode and refuse overwriting non-image
   files (`217cf4b`).
4. Envelope HTTP carrier: remove it (`45b8f78`).
5. Isolated browser port: leave it; the `unshare -rn` wrapper contains it on this machine.
