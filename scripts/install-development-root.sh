#!/bin/bash
set -euo pipefail

if [[ $EUID -ne 0 || -z ${PKEXEC_UID:-} || ! ${PKEXEC_UID} =~ ^[0-9]+$ ]]; then
    echo "install-development-root.sh must be invoked through pkexec" >&2
    exit 1
fi

source_root=${1:-}
if [[ -z "$source_root" || ! -d "$source_root" || -L "$source_root" ]]; then
    echo "invalid source repository" >&2
    exit 1
fi
source_root=$(realpath -- "$source_root")
if [[ $(stat -c %u -- "$source_root") != "$PKEXEC_UID" ]]; then
    echo "source repository is not owned by the invoking user" >&2
    exit 1
fi

require_regular() {
    local path=$1
    if [[ ! -f "$path" || -L "$path" ]]; then
        echo "required install source is missing or a symlink: $path" >&2
        exit 1
    fi
}

for binary in touchbard touchbar-sessiond touchbarctl touchbar-plugin-host touchbar-plugin-supervisor touchbar-secret-helper; do
    require_regular "$source_root/target/release/$binary"
done
for file in \
    packaging/systemd/touchbar.service \
    packaging/systemd/user/touchbar-session.service \
    packaging/udev/99-touchbar.rules \
    assets/touchbar.png \
    scripts/service-admin-root.sh \
    scripts/activate-installed-services.sh \
    scripts/rollback-to-tiny-dfr.sh; do
    require_regular "$source_root/$file"
done

hardware_was_active=false
if systemctl is-active --quiet touchbar.service; then
    hardware_was_active=true
fi

install -d -o root -g root -m 0755 \
    /usr/lib/touchbar /usr/share/touchbar /usr/lib/systemd/system \
    /usr/lib/systemd/user /usr/lib/udev/rules.d
for binary in touchbard touchbar-sessiond touchbar-plugin-host touchbar-plugin-supervisor touchbar-secret-helper; do
    install -o root -g root -m 0755 \
        "$source_root/target/release/$binary" "/usr/lib/touchbar/$binary"
done
install -o root -g root -m 0755 "$source_root/target/release/touchbarctl" /usr/bin/touchbarctl
install -o root -g root -m 0755 \
    "$source_root/scripts/service-admin-root.sh" /usr/lib/touchbar/service-admin-root
install -o root -g root -m 0755 \
    "$source_root/scripts/activate-installed-services.sh" /usr/bin/touchbar-activate
install -o root -g root -m 0755 \
    "$source_root/scripts/rollback-to-tiny-dfr.sh" /usr/bin/touchbar-rollback
install -o root -g root -m 0644 \
    "$source_root/assets/touchbar.png" /usr/share/touchbar/icon.png
install -o root -g root -m 0644 \
    "$source_root/packaging/systemd/touchbar.service" /usr/lib/systemd/system/touchbar.service
install -o root -g root -m 0644 \
    "$source_root/packaging/systemd/user/touchbar-session.service" /usr/lib/systemd/user/touchbar-session.service
install -o root -g root -m 0644 \
    "$source_root/packaging/udev/99-touchbar.rules" /usr/lib/udev/rules.d/99-touchbar.rules

systemctl daemon-reload
udevadm control --reload-rules
if [[ "$hardware_was_active" == true ]]; then
    systemctl restart touchbar.service
    systemctl is-active --quiet touchbar.service
    echo "development-hardware-service=restarted"
    echo "development-install=complete activation=preserved"
else
    echo "development-install=complete activation=required"
fi
