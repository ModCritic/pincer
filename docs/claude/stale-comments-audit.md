# Stale comment and string audit (2026-11-13)

An audit of the whole tree for comments, tool descriptions, schema text, error
and help strings, docs and skill text that no longer match the code, plus
leftovers from the 2026-10-29 rename to Pincer. Nine read-only subagents
(Sonnet) reported findings with proof; every finding below was checked against
the code before anything changed. Line numbers refer to `c5850db` (before the
fixes).

Kept on purpose and not reported: the LICENSE notice and upstream attribution,
dated docs, plans, RFCs and the CHANGELOG, upstream product names (Cua Spaces,
Cua Fleet, cua-spacesd, `cua doctor`, Cua-S1), the `cua/driver/v1/*` methods,
the `CUATHEM3` and `CUA2` magics, legacy-cleanup names, upstream Swift "ported
from" notes, and upstream compositor names (`cua-compositor`,
`CUA_INJECT_SOCKET`, `cua_key_named`, `cua_init_keymap`).

## Commits

| Commit | Scope |
| --- | --- |
| `585fbff` | pincer crate: CLI help, doctor, proxy error, serve/mcp_http/skills comments |
| `89ddda4` | rust/README.md: crate table, build note, policy example |
| `c0a0755` | pincer-core and pincer-contract tool text and comments, plus the generated TS contract doc line |
| `899d9f3` | platform-linux |
| `dadc6f2` | platform-macos |
| `3ef8ca4` | platform-windows |
| `b7896a3` | hyprland-plugin build options and test names |
| `23d95e5` | docs, skill pack, SDK READMEs, scripts, fixtures, notices |
| `b38afab` | cursor-overlay and pincer-e2e comments |

## Checks run

- `cargo test --release -p platform-linux --lib`: 835 passed, 12 ignored.
- `cargo test --release -p pincer-core -p pincer-contract`: 1128 passed, 2 ignored.
- `cargo test --release -p pincer`: 212 passed, 1 ignored, 1 failed. The failure,
  `protocol_proxy_recovery_test::idle_reclaimed_unnamed_session_is_recreated_through_the_daemon`
  (`rust/crates/pincer/tests/protocol_proxy_recovery_test.rs:492`), predates this audit:
  d8e6d5e (11/09/26) made the daemon restart a session label the idle sweep ended, and the
  test still expects the old refusal. This audit touches neither file.
- `pincer-contract-gen all --check`: `contract/manifest.json` is current.
- `cargo check --release --target x86_64-pc-windows-gnu -p platform-windows --lib --examples`: clean
  (two pre-existing dead-code warnings).
- platform-macos cannot build here; every edited file parses with rustfmt.
- `cargo fmt --check`: no diffs in lines this audit touched (pre-existing diffs elsewhere,
  for example pincer-core `batch_tools/locate.rs`, were left alone).
- hyprland-plugin `python3 -m unittest test_release test_profile_release`: 54 passed.

## Left for a decision (behavior changes)

These need a code change, not a text fix, so they were not touched.

| Where | What | Options |
| --- | --- | --- |
| platform-windows `src/tools/impl_.rs:4709`, `:5055` | type_text's `delay_ms` is parsed into `_delay_ms` and never applied; the result text still says "30ms delay" | pass it to `post_type_text_with_delay`, or drop `delay_ms` |
| platform-windows `src/tools/impl_.rs:8836-8846` | set_config's reply reads `agent_cursor.enabled` from the first cursor, not the caller's session (description now says the reply has no `cursor` block) | add the session lookup and `cursor` block |
| platform-windows `examples/screenshot_parity.rs` | calls the removed `screenshot` tool | delete it or retarget at get_window_state |
| platform-macos `src/tools/move_cursor.rs:46` | schema advertises `cursor_id`, which the handler ignores (a test pins that) | remove it from the schema |
| platform-macos `src/tools/type_text_chars.rs` | unregistered tool module kept alive by a `let _` | delete it |
| platform-macos `src/permissions/gate.rs:260-345` | telemetry context and observer plumbing with no consumer since 372a005 | delete it |
| platform-macos `src/tools/type_text.rs:526`, `desktop_pointer_guard.rs:115`, `set_value.rs:1188` | escalation `recommended: "page"` names the removed page tool's rung (the reason text now names the browser tools) | rename the rung value (structured output change) |
| pincer-core `src/recording.rs:856` | `RecordingSession::configure` has no callers | delete it and its comment mentions |
| platform-linux `src/input/delivery.rs:61` | `BackgroundUnavailable::NoLibeiBackend` is never constructed but documented as returned | wire it up or remove it |
| typescript `src/daemon.ts:15,71,86` | looks only for `/Applications/Pincer.app`; source installs make `PincerLocal.app` | also check PincerLocal.app |
| python `src/pincer_driver/wrapper.py:70-77` | sets `PINCER_INSTALL_CHANNEL`, which nothing reads | delete it and its test assertion |

