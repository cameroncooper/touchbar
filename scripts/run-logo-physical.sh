#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
duration=${1:-15}
runtime_dir="$project_dir/run/touchbar-logo-physical"
socket_name="touchbar-logo-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
server_pid=
presenter_pid=
supervisor_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 60)); then
    echo "duration must be between 1 and 60 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
cleanup() {
    for pid in "$supervisor_pid" "$server_pid" "$presenter_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -f -- "$output_socket"
}
trap cleanup EXIT INT TERM

"$project_dir/scripts/build-first-party-packs.sh" >/dev/null
cargo build --quiet --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbard -p touchbar-sessiond -p touchbar-plugin-host -p touchbar-plugin-supervisor

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --hardware-listen "$output_socket" --no-plugins --exit-after-client \
    >"$runtime_dir/sessiond.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 500); do
    [[ -S "$output_socket" ]] && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$output_socket" ]]; then
    sed -n '1,240p' "$runtime_dir/sessiond.log" >&2
    echo "touchbar-sessiond did not create its hardware output socket" >&2
    exit 1
fi

echo "The TouchBar wordmark will be centered across the Touch Bar for $duration seconds."
echo "It is a sealed sandbox asset rendered by the GPU and tinted from the live theme accent color."
echo "Switch themes while it is visible to verify dynamic tinting."
echo "The previously active Touch Bar service will be restored automatically."
pkexec "$project_dir/scripts/run-m3-logo-root.sh" \
    "$project_dir/target/release/touchbard" "$project_dir/assets/touchbar.png" "$duration" \
    --direct "$output_socket" >"$runtime_dir/presenter.log" 2>&1 &
presenter_pid=$!

for _ in $(seq 1 12000); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    sed -n '1,260p' "$runtime_dir/sessiond.log" >&2
    sed -n '1,260p' "$runtime_dir/presenter.log" >&2
    echo "touchbar-sessiond did not create its Wayland socket" >&2
    exit 1
fi

component_digest="sha256:$(sha256sum "$project_dir/plugins/media/component/plugin.wasm" | awk '{print $1}')"
asset_digest="sha256:$(sha256sum "$project_dir/plugins/media/assets/touchbar.svg" | awk '{print $1}')"
env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-plugin-supervisor" \
    "$project_dir/plugins/media" \
    --host "$project_dir/target/release/touchbar-plugin-host" \
    --source github:cameroncooper/touchbar-media --version 1.0.0 \
    --digest "$component_digest" --asset-digest "touchbar-wordmark=$asset_digest" \
    --provenance local-development --state "$runtime_dir" -- \
    --live --item touchbar-logo --width 1004 --require-hardware \
    >"$runtime_dir/component.log" 2>&1 &
supervisor_pid=$!

if ! wait "$presenter_pid"; then
    presenter_pid=
    sed -n '1,280p' "$runtime_dir/presenter.log" >&2
    sed -n '1,280p' "$runtime_dir/component.log" >&2
    exit 1
fi
presenter_pid=
kill "$supervisor_pid" 2>/dev/null || true
wait "$supervisor_pid" 2>/dev/null || true
supervisor_pid=
wait "$server_pid"
server_pid=

rg -q 'hardware-session active' "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"
rg -q 'configured plugin=component item=touchbar-logo region=1004x60 renderer=Apple M1 .* transport=dmabuf runtime=component' \
    "$runtime_dir/component.log"
rg -q 'assigned plugin=github:cameroncooper/touchbar-media item=touchbar-logo x=0 region=1004x60' \
    "$runtime_dir/sessiond.log"

sed -n '1,220p' "$runtime_dir/presenter.log"
sed -n '1,220p' "$runtime_dir/component.log"
