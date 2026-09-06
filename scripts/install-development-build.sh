#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
if [[ $EUID -eq 0 ]]; then
    echo "run this command as the graphical desktop user, not root" >&2
    exit 1
fi

session_was_active=false
if systemctl --user is-active --quiet touchbar-session.service; then
    session_was_active=true
fi

cargo build --locked --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbard -p touchbar-sessiond -p touchbar-cli \
    -p touchbar-plugin-host -p touchbar-plugin-supervisor
pkexec "$project_dir/scripts/install-development-root.sh" "$project_dir"
systemctl --user daemon-reload

for binary in touchbard touchbar-sessiond touchbar-plugin-host touchbar-plugin-supervisor touchbar-secret-helper; do
    cmp --silent "$project_dir/target/release/$binary" "/usr/lib/touchbar/$binary" || {
        echo "installed binary does not match the verified build: $binary" >&2
        exit 1
    }
done
cmp --silent "$project_dir/target/release/touchbarctl" /usr/bin/touchbarctl || {
    echo "installed binary does not match the verified build: touchbarctl" >&2
    exit 1
}

if [[ "$session_was_active" == true ]]; then
    systemctl --user restart touchbar-session.service
    systemctl --user is-active --quiet touchbar-session.service
    session_status=
    for _ in $(seq 1 300); do
        session_status=$(env -u TOUCHBAR_HOME \
            /usr/bin/touchbarctl session status --format json 2>/dev/null || true)
        [[ "$session_status" == *'"hardware_connected": true'* ]] && break
        sleep 0.1
    done
    if [[ "$session_status" != *'"hardware_connected": true'* ]]; then
        echo "updated user session did not establish an authenticated hardware connection" >&2
        echo "inspect it with: journalctl --user -u touchbar-session.service -n 100" >&2
        exit 1
    fi
    echo "development-user-session=restarted"
    echo "development-handoff=ready hardware_connected=true"
    echo "Development binaries and units are installed; previously active services now run the new build."
else
    echo "Development binaries and units are installed but inactive."
    echo "After the physical handoff test passes, run: touchbar-activate"
fi
