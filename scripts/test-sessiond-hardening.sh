#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
sessiond="$project_dir/target/release/touchbar-sessiond"
if [[ ! -x "$sessiond" ]]; then
    echo "release touchbar-sessiond is missing; run cargo build --release first" >&2
    exit 1
fi
if [[ $EUID -eq 0 ]]; then
    echo "run this test as the graphical desktop user, not root" >&2
    exit 1
fi

# Match packaging/systemd/user/touchbar-session.service exactly. This probe
# caught EGL_DEFAULT_DISPLAY accidentally depending on the host X11 socket:
# PrivateTmp correctly hid /tmp/.X11-unix and exposed the bad dependency.
probe_id="touchbar-hardening-$$"
probe_unit="$probe_id.service"
probe_control="${XDG_RUNTIME_DIR:?}/$probe_id.sock"

set +e
probe_output=$(systemd-run --user --wait --collect --pipe \
    --unit="$probe_unit" \
    --property=RuntimeMaxSec=2s \
    --property=NoNewPrivileges=true \
    --property=ProtectSystem=strict \
    --property=ProtectHome=false \
    --property=PrivateTmp=true \
    --property=ProtectKernelTunables=true \
    --property=ProtectKernelModules=true \
    --property=ProtectKernelLogs=true \
    --property=ProtectControlGroups=true \
    --property=RestrictSUIDSGID=true \
    --property=LockPersonality=true \
    --property='RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6' \
    "$sessiond" \
    --socket "$probe_id" \
    --control-socket "$probe_control" \
    --system-bar \
    --no-plugins 2>&1)
probe_status=$?
set -e

printf '%s\n' "$probe_output"

# RuntimeMaxSec intentionally terminates the otherwise persistent daemon, so
# systemd-run returns nonzero. Readiness before that timeout is the assertion.
if [[ $probe_status -eq 0 ]]; then
    echo "hardening probe exited before its intentional runtime limit" >&2
    exit 1
fi
if ! grep -Fq 'compositor-renderer=' <<<"$probe_output" \
    || grep -Fq 'compositor-renderer=llvmpipe' <<<"$probe_output" \
    || ! grep -Fq 'dmabuf-device=' <<<"$probe_output" \
    || ! grep -Fq "ready socket=$probe_id " <<<"$probe_output" \
    || ! grep -Fq 'Finished with result: timeout' <<<"$probe_output"; then
    echo "touchbar-sessiond did not become GPU-ready under packaged hardening" >&2
    exit 1
fi

echo "sessiond-hardening=ok private-tmp=yes gpu=ready"
