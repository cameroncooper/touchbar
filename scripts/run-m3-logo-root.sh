#!/bin/bash
set -euo pipefail

invoking_uid=${PKEXEC_UID:-${SUDO_UID:-}}
if [[ $EUID -ne 0 || ! "$invoking_uid" =~ ^[0-9]+$ ]]; then
    echo "run-m3-logo-root.sh must be invoked through pkexec or sudo" >&2
    exit 1
fi

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
expected_binary="$project_dir/target/release/touchbard"
binary=${1:-}
logo=${2:-}
duration=${3:-10}
action=${4:---display}
payload_path=${5:-}

if [[ "$binary" != "$expected_binary" || ! -x "$binary" ]]; then
    echo "refusing unexpected demo binary: $binary" >&2
    exit 1
fi
if [[ ! -f "$logo" ]]; then
    echo "logo does not exist: $logo" >&2
    exit 1
fi
if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 1
fi
if [[ "$action" != "--authorize" && "$action" != "--display" && "$action" != "--animate" && "$action" != "--scene" && "$action" != "--direct" && "$action" != "--serve-demo" && "$action" != "--restart-demo" ]]; then
    echo "refusing unexpected demo action: $action" >&2
    exit 1
fi
if [[ "$action" == "--authorize" ]]; then
    echo "physical-demo-authorization=ok"
    exit 0
