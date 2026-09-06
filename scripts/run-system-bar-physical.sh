#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
duration=${1:-20}
runtime_dir="$project_dir/run/system-bar-physical"
socket_name="touchbar-system-physical-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
session_pid=
hardware_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
exec 9>"$runtime_dir/run.lock"
flock -n 9 || { echo "A system-bar physical demo is already running." >&2; exit 1; }

cleanup() {
    for pid in "$session_pid" "$hardware_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -f -- "$output_socket"
}
trap cleanup EXIT INT TERM

cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbard -p touchbar-sessiond

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --hardware-listen "$output_socket" --system-bar --no-plugins \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
session_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$output_socket" ]] && break
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$output_socket" ]]; then
    echo "touchbar-sessiond did not create its hardware socket" >&2
    sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

echo "Showing the built-in media layer for $duration seconds; hold Fn for F1–F12."
echo "Buttons emit restricted Linux system keys through touchbard's uinput device."
echo "The previously active Touch Bar service will be restored automatically."
pkexec "$project_dir/scripts/run-m3-logo-root.sh" \
    "$project_dir/target/release/touchbard" "$project_dir/assets/touchbar.png" \
    "$duration" --direct "$output_socket" >"$runtime_dir/touchbard.log" 2>&1 &
hardware_pid=$!

if ! wait "$hardware_pid"; then
    hardware_pid=
    sed -n '1,320p' "$runtime_dir/touchbard.log" >&2
    sed -n '1,320p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi
hardware_pid=

kill "$session_pid" 2>/dev/null || true
wait "$session_pid" 2>/dev/null || true
session_pid=

rg -q "hardware-session active" "$runtime_dir/touchbard.log"
rg -q "system-keys=ready" "$runtime_dir/touchbard.log"
rg -q "fn-input=ready" "$runtime_dir/touchbard.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/touchbard.log"

sed -n '1,220p' "$runtime_dir/touchbard.log"
sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log"
