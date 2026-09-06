#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/sessiond-idle.XXXXXX")
socket_name="sessiond-idle-$$"
session_pid=
chmod 700 "$runtime_dir"

cleanup() {
    status=$?
    if [[ -n "$session_pid" ]] && kill -0 "$session_pid" 2>/dev/null; then
        kill "$session_pid" 2>/dev/null || true
        wait "$session_pid" 2>/dev/null || true
    fi
    if ((status != 0)) && [[ -f "$runtime_dir/sessiond.log" ]]; then
        sed -n '1,200p' "$runtime_dir/sessiond.log" >&2
    fi
    if [[ "$runtime_dir" == "$runtime_root"/sessiond-idle.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

sessiond="$project_dir/target/release/touchbar-sessiond"
if [[ ! -x "$sessiond" ]]; then
    echo "release touchbar-sessiond is missing; run cargo build --release first" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" \
    TOUCHBAR_THEME="$project_dir/tests/fixtures/reduced-theme.toml" \
    "$sessiond" --socket "$socket_name" --frame-output "$runtime_dir/frame.bin" \
    --system-bar --no-plugins >"$runtime_dir/sessiond.log" 2>&1 &
session_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "idle compositor did not become ready" >&2
    exit 1
fi

# Let the startup scene settle before measuring the static interval. The
# process-level counters below belong to the event-loop thread on Linux.
sleep 0.1
context_before=$(awk '/^voluntary_ctxt_switches:/ {print $2}' "/proc/$session_pid/status")
cpu_before=$(awk '{print $14+$15}' "/proc/$session_pid/stat")
sleep 2
context_after=$(awk '/^voluntary_ctxt_switches:/ {print $2}' "/proc/$session_pid/status")
cpu_after=$(awk '{print $14+$15}' "/proc/$session_pid/stat")

context_delta=$((context_after - context_before))
cpu_delta=$((cpu_after - cpu_before))
if ((context_delta > 32)); then
    echo "static compositor woke $context_delta times in two seconds; expected at most 32" >&2
    exit 1
fi
if [[ $(rg -c '^scene=' "$runtime_dir/sessiond.log") -ne 1 ]]; then
    echo "static compositor rendered more than its one startup scene" >&2
    exit 1
fi

echo "sessiond-idle=ok duration_ms=2000 context_switches=$context_delta cpu_ticks=$cpu_delta frames=1"
