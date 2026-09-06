#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/sdk-ui-physical"
duration=${1:-15}
socket_name="touchbar-ui-physical-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
presenter_binary="$project_dir/target/release/touchbard"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
server_pid=
presenter_pid=
ui_pid=
companion_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$ui_pid" "$companion_pid" "$server_pid" "$presenter_pid"; do
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
    sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

echo "The interactive SDK UI will appear on the physical Touch Bar for $duration seconds."
echo "Hold and slide across fixed choices, or tap once and then tap a choice."
echo "Tap the pinned volume button again to close; persistent choices time out after 4 seconds."
echo "Switch host themes during the run to recolor the full-width backdrop."
echo "The previously active Touch Bar service will be restored automatically on exit or interruption."
pkexec "$root_helper" "$presenter_binary" "$logo" "$duration" --direct "$output_socket" \
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
    sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2
    sed -n '1,260p' "$runtime_dir/presenter.log" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-ui-demo" \
    --frames 36000 --require-hardware \
    >"$runtime_dir/ui.log" 2>&1 &
ui_pid=$!

for _ in $(seq 1 500); do
    rg -q "assigned plugin=touchbar.ui-demo" "$runtime_dir/touchbar-sessiond.log" && break
    sleep 0.01
done

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --plugin-id touchbar.backdrop --backdrop --variant 1 \
    --frames 36000 --require-hardware \
    >"$runtime_dir/companion.log" 2>&1 &
companion_pid=$!

if ! wait "$presenter_pid"; then
    presenter_pid=
    sed -n '1,280p' "$runtime_dir/presenter.log" >&2
    sed -n '1,280p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi
presenter_pid=

kill "$ui_pid" "$companion_pid" 2>/dev/null || true
wait "$ui_pid" 2>/dev/null || true
wait "$companion_pid" 2>/dev/null || true
ui_pid=
companion_pid=
wait "$server_pid"
server_pid=

rg -q "touch-input=ready device=/dev/input/event" "$runtime_dir/presenter.log"
rg -q "hardware-session active" "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"
rg -q "renderer=Apple M1 .* transport=dmabuf ui=touchbar-ui" "$runtime_dir/ui.log"
rg -q "shader-appearance generation=1" "$runtime_dir/companion.log"
rg -q "assigned plugin=touchbar.ui-demo item=audio.volume x=0 region=80x60" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "assigned plugin=touchbar.backdrop role=backdrop x=0 region=2008x60" \
    "$runtime_dir/touchbar-sessiond.log"

sed -n '1,220p' "$runtime_dir/presenter.log"
sed -n '1,180p' "$runtime_dir/ui.log"
sed -n '1,300p' "$runtime_dir/touchbar-sessiond.log"
