#!/usr/bin/env bash
# Run the ignored tests that need an X server, a D-Bus session bus, AT-SPI or
# a Secret Service, against servers this script starts for them and nobody
# else uses: an Xvfb, a session bus, the accessibility bus with its registry,
# and gnome-keyring. The x11 job in .github/workflows/ci.yml runs it; it runs
# the same way locally.
#
# Tests that drive the X server DISPLAY names refuse to run unless
# PINCER_TEST_THROWAWAY_DISPLAY names it too (crates/pincer-testkit/src/
# throwaway_display.rs). This script sets it for its own Xvfb only.
#
# Locally, run it where it cannot reach your desktop: its own network
# namespace (no abstract X sockets) and its own /tmp/.X11-unix, from rust/:
#   unshare -rn --mount sh -c 'mount -t tmpfs -o mode=1777 tmpfs /tmp/.X11-unix &&
#     exec unshare --map-user="$1" --map-group="$2" --pid --fork --mount-proc "$3"' \
#     sh "$(id -u)" "$(id -g)" scripts/ci-x11-tests.sh
# cargo has no network there: build first (the same command outside the
# namespace with CI_X11_BUILD_ONLY=1), or set CARGO_NET_OFFLINE=true.
#
# Needs Xvfb, dbus-daemon, at-spi2-core, gnome-keyring and ImageMagick's
# `import` (Ubuntu: xvfb dbus at-spi2-core gnome-keyring imagemagick).
# CARGO names the cargo command (default: cargo). Test names as arguments run
# only those platform-linux lib tests (to rerun one locally); CI passes none.
set -euo pipefail
cd "$(dirname "$0")/.."
CARGO=${CARGO:-cargo}

# Ignored platform-linux lib tests this lane runs.
lib_tests=(
    atspi::native::hit_test::tests::x11_point_cover_names_the_window_on_top
    capture::tests::desktop_capture_does_not_wait_on_a_hung_import
    capture::tests::live_directcolor_capture_tracks_window_colormap
    capture::tests::live_xshm_matches_xgetimage_across_resize_reconnect_and_repetition
    clipboard::tests::an_unowned_x11_clipboard_reads_as_empty
    clipboard::tests::native_clipboard_round_trips_text_on_x11
    history::tests::secret_service_key_lifecycle_is_isolated_corruptible_and_destroyable
    input::xlib_live_tests::a_broken_connection_releases_held_buttons_before_the_exit
    input::xlib_live_tests::an_early_refusal_closes_its_display
    input::xlib_live_tests::an_x_error_does_not_end_the_process
    overlay::tests::x11_desktop_capture_hides_the_resting_overlay_and_restores_it
    overlay::tests::x11_overlay_input_shape_stays_empty_and_clicks_pass_through_after_resize
    overlay::tests::x11_overlay_owner_stays_click_through_across_cursor_lifecycle_and_randr
    tools::impl_::held_button_live_tests::a_held_button_is_released_after_its_window_closed
    x11::tests::shared_query_connection_survives_restart_and_display_change
)
# Ignored platform-linux lib tests this lane leaves out, and why.
lib_left_out=(
    # A writable /dev/uinput: root or a udev rule on the runner.
    input::path_tests::real_uinput_accepts_normalized_overlong_multibyte_name
    # A GTK entry, button and terminal at coordinates the harness passes in.
    pointer_shape::tests::live_x11_hit_test_system_shape_and_probe
    # A Wayland compositor with a focused native surface, and wtype.
    wayland::tests::standalone_modifiers_reach_wayland_keyboard
    # Not a test of its own: a_process_a_descendant_started_from_a_second_thread_is_listed
    # runs it in a child copy of the test binary, and alone it returns at once.
    tools::impl_::launch_child::tests::second_thread_starts_a_sleeper
)
# Integration test targets whose ignored tests all run here.
linux_targets=(short_connection_x11 key_input_x11 click_input_x11)

build_args=(--locked -p platform-linux --lib)
for target in "${linux_targets[@]}"; do build_args+=(--test "$target"); done
if [[ ${CI_X11_BUILD_ONLY:-} == 1 ]]; then
    "$CARGO" test "${build_args[@]}" --no-run
    "$CARGO" test --locked -p pincer --test atspi_listener_startup_test --no-run
    exit 0
