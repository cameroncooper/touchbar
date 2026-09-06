#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/m1"
frames=${1:-600}
server_pid=
socket_name="touchbar-test-$$"

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
    "$project_dir/target/release/touchbar-sessiond" --socket "$socket_name" --exit-after-client \
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

env XDG_RUNTIME_DIR="$runtime_dir" \
    WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames "$frames" --require-hardware \
    >"$runtime_dir/client.log" 2>&1

wait "$server_pid"
server_pid=

sed -n '1,200p' "$runtime_dir/client.log"
sed -n '1,240p' "$runtime_dir/touchbar-sessiond.log"

rg -q "client-summary plugin=.* frames=$frames transport=dmabuf" "$runtime_dir/client.log"
rg -q "summary frames=$frames changed=$frames invalid=0" "$runtime_dir/touchbar-sessiond.log"

measured_fps=$(sed -n 's/.* fps=\([0-9.]*\) .*/\1/p' "$runtime_dir/touchbar-sessiond.log")
awk -v fps="$measured_fps" 'BEGIN { exit !(fps >= 55.0) }'
