# Pincer Rust integration tests

Tests in this directory exercise the public driver interface without a
desktop: MCP and CLI protocol, schemas, sessions, policy, and daemon
lifecycle. Plain `cargo test -p pincer` runs them and must stay hermetic.
They get `CARGO_BIN_EXE_pincer`, so Cargo builds the driver for them.

Desktop E2E suites that need a real desktop, fixture apps, installed apps, or
OS permissions live in their own crate:
[`../../pincer-e2e/tests/`](../../pincer-e2e/tests/README.md).

The few `#[ignore]` tests left here need real input or real per-user state.
Each is either selected by a canonical runner or listed with a reason in
`tests/manual-e2e-allowlist.txt`.

## Naming

| Prefix | Purpose |
| --- | --- |
| `protocol_*_test.rs` | MCP/CLI protocol and schema behavior |
| `session_capture_scope_test.rs` | Refusal of the removed `capture_scope` session input and the retired config key |
| `schema_*_test.rs` | Generated schema consistency |
| `*_cli_test.rs` | CLI verbs driven through the built binary |

## Running

```bash
cargo test -p pincer --tests
cargo test -p pincer --test protocol_handshake_test
```
