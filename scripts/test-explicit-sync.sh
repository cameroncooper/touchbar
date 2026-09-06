#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/explicit-sync.XXXXXX")
server_pid=

cleanup() {
    status=$?
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if ((status != 0)); then
        find "$runtime_dir" -type f -maxdepth 2 -name '*.log' -print -exec sed -n '1,220p' {} \; >&2 || true
    fi
    if [[ "$runtime_dir" == "$runtime_root"/explicit-sync.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --locked --release -p touchbar-sessiond \
    --bin touchbar-sessiond --example touchbar-acquire-fence-abuse
cargo build --locked --release -p touchbar-gl-demo --bin touchbar-gl-demo

normal_dir="$runtime_dir/normal"
mkdir -m 700 "$normal_dir"
normal_socket="explicit-sync-normal-$$"
env XDG_RUNTIME_DIR="$normal_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$normal_socket" --no-plugins --exit-after-client \
    >"$normal_dir/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 500); do
    [[ -S "$normal_dir/$normal_socket" ]] && break
    kill -0 "$server_pid" 2>/dev/null || break
    sleep 0.01
done
[[ -S "$normal_dir/$normal_socket" ]]
env XDG_RUNTIME_DIR="$normal_dir" WAYLAND_DISPLAY="$normal_socket" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --frames 120 --require-hardware \
    >"$normal_dir/client.log" 2>&1
wait "$server_pid"
server_pid=
rg -q 'client-summary .* frames=120 transport=dmabuf' "$normal_dir/client.log"
rg -q 'summary frames=120 .* invalid=0 dmabuf=120 shm=0 explicit_sync=120 implicit_sync=0 ' \
    "$normal_dir/server.log"
measured_fps=$(sed -n 's/.* fps=\([0-9.]*\) .*/\1/p' "$normal_dir/server.log")
awk -v fps="$measured_fps" 'BEGIN { exit !(fps >= 55.0) }'

for mode in duplicate no-buffer shm invalid; do
    mode_dir="$runtime_dir/$mode"
    mkdir -m 700 "$mode_dir"
    socket_name="explicit-sync-$mode-$$"
    env XDG_RUNTIME_DIR="$mode_dir" \
        "$project_dir/target/release/touchbar-sessiond" \
        --socket "$socket_name" --no-plugins \
        >"$mode_dir/server.log" 2>&1 &
    server_pid=$!
    for _ in $(seq 1 500); do
        [[ -S "$mode_dir/$socket_name" ]] && break
        kill -0 "$server_pid" 2>/dev/null || break
        sleep 0.01
    done
    [[ -S "$mode_dir/$socket_name" ]]
    env XDG_RUNTIME_DIR="$mode_dir" WAYLAND_DISPLAY="$socket_name" \
        "$project_dir/target/release/examples/touchbar-acquire-fence-abuse" "$mode" \
        >"$mode_dir/client.log" 2>&1
    rg -q "acquire-fence-abuse=disconnected mode=$mode" "$mode_dir/client.log"
    case "$mode" in
        duplicate) expected_code=0 ;;
        no-buffer) expected_code=1 ;;
        shm) expected_code=2 ;;
        invalid) expected_code=3 ;;
    esac
    rg -q "error $expected_code:" "$mode_dir/client.log"
    if kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid"
    fi
    wait "$server_pid" 2>/dev/null || true
    server_pid=
done

echo "explicit-sync=ok renderer=Apple-M1 frames=120 malformed=4 fps=$measured_fps"
