#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/m2-dual"
frames=${1:-600}
socket_name="touchbar-dual-$$"
server_pid=
left_pid=
right_pid=

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
    --socket "$socket_name" --exit-after-clients 2 \
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
    --plugin-id touchbar.gles-left --variant 0 \
    >"$runtime_dir/left.log" 2>&1 &
left_pid=$!

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames "$frames" --require-hardware \
    --plugin-id touchbar.gles-right --variant 1 \
    >"$runtime_dir/right.log" 2>&1 &
right_pid=$!

wait "$left_pid"
left_pid=
wait "$right_pid"
right_pid=
wait "$server_pid"
server_pid=

sed -n '1,120p' "$runtime_dir/left.log"
sed -n '1,120p' "$runtime_dir/right.log"
sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log"

rg -q "plugin=touchbar.gles-left .* renderer=Apple M1 .* transport=dmabuf" \
    "$runtime_dir/left.log"
rg -q "plugin=touchbar.gles-right .* renderer=Apple M1 .* transport=dmabuf" \
    "$runtime_dir/right.log"
rg -q "client-summary plugin=touchbar.gles-left frames=$frames transport=dmabuf" \
    "$runtime_dir/left.log"
rg -q "client-summary plugin=touchbar.gles-right frames=$frames transport=dmabuf" \
    "$runtime_dir/right.log"
rg -q "compositor-renderer=Apple M1" "$runtime_dir/touchbar-sessiond.log"

total_frames=$((frames * 2))
rg -q "summary frames=$total_frames .* invalid=0 dmabuf=$total_frames shm=0 .* max_surfaces=2" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "releases=$total_frames" "$runtime_dir/touchbar-sessiond.log"

summary=$(rg '^summary ' "$runtime_dir/touchbar-sessiond.log")
changed=$(sed -n 's/.* changed=\([0-9]*\) .*/\1/p' <<<"$summary")
presented=$(sed -n 's/.* presented=\([0-9]*\) .*/\1/p' <<<"$summary")
measured_fps=$(sed -n 's/.* fps=\([0-9.]*\) .*/\1/p' <<<"$summary")
minimum_scenes=$((frames - 5))
awk -v changed="$changed" -v presented="$presented" -v minimum="$minimum_scenes" \
    -v fps="$measured_fps" \
    'BEGIN { exit !(changed >= minimum && presented >= minimum && fps >= 55.0) }'
