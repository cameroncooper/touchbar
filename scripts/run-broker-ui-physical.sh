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

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi
if [[ ! "$width" =~ ^[0-9]+$ ]] || ((width < 160 || width > 2008)); then
    echo "width must be between 160 and 2008 pixels" >&2
    exit 2
fi
if ! command -v dbus-run-session >/dev/null 2>&1; then
    echo "dbus-run-session is required for the isolated MPRIS demo" >&2
    exit 1
fi

# Keep the fake player and every broker process on a private bus. The physical
# presenter itself does not use D-Bus, but inheriting this address is harmless.
if [[ ${TOUCHBAR_BROKER_DEMO_SESSION:-} != 1 ]]; then
    exec dbus-run-session -- env TOUCHBAR_BROKER_DEMO_SESSION=1 \
        "$project_dir/scripts/run-broker-ui-physical.sh" "$duration" "$width"
fi

runtime_dir="$project_dir/run/broker-ui-physical"
socket_name="touchbar-broker-physical-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
presenter_binary="$project_dir/target/release/touchbard"
host_binary="$project_dir/target/release/touchbar-plugin-host"
supervisor_binary="$project_dir/target/release/touchbar-plugin-supervisor"
fake_player_binary="$project_dir/target/release/examples/fake_mpris"
grant_writer_binary="$project_dir/target/release/examples/write_broker_demo_grants"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
server_pid=
presenter_pid=
supervisor_pid=
fake_player_pid=

package_dir=$("$project_dir/scripts/build-broker-component-demo.sh")
mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$supervisor_pid" "$server_pid" "$presenter_pid" "$fake_player_pid"; do
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
    -p touchbar-plugin-supervisor --example fake_mpris
cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-policy --example write_broker_demo_grants

digest="sha256:$(sha256sum "$package_dir/component/plugin.wasm" | awk '{print $1}')"
grants="$runtime_dir/permissions.toml"
"$grant_writer_binary" "$package_dir/touchbar-plugin.toml" "$digest" "$grants"

"$fake_player_binary" >"$runtime_dir/fake-mpris.log" 2>&1 &
fake_player_pid=$!
for _ in $(seq 1 300); do
    rg -q "fake-mpris: ready" "$runtime_dir/fake-mpris.log" 2>/dev/null && break
    if ! kill -0 "$fake_player_pid" 2>/dev/null; then break; fi
    sleep 0.01
done
if ! rg -q "fake-mpris: ready" "$runtime_dir/fake-mpris.log"; then
    echo "the isolated fake MPRIS player did not start" >&2
    sed -n '1,180p' "$runtime_dir/fake-mpris.log" >&2
    exit 1
fi

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

echo "The sandboxed broker demo will appear at ${width}px for $duration seconds."
echo "Tap STATUS to subscribe; it should alternate PAUSED/PLAYING every 1.5 seconds."
echo "Tap PLAY/PAUSE to exercise the physical-activation-gated D-Bus call."
echo "The fake player and broker are isolated from your normal session bus."
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
    --source github:cameroncooper/touchbar-broker-component-demo \
    --version 0.1.0 --digest "$digest" --provenance local-development --state "$runtime_dir" --grants "$grants" -- \
    --live --item media-broker --width "$width" --require-hardware \
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
rg -q "configured plugin=component item=media-broker region=${width}x60 .*runtime=component" \
    "$runtime_dir/component.log"
rg -q "assigned plugin=github:cameroncooper/touchbar-broker-component-demo item=media-broker" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "touch-input=ready device=/dev/input/event" "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"

sed -n '1,220p' "$runtime_dir/presenter.log"
sed -n '1,220p' "$runtime_dir/fake-mpris.log"
sed -n '1,300p' "$runtime_dir/component.log"
sed -n '1,320p' "$runtime_dir/touchbar-sessiond.log"
