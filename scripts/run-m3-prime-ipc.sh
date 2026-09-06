#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
probe_dir=$(mktemp -d /tmp/touchbar-prime-ipc.XXXXXX)
socket_path="$probe_dir/swapchain.sock"
client_log="$probe_dir/touchbar-sessiond.log"
client_pid=""

cleanup() {
    if [[ -n "$client_pid" ]]; then
        kill "$client_pid" 2>/dev/null || true
        wait "$client_pid" 2>/dev/null || true
    fi
    rm -f -- "$socket_path" "$client_log"
    rmdir -- "$probe_dir" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

cd "$project_root"
cargo build --release --bin touchbar-sessiond --bin touchbard

target/release/touchbar-sessiond --swapchain-probe "$socket_path" >"$client_log" 2>&1 &
client_pid=$!

for _ in {1..200}; do
    if [[ -S "$socket_path" ]]; then
        break
    fi
    if ! kill -0 "$client_pid" 2>/dev/null; then
        sed -n '1,200p' "$client_log"
        exit 1
    fi
    sleep 0.02
done
if [[ ! -S "$socket_path" ]]; then
    echo "touchbar-sessiond did not create its ADP swapchain socket" >&2
    exit 1
fi

pkexec "$project_root/target/release/touchbard" \
    --prime-serve "$socket_path"
wait "$client_pid"
client_pid=""
sed -n '1,200p' "$client_log"
