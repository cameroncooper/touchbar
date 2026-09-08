#!/usr/bin/env bash
# Render one complete Omarchy screensaver period through the production GLES
# replay path and encode it as a compact, accelerated README GIF.
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

for command in cargo ffmpeg; do
    command -v "$command" >/dev/null || {
        echo "$command is required" >&2
        exit 1
    }
done

cli="target/release/touchbarctl"
host="target/release/touchbar-plugin-host"
package="plugins/omarchy"
output="docs/images/packs/omarchy/screensaver-2008-dark.gif"
work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT INT TERM
scenario="$work_dir/replay.json"
frames="$work_dir/frames"
mkdir -p "$frames"

cargo build --locked --release --quiet -p touchbar-cli -p touchbar-plugin-host
"$cli" plugin build --package "$package" >/dev/null

{
    printf '%s\n' '{'
    printf '%s\n' '  "version": 1,'
    printf '%s\n' '  "item": "screensaver",'
    printf '%s\n' '  "width": 2008,'
    printf '%s\n' '  "appearance": { "preset": "dark", "motion": "full", "colors": {} },'
    printf '%s\n' '  "steps": ['
    for frame in $(seq 0 59); do
        if ((frame > 0)); then
            printf ',\n    { "kind": "advance", "time_ms": %d },\n' "$((frame * 1000))"
        fi
        printf '    { "kind": "snapshot", "name": "frame-%03d" }' "$frame"
    done
    printf '\n%s\n' '  ]'
    printf '%s\n' '}'
} >"$scenario"

"$cli" plugin replay \
    --package "$package" \
    --scenario "$scenario" \
    --screenshots "$frames" \
    --host "$host" >/dev/null

ffmpeg -hide_banner -loglevel error -y \
    -framerate 10 -i "$frames/frame-%03d.png" \
    -filter_complex \
    '[0:v]split[frames][palette_source];[palette_source]palettegen=max_colors=128:stats_mode=diff[palette];[frames][palette]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle' \
    -loop 0 "$work_dir/screensaver.gif"

mkdir -p "$(dirname -- "$output")"
install -m 0644 "$work_dir/screensaver.gif" "$output"
echo "omarchy-gif=written frames=60 playback_fps=10 output=$output"