fi
if [[ "$action" == "--scene" ]]; then
    if [[ ! -f "$payload_path" ]]; then
        echo "frame stream does not exist: $payload_path" >&2
        exit 1
    fi
    resolved_payload=$(realpath -- "$payload_path")
    if [[ "$resolved_payload" != "$project_dir"/run/* ]]; then
        echo "refusing frame stream outside the project runtime directory" >&2
        exit 1
    fi
fi
if [[ "$action" == "--direct" ]]; then
    if [[ ! -S "$payload_path" ]]; then
        echo "ADP output socket does not exist: $payload_path" >&2
        exit 1
    fi
    resolved_payload=$(realpath -- "$payload_path")
    if [[ ! "$resolved_payload" =~ ^/tmp/touchbar-direct-[0-9]+-[0-9]+\.sock$ ]]; then
        echo "refusing unexpected ADP output socket path" >&2
        exit 1
    fi
    socket_owner=$(stat -c %u -- "$resolved_payload")
    if [[ "$socket_owner" != "$invoking_uid" ]]; then
        echo "refusing ADP output socket not owned by the invoking user" >&2
        exit 1
    fi
fi
touchbard_was_active=false
tiny_dfr_was_active=false
if systemctl is-active --quiet touchbar.service; then
    touchbard_was_active=true
fi
if systemctl is-active --quiet tiny-dfr.service; then
    tiny_dfr_was_active=true
fi
if [[ "$touchbard_was_active" == true && "$tiny_dfr_was_active" == true ]]; then
    echo "refusing ambiguous Touch Bar ownership: touchbard and tiny-dfr are both active" >&2
    exit 1
fi

demo_runtime=
service_pid=
if [[ "$action" == "--serve-demo" || "$action" == "--restart-demo" ]]; then
    if [[ ! "$payload_path" =~ ^/run/touchbar-demo-${invoking_uid}-[0-9]+/hardware\.sock$ ]]; then
        echo "refusing unexpected hardware service demo socket path" >&2
        exit 1
    fi
    demo_runtime=${payload_path%/*}
    if [[ -e "$demo_runtime" || -L "$demo_runtime" ]]; then
        echo "refusing pre-existing hardware service demo directory" >&2
        exit 1
    fi
    # The invoking user needs directory traversal to reach the 0666 socket;
    # listing and mutation remain unavailable. SO_PEERCRED plus active-seat
    # authentication is the service boundary.
    install -d -o root -g root -m 0711 -- "$demo_runtime"
fi

restore_touchbar() {
    status=$?
    trap - EXIT INT TERM HUP
    if [[ -n "$service_pid" ]]; then
        kill "$service_pid" 2>/dev/null || true
        wait "$service_pid" 2>/dev/null || true
        service_pid=
    fi
    if [[ -n "$demo_runtime" && "$demo_runtime" =~ ^/run/touchbar-demo-[0-9]+-[0-9]+$ ]]; then
        rm -f -- "$demo_runtime/hardware.sock" "$demo_runtime/recovery-fallback"
        rmdir -- "$demo_runtime" 2>/dev/null || true
    fi
    restored_owner=none
    if [[ "$touchbard_was_active" == true ]]; then
        echo "restoring touchbar.service"
        systemctl start touchbar.service || status=$?
        if systemctl is-active --quiet touchbar.service; then
            restored_owner=touchbar.service
        else
            echo "WARNING: touchbar.service did not return to active state" >&2
            status=1
        fi
    elif [[ "$tiny_dfr_was_active" == true ]]; then
        echo "restoring tiny-dfr.service"
        systemctl start tiny-dfr.service || status=$?
        if systemctl is-active --quiet tiny-dfr.service; then
            restored_owner=tiny-dfr.service
        else
            echo "WARNING: tiny-dfr.service did not return to active state" >&2
            status=1
        fi
    fi
    if [[ "$restored_owner" != none || \
        ("$touchbard_was_active" == false && "$tiny_dfr_was_active" == false) ]]; then
        echo "physical-owner=restored service=$restored_owner"
    fi
    exit "$status"
}
trap restore_touchbar EXIT INT TERM HUP

adp_fe_irq_total() {
    awk '
        $NF == "adp-fe" {
            total = 0
            for (field = 2; field <= NF; field++) {
                if ($field !~ /^[0-9]+$/)
                    break
                total += $field
            }
            print total
            found = 1
        }
        END {
            if (!found)
                print 0
        }
    ' /proc/interrupts
}

if [[ "$touchbard_was_active" == true ]]; then
    echo "temporarily stopping touchbar.service"
    systemctl stop touchbar.service
    for _ in $(seq 1 100); do
        systemctl is-active --quiet touchbar.service || break
        sleep 0.05
    done
    if systemctl is-active --quiet touchbar.service; then
        echo "touchbar.service did not stop" >&2
        exit 1
    fi
elif [[ "$tiny_dfr_was_active" == true ]]; then
    echo "temporarily stopping tiny-dfr.service"
    systemctl stop tiny-dfr.service
    for _ in $(seq 1 100); do
        systemctl is-active --quiet tiny-dfr.service || break
        sleep 0.05
    done
    if systemctl is-active --quiet tiny-dfr.service; then
        echo "tiny-dfr.service did not stop" >&2
        exit 1
    fi
fi

if [[ "$action" == "--restart-demo" ]]; then
    first_phase=$((duration / 2))
    second_phase=$((duration - first_phase))
    "$binary" --socket "$payload_path" &
    service_pid=$!
    sleep "$first_phase"
    kill "$service_pid"
    wait "$service_pid"
    service_pid=
    echo "restarting touchbard for lifecycle acceptance"
    "$binary" --socket "$payload_path" &
    service_pid=$!
    sleep "$second_phase"
    kill "$service_pid"
    wait "$service_pid"
    service_pid=
elif [[ "$action" == "--serve-demo" ]]; then
    set +e
    timeout --signal=TERM "$duration" \
        "$binary" --socket "$payload_path"
    service_status=$?
    set -e
    if [[ $service_status -ne 0 && $service_status -ne 124 && $service_status -ne 143 ]]; then
        exit "$service_status"
    fi
elif [[ "$action" == "--scene" ]]; then
    env TOUCHBAR_PHYSICAL_DEMO=1 \
        "$binary" --scene "$resolved_payload" --duration "$duration"
elif [[ "$action" == "--direct" ]]; then
    env TOUCHBAR_PHYSICAL_DEMO=1 \
        "$binary" --direct "$resolved_payload" --duration "$duration"
else
    if [[ "$action" == "--animate" ]]; then
        adp_fe_irq_before=$(adp_fe_irq_total)
    fi
    env TOUCHBAR_PHYSICAL_DEMO=1 \
        "$binary" "$action" --logo "$logo" --duration "$duration"
    if [[ "$action" == "--animate" ]]; then
        adp_fe_irq_after=$(adp_fe_irq_total)
        adp_fe_irq_delta=$((adp_fe_irq_after - adp_fe_irq_before))
        adp_fe_irq_hz=$(awk -v count="$adp_fe_irq_delta" -v seconds="$duration" \
            'BEGIN { printf "%.2f", count / seconds }')
        echo "adp-fe-irq-summary interrupts=$adp_fe_irq_delta duration_seconds=$duration irq_hz=$adp_fe_irq_hz"
    fi
fi
