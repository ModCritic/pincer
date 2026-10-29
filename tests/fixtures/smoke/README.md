# test fixture CLI smoke runners

Per-OS broad PASS / FAIL / SKIP probes across the full pincer tool
surface. Complements the Rust integration tests under `rust/crates/pincer-e2e/tests/`:

- Integration tests drive pincer through the MCP stdio loop and
  assert on specific UIA / AX state changes.
- Smoke runners drive pincer through the CLI (`pincer call
  <tool> <json>`): different code path (CLI argument parser + tool
  resolution), one broad sweep across every registered tool in ~30 sec.

| File              | Host OS  | What it runs                                                   |
|-------------------|----------|----------------------------------------------------------------|
| `macos.sh`        | macOS    | Spawns the AppKit harness, calls every tool once               |

Each script checks in a baseline result file under `results/` for the
current driver version. Diff against it to catch regressions in tool
coverage:

```bash
./macos.sh > /tmp/run.txt
diff results/macos.txt /tmp/run.txt
```

Prereqs (macOS):
- `../build/macos.sh` must have produced `rust/test-apps/harness-appkit/`
- `rust/target/release/pincer` built (debug works too)
- TCC Accessibility granted to the pincer binary
