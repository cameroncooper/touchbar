#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
account_dir=$(getent passwd "$UID" | cut -d: -f6)
rustup_tools="${CARGO_HOME:-$account_dir/.cargo}/bin"
if [[ -x "$rustup_tools/rustup" ]]; then
    export PATH="$rustup_tools:$PATH"
fi
duration=${1:-20}
width=${2:-320}
selected=${3:-"$account_dir/Pictures"}

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 60)); then
    echo "duration must be between 1 and 60 seconds" >&2
    exit 2
fi
if [[ ! "$width" =~ ^[0-9]+$ ]] || ((width < 160 || width > 2008)); then
    echo "width must be between 160 and 2008 pixels" >&2
    exit 2
fi
if [[ ! -d "$selected" ]]; then
    echo "selected demo root is not a directory: $selected" >&2
    exit 2
fi

runtime_dir="$project_dir/run/filesystem-ui-physical"
socket_name="touchbar-filesystem-physical-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
presenter_binary="$project_dir/target/release/touchbard"
host_binary="$project_dir/target/release/touchbar-plugin-host"
supervisor_binary="$project_dir/target/release/touchbar-plugin-supervisor"
grant_writer_binary="$project_dir/target/release/examples/write_filesystem_demo_grants"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
server_pid=
presenter_pid=
supervisor_pid=

package_dir=$("$project_dir/scripts/build-filesystem-component-demo.sh")
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

cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbard -p touchbar-sessiond -p touchbar-plugin-host \
    -p touchbar-plugin-supervisor
cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-policy --example write_filesystem_demo_grants

digest="sha256:$(sha256sum "$package_dir/component/plugin.wasm" | awk '{print $1}')"
grants="$runtime_dir/permissions.toml"
"$grant_writer_binary" "$package_dir/touchbar-plugin.toml" "$digest" "$grants" "$selected"

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

echo "The sandboxed filesystem demo will appear at ${width}px for $duration seconds."
echo "Granted gallery root: $(realpath -- "$selected")"
echo "Tap OPEN GALLERY to list it and stream the first regular file through the broker."
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
    "$supervisor_binary" "$package_dir" \
    --host "$host_binary" \
    --source github:cameroncooper/touchbar-filesystem-component-demo \
    --version 0.1.0 --digest "$digest" --provenance local-development --state "$runtime_dir" --grants "$grants" -- \
    --live --item filesystem-demo --width "$width" --require-hardware \
    >"$runtime_dir/component.log" 2>&1 &
supervisor_pid=$!

if ! wait "$presenter_pid"; then
    presenter_pid=
    sed -n '1,280p' "$runtime_dir/presenter.log" >&2
    sed -n '1,320p' "$runtime_dir/component.log" >&2
    sed -n '1,280p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi
presenter_pid=

kill "$supervisor_pid" 2>/dev/null || true
wait "$supervisor_pid" 2>/dev/null || true
supervisor_pid=
wait "$server_pid"
server_pid=

rg -q "broker: supervised generation=1" "$runtime_dir/component.log"
rg -q "configured plugin=component item=filesystem-demo region=${width}x60 .*runtime=component" \
    "$runtime_dir/component.log"
rg -q "touch-input=ready device=/dev/input/event" "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"

sed -n '1,220p' "$runtime_dir/presenter.log"
sed -n '1,300p' "$runtime_dir/component.log"
sed -n '1,320p' "$runtime_dir/touchbar-sessiond.log"
