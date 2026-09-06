#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/service-handoff-physical"
duration=${1:-20}
socket_path="/run/touchbar-demo-${UID}-$$/hardware.sock"
wayland_socket="touchbar-handoff-$$"
hardware_pid=
session_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 12 || duration > 30)); then
    echo "duration must be between 12 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
exec 9>"$runtime_dir/run.lock"
flock -n 9 || { echo "A service handoff demo is already running." >&2; exit 1; }

cleanup() {
    if [[ -n "$session_pid" ]] && kill -0 "$session_pid" 2>/dev/null; then
        kill "$session_pid" 2>/dev/null || true
        wait "$session_pid" 2>/dev/null || true
    fi
    if [[ -n "$hardware_pid" ]]; then
        # This may still be pkexec waiting for authorization, or the root
        # helper after authorization. Stop either one before waiting so a
        # failed readiness check cannot leave the harness hung in cleanup.
        kill "$hardware_pid" 2>/dev/null || true
        wait "$hardware_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbard -p touchbar-sessiond -p touchbar-cli

echo "Phase 1: touchbard fallback (media row; hold Fn for F1-F12)."
echo "Phase 2: themed user-session row. Phase 3: hardware restart and automatic reconnect."
echo "Phase 4: automatic fallback recovery after the user session exits."
echo "The previously active Touch Bar service will be restored automatically."
echo "Authorize the guarded physical demo when prompted; this preflight does not touch hardware."
pkexec "$project_dir/scripts/run-m3-logo-root.sh" \
    "$project_dir/target/release/touchbard" "$project_dir/assets/touchbar.png" \
    "$duration" --authorize
pkexec "$project_dir/scripts/run-m3-logo-root.sh" \
    "$project_dir/target/release/touchbard" "$project_dir/assets/touchbar.png" \
    "$duration" --restart-demo "$socket_path" >"$runtime_dir/touchbard.log" 2>&1 &
hardware_pid=$!

# Authorization normally remains cached after the foreground preflight. Keep a
# generous bound in case local polkit policy requests a second confirmation.
for _ in $(seq 1 12000); do
    [[ -S "$socket_path" ]] && break
    sleep 0.01
done
if [[ ! -S "$socket_path" ]]; then
    echo "touchbard did not create its service socket (or polkit authorization timed out)" >&2
    sed -n '1,260p' "$runtime_dir/touchbard.log" >&2
    exit 1
fi

sleep 2
env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$wayland_socket" --hardware-socket "$socket_path" --system-bar --no-plugins \
    --control-socket "$runtime_dir/control.sock" \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
session_pid=$!

session_seconds=$((duration - 7))
sleep "$session_seconds"
if ! kill -0 "$session_pid" 2>/dev/null; then
    echo "touchbar-sessiond exited before the handoff test completed" >&2
    sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2
    sed -n '1,320p' "$runtime_dir/touchbard.log" >&2
    exit 1
fi
env TOUCHBAR_HOME="$runtime_dir" \
    "$project_dir/target/release/touchbarctl" session status --format json \
    >"$runtime_dir/session-status.json"
rg -q '"hardware_connected": true' "$runtime_dir/session-status.json"
kill "$session_pid"
wait "$session_pid" 2>/dev/null || true
session_pid=

wait "$hardware_pid" || {
    hardware_pid=
    sed -n '1,320p' "$runtime_dir/touchbard.log" >&2
    exit 1
}
hardware_pid=

[[ $(rg -c "hardware-fallback=active" "$runtime_dir/touchbard.log") -ge 2 ]]
rg -q "hardware-session=accepted uid=$UID" "$runtime_dir/touchbard.log"
rg -q "hardware-session active" "$runtime_dir/touchbard.log"
rg -q "hardware-service=stopping keys=released backlight=idle" "$runtime_dir/touchbard.log"
rg -q "hardware-service=stopped" "$runtime_dir/touchbard.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/touchbard.log"
rg -q "hardware-output=ready buffers=3" "$runtime_dir/touchbar-sessiond.log"
rg -q "hardware-output=reconnected" "$runtime_dir/touchbar-sessiond.log"

sed -n '1,320p' "$runtime_dir/touchbard.log"
sed -n '1,80p' "$runtime_dir/session-status.json"
sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log"
