#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/m3-stream"
frames=${1:-120}
socket_name="touchbar-stream-$$"
frame_stream="$runtime_dir/scene.rgba-stream"
server_pid=

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid"
        wait "$server_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --frame-output "$frame_stream" --exit-after-client \
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
    --frames "$frames" --require-hardware \
    >"$runtime_dir/client.log" 2>&1

wait "$server_pid"
server_pid=

"$project_dir/target/release/touchbard" --stream-probe "$frame_stream"
rg -q "frame-output=$frame_stream size=2008x60 slots=3" "$runtime_dir/touchbar-sessiond.log"
rg -q "summary frames=$frames changed=$frames invalid=0 dmabuf=$frames shm=0" \
    "$runtime_dir/touchbar-sessiond.log"

sed -n '1,120p' "$runtime_dir/client.log"
sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log"
