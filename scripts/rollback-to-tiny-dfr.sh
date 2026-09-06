#!/bin/bash
set -euo pipefail

if [[ $EUID -eq 0 ]]; then
    echo "run this command as the graphical desktop user, not root" >&2
    exit 1
fi

systemctl --user disable --now touchbar-session.service 2>/dev/null || true
pkexec /usr/lib/touchbar/service-admin-root rollback
echo "tiny-dfr has been restored; installed TouchBar files were left intact."
