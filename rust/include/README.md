# Pincer native ABI

`pincer_abi.h` is the stable native interoperability boundary below the
public Pincer SDK. Release archives place it beside the platform shared
library.

The ABI uses versioned symbols, opaque handles, explicit buffer ownership, and
callbacks for asynchronous work. Call `pincer_abi_version_v1` and
`pincer_abi_is_compatible_v1` before creating a driver. Every returned
`PincerBuffer` must be released with `pincer_buffer_free_v1`; driver and
operation release functions accept pointer-to-pointer values and are
idempotent after clearing them.

Applications should normally use the safe Rust, Python, or TypeScript SDK.
The C ABI exists for native embedding and to keep those SDKs independent of the
private language used to implement the core.
