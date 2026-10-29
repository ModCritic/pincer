#!/usr/bin/env bash
# Stage the Tauri test fixture app for pincer tests on Linux and macOS.
#
# The app embeds the same shared/web/index.html DOM used by Electron,
# WebView2, and WKWebView. Build output is copied into
# rust/test-apps/harness-tauri/ with deterministic executable names.
set -euo pipefail

tauriDir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crossDir="$(dirname "$tauriDir")"
appsDir="$(dirname "$crossDir")"
harnessDir="$(dirname "$appsDir")"
pincerDir="$(cd "$harnessDir/../.." && pwd)"
outDir="$pincerDir/rust/test-apps/harness-tauri"
platform="$(uname -s)"
tauriTargetDir="${CARGO_TARGET_DIR:-$tauriDir/src-tauri/target}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "[ERROR] cargo not on PATH. Install Rust first." >&2
  exit 1
fi

mkdir -p "$tauriDir/web"
cp -r "$harnessDir/shared/web/." "$tauriDir/web/"

echo "[BUILD] cargo build --release --features custom-protocol ..."
CARGO_TARGET_DIR="$tauriTargetDir" \
  cargo build --release --features custom-protocol \
    --manifest-path "$tauriDir/src-tauri/Cargo.toml"

rm -rf "$outDir"
mkdir -p "$outDir"

if [ "$platform" = "Darwin" ]; then
  srcBin="$tauriTargetDir/release/pincer-test-harness-tauri"
  [ -x "$srcBin" ] || { echo "[ERROR] Tauri binary missing after build" >&2; exit 1; }

  bundle="$outDir/PincerTestHarness.Tauri.app"
  appMacOS="$bundle/Contents/MacOS"
  mkdir -p "$appMacOS" "$bundle/Contents/Resources"
  cp "$srcBin" "$appMacOS/PincerTestHarness.Tauri"
  chmod +x "$appMacOS/PincerTestHarness.Tauri"

  cat > "$bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>PincerTestHarness.Tauri</string>
    <key>CFBundleIdentifier</key><string>io.github.modcritic.pincer.harness.tauri</string>
    <key>CFBundleName</key><string>PincerTestHarness.Tauri</string>
    <key>CFBundleDisplayName</key><string>PincerTestHarness.Tauri</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>1.0.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  xattr -cr "$bundle" 2>/dev/null || true
  # Cargo's Mach-O is linker-signed, but copying it into a hand-built bundle
  # leaves the bundle without a valid CodeResources envelope. In the macOS E2E
  # VM this Tauri bundle produced a blank window and errSecCSUnsigned (-67062)
  # while launching its sandboxed content process. An ad-hoc deep signature is
  # sufficient for this disposable test fixture; the driver under test retains
  # its separate stable signing identity.
  codesign --force --deep --sign - "$bundle"
  echo "[OK]    Staged: $bundle"
else
  srcBin="$tauriTargetDir/release/pincer-test-harness-tauri"
  [ -x "$srcBin" ] || { echo "[ERROR] Tauri binary missing after build" >&2; exit 1; }

  cp "$srcBin" "$outDir/PincerTestHarness.Tauri"
  chmod +x "$outDir/PincerTestHarness.Tauri"
  echo "[OK]    Staged: $outDir/PincerTestHarness.Tauri"
fi
