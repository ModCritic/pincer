# Try the Computer History desktop preview

This guide shows you how to enable Pincer's encrypted Computer History,
inspect it, let an authorized agent read it, and remove it. It applies to
nightly macOS, Windows, and Linux builds that include the experimental preview.

Computer History is off by default. The preview records metadata for actions
performed through Pincer. It does not watch unrelated desktop activity.

## Before you start

- Use macOS, Windows, or Linux in an interactive desktop session.
- Use a build that includes Computer History. For Pincer, build from source;
  see the [README](../README.md#build-from-source).
- Grant Pincer the usual permissions required for the actions you want it
  to perform. Computer History does not add permission to perform an action.
- On Linux, run a desktop Secret Service implementation such as GNOME
  Keyring. The preview fails closed if no unlocked Secret Service is available.

## Install or switch to the nightly channel

> [!NOTE]
> The commands in this section run upstream Cua's installer from cua.ai and
> install upstream Cua Driver, not Pincer. To try Computer History with this
> fork, build it from source as described in the
> [README](../README.md#build-from-source).

For a fresh macOS or Linux installation, run:

```bash
/bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)" -- --channel nightly
```

For a fresh Windows installation, run in PowerShell:

```powershell
& ([scriptblock]::Create((irm https://cua.ai/driver/install.ps1))) -Channel nightly
```

This build has no `channel` or `update` command. To switch an existing
installation, run the installer above again with the nightly channel option.

Confirm the installed version:

```bash
pincer --version
```

## Enable Computer History

Run:

```bash
pincer history enable
```

The command admits the experimental feature, initializes its device-local
native credential, verifies an encrypted write and read, and then enables
capture. If the installed daemon must restart, Pincer preserves its
existing permission mode, capability manifest, approval flags, compatibility
mode, and launch grants.

Check the result:

```bash
pincer history status
```

The default retention period is 7 days and the default encrypted-store quota
is 100 MiB. Queries never return events older than 7 days. The writer seals a
live chunk at least hourly and prunes sealed chunks while it is running. If
history is disabled or the daemon is offline, expired ciphertext can remain on
disk until the next enable or query checkpoint, although it is never returned
after the query-visible cutoff.

## Inspect history locally

List recent events:

```bash
pincer history list 50
```

Show one event by sequence number:

```bash
pincer history show 42
```

Use `--json` with `status`, `list`, or `show` when another local program needs
structured output.

## Let an agent hydrate from history

Connect the agent through the normal Pincer tool path. The preview exposes
two read-only tools:

- `history_status`, which requires `history.status`;
- `history_query`, which requires `history.query`.

The existing Pincer permission mode, policy ceiling, and capability manifest make
the decision for each call. A permitted query returns at most 200 metadata
events. The result can enter the agent's model context, and the encrypted store
records a fixed access-audit event.

The agent cannot enable, pause, resume, disable, delete, export, or obtain the
encryption key through these tools.

## Pause, resume, or disable capture

Pause capture without clearing the enabled preference:

```bash
pincer history pause
```

Resume it:

```bash
pincer history resume
```

Disable capture and stop the writer:

```bash
pincer history disable
```

These commands preserve existing encrypted history.

## Delete Computer History

Delete the encrypted store and destroy its exact namespace native key:

```bash
pincer history delete --yes
```

This is cryptographic deletion. Pincer does not claim physical erasure from
APFS snapshots, backups, copied ciphertext, SSD wear leveling, or memory that a
process already decrypted.

The uninstall commands below run upstream Cua's uninstaller from cua.ai.
They remove an upstream installation and do not apply to a source build of
Pincer.

To purge history while uninstalling on macOS or Linux, run:

```bash
/bin/bash -c "$(curl -fsSL https://cua.ai/driver/uninstall.sh)" -- --purge
```

On Windows, run:

```powershell
$env:PINCER_UNINSTALL_FORCE = '1'
$env:PINCER_UNINSTALL_PURGE = '1'
irm https://cua.ai/driver/uninstall.ps1 | iex
```

Run the uninstaller as the interactive login user. Do not prefix the Unix
uninstaller with `sudo`: native credential and history cleanup must remain in
that user's context.

## Return to the stable channel

Disable capture before switching binaries:

```bash
pincer history disable
```

Then run the installer again without the nightly channel option.

Switching channels does not delete encrypted history. A stable build that does
not understand the preview store must leave it untouched.

## What the preview stores

The encrypted event allowlist contains:

- time and monotonic sequence;
- opaque session and action identifiers;
- a fixed Pincer capability;
- an optional fixed-field platform application identifier and display name;
- fixed action outcome, delivery, route, evidence, and escalation categories;
- lifecycle, access-audit, and writer-health events.

The preview never stores screenshots, video, audio, typed text, raw keystrokes,
clipboard contents, raw tool arguments or results, accessibility trees, file
paths, window titles, URLs, or free-form diagnostics.

Files use the Pincer History Profile: a CBOR Sequence of COSE_Encrypt0 records with
CloudEvents JSON inside each encrypted payload. ChaCha20-Poly1305 authenticates
each record. A namespace root key protected by macOS Keychain, Windows
Credential Manager, or Linux Secret Service and per-chunk HKDF keys protect
data at rest. There is no plaintext fallback and history performs no network
I/O.

The filesystem can still reveal that a history directory exists, its total
size, and file modification times. The native key is bound to the current
user's credential store, so copying only the encrypted files to another
machine does not provide recovery.

Default encrypted-store locations are:

- macOS: `~/Library/Application Support/pincer/computer-history`;
- Windows: `%LOCALAPPDATA%\pincer\computer-history`;
- Linux: `$XDG_STATE_HOME/pincer/computer-history`, or
  `~/.local/state/pincer/computer-history` when `XDG_STATE_HOME` is unset.

Local-development installs use `pincer-local` instead of `pincer` in
both filesystem and native-key namespaces.

## Troubleshooting

**`history_preview_not_admitted`**

The running local-development daemon was not started with the preview flag.
Restart that daemon with `pincer serve --experimental-history`, then run
`pincer history enable` again. Installed nightly builds handle the verified
relaunch automatically.

**`history_key_locked` or `history_key_unavailable`**

Unlock the login session and its native credential store (Keychain, Credential
Manager, or Secret Service), then retry. On Linux, also confirm that a Secret
Service implementation is running. Capture stays disabled and Pincer does
not create plaintext files.

**`history_quota_reached`**

Delete history if you no longer need it. The preview does not allow unbounded
growth and does not block the computer action that encountered the full store.

**`history_storage_corrupt`**

The reader found an invalid, incomplete, reordered, or unauthenticated record.
It refuses the affected store instead of repairing or skipping evidence. If
you accept permanent data loss, `pincer history delete --yes` destroys the
namespace key and removes the corrupt encrypted store so capture can start
fresh.

## Architecture and format

See [Computer History for Pincer](computer-history-architecture.md) for the
staged architecture, security limits, storage profile, launch gates, and later
NVIDIA OpenShell integration plan.
