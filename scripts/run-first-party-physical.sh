#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
pack=${1:-controls}
duration=${2:-20}
consent=${3:-}
case "$pack" in
    controls)
        source_id=github:cameroncooper/touchbar-controls
        action_capability=command.run.v1
        ;;
    media)
        source_id=github:cameroncooper/touchbar-media
        action_capability=dbus.call.v1
        ;;
    hyprland)
        source_id=github:cameroncooper/touchbar-hyprland
        action_capability=
        ;;
    capture)
        source_id=github:cameroncooper/touchbar-capture
        action_capability=
        ;;
    command-deck)
        source_id=github:cameroncooper/touchbar-command-deck
        action_capability=command.run.v1
        ;;
    *) echo "pack must be controls, media, hyprland, capture, or command-deck" >&2; exit 2 ;;
esac
if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 60)); then
    echo "duration must be between 1 and 60 seconds" >&2
    exit 2
fi
if [[ -n "$consent" && "$consent" != "--allow-session-actions" ]]; then
    echo "third argument must be --allow-session-actions" >&2
    exit 2
fi
if [[ "$consent" == "--allow-session-actions" && -z "$action_capability" ]]; then
    echo "session action demo is currently supported only for controls, media, and command-deck" >&2
    exit 2
fi

runtime_dir="$project_dir/run/first-party-$pack"
store_dir="$runtime_dir/store"
control_socket="$store_dir/control.sock"
socket_name="touchbar-${pack}-physical-$$"
output_socket="/tmp/touchbar-direct-${UID}-$$.sock"
server_pid=
presenter_pid=
mkdir -p "$runtime_dir" "$store_dir"
chmod 700 "$runtime_dir" "$store_dir"
exec 9>"$runtime_dir/run.lock"
flock -n 9 || { echo "A $pack physical demo is already running." >&2; exit 1; }

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
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    rm -f -- "$output_socket"
    [[ ! -S "$control_socket" ]] || rm -f -- "$control_socket"
}
trap cleanup EXIT INT TERM

"$project_dir/scripts/build-first-party-packs.sh"
env TOUCHBAR_HOME="$store_dir" "$project_dir/target/release/touchbarctl" plugin add --path "$project_dir/plugins/$pack"
env TOUCHBAR_HOME="$store_dir" "$project_dir/target/release/touchbarctl" plugin enable "$source_id"

env XDG_RUNTIME_DIR="$runtime_dir" TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbar-sessiond" --socket "$socket_name" --hardware-listen "$output_socket" \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 500); do [[ -S "$output_socket" ]] && break; kill -0 "$server_pid" 2>/dev/null || break; sleep 0.01; done
[[ -S "$output_socket" ]] || { sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2; exit 1; }

echo "The $pack pack will appear on the physical Touch Bar for $duration seconds."
if [[ "$consent" == "--allow-session-actions" ]]; then
    echo "The normalized $action_capability request will be allowed only for this disposable session."
else
    echo "Theme changes and local touch state work; OS actions remain denied."
    echo "Pass --allow-session-actions as the third argument to explicitly enable this demo's Controls, Media, or Command Deck actions."
fi
echo "The previously active Touch Bar service will be restored automatically."
pkexec "$project_dir/scripts/run-m3-logo-root.sh" "$project_dir/target/release/touchbard" \
    "$project_dir/assets/touchbar.png" "$duration" --direct "$output_socket" >"$runtime_dir/presenter.log" 2>&1 &
presenter_pid=$!
status_ready=false
for _ in $(seq 1 12000); do
    if env TOUCHBAR_HOME="$store_dir" \
        "$project_dir/target/release/touchbarctl" session status >"$runtime_dir/status.log" 2>/dev/null; then
        status_ready=true
        break
    fi
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ "$status_ready" != true ]]; then
    echo "touchbar-sessiond did not make its control interface ready" >&2
    sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi
if [[ "$consent" == "--allow-session-actions" ]]; then
    env TOUCHBAR_HOME="$store_dir" \
        "$project_dir/target/release/touchbarctl" plugin permissions "$source_id"
    env TOUCHBAR_HOME="$store_dir" \
        "$project_dir/target/release/touchbarctl" plugin permission \
        "$source_id" "$action_capability" allow --session
fi
wait "$presenter_pid"
presenter_pid=
kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true
server_pid=
rg -q "hardware-session active" "$runtime_dir/presenter.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/presenter.log"
rg -q "running  ${source_id}:" "$runtime_dir/status.log"
sed -n '1,180p' "$runtime_dir/status.log"
sed -n '1,220p' "$runtime_dir/presenter.log"
sed -n '1,340p' "$runtime_dir/touchbar-sessiond.log"
