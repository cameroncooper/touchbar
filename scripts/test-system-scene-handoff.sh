#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/system-scene-handoff"
socket_name="touchbar-system-handoff-$$"
session_pid=

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
exec 9>"$runtime_dir/run.lock"
flock -n 9 || { echo "A system-scene handoff test is already running." >&2; exit 1; }

cleanup() {
    if [[ -n "$session_pid" ]] && kill -0 "$session_pid" 2>/dev/null; then
        kill "$session_pid" 2>/dev/null || true
        wait "$session_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-sessiond -p touchbar-gl-demo -p touchbar-cli

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --frame-output "$runtime_dir/frame-stream" \
    --system-bar --no-plugins --control-socket "$runtime_dir/control.sock" \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
session_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

env TOUCHBAR_HOME="$runtime_dir" \
    "$project_dir/target/release/touchbarctl" session status --format json \
    >"$runtime_dir/session-status.json"
rg -q '"hardware_connected": false' "$runtime_dir/session-status.json"

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames 30 --require-hardware --plugin-id touchbar.system-handoff \
    >"$runtime_dir/plugin.log" 2>&1

for _ in $(seq 1 500); do
    rg -q "system-scene=visible reason=empty-profile" "$runtime_dir/touchbar-sessiond.log" && break
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done

rg -q "system-scene=hidden reason=user-content" "$runtime_dir/touchbar-sessiond.log"
rg -q "system-scene=visible reason=empty-profile" "$runtime_dir/touchbar-sessiond.log"
rg -q "renderer=Apple M1 .* transport=dmabuf" "$runtime_dir/plugin.log"

kill "$session_pid"
wait "$session_pid" 2>/dev/null || true
session_pid=

echo "system-scene-handoff=ok"
sed -n '1,80p' "$runtime_dir/session-status.json"
sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log"
