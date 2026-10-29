/* SPDX-License-Identifier: MIT */

#ifndef PINCER_ABI_H
#define PINCER_ABI_H

/* Generated from pincer-sdk/src/abi.rs by pincer-abi-header. Do not edit. */

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#if defined(_WIN32)
#if defined(PINCER_ABI_BUILD)
#define PINCER_API __declspec(dllexport)
#else
#define PINCER_API __declspec(dllimport)
#endif
#else
#define PINCER_API __attribute__((visibility("default")))
#endif


#define PINCER_ABI_MAJOR 1

#define PINCER_ABI_MINOR 1

#define PINCER_ABI_PATCH 0

#define DRIVER_ENVELOPE_VERSION 1

#define PRIVATE_WORKER_PROTOCOL_VERSION 1

/**
 * Stable status codes returned across the C ABI.
 */
enum PincerStatus
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
  PINCER_STATUS_OK = 0,
  PINCER_STATUS_INVALID_ARGUMENT = 1,
  PINCER_STATUS_NULL_POINTER = 2,
  PINCER_STATUS_RUNTIME_UNAVAILABLE = 3,
  PINCER_STATUS_SHUTDOWN = 4,
  PINCER_STATUS_CANCELLED = 5,
  PINCER_STATUS_INTERNAL = 6,
  PINCER_STATUS_PANIC = 7,
  PINCER_STATUS_RUNTIME_CONFLICT = 8,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum PincerStatus PincerStatus;
#else
typedef int32_t PincerStatus;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/**
 * Opaque driver runtime handle.
 */
typedef struct PincerHandle PincerHandle;

/**
 * Opaque token for one asynchronous operation.
 */
typedef struct PincerOperation PincerOperation;

/**
 * Opaque session handle whose actions are already bound to one immutable
 * authorization context.
 */
typedef struct PincerSessionHandle PincerSessionHandle;

/**
 * Version of the implementation-neutral Pincer ABI.
 */
typedef struct {
  uint32_t struct_size;
  uint16_t major;
  uint16_t minor;
  uint16_t patch;
  uint16_t reserved;
} PincerAbiVersion;

/**
 * Caller-owned byte buffer returned by the ABI.
 * Pass it to `pincer_buffer_free_v1`; freeing an empty buffer is harmless.
 */
typedef struct {
  uint8_t *data;
  size_t len;
  size_t capacity;
} PincerBuffer;

/**
 * Completion callback for asynchronous operations. It is called exactly once
 * unless the process terminates. Result and error buffers are caller-owned.
 */
typedef void (*PincerCompletionV1)(void *context,
                                   PincerStatus status,
                                   PincerBuffer result,
                                   PincerBuffer error);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Read the runtime ABI version into `out_version`.
 */
PINCER_API PincerStatus pincer_abi_version_v1(PincerAbiVersion *out_version);

/**
 * Return whether this runtime supports a caller compiled for `major.minor`.
 */
PINCER_API bool pincer_abi_is_compatible_v1(uint16_t major, uint16_t minor);

/**
 * Free and clear a buffer returned by this ABI. Repeated calls are harmless.
 */
PINCER_API void pincer_buffer_free_v1(PincerBuffer *buffer);

/**
 * Create an in-process driver. `options_json` is empty or a UTF-8 JSON object.
 * It accepts `claude_code_compatibility` and an optional immutable
 * `authorization` ceiling. Unknown fields fail closed.
 */
PINCER_API
PincerStatus pincer_create_v1(const uint8_t *options_json,
                              size_t options_len,
                              PincerHandle **out_handle,
                              PincerBuffer *out_error);

/**
 * Destroy and clear a driver handle. Repeated calls are harmless.
 */
PINCER_API void pincer_destroy_v1(PincerHandle **handle);

/**
 * Return whether the driver still accepts operations.
 */
PINCER_API
PincerStatus pincer_is_available_v1(PincerHandle *handle,
                                    bool *out_available,
                                    PincerBuffer *out_error);

/**
 * Return driver metadata as caller-owned UTF-8 JSON.
 */
PINCER_API
PincerStatus pincer_metadata_json_v1(PincerHandle *handle,
                                     PincerBuffer *out_json,
                                     PincerBuffer *out_error);

/**
 * Return the canonical SDK/MCP tool inventory as caller-owned UTF-8 JSON.
 */
PINCER_API
PincerStatus pincer_list_tools_json_v1(PincerHandle *handle,
                                       PincerBuffer *out_json,
                                       PincerBuffer *out_error);

/**
 * Invoke a named tool asynchronously. Cancellation is requested with
 * `pincer_operation_cancel_v1`; release the token after completion.
 */
PINCER_API
PincerStatus pincer_invoke_v1(PincerHandle *handle,
                              const uint8_t *name,
                              size_t name_len,
                              const uint8_t *arguments_json,
                              size_t arguments_len,
                              PincerCompletionV1 callback,
                              void *context,
                              PincerOperation **out_operation,
                              PincerBuffer *out_error);

/**
 * Create a trusted, immutable session binding below this runtime's ceiling.
 *
 * This is a host API, not an agent tool. The returned handle carries its
 * authority in memory and cannot be reconstructed from a public session ID.
 */
PINCER_API
PincerStatus pincer_session_create_v1(PincerHandle *handle,
                                      const uint8_t *options_json,
                                      size_t options_len,
                                      PincerSessionHandle **out_session,
                                      PincerBuffer *out_error);

/**
 * Destroy a trusted session handle and revoke its connection-bound grants.
 */
PINCER_API void pincer_session_destroy_v1(PincerSessionHandle **handle);

/**
 * Invoke through an already-bound trusted session context.
 */
PINCER_API
PincerStatus pincer_session_invoke_v1(PincerSessionHandle *handle,
                                      const uint8_t *name,
                                      size_t name_len,
                                      const uint8_t *arguments_json,
                                      size_t arguments_len,
                                      PincerCompletionV1 callback,
                                      void *context,
                                      PincerOperation **out_operation,
                                      PincerBuffer *out_error);

/**
 * Stop admission, drain admitted calls, and finalize SDK-owned resources.
 */
PINCER_API
PincerStatus pincer_shutdown_v1(PincerHandle *handle,
                                PincerCompletionV1 callback,
                                void *context,
                                PincerOperation **out_operation,
                                PincerBuffer *out_error);

/**
 * Request cancellation of an asynchronous operation.
 */
PINCER_API void pincer_operation_cancel_v1(PincerOperation *operation);

/**
 * Release and clear an operation token. Repeated calls are harmless.
 */
PINCER_API void pincer_operation_release_v1(PincerOperation **operation);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* PINCER_ABI_H */