fi

missing=()
find_tool() {
    local name=$1 dir
    shift
    for dir in "$@"; do
        [[ -x $dir/$name ]] && { echo "$dir/$name"; return; }
    done
    command -v "$name" || true
}
for tool in Xvfb dbus-daemon dbus-send gnome-keyring-daemon; do
    command -v "$tool" >/dev/null || missing+=("$tool")
done
# ImageMagick is the pixel oracle of one capture test; a local run may go on
# without it and see that test fail.
if ! command -v import >/dev/null; then
    if [[ -n ${CI:-} ]]; then
        missing+=(import)
    else
        echo "ci-x11-tests: no ImageMagick import; live_xshm_matches_xgetimage... will fail" >&2
    fi
fi
libexec=(/usr/libexec /usr/lib/at-spi2-core /usr/lib)
bus_launcher=$(find_tool at-spi-bus-launcher "${libexec[@]}")
registryd=$(find_tool at-spi2-registryd "${libexec[@]}")
[[ -n $bus_launcher ]] || missing+=(at-spi-bus-launcher)
[[ -n $registryd ]] || missing+=(at-spi2-registryd)
if ((${#missing[@]})); then
    echo "ci-x11-tests: missing ${missing[*]}" >&2
    echo "  (Ubuntu: apt-get install xvfb dbus at-spi2-core gnome-keyring imagemagick)" >&2
    exit 2
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/pincer-x11-tests.XXXXXX")
pids=()
cleanup() {
    local i
    for ((i = ${#pids[@]} - 1; i >= 0; i--)); do kill "${pids[i]}" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# Xvfb on the first display number with neither a lock file nor a socket.
# Not -displayfd: a server started with it writes no lock file, so Pincer
# (which reads /tmp/.X<n>-lock to recognize Xvfb) would take it for a real
# desktop, and Xvfb's own search can land on a number whose server has no
# lock file either (Xorg under SDDM) and unlink that server's socket. Ready
# once its socket exists and its lock names it (-noreset: a client that
# connects early is answered after startup, and the server does not reset
# when the last client leaves). Not SIGUSR1: the server sends none when its
# parent is PID 1, which this script is in a PID namespace.
display_number=
for ((n = 120; n < 340; n++)); do
    [[ -e /tmp/.X$n-lock || -e /tmp/.X11-unix/X$n ]] && continue
    Xvfb ":$n" -screen 0 1280x1024x24 -nolisten tcp -noreset 2>"$work/xvfb.log" &
    pid=$!
    for _ in $(seq 400); do
        kill -0 "$pid" 2>/dev/null || break
        if [[ -S /tmp/.X11-unix/X$n && $(tr -d ' \n' <"/tmp/.X$n-lock" 2>/dev/null) == "$pid" ]]; then
            pids+=("$pid")
            display_number=$n
            break 2
        fi
        sleep 0.05
    done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
done
[[ -n $display_number ]] || { echo "ci-x11-tests: Xvfb did not start" >&2; cat "$work/xvfb.log" >&2; exit 1; }

# A plain X11 session, whatever desktop the script was started from: no
# Wayland, and no desktop name (Pincer turns some routes off on KDE X11).
unset WAYLAND_DISPLAY WAYLAND_SOCKET NO_AT_BRIDGE XAUTHORITY \
    XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP DESKTOP_SESSION KDE_FULL_SESSION \
    KDE_SESSION_VERSION KDE_SESSION_UID GNOME_DESKTOP_SESSION_ID \
    XDG_SESSION_ID XDG_SESSION_PATH XDG_SEAT XDG_SEAT_PATH XDG_VTNR
export DISPLAY=:$display_number PINCER_TEST_THROWAWAY_DISPLAY=:$display_number
export XDG_SESSION_TYPE=x11
export XDG_RUNTIME_DIR=$work/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
# Nothing here may reach the host's system bus.
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent/system_bus_socket
export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus
dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --nofork --nopidfile \
    >"$work/dbus.log" 2>&1 &
pids+=($!)
for _ in $(seq 100); do [[ -S $XDG_RUNTIME_DIR/bus ]] && break; sleep 0.05; done
[[ -S $XDG_RUNTIME_DIR/bus ]] || { echo "ci-x11-tests: no session bus" >&2; cat "$work/dbus.log" >&2; exit 1; }

# The accessibility bus, then its registry, started by hand: activation could
# hand the registry to the host's systemd.
"$bus_launcher" --launch-immediately >"$work/a11y-bus.log" 2>&1 &
pids+=($!)
registry_up=
for attempt in 1 2 3 4 5; do
    "$registryd" >>"$work/registryd.log" 2>&1 &
    pid=$!
    sleep 1
    if kill -0 "$pid" 2>/dev/null; then
        pids+=("$pid")
        registry_up=1
        break
    fi
    echo "registryd attempt $attempt exited" >>"$work/registryd.log"
done
[[ -n $registry_up ]] || { echo "ci-x11-tests: no AT-SPI registry" >&2; cat "$work"/a11y-bus.log "$work/registryd.log" >&2; exit 1; }

# A Secret Service of its own whose default collection is an unencrypted
# keyring (empty password), unlocked without a prompt.
keyrings=$work/data/keyrings
mkdir -p "$keyrings"
printf '[keyring]\ndisplay-name=ci\nctime=0\nmtime=0\nlock-on-idle=false\nlock-after=false\n' \
    >"$keyrings/ci.keyring"
printf ci >"$keyrings/default"
chmod 600 "$keyrings"/*
XDG_DATA_HOME=$work/data gnome-keyring-daemon --foreground --components=secrets \
    </dev/null >"$work/keyring.log" 2>&1 &
pids+=($!)
secrets() {
    dbus-send --session --print-reply --dest=org.freedesktop.secrets "$@"
}
for _ in $(seq 100); do
    secrets /org/freedesktop/secrets org.freedesktop.Secret.Service.Unlock \
        array:objpath:/org/freedesktop/secrets/collection/ci >/dev/null 2>&1 && break
    sleep 0.05
done
secrets /org/freedesktop/secrets/collection/ci org.freedesktop.DBus.Properties.Get \
    string:org.freedesktop.Secret.Collection string:Locked 2>/dev/null | grep -q 'boolean false' ||
    { echo "ci-x11-tests: the keyring did not unlock" >&2; cat "$work/keyring.log" >&2; exit 1; }

echo "ci-x11-tests: DISPLAY=$DISPLAY, private session and accessibility buses, unlocked keyring"

# The lists above must match the crate's ignored tests, so a new one is
# either run here or left out on purpose.
ignored=$("$CARGO" test --locked -p platform-linux --lib -- --list --ignored --format terse |
    sed -n 's/: test$//p' | sort)
known=$(printf '%s\n' "${lib_tests[@]}" "${lib_left_out[@]}" | sort)
if [[ $ignored != "$known" ]]; then
    echo "ci-x11-tests: platform-linux's ignored lib tests changed; add each to lib_tests or lib_left_out:" >&2
    diff <(echo "$known") <(echo "$ignored") >&2 || true
    exit 1
fi

status=0
run() {
    echo "+ $*"
    "$@" || status=1
}
target_args=()
for target in "${linux_targets[@]}"; do target_args+=(--test "$target"); done
if (($#)); then
    run "$CARGO" test --locked -p platform-linux --lib -- --ignored --exact --test-threads=1 "$@"
else
    run "$CARGO" test --locked -p platform-linux --lib -- --ignored --exact --test-threads=1 "${lib_tests[@]}"
    run "$CARGO" test --locked -p platform-linux "${target_args[@]}" -- --ignored --test-threads=1
    # Not ignored, but it needs dbus-daemon, which the headless job does not
    # check for; the variable turns its skip into a failure.
    run env PINCER_TEST_REQUIRE_DRIVER_BIN=1 "$CARGO" test --locked -p pincer \
        --test atspi_listener_startup_test -- --test-threads=1
fi
if ((status)); then
    for log in "$work"/*.log; do
        echo "--- $(basename "$log")"
        tail -n 20 "$log"
    done
fi
exit "$status"
