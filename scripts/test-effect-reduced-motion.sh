#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/effect-reduced.XXXXXX")
socket_name="effect-reduced-$$"
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
    if [[ "$runtime_dir" == "$runtime_root"/effect-reduced.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --quiet --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-sessiond -p touchbar-plugin-host -p touchbar-plugin-supervisor
"$project_dir/target/release/touchbarctl" plugin check \
    --package "$project_dir/plugins/media" >/dev/null

env XDG_RUNTIME_DIR="$runtime_dir" \
    TOUCHBAR_THEME="$project_dir/tests/fixtures/reduced-theme.toml" \
    "$project_dir/target/release/touchbar-sessiond" --socket "$socket_name" \
    --no-plugins --exit-after-client >"$runtime_dir/sessiond.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "reduced-motion compositor did not become ready" >&2
    exit 1
fi

digest="sha256:$(sha256sum "$project_dir/plugins/media/component/plugin.wasm" | awk '{print $1}')"
asset_digest="sha256:$(sha256sum "$project_dir/plugins/media/assets/touchbar.svg" | awk '{print $1}')"
set +e
timeout --signal=TERM --kill-after=1 1 \
    env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-plugin-supervisor" \
    "$project_dir/plugins/media" \
    --host "$project_dir/target/release/touchbar-plugin-host" \
    --source github:cameroncooper/touchbar-media --version 1.0.0 \
    --digest "$digest" --asset-digest "touchbar-wordmark=$asset_digest" \
    --provenance local-development --state "$runtime_dir" -- \
    --live --item now-playing --width 320 --require-hardware \
    >"$runtime_dir/component.log" 2>&1
client_status=$?
set -e
if ((client_status != 124)); then
    echo "reduced-motion client exited unexpectedly with status $client_status" >&2
    exit 1
fi

wait "$server_pid"
server_pid=
rg -q 'component-appearance item=now-playing .* motion=Reduced ' "$runtime_dir/component.log"
rg -q 'shader-effect=ready backend=gles300 cache_entries=1' "$runtime_dir/component.log"
rg -q 'component-frame item=now-playing number=0 .* primitives=55' "$runtime_dir/component.log"
if rg -q 'component-frame item=now-playing number=[1-9]' "$runtime_dir/component.log"; then
    echo "reduced-motion effect scheduled an additional frame" >&2
    exit 1
fi
rg -q 'summary frames=1 .* invalid=0 dmabuf=1 shm=0' "$runtime_dir/sessiond.log"

echo "effect-reduced-motion=ok frames=1 guest-renders=1 policy=reduced"
