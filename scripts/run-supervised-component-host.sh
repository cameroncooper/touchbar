#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
item=${1:-hello}
width=${2:-160}
activate_widget=${3:-}
sandbox_state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/touchbar/sandbox"
install -d -m 700 "$sandbox_state_dir"

package_dir=$($root_dir/scripts/build-component-demo.sh)
cargo build --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -p touchbar-plugin-supervisor
digest=$(sha256sum "$package_dir/component/plugin.wasm" | awk '{print $1}')
arguments=("$package_dir" --host "$root_dir/target/debug/touchbar-plugin-host" \
    --source github:cameroncooper/touchbar-component-demo --version 0.1.0 \
    --digest "sha256:$digest" --provenance local-development --state "$sandbox_state_dir" -- "$item" "$width")
if [[ -n "$activate_widget" ]]; then
    arguments+=("$activate_widget")
fi
exec "$root_dir/target/debug/touchbar-plugin-supervisor" "${arguments[@]}"
