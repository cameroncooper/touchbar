#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/power-aware-animation.XXXXXX")
power_root="$runtime_dir/power_supply"
server_pid=
chmod 700 "$runtime_dir"
install -d -m 700 "$power_root/battery" "$power_root/adapter"
printf 'Battery\n' >"$power_root/battery/type"
printf 'Mains\n' >"$power_root/adapter/type"

cleanup() {
    status=$?
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if ((status != 0)); then
        for log in "$runtime_dir"/*.log; do
            [[ -f "$log" ]] && sed -n '1,220p' "$log" >&2
        done
    fi
    if [[ "$runtime_dir" == "$runtime_root"/power-aware-animation.* \
        && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --release -p touchbar-sessiond -p touchbar-gl-demo -p touchbar-cli

run_case() {
    local name=$1
    local online=$2
    local expected_power=$3
    local expected_hz=$4
    local frames=$5
    local minimum_fps=$6
    local maximum_fps=$7
    local case_dir="$runtime_dir/$name"
    local socket_name="power-$name-$$"
    local control_socket="$case_dir/store/control.sock"
    install -d -m 700 "$case_dir" "$case_dir/store"
    printf '%s\n' "$online" >"$power_root/adapter/online"

    env XDG_RUNTIME_DIR="$case_dir" \
        TOUCHBAR_HOME="$case_dir/store" \
        TOUCHBAR_THEME="$project_dir/tests/fixtures/power-aware-theme.toml" \
        TOUCHBAR_POWER_SUPPLY_ROOT="$power_root" \
        "$project_dir/target/release/touchbar-sessiond" \
        --socket "$socket_name" --control-socket "$control_socket" \
        --frame-output "$case_dir/frame.bin" --no-plugins --exit-after-client \
        >"$case_dir/sessiond.log" 2>&1 &
    server_pid=$!

    for _ in $(seq 1 500); do
        [[ -S "$case_dir/$socket_name" && -S "$control_socket" ]] && break
        kill -0 "$server_pid" 2>/dev/null || break
        sleep 0.01
    done
    [[ -S "$case_dir/$socket_name" && -S "$control_socket" ]]

    status=$(TOUCHBAR_HOME="$case_dir/store" \
        "$project_dir/target/release/touchbarctl" session status --format json)
    [[ "$status" == *"\"power_source\": \"$expected_power\""* ]]
    [[ "$status" == *"\"animation_frame_rate_hz\": $expected_hz"* ]]

    env XDG_RUNTIME_DIR="$case_dir" WAYLAND_DISPLAY="$socket_name" \
        "$project_dir/target/release/touchbar-gl-demo" \
        --frames "$frames" --plugin-id "touchbar.power-$name" \
        --fixed-width 160 --require-hardware \
        >"$case_dir/client.log" 2>&1
    wait "$server_pid"
    server_pid=

    rg -q "power-source=$power_root state=$expected_power" "$case_dir/sessiond.log"
    rg -q "client-summary plugin=touchbar.power-$name frames=$frames transport=dmabuf" \
        "$case_dir/client.log"
    summary=$(rg '^summary ' "$case_dir/sessiond.log")
    measured_fps=$(sed -n 's/.* fps=\([0-9.]*\) .*/\1/p' <<<"$summary")
    awk -v fps="$measured_fps" -v minimum="$minimum_fps" -v maximum="$maximum_fps" \
        'BEGIN { exit !(fps >= minimum && fps <= maximum) }'
}

run_case external 1 external 60 61 50 70
run_case battery 0 battery 30 31 25 35

echo "power-aware-animation=ok external=60Hz battery=30Hz source=fake-sysfs renderer=Apple-M1"
