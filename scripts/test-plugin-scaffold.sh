#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
test_dir=$(mktemp -d "$runtime_root/plugin-scaffold.XXXXXX")

cleanup() {
    if [[ "$test_dir" == "$runtime_root"/plugin-scaffold.* && -d "$test_dir" ]]; then
        rm -r -- "$test_dir"
    fi
}
trap cleanup EXIT INT TERM

cli="$project_dir/target/release/touchbarctl"
host="$project_dir/target/release/touchbar-plugin-host"
package_dir="$test_dir/demo"

"$cli" plugin new demo \
    --source github:alice/demo \
    --directory "$package_dir" \
    --sdk "$project_dir/crates/touchbar-component-sdk" >/dev/null

"$cli" plugin build --package "$package_dir" >/dev/null
check_json=$("$cli" plugin check --package "$package_dir" --format json)
test_json=$("$cli" plugin test --package "$package_dir" --host "$host" --format json)
replay_json=$("$cli" plugin replay --package "$package_dir" --host "$host" \
    --scenario "$package_dir/tests/interaction.json" \
    --screenshots "$test_dir/screenshots")
"$cli" plugin pack --package "$package_dir" \
    --output "$test_dir/demo.touchbar" >/dev/null

printf '%s\n' "$check_json" | rg -q '"ok": true'
printf '%s\n' "$test_json" | rg -q '"report_version": 1'
printf '%s\n' "$test_json" | rg -q '"presentation_bars"'
printf '%s\n' "$test_json" | rg -q '"render_cases": 16'
printf '%s\n' "$test_json" | rg -q '2008'
printf '%s\n' "$replay_json" | rg -q '"report_version": 1'
printf '%s\n' "$replay_json" | rg -q '"kind": "long-pressed"'
printf '%s\n' "$replay_json" | rg -q '"lifecycle": "persistent"'
printf '%s\n' "$replay_json" | rg -q '"lifecycle": "transient"'
printf '%s\n' "$replay_json" | rg -q '"accent": "#246bfeff"'
printf '%s\n' "$replay_json" | rg -q '"name": "final"'
printf '%s\n' "$replay_json" | rg -q '"screenshot": "final.png"'
printf '%s\n' "$replay_json" | rg -q '"screenshot_renderer": "'
file "$test_dir/screenshots/final.png" | rg -q 'PNG image data, 160 x 60, 8-bit/color RGBA'
test -s "$test_dir/demo.touchbar"

echo "plugin-scaffold=ok items=2 bars=1 render-cases=16 replay=tap+hold+theme+semantics screenshot=GPU-RGBA"
