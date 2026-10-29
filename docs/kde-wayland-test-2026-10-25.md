# Pincer on KDE Plasma 6.8.1 Wayland, 2026-10-25

Installed binary: 0.35.4, git `f30d522` (two commits behind `d90ebe3`; those only change cursor
art and the logo). Tested as configured in Claude Code: `unshare -cn pincer mcp`, no
`CUA_DRIVER_RS_ENABLE_WAYLAND`, so XWayland mode. Native mode was checked separately with a
temporary `pincer serve` on its own socket. Targets were throwaway Kate windows on scratch
files, one forced onto XWayland (`QT_QPA_PLATFORM=xcb`), one native.

## Bugs

### 1. Background input crashes every Qt app on XWayland (severe)

The first background `click` created a uinput pointer (kernel: `CUA v1... uinput pointer`,
10:47:27) and an XInput2 master device, waited for the slave to appear in XWayland (it never
does; KWin owns real input devices), and removed them again. Five seconds later four Qt
XWayland clients segfaulted in `QXcbConnection::xi2SetupDevices`: the test Kate,
`xembedsniproxy`, `gmenudbusmenuproxy` and `kaccess` (systemd restarted the last three).

Upstream already knows this crash on KDE X11 and disables the route there
(`crates/platform-linux/src/input/mod.rs:519-524`, `kde_x11_uinput_hotplug_is_unsafe`). But
the guard returns "safe" for any Wayland session (`mod.rs:536`), assuming the native backend
takes over. In XWayland mode it doesn't. Fix: treat a Wayland session as unsafe for the X11
MPX/uinput route too. On XWayland that route can never work anyway, since uinput devices
never reach XWayland.

Affected tools in XWayland mode: every background pointer or keyboard action (`click`,
`double_click`, `right_click`, `drag`, `scroll`, `press_key`, `hotkey`, `type_text` without
an element token, `mouse_button_down`/`up`, `parallel_mouse_drag`). I ran only the first and
inferred the rest from the shared route.

### 2. Phantom popup on every XWayland window

`get_window_state` reports a popup "open" for every XWayland app: window `0x200001`,
2560x1440, no pid, no title, and sets `covers_window`. It's KWin's full-screen **InputOnly**
window (`xwininfo`: `Class: InputOnly`, depth 0). Following the `follow_up` hint fails with
"no X11 pixmap format for depth 0". `popup_info` (`mod.rs:1799`) checks override-redirect,
mapped and size, never the window class; `impl_.rs:1033-1046` then keeps pid-less popups.
Fix: skip `InputOnly` windows in `popup_info`. They have no pixels and can't be menus.

### 3. `invoke_menu` can't open Qt menus (X11 too)

`invoke_menu(["Edit","Select All"])` on Kate: "menu path segment 0 has no safe activation
action". Qt's menu-bar items expose one AT-SPI action, `Show Menu`, which isn't in
`ACTIVATION_VERBS` (`crates/platform-linux/src/atspi/native.rs:3154`). Fix: accept
`show menu` for menu and menu-item roles only, the way `CHECKBOX_ACTIVATION_VERBS` is gated.

### 4. Whole-screen capture on rootless XWayland

- `get_desktop_state` fails with a raw `BadMatch` on `GetImage` of the root window. XWayland
  runs rootless, so the root has no contents.
- `start_recording` with `record_video: true` writes a black video (19.5 s, every sampled
  frame level 0) and reports `last_error: null`. ffmpeg's `x11grab` hits the same root.

Fix: report both as unsupported in XWayland mode (or capture through the portal), instead of
an X11 error and a silent black file.

### 5. Multi-line values break `query`

