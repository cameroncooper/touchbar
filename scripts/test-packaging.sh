#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
stage_dir=$(mktemp -d "$runtime_root/package-verify.XXXXXX")

cleanup() {
    if [[ "$stage_dir" == "$runtime_root"/package-verify.* && -d "$stage_dir" ]]; then
        rm -r -- "$stage_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --locked --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbard -p touchbar-sessiond -p touchbar-cli \
    -p touchbar-plugin-host -p touchbar-plugin-supervisor \
    -p touchbar-gl-demo -p touchbar-ui-demo

# Plugin packages contain their compiled Wasm component. Always refresh every
# first-party package before lifecycle tests so a protocol rename cannot leave
# a source-clean tree shipping stale guest imports.
for pack in controls media hyprland capture command-deck; do
    "$project_dir/target/release/touchbarctl" plugin build \
        --package "$project_dir/plugins/$pack"
    "$project_dir/target/release/touchbarctl" plugin check \
        --package "$project_dir/plugins/$pack"
done

install -m 0644 "$project_dir/packaging/systemd/touchbar.service" \
    "$stage_dir/touchbar.service"
install -m 0644 "$project_dir/packaging/systemd/user/touchbar-session.service" \
    "$stage_dir/touchbar-session.service"

# systemd-analyze validates ExecStart targets. The package intentionally is not
# installed while this check runs, so point temporary copies at the exact ELF
# files that the installer will copy. The checked-in units remain untouched.
sed -i \
    "s#/usr/lib/touchbar/touchbard#$project_dir/target/release/touchbard#" \
    "$stage_dir/touchbar.service"
sed -i \
    "s#/usr/lib/touchbar/touchbar-sessiond#$project_dir/target/release/touchbar-sessiond#" \
    "$stage_dir/touchbar-session.service"

systemd-analyze verify "$stage_dir/touchbar.service"
systemd-analyze --user verify "$stage_dir/touchbar-session.service"
udevadm verify "$project_dir/packaging/udev/99-touchbar.rules"

rg -q '^ExecStart=/usr/lib/touchbar/touchbard ' \
    "$project_dir/packaging/systemd/touchbar.service"
rg -q '^ExecStart=/usr/lib/touchbar/touchbar-sessiond ' \
    "$project_dir/packaging/systemd/user/touchbar-session.service"
rg -Fxq 'Delegate=cpu memory pids' \
    "$project_dir/packaging/systemd/user/touchbar-session.service"
rg -Fxq 'ProtectControlGroups=private' \
    "$project_dir/packaging/systemd/user/touchbar-session.service"
rg -q '^for binary in touchbard touchbar-sessiond touchbarctl touchbar-plugin-host touchbar-plugin-supervisor touchbar-secret-helper; do$' \
    "$project_dir/scripts/install-development-root.sh"
rg -Fq '"$source_root/target/release/$binary"' \
    "$project_dir/scripts/install-development-root.sh"
rg -Fq '"$source_root/target/release/touchbarctl" /usr/bin/touchbarctl' \
    "$project_dir/scripts/install-development-root.sh"
rg -Fq '"$source_root/assets/touchbar.png" /usr/share/touchbar/icon.png' \
    "$project_dir/scripts/install-development-root.sh"
rg -Fq 'if systemctl is-active --quiet touchbar.service; then' \
    "$project_dir/scripts/install-development-root.sh"
rg -Fq 'systemctl restart touchbar.service' \
    "$project_dir/scripts/install-development-root.sh"
rg -Fq 'if systemctl --user is-active --quiet touchbar-session.service; then' \
    "$project_dir/scripts/install-development-build.sh"
rg -Fq 'systemctl --user restart touchbar-session.service' \
    "$project_dir/scripts/install-development-build.sh"
rg -Fq 'development-handoff=ready hardware_connected=true' \
    "$project_dir/scripts/install-development-build.sh"
rg -Fq 'temporarily stopping touchbar.service' \
    "$project_dir/scripts/run-m3-logo-root.sh"
rg -Fq 'physical-owner=restored service=$restored_owner' \
    "$project_dir/scripts/run-m3-logo-root.sh"
for script in "$project_dir"/scripts/*.sh; do
    bash -n "$script"
done

"$project_dir/scripts/test-sessiond-hardening.sh"
"$project_dir/scripts/test-sessiond-idle.sh"
"$project_dir/scripts/test-static-native-client.sh"
"$project_dir/scripts/test-profile-control-live.sh"
"$project_dir/scripts/test-native-commit-budget.sh"
"$project_dir/scripts/test-explicit-sync.sh"
"$project_dir/scripts/test-power-aware-animation.sh"
"$project_dir/scripts/test-presentation-policies.sh"
"$project_dir/scripts/test-component-presentations.sh"
"$project_dir/scripts/test-component-host.sh" >/dev/null
"$project_dir/scripts/test-plugin-scaffold.sh"
"$project_dir/scripts/test-session-permission-live.sh"
"$project_dir/scripts/test-canvas-component-ui.sh"
"$project_dir/scripts/test-effect-reduced-motion.sh"
"$project_dir/scripts/test-asset-component-ui.sh"
"$project_dir/scripts/test-installed-asset-lifecycle.sh"
"$project_dir/target/release/touchbarctl" plugin catalog-check \
    --catalog "$project_dir/catalog/plugins.toml"

echo "packaging=ok units=2 udev=verified installer=verified"
