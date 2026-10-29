# Pincer Test Harnesses Guide

This contributor guide explains where the Pincer tests live, how to run
them, and where to add a new case. For current coverage and platform gaps, use
`test-matrix.md` and `action-support.md`.

## The Short Version

There are two main test layers:

1. **Unit and protocol tests** exercise Rust code and the public MCP/CLI
   contract without launching a real application.
2. **Harness E2E tests** launch a small application built from this repository,
   drive it through the Rust driver, and verify an external application or
   desktop state.

The Rust tests are the source of truth. Python tests, old shell runners, and
historical recording scripts are not part of the canonical E2E path.

Upstream runs the complete matrix on every OS with a canonical command that
takes no suite selector:

```text
Linux:  scripts/ci/linux/run-rust-e2e.sh
Windows: .\scripts\ci\windows\run-rust-e2e.ps1 -RequireGui
macOS:  scripts/ci/macos/run-rust-e2e.sh
```

Those runners live in upstream's monorepo-level `scripts/ci/` and are not part
of this repository, so this guide's references to them describe upstream's
process. Here, run the suites with `cargo test` as described in
[`rust/crates/pincer-e2e/tests/README.md`](../rust/crates/pincer-e2e/tests/README.md).
The legacy `tests/runners/windows-sandbox/` runner is a supporting runner, not a
canonical entrypoint.

### Evidence authority

A canonical result is the complete repository harness run at the exact source
SHA. A one-off app smoke, manually assembled script, video, or environment
replay can diagnose a failure or provide release presentation evidence, but it
does not replace the complete matrix.

Historical `*-plan.md` and release evidence documents record
what was run at that time. They are not current execution instructions and do
not override this guide.

### Hyprland validation decision (2026-09-07)

