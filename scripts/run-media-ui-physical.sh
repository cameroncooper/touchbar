#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/media-ui-physical"
duration=${1:-15}
width=${2:-160}
socket_name="touchbar-media-physical-$$"
# The privileged presenter helper intentionally accepts only this narrowly
# scoped socket namespace.
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
presenter_binary="$project_dir/target/release/touchbard"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
server_pid=
presenter_pid=
media_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi
if [[ ! "$width" =~ ^(80|160|420)$ ]]; then
    echo "width must be one of 80, 160, or 420" >&2
    exit 2
fi
if [[ ! "$output_socket" =~ ^/tmp/touchbar-direct-[0-9]+-[0-9]+\.sock$ ]]; then
    echo "internal error: physical presenter socket is outside the approved namespace" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$media_pid" "$server_pid" "$presenter_pid"; do
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
    --socket "$socket_name" --hardware-listen "$output_socket" --exit-after-client \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$output_socket" ]] && break
    if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
    sleep 0.01
done
if [[ ! -S "$output_socket" ]]; then
    echo "touchbar-sessiond did not create its ADP output socket" >&2
    sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

echo "The simulated media widget will appear on the physical Touch Bar for $duration seconds."
echo "Tap artwork or text to expand, or hold and slide directly onto the timeline."
echo "The center button plays/pauses; drag the timeline to seek; tap outside to close."
echo "Switch host themes to verify live colors and contrast."
echo "The previously active Touch Bar service will be restored automatically on exit or interruption."
pkexec "$root_helper" "$presenter_binary" "$logo" "$duration" --direct "$output_socket" \
    >"$runtime_dir/presenter.log" 2>&1 &
presenter_pid=$!

for _ in $(seq 1 12000); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "touchbar-sessiond did not create its Wayland socket" >&2
    sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2
    sed -n '1,260p' "$runtime_dir/presenter.log" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-media-demo" \
    --frames 36000 --width "$width" --require-hardware \
    >"$runtime_dir/media.log" 2>&1 &
media_pid=$!

if ! wait "$presenter_pid"; then
    presenter_pid=
    sed -n '1,280p' "$runtime_dir/presenter.log" >&2
    sed -n '1,280p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi
presenter_pid=

kill "$media_pid" 2>/dev/null || true
wait "$media_pid" 2>/dev/null || true
media_pid=
wait "$server_pid"
server_pid=

rg -q "touch-input=ready device=/dev/input/event" "$runtime_dir/presenter.log"
rg -q "hardware-session active" "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"
rg -q "configured plugin=touchbar.media-demo region=${width}x60 renderer=Apple M1 .* transport=dmabuf" \
    "$runtime_dir/media.log"
rg -q "assigned plugin=touchbar.media-demo item=media.now-playing x=0 region=${width}x60" \
    "$runtime_dir/touchbar-sessiond.log"

sed -n '1,220p' "$runtime_dir/presenter.log"
sed -n '1,220p' "$runtime_dir/media.log"
sed -n '1,300p' "$runtime_dir/touchbar-sessiond.log"
