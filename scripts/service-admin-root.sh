#!/bin/bash
set -euo pipefail

if [[ $EUID -ne 0 || -z ${PKEXEC_UID:-} || ! ${PKEXEC_UID} =~ ^[0-9]+$ ]]; then
    echo "service-admin-root.sh must be invoked through pkexec by a logged-in user" >&2
    exit 1
fi

action=${1:-}
case "$action" in
    activate | rollback) ;;
    *)
        echo "usage: service-admin-root.sh activate|rollback" >&2
        exit 2
        ;;
esac

hardware_unit=/usr/lib/systemd/system/touchbar.service
hardware_binary=/usr/lib/touchbar/touchbard
if [[ ! -f /usr/lib/systemd/system/tiny-dfr.service && ! -f /etc/systemd/system/tiny-dfr.service ]]; then
    echo "tiny-dfr.service is not installed, so a verified rollback target is unavailable" >&2
    exit 1
fi

wait_hardware_stopped() {
    local state
    for _ in $(seq 1 100); do
        state=$(systemctl show touchbar.service --property=ActiveState --value 2>/dev/null || true)
        case "$state" in
            "" | inactive | failed) return 0 ;;
        esac
        sleep 0.05
    done
    echo "refusing to start tiny-dfr while touchbar.service state is ${state:-unknown}" >&2
    return 1
}

trigger_touchbar_display() {
    local count=0
    local driver_path
    local driver
    local name
    local sysfs_card
    for sysfs_card in /sys/class/drm/card*; do
        [[ -e "$sysfs_card" ]] || continue
        name=${sysfs_card##*/}
        [[ "$name" =~ ^card[0-9]+$ ]] || continue
        driver_path=$(readlink -f -- "$sysfs_card/device/driver" 2>/dev/null || true)
        driver=${driver_path##*/}
        case "$driver" in
            adp | appletbdrm)
                udevadm trigger --action=change "$sysfs_card"
                count=$((count + 1))
                ;;
        esac
    done
    if ((count != 1)); then
        echo "expected exactly one ADP Touch Bar DRM card, found $count" >&2
        return 1
    fi
    udevadm settle --timeout=10
    for _ in $(seq 1 100); do
        systemctl is-active --quiet dev-touchbar_display.device && return 0
        sleep 0.05
    done
    echo "Touch Bar systemd device alias did not become active" >&2
    return 1
}

if [[ "$action" == "rollback" ]]; then
    rm -f -- /etc/touchbar/enabled
    systemctl disable touchbar.service || true
    systemctl stop touchbar.service || true
    wait_hardware_stopped
    systemctl unmask tiny-dfr.service
    systemctl start tiny-dfr.service
    systemctl is-active --quiet tiny-dfr.service
    echo "hardware-service=rolled-back replacement=tiny-dfr.service"
    exit 0
fi

if [[ ! -f "$hardware_unit" || ! -x "$hardware_binary" ]]; then
    echo "TouchBar system files are not installed" >&2
    exit 1
fi

restore_tiny_dfr() {
    status=${1:-1}
    trap - EXIT INT TERM HUP
    if [[ $status -ne 0 ]]; then
        rm -f -- /etc/touchbar/enabled
        systemctl disable touchbar.service 2>/dev/null || true
        systemctl stop touchbar.service 2>/dev/null || true
        if wait_hardware_stopped; then
            systemctl unmask tiny-dfr.service 2>/dev/null || status=1
            systemctl start tiny-dfr.service 2>/dev/null || status=1
        else
            echo "WARNING: tiny-dfr was not started because touchbard did not stop" >&2
            status=1
        fi
    fi
    exit "$status"
}
trap 'restore_tiny_dfr $?' EXIT
trap 'restore_tiny_dfr 130' INT TERM HUP

# tiny-dfr is also started directly by its udev rules. Disabling it is not
# sufficient: masking is the deterministic ownership switch and is reversible.
systemctl mask --now tiny-dfr.service
install -d -o root -g root -m 0755 /etc/touchbar
install -o root -g root -m 0644 /dev/null /etc/touchbar/enabled
systemctl daemon-reload
trigger_touchbar_display
systemctl enable --now touchbar.service
systemctl is-active --quiet touchbar.service
trap - EXIT INT TERM HUP
echo "hardware-service=activated replacement=touchbar.service"
