#!/usr/bin/env bash
# Render every documented Touch Bar item to PNG through the production
# renderer, at each responsive width, in both themes.
#
# Replay is deterministic in everything it controls: the clock only advances
# when a scenario says so, and no host service is contacted. Rasterization is
# not, because it runs on whatever GPU is present, so output is byte-identical
# on one machine but differs between renderers.
#
# `--check` is therefore a local tool for confirming a tree is current, not a
# cross-machine CI gate. Regenerate on the same machine that last did.
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

check_only=false
if [[ ${1:-} == "--check" ]]; then
    check_only=true
elif [[ $# -gt 0 ]]; then
    echo "usage: generate-doc-images.sh [--check]" >&2
    exit 2
fi

output_root="docs/images/packs"
cli="target/release/touchbarctl"
host="target/release/touchbar-plugin-host"

# Responsive widths. These match the matrix `touchbarctl plugin test` renders,
# so a documented image and a tested representation cannot drift apart.
widths=(80 160 320 1004)

cargo build --locked --release --quiet -p touchbar-cli -p touchbar-plugin-host

if $check_only; then
    destination=$(mktemp -d)
    trap 'rm -rf -- "$destination"' EXIT INT TERM
else
    destination="$output_root"
    rm -rf -- "$destination"
fi

scenario_dir=$(mktemp -d)
trap 'rm -rf -- "$scenario_dir"' EXIT INT TERM

rendered=0
for pack in controls media hyprland capture command-deck; do
    manifest="plugins/$pack/touchbar-plugin.toml"
    [[ -f "$manifest" ]] || { echo "missing $manifest" >&2; exit 1; }

    # Refresh the component so an image can never be rendered from a stale
    # guest build.
    "$cli" plugin build --package "plugins/$pack" >/dev/null

    mapfile -t items < <(grep -A1 '^\[\[items\]\]' "$manifest" |
        grep '^id' | sed 's/id = //; s/"//g')

    for item in "${items[@]}"; do
        for width in "${widths[@]}"; do
            scenario="$scenario_dir/$pack-$item-$width.json"
            # One run captures both themes: the appearance step re-resolves
            # every semantic role without restarting the component.
            cat > "$scenario" <<JSON
{
  "version": 1,
  "item": "$item",
  "width": $width,
  "appearance": { "preset": "dark", "motion": "full", "colors": {} },
  "steps": [
    { "kind": "snapshot", "name": "$item-${width}-dark" },
    { "kind": "appearance", "preset": "light", "revision": 2, "colors": {} },
    { "kind": "snapshot", "name": "$item-${width}-light" }
  ]
}
JSON
            mkdir -p "$destination/$pack"
            "$cli" plugin replay \
                --package "plugins/$pack" \
                --scenario "$scenario" \
                --screenshots "$destination/$pack" \
                --host "$host" >/dev/null
            rendered=$((rendered + 2))
        done
    done
done

if $check_only; then
    if ! diff -r --brief "$output_root" "$destination" >/dev/null 2>&1; then
        echo "documentation images are stale; run scripts/generate-doc-images.sh" >&2
        diff -r --brief "$output_root" "$destination" >&2 || true
        exit 1
    fi
    echo "doc-images=current rendered=$rendered"
else
    echo "doc-images=written rendered=$rendered root=$output_root"
fi
