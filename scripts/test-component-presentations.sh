#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
runtime_dir=$(mktemp -d "$runtime_root/component-presentations.XXXXXX")
store_dir="$runtime_dir/store"
socket_name=bar
session_pid=
mkdir -p "$store_dir"
chmod 700 "$runtime_dir" "$store_dir"

cleanup() {
    local status=$?
    if [[ -n "$session_pid" ]] && kill -0 "$session_pid" 2>/dev/null; then
        kill "$session_pid" 2>/dev/null || true
        wait "$session_pid" 2>/dev/null || true
    fi
    if ((status != 0)) && [[ -f "$runtime_dir/sessiond.log" ]]; then
        sed -n '1,420p' "$runtime_dir/sessiond.log" >&2
    fi
    if [[ "$runtime_dir" == "$runtime_root"/component-presentations.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
    return "$status"
}
trap cleanup EXIT INT TERM

for binary in touchbar-sessiond touchbar-plugin-host touchbar-plugin-supervisor touchbarctl; do
    [[ -x "$project_dir/target/release/$binary" ]] || {
        echo "release binary is missing: $binary" >&2
        exit 1
    }
done

env TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbarctl" plugin add --path "$project_dir/plugins/controls" >/dev/null
env TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbarctl" plugin enable \
    github:cameroncooper/touchbar-controls >/dev/null

env XDG_RUNTIME_DIR="$runtime_dir" TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --demo-tap \
    --profiles "$project_dir/tests/fixtures/first-party-controls-presentation-v1.toml" \
    >"$runtime_dir/sessiond.log" 2>&1 &
session_pid=$!

for _ in $(seq 1 1500); do
    if rg -q 'component-presentation item=volume event=Ended.*Selection' "$runtime_dir/sessiond.log"; then
        break
    fi
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done

rg -q 'presentation-catalog bars=1' "$runtime_dir/sessiond.log"
rg -q 'presentation=anchored mode=persistent contact=0 session=1 content=volume-controls' \
    "$runtime_dir/sessiond.log"
rg -q 'presentation-layout item=github:cameroncooper/touchbar-controls#volume x=0 width=240 overlay=true' \
    "$runtime_dir/sessiond.log"
rg -q 'presentation-layout item=github:cameroncooper/touchbar-controls#microphone x=248 width=112 overlay=true' \
    "$runtime_dir/sessiond.log"
rg -q 'component-input item=microphone widget=300 kind=Activated' "$runtime_dir/sessiond.log"
rg -q 'component-presentation item=volume event=Ended.*Selection' "$runtime_dir/sessiond.log"
rg -q 'presentation=compact previous=anchored session=1' "$runtime_dir/sessiond.log"

kill "$session_pid"
wait "$session_pid" 2>/dev/null || true
session_pid=
mv "$runtime_dir/sessiond.log" "$runtime_dir/persistent.log"

env XDG_RUNTIME_DIR="$runtime_dir" TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --demo-touch \
    --profiles "$project_dir/tests/fixtures/first-party-controls-presentation-v1.toml" \
    >"$runtime_dir/sessiond.log" 2>&1 &
session_pid=$!

for _ in $(seq 1 1500); do
    if rg -q 'component-presentation item=volume event=Ended.*Selection' "$runtime_dir/sessiond.log"; then
        break
    fi
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done

rg -q 'presentation=anchored mode=transient contact=1 session=1 content=volume-controls' \
    "$runtime_dir/sessiond.log"
rg -q 'touch plugin=github:cameroncooper/touchbar-controls item=volume phase=Cancel contact=1' \
    "$runtime_dir/sessiond.log"
rg -q 'touch plugin=github:cameroncooper/touchbar-controls item=microphone phase=Down contact=1' \
    "$runtime_dir/sessiond.log"
rg -q 'component-input item=microphone widget=300 kind=Activated' "$runtime_dir/sessiond.log"
rg -q 'component-presentation item=volume event=Ended.*Selection' "$runtime_dir/sessiond.log"

kill "$session_pid"
wait "$session_pid" 2>/dev/null || true
session_pid=
mv "$runtime_dir/sessiond.log" "$runtime_dir/transient.log"

env XDG_RUNTIME_DIR="$runtime_dir" TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --demo-tap \
    --profiles "$project_dir/tests/fixtures/first-party-controls-presentation-v1.toml" \
    >"$runtime_dir/sessiond.log" 2>&1 &
session_pid=$!

for _ in $(seq 1 1500); do
    if rg -q 'presentation=anchored mode=persistent contact=0 session=1 content=volume-controls' \
        "$runtime_dir/sessiond.log"; then
        break
    fi
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done
rg -q 'presentation=anchored mode=persistent contact=0 session=1 content=volume-controls' \
    "$runtime_dir/sessiond.log"

env TOUCHBAR_HOME="$store_dir" \
    "$project_dir/target/release/touchbarctl" plugin item \
    github:cameroncooper/touchbar-controls volume disable >/dev/null

for _ in $(seq 1 500); do
    if rg -q 'presentation=compact previous=anchored session=1 reason=SourceHidden' \
        "$runtime_dir/sessiond.log"; then
        break
    fi
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done
rg -q 'presentation=anchored refresh-failed error=presentation bar is no longer enabled' \
    "$runtime_dir/sessiond.log"
rg -q 'presentation=compact previous=anchored session=1 reason=SourceHidden' \
    "$runtime_dir/sessiond.log"

echo 'component-presentations=ok content=volume-controls group=natural width=360 members=2 persistent=tap transient=hold-slide dismissal=selection hot-reload=source-hidden'
