/**
 * Rust-backed SDK for Pincer client applications.
 *
 * Agents should configure `pincer mcp` through their runtime's existing
 * MCP client instead of importing a language MCP facade.
 */
import {
  Pincer as NativePincer,
  type PincerLike,
  type DriverOptions,
  SdkClientKind,
} from "./native/pincer_sdk.js"
import { createDriver } from "./daemon.js"

/**
 * Options for `Pincer.create()`. `embedded: true` forces the in-process
 * runtime on macOS, where `create()` otherwise prefers the Pincer.app
 * daemon so the agent cursor overlay and the app's permissions work.
 */
export type CreateOptions = DriverOptions & { embedded?: boolean }

const Pincer = NativePincer as typeof NativePincer & {
  create(options?: CreateOptions): PincerLike
}
type Pincer = NativePincer

// The same native library backs Python and TypeScript. The package root tags
// both the canonical same-process constructor and the temporary daemon
// compatibility constructor with the importing runtime.
NativePincer.create = (options?: CreateOptions) => {
  const { embedded, ...driverOptions } = options ?? {}
  return createDriver(
    () => NativePincer.connectWithClientKind(undefined, SdkClientKind.Typescript),
    () =>
      NativePincer.createWithClientKind(
        options === undefined ? undefined : (driverOptions as DriverOptions),
        SdkClientKind.Typescript,
      ),
    embedded === true,
    (driverOptions as Partial<DriverOptions>).claudeCodeCompatibility === true,
  )
}
NativePincer.createConfigured = (options) =>
  NativePincer.createConfiguredWithClientKind(options, SdkClientKind.Typescript)
NativePincer.createConfiguredWithAuthorizationHost = (options, host) =>
  NativePincer.createConfiguredWithAuthorizationHostAndClientKind(
    options,
    host,
    SdkClientKind.Typescript,
  )
NativePincer.createConfiguredWithActivityObserver = (options, observer) =>
  NativePincer.createConfiguredWithActivityObserverAndClientKind(
    options,
    observer,
    SdkClientKind.Typescript,
  )
NativePincer.createConfiguredWithHostIntegrations = (options, host, observer) =>
  NativePincer.createConfiguredWithHostIntegrationsAndClientKind(
    options,
    host,
    observer,
    SdkClientKind.Typescript,
  )
NativePincer.createPrivateWorker = (options) =>
  NativePincer.createPrivateWorkerWithClientKind(options, SdkClientKind.Typescript)
NativePincer.connect = (socketPath: string | undefined) =>
  NativePincer.connectWithClientKind(socketPath, SdkClientKind.Typescript)

export * from "./native/index.js"
export { Pincer }
export { default } from "./native/index.js"