`tree_markdown` prints element values with raw newlines. With `query`, the line-based
projection then emits a stray fragment of the value as a row (`li" [actions=[]]`, the tail
of Kate's document text). Fix: escape newlines in values when rendering markdown.

### 6. No agent cursor at all in XWayland mode; cursor setters silently do nothing

`should_start_x11_overlay` (`crates/platform-linux/src/overlay.rs:133`) refuses whenever
`WAYLAND_DISPLAY` is set, and the layer-shell overlay only runs in native mode. So in
XWayland mode no overlay window exists (no `Cua.AgentCursorOverlay.*` on the X server). The
file's header comment says the opposite ("when WAYLAND_DISPLAY is set but DISPLAY is also
available (XWayland), the X11 path is used"). `set_agent_cursor_motion`,
`set_agent_cursor_theme` and `set_agent_cursor_enabled` still return success with the new
values, and `get_agent_cursor_state` keeps reporting defaults (`signature_arc`, `steady`,
reduced motion `auto`, enabled), because it reads the render thread's per-session state and
falls back to defaults when there is none. Fix: either start the X11 overlay on XWayland as
the comment says, or have the setters report that no cursor renderer is running.

### 7. Browser tools can't launch under the `unshare -cn` wrapper

`browser_prepare` with `allow_launch` and an isolated profile refused: "no root-owned,
non-writable system Chromium executable". `/usr/lib/chromium/chromium` is on the candidate
list and is root-owned 0755, but inside the user namespace that `unshare -c` creates, real
root isn't mapped, so the file shows owner 65534 (`nobody`), and
`trusted_root_owned_installation` requires `uid() == 0` (`browser_platform.rs:155`). Every
other `browser_*` tool needs an attached browser, so none of them can run in this setup
short of attaching to the user's own profile. Fix: when uid 0 isn't mapped in
`/proc/self/uid_map`, accept the overflow uid as "owned outside this namespace", keeping the
not-writable and no-symlink checks.

### Smaller issues

- `right_click` in foreground mode opened Kate's context menu but didn't name the popup in
  its result, as the tool description says it should.
- `zoom` said a window "has not been read yet" right after a screenshot read; a later
  tree-only read seems to replace the screenshot context. The message is misleading.
- `set_window_frame` on a maximized window: honest `suspected_noop`, but it could
  unmaximize first (KWin ignores move/resize on maximized windows).
- `get_accessibility_tree` lists every process, kernel threads included (400+ rows for 3
  windows).
- The default 1000 ms AT-SPI budget truncates Kate on later reads (27 of 62 elements);
  15000 ms completes. A full walk visits about 870 nodes, about 220 a second here.
- CLI only: `kill_app` refused to kill a process the same daemon launched in an earlier
  `call` without a session label ("foreign_process_termination_denied").

## What works

| Area | Tools | Result |
| --- | --- | --- |
| Diagnostics | `health_report`, `check_permissions`, `get_screen_size`, `get_config` | pass |
| Discovery | `launch_app`, `list_windows`, `get_window_state` (XWayland), `get_accessibility_tree` | pass (with bug 2) |
| AT-SPI actions | `set_value`, `type_text` with element token, `run_steps` with `expect`, `verify_state`, element `click` | pass, read-back confirmed; element `click` on Kate's Output toggle reached Kate but changed nothing visible |
| Foreground input | `click`, `double_click`, `right_click`, `drag`, `scroll`, `press_key`, `hotkey`, `type_text`, `move_along_path` | pass, after you allowed KWin's input prompt (`kwin_eis_prompter`) for XWayland; route `x11_xtest_fg`, no uinput |
| Clipboard | `clipboard_read`, `clipboard_write` | pass; Klipper on the Wayland side sees the written text |
| Capture | `zoom`, `capture_series`, turn recording | pass (video: bug 4) |
| Cursor and sessions | `move_cursor`, `get_cursor_position`, `get_agent_cursor_state`, `get_session`, `list_sessions` | pass; `get_cursor_position` is X11's view and can be stale over native windows |
| Cleanup | `kill_app` | pass over MCP |

Before your Allow, foreground input failed: X11 `_NET_ACTIVE_WINDOW` activation timed out
when a native window had focus, and XTEST waited on KWin's prompt.

## Native Wayland mode (`CUA_DRIVER_RS_ENABLE_WAYLAND=1`)

- `list_windows` sees native Wayland windows (Dolphin, pid 1040), with `z_index: null`.
- AT-SPI works on a native Kate: tree walk and `set_value` with read-back.
- Raw input: none. `health_report`: "this build was compiled without libei/portal support".
  A raw `click` is refused cleanly (`background_unavailable`). Needs a build with
  `--features portal-input`; even then, upstream refuses target-addressed KWin raw input
  (`kwin-target-helper/README.md`).
- Screen capture goes through the desktop portal, with a consent prompt per session (not
  run).

## Second round (same day)

| Tool | Result |
| --- | --- |
| `set_config` | pass: `max_image_dimension` and `cursor.badges` changed, `get_config` agreed, restored |
| `install_ffmpeg` | pass: reports ffmpeg already installed and does nothing |
| `start_session`, `end_session` | pass |
| `set_agent_cursor_motion`, `set_agent_cursor_theme`, `set_agent_cursor_enabled` | report success, take no effect (bug 6); an unknown theme id is refused cleanly |
| `replay_trajectory` | pass: a recorded foreground `type_text` replayed and the text appeared twice |
| `run_script` | pass: `cua.getApp` plus `app.query` read the document text through AT-SPI |
| `browser_prepare` | refused in this setup (bug 7); the other `browser_*` tools depend on it |
| `launch_app` | apps it starts stay in Pincer's network namespace (checked with `/proc/<pid>/ns/net`); its "handed off for D-Bus activation" message for a windowless command is misleading |

## Third round, in the sandbox (same day)

The remaining tools ran inside the rehearsal sandbox (`~/plasma-x11-baseline/sonicde/rehearsal`,
scripts in `tools/ptest/`): its own headless KWin with XWayland, a Pincer daemon in XWayland
mode, no network, `/dev/uinput` blocked so nothing could reach the host's input devices, and a
local test page on loopback. Inside that user namespace the sandbox's files are owned by the
namespace's root, so bug 7's trust check passes there.

| Tool | Result |
| --- | --- |
| `mouse_button_down`, `mouse_drag`, `mouse_button_up` | ran and reported success through the global-input route (uinput blocked); the XWayland Kate survived; effect unverified |
| `parallel_mouse_drag` | clean refusal, `uinput_unavailable` |
| `browser_prepare` | launched an isolated Chromium (bug 7 confirmed as the only blocker on the desktop) |
| `get_browser_state` | bind (`binding_quality: exact`), `dom_refs_v1` and `semantic_v2` snapshots, screenshot: pass |
| `browser_navigate` | pass |
| `browser_type` | pass; the field read "pincer browser test" |
| `browser_set_input_files` | pass; the page showed "1 file(s)" |
| `browser_pointer` | default background route refuses on Linux by design; `hover` with `input_route: "dom_event"` and `scroll` with `delivery_mode: "foreground"` pass |
| `browser_click` | default background route refuses by design; a stale ref is refused clearly; see bug 8 for a click that opens a dialog |
| `browser_dialog` | inspect, then dismiss: pass (page read "dismissed"), but only if inspect ran before the dialog opened (bug 8) |
| `browser_download` | pass over MCP: a minimal stdio MCP client (`tools/ptest/drive_mcp.py`) drove `pincer mcp`, and the download completed (21 bytes, saved under its download id, content intact). Over the CLI it always refuses, even with `--dangerously-bypass-approvals`: only the MCP server stamps `_cua_browser_download_mcp_host_approved` (`server.rs`, `download.rs:296`) |

Notes for running browsers there: Pincer in XWayland mode can only bind an X11 browser window,
so Chromium needs `XDG_SESSION_TYPE=x11` and no `WAYLAND_DISPLAY`; with
`XDG_SESSION_TYPE=wayland` it picks Wayland even without `WAYLAND_DISPLAY`.

### 8. A browser click that opens a JavaScript dialog hangs and reports failure

Clicking a button whose handler calls `confirm()` blocked for 20 s and returned an error
(`Input.dispatchMouseEvent` timed out on the trusted route, `Runtime.callFunctionOn` on the
DOM route), although the click worked and the dialog was open. If `browser_dialog` hadn't
been called before the click, every later CDP call on the tab timed out too (`Page.enable`,
`Page.getFrameTree`, `DOM.getDocument`), so the tab was stuck. With an earlier inspect, the
dialog showed up as `dialog-1` and dismissing it worked. Fix: enable the Page domain's dialog
events when binding a tab, and have click return as soon as `Page.javascriptDialogOpening`
fires instead of waiting for the dispatch to finish.

## Applied fixes (same day)

Fixed on top of `d90ebe3`, in the commit that adds this section. How each fix was checked:
unit tests; the sandbox, comparing the old build (`pincer.before-fixes`, git `f30d522`) with the
new one; and, for fixes that need no pointer, keyboard or browser action, a temporary daemon on
the desktop under `unshare -cn` against a throwaway XWayland Kate on a scratch file. Nothing was
installed to `~/.local/bin/pincer`.

### 1. Background input on XWayland

`kde_x11_uinput_hotplug_is_unsafe` became `mpx_hotplug_block` (`input/mod.rs`). It now blocks
the MPX/uinput route on XWayland as well as on KDE Xorg. XWayland is detected two ways: the
session env (`XDG_SESSION_TYPE=wayland`, or `WAYLAND_DISPLAY` without an explicit `x11`), or the
X server advertising the `XWAYLAND` extension (`x11::server_is_xwayland`, probed once). The
existing lock-file check can't tell, because KWin writes its own wrapper's pid into
`/tmp/.X0-lock`. Both `real_pointer_input_available` and the choke point
`ensure_master_pointer_for_session` refuse, before any uinput device or XI2 master exists. A
refused background keystroke now gives this as the reason, instead of advising access to
`/dev/uinput`.

- Tests: `kde_x11_sessions_disable_uinput_pointer_hotplug` (rewritten),
  `xwayland_sessions_disable_uinput_pointer_hotplug`.
- Sandbox: `/dev/uinput` was covered by a writable plain file, so the real device stayed out of
  reach but an attempt to use it would show. The old build's background `click` and `press_key`
  went for uinput and failed at the ioctl ("Inappropriate ioctl for device"). The new build never
  opened it, both in the XWayland-mode daemon and in one started with `XDG_SESSION_TYPE=x11` and
  no `WAYLAND_DISPLAY`. No XI2 device appeared and Kate survived.
- Not reproduced: the crash itself, which needs the real device. Not run on the desktop.
- What callers see now: on XWayland a background key goes the synthetic route, and a background
  pixel click on a Qt window is refused with `delivery_mode: "foreground"` as the escalation
  (Qt drops synthetic pointer events).

### 2. Phantom popup

`popup_info` checks the new `could_be_popup`, which rejects `InputOnly` windows. Test:
`input_only_windows_are_never_popups`. In the sandbox (1600x1000) and on the desktop
(2560x1440), the old build listed `0x200001` as an open popup and the new build lists none.
The AT-SPI walk's open-menu probe also reads `mapped_popup_windows`, so the phantom used to make
it treat every menu with on-screen entries as open; I haven't checked how `get_window_state`
output changed for menus.

### 3. `invoke_menu` on Qt menus

Two causes, plus a wrong test path. Kate keeps "Select All" under **Selection**, not Edit, so the
report's `["Edit", "Select All"]` can't resolve on Kate with any build.

- `Show Menu` is now an activation verb for the `menu` and `menu item` roles only
  (`MENU_ACTIVATION_VERBS` in `atspi/native.rs`).
- Qt puts a menu-bar item's entries under a `popup menu` container, and names that container
  after the window title once it's open ("Kate"; read with AT-SPI from Python). The path match
  then read `Selection > Kate > Select All`. `popup menu` containers are now transparent in the
  lineage (`counts_in_menu_lineage`).
- Tests: `qt_show_menu_is_role_gated_to_menus`, `qt_popup_menu_containers_are_transparent_in_menu_paths`.
- Desktop: the old build answered "menu path segment 0 has no safe activation action". The new
  build resolved `["Selection", "Select All"]`, and an AT-SPI read of the document showed
  selection (0, 35) of 35 characters.
- Left over: Qt keeps the Selection popup open after an item fires through AT-SPI, so it stays
  on screen until something closes it.

### 4. Whole-screen capture on rootless XWayland

`x11::rootless_xwayland()` is true when the server is XWayland and a one-pixel `GetImage` of the
root fails. On this desktop it fails with error 8 (BadMatch) for one pixel and for the whole
screen, checked with Xlib. `get_desktop_state` (`screenshot_display_bytes_x11`) refuses with a
message that points at `get_window_state` or at `CUA_DRIVER_RS_ENABLE_WAYLAND=1`. A new
`X11VideoBackendFactory` (`recording_hooks.rs`, selected in `cua-driver-sdk/src/runtime.rs`)
refuses before starting ffmpeg, so `start_recording` keeps recording turns and reports the reason
in `last_error` with `video_active: false`. A rootful XWayland passes the probe and keeps both
paths.

- Desktop: `get_desktop_state` gave the raw `BadMatch` with the old build, the message with the new one.
- Sandbox: `start_recording` with `record_video: true` set `last_error` to the same message.
- No unit test: the probe needs an X server.

### 5. Multi-line values and `query`

Names, descriptions and values are written into markdown rows with `\n` and `\r` escaped
(`one_line`); the structured nodes keep the raw text. Test:
`multi_line_text_stays_on_one_markdown_row`. Desktop, `query: "tail"` on a three-line file: the
old build's only row was the stray fragment `gamma tail li`; the new build returned the text row
with `value="alpha line\nbeta line\ngamma tail li\n"` and its ancestors.

### 6. Agent cursor under `WAYLAND_DISPLAY`

I picked the second option: the setters and `get_agent_cursor_state` now say that nothing draws.
Starting the X11 overlay on XWayland can't be verified safely. Upstream turned it off there on
purpose (the comment in `run_on_thread` says it put an opaque black window over the Wayland
desktop). Proving otherwise needs two things checked on a real KWin session: that KWin blends
the ARGB window (it does own `_NET_WM_CM_S0` on this desktop, so it may), and that it honors the
overlay's XShape input region. If it doesn't honor that region, a screen-sized window takes
every click, and this desktop is reached only through RustDesk.

- `overlay::active_renderer()` reports `x11_overlay`, `wayland_layer_shell`, `gnome_shell_helper`
  or none. All four cursor tools add a `renderer` field; with none, the text adds "Nothing
  changes on screen: no agent cursor is drawn in XWayland mode ...". The output schemas allow
  extra properties, so the contract is unchanged. The header comment now matches the code.
- Test: `xwayland_mode_has_no_cursor_renderer` (since 2026-10-29
  `cursor_renderer_follows_the_x11_overlay_gate`). Sandbox: the XWayland-mode daemon reported
  `renderer: null`; the daemon started without `WAYLAND_DISPLAY` reported `x11_overlay`.
- This means the agent cursor turned on 2026-10-20 doesn't show on KDE Wayland in the configured
  mode.

### 7. `browser_prepare` under `unshare -cn`

There were two blockers; the second isn't in the report above.

- Owner check: `owner_can_be_real_root` accepts uid 0, or the overflow uid when that uid isn't
  mapped inside this namespace and isn't the process's own. It reads only the inside column of
  `/proc/self/uid_map`, because the outside column is relative to the parent namespace (a nested
  namespace shows `1000 0 1` for a plain user). The not-writable and no-symlink checks are
  unchanged. Another unmapped account also shows as 65534, so a path component owned by a
  different unprivileged user would pass. Test:
  `overflow_owner_counts_as_root_only_when_it_is_unmapped`.
- Loopback: a fresh `unshare -n` namespace has `lo` down. Bind works, connect fails with
  "Network is unreachable" (checked under `unshare -cn` with Python). Chromium's DevTools
  endpoint is on 127.0.0.1, so no browser tool can work under the current wrapper even with the
  owner fix. `browser_prepare` now checks loopback before launching and refuses with that reason,
  so it doesn't leave an unreachable Chromium running. Test:
  `loopback_probe_connects_where_loopback_is_up`.
- Sandbox: the fix scripts enter the root with `pivot_root`, because the kernel refuses new user
  namespaces inside a `chroot`. Under `unshare -c`, Chromium and its parent directories showed
  owner 65534 and `browser_prepare` launched an isolated Chromium in 1.0 s. Under `unshare -cn`
  it refused at once and no Chromium process started.
- Wrapper, applied to the MCP registration that evening with your OK: it keeps the network cut
  but loopback up (only `lo` exists inside, Pincer runs as uid 1000, root-owned files show 65534):

  ```bash
  unshare -rn sh -c 'ip link set lo up && exec unshare --map-user=1000 --map-group=1000 ~/.local/bin/pincer mcp'
  ```

  `unshare -cn` alone can't bring `lo` up: the mapped user loses its capabilities at exec
  ("RTNETLINK answers: Operation not permitted"). The outer `-r` provides `CAP_NET_ADMIN` for
  `ip link`, and the inner `unshare` maps back to uid 1000 so Pincer and Chromium don't run as
  namespace root.
- A live run through the wrapper on this desktop found two more problems after
  `browser_prepare`, which itself worked (0.97 s, endpoint reached over loopback):
  - Chromium chose native Wayland, because `WAYLAND_DISPLAY` is set and the isolated launch only
    passed `--ozone-platform` with no XWayland at all. `list_windows` never saw its window.
    `linux_ozone_platform` now passes `--ozone-platform=x11` whenever `DISPLAY` is set and the
    native Wayland backend is off. Test: `isolated_launch_uses_x11_unless_the_driver_reads_wayland_windows`.
  - `get_browser_state` then refused with "no CDP target correlates with native window". The
    browser platform's `native_window` and `is_only_exact_native_window` checked
    `WAYLAND_DISPLAY` alone, so XWayland-mode X11 ids went to the Wayland lookups: inexact
    geometry, no window count, and no title match against "Untitled - Chromium". Both now use
    `crate::wayland::is_wayland()`, the rule `list_windows` uses. Test:
    `xwayland_mode_looks_up_browser_windows_through_x11`, which fails on the old check with
    `BrowserRouteUnavailable`.
  - After both: `browser_prepare` 0.58 s, `list_windows` finds the window at once,
    `get_browser_state` binds exactly with 1 tab, and `end_session` closes Chromium and deletes
    its profile. Page navigation wasn't checked live: `browser_navigate` refuses `data:` URLs,
    and nothing else is reachable inside the namespace.
  - Not changed: the window's class is `Com.anthropic.Claude`. The Claude desktop app sets
    `CHROME_DESKTOP=com.anthropic.Claude.desktop`, and the launched Chromium inherits it; that is
    the likely cause, not tested.
  - Tests after these: `cua-driver-core` 950 passed; `platform-linux` 691 passed with the same one
    failure as below.

### 8. A click that opens a JavaScript dialog

`CdpConnection::call_until_dialog` races a command against `Page.javascriptDialogOpening` for
the tab. The first attach per connection and tab now enables dialog events
(`enable_dialog_events`, moved out of `browser_dialog`, with a 5 s bound on `Page.enable`).
`browser_click` uses the race on both routes:

- When the click opens a dialog, it returns `effect: "confirmed"` with `dialog: {dialog_id, kind}`
  and `browser_readback` evidence, so MCP's action record keeps "confirmed" and shows the id.
- Focus emulation is restored in the background, because the page answers nothing while the
  dialog is open.
- A click while a dialog is open is refused at once, with `next_call: "browser_dialog"`.
- Tests: `a_call_returns_when_its_target_opens_a_dialog`,
  `a_click_that_opens_a_dialog_returns_at_once_with_the_dialog` (both routes and the action-record
  projection).
- Sandbox: the trusted click returned in 0.7 s and the DOM click in 0.05 s, where both used to
  take 20 s and report failure. The second click was refused in 0.06 s. Inspect and dismiss
  worked with no inspect before the click.
- Not checked: the page text reading "dismissed" afterwards (my check read interactive refs,
  which leave out the `<p>`).
- Not done: `browser_type` and `browser_pointer` can open dialogs too and still wait for the reply.

### Smaller issues

- `get_accessibility_tree` drops kernel threads (empty `/proc/<pid>/cmdline`). Test:
  `kernel_threads_are_left_out`.
- `zoom` without a pid now says `no_screenshot` or `other_session` for a window that was read but
  has no frame for the session, instead of "has not been read yet". Test:
  `zoom_without_pid_says_why_a_read_window_has_no_frame`. The original sequence wasn't replayed.
- Not changed: `right_click` naming its popup, `set_window_frame` unmaximizing first, the 1000 ms
  AT-SPI budget, and `kill_app`'s CLI ownership rule. Each needs foreground input on the desktop
  or a design decision. `launch_app`'s "handed off for D-Bus activation" wording is also unchanged.

### Test runs and sandbox files

- `cargo test -p cua-driver-core --lib`: 949 passed.
- `cargo test -p platform-linux --lib`: 690 passed, 1 failed. The failure,
  `overlay::tests::unreported_arrival_releases_the_waiter_after_the_cap`, happens whenever
  `WAYLAND_DISPLAY` is set. It fails the same way on unmodified `d90ebe3` and passes with
  `env -u WAYLAND_DISPLAY`.
- Sandbox: `tools/ptest/run-fixes.sh`, `pincer-inner-fixes.sh`, `session-fixes.sh` and
  `fixes_check.py`; results in `screens/pincer-fixes`; the old build is kept at
  `root/usr/local/bin/pincer.before-fixes`. AT-SPI doesn't run in the sandbox: its
  `at-spi-bus-launcher` starts `dbus-broker`, which exits without a systemd journal. Bugs 3 and 5
  were therefore checked on the desktop.

## Sandbox round, 2026-10-28

The desktop is back on Plasma 6.7.5 X11, so this round ran only in the rehearsal sandbox: a
headless KWin 6.7.5 Wayland (`kwin_wayland --virtual`, 1600x1000) with XWayland, Qt 6.12, no
network, `/dev/uinput` covered by a plain file. The desktop test above was on KWin 6.8.1, so
anything that depends on the KWin version is unconfirmed for 6.8.

### Harness

`~/plasma-x11-baseline/sonicde/rehearsal/tools/ptest/wl/`: `run.sh <script> [seconds]` boots the
session and runs `wl/<script>` as the test user; results land in `screens/wl-<script>`.
`common.sh` adds what the 10/25 rounds lacked:

- AT-SPI. The stock `at-spi-bus-launcher` starts `dbus-broker`, which exits without a systemd
  journal. `common.sh` runs the accessibility bus with `dbus-daemon` and the stock
  `accessibility.conf`, starts `at-spi2-registryd`, and `a11y_shim.py` answers `org.a11y.Bus`
  on the session bus. Pincer's tree walks work there now.
- Real compositor input. `fakeinput.c` sends pointer events through KWin's
  `org_kde_kwin_fake_input` (allowed by `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1` in KWin's
  environment), so clicks reach native Wayland windows the way a mouse's would. `fakeinput hold`
  keeps one pointer device on the seat for the whole run (see below).
- `EIS_NO_PROMPT=1` sets `[Xwayland] XwaylandEisNoPrompt=true` in the sandbox user's `kwinrc`
  for one run, which pre-approves KWin's "is asking to control input devices" prompt for XTEST.

### Agent cursor on XWayland (bug 6 follow-up)

The X11 overlay was started on XWayland with a throwaway build (an env switch around
`should_start_x11_overlay`, not committed; the binary is `root/usr/local/bin/pincer.overlay-exp`).
On KWin 6.7.5:

- It drew correctly: KWin owns `_NET_WM_CM_S0`, the overlay picked its 32-bit ARGB visual, and
  the cursor showed over a native Wayland Kate with everything outside it see-through. No black
  window. KWin lists it as an unmanaged full-screen window in layer 9.
- Clicks passed through. Real compositor clicks opened the File menu of a native Wayland Kate
  and of an XWayland Kate with the overlay mapped, also with the cursor resting exactly on the
  File item, so KWin honors the overlay's empty input shape.
- Pincer's own foreground click worked with the overlay up.

So upstream's reason for keeping it off (an opaque black window over the Wayland desktop) does
not hold on KWin 6.7.5. Not checked: KWin 6.8, GNOME, scaled or multi-monitor outputs, and CPU
cost.

Turned on for KWin on 2026-10-29. `x11_overlay_allowed` in `overlay.rs` starts the overlay
under `WAYLAND_DISPLAY` only when XWayland's window manager is KWin: the
`_NET_SUPPORTING_WM_CHECK` window points at itself and its `_NET_WM_NAME` is `KWin`. KWin sets
that before anything in the session runs (`wl/wmprobe.sh`). The decision is made once per
process. Native Wayland mode (`CUA_DRIVER_RS_ENABLE_WAYLAND=1`) still keeps it off on every
compositor, since the layer-shell backend draws there. Checked with the release build and no
switch (`wl/gate.sh`):

- Default mode: `renderer: "x11_overlay"`, and `Cua.AgentCursorOverlay.default` is mapped at
  1600x1000, depth 32.
- A real click on the cursor's pixels over an XWayland Kate's File item opened the menu.
- Native mode: `renderer: "wayland_layer_shell"` and no X11 overlay window.

### `move_along_path` on XWayland

With KWin's prompt approved, both scopes glide the real pointer to the right place (`wl/gate.sh`,
`wl/example.sh`). A smoothed desktop polyline ended at (1000, 700), a window-scope 360° arc on
Kate ended at its expected screen point (600, 400), and the README's parametric circle ended at
(1000, 500), all read from KWin's `workspace.cursorPos`. Don't check this with `xdotool
getmouselocation`: XWayland only learns the pointer position while it is over an X11 window, so
the desktop path seemed to stop at (898, 415), the edge of Kate.

