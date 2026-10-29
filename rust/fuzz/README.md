# Tool-call boundary fuzzing

Coverage-guided fuzz targets for the transport-free MCP tool-call boundary in
`pincer-core`: JSON-RPC request parsing, reserved-argument stripping,
argument normalisation, authorization, typed contract input parsing, and
`ToolRegistry` dispatch against stub tools. No platform adapter, display, or
input device is touched.

The target bodies live in `pincer_testkit::boundary_fuzz`; the files under
`fuzz_targets/` are one-line libFuzzer wrappers. The same bodies run as bounded
smoke tests in `cargo test -p pincer-testkit`, so this directory is only
needed for real fuzzing.

The workspace pins a stable toolchain. `cargo fuzz` needs nightly only for
sanitizers, so run with `--sanitizer none`:

```bash
cd rust
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#cargo-fuzz --command \
  cargo fuzz run --fuzz-dir fuzz --sanitizer none mcp_request \
  fuzz/work/mcp_request fuzz/corpus/mcp_request -- -max_total_time=300
```

Targets: `mcp_request`, `tool_arguments`, `typed_input_json`, `registry_invoke`.

libFuzzer writes every new input it discovers into the first corpus directory
it is given. Passing an ignored `fuzz/work/<target>` directory first keeps
`fuzz/corpus/<target>` limited to the hand-picked seeds that are committed and
run by the smoke test. Omitting the directories makes `cargo fuzz` write into
`fuzz/corpus/<target>` directly; delete the generated entries before
committing if you do that.

Keep the `PINCER_*` policy and permission-mode environment variables unset
while fuzzing so the process-default authorization path stays reproducible.

A crash lands in `artifacts/<target>/`. Reproduce it with
`cargo fuzz run --fuzz-dir fuzz --sanitizer none <target> artifacts/<target>/<file>`,
fix the bug, then copy the file into `corpus/<target>/` so the smoke test keeps
it as a regression case. Only hand-picked seeds are committed; libFuzzer's
generated corpus entries stay local.

See `docs/2026-09-04-tool-call-boundary-fuzzing-design.md`.
