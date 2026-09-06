#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/installed-asset.XXXXXX")
store_dir="$runtime_dir/store"
socket_name="installed-asset-$$"
server_pid=
mkdir -p "$store_dir"
chmod 700 "$runtime_dir" "$store_dir"

cleanup() {
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if [[ "$runtime_dir" == "$runtime_root"/installed-asset.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

"$project_dir/scripts/build-first-party-packs.sh" >/dev/null
source_id=github:cameroncooper/touchbar-media
env TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbarctl" plugin add --path "$project_dir/plugins/media" >/dev/null
for item in now-playing transport timeline; do
    env TOUCHBAR_HOME="$store_dir" \
        "$project_dir/target/release/touchbarctl" plugin item "$source_id" "$item" disable >/dev/null
done
env TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbarctl" plugin item "$source_id" touchbar-logo width 1004 >/dev/null
env TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbarctl" plugin enable "$source_id" >/dev/null

env XDG_RUNTIME_DIR="$runtime_dir" TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbar-sessiond" --socket "$socket_name" \
    --plugin-host "$project_dir/target/release/touchbar-plugin-host" \
    --plugin-supervisor "$project_dir/target/release/touchbar-plugin-supervisor" \
    >"$runtime_dir/sessiond.log" 2>&1 &
server_pid=$!

ready=false
for _ in $(seq 1 800); do
    if env TOUCHBAR_HOME="$store_dir" \
        "$project_dir/target/release/touchbarctl" session status --format json \
        >"$runtime_dir/status.json" 2>/dev/null \
        && rg -q '"item": "touchbar-logo"' "$runtime_dir/status.json" \
        && rg -q '"state": "running"' "$runtime_dir/status.json"; then
        ready=true
        break
    fi
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ "$ready" != true ]]; then
    sed -n '1,320p' "$runtime_dir/sessiond.log" >&2
    [[ ! -f "$runtime_dir/status.json" ]] || sed -n '1,180p' "$runtime_dir/status.json" >&2
    echo "installed asset component did not become healthy" >&2
    exit 1
fi

for _ in $(seq 1 300); do
    rg -q 'component-frame item=touchbar-logo number=0 .* primitives=3' \
        "$runtime_dir/sessiond.log" 2>/dev/null && break
    sleep 0.01
done
rg -q 'component-frame item=touchbar-logo number=0 .* primitives=3' "$runtime_dir/sessiond.log"
rg -q 'configured plugin=component item=touchbar-logo region=1004x60 renderer=Apple M1 .* transport=dmabuf runtime=component' \
    "$runtime_dir/sessiond.log"

kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=
echo "installed-asset-lifecycle=ok source=$source_id item=touchbar-logo renderer=Apple-M1 transport=dmabuf"
