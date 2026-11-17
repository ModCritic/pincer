#!/usr/bin/env bash
# Type-check the macOS build on Linux: `cargo check --target aarch64-apple-darwin` for the
# pincer binary and platform-macos, tests included. Nothing is linked, so no Apple SDK is
# needed:
#   - C dependencies (rquickjs-sys, zstd-sys) compile with `zig cc -target aarch64-macos`,
#     which ships its own macOS libc headers;
#   - DOCS_RS=1 makes apple-cf (from screencapturekit) skip its Swift bridge build, as it
#     does on docs.rs.
# Needs: rustup target aarch64-apple-darwin, and zig ($ZIG, or `zig` on PATH).
# Extra arguments go to cargo (e.g. -q).
set -euo pipefail
cd "$(dirname "$0")/.."
ZIG=${ZIG:-$(command -v zig || true)}
[ -n "$ZIG" ] || { echo "check-macos: set ZIG or put zig on PATH" >&2; exit 2; }
CARGO=${CARGO:-cargo}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/cc" <<EOF
#!/usr/bin/env python3
import os, sys
args, skip = [], False
for a in sys.argv[1:]:
    if skip:
        skip = False
        continue
    if a in ("-arch", "-target"):        # cc-rs's target spellings; zig takes its own
        skip = True
        continue
    if a.startswith("--target="):
        continue
    args.append(a)
os.execv("$ZIG", ["zig", "cc", "-target", "aarch64-macos", *args])
EOF
printf '#!/bin/sh\nexec "%s" ar "$@"\n' "$ZIG" > "$work/ar"
chmod +x "$work/cc" "$work/ar"
DOCS_RS=1 CC_aarch64_apple_darwin="$work/cc" AR_aarch64_apple_darwin="$work/ar" \
    "$CARGO" check --target aarch64-apple-darwin -p pincer -p platform-macos --all-targets "$@"
