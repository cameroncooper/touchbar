#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
duration=${1:-15}
runtime_dir="$project_dir/run/hardware-fallback-physical"
socket_path="/run/touchbar-demo-${UID}-$$/hardware.sock"

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 3 || duration > 30)); then
    echo "duration must be between 3 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
exec 9>"$runtime_dir/run.lock"
flock -n 9 || {
    echo "A hardware-fallback physical demo is already running." >&2
    exit 1
}

cargo build --release --manifest-path "$project_dir/Cargo.toml" -p touchbard

echo "This test starts touchbard alone: there is no user compositor and no restart."
echo "Expected: the media row is visible immediately; hold Fn for F1-F12; release Fn to restore media."
echo "The previously active Touch Bar service will be restored automatically."
echo "Authorize the guarded physical demo when prompted; this preflight does not touch hardware."
pkexec "$project_dir/scripts/run-m3-logo-root.sh" \
    "$project_dir/target/release/touchbard" "$project_dir/assets/touchbar.png" \
    "$duration" --authorize

pkexec "$project_dir/scripts/run-m3-logo-root.sh" \
    "$project_dir/target/release/touchbard" "$project_dir/assets/touchbar.png" \
    "$duration" --serve-demo "$socket_path" | tee "$runtime_dir/touchbard.log"

rg -q 'hardware-fallback=active layer=media' "$runtime_dir/touchbard.log"
rg -q 'hardware-fallback=presented initial=true' "$runtime_dir/touchbard.log"
if rg -q 'hardware-session=accepted' "$runtime_dir/touchbard.log"; then
    echo "hardware-only fallback unexpectedly accepted a user session" >&2
    exit 1
fi
rg -q 'hardware-service=stopping keys=released backlight=idle' \
    "$runtime_dir/touchbard.log"
rg -q 'hardware-service=stopped' "$runtime_dir/touchbard.log"
rg -q 'physical-owner=restored service=' "$runtime_dir/touchbard.log"

if rg -q 'hardware-fn=pressed fallback-layer=function' "$runtime_dir/touchbard.log" \
    && rg -q 'hardware-fn=released fallback-layer=media' "$runtime_dir/touchbard.log"; then
    fn_result=observed
else
    fn_result=not-exercised
fi

echo "hardware-fallback-physical=ok session=absent restart=absent fn=$fn_result"