[PR #3572](https://github.com/trycua/cua/pull/3572) is the dated, exact-source
result record for the input v3 candidate. It distinguishes ordinary CI, the
complete hosted X11, Sway, and Windows harnesses, native Hyprland acceptance,
and bounded app evidence. A native behavioral pass with a failed wrapper or
source-provenance check is not an accepted run. Use that record for results;
the coverage requirements below do not assert a passing result or add macOS
certification.

The maintainer selects the same canonical Linux Rust harness for native
Hyprland: run `scripts/ci/linux/run-rust-e2e.sh` with its complete `all` suite
in the prepared native Hyprland desktop at the exact candidate SHA. Preserve
the runner's required cells, assertions, and evidence checks. Record compositor
and backend provenance; an X11 run does not establish native Hyprland coverage.
Environment failures and failed cells remain failures, not permission to skip
tests or change expected results. In the same desktop, also run
`cargo test -p pincer-e2e --test hyprland_foreground_test -- --ignored --test-threads=1`
and the same command for `hyprland_native_observation_test`. No automated lane
runs these foreground-safety and exact-identity rows, so
`tests/manual-e2e-allowlist.txt` records them as a manual native-Hyprland gate.

Before launching the native Hyprland harness, apply this map-time rule in the
disposable desktop's Lua configuration and reload it:

```lua
hl.window_rule({
    name = "pincer-canonical-sentinel-animation",
    match = {title = "^PincerTestHarness Sentinel \\[cdp=[0-9]+\\]$"},
    no_anim = true,
})
```

The preflight independently verifies `no_anim` for the exact sentinel, then
waits for mapped fullscreen readiness before its first Driver activation.
`hyprctl clients` reports geometry goals, not animated surface bounds; repeated
equal goals alone cannot establish animation completion. This rule changes
only the test sentinel's animation, not its placement or the product's geometry
guard. Restore the original configuration after the run. It is a deterministic
fixture requirement, not an Omarchy user configuration requirement.

This is fixture regression coverage. For background `TARGET` input, the production compatibility gate in
`platform-linux/src/wayland/hyprland_compatibility.rs` admits only the qualified
native Calc `26.2.5-3` and Inkscape `1.4.4-6` packages. Ordinary GTK, Electron,
and Tauri fixtures do not qualify for v3 background raw input. Their declared refusals
can prove refusal behavior, but cannot prove plugin delivery or isolation.
Do not widen background production admission or add a test bypass to make them qualify.

The [accepted foreground extension](https://github.com/trycua/cua/issues/3550#issuecomment-5564996417)
adds a separate production `FOREGROUND_TARGET` route for ordinary native
top-level surfaces, advertised by `HELLO` with `foreground_target:true`.
It does not apply the Calc/Inkscape background package gate. The existing
complete suite covers defined native GTK3, Electron, and Tauri foreground
cases. Acceptance requires those cases to pass. Preserve the runner and tests.
Foreground activation and primary-cursor movement are intentional, with no
restoration promise. Verify exact-target delivery and refusals for held
keys/buttons, grabs, constraints, and drag-and-drop before primary takeover.
Foreground drag cancellation on primary-input/focus transitions requires
review and native evidence. No background refusal may escalate to this route.

Retain a short real-app production smoke and instrumented isolation proof on
both qualified apps as supporting compatibility evidence. Verify actual app
effects, plugin transport attribution, primary-seat isolation, and cleanup;
the uninstrumented smoke alone cannot establish those trace-based claims.
The retained proof at source `f180e8828b8f31cc153e3c44eaa89a9c13c5bc68`
includes 20 instrumented actions, nine pointer effects, and six observation
intervals, plus an uninstrumented six-action smoke. Saved outputs confirm Calc
cell A1 contains `a` and the Inkscape object's x-coordinate changes from 40 to
42 while y remains 60. The plugin tree and uninstrumented module hash are
unchanged at `1133a06e4f205cf80188a7ac9e41102f37611fea`. This is bounded
supporting evidence, not a complete app matrix or release-byte proof. Raw
background qualification remains native Calc from `libreoffice-fresh 26.2.5-3`,
Inkscape `1.4.4-6`, the plain compiled `evdev`/`pc105`/`us` keymap, and two
seats. Chromium, Electron, and XWayland raw background input remain unqualified;
semantic AT-SPI actions are separate.
See [production proof preparation](../hyprland-plugin/docs/production-proof.md)
for the bounded plans and their limits.

The explicit [Inkscape-only qualification profile](../hyprland-plugin/docs/production-inkscape-profile.md)
supports a bounded packaging candidate using exact Inkscape `1.4.4-6`, with
independent native clients, two app lanes, separate SVG oracles, and third-owner
capacity refusal. It preserves the default Calc/Inkscape profile and the native
all-suite gate. Product, harness, kit, and mapped module identities remain
separate; adding the profile records no new native passing result and does not
let diagnostic trace evidence certify trace-disabled package bytes.

Three complete repetitions of the long Python Calc/Inkscape plan, including
the 34 policy cases across both apps, are no longer a merge requirement.
Extended Python stress runs remain diagnostics for specific unresolved
failures. This decision changes test strategy, records no new passing result,
and does not waive the affected CI, native evidence, or release gates in
RFC 3550 (`rfcs/3550-hyprland-isolated-input.md` in upstream's trycua/cua).

## Repository Map

```text
pincer/
|-- rust/
|   |-- crates/
|   |   |-- pincer/          Rust driver and hermetic integration tests
|   |   |-- pincer-e2e/      Desktop E2E suites the OS runners select
|   |   |-- pincer-core/     Shared driver logic and unit tests
|   |   |-- pincer-testkit/  Shared Rust E2E helpers and evidence capture
|   |   |-- platform-linux/      Linux backend
|   |   |-- platform-macos/      macOS backend
|   |   |-- platform-windows/    Windows backend
|   |   `-- cursor-overlay/      Cursor evidence helper
|   |-- test-apps/               Ignored staged harness binaries
|   `-- Cargo.toml               Rust workspace
|-- tests/
|   |-- fixtures/
|   |   |-- shared/web/           Shared web page and external markers
|   |   |-- apps/                 Repo-local fixture sources
|   |   `-- build/                macOS/Linux/Windows fixture builders
|   `-- runners/
|       `-- macos-lume/           Maintainer Lume image setup and TCC seed helpers
`-- docs/                         Test matrix, reporting, and contributor docs
```

The important separation is:

| Layer                 | Owns                                                                      | Does not own                     |
| --------------------- | ------------------------------------------------------------------------- | -------------------------------- |
| Rust integration test | Scenarios, driver calls, assertions, action metadata                      | OS setup and fixture compilation |
| `pincer-testkit`  | Session helpers, fixture launching, screenshots, recordings, trajectories | The scenario list                |
| Fixture app           | Visible controls and externally observable state markers                  | Driver correctness assertions    |
| OS runner             | Build environment, user session, test selection, artifact collection      | Test behavior definitions        |

There is deliberately no second Python E2E implementation that the Rust suite
has to mirror.

## How One E2E Test Works

Every canonical E2E cell follows this shape:

```text
OS runner
  -> builds the Rust driver and repo-local fixture
  -> starts or connects to a real desktop session
  -> Rust testkit starts one harness application
  -> Rust test discovers the target window
  -> get_window_state provides accessibility tree and screenshot
  -> action addresses a target by AX element index or PX coordinates
  -> delivery is foreground or background where the action supports it
  -> fixture state, focus, pixels, or protocol response is checked
  -> testkit writes video, screenshots, trajectory, logs, and result data
```

A tool returning `ok` is not enough to pass an E2E cell. The fixture must show
that the action happened, or the test must verify a documented structured
refusal and the absence of focus or input side effects.

## The Test Layers

### Unit and Protocol Tests

These run without a repo-local GUI application and normally run without
`--ignored`:

| Location or prefix                     | What it proves                                                |
| -------------------------------------- | ------------------------------------------------------------- |
| `rust/crates/*/src/**`                 | Core driver, platform-independent logic, schemas, and helpers |
| `protocol_*_test.rs`                   | MCP handshake, tool calls, sessions, media, and errors        |
| `schema_*_test.rs`                     | Shared schema and backend consistency                         |
| `transport_config_persistence_test.rs` | CLI configuration persistence across processes                |
| `protocol_element_token_test.rs`       | Element-token protocol behavior                               |
| `tool_boundary_fuzz_smoke.rs`          | Tool-call boundary fuzz targets over seeds and random bytes   |

These tests should be fast, deterministic, and safe to run on ordinary CI
workers. They do not prove that a real click, key, scroll, or background input
reached an application.

The fuzz smoke test drives the target bodies in
`pincer_testkit::boundary_fuzz` (JSON-RPC parsing, reserved-argument
stripping, argument normalisation, authorization, typed contract inputs, and
`ToolRegistry` dispatch against stubs). The same bodies run under libFuzzer
from `rust/fuzz/`; see `rust/fuzz/README.md` and
`docs/2026-09-04-tool-call-boundary-fuzzing-design.md`.

### Harness E2E Tests

These are Rust integration tests in their own crate:

```text
rust/crates/pincer-e2e/tests/
```

Hermetic protocol, CLI, and schema tests stay in
`rust/crates/pincer/tests/`. A test that needs a desktop,
built fixtures, installed apps, or platform permissions belongs in
`pincer-e2e`. Most E2E tests are marked `#[ignore]`; the OS runner selects
them with `cargo test -p pincer-e2e --test <suite> -- --ignored`. The
crate does not depend on the driver, so build `pincer` first or set
`PINCER_TEST_DRIVER_BIN`. Its few non-ignored tests are hermetic oracle and
parser checks that ordinary CI runs with the driver's tests.

The canonical E2E suite has two behavior owners:

| Owner          | Purpose                                              |
| -------------- | ---------------------------------------------------- |
| Shared app     | Same web behavior tested through Electron and Tauri  |
| Native harness | Toolkit-specific controls and native window behavior |

WebView, CDP, and page-tool integration stays inside the shared or native
owner that exercises it. It is not a third public test family or command.

Delivery is not a test family. It is a dimension on each action row: an action
is tested in foreground and background modes whenever the driver and OS support
both. Capture and desktop scope are separate environment checks, while focus
preservation is a cross-cutting oracle that can be attached to any action row.
Focus, z-order, cursor, and desktop-state checks are cross-cutting invariants,
not a separate family. These names describe responsibilities, not separate
sources of truth or commands to run instead of the selector-free canonical
invocation.

## What The Complete Run Includes

### Windows

Upstream runner: `scripts/ci/windows/run-rust-e2e.ps1`

| Runner area       | Rust test                         | Real harness or app                            |
| ----------------- | --------------------------------- | ---------------------------------------------- |
| Shared app matrix | `cross_platform_behavior_test.rs` | Electron and Tauri                             |
| Native controls   | `harness_wpf_test.rs`             | Repo-local WPF app                             |
| Native controls   | `harness_winui3_test.rs`          | Repo-local WinUI3 app                          |
| Web integration   | `harness_web_test.rs`             | WebView2                                       |
| Capture contract  | `capture_contract_test.rs`        | WPF plus driver tree/image output              |
| Launch contract   | `launch_windows_test.rs`          | Repo-local Electron launch and focus behavior  |
| Agent cursor      | `agent_cursor_windows_test.rs`    | Source-built cursor overlay and pixel evidence |
| Desktop scope     | `desktop_scope_windows_test.rs`   | Windowless desktop input and scope rejection   |

Cross-cutting instrumentation used by these rows includes the testkit
`DesktopObserver`, capture validation, cursor evidence, and desktop-scope
checks. These invariants are attached to their owning action rows rather than
run as a separate test family.

Windows is currently the broadest native matrix. It covers UIA controls, web
integration, background focus checks, and Windows-specific input routes. The
desktop observer is attached to shared and native action rows wherever
background delivery is tested.

### macOS

Upstream runner: `scripts/ci/macos/run-rust-e2e.sh`, which upstream ran in a
logged-in Lume worker through a `run-all.sh` wrapper. This repository keeps
only the Lume image setup and TCC seed helpers in `tests/runners/macos-lume/`.

| Runner area               | Rust test                              | Real harness or app                     |
| ------------------------- | -------------------------------------- | --------------------------------------- |
| Shared app matrix         | `cross_platform_behavior_test.rs`      | Electron and Tauri                      |
| Native web matrix         | `cross_platform_behavior_test.rs`      | Repo-local WKWebView host               |
| Native controls           | `harness_appkit_test.rs`               | Repo-local AppKit app                   |
| Native controls           | `harness_swiftui_test.rs`              | Repo-local SwiftUI app                  |
| Installed app launch      | `installed_app_launch_macos_test.rs`   | Calculator and TextEdit                 |
| Installed app AX delivery | `installed_app_textedit_macos_test.rs` | TextEdit                                |
| Exact window activation   | `bring_to_front_macos_test.rs`         | Repo-local AppKit and SwiftUI apps      |
| Pointer-reading toolkit   | `tk_pointer_click_macos_test.rs`       | Tk visual-only canvas (skips sans Tk)   |
| Capture contract          | `capture_contract_test.rs`             | Installed driver and macOS capture APIs |
| Desktop scope             | `desktop_scope_macos_test.rs`          | macOS window and desktop scope          |
| Standalone browsers       | `standalone_browser_behavior_test.rs`  | Installed Google Chrome and Edge        |

The WKWebView host runs the same typed shared-web catalog as Electron and
Tauri. Calculator and TextEdit add typed supporting rows for built-in app
launch focus and native Cocoa background value delivery. They run in the
canonical logged-in macOS lane, but they do not replace repo-local fixtures.

Workers cloned from a granted private seed verify and reuse its app-owned TCC
identity. A disposable SIP-disabled Lume worker without inherited grants must
use `tests/runners/macos-lume/seed-tcc.sh` after installing the exact candidate
as a certificate-signed `PincerLocal.app`. The guarded helper is the only
supported automated path: it verifies `VirtualMac*`, disabled SIP, the expected
bundle identity, and the signed requirement before seeding only Accessibility
and Screen Recording. Never hand-edit `TCC.db`. Restart the app afterward and
prove live capture and input; database rows alone do not certify the desktop.

### Linux

Upstream runner: `scripts/ci/linux/run-rust-e2e.sh`

| Runner area       | Rust test                         | Real harness or app       |
| ----------------- | --------------------------------- | ------------------------- |
| Shared app matrix | `cross_platform_behavior_test.rs` | Electron and Tauri        |
| Native controls   | `harness_gtk3_test.rs`            | Repo-local GTK3 app       |
| Capture contract  | `capture_contract_test.rs`        | Linux capture backend     |
| Desktop scope     | `desktop_scope_linux_test.rs`     | X11/Wayland desktop scope |

Linux has separate X11 and Wayland concerns. Nix supplies the reproducible
build and desktop environment, but the E2E test still needs an actual X11 or
Wayland session. Linux does not need GIF output; MP4, screenshots, accessibility
trees, trajectories, and logs are the useful evidence.

Wayland results are compositor-specific. The hosted lane uses Sway to prove
wlroots protocols. GNOME requires the optional WinRects Shell helper for
authoritative frame and buffer geometry, observation, capture, and verified
target activation. A portal/libei grant persists until the user revokes it, so
subsequent driver processes do not reopen the consent dialog.
KDE requires a future target-addressable KWin adapter; portal availability by
itself is not evidence that input can be sent safely to a named window.
Standard Wayland does not expose the physical pointer position, so canonical
Wayland rows do not claim the real-cursor preservation oracle. Focus, full
occlusion, sentinel input isolation, liveness, and fixture-state oracles remain
mandatory. [Issue #2194](https://github.com/trycua/cua/issues/2194) tracks
compositor, portal/libei, sentinel, and capture-based ways to add a proven
cursor observer where the environment supports one.

## AX, PX, and Delivery

See [`action-support.md`](action-support.md) for the current Windows, macOS, and Linux
delivery, refusal, and unproven-action ledger.

These terms describe different dimensions:

| Term          | Meaning                                                                     |
| ------------- | --------------------------------------------------------------------------- |
| AX            | Address a target through its accessibility/UI automation element            |
| PX            | Address a target by screen coordinates or pointer geometry                  |
| Foreground    | The target may be brought to the foreground for delivery                    |
| Background    | The target should receive the action without being raised or stealing focus |
| Window scope  | Capture or action is limited to one target window                           |
| Desktop scope | Capture or action covers the full desktop                                   |

The shared and native action matrices should test left click, right click,
double click, typing, keys, hotkeys, scroll, child windows, and drag across
AX/PX and foreground/background combinations where the driver supports them.
Unsupported background routes require an explicit refusal contract with an
allowed structured code and desktop-side-effect oracles. A desktop-scope
gesture that the platform's pointer route cannot carry uses the same contract
with the fixture under the gesture as its oracle; today that is only a modified
drag on native Wayland (`modified_pointer_unavailable`). A refusal fails a
cell that requires delivery. There should not be a separate "delivery" family
whose only purpose is to repeat those same actions in the background.

Native harness rows use the same typed case/result contract as the shared
matrix. Current native `set_value` rows declare background delivery because
their contract includes no-focus and no-raise observations; actions without a
delivery concept use `not_applicable` explicitly.

## Cross-Cutting Invariants

The desktop observer is cross-cutting test instrumentation. It answers the same
question for any action, harness, or catalog area:

> Did the driver perform or reject the operation without disturbing the user's
> foreground application or desktop?

`pincer-testkit::DesktopObserver` owns the shared interface. Native Windows,
macOS, and Linux backends snapshot foreground-window, target z-order, cursor,
and focus state before and after an action. A separate full-desktop Electron
sentinel journals keyboard, pointer, wheel, visibility, focus, and heartbeat
events while it fully covers the target. Background rows opt into both pieces
of instrumentation directly; there is no special guard suite.

| Invariant or scenario     | What it checks                                                               |
| ------------------------- | ---------------------------------------------------------------------------- |
| Background click/type/key | The target action does not move focus away from the user's foreground window |
| Minimized app launch      | `launch_app(start_minimized=true)` does not raise the new app                |
| Background hotkey         | A keyboard chord does not steal focus                                        |
| Child-window click        | A target-created window does not unexpectedly become foreground              |
| Background screenshot     | Reading the target does not change focus or z-order                          |
| Agent cursor visibility   | The cursor appears in the captured pixels when enabled and moved             |

The sentinel contract fails closed when the target is only partly covered or
the heartbeat stops. Before any behavioral cells run, the strict environment
preflight deliberately sends input to the sentinel and deliberately raises the
background target. The lane proceeds only if the leaked input and transient
focus loss are observed, the sentinel is restored, and it once again fully
occludes the target. Windows, macOS, and X11 require the sentinel's live focus
journal to report the loss. Wayland uses the compositor-backed native focus
observer because Electron/Ozone does not reliably emit a DOM `blur` event for
an external surface focus transition. The sentinel heartbeat and leaked-input
journal remain mandatory on Wayland. This positive control prevents a broken
guard from making every background row look green.

A focus assertion can prove "no focus steal" while failing to prove that a
click changed the target application state. An action row must therefore check
both the target's external state and, when background delivery is under test,
the cross-cutting desktop observer.

These tests require a real interactive Windows user desktop. They reject
Session 0, locked desktops, and disconnected RDP sessions. Without
`PINCER_REQUIRE_GUI=1`, an unusable desktop can self-skip for local development;
the canonical Windows runner enables the hard-failure behavior.

## Evidence

Canonical GUI runs are expected to produce evidence per test cell:

```text
artifacts/pincer/<os>/
|-- recordings/<cell-label>-pid<pid>-<sequence>/recording.mp4
|-- recordings/<cell-label>-pid<pid>-<sequence>/trajectory.json
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/before_state.json
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/before.png
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/after_state.json
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/after.png
|-- cases.jsonl
|-- environment.jsonl
|-- results.jsonl
|-- summary.md
`-- <rust-target>.log
```

The GitHub Actions summary contains one row per meaningful behavioral cell,
including its OS, harness, action, AX/PX targeting, delivery mode, driver route,
expected and observed behavior, oracles, and one evidence link. The link uses
the exact video path as its label and opens the owning lane archive. Unit tests
need normal test output and logs; they do not need desktop video.

## What Is Implemented Today

- Rust owns the canonical scenario definitions and external-state assertions.
- Electron and Tauri use the same shared web fixture across supported OSs.
- Native Windows, macOS, and Linux harnesses are repo-local applications built
  from source.
- The three OS runners use a selector-free command for the complete matrix.
- Shared and native harness owners emit the same typed v2 result records.
- Canonical GUI rows collect a trajectory and MP4, validated before reporting.
- Canonical GUI rows require parseable pre/post state and non-empty pre/post
  target-window images for each targeted turn. The one narrow exception is a
  successful Windows `bring_to_front` restore whose minimized target has no
  pre-action image or whose host capture remains unavailable afterward: it must
  retain the successful action response, captured post-action accessibility
  state, an explicit capture classification, trajectory, and MP4. Any other
  missing expected evidence fails the report.
- Per-cell video starts after fixture readiness and foreground/background
  posture. A 300 ms baseline precedes dispatch, and capture continues through
  external oracle collection. `trajectory.json` must finish with
  `behavior_video.status = "finalized"`.
- Use `agent_cursor_showcase_test` for cursor review media. Shared behavior
  matrix daemons deliberately use `--no-overlay` so synthetic cursor pixels do
  not contaminate action oracles; their videos prove tool behavior, not cursor
  rendering.
- Windows hosted runs use `GetConsoleWindow` to select the inherited
  HostedComputeAgent/runner console, verify its identity, and minimize it
  through `ShowWindow(SW_MINIMIZE)` before fixture or sentinel posture is
  established. The sentinel remains a separate test fixture and is reasserted
  after console cleanup.
- Strict lane preflights fail on missing fixtures, desktop access, permissions,
  accessibility, capture, recording support, or ineffective background guards
  instead of silently skipping.
- Canonical runners set `PINCER_E2E_FORBID_SKIPS=1`. Unfiltered shared runs also
  set `PINCER_E2E_EXPECTED_MIN_CELLS` to 80 on Windows/Linux and 120 on macOS, so
  a filtered, shortened, or accidentally emptied catalog cannot report green.
  Explicit diagnostic cell or harness filters disable only the minimum-count
  check; matching no cells still fails inside the Rust matrix.
- GitHub summaries link every evidence-bearing row to its lane archive and
  display the exact recording path. The trajectory path remains in the typed
  evidence and archive.
- Unit/protocol tests remain separate from interactive E2E tests.

## What Still Needs Implementation

The remaining work is platform coverage and validation, not another test
hierarchy:

1. **Broaden native action rows.** The shared web matrix covers every declared
   AX/PX and foreground/background cell. AppKit, SwiftUI, WPF, WinUI3,
   WebView2, and non-GTK3 Linux toolkits still have unproven native combinations listed in
   [`action-support.md`](action-support.md).
2. **Preserve exact-source validation.** Every accepted platform run must record
   one immutable source SHA and retain the typed evidence contract.
3. **Close representative-desktop gaps.** Hosted Sway passes the complete
   Electron, Tauri, GTK3, capture, and desktop-scope catalogs. A real GNOME 46
   session passes GTK3, capture, and desktop scope, but still needs the shared
   renderer catalog and portal-video parity. Plasma 6 still needs a verified
   KWin activation adapter and its first accepted behavioral lane. Issue `#1922`
   tracks the grouped backend work.
4. **Add representative toolkit surfaces.** GTK4, Qt5/Qt6, VTE, VCL, and GL
   canvases remain optional real-app gaps; shared Electron/Tauri coverage does
   not substitute for those native stacks.
5. **Flake cleanup.** Replace remaining fixed native waits with external-state
   polling and add fixture reset tokens before reusing a harness process.

## File Convergence Plan

The goal is not to put every assertion into one enormous test file. The goal is
to give each behavior one clear owner and make cross-cutting evidence reusable.

### Target Ownership

```text
rust/crates/pincer-testkit/src/
`-- observer.rs                     Cross-OS desktop-side-effect interface

rust/crates/pincer-e2e/tests/
|-- cross_platform_behavior_test.rs Shared Electron/Tauri action matrix
|-- harness_wpf_test.rs             Windows WPF action rows
|-- harness_winui3_test.rs          Windows WinUI3 action rows
|-- harness_web_test.rs             WebView2 PX action row
|-- harness_appkit_test.rs          macOS AppKit action rows
|-- harness_swiftui_test.rs         macOS SwiftUI action rows
|-- harness_gtk3_test.rs            Linux GTK3 action rows
|-- capture_contract_test.rs        Tree and screenshot read contract
`-- desktop_scope_<os>_test.rs      Window/desktop scope invariants

rust/crates/pincer/tests/
`-- protocol_*_test.rs              Protocol and schema tests
```

The desktop observer is a helper, not a test family. An action row invokes it when
the row is testing background delivery. The row then records both outcomes:

1. Did the target application state change, or did the driver return the
   documented structured refusal?
2. Did focus, z-order, cursor, and desktop state remain within the contract?

Focus, z-order, cursor, and input-leak assertions belong to the typed action
rows that exercise them; launch, capture, cursor, and desktop-scope contracts
retain their narrowly owned scenarios. The canonical runner is the only
user-facing command; lane selectors are internal diagnostics.

## Contributor Workflow

When adding a new scenario:

1. Add or update the repo-local fixture and its external state marker.
2. Add the Rust scenario under `rust/crates/pincer-e2e/tests/`.
3. Declare AX/PX addressing, foreground/background delivery, scope, and oracle.
4. Add the scenario to `docs/test-matrix.md` and this guide when it changes the
   cross-OS structure.
5. Update only the OS runner selection when the test is platform-specific.
   List every `#[ignore]` test that no runner selects, with a reason, in
   `tests/manual-e2e-allowlist.txt`.
6. Run the smallest Rust test locally, then run the OS command before
   calling the matrix complete.

The goal is one understandable Rust E2E model across platforms, with
platform-specific harnesses where the OS genuinely differs.

## Test Layout Conventions

Rust tests in the driver workspace use one layout:

| Test kind | Where it lives |
| --- | --- |
| Small unit suite | Inline `#[cfg(test)] mod tests { ... }` at the end of the module |
| Large unit suite | `src/<module>/tests.rs`, declared as `#[cfg(test)] mod tests;` in `src/<module>.rs` |
| Several suites for one large module | `src/<module>/<name>_tests.rs`, one `#[cfg(test)] mod <name>_tests;` each, for example `platform-{linux,windows}/src/tools/impl_/` |
| Crate-root suites | `src/tests/`, for example `pincer-sdk/src/tests/` |
| Integration test in `pincer` or its E2E suites | `tests/<subject>_test.rs`; shared helpers go in `tests/support/` |

Moving a suite into its own file keeps its module path, so test names and
CI filters stay the same. Prefer Rust's default module file lookup over
`#[path]`. Other crates' `tests/` directories keep their existing names.

Scripts and their tests live next to their owner:

- Upstream keeps its desktop runners and focused native gates in its
  monorepo-level `scripts/ci/<os>/`, which is not part of this repository.
- Example code keeps its tests in a `tests/` directory beside it, for example
  `examples/agent-sdks/tests/`.
- `rust/test-apps/` is the staging directory for built
  harness apps, not a test directory. The fixture build scripts `cd` into it,
  so its tracked README and `.gitignore` must stay.
