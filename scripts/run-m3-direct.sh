#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/m3-direct"
duration=${1:-5}
socket_name="touchbar-direct-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
binary="$project_dir/target/release/touchbard"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
server_pid=
presenter_pid=
left_pid=
right_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$left_pid" "$right_pid" "$server_pid" "$presenter_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -f -- "$output_socket"
}
trap cleanup EXIT INT TERM

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --hardware-listen "$output_socket" --exit-after-clients 2 \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$output_socket" ]] && break
    if ! kill -0 "$server_pid" 2>/dev/null; then
        break
    fi
    sleep 0.01
done
if [[ ! -S "$output_socket" ]]; then
    echo "touchbar-sessiond did not create its ADP output socket" >&2
    sed -n '1,200p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

echo "The zero-copy GPU plugin scene will appear for $duration seconds."
echo "The previously active Touch Bar service will be restored automatically on exit or interruption."
pkexec "$root_helper" "$binary" "$logo" "$duration" --direct "$output_socket" \
    >"$runtime_dir/presenter.log" 2>&1 &
presenter_pid=$!

for _ in $(seq 1 12000); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    if ! kill -0 "$server_pid" 2>/dev/null; then
        break
    fi
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "touchbar-sessiond did not create its Wayland socket" >&2
    sed -n '1,240p' "$runtime_dir/touchbar-sessiond.log" >&2
    sed -n '1,240p' "$runtime_dir/presenter.log" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames 36000 --require-hardware --plugin-id touchbar.direct-left --variant 0 \
    >"$runtime_dir/left.log" 2>&1 &
left_pid=$!

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames 36000 --require-hardware --plugin-id touchbar.direct-right --variant 1 \
    >"$runtime_dir/right.log" 2>&1 &
right_pid=$!

if ! wait "$presenter_pid"; then
    presenter_pid=
    sed -n '1,260p' "$runtime_dir/presenter.log" >&2
    sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi
presenter_pid=

kill "$left_pid" "$right_pid" 2>/dev/null || true
wait "$left_pid" 2>/dev/null || true
wait "$right_pid" 2>/dev/null || true
left_pid=
right_pid=
wait "$server_pid"
server_pid=

rg -q "compositor-renderer=Apple M1" "$runtime_dir/touchbar-sessiond.log"
rg -q "hardware-output=ready buffers=3" "$runtime_dir/touchbar-sessiond.log"
rg -q "output=adp-direct" "$runtime_dir/touchbar-sessiond.log"
rg -q "renderer=Apple M1 .* transport=dmabuf" "$runtime_dir/left.log"
rg -q "renderer=Apple M1 .* transport=dmabuf" "$runtime_dir/right.log"
rg -q "direct-summary updates=" "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"

sed -n '1,180p' "$runtime_dir/presenter.log"
sed -n '1,100p' "$runtime_dir/left.log"
sed -n '1,100p' "$runtime_dir/right.log"
sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log"
