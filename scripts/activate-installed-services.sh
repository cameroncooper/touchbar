#!/bin/bash
set -euo pipefail

if [[ $EUID -eq 0 ]]; then
    echo "run this command as the graphical desktop user, not root" >&2
    exit 1
fi
if [[ ! -x /usr/lib/touchbar/touchbar-sessiond || ! -f /usr/lib/systemd/user/touchbar-session.service ]]; then
    echo "install the TouchBar package before activating its services" >&2
    exit 1
fi

pkexec /usr/lib/touchbar/service-admin-root activate
systemctl --user daemon-reload
if ! systemctl --user enable --now touchbar-session.service; then
    echo "touchbard is active with its safe fallback, but the user session service failed" >&2
    echo "inspect it with: systemctl --user status touchbar-session.service" >&2
    exit 1
fi

systemctl --user is-active --quiet touchbar-session.service
echo "TouchBar is active. tiny-dfr remains installed but masked for deterministic ownership."
