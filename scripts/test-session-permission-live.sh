#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
probe_dir=$(mktemp -d "$runtime_root/session-permission-live.XXXXXX")
store="$probe_dir/store"
runtime="$probe_dir/runtime"
socket_name="otb-c-$$"
session_pid=
mkdir -p "$store" "$runtime"
chmod 700 "$probe_dir" "$store" "$runtime"

cleanup() {
    if [[ -n "$session_pid" ]] && kill -0 "$session_pid" 2>/dev/null; then
        kill "$session_pid" 2>/dev/null || true
        wait "$session_pid" 2>/dev/null || true
    fi
    if [[ "$probe_dir" == "$runtime_root"/session-permission-live.* && -d "$probe_dir" ]]; then
        rm -r -- "$probe_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --quiet --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-cli -p touchbar-sessiond -p touchbar-plugin-host \
    -p touchbar-plugin-supervisor

cli="$project_dir/target/release/touchbarctl"
sessiond="$project_dir/target/release/touchbar-sessiond"
host="$project_dir/target/release/touchbar-plugin-host"
supervisor="$project_dir/target/release/touchbar-plugin-supervisor"
source=github:cameroncooper/touchbar-controls
archive="$probe_dir/controls.touchbar"

"$cli" plugin pack --package "$project_dir/plugins/controls" --output "$archive" >/dev/null
TOUCHBAR_HOME="$store" "$cli" plugin add --path "$archive" >/dev/null
TOUCHBAR_HOME="$store" "$cli" plugin enable "$source" >/dev/null

start_session() {
    local log=$1
    env XDG_RUNTIME_DIR="$runtime" TOUCHBAR_HOME="$store" \
        "$sessiond" --socket "$socket_name" --plugin-host "$host" \
        --plugin-supervisor "$supervisor" >"$log" 2>&1 &
    session_pid=$!
    for _ in $(seq 1 1000); do
        if TOUCHBAR_HOME="$store" "$cli" session status --format json \
            >"$probe_dir/status.json" 2>/dev/null; then
            return 0
        fi
        kill -0 "$session_pid" 2>/dev/null || break
        sleep 0.01
    done
    sed -n '1,260p' "$log" >&2
    echo "isolated touchbar-sessiond did not become ready" >&2
    return 1
}

stop_session() {
    kill "$session_pid"
    wait "$session_pid" 2>/dev/null || true
    session_pid=
}

wait_for_running_plugin() {
    for _ in $(seq 1 500); do
        TOUCHBAR_HOME="$store" "$cli" session status --format json \
            >"$probe_dir/status.json"
        if rg -q '"state": "running"' "$probe_dir/status.json"; then
            return 0
        fi
        sleep 0.01
    done
    echo "isolated plugin did not reach running state" >&2
    return 1
}

start_session "$probe_dir/first-session.log"
before=$(TOUCHBAR_HOME="$store" "$cli" plugin permissions "$source" --format json)
rg -q '"status": "needs-consent"' <<<"$before"

TOUCHBAR_HOME="$store" "$cli" plugin permission \
    "$source" command.run.v1 allow --session >/dev/null
allowed=$(TOUCHBAR_HOME="$store" "$cli" plugin permissions "$source" --format json)
rg -q '"status": "granted"' <<<"$allowed"
rg -q '"from_session": true' <<<"$allowed"
[[ $(stat -c %a "$store/session-permissions.toml") == 600 ]]
[[ ! -e "$store/permissions.toml" ]]
wait_for_running_plugin

stop_session
start_session "$probe_dir/second-session.log"
expired=$(TOUCHBAR_HOME="$store" "$cli" plugin permissions "$source" --format json)
rg -q '"status": "needs-consent"' <<<"$expired"
rg -q '"from_session": false' <<<"$expired"
wait_for_running_plugin

echo "session-permission-live=ok owner=authenticated reload=effective restart=expired"
