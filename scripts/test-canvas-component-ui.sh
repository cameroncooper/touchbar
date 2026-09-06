#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/canvas-component.XXXXXX")
socket_name="canvas-$$"
server_pid=
chmod 700 "$runtime_dir"

cleanup() {
    status=$?
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if ((status != 0)); then
        for log in "$runtime_dir"/*.log; do
            [[ -f "$log" ]] || continue
            echo "--- $log" >&2
            sed -n '1,260p' "$log" >&2
        done
    fi
    if [[ "$runtime_dir" == "$runtime_root"/canvas-component.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --quiet --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-sessiond -p touchbar-plugin-host -p touchbar-plugin-supervisor
"$project_dir/target/release/touchbarctl" plugin check \
    --package "$project_dir/plugins/media" >/dev/null
"$project_dir/target/release/touchbar-plugin-host" \
    "$project_dir/plugins/media" now-playing 320 >"$runtime_dir/semantic.log"
rg -q 'Canvas "Playback signal and position"' "$runtime_dir/semantic.log"
rg -q 'Canvas "Theme-reactive ambient playback wave"' "$runtime_dir/semantic.log"

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" --socket "$socket_name" \
    --no-plugins --exit-after-client >"$runtime_dir/sessiond.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    sed -n '1,240p' "$runtime_dir/sessiond.log" >&2
    echo "Canvas2D test compositor did not become ready" >&2
    exit 1
fi

digest="sha256:$(sha256sum "$project_dir/plugins/media/component/plugin.wasm" | awk '{print $1}')"
asset_digest="sha256:$(sha256sum "$project_dir/plugins/media/assets/touchbar.svg" | awk '{print $1}')"
env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-plugin-supervisor" \
    "$project_dir/plugins/media" \
    --host "$project_dir/target/release/touchbar-plugin-host" \
    --source github:cameroncooper/touchbar-media --version 1.0.0 \
    --digest "$digest" --asset-digest "touchbar-wordmark=$asset_digest" \
    --provenance local-development --state "$runtime_dir" -- \
    --live --item now-playing --width 320 --frames 60 --require-hardware \
    >"$runtime_dir/component.log" 2>&1

wait "$server_pid"
server_pid=
rg -q 'configured plugin=component item=now-playing region=320x60 renderer=Apple M1 .* transport=dmabuf runtime=component' \
    "$runtime_dir/component.log"
rg -q 'shader-effect=ready backend=gles300 cache_entries=1' "$runtime_dir/component.log"
rg -q 'component-frame item=now-playing number=0 .* primitives=55' \
    "$runtime_dir/component.log"
rg -q 'component-animation-summary frames=60 guest-renders=1' \
    "$runtime_dir/component.log"
rg -q 'client-summary plugin=github:cameroncooper/touchbar-media frames=60 renderer=Apple M1 .* runtime=component' \
    "$runtime_dir/component.log"
rg -q 'summary frames=60 .* invalid=0 dmabuf=60 shm=0' "$runtime_dir/sessiond.log"

echo "canvas-component-ui=ok renderer=Apple-M1 transport=dmabuf frames=60 guest-renders=1 shader-effect=validated invalid=0"
