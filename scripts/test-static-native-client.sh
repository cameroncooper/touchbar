#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/static-native-client.XXXXXX")
socket_name="static-native-client-$$"
server_pid=
client_pid=
chmod 700 "$runtime_dir"
mkdir -m 700 "$runtime_dir/config"

cleanup() {
    status=$?
    if [[ -n "$client_pid" ]] && kill -0 "$client_pid" 2>/dev/null; then
        kill "$client_pid" 2>/dev/null || true
        wait "$client_pid" 2>/dev/null || true
    fi
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if ((status != 0)); then
        sed -n '1,200p' "$runtime_dir/sessiond.log" >&2 || true
        sed -n '1,120p' "$runtime_dir/client.log" >&2 || true
    fi
    if [[ "$runtime_dir" == "$runtime_root"/static-native-client.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --locked --release -p touchbar-sessiond -p touchbar-gl-demo

env XDG_RUNTIME_DIR="$runtime_dir" XDG_CONFIG_HOME="$runtime_dir/config" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --no-plugins \
    >"$runtime_dir/sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "static-client compositor did not become ready" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" XDG_CONFIG_HOME="$runtime_dir/config" \
    WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --static --require-hardware \
    >"$runtime_dir/client.log" 2>&1 &
client_pid=$!

for _ in $(seq 1 500); do
    rg -q '^scene=' "$runtime_dir/sessiond.log" && break
    kill -0 "$client_pid" 2>/dev/null || break
    sleep 0.01
done
if ! kill -0 "$client_pid" 2>/dev/null; then
    echo "static native client exited instead of waiting for events" >&2
    exit 1
fi
if [[ $(rg -c '^scene=' "$runtime_dir/sessiond.log") -ne 1 ]]; then
    echo "static native client did not settle after its initial commit" >&2
    exit 1
fi

cpu_before=$(awk '{print $14+$15}' "/proc/$client_pid/stat")
context_before=$(awk '/^voluntary_ctxt_switches:/ {print $2}' "/proc/$client_pid/status")
sleep 2
cpu_after=$(awk '{print $14+$15}' "/proc/$client_pid/stat")
context_after=$(awk '/^voluntary_ctxt_switches:/ {print $2}' "/proc/$client_pid/status")

frames=$(rg -c '^scene=' "$runtime_dir/sessiond.log")
cpu_delta=$((cpu_after - cpu_before))
context_delta=$((context_after - context_before))
if [[ "$frames" -ne 1 ]]; then
    echo "static native client caused $frames compositor frames; expected exactly one" >&2
    exit 1
fi
if ((cpu_delta > 1)); then
    echo "static native client used $cpu_delta CPU ticks while waiting; expected at most one" >&2
    exit 1
fi
if ((context_delta > 8)); then
    echo "static native client woke $context_delta times while waiting; expected at most eight" >&2
    exit 1
fi

rg -q 'configured plugin=touchbar.gles-demo .* renderer=Apple M1 .* transport=dmabuf' \
    "$runtime_dir/client.log"

echo "static-native-client=ok duration_ms=2000 frames=$frames cpu_ticks=$cpu_delta context_switches=$context_delta renderer=Apple-M1"
