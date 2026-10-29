#pragma once

#if __cplusplus <= 202302L
#error The plugin API probe must compile in C++26 mode
#endif
#ifndef PINCER_PROBE_FLAG
#error The plugin API probe must propagate pkg-config flags
#endif

#if defined(PINCER_PROBE_socket1)
namespace IPC::Socket1 {
struct SCommand {};
}
#elif defined(PINCER_PROBE_legacy)
struct SHyprCtlCommand {};
#endif
