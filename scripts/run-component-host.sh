#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
item=${1:-hello}
width=${2:-160}
activate_widget=${3:-}

package_dir=$($root_dir/scripts/build-component-demo.sh)
arguments=("$package_dir" "$item" "$width")
if [[ -n "$activate_widget" ]]; then
    arguments+=("$activate_widget")
fi
cargo run --quiet --manifest-path "$root_dir/Cargo.toml" -p touchbar-plugin-host -- "${arguments[@]}"
