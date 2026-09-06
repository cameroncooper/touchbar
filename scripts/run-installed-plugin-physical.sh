#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/installed-plugin-physical"
store_dir="$runtime_dir/store"
control_socket="$store_dir/control.sock"
duration=${1:-15}
socket_name="touchbar-installed-physical-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
presenter_binary="$project_dir/target/release/touchbard"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
server_pid=
presenter_pid=

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
mkdir -p "$store_dir"
chmod 700 "$store_dir"
exec 9>"$runtime_dir/run.lock"
flock -n 9 || { echo "An installed-plugin physical demo is already running." >&2; exit 1; }

if [[ -e "$control_socket" || -L "$control_socket" ]]; then
    if [[ -L "$control_socket" || ! -S "$control_socket" ]]; then
        echo "Refusing to replace unexpected control socket path: $control_socket" >&2
        exit 1
    fi
    if env TOUCHBAR_HOME="$store_dir" \
        "$project_dir/target/release/touchbarctl" session status >/dev/null 2>&1; then
        echo "A touchbar-sessiond instance is already using $control_socket" >&2
        exit 1
    fi
    rm -f -- "$control_socket"
fi

cleanup() {
    for pid in "$server_pid" "$presenter_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -f -- "$output_socket"
    [[ ! -S "$control_socket" ]] || rm -f -- "$control_socket"
}
trap cleanup EXIT INT TERM

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"
package_dir=$("$project_dir/scripts/build-component-demo.sh")
env TOUCHBAR_HOME="$store_dir" "$project_dir/target/release/touchbarctl" plugin add --path "$package_dir"
env TOUCHBAR_HOME="$store_dir" "$project_dir/target/release/touchbarctl" plugin enable github:cameroncooper/touchbar-component-demo

env XDG_RUNTIME_DIR="$runtime_dir" TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbar-sessiond" --socket "$socket_name" --hardware-listen "$output_socket" \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$output_socket" ]] && break
    if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
    sleep 0.01
done
if [[ ! -S "$output_socket" ]]; then
    echo "touchbar-sessiond did not create its ADP output socket" >&2
    sed -n '1,240p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

echo "The installed component pack will appear on the physical Touch Bar for $duration seconds."
echo "Its two items are daemon-launched from the content-addressed store."
echo "Switch host themes to verify live theme propagation."
echo "The previously active Touch Bar service will be restored automatically on exit or interruption."
pkexec "$root_helper" "$presenter_binary" "$logo" "$duration" --direct "$output_socket" \
    >"$runtime_dir/presenter.log" 2>&1 &
presenter_pid=$!

status_ready=false
for _ in $(seq 1 12000); do
    if env TOUCHBAR_HOME="$store_dir" \
        "$project_dir/target/release/touchbarctl" session status >"$runtime_dir/status.log" 2>/dev/null; then
        status_ready=true
        break
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
    sleep 0.01
done
if [[ "$status_ready" != true ]]; then
    echo "touchbar-sessiond did not make its control interface ready" >&2
    sed -n '1,320p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

if ! wait "$presenter_pid"; then
    presenter_pid=
    sed -n '1,280p' "$runtime_dir/presenter.log" >&2
    sed -n '1,320p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi
presenter_pid=

kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true
server_pid=

rg -q "touch-input=ready device=/dev/input/event" "$runtime_dir/presenter.log"
rg -q "hardware-session active" "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"
rg -q "running  github:cameroncooper/touchbar-component-demo:hello" "$runtime_dir/status.log"
rg -q "assigned plugin=github:cameroncooper/touchbar-component-demo item=hello" "$runtime_dir/touchbar-sessiond.log"
rg -q "assigned plugin=github:cameroncooper/touchbar-component-demo item=theme" "$runtime_dir/touchbar-sessiond.log"

sed -n '1,220p' "$runtime_dir/status.log"
sed -n '1,220p' "$runtime_dir/presenter.log"
sed -n '1,340p' "$runtime_dir/touchbar-sessiond.log"
