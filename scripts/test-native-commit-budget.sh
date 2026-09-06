#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/native-commit-budget.XXXXXX")
socket_name="commit-budget-$$"
server_pid=
chmod 700 "$runtime_dir"

cleanup() {
    status=$?
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if ((status != 0)); then
        sed -n '1,240p' "$runtime_dir/sessiond.log" >&2 || true
        sed -n '1,80p' "$runtime_dir/client.log" >&2 || true
    fi
    if [[ "$runtime_dir" == "$runtime_root"/native-commit-budget.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --release -p touchbar-sessiond --bin touchbar-sessiond \
    --example touchbar-commit-flood

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --no-plugins \
    >"$runtime_dir/sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
[[ -S "$runtime_dir/$socket_name" ]]

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/examples/touchbar-commit-flood" \
    >"$runtime_dir/client.log" 2>&1

for _ in $(seq 1 500); do
    rg -q '^client-disconnected=commit-flood ' "$runtime_dir/sessiond.log" && break
    sleep 0.01
done
rg -q '^commit-flood=disconnected ' "$runtime_dir/client.log"
rg -q '^client-disconnected=commit-flood rejected=256 ' "$runtime_dir/sessiond.log"

echo "native-commit-budget=ok rate=120 burst=8 sustained-rejections=256 action=disconnect"