### Foreground XTEST

With the prompt approved, `click`, `right_click` and `press_key` with
`delivery_mode: "foreground"` all land on an XWayland Kate.

One false lead: the first foreground click of a session lost its button press (only a
`RawButtonRelease` arrived, after a `HierarchyChanged`). That happens only when KWin's seat has
no pointer device at all, which the sandbox had between `fakeinput` calls: XWayland disables its
pointer devices, and the press arrives while they come back. With `fakeinput hold` keeping a
pointer on the seat, as a physical mouse does, the first click lands. A bare `ctypes` XTEST
client behaves the same way, so this is not Pincer's. The first foreground key press landed
either way.

### Chromium's window class

Confirmed and fixed. The Claude desktop app sets `CHROME_DESKTOP=com.anthropic.Claude.desktop`;
an isolated Chromium launched by a Pincer with that variable set its `WM_CLASS` to
`com.anthropic.Claude`, without it `chromium-browser`. `configure_linux_isolated_browser_command`
now removes `CHROME_DESKTOP` from the browser's environment. Test:
`isolated_launch_does_not_inherit_the_parent_apps_chrome_desktop`. Sandbox, with the variable
set: `Chromium-browser`, exact bind, and `end_session` closed Chromium. This run also used a
normal Wayland session environment (`XDG_SESSION_TYPE=wayland`, `WAYLAND_DISPLAY` set), which
confirms the `--ozone-platform=x11` fix from 10/25 outside the desktop.

### Still open, not Wayland-specific

Reproduced with AT-SPI working:

- `invoke_menu(["Selection", "Select All"])` fires the item but leaves the Selection popup open
  until something closes it (an XTEST Escape did).
- A foreground `right_click` opens Kate's context menu, and the reply doesn't name it (no
  `window_change`, `effect: "unverifiable"`).
- The default 1000 ms walk budget is borderline: the default read of Kate took 1.02 s and came
  back `truncated: true` (`truncation_reason: "timeout"`, `elements_complete: false`), with the
  same 51 elements a 15 s read returned complete in 1.04 s.
