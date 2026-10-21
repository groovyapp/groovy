#!/usr/bin/env bash
# Launches a Groovy AppImage on Wayland and on X11, and checks that it starts.
#
#   scripts/smoke-test-linux.sh dist/Groovy-0.0.1-x86_64.AppImage
#
# Each run uses a throwaway data directory, so it never touches your own Groovy. A fresh data
# directory opens on onboarding, so the node is not started yet. A run passes when the window
# opens on the expected display backend with a GPU, nothing panics, and the app is still running
# ten seconds later. Needs a graphical session and FUSE, as an AppImage normally does; a backend
# that is not available (no Wayland compositor, or no X server) is skipped.

set -uo pipefail

APPIMAGE=$(readlink -f "${1:?give the AppImage to test}")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/groovy-smoke.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
failures=0

run() {
    local backend=$1
    local dir="$WORK/$backend"
    mkdir -p "$dir"
    local env_args=(env)
    case $backend in
        wayland)
            if [ -z "${WAYLAND_DISPLAY:-}" ]; then
                echo "skip  wayland: no Wayland session"
                return
            fi
            ;;
        x11)
            if [ -z "${DISPLAY:-}" ]; then
                echo "skip  x11: no X display"
                return
            fi
            env_args+=(-u WAYLAND_DISPLAY)
            ;;
    esac
    env_args+=("GROOVY_DATA_DIR=$dir")

    # Its own process group, so stopping it stops the runtime and the app together.
    setsid "${env_args[@]}" "$APPIMAGE" >"$dir/stdout.txt" 2>&1 &
    local pid=$!
    sleep 10
    local log
    log=$(cat "$dir"/logs/*.log "$dir/stdout.txt" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g')

    local problems=()
    kill -0 "$pid" 2>/dev/null || problems+=("it exited")
    grep -q "Selected GPU adapter" <<<"$log" || problems+=("no GPU was selected")
    grep -qi "panicked" <<<"$log" && problems+=("it panicked")
    # GPUI names its X11 backend in the log, and not its Wayland one.
    if [ "$backend" = x11 ]; then
        grep -q "gpui_linux::linux::x11" <<<"$log" || problems+=("it did not use X11")
    else
        grep -q "gpui_linux::linux::x11" <<<"$log" && problems+=("it used X11, not Wayland")
    fi

    kill -- -"$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    # Give the AppImage's mount time to go before the next run mounts it again.
    sleep 3

    if [ ${#problems[@]} -eq 0 ]; then
        echo "pass  $backend"
    else
        echo "FAIL  $backend: ${problems[*]}"
        tail -n 5 "$dir/stdout.txt" | sed 's/\x1b\[[0-9;]*m//g; s/^/      /'
        failures=$((failures + 1))
    fi
}

run wayland
run x11
exit $failures
