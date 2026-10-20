# cua-driver Python SDK

Rust-backed Python SDK and bundled executable for
[Pincer](https://github.com/ModCritic/pincer), a fork of
[Cua Driver](https://github.com/trycua/cua/tree/main/libs/cua-driver).

## Product boundary

This package is for client applications importing Cua Driver as an SDK:

```python
from cua_driver import CuaDriver
```

It does not contain a Python MCP protocol implementation. The optional
`cua_driver.fleet` module forwards service bytes to the shared Rust typed-MCP
client. See [the candidate Fleet connection guide](../docs/shared-fleet-mcp-client.md)
for prerequisites, ownership, and release limits. Agents already have runtime-neutral
MCP clients and should configure the bundled server directly:

```text
cua-driver mcp
```

The removed pre-release MCP facade used `CuaDriver.stdio()`,
`AsyncCuaDriver`, `*Args`, and transport classes. Application code imports the
typed Rust-backed SDK shown below; agent code supplies `cua-driver mcp` to its
agent SDK.

## Installation

This fork publishes no wheels. The `cua-driver` name in `pyproject.toml` is
upstream's, so a `cua-driver` package installed from PyPI is not this fork.

To build a wheel yourself, first build the driver and the SDK library from
source as described in the [top-level README](../README.md#build-from-source),
adding the `cua-driver-sdk` package. From `rust/`:

```bash
cargo build --release -p cua-driver -p cua-driver-sdk
```

On Windows, add `-p cua-driver-uia` to also bundle the UIAccess worker. Then
run `python build_wheel.py` from this directory. It copies the binary and the
library from `../rust/target/release` (or `$CARGO_TARGET_DIR/release`) into
`src/cua_driver/` and builds the wheel with `python -m build`, so the `build`
package must be installed. Use `--target-dir`, `--binary`, or
`--native-library` to take them from somewhere else. The script never
downloads anything.

The wheel's platform tag comes from the build. The architecture is read from
the binary, or taken from the host if the binary's can't be read. On Linux the
tag is `manylinux_<glibc>_<arch>` with the glibc version of the machine that
runs the script, since a local build only runs where the glibc is at least as
new as where it was built. On macOS it is `macosx_13_0_arm64`, `_x86_64`, or
`_universal2` for a universal binary; 13.0 matches `MACOSX_DEPLOYMENT_TARGET`
in `rust/.cargo/config.toml`. On Windows it is `win_amd64` or `win_arm64`. To
set the tag yourself, for example after building against an older glibc, pass
`--plat-name manylinux_2_31_x86_64`.

The wheel contains generated UniFFI bindings, a platform-specific Rust SDK
library, and the `cua-driver` executable. `CuaDriver.create()` loads the runtime
in the importing process and does not require the executable or daemon.

## SDK example

```python
import asyncio

from cua_driver import (
    CuaDriver,
    CursorReducedMotion,
    EndSessionInput,
    GetDesktopStateInput,
    SetAgentCursorThemeInput,
    StartSessionInput,
)

async def main() -> None:
    driver = CuaDriver.create()
    await driver.start_session(
        StartSessionInput(session="demo", capture_scope=None, cursor_theme=None)
    )
    try:
        await driver.set_agent_cursor_theme(
            SetAgentCursorThemeInput(
                session="demo",
                theme_id="cua.default",
                reduced_motion=CursorReducedMotion.AUTO,
            )
        )
        desktop = await driver.get_desktop_state(
            GetDesktopStateInput(session="demo", screenshot_out_file=None)
        )
        print(desktop.images[0].mime_type)
    finally:
        await driver.end_session(EndSessionInput(session="demo"))
        await driver.shutdown()


asyncio.run(main())
```

SDK operations are asynchronous. Desktop observations return a typed `ToolResult` with
text, images, verification/error metadata, and `structured_json` / `raw_json`
for platform-extensible results. Session lifecycle calls return dedicated
generated records.

`start_session` is optional for ordinary calls. The runtime creates one
implicit session for this SDK transport and reuses it until shutdown, explicit
end, or five minutes of inactivity. Use a named session when application code
needs to configure or inspect that run explicitly.

The agent cursor is session-owned and initializes on the first cursor-bearing
action, including `move_cursor`. Its default theme and custom dotLottie
authoring workflow are documented in
[`docs/cursor-themes.md`](../docs/cursor-themes.md). Custom source is compiled
and installed with the local CLI; SDK and MCP tools select only an installed
theme ID. The built-in cursor shows the sanitized public session name in a
badge below the pointer.

## Typed native-window migration

The next breaking release adds typed app and window discovery, window snapshots,
and token-based clicks. Version 0.25 supports native-window operations through
the generic tool surface; it does not expose this typed window API. Upgrade the
bindings and native library together.

`list_apps` returns `ListAppsOutput`, `list_windows` returns `ListWindowsOutput`,
and `get_window_state` returns `WindowStateOutput`. `click` takes a required exact
target, a coordinate or element-token position, and an explicit delivery mode.
It returns `ActionResult` directly and raises `DriverError.Tool` on refusal.
Other action methods retain `ToolResult`.

Select a unique app and window, resolve an element from a fresh snapshot, request
background delivery explicitly, then capture again to verify the intended UI
change. An unsupported background route must not trigger an automatic foreground
retry. Refresh stale tokens from the same exact window.

See the [migration guide](../docs/native-window-sdk-migration.md) for input and
return-type changes, and upstream's [complete Python and TypeScript examples](https://cua.ai/docs/cua-driver/guides/use-the-sdk)
for discovery, token selection, verification, and shutdown.

## Authorization integrations

`standard` is promptless for normal automation. An application that needs to
authorize attachment to an existing logged-in Chromium profile can construct a
configured runtime with
`CuaDriver.create_configured_with_authorization_host(options, host)`.
Implement `DriverAuthorizationHost.authorize()` in trusted application code
and return the request's exact digest with `ALLOW`, `DENY`, or `CANCEL`.

`CuaDriver.create_configured_with_activity_observer(options, observer)` emits
content-free action, refusal, grant, and session events. The observer cannot
change authorization or tool results. Use
`create_configured_with_host_integrations` when the application needs both.

See upstream's [SDK reference](https://cua.ai/docs/cua-driver/guides/use-the-sdk#constructors)
for complete examples and the callback trust rules.

`CuaDriver.connect(socket_path)` remains available while existing applications
migrate. It exposes the same methods over the installed daemon, but it does not
provide a second SDK contract.

`shutdown()` closes admission, waits for already admitted operations to finish,
and is idempotent. Calls started after shutdown fail with `DriverError.Shutdown`.
Destroying a binding handle releases native resources, but orderly applications
should still await `shutdown()`.

## Daemon-backed MCP host

Applications that must also expose MCP to an external agent can own a private
daemon child. The child provides a stable permission identity and session
lifetime for short-lived or external clients:

```python
import asyncio

from cua_driver import CuaDriver, EmbeddedCuaDriverHost, get_binary_path


async def main() -> None:
    host = EmbeddedCuaDriverHost(
        binary_path=str(get_binary_path()),
        host_bundle_id="com.example.your-app",
    )
    connection = await host.start()
    driver = CuaDriver.connect(connection.socket_path)
    try:
        # Application calls use driver. An agent runtime can launch
        # connection.mcp.command with connection.mcp.args and environment.
        print(await driver.metadata())
    finally:
        del driver
        await host.stop()


asyncio.run(main())
```

`start()` coalesces concurrent callers, `stop()` cancels startup and is
idempotent, and `restart()` returns a new generation/PID/endpoint. Destroy SDK
clients and MCP proxies before stopping or restarting, then reconnect from the
new connection. `wait_for_exit(connection.generation)` observes unexpected
termination. Dropping the host closes its parent-liveness pipe and kills the
child as a fallback, but orderly applications should still await `stop()`.

## Binary wrapper

The package also exposes the bundled executable:

```python
from cua_driver import get_binary_path, run_cua_driver

print(get_binary_path())
exit_code = run_cua_driver(["mcp"])
```

## Platform support

| Platform | Architecture | Status |
| --- | --- | --- |
| macOS 13+ | ARM64, x86_64, or universal | Supported |
| Linux | x86_64 | Supported |
| Windows | x86_64 | Supported |
| Windows | ARM64 | Supported |

## License

MIT License: see [LICENSE](https://github.com/ModCritic/pincer/blob/main/LICENSE).