## Left as uncertain or out of scope

| Where | Why left |
| --- | --- |
| pincer-sdk `src/lib.rs:1317` (`PincerProtocol` / `PincerLike`) | `PincerProtocol` exists nowhere, but the comment is copied into the generated TS binding and "released" may mean upstream's names; change it with the next binding regeneration |
| pincer crate `proxy.rs:202,260`, `cli.rs:3,988-1184` | Swift parity notes (`makeProxy`, `waitForDaemon`, `CombinedDocs`, `DiagnoseCommand`): upstream attribution |
| pincer-e2e `tests/README.md:67-69`, `docs/e2e-ci-reporting.md`, `docs/test-harnesses-guide.md:443-446` | describe upstream's `cases.jsonl`/`results.jsonl` reporter, which this repo does not have; dropping that pipeline from the docs is a bigger call |
| pincer-e2e `tests/README.md:125`, `docs/test-harnesses-guide.md:428` | say `PINCER_REQUIRE_GUI=1` turns desktop self-skips into failures; only pincer-sdk and `private_worker_test.rs` read it, so the claim is likely wrong, but the Windows runner behavior could not be checked here |
| pincer-contract `src/desktop.rs:218` (set_window_frame) | still names a maximized window as refusable; true on Windows (`impl_.rs:9439`), so Linux got an addendum instead |
| pincer-core `src/protocol.rs:403` (server instructions) | "detail.next_call if refused" holds for browser_consent_required, the common case |
| pincer-core `src/recording_tools.rs:86-89` | record_video schema says "ffmpeg on PATH"; Wayland uses wf-recorder or the GNOME helper |
| pincer-core `src/agent_cursor.rs:124` | names cua-spacesd constants that live upstream |
| platform-linux `atspi/native.rs:3089-3100` | list_windows doc says the walk ignores the xid; partly true on GNOME/KDE Wayland, where resolve_window_frame often falls back to an app-wide walk |
| platform-linux `atspi/native.rs:4224` | points at `qt-atspi-crash.md` in a local bench folder outside the repo |
| platform-linux `pip/mod.rs:3-30`, platform-windows `pip/mod.rs:18-44` | PiP error points at upstream's tracker; a test asserts the string |
| platform-linux `tools/impl_.rs:10837` | hotkey's legacy `key`+`modifiers` fallback; depends on whether the registry enforces `additionalProperties` |
| platform-linux `tools/impl_.rs:4816` | held-button refusal compared with drag's synthetic fallback, which now allows Qt |
| platform-linux `input/mpx_keyboard.rs:5-7`, `input/mod.rs:3798,3847` | list Qt among toolkits that drop synthetic keys; needs a live test |
| platform-linux `wayland/ext_screencopy.rs:20-23`, `wayland/hyprland.rs:1`, `wayland/mod.rs:2152` | follow-up notes and a "read-only" module claim that need the author's intent |
| platform-linux `overlay.rs:27`, `atspi/native.rs:6359-6509`, `:3033`, upstream `#NNNN` refs | shorthand or attribution, not wrong enough to change |
| platform-macos `src/focus_guard.rs:29-49`, `src/permissions/gate.rs:3-32` | layer and terminal-only narratives need a larger rewrite by someone who knows the macOS flow |
| platform-macos `src/tools/get_window_state.rs:555-604` | verbose `_note` says prefer `elements` while markdown is the default; a wording policy call |
| platform-macos `src/browser/cdp_client.rs:111-150` | `CdpSessionCache` has no live caller; dead-code question |
| platform-macos `src/tools/list_windows.rs:17`, `health_report.rs:37-135`, `apps/mod.rs:49,178` | shared wording, PincerLocal bundle id handling, upstream tool names |
| platform-windows `src/tools/stubs.rs` | non-Windows stub registry still has a `screenshot` stub |
| platform-windows `src/tools/impl_.rs:6864-6872` (double_click), `:3292` (click) | element route wording; several routes, medium confidence |
| platform-windows `src/overlay.rs:1326` TODO | still open: overlay paint has no per-monitor DPI handling |
| Linux and macOS get_window_state `max_elements` schema | omit that a `query` raises the cap to the platform default (fixed on Windows); an omission, not stale text |
| python/typescript READMEs and `docs/native-window-sdk-migration.md` | "next breaking release" framing from upstream 0.26; rewording is an editorial call |
| `hyprland-plugin/docs/production-proof.md:312`, validation tables naming Cua Driver 0.22/0.23 | historical proof and validation records |
| `docs/sdk-first-runtime-north-star.md`, `docs/why-cua-driver-uses-mcp-instead-of-uniffi.md`, `docs/mcp-protocol-and-skills.md:90-110` | upstream design records |
| `docs/tool-output-format.md`, WORKFLOW.md `structuredContent` | do not mention `PINCER_MCP_RESULTS=text`; an addition, not a stale claim |
| `rust/scripts/PincerBundle/Contents/MacOS/.gitkeep`, `pincer-cursor-motion/tests/common/golden_cases.rs:361` | upstream history and fixture data |

