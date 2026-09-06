#!/bin/bash
set -euo pipefail

if ! systemctl is-active --quiet touchbar.service; then
    echo "touchbar.service must already be active" >&2
    exit 1
fi
if ! systemctl --user is-active --quiet touchbar-session.service; then
    echo "touchbar-session.service must already be active" >&2
    exit 1
fi

wait_for_status() {
    local scope=$1
    local expected=$2
    local attempts=${3:-200}
    local output=
    for _ in $(seq 1 "$attempts"); do
        output=$(touchbarctl "$scope" status --format json 2>/dev/null || true)
        if [[ "$output" == *"$expected"* ]]; then
            printf '%s\n' "$output"
            return 0
        fi
        sleep 0.1
    done
    echo "timed out waiting for status fragment: $expected" >&2
    printf '%s\n' "$output" >&2
    return 1
}

initial_hardware=$(touchbarctl hardware status --format json)
initial_session=$(touchbarctl session status --format json)
if [[ "$initial_hardware" != *'"recovery": "normal"'* \
    || "$initial_session" != *'"hardware_connected": true'* ]]; then
    echo "recovery acceptance requires a normal connected starting state" >&2
    printf '%s\n' "$initial_hardware" "$initial_session" >&2
    exit 1
fi

echo "Hold the physical Fn key continuously for eight seconds now."
echo "Expected: the trusted fallback replaces the user composition and stays visible after release."
wait_for_status hardware '"recovery": "fallback-locked"'
wait_for_status session '"hardware_connected": false' 100

echo "Release Fn completely, then hold it continuously for eight seconds again."
echo "Expected: the user composition reconnects automatically after the second hold."
wait_for_status hardware '"recovery": "normal"'
wait_for_status session '"hardware_connected": true' 100

echo "installed-recovery=ok entry=fn-hold fallback=latched exit=fn-hold reconnect=automatic"
