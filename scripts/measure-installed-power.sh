#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
mode=${1:---check}
sample_seconds=${2:-20}

if (($# > 2)) || [[ "$mode" != "--check" && "$mode" != "--run" ]]; then
    echo "usage: $0 [--check | --run [SECONDS]]" >&2
    exit 2
fi
if [[ ! "$sample_seconds" =~ ^[0-9]+$ ]] || ((sample_seconds < 10 || sample_seconds > 20)); then
    echo "sample duration must be an integer from 10 through 20 seconds" >&2
    exit 2
fi

if [[ "$mode" == "--check" ]]; then
    echo "power-acceptance=ready method=ABBA sample_seconds=$sample_seconds max_before_dim_seconds=20"
    echo "Run --run only after installing and activating the fresh v1 services, unplugging AC, and closing variable background workloads."
    exit 0
fi
if [[ $EUID -eq 0 ]]; then
    echo "run this measurement as the graphical desktop user, not root" >&2
    exit 1
fi
for command in cargo systemctl rg awk; do
    command -v "$command" >/dev/null || {
        echo "required command is unavailable: $command" >&2
        exit 1
    }
done

systemctl is-active --quiet touchbar.service || {
    echo "touchbar.service must be active" >&2
    exit 1
}
systemctl --user is-active --quiet touchbar-session.service || {
    echo "touchbar-session.service must be active" >&2
    exit 1
}
[[ -S /run/touchbar/hardware.sock ]] || {
    echo "the fresh v1 hardware socket is unavailable" >&2
    exit 1
}

mapfile -t battery_candidates < <(
    for supply in /sys/class/power_supply/*; do
        [[ -r "$supply/type" && -r "$supply/energy_now" ]] || continue
        [[ $(<"$supply/type") == Battery ]] || continue
        printf '%s\n' "$supply"
    done
)
if ((${#battery_candidates[@]} != 1)); then
    echo "exactly one battery with an energy_now counter is required" >&2
    exit 1
fi
battery=${battery_candidates[0]}
if [[ ! -r "$battery/status" ]] || [[ $(<"$battery/status") != Discharging ]]; then
    echo "battery must report Discharging; unplug external power before measuring" >&2
    exit 1
fi
for supply in /sys/class/power_supply/*; do
    [[ -r "$supply/type" && -r "$supply/online" ]] || continue
    case $(<"$supply/type") in
        Mains|USB|USB_C|USB_PD)
            if [[ $(<"$supply/online") == 1 ]]; then
                echo "external power is still online at $supply" >&2
                exit 1
            fi
            ;;
    esac
done
initial_energy=$(<"$battery/energy_now")
if [[ ! "$initial_energy" =~ ^[0-9]+$ ]] || ((initial_energy <= 0)); then
    echo "battery energy_now is not usable on this boot" >&2
    exit 1
fi

backlight=
for candidate in /sys/class/backlight/*; do
    case ${candidate##*/} in
        appletb_backlight|228200000.display-pipe.0|228600000.dsi.0)
            [[ -r "$candidate/brightness" ]] || continue
            if [[ -n "$backlight" ]]; then
                echo "multiple supported Touch Bar backlights were found" >&2
                exit 1
            fi
            backlight=$candidate
            ;;
    esac
done
[[ -n "$backlight" ]] || {
    echo "no supported Touch Bar backlight counter was found" >&2
    exit 1
}

runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/power-acceptance.XXXXXX")
results="$runtime_dir/results.tsv"
printf 'condition\tenergy_uwh\telapsed_ms\tenergy_watts\tpower_now_watts\tcpu_busy_percent\tbrightness\tactual_brightness\n' >"$results"
session_pid=
client_pid=

stop_phase() {
    if [[ -n "$client_pid" ]]; then
        if kill -0 "$client_pid" 2>/dev/null; then
            kill "$client_pid" 2>/dev/null || true
        fi
        wait "$client_pid" 2>/dev/null || true
    fi
    client_pid=
    if [[ -n "$session_pid" ]]; then
        if kill -0 "$session_pid" 2>/dev/null; then
            kill "$session_pid" 2>/dev/null || true
        fi
        wait "$session_pid" 2>/dev/null || true
    fi
    session_pid=
}

cleanup() {
    status=$?
    stop_phase
    systemctl --user start touchbar-session.service >/dev/null 2>&1 || true
    if ((status != 0)); then
        echo "power measurement failed; logs retained in $runtime_dir" >&2
    fi
}
trap cleanup EXIT INT TERM

read_cpu() {
    local label user nice system idle iowait irq softirq steal guest guest_nice
    read -r label user nice system idle iowait irq softirq steal guest guest_nice < /proc/stat
    printf '%s %s\n' \
        "$((user + nice + system + idle + iowait + irq + softirq + steal))" \
        "$((idle + iowait))"
}

wait_for_log() {
    local pattern=$1
    local log=$2
    local pid=$3
    for _ in $(seq 1 500); do
        rg -q "$pattern" "$log" 2>/dev/null && return 0
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.02
    done
    echo "timed out waiting for '$pattern' in $log" >&2
    sed -n '1,200p' "$log" >&2 || true
    return 1
}

measure_phase() {
    local condition=$1
    local phase_number=$2
    local phase_dir="$runtime_dir/$phase_number-$condition"
    local socket_name="touchbar-power-$phase_number-$$"
    local server_log="$phase_dir/session.log"
    local client_log="$phase_dir/client.log"
    local power_sum=0
    local power_count=0
    local energy_before energy_after energy_delta
    local brightness_before brightness_now actual_before actual_now
    local started_ns ended_ns elapsed_ms
    local cpu_before cpu_idle_before cpu_after cpu_idle_after cpu_delta cpu_idle_delta cpu_busy

    mkdir -p "$phase_dir/config"
    env XDG_RUNTIME_DIR="$phase_dir" XDG_CONFIG_HOME="$phase_dir/config" \
        "$project_dir/target/release/touchbar-sessiond" \
        --socket "$socket_name" \
        --hardware-socket /run/touchbar/hardware.sock \
        --system-bar --no-plugins \
        --control-socket "$phase_dir/control.sock" \
        >"$server_log" 2>&1 &
    session_pid=$!
    wait_for_log 'hardware-output=ready' "$server_log" "$session_pid"

    client_args=(
        --require-hardware
        --plugin-id touchbar.power-benchmark
        --item-id scene
        --fixed-width 2008
        --frames 1000000
    )
    if [[ "$condition" == static ]]; then
        client_args+=(--static)
    fi
    env XDG_RUNTIME_DIR="$phase_dir" WAYLAND_DISPLAY="$socket_name" \
        "$project_dir/target/release/touchbar-gl-demo" "${client_args[@]}" \
        >"$client_log" 2>&1 &
    client_pid=$!
    wait_for_log 'configured plugin=touchbar.power-benchmark' "$client_log" "$client_pid"

    # Let hardware handoff and the first GPU frame settle. The complete phase
    # remains below the 30-second OLED dim threshold.
    sleep 2
    brightness_before=$(<"$backlight/brightness")
    if [[ ! "$brightness_before" =~ ^[0-9]+$ ]] || ((brightness_before <= 0)); then
        echo "Touch Bar backlight did not wake for the $condition phase" >&2
        return 1
    fi
    actual_before=$brightness_before
    if [[ -r "$backlight/actual_brightness" ]]; then
        actual_before=$(<"$backlight/actual_brightness")
        if [[ ! "$actual_before" =~ ^[0-9]+$ ]] || ((actual_before <= 0)); then
            echo "Touch Bar actual brightness is not visible for the $condition phase" >&2
            return 1
        fi
    fi
    energy_before=$(<"$battery/energy_now")
    [[ "$energy_before" =~ ^[0-9]+$ ]] || {
        echo "battery energy_now became malformed" >&2
        return 1
    }
    read -r cpu_before cpu_idle_before < <(read_cpu)
    started_ns=$(date +%s%N)

    for _ in $(seq 1 "$sample_seconds"); do
        sleep 1
        kill -0 "$client_pid" 2>/dev/null || {
            echo "$condition rendering client exited during measurement" >&2
            return 1
        }
        kill -0 "$session_pid" 2>/dev/null || {
            echo "$condition session compositor exited during measurement" >&2
            return 1
        }
        [[ $(<"$battery/status") == Discharging ]] || {
            echo "battery stopped discharging during measurement" >&2
            return 1
        }
        brightness_now=$(<"$backlight/brightness")
        if [[ "$brightness_now" != "$brightness_before" ]]; then
            echo "Touch Bar brightness changed during $condition phase" >&2
            return 1
        fi
        actual_now=$brightness_now
        if [[ -r "$backlight/actual_brightness" ]]; then
            actual_now=$(<"$backlight/actual_brightness")
        fi
        if [[ "$actual_now" != "$actual_before" ]]; then
            echo "Touch Bar actual brightness changed during $condition phase" >&2
            return 1
        fi
        if [[ -r "$battery/power_now" ]]; then
            power_now=$(<"$battery/power_now")
            if [[ "$power_now" =~ ^[0-9]+$ ]] && ((power_now > 0)); then
                power_sum=$((power_sum + power_now))
                power_count=$((power_count + 1))
            fi
        fi
    done

    ended_ns=$(date +%s%N)
    read -r cpu_after cpu_idle_after < <(read_cpu)
    energy_after=$(<"$battery/energy_now")
    [[ "$energy_after" =~ ^[0-9]+$ ]] || {
        echo "battery energy_now became malformed" >&2
        return 1
    }
    if ((energy_after >= energy_before)); then
        echo "energy_now did not decrease during $condition phase" >&2
        return 1
    fi
    energy_delta=$((energy_before - energy_after))
    elapsed_ms=$(((ended_ns - started_ns) / 1000000))
    cpu_delta=$((cpu_after - cpu_before))
    cpu_idle_delta=$((cpu_idle_after - cpu_idle_before))
    cpu_busy=$(awk -v total="$cpu_delta" -v idle="$cpu_idle_delta" \
        'BEGIN { if (total <= 0) print "0.000"; else printf "%.3f", 100 * (total - idle) / total }')
    energy_watts=$(awk -v energy="$energy_delta" -v elapsed="$elapsed_ms" \
        'BEGIN { printf "%.6f", energy * 3.6 / elapsed }')
    power_watts=$(awk -v total="$power_sum" -v count="$power_count" \
        'BEGIN { if (count == 0) print "unavailable"; else printf "%.6f", total / count / 1000000 }')
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$condition" "$energy_delta" "$elapsed_ms" "$energy_watts" \
        "$power_watts" "$cpu_busy" "$brightness_before" "$actual_before" | tee -a "$results"

    stop_phase
    sleep 1
}

echo "Building the exact release binaries before changing the user session."
cargo build --locked --release -p touchbar-sessiond -p touchbar-gl-demo
echo "This explicitly requested test temporarily replaces touchbar-session.service."
echo "Do not touch the keyboard, pointer, or Touch Bar during the four short samples."
echo "Results and logs will be retained in $runtime_dir"

systemctl --user stop touchbar-session.service
for _ in $(seq 1 100); do
    systemctl --user is-active --quiet touchbar-session.service || break
    sleep 0.05
done
if systemctl --user is-active --quiet touchbar-session.service; then
    echo "touchbar-session.service did not stop" >&2
    exit 1
fi

measure_phase static 1
measure_phase animated 2
measure_phase animated 3
measure_phase static 4

static_watts=$(awk -F '\t' '$1 == "static" { energy += $2; elapsed += $3 } END { printf "%.6f", energy * 3.6 / elapsed }' "$results")
animated_watts=$(awk -F '\t' '$1 == "animated" { energy += $2; elapsed += $3 } END { printf "%.6f", energy * 3.6 / elapsed }' "$results")
delta_watts=$(awk -v static="$static_watts" -v animated="$animated_watts" \
    'BEGIN { printf "%.6f", animated - static }')

echo "power-acceptance=ok method=ABBA static_watts=$static_watts animated_watts=$animated_watts delta_watts=$delta_watts samples=4 duration_seconds=$sample_seconds results=$results"