## Rejected findings

- platform-linux `tools/impl_.rs:19128` and `tools/stubs.rs:369`: an agent said
  `type_text_chars` has no alias. pincer-core `src/tool.rs:1691-1702` rewrites it to
  `type_text`, so the comment is right.
- platform-windows: the brief said `PACKAGED_ONLY_EXES` replaced `XAML_HOST_EXES`;
  both exist (`input/keyboard.rs:56,70`), so comments naming `XAML_HOST_EXES` stand.
- pincer crate test headers naming `compatibility_contract_test`, `schema_consistency_test`
  and `cross_platform_behavior_test`: the files exist.

## Fixed

### pincer crate and rust/README.md (`585fbff`, `89ddda4`)

| Where | Was | Now |
| --- | --- | --- |
| `cli.rs:162`, `:330` | skills help listed 5 agents | all 8 (adds Pi, Antigravity, Hermes; `skills.rs` AGENTS) |
| `cli.rs:429-432`, `:1831`, `:3864`, `:3964` | cursor `--timing` listed native, fitts, fixed | all 7, steady default (`pincer-contract/src/cursor.rs:253`) |
| `cli.rs:381-396` | compat flag "selects the compat surface" | has no effect (`cli.rs:25-28`) |
| `proxy.rs:208` | `open -n -g -a Pincer --args serve` on every OS | macOS keeps it; elsewhere `pincer serve` |
| `doctor.rs:215` | `PINCER_KEEP_VERSIONS` (removed with self-update, ee05869) | dropped |
| `doctor.rs:287`, `:480` | named the removed `screenshot` tool | get_window_state |
| `serve.rs:6-14` | four request methods | notes the other methods the dispatch handles |
| `mcp_http.rs:10` | "this PR" | trycua/cua#1799 |
| `skills.rs:38` | "same agent dirs as upstream" | "agent dirs it detects" |
| `rust/README.md:19` | pip-preview "packaging preview helper"; 4 crates missing | PiP types; adds pincer-contract, pincer-sdk, pincer-cursor-motion, pincer-bindgen |
| `rust/README.md:32-34` | `PINCER_BINARY` (read nowhere) | install scripts take `target/<profile>/pincer`; wheel bundles the binary |
| `rust/README.md:100-121` | policy example used screenshot, wait, shell_execute, file_delete and launch_app `app` | real tools and `name` |

### pincer-core and pincer-contract (`c0a0755`)

| Where | Fix |
| --- | --- |
| `batch_tools.rs:167`, `script_tool.rs:189` | run_steps and run_script lists add move_cursor and move_along_path (`BATCHABLE_TOOLS`) |
| `recording_tools.rs:69` | start_recording mentions the 300 s idle auto-stop (`pincer-sdk/src/runtime.rs:26,530`) |
| `recording_tools.rs:11` | dangling `JOURNAL_VIDEO.md` |
| `recording.rs:610`, `:853` | CLI `recording start` goes through start_recording; `configure()` has no caller |
| `recording_render.rs:115-117` | `build_zoompan_expressions` |
| `cursor_sampler.rs:6-13`, `:145-157` | Linux sampler is a stub on X11 too |
| `image_utils.rs:7` | dangling `PINCER_DEDUP_AUDIT.md` |
| `video.rs:7-15` | Linux backends (x11grab, wf-recorder, GNOME helper); registered by the pincer-sdk runtime |
| `tool.rs:743-750`, `:7026` | type_text_chars is an alias on every platform |
| `tests/session_lifecycle.rs:9`, `:62` | the SDK runtime stops the session's recording |
| `pincer-contract/src/windows.rs:339` (and the TS binding) | since_status lists no_baseline |
| `browser/types.rs:152` | DevToolsActivePort |
| `browser/tools.rs:306` | only browser_consent_required carries detail.next_call |
| `socket_io.rs:3` | `daemon::send_request` |
| `tests/cursor_hook_platform_support.rs:3` | dangling `AGENTS.md`; rule stated inline |
| `cursor_shape.rs:5` | "Pincer's own cursor" |

