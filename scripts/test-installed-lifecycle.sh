#!/bin/bash
set -euo pipefail

mode=${1:---check}
if (($# > 1)) || [[ "$mode" != "--check" && "$mode" != "--suspend" ]]; then
    echo "usage: $0 [--check|--suspend]" >&2
    exit 2
fi
if [[ $EUID -eq 0 ]]; then
    echo "run this test as the graphical desktop user, not root" >&2
    exit 1
fi

require_active() {
    local scope=$1
    local unit=$2
    local attempts=${3:-1}
    for _ in $(seq 1 "$attempts"); do
        if [[ "$scope" == system ]]; then
            systemctl is-active --quiet "$unit" && return 0
        else
            systemctl --user is-active --quiet "$unit" && return 0
        fi
        sleep 0.1
    done
    echo "$unit did not become active" >&2
    return 1
}

require_socket() {
    local path=$1
    local attempts=${2:-1}
    for _ in $(seq 1 "$attempts"); do
        [[ -S "$path" ]] && return 0
        sleep 0.1
    done
    echo "$path did not become a socket" >&2
    return 1
}

require_session_hardware() {
    local attempts=${1:-1}
    local status
    for _ in $(seq 1 "$attempts"); do
        if status=$(touchbarctl session status --format json 2>/dev/null) \
            && [[ "$status" == *'"hardware_connected": true'* ]]; then
            return 0
        fi
        sleep 0.1
    done
    echo "touchbar-sessiond did not attach to touchbard" >&2
    return 1
}

if [[ ! -x /usr/bin/touchbarctl \
    || ! -f /usr/lib/systemd/system/touchbar.service \
    || ! -f /usr/lib/systemd/user/touchbar-session.service ]]; then
    echo "the TouchBar services are not installed" >&2
    echo "install them with: ./scripts/install-development-build.sh" >&2
    exit 1
fi
if [[ ! -e /etc/touchbar/enabled ]]; then
    echo "the installed services have not been activated" >&2
    echo "activate them with: touchbar-activate" >&2
    exit 1
fi
if [[ $(systemctl is-enabled tiny-dfr.service 2>/dev/null || true) != masked* ]]; then
    echo "tiny-dfr.service must be masked while touchbard owns the hardware" >&2
    exit 1
fi
if [[ $(systemctl is-enabled touchbar.service 2>/dev/null || true) != enabled ]]; then
    echo "touchbar.service is not enabled" >&2
    exit 1
fi
if [[ $(systemctl --user is-enabled touchbar-session.service 2>/dev/null || true) != enabled ]]; then
    echo "touchbar-session.service is not enabled for graphical sessions" >&2
    exit 1
fi

require_active system touchbar.service
require_active user touchbar-session.service
require_socket /run/touchbar/hardware.sock
touchbarctl hardware status
touchbarctl session status
require_session_hardware

if [[ "$mode" == "--check" ]]; then
    echo "installed-lifecycle=ok suspend=not-requested"
    exit 0
fi

started_at=$(date --iso-8601=seconds)
echo "Suspending now. This script will continue after the machine resumes."
systemctl suspend

# Device units and udev may need a moment to settle after the graphical session
# resumes. Both daemons have restart/reconnect loops, so accept up to 30 seconds.
require_active system touchbar.service 300
require_socket /run/touchbar/hardware.sock 300
require_active user touchbar-session.service 300
require_session_hardware 300
touchbarctl hardware status
touchbarctl session status

echo "Recent hardware lifecycle log:"
journalctl -u touchbar.service --since "$started_at" --no-pager -n 80
echo "Recent user-session lifecycle log:"
journalctl --user -u touchbar-session.service --since "$started_at" --no-pager -n 80
echo "installed-lifecycle=ok suspend=passed"
