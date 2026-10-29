# Pincer Rust workspace

This directory is the standalone Cargo workspace for the driver daemon, platform
implementations, testkit, and helper crates.

## Crates

| Crate | Purpose |
| --- | --- |
| `pincer` | Main CLI/MCP daemon and hermetic integration tests |
| `pincer-e2e` | Desktop E2E suites that the canonical OS runners select; never shipped |
| `pincer-core` | Shared protocol, tool models, config, and CDP helpers |
| `platform-macos` | macOS AX, capture, input, browser, and daemon support |
| `platform-windows` | Windows UIA, capture, input, overlay, and diagnostics |
| `platform-linux` | Linux AT-SPI, X11/Wayland, capture, and input support |
| `pincer-testkit` | Test-only helpers for spawning the daemon and parsing responses |
| `pincer-uia` | Windows UIAccess worker |
| `cursor-overlay` | Cross-platform semantic cursor renderer and bounded compiled-theme loader |
| `cursor-theme-cli` | Short-lived dotLottie validation, compilation, preview, and installation sidecar |
| `pip-preview` | Packaging preview helper |

Platform crates are selected with `cfg(target_os)` from the main driver crate.
Keep platform-specific behavior inside the matching `platform-<os>` crate.

## Build

```bash
cargo build -p pincer
cargo build -p pincer --release
```

The Python wrapper and install scripts expect the built binary at
`target/<profile>/pincer` (or `pincer.exe` on Windows), unless
`PINCER_BINARY` is set.

## Formatting

The supported Rust toolchain and `rustfmt` component are pinned in
`rust-toolchain.toml`. Install [rustup](https://rustup.rs/) and run the
canonical formatting check from this directory:

```bash
cargo fmt --all -- --check
```

To apply formatting, omit `--check`:

```bash
cargo fmt --all
```

## Tests

Default tests should be headless and safe for CI:

```bash
cargo test -p pincer-core
cargo test -p pincer --test protocol_schema_test
```

GUI and VM-backed tests live in `pincer-e2e`, are marked `#[ignore]`, and
require a built driver, harness apps from `../tests/fixtures/build/`, and a
real interactive desktop session:

```bash
cargo build -p pincer
cargo test -p pincer-e2e --test harness_appkit_test -- --ignored --nocapture
cargo test -p pincer-e2e --test harness_wpf_test -- --ignored --nocapture
```

See `crates/pincer-e2e/tests/README.md` for the desktop E2E suites and
`crates/pincer/tests/README.md` for the hermetic driver tests.

Testkit-owned daemons use a temporary `PINCER_HOME` on every platform.
A nonempty override also places Computer History data and its `admission.json`
under `<PINCER_HOME>/computer-history`. Without an override (or with an
empty value), History keeps its platform-specific default location. Existing
data is not migrated, and installed-product admission checks still apply.
On Windows this isolates driver state without redirecting `LOCALAPPDATA`,
`APPDATA`, or `USERPROFILE`, which Windows app discovery needs. Explicit caller
environment values take precedence; `SHARE_HOST_STATE` opts a testkit spawn out
of isolation. Use separate roots for independent driver instances. This isolates
files, not the operating system's credential store.

## Generated Outputs

- `target/`: Cargo output, ignored.
- `test-apps/harness-*/`: staged harness apps, ignored.
- `test-apps/README.md`: explains how staged harness outputs are produced.

## Permission Policies

Set `PINCER_POLICY_FILE` on the daemon to enforce a deny-by-default policy
on all tool calls from MCP and CLI clients. If the variable is unset, or points to a path that does
not exist, the driver keeps its backward-compatible behavior with no policy
enforcement. Supported paths are:

- `.yaml` / `.yml`: a single YAML policy file.
- `.rego`: a single Rego policy file.
- A directory: all top-level `.rego` files are loaded in sorted order.

YAML policies allow unconstrained tools through `allow.tools`, constrained
calls through `allow.rules`, and explicit overrides through `deny.tools`:

```yaml
allow:
  tools: [screenshot, scroll, wait]
  rules:
    - tool: click
      constraints:
        x: { min: 0, max: 1920 }
        y: { min: 0, max: 1080 }
    - tool: type_text
      constraints:
        text: { max_length: 1000, pattern: "^[a-zA-Z0-9\\s]+$" }
    - tool: launch_app
      constraints:
        app:
          allowed: [Calculator, TextEdit, Safari]
deny:
  tools: [shell_execute, file_delete]
```

Rego policies must expose a boolean rule at `data.pincer.policy.allow`. The input
shape is `{ "server": "pincer", "tool": <name>, "arguments": <object> }`.
Regorus evaluates policies inside the daemon; no OPA service is required.
