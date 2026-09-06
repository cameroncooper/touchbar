#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
binary="$project_dir/target/release/touchbard"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
duration=${1:-5}
runtime_dir="$project_dir/run/adp-cadence-physical"
log="$runtime_dir/probe.log"

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 3 || duration > 30)); then
    echo "duration must be between 3 and 30 seconds" >&2
    exit 2
fi

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
exec 9>"$runtime_dir/run.lock"
flock -n 9 || {
    echo "An ADP cadence probe is already running." >&2
    exit 1
}

cargo build --release --manifest-path "$project_dir/Cargo.toml" -p touchbard

echo "This guarded probe animates the TouchBar logo for $duration seconds."
echo "It correlates DRM vblank replies with the kernel adp-fe IRQ count."
echo "The previously active Touch Bar service will be restored automatically."
echo "Authorize the physical probe when prompted; this preflight does not touch hardware."
pkexec "$root_helper" "$binary" "$logo" "$duration" --authorize

pkexec "$root_helper" "$binary" "$logo" "$duration" --animate | tee "$log"

scanout_summary=$(rg 'scanout-summary ' "$log" | tail -n 1)
irq_summary=$(rg 'adp-fe-irq-summary ' "$log" | tail -n 1)
rg -q 'physical-owner=restored service=' "$log"

updates=$(sed -n 's/.* updates=\([0-9]\+\).*/\1/p' <<<"$scanout_summary")
sequence_delta=$(sed -n 's/.* sequence_delta=\([0-9]\+\).*/\1/p' <<<"$scanout_summary")
vblank_hz=$(sed -n 's/.* fps=\([0-9.]\+\).*/\1/p' <<<"$scanout_summary")
irq_hz=$(sed -n 's/.* irq_hz=\([0-9.]\+\).*/\1/p' <<<"$irq_summary")

if [[ -z "$updates" || -z "$sequence_delta" || -z "$vblank_hz" || -z "$irq_hz" ]]; then
    echo "cadence probe produced an incomplete summary" >&2
    exit 1
fi
if ((updates < 2 || sequence_delta != updates - 1)); then
    echo "DRM vblank sequence was not contiguous: updates=$updates sequence_delta=$sequence_delta" >&2
    exit 1
fi

if awk -v vblank="$vblank_hz" -v irq="$irq_hz" \
    'BEGIN { exit !(vblank >= 25 && vblank <= 35 && irq >= 25 && irq <= 35) }'; then
    classification=half-rate
elif awk -v vblank="$vblank_hz" -v irq="$irq_hz" \
    'BEGIN { exit !(vblank >= 55 && vblank <= 65 && irq >= 55 && irq <= 65) }'; then
    classification=full-rate
else
    classification=unexpected
fi

echo "adp-cadence-physical=ok classification=$classification advertised_hz=60 vblank_hz=$vblank_hz irq_hz=$irq_hz contiguous_sequences=true"
