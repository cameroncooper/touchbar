#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/m3-scene"
duration=${1:-5}
socket_name="touchbar-physical-$$"
frame_stream="$runtime_dir/scene.rgba-stream"
binary="$project_dir/target/release/touchbard"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
server_pid=
left_pid=
right_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$left_pid" "$right_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid"
            wait "$pid" 2>/dev/null || true
        fi
    done
}
trap cleanup EXIT

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --frame-output "$frame_stream" --exit-after-clients 2 \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "touchbar-sessiond did not create its Wayland socket" >&2
    sed -n '1,160p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames 36000 --require-hardware --plugin-id touchbar.physical-left --variant 0 \
    >"$runtime_dir/left.log" 2>&1 &
left_pid=$!

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames 36000 --require-hardware --plugin-id touchbar.physical-right --variant 1 \
    >"$runtime_dir/right.log" 2>&1 &
right_pid=$!

for _ in $(seq 1 500); do
    if "$binary" --stream-probe "$frame_stream" >/dev/null 2>&1; then
        break
    fi
    sleep 0.01
done
"$binary" --stream-probe "$frame_stream"

echo "The GPU-composited plugin scene will appear for $duration seconds."
echo "The previously active Touch Bar service will be restored automatically on exit or interruption."
pkexec "$root_helper" "$binary" "$logo" "$duration" --scene "$frame_stream"

kill "$left_pid" "$right_pid" 2>/dev/null || true
wait "$left_pid" 2>/dev/null || true
wait "$right_pid" 2>/dev/null || true
left_pid=
right_pid=
wait "$server_pid"
server_pid=

rg -q "compositor-renderer=Apple M1" "$runtime_dir/touchbar-sessiond.log"
rg -q "frame-output=$frame_stream size=2008x60 slots=3" "$runtime_dir/touchbar-sessiond.log"
rg -q "renderer=Apple M1 .* transport=dmabuf" "$runtime_dir/left.log"
rg -q "renderer=Apple M1 .* transport=dmabuf" "$runtime_dir/right.log"

sed -n '1,100p' "$runtime_dir/left.log"
sed -n '1,100p' "$runtime_dir/right.log"
sed -n '1,240p' "$runtime_dir/touchbar-sessiond.log"
