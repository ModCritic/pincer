# Prepare a macOS Lume golden image for GUI E2E

These steps build a private, SIP-disabled macOS golden image in Lume and clone
disposable workers from it for the macOS GUI E2E suites. Run them on an Apple
Silicon Mac with Lume.

Upstream ran its macOS acceptance gate in such a worker through a `run-all.sh`
entrypoint that delegated to its monorepo-level `scripts/ci/macos/run-rust-e2e.sh`.
Neither script is part of this repository. In a worker, run the ignored suites
with `cargo test` as described in
[`rust/crates/cua-driver-e2e/tests/README.md`](../../../rust/crates/cua-driver-e2e/tests/README.md).

The golden image supplies the logged-in Aqua session, stable local signing
identity, and existing Accessibility and Screen Recording grants. Install the
exact source commit in each worker before testing it.

## Before you start

- Use an Apple Silicon Mac with enough free space for the 150 GB sparse guest.
- Install Lume and `jq` on the host.
- Start from a clean, committed checkout of this repository.
- Keep the public base, private seed, and workers on host-local Lume storage.

## Image contract

- Use the versioned public base `macos-tahoe-cua:26.5.2`; do not use `latest`
  for an acceptance result.
- Treat the public image as a sanitized base. It contains macOS Tahoe 26.5.2,
  SIP disabled, Xcode Command Line Tools 26.6, autologin, and SSH. It does not
  contain repository source, TCC grants, or a local signing identity.
- Build one private, host-local seed from the public base. The private seed owns
  the remaining toolchain, signing identity, and TCC grants and must never be
  pushed to a registry.
- Keep the named golden VM stopped and never run tests in it directly.
- Put no repository credentials, signing secrets, or maintainer private SSH
  keys in the guest. A host public key is sufficient for source sync.
- For reusable private seeds, request the app-owned Accessibility and Screen
  Recording grants through `CuaDriverLocal.app`. For disposable SIP-off workers
  that are not cloned from a granted seed, use the checked-in `seed-tcc.sh`
  helper below. Do not hand-edit `TCC.db`.
- Require a certificate-backed local signature. An ad-hoc signature invalidates
  the inherited grants on the next build.
- Clone one worker per run, retrieve its evidence, then delete the worker.

SIP-off alone does not grant Accessibility, Screen Recording, Automation, or
direct-capture consent. The reusable private seed still carries grants approved
through the normal `CuaDriverLocal.app` prompt flow. The helper below is only for
disposable SIP-off workers that need the same app-owned Accessibility and Screen
Recording grants without preserving a granted seed. The SIP-on check below proves
the normal user-facing permission flow still works with platform protection.

## Reuse a prepared private seed

Check the host-local inventory before building from the public base. Reuse a
seed only when its versioned maintainer log records the expected public base,
macOS build, toolchain versions, signing-certificate hash, TCC grants, and
Chrome consent state. The seed and both backups must be stopped:

```bash
SEED=cua-driver-macos-e2e-seed-26.5.2-YYYYMMDD
BACKUP_A="${SEED}-backup-a"
BACKUP_B="${SEED}-backup-b"

for VM in "$SEED" "$BACKUP_A" "$BACKUP_B"; do
  lume get "$VM" --format json | jq -e '.[0].status == "stopped"'
done
```

