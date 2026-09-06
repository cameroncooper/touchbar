#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/m2-slow-client"
fast_frames=${1:-600}
slow_frames=$(((fast_frames + 3) / 4))
socket_name="touchbar-slow-$$"
server_pid=
fast_pid=
slow_pid=

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$fast_pid" "$slow_pid" "$server_pid"; do
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
    --frames "$fast_frames" --require-hardware \
    --plugin-id touchbar.fast --variant 0 \
    >"$runtime_dir/fast.log" 2>&1 &
fast_pid=$!

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames "$slow_frames" --require-hardware \
    --plugin-id touchbar.slow --variant 1 --delay-ms 50 \
    >"$runtime_dir/slow.log" 2>&1 &
slow_pid=$!

wait "$fast_pid"
fast_pid=
wait "$slow_pid"
slow_pid=
wait "$server_pid"
server_pid=

sed -n '1,120p' "$runtime_dir/fast.log"
sed -n '1,120p' "$runtime_dir/slow.log"
sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log"

rg -q "client-summary plugin=touchbar.fast frames=$fast_frames transport=dmabuf" \
    "$runtime_dir/fast.log"
rg -q "client-summary plugin=touchbar.slow frames=$slow_frames transport=dmabuf" \
    "$runtime_dir/slow.log"
rg -q "renderer=Apple M1" "$runtime_dir/fast.log"
rg -q "renderer=Apple M1" "$runtime_dir/slow.log"
rg -q "compositor-renderer=Apple M1" "$runtime_dir/touchbar-sessiond.log"

total_frames=$((fast_frames + slow_frames))
rg -q "summary frames=$total_frames .* invalid=0 dmabuf=$total_frames shm=0 .* max_surfaces=2" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q 'rate_limited=0 abusive_disconnects=0 dropped_callbacks=0$' \
    "$runtime_dir/touchbar-sessiond.log"

summary=$(rg '^summary ' "$runtime_dir/touchbar-sessiond.log")
changed=$(sed -n 's/.* changed=\([0-9]*\) .*/\1/p' <<<"$summary")
presented=$(sed -n 's/.* presented=\([0-9]*\) .*/\1/p' <<<"$summary")
released=$(sed -n 's/.* releases=\([0-9]*\) .*/\1/p' <<<"$summary")
measured_fps=$(sed -n 's/.* fps=\([0-9.]*\) .*/\1/p' <<<"$summary")
minimum_scenes=$((fast_frames - 5))
awk -v changed="$changed" -v presented="$presented" -v minimum="$minimum_scenes" \
    -v released="$released" -v total="$total_frames" -v fps="$measured_fps" \
    'BEGIN { exit !(changed >= minimum && presented >= minimum && released == total && fps >= 55.0) }'
