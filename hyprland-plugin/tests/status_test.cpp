#include "pincer_hyprland/status.hpp"

#include <cstdlib>
#include <iostream>
#include <string>

namespace {

void check(bool condition, const std::string& message) {
    if (condition)
        return;
    std::cerr << "FAIL: " << message << '\n';
    std::exit(1);
}

} // namespace

int main(int argc, char** argv) {
    const pincer::hyprland::StatusReport report{
        .plugin_version = "0.1.0",
        .compositor_epoch = 42,
        .compiled_hash = "compiled\"hash",
        .runtime_hash = "runtime\\hash",
        .configured = true,
        .transport_ready = false,
        .socket_path = "/run/user/1000/hypr/test/pincer.sock",
        .last_error = "line one\nline two",
        .discovery_enabled = false,
        .accepted_connections = 7,
        .rejected_peers = 2,
        .rejected_busy = 3,
        .timed_out_handshakes = 4,
        .timed_out_idle = 5,
        .malformed_frames = 1,
    };

    const auto json = pincer::hyprland::render_status_json(report);
    if (argc == 2 && std::string{argv[1]} == "--json") {
        std::cout << json << '\n';
        return 0;
    }
    check(
        json ==
            R"({"name":"pincer-hyprland-plugin","plugin_version":"0.1.0",)"
            R"("state":"discovery_only","protocol":{"major":2,"minor":0,)"
            R"("max_frame_bytes":4120},"compositor_epoch":42,"abi":{)"
            R"("compiled_hash":"compiled\"hash","runtime_hash":"runtime\\hash",)"
            R"("match":false},"configured":true,"transport":{"ready":false,)"
            R"("socket":"/run/user/1000/hypr/test/pincer.sock","mode":"0600",)"
            R"("peer_policy":"same_uid","last_error":"line one\nline two"},)"
            R"("capabilities":{"supported":["discovery"],"enabled":[],)"
            R"("mutation":"disabled_pending_rfc_and_agent_seat"},)"
            R"("connections":{"accepted":7,"rejected_peers":2,)"
            R"("rejected_busy":3,"timed_out_handshakes":4,)"
            R"("timed_out_idle":5,"malformed_frames":1}})",
        "status JSON is stable, escaped, and structurally complete");

    check(pincer::hyprland::render_status_text(report) ==
              "pincer-hyprland-plugin 0.1.0\nconfigured: yes\ntransport: stopped\n"
              "socket: /run/user/1000/hypr/test/pincer.sock\n"
              "last error: line one\\nline two\nprotocol: 2.0\n"
              "compositor epoch: 42\n"
              "ABI compiled: compiled\\\"hash\n"
              "ABI runtime: runtime\\\\hash\n"
              "ABI match: no\n"
              "connections: accepted=7, rejected_peers=2, rejected_busy=3, "
              "timed_out_handshakes=4, timed_out_idle=5, malformed_frames=1\n"
              "capabilities: discovery only; background mutation disabled\n",
          "plain status is stable");

    std::string unusual_error = "valid:";
    unusual_error += "\xc3\xa9";
    unusual_error += ":invalid:";
    unusual_error.push_back(static_cast<char>(0x80));
    unusual_error.push_back(static_cast<char>(0x7f));
    const auto unusual_json = pincer::hyprland::render_status_json(
        pincer::hyprland::StatusReport{
            .plugin_version = "0.1.0",
            .compiled_hash = "same",
            .runtime_hash = "same",
            .last_error = unusual_error,
        });
    check(unusual_json.find("valid:\xc3\xa9:invalid:\\u0080\\u007f") !=
              std::string::npos &&
              unusual_json.find(R"("match":true)") != std::string::npos,
          "status preserves valid UTF-8 and escapes invalid bytes");
    check(pincer::hyprland::is_valid_utf8("\xc3\xa9") &&
              !pincer::hyprland::is_valid_utf8(std::string_view{"\x80", 1}),
          "UTF-8 validation distinguishes valid and invalid runtime paths");

    std::cout << "pincer-hyprland-plugin status tests passed\n";
    return 0;
}