If those checks and the maintainer log match, skip image creation and clone a
worker from the seed under [Clone a worker](#clone-a-worker).
Never boot a seed to inspect or update it. Clone a disposable worker and verify
the recorded state there. Build a new versioned seed when the inventory is
missing, incomplete, or stale.

## Create the private seed

Pull the versioned public base into a mutable local builder. The initial guest
credentials are `lume` / `lume`:

```bash
IMAGE=macos-tahoe-cua:26.5.2
BUILDER=cua-driver-macos-e2e-builder-26.5.2
lume pull "$IMAGE" "$BUILDER"
lume run "$BUILDER"
```

Keep `lume run` open. In Terminal in the VM display, verify the immutable base
properties before installing anything:

```bash
sw_vers
csrutil status
xcode-select -p
xcrun swiftc --version
```

Require macOS 26.5.2 build 25F84, disabled SIP, and
`/Library/Developer/CommandLineTools`. Stop if any value differs.

Install Homebrew from the pinned, checksum-verified upstream installer. Run it
from Terminal in the VM display so macOS can request the `lume` password when
needed:

```bash
HOMEBREW_INSTALL_COMMIT=4b0227cf8416504142d23893368c2e1d211d5191
HOMEBREW_INSTALL_SHA256=99287f194a8b3c9e6b0203a11a5fa54518be57209343e6bb954dec4635796d9d
HOMEBREW_INSTALLER="/tmp/homebrew-install-${HOMEBREW_INSTALL_COMMIT}.sh"
curl -fsSL \
  "https://raw.githubusercontent.com/Homebrew/install/${HOMEBREW_INSTALL_COMMIT}/install.sh" \
  -o "$HOMEBREW_INSTALLER"
printf '%s  %s\n' "$HOMEBREW_INSTALL_SHA256" "$HOMEBREW_INSTALLER" \
  | shasum -a 256 -c -
/bin/bash "$HOMEBREW_INSTALLER"

eval "$(/opt/homebrew/bin/brew shellenv)"
brew install node ffmpeg jq rust python-tk

printf '\n%s\n' 'eval "$(/opt/homebrew/bin/brew shellenv)"' \
  >> "$HOME/.zprofile"
```

The pinned Homebrew installer pauses for a Return confirmation after its sudo
check. Keep this step in the VM display and confirm the prompt there. A detached
SSH run can look stalled while it is waiting for that input.

Open a new Terminal window and require each command to succeed:

```bash
xcrun swiftc --version
cargo --version
node --version
npm --version
ffmpeg -version | head -1
ffprobe -version | head -1
jq --version
python3 -c 'import tkinter; print(tkinter.Tcl().eval("info patchlevel"))'
```

`python3` must resolve to Homebrew's Python with Tk 8.6 or later. Apple's
Command Line Tools Python ships Tk 8.5.9, which renders the Tk canvas fixture
too poorly for its OCR row to read the labels.

Keep autologin, sleep prevention, and screen-lock prevention enabled. Add only
the maintainer host's public SSH key to `~/.ssh/authorized_keys`; never copy a
private key or registry credential into the guest.

The public base must already have the Command Line Tools agreement accepted.
If macOS presents that agreement while installing the tools, accept it in the
VM display, then rerun the four base-property checks above. Do not freeze a
builder while `xcrun` or `swiftc` still requests an agreement.

From another host terminal, get the builder address and sync a clean committed
checkout. The sync intentionally omits `.git` and writes the exact commit to
`.cua-e2e-source-sha`. It also excludes ignored credential files such as
`.env.local`.

```bash
BUILDER=cua-driver-macos-e2e-builder-26.5.2
VM_IP="$(lume get "$BUILDER" --format json | jq -r '.[0].ipAddress')"
scripts/sync-vm-worktree.sh push "lume@${VM_IP}" '~/cua'
```

Back in Terminal in the VM display, install the app with the commit embedded
in the daemon. Tahoe Lume images can unlock their login keychain while still
rejecting the partition ACL that non-interactive `codesign` needs, so keep the
golden identity in a dedicated keychain. Create it with the same password as
the VM account so maintainers have only one local credential to enter:

```bash
SIGNING_KEYCHAIN="$HOME/Library/Keychains/cua-driver-signing.keychain-db"
security create-keychain "$SIGNING_KEYCHAIN"
security set-keychain-settings "$SIGNING_KEYCHAIN"
security list-keychains -d user -s "$SIGNING_KEYCHAIN" \
  "$HOME/Library/Keychains/login.keychain-db"
security unlock-keychain "$SIGNING_KEYCHAIN"
export CUA_DRIVER_LOCAL_SIGNING_KEYCHAIN="$SIGNING_KEYCHAIN"
```

Keep the login Keychain in the search list. Computer History adds its key to
the default login Keychain and reads it back through the search list, so a list
with only the signing keychain fails the history gate with
`history_key_unavailable`.

The first strict local install creates and imports the self-signed identity,
then stops without replacing the app if the new certificate is not usable yet.
Run it once, then open Keychain Access in the VM display, select the
`cua-driver-signing` keychain, open
`CuaDriver Local Signing (cua-driver-rs)`, and set Trust to Always Trust. Back
in Terminal, give Apple tooling access to the private key without storing the
keychain password in the image or repository:

```bash
cd ~/cua
export CUA_DRIVER_SOURCE_SHA="$(cat .cua-e2e-source-sha)"
bash scripts/install-local.sh \
  --release --autostart --require-stable-signing

read -r -s -p 'Keychain password: ' KEYCHAIN_PASSWORD; echo
security set-key-partition-list \
  -S apple-tool:,apple:,codesign: -s -k "$KEYCHAIN_PASSWORD" \
  "$SIGNING_KEYCHAIN"
unset KEYCHAIN_PASSWORD
```

Rerun the installer and require its `signed staged app with a stable local
identity` message before granting permissions through the app-owned flow:

```bash
bash scripts/install-local.sh \
  --release --autostart --require-stable-signing
~/.local/bin/cua-driver-local permissions grant
```

Record the certificate hash after the successful install and keep that identity
for the life of the seed:

```bash
security find-certificate \
  -c 'CuaDriver Local Signing (cua-driver-rs)' -Z "$SIGNING_KEYCHAIN"
codesign -d -r- /Applications/CuaDriverLocal.app 2>&1 \
  | grep 'certificate leaf'
```

Recreating the certificate changes the app identity and invalidates inherited
TCC grants, even when the bundle identifier stays the same.

Complete the Accessibility and Screen Recording prompts. Tahoe 26 also asks
separately for Automation and direct ScreenCaptureKit access. Trigger all
remaining consent paths before freezing the seed:

```bash
# Terminal owns the test sentinel's read-only frontmost-state probes.
osascript -e \
  'tell application "System Events" to get name of first application process whose frontmost is true'

# The installed app owns driver-side app enumeration. This uses NSWorkspace and
# must not require Automation access to System Events.
~/.local/bin/cua-driver-local list_apps '{}'

# The app-owned grant flow probes a desktop capture and triggers Tahoe's
# direct-capture/private-window prompt.
~/.local/bin/cua-driver-local permissions grant
```

Choose Allow for `Terminal` -> `System Events` and on the CuaDriverLocal
direct-capture prompt. Then add Terminal under System Settings > Privacy &
Security > Accessibility: the `bring_to_front` oracles raise fixture windows and
read `AXFocusedWindow` through System Events UI scripting, which needs it.
CuaDriverLocal app enumeration must not ask for System Events. Target-specific Automation prompts may still appear later when a user
explicitly requests an Apple Events-backed browser or app operation; do not
pre-grant those in the seed. These are normal macOS consent flows; do not edit
`TCC.db` by hand while building a reusable seed. Rerun the commands and require
them to finish without another prompt.
Notification banners can cover Tahoe's consent controls; swipe the banners away
before acting instead of clicking through them. After enabling Screen Recording,
restart the app-owned daemon once before checking status so the live process
observes the new grant:

```bash
~/.local/bin/cua-driver-local stop
open -a CuaDriverLocal
```

Then verify the daemon's own identity and the read-only status contract before
running the explicit
LaunchServices-hosted grant flow. The first command must not raise a dialog;
the second is intentionally prompt-capable and must be run by the human:

```bash
~/.local/bin/cua-driver-local permissions status --json | jq -e '
  .accessibility == true
  and .screen_recording == true
  and .screen_recording_capturable == null
  and .direct_capture_status == "not_checked"
  and .source.attribution == "driver-daemon"
'
~/.local/bin/cua-driver-local permissions grant
~/.local/bin/cua-driver-local permissions status --json | jq -e '
  .accessibility == true
  and .screen_recording == true
  and .screen_recording_capturable == null
  and .direct_capture_status == "not_checked"
  and .direct_capture_verification.source == "permissions_grant"
  and (.direct_capture_verification.verified_at | endswith("Z"))
  and .direct_capture_verification.bundle_id == "com.trycua.driver.local"
'
codesign -d -r- /Applications/CuaDriverLocal.app 2>&1 | grep 'certificate leaf'
csrutil status
```

All five commands must succeed, and `csrutil status` must report disabled.

### Seed app-owned grants in disposable SIP-off workers

Upstream's public [Run Cua Driver in a macOS Lume VM](https://cua.ai/docs/cua-driver/guides/vms-and-remote)
guide grants macOS consent through the VM display. Keep using that prompt flow
for reusable private seeds. For automated disposable workers that are not cloned
from a granted seed, run the host helper after `CuaDriverLocal.app` is installed
in each running worker with a certificate-backed identity:

```bash
WORKER_A=cua-driver-macos-e2e-a
WORKER_B=cua-driver-macos-e2e-b
tests/runners/macos-lume/seed-tcc.sh "$WORKER_A" "$WORKER_B"
```

Add `--storage <name-or-path>` when the workers live outside the default Lume
storage.

The wrapper resolves each VM's IP with `lume get`, copies
`seed-tcc-guest.sh` with `scp`, then runs it with `ssh` and guest `sudo`.
Use the default `lume` SSH password, set `LUME_SSH_PASSWORD`, or pass
`--ssh-password`. Password mode uses OpenSSH's askpass path; leave the password
empty when the VM accepts host SSH keys.
The guest helper refuses to write unless `sysctl -n hw.model` reports a
`VirtualMac*` VM and `csrutil status` reports disabled SIP. It derives the
permission identity from `/Applications/CuaDriverLocal.app`, writes only the
Accessibility and Screen Recording entries for that app, restarts `tccd`, and
verifies both entries.
It accepts a designated requirement that pins a certificate leaf or root hash
(local stable signing) or the exact Apple-anchored Developer ID requirement of a
notarized release (`anchor apple generic` with a 10-character team ID). It
rejects ad hoc `cdhash` requirements unless `--allow-adhoc` is passed. To seed
the released app, pass `--app /Applications/CuaDriver.app --expected-client
com.trycua.driver`. The host wrapper parses `lume get` with `jq`.
Database rows or a successful helper exit alone do not certify the desktop:
require both grants in `permissions status --json`, a fresh screenshot, and a
reversible input action through the guest Driver before the matrix.

After seeding, restart `CuaDriverLocal.app` before checking permission status if
`install-local --autostart` or an earlier probe may have started the daemon:

```bash
~/.local/bin/cua-driver-local stop
open -a CuaDriverLocal
```

If the VM sudo password is not the default `lume`, pass it without putting it
on the command line:

```bash
printf '%s\n' "$VM_SUDO_PASSWORD" \
  | tests/runners/macos-lume/seed-tcc.sh \
      --sudo-password-stdin "$WORKER_A" "$WORKER_B"
```

This helper is for SIP-off disposable VMs only. Keep the SIP-on permission-flow
check below as the proof that the normal user-facing consent path still works.

Stop the builder and clone it to a date/version-named private seed plus two
stopped backups:

```bash
SEED=cua-driver-macos-e2e-seed-26.5.2-YYYYMMDD
BACKUP_A="${SEED}-backup-a"
BACKUP_B="${SEED}-backup-b"
lume stop "$BUILDER"
lume clone "$BUILDER" "$SEED"
lume clone "$SEED" "$BACKUP_A"
lume clone "$SEED" "$BACKUP_B"
lume ls
```

Require the seed and both backups to show `stopped`. Treat them as immutable and
local-only. Record their names, the public base tag, macOS build (`sw_vers`),
Lume version, CLT version, Rust version, Node version, and signing-certificate
hash in the maintainer log. Also record that the following consent paths were
granted and then rerun without prompts:

- `CuaDriverLocal.app`: Accessibility and Screen Recording
- Terminal controlling System Events
- Terminal: Accessibility, for System Events UI scripting
- CuaDriverLocal direct screen capture without the system picker. macOS labels
  this combined consent as screen and system-audio access even though Cua
  Driver's current ScreenCaptureKit recorder does not enable audio capture.

Build a new seed instead of updating one in place.

## Clone a worker

Start from a clean committed host checkout. Give the worker a unique name:

```bash
SEED=cua-driver-macos-e2e-seed-26.5.2-YYYYMMDD
WORKER="cua-driver-macos-e2e-$(date -u +%Y%m%dT%H%M%SZ)"
lume clone "$SEED" "$WORKER"
echo "$WORKER"
lume run "$WORKER"
```

Keep `lume run` open. From another host terminal, set `WORKER` to the printed
name and sync the exact host commit:

```bash
WORKER=cua-driver-macos-e2e-YYYYMMDDTHHMMSSZ
VM_IP="$(lume get "$WORKER" --format json | jq -r '.[0].ipAddress')"
scripts/sync-vm-worktree.sh push "lume@${VM_IP}" '~/cua'
```

Run GUI suites from Terminal in the VM display, not over SSH: GUI fixtures must
inherit the logged-in console session. A fresh public image can leave the login
Keychain locked even after automatic GUI login, so unlock both the dedicated
signing keychain and the login Keychain after each worker boot. Use the same
local VM credential for both, and do not put it in the repository, image, logs,
or artifacts.

On macOS Tahoe, first-use Chrome can present a native local-network discovery
prompt over `chrome://inspect/#remote-debugging`. The standalone browser suite
uses loopback DevTools and does not need LAN discovery. Before freezing a seed
that will run this suite, complete Chrome's welcome screen without
signing in and leave the default-browser and usage-reporting choices disabled.
Launch Chrome on that exact page in the VM display, choose **Don't Allow**, quit
Chrome, then relaunch the page and require that the prompt does not return. Do
not answer or dismiss OS consent UI while the behavior matrix is running. If an
existing immutable seed lacks these decisions, clone it to a new versioned seed,
complete this setup there, stop it, and use that new seed for workers; never
update the original seed in place.

Pull evidence before deleting the worker, even after a failed run. Set
`REMOTE_ARTIFACT_DIR` to the directory, relative to the synced checkout, that
the tests wrote their evidence to:

```bash
REMOTE_ARTIFACT_DIR=artifacts/cua-driver/macos \
  scripts/sync-vm-worktree.sh pull-artifacts \
  "lume@${VM_IP}" '~/cua'
lume stop "$WORKER"
lume delete "$WORKER" --force
```

The host stores pulled evidence under `artifacts/cua-driver/vm/`. A setup
failure is an environment failure, not permission to report a smaller green
matrix.

## Test the seed helpers

The TCC seed helpers and the keychain setup in this guide are covered by
host-side tests that need no VM:

```bash
python -m pytest scripts/tests/test_macos_lume_runner.py -v
```

## Validate the SIP-on permission flow

Run this disposable lane before a release and after macOS, Lume, signing, or
TCC-related changes. It verifies the supported user flow rather than the
golden image's inherited grants.

1. Clone the golden seed to a new worker while both are stopped.
2. Run `lume sip on <worker> --yes`.
3. Boot it with a display, sync the exact source as above, and install it with
   `CUA_DRIVER_SOURCE_SHA` set to `.cua-e2e-source-sha`.
4. In the VM display, reset only the disposable worker's grants:

   ```bash
   tccutil reset Accessibility com.trycua.driver.local
   tccutil reset ScreenCapture com.trycua.driver.local
   ~/.local/bin/cua-driver-local permissions grant
   ```

5. Complete the prompts and require the same four-field
   `permissions status --json` check used during image creation.
6. Run `e2e_environment_preflight_test` and the macOS GUI suites to prove
   the preflight and matrix with SIP enabled.
7. Pull artifacts, stop the worker, and delete it. Never promote this mutated
   worker back to the SIP-off seed.

## Repair or rotate the seed

Discard and rebuild the private seed when its signature becomes ad-hoc, the
permission check fails, the public base or toolchain needs an update, or the
seed has been booted for a test. Keep the last known-good stopped seed until its
replacement passes one full worker run and one SIP-on permission-flow check.
