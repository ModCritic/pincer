<div align="center">
  <img src="docs/pincer-logo.svg" alt="Pincer logo" width="160">
  <h1>Pincer</h1>
  <p>Linux desktop control for AI agents over MCP.</p>
</div>

## About this fork

Pincer is a fork of Cua Driver, taken from `libs/cua-driver` in
[trycua/cua](https://github.com/trycua/cua) at commit `e6a244a8d`. It is
licensed under the MIT License; see [LICENSE](LICENSE).

Everything upstream named `cua-driver` is renamed: the binary and CLI are
`pincer`, the crates are `pincer-*`, environment variables start with
`PINCER_`, settings live in `~/.pincer/config.json`, and Linux state lives in
`~/.local/state/pincer`. The Python and TypeScript SDK packages are
`pincer-driver`, because `pincer` is taken on PyPI and npm by unrelated
projects. Upstream's `CUA_DRIVER_*` variables and `cua-driver` folders are not
read.

Changes from upstream:

- Linux gains a `capture_series` tool, which takes screenshots on a schedule
  and can stop early once the image stops changing, and an optional `region`
  crop for `get_desktop_state`.
- Linux gains a `move_along_path` tool, which glides the real pointer along
  lines, curves, or a formula; see [Pointer paths](#pointer-paths).
- On KDE Plasma's Wayland session, the agent cursor overlay draws through
  XWayland. Upstream keeps it off on every Wayland compositor.
- PostHog telemetry is removed at compile time.
- The optional perception extension is removed, together with the
  `parse_visual_regions` tool, the extension installer, the
  `install_extension` tool, and the `cua-perception` worker.
- The update check is removed: the startup check, the `check-update`,
  `update` and `channel` commands, and the `check_for_update` tool. The
  driver never looks up releases or runs upstream's installer, so it cannot
  replace itself with an upstream build. The removed commands exit with a
  note to pull and rebuild instead.
- `skills install` and `skills update` write the skill pack built into the
  binary instead of downloading it from GitHub, so they work offline.
  `--from main` is refused.

The driver makes no network calls of its own; the browser tools only talk to
a browser on your machine through its loopback debugging port.
`install_ffmpeg` with `confirm: true` runs the system package manager, which
downloads ffmpeg from that package manager's sources.

This fork publishes no release binaries, Python wheels, or npm packages, and
upstream's release installer scripts are removed. Build it from source as
described below. The fork's own changes target Linux;
the macOS and Windows code from upstream is still in the tree.

See [MCP protocol and skills](docs/mcp-protocol-and-skills.md) for the modern
stdio profile, legacy compatibility, bundled skill resources, and HTTP limits.

## Build from source

1. Install [rustup](https://rustup.rs/). `rust/rust-toolchain.toml` pins Rust
   1.97.1 with the `rustfmt` component. rustup installs that toolchain the
   first time you run `cargo` in `rust/`, unless its auto-install setting is
   disabled; `rustup toolchain install` run in `rust/` installs it up front.
2. Build the driver.

   ```bash
   cd rust
   cargo build --release -p pincer
   ```

3. The binary is `rust/target/release/pincer` (`pincer.exe` on
   Windows). If `CARGO_TARGET_DIR` is set, it is
   `$CARGO_TARGET_DIR/release/pincer` instead. The workspace uses Cargo's
   standard `release` profile.
4. Check that the driver can reach the current graphical session.

   ```bash
   rust/target/release/pincer doctor
   ```

On Linux, the driver must run in the graphical user's session and share its
AT-SPI session bus.

## Connect an agent

Register the binary with Claude Code for your user account, using its
absolute path.

```bash
claude mcp add --scope user pincer -- /path/to/pincer mcp
```

The agent cursor overlay is on by default: an arrow that shows where the
driver points, clicks and types, and follows every pointer move it makes.
Add `--no-overlay` to turn it off. On a Wayland session in the default
XWayland mode, the overlay only draws on KDE Plasma (KWin); on other
compositors it stays off. On Linux and Windows, `pincer mcp` owns its
runtime directly and shuts it down when stdin closes.

On Linux, you can also deny the driver network access by starting it with
`unshare -cn`, which runs it in a new user and network namespace whose only
interface is a loopback device that is down.

```bash
claude mcp add --scope user pincer -- \
  unshare -cn /path/to/pincer mcp
```

This needs unprivileged user namespaces. Inside the namespace the driver
cannot open TCP/IP connections, including to a browser's loopback debugging
port, so the browser tools do not work. Abstract Unix sockets are isolated as
well (see `network_namespaces(7)`); Unix sockets with a filesystem path are
not.

`pincer mcp-config --client <name>` prints setup for other MCP clients.

### Claude Code computer-use compatibility

`--claude-code-computer-use-compat` is still accepted, so older setup commands
keep working, but it changes nothing. The window-scoped `screenshot` tool it
used to register is gone; window screenshots come from `get_window_state`.
Claude Code reserves the server name `computer-use` for its own server and
gives no other name special treatment, so register the driver under any other
name, such as `pincer`.

## Pointer paths

`move_along_path` (Linux only) moves your real mouse pointer along a path, so
it visibly travels instead of jumping. Use it to draw in a paint program,
drag along a curve, or trace a hover route. The agent cursor follows it.
Because it takes over your pointer for the length of the glide, the bundled
skill tells agents to use it only when you ask for real-pointer control.

Describe the path in one of three ways:

- `segments`: a `start` point, then up to 256 pieces. Each piece is a `line`,
  a `quad` or `cubic` Bezier curve, or an `arc` around a center (a positive
  `sweep_deg` turns clockwise on screen).
- `points`: up to 2000 `[x, y]` points joined by straight lines, or by one
  smooth curve through all of them with `smooth: true`.
- `parametric`: `x` and `y` as formulas of `t`, run from `t_from` to `t_to`,
  for circles, loops and spirals. Formulas take `+ - * / ^`, `sin`, `cos`,
  `tan`, `sqrt`, `abs`, `exp`, `ln`, `pi` and `e`.

Options:

- `button` (`left`, `right`, `middle`) presses at the first point, holds the
  whole way, and releases at the last. Without it, the pointer only hovers.
- `click_at_end` (`left`, `right`, `middle`, `double`) clicks at the last
  point. It can't be combined with `button`.
- `duration_ms` (default 600, at most 30000) and `easing` (`linear`,
  `ease_in`, `ease_out`, or the default `ease_in_out`) set the timing.
- `spacing_px` (default 4) is the largest gap between pointer positions along
  the path. One call sends at most 4000 positions; a longer path is refused,
  and the error names the smallest spacing that fits.

By default, `scope` is `"window"`: points are pixels in that window's
`get_window_state` screenshot, `pid` and `window_id` are required, and the
window is brought to the front first. `scope: "desktop"` takes
`get_desktop_state` pixels instead. A path that leaves the screen is refused
before any input is sent. In the native Wayland mode
(`PINCER_ENABLE_WAYLAND=1`), only `scope: "desktop"` without `pid` and
`window_id` works. On KDE Plasma's Wayland session, KWin asks whether to let
the driver control input, and the pointer only moves once you allow it.

This traces a circle with a 200-pixel radius around (800, 500) in 1.5 seconds:

```bash
pincer call move_along_path '{"scope":"desktop","parametric":{"x":"800+200*cos(t)","y":"500+200*sin(t)","t_from":0,"t_to":6.2832},"duration_ms":1500}'
```

The reply ends with one JSON line giving the number of segments and samples,
the path length in pixels, the duration, and the start and end points.

## Agent skill pack

The skill pack in `rust/Skills/pincer` is compiled into the binary. The
MCP server serves it as resources, and `pincer skills install` writes the
same copy into detected agent directories without downloading anything. The
direct installer keeps only the host OS document unless `--all-platforms` is
used. The `@cua/driver` skill on ClawHub is upstream's, not this fork's.

## Integration surfaces

- **Agent MCP or CLI:** MCP-capable agents connect directly to
  `pincer mcp`; shell-oriented agents and automation can use
  `pincer call`. No generated language client is required.
- **Application SDK:** Python applications import `pincer_driver`; TypeScript
  applications import `pincer-driver`. Both package roots call the same
  in-process native runtime through generated UniFFI bindings. The safe Rust
  SDK and generated bindings sit above a versioned C ABI; they do not require
  a daemon for direct application use.

The language packages are for client applications, not agents. They contain no
language-native MCP facade and have no `/sdk`, `/mcp`, or `/native` public
suffix. MCP remains implemented by the `pincer` executable as the
runtime-neutral agent boundary.

This fork publishes neither package, so build them from `python/` and
`typescript/`. A `pincer` package on PyPI or npm is an unrelated project.

## Computer History preview

Upstream's opt-in, encrypted history of actions performed through the driver
is still in the tree. The preview stores a strict metadata allowlist, stays
local, and exposes permission-gated `history_status` and `history_query` tools
for read-only agent hydration. It never stores screenshots, typed text,
clipboard contents, raw arguments or results, accessibility trees, paths,
window titles, or URLs.

[Try the Computer History desktop preview](docs/computer-history-preview.md)
covers enabling, inspection, and deletion; its install and uninstall commands
fetch upstream builds and do not apply to this fork. The [architecture and
staged plan](docs/computer-history-architecture.md) defines the format,
security boundary, release gates, and later NVIDIA OpenShell integration.

## Permission modes

`standard` is the promptless default for normal automation. `bounded` admits
only the tools and resources in a reviewed manifest. `unrestricted` requires
`--dangerously-bypass-approvals`.

The mode belongs to the process that owns the runtime and is fixed at launch:
`pincer serve` takes the flags, while `pincer mcp` and embedding hosts
use the matching `PINCER_PERMISSION_MODE`,
`PINCER_CAPABILITY_MANIFEST_FILE`, and
`PINCER_CAPABILITY_MANIFEST_APPROVED` variables. Choose it before starting
the daemon; a running daemon must be restarted to change it.

Attaching to an existing logged-in Chromium profile remains explicit:

```bash
pincer mcp --grant existing-profile
```

An embedding application can instead provide `DriverAuthorizationHost`, and a
bounded runtime can declare `kind: existing_profile` in its manifest. The
driver does not render its own authorization modal or banner.

See [`rust/Skills/pincer/RUNTIME.md`](rust/Skills/pincer/RUNTIME.md)
for the permission modes, capability manifests, and authorization rules.

## Repository layout

| Path                            | Purpose                                                                |
| ------------------------------- | ---------------------------------------------------------------------- |
| `rust/`                         | Cargo workspace for the daemon, UniFFI SDK, platform crates, and tests |
| `python/`                       | Python SDK, bundled-binary wrapper, and package tests                  |
| `contract/`                     | Experimental generated SDK contract and fixtures                       |
| `typescript/`                   | Generated TypeScript SDK                                               |
| `tests/fixtures/`               | Source-built GUI harness apps and shared fixtures                      |
| `rust/crates/pincer/tests/`     | Hermetic Rust integration tests for the driver                         |
| `rust/crates/pincer-e2e/tests/` | Desktop E2E suites for the GUI harnesses                               |
| `scripts/`                      | Local build-and-install, uninstall, and VM sync helpers                |
| `docs/`                         | Repo-local specs, design notes, and historical plans                   |

Start with `rust/README.md`, `rust/crates/pincer-e2e/tests/README.md`, and
`tests/fixtures/README.md` when changing driver behavior or tests.

Upstream drives the complete Rust harness catalog from canonical GUI runner
scripts under `scripts/ci/`; those scripts are not part of this repository.
See [`docs/test-harnesses-guide.md`](docs/test-harnesses-guide.md) and, for
preparing a macOS Lume worker, [`tests/runners/macos-lume/README.md`](tests/runners/macos-lume/README.md).
Keep build artifacts, VM logs, and local verification journals out of git
unless they become a stable fixture or contributor doc.

The contract-first SDK architecture is documented in
[`contract/README.md`](contract/README.md). The Rust contract crate generates
the checked-in manifest and supplies the typed inputs used by the live daemon
and UniFFI SDK.

The imported SDK bindings are generated by
`scripts/generate-uniffi-bindings.mjs`. `Pincer.create()` loads the native
runtime in the application process; the daemon-compatible `connect()` path is
retained for external clients. The stable native boundary is declared in
[`rust/include/pincer_abi.h`](rust/include/pincer_abi.h). That header is
generated from the Rust `#[repr(C)]` exports with
`cargo run -p pincer-bindgen --bin pincer-abi-header`; running the
same command with `--check` confirms that the implementation and the header
have not drifted. UniFFI separately generates its private FFI scaffolding and
the Python and TypeScript SDK bindings; that implementation-specific
scaffolding is not the stable public `pincer_*_v1` C contract.

For direct agent integrations, see the
[`examples/agent-sdks`](examples/agent-sdks/README.md) Codex and Claude Agent
SDK examples. They connect the agent to `pincer mcp` without importing a
generated client.

Contributor documentation:

- `docs/cursor-themes.md` documents the default semantic cursor and custom
  dotLottie authoring contract.
- `docs/test-matrix.md` maps unit and canonical harness E2E suites.
- `docs/action-support.md` is the empirical platform behavior ledger.
- `docs/test-harnesses-guide.md` explains fixture and runner ownership.
- `docs/linux-desktop-validation.md` covers representative Linux sessions.
- `docs/linux-support-completion-plan.md` preserves the historical Linux plan.

## macOS process identity and permissions

This fork ships no prebuilt `Pincer.app`. On macOS,
`scripts/install-local.sh` builds a separate `PincerLocal.app` from this
checkout.

macOS attributes Accessibility and Screen Recording grants to a responsible app identity, not simply to an executable path. Use one of these supported launch modes:

- **Standalone:** install `Pincer.app`, grant permissions to it, and start its daemon with `open -n -g -a Pincer --args serve`. The installed `pincer mcp` CLI may proxy through this daemon automatically.
- **Explicit direct MCP:** `pincer mcp --direct` makes the MCP process own
  its runtime. On macOS this deliberately uses the spawning host's TCC
  attribution and does not provide the AppKit cursor overlay without a
  certified host adapter.
- **Embedded:** have the macOS app that owns the grants use the generated `EmbeddedPincerHost` to spawn a private daemon and return both SDK and MCP connection details. The daemon stays in the app's responsibility chain and inherits its grants. A gateway, terminal, or unrelated helper must not spawn the daemon on the app's behalf. `pincer-driver/embedded` is an organizational alias for the same Rust host exported at the package root; it has no separate lifecycle implementation.

Directly spawning a raw `pincer serve` outside `Pincer.app` without embedded mode is unsupported: it has no stable bundle identity for TCC attribution. Do not grant permissions to arbitrary binary paths or rely on that configuration in production. See [`rust/Skills/pincer/EMBEDDING.md`](rust/Skills/pincer/EMBEDDING.md) for the embedding contract and examples.
