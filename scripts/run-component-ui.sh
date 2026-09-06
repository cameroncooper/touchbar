#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/component-ui"
width=${1:-160}
socket_name="touchbar-component-$$"
server_pid=
host_pid=

if [[ ! "$width" =~ ^[0-9]+$ ]] || ((width < 60 || width > 2008)); then
    echo "width must be between 60 and 2008" >&2
    exit 2
fi

package_dir=$($project_dir/scripts/build-component-demo.sh)
mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$host_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
}
trap cleanup EXIT

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"

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

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-plugin-host" \
    "$package_dir" --live --item hello --width "$width" --frames 3 --require-hardware \
    >"$runtime_dir/component.log" 2>&1 &
host_pid=$!

wait "$host_pid"
host_pid=
wait "$server_pid"
server_pid=

sed -n '1,220p' "$runtime_dir/component.log"
sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log"

rg -q "configured plugin=component item=hello region=${width}x60 renderer=Apple M1 .* transport=dmabuf runtime=component" \
    "$runtime_dir/component.log"
rg -q "component-appearance item=hello generation=1 scheme=Dark" "$runtime_dir/component.log"
rg -q "component-input item=hello widget=1 kind=Pressed rerender=true" "$runtime_dir/component.log"
rg -q "component-input item=hello widget=1 kind=Activated rerender=true" "$runtime_dir/component.log"
rg -q "component-input item=hello widget=1 kind=Released rerender=true" "$runtime_dir/component.log"
rg -q "client-summary plugin=github:cameroncooper/touchbar-component-demo frames=3 renderer=Apple M1 runtime=component" \
    "$runtime_dir/component.log"
rg -q "assigned plugin=github:cameroncooper/touchbar-component-demo item=hello x=0 region=${width}x60" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "summary frames=3 .* invalid=0 dmabuf=3 shm=0 .* input_events=2" \
    "$runtime_dir/touchbar-sessiond.log"

echo "component live GPU integration: PASS"
