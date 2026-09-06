#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
cargo build --release --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-cli -p touchbar-plugin-host -p touchbar-plugin-supervisor \
    -p touchbar-sessiond -p touchbard

for pack in controls media hyprland capture command-deck; do
    "$project_dir/target/release/touchbarctl" plugin build --package "$project_dir/plugins/$pack"
    "$project_dir/target/release/touchbarctl" plugin check --package "$project_dir/plugins/$pack"
done
