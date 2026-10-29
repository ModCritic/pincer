import assert from "node:assert/strict"
import { existsSync } from "node:fs"
import path from "node:path"
import { test } from "node:test"
import { fileURLToPath } from "node:url"

const testDirectory = path.dirname(fileURLToPath(import.meta.url))
const libraryName =
  process.platform === "darwin"
    ? "libpincer_sdk.dylib"
    : process.platform === "win32"
      ? "pincer_sdk.dll"
      : "libpincer_sdk.so"
const nodeTriple =
  process.platform === "darwin"
    ? `darwin-${process.arch}`
    : process.platform === "win32"
      ? `win32-${process.arch}-msvc`
      : `linux-${process.arch}-${process.report.getReport().header.glibcVersionRuntime ? "gnu" : "musl"}`
const library = path.resolve(
  testDirectory,
  "../node_modules",
  `pincer-driver-${nodeTriple}`,
  libraryName,
)

test(
  "embedded subpath is the same generated Rust host as the SDK root",
  { skip: !existsSync(library) },
  async () => {
    const root = await import("pincer-driver")
    const embedded = await import("pincer-driver/embedded")

    assert.equal(embedded.EmbeddedPincerHost, root.EmbeddedPincerHost)
    assert.equal(embedded.EmbeddedDriverHostOptions, root.EmbeddedDriverHostOptions)
    assert.equal(embedded.EmbeddedPermissionMode, root.EmbeddedPermissionMode)

    const requiredOptions = {
      binaryPath: "/example/pincer",
      hostBundleId: "com.example.host",
      approveSessionPolicy: false,
      dangerouslyBypassApprovals: false,
      environment: [],
      inheritStderr: false,
    }
    assert.equal(
      embedded.EmbeddedDriverHostOptions.new(requiredOptions).noOverlay,
      false,
    )
    assert.equal(
      embedded.EmbeddedDriverHostOptions.new({
        ...requiredOptions,
        noOverlay: true,
      }).noOverlay,
      true,
    )

    assert.throws(
      () => new embedded.EmbeddedPincerHost("", "com.example.host"),
      error => error?.inner?.reason === "binary_path must not be empty",
    )

    assert.throws(
      () =>
        embedded.EmbeddedPincerHost.withOptions(
          embedded.EmbeddedDriverHostOptions.new({
            binaryPath: "/example/pincer",
            hostBundleId: "com.example.host",
            approveSessionPolicy: false,
            dangerouslyBypassApprovals: false,
            environment: [
              embedded.EmbeddedEnvironmentVariable.new({
                name: "PINCER_PERMISSION_MODE",
                value: "unrestricted",
              }),
            ],
            inheritStderr: false,
          }),
        ),
      error => error?.inner?.reason?.includes("safe allowlist") === true,
    )
  },
)
