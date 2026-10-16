# cua-driver scripts

Local build-and-install, uninstall, and VM sync helpers for cua-driver.

Upstream Cua's release installers and uninstallers (`install.sh`,
`install.ps1`, `uninstall.sh`, `uninstall.ps1`, and their helpers) fetched
and managed upstream releases, so this fork removed them. To build the driver
without installing it, see the [top-level README](../README.md#build-from-source).

| Script | Purpose |
| --- | --- |
| `install-local.sh` / `install-local.ps1` | Build this checkout as the separate `cua-driver-local` product |
| `uninstall-local.sh` / `uninstall-local.ps1` | Remove only the source-built `cua-driver-local` product |
| `_install-local-rust.sh` | Rust build and install internals for `install-local.sh` |
| `_local-signing.sh` | Stable macOS code-signing helpers for the local installer |
| `sync-vm-worktree.sh` | Sync this checkout to verification VMs and pull artifacts back |
| `post-install-hints.txt` | User-facing hints printed by the local installers |

## Stable macOS local signing

macOS Accessibility and Screen Recording grants are tied to an app's
designated requirement. An ad-hoc signature uses a `cdhash` requirement that
changes on every rebuild, so its grants do not survive the next local install.
The installer now reports whether the installed requirement is
`certificate-backed` or `ad-hoc cdhash`; an ad-hoc install always prints a
prominent warning and bootstrap instructions.

For behavior or E2E verification, require the stable path:

```bash
bash scripts/install-local.sh \
  --release --autostart --require-stable-signing
```

`CUA_DRIVER_REQUIRE_STABLE_SIGNING=1` is the environment equivalent. Strict
mode stops before replacing the live app when no usable certificate-backed
identity is available.

For the most reliable non-interactive rebuilds, use a dedicated keychain:

```bash
SIGNING_KEYCHAIN="$HOME/Library/Keychains/cua-driver-signing.keychain-db"
security create-keychain "$SIGNING_KEYCHAIN"  # first time only
security set-keychain-settings "$SIGNING_KEYCHAIN"
security unlock-keychain "$SIGNING_KEYCHAIN"
export CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN="$SIGNING_KEYCHAIN"
```

To use an existing certificate without allowing the installer to select a
different identity from that keychain, also provide its exact SHA-1 fingerprint:

```bash
export CUA_DRIVER_LOCAL_SIGNING_IDENTITY="<40-hex-character SHA-1>"
```

The installer fails closed when that exact usable code-signing identity is not
present in `CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN`.

The first install creates `CuaDriver Local Signing (cua-driver-rs)` in that
keychain. If `codesign` cannot use its private key non-interactively, unlock
the keychain, trust the certificate in Keychain Access, and authorize Apple
code-signing tools:

```bash
read -r -s -p 'Keychain password: ' KEYCHAIN_PASSWORD; echo
security set-key-partition-list \
  -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PASSWORD" \
  "$SIGNING_KEYCHAIN"
unset KEYCHAIN_PASSWORD
```

Then rerun the strict installer and grant Accessibility and Screen Recording
once. When the dedicated default keychain above exists, the installer prefers
it automatically; exporting `CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN` remains the
most explicit choice.

Keep source commits host-owned. Verification machines should sync from this
checkout and return artifacts, not push code.

To remove a local installation, run the matching script from the checkout.

```bash
# macOS / Linux, from the checkout
scripts/uninstall-local.sh

# Windows, from the checkout
scripts/uninstall-local.ps1
```

The local uninstaller leaves any `cua-driver` or `CuaDriver.app` installed by
other means untouched, together with their services, state, and TCC grants.
On macOS it revokes only `com.trycua.driver.local`; pass `--keep-tcc` to
retain that local grant.
