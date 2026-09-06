#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
runtime_dir="$project_dir/run/scui"
width=${1:-160}
socket_name="scui-$$"
server_pid=

if [[ ! "$width" =~ ^[0-9]+$ ]] || ((width < 60 || width > 2008)); then
    echo "width must be between 60 and 2008" >&2
    exit 2
fi

package_dir=$($project_dir/scripts/build-component-demo.sh)
install -d -m 700 "$runtime_dir"

cleanup() {
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-sessiond -p touchbar-plugin-host -p touchbar-plugin-supervisor

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --demo-tap --exit-after-client \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "touchbar-sessiond did not create its Wayland socket" >&2
    sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

digest="sha256:$(sha256sum "$package_dir/component/plugin.wasm" | awk '{print $1}')"
env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-plugin-supervisor" "$package_dir" \
    --host "$project_dir/target/release/touchbar-plugin-host" \
    --source github:cameroncooper/touchbar-component-demo \
    --version 0.1.0 --digest "$digest" --provenance local-development --state "$runtime_dir" -- \
    --live --item hello --width "$width" --frames 3 --require-hardware \
    >"$runtime_dir/component.log" 2>&1

wait "$server_pid"
server_pid=

rg -q "broker: supervised generation=1" "$runtime_dir/component.log"
rg -q "configured plugin=component item=hello region=${width}x60 renderer=Apple M1 .* transport=dmabuf runtime=component" \
    "$runtime_dir/component.log"
rg -q "client-summary plugin=github:cameroncooper/touchbar-component-demo frames=3 renderer=Apple M1 .* runtime=component" \
    "$runtime_dir/component.log"
rg -q "summary frames=3 .* invalid=0 dmabuf=3 shm=0" "$runtime_dir/touchbar-sessiond.log"

sed -n '1,220p' "$runtime_dir/component.log"
sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log"
echo "supervised component live GPU integration: PASS"