### platform-linux (`899d9f3`)

| Where | Fix |
| --- | --- |
| `lib.rs:7-8`, `health_report.rs:242` | AT-SPI is native zbus; capture is MIT-SHM/XGetImage then `import` |
| `lib.rs:185` | `wsl_env_set` |
| `lib.rs:242-245` | `compat` is ignored; ScreenshotCompatTool is gone |
| `capture.rs:1012-1016` | helpers moved to `pincer_core::image_utils`; dangling audit file |
| `tools/impl_.rs:160` | `ZoomRegistry` header |
| `tools/impl_.rs:859` | label is name, else description |
| `tools/impl_.rs:3522`, `:5270`, `:7195`, `:12721` | orphaned or misplaced doc blocks moved to their items |
| `tools/impl_.rs:4990` | effect `confirmed`, not `landed` |
| `tools/impl_.rs:8345` | no legacy element_index branch |
| `tools/impl_.rs:12961`, `:19087` | get_window_state returns a screenshot by default |
| `tools/impl_.rs:17725` | unregistered type_text_chars schema drops `type_chars_only` |
| `tools/impl_.rs:18329` | set_window_frame: X11 restores a maximized or full-screen window first (015e819) |
| `terminal.rs:35`, `installed_apps.rs:249`, `browser_setup_ui.rs:173-189` | single terminal list; launch_path runs without a shell; URL pasted and checked by page title |
| `atspi/native.rs:427`, `:1109`, `:6582`, `:7361`, `:7510` | doc blocks moved back onto Visited, FrameExtents, is_passive_role, element_bounds_by_walk, x11_window_origin |
| `atspi/native.rs:3587`, `:4652`, `atspi/mod.rs:332` | focused_is_editable doc merged; activation verb, not "first action" |
| `clipboard.rs:146`, `overlay.rs:1047`, `:1090`, `:2007` | upstream CI names, SetShape/SetGradient, 80 ms reassertion |
| `input/mod.rs:1-12`, `:3955`, `:2927`, `:1698`, `:1874` | MPX or XSendEvent routes; pair kept for the session; Qt drops only wheel; popup_info doc |
| `input/mpx_keyboard.rs:15-19`, `:34-36` | pair kept for the session; unconfirmed delivery |
| `wayland/mod.rs:668`, `:906`, `:3202` | Sway fills pid and geometry; no shm_open fallback; `cua_init_keymap` |
| `wayland/libei.rs:575`, `:601`, `:1146`, `:1313` | worker serves more than click/move/type; `cmd_ready`; xkb keymap typing |
| `wayland/portal_screenshot.rs:108` | `pincer doctor` |

### platform-macos (`dadc6f2`)

| Where | Fix |
| --- | --- |
| `tools/type_text.rs:62`, `:462`, `:512`, `:531` | `page` tool (removed in 0678402) replaced by get_browser_state and browser_type |
| `tools/set_value.rs:51`, `tools/type_text.rs:21` | type_text_chars replaced by type_text / `delay_ms` |
| `tools/check_permissions.rs:307` | read-only by default; `prompt:true` is for the host setup route |
| `tools/double_click.rs:53` | x/y are window-local screenshot pixels |
| `tools/get_screen_size.rs:18` | pixel actions take screenshot pixels |
| `capture.rs:1066` | dangling audit file |
| `cursor/mod.rs:6`, `cursor/overlay.rs:9-10`, `:26-30` | `send_command(key, cmd)`, `CMD_RX_CELL`, per-cursor MotionConfig |
| `ax/mod.rs:4`, `ax/tree.rs:3`, `:651`, `ax/window_scope.rs:10`, `input/mod.rs:4`, `tools/mod.rs:272` | element_token; current tree line format |
| `browser/cdp_client.rs:2`, `:117-121`, `tools/mod.rs:781` | no page tool or resolve_cdp_port; dispatch_keystrokes |
| `permissions/mod.rs:10`, `panel.rs:44`, `gate.rs:504` | panel is opt-in via PINCER_PERMISSIONS_PANEL |
| `permissions/gate.rs:284`, `:297`, `tools/health_report.rs:357` | telemetry consumer removed |
| `lib.rs:88`, `:111`, `tools/mod.rs:23`, `:904` | compat has no effect; capture lives in crate::capture |
| `tools/mod.rs:303`, `:1110` | BackgroundMutationLease doc moved; alias resolved in pincer_core's ToolRegistry |
| `tools/get_config.rs:55`, `window_change_detector.rs:454`, `:633`, `windows.rs:51-57` | session key order; detect_bounded_with; direct CGWindowListCopyWindowInfo |

### platform-windows (`3ef8ca4`)

| Where | Fix |
| --- | --- |
| `tools/impl_.rs:4566-4578`, `:4558`, `:4876` | type_text routing: element_token tries ValuePattern on any host; no XAML refusal exists |
| `tools/impl_.rs:5633`, `input/mouse.rs:420`, `:733`, `uia/fg_bypass.rs:90` | nothing routes to the pincer-uia worker pipe |
| `tools/impl_.rs:6306` | scroll's element_token drives UIA ScrollPattern |
| `tools/impl_.rs:8222` | move_cursor scope desktop moves the real pointer |
| `tools/impl_.rs:8701-8710`, `:8772` | get_config keys; set_config reply has no `cursor` block |
| `tools/impl_.rs:9582`, `:9623` | bring_to_front exists on macOS and Linux; force_foreground_attached |
| `tools/impl_.rs:3317`, `:2237`, `:1251`, `:10346` | real "No windows found" error; no screenshot tool; query floor; removed structs |
| `capture.rs:7-27`, `:439`, `:697-709`, `:833` | WGC fallback order; wgc_capture_impl; dangling audit file; png_dimensions_pub |
| `diagnostics.rs:10`, `lib.rs:134`, `overlay.rs:7` | no screenshot tool; compat ignored; timer cadence |
| `input/keyboard.rs:8-14`, `input/inject.rs:15`, `:450` | is_xaml_host_hwnd callers; upstream RE doc; point_on_virtual_desktop |
| `msaa.rs:17`, `uia/fg_bypass.rs:14`, `:28`, `examples/zdrop_probe.rs:3`, `:15` | flash-repro scripts and RE plan marked as upstream's |
| `uia/snapshot_uaf_repro.rs:2-20`, `uia/windows_enum.rs:18`, `examples/recording_parity.rs:6` | publish_for_session and `inner`; overlay.rs allow; dangling JOURNAL_VIDEO.md |

### hyprland-plugin (`b7896a3`)

`CUA_HYPRLAND_*` CMake options in `packaging/release/PKGBUILD.in:41-44`,
`PROFILE-PKGBUILD.in:42-45`, `packaging/arch/PKGBUILD:52`,
`packaging/release/test_release.py:100`, `README.md:85-105`, `tests/README.md:32`
and two proof docs became `PINCER_HYPRLAND_*` (CMakeLists.txt:5-16 defines only
those since 5d37cca). `protocol/input-experiment.md` names the current sockets,
lease header, seats and `PINCER_EXPERIMENTAL_HYPRLAND_INPUT`.

### Docs, skills, SDKs, scripts and fixtures (`23d95e5`)

| Where | Fix |
| --- | --- |
| `rust/Skills/pincer/RUNTIME.md:72-80` | authorization flags go on `pincer serve` (`cli.rs:232-240` rejects them on `mcp`) |
| `RUNTIME.md:101`, `docs/cursor-themes.md:104-109` | seven timings, steady default |
| `LINUX.md:343`, `:488`, `WINDOWS.md:445-451`, `:705`, `:770-780` | the probes `pincer doctor` runs; pincer-local-serve task |
| `python/README.md:111` | 30 minute session idle limit |
| `docs/mcp-protocol-and-skills.md:82` | ten skill files |
| `docs/cursor-themes.md:315` | generated GIFs are not committed |
| `scripts/node-runtime-NOTICE.md:1-10` | pincer_node_runtime.node; this repository |
| `scripts/_install-local-rust.sh:6-9` | no backend choice |
| `rust/THIRD_PARTY_NOTICES.md:3` | Pincer workspace |
| `tests/fixtures/README.md`, `rust/test-apps/README.md`, `tests/fixtures/smoke/README.md` | gtk4, qt6, isolated-input, visual-only-canvas; protocol tests location; no committed baseline |
| `tests/manual-e2e-allowlist.txt:47` | test removed in 66c8ea7 |

### cursor-overlay and pincer-e2e (`b38afab`)

`cursor-overlay/src/render_state.rs:1245` (no `display_size`; theme artwork) and
`pincer-e2e/tests/desktop_scope_windows_test.rs:5` (two upstream test names).

The GNOME Shell helper (`wayland-helper/winrects@pincer/extension.js`) was also
checked: every Rust file and function its comments cite exists.
