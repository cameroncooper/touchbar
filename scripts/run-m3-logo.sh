#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
binary="$project_dir/target/release/touchbard"
root_helper="$project_dir/scripts/run-m3-logo-root.sh"
logo="$project_dir/assets/touchbar.png"
action=${1:---probe}
duration=${2:-10}

case "$action" in
    --probe | --display | --animate) ;;
    *)
        echo "usage: $0 [--probe | --display | --animate] [duration-seconds]" >&2
        exit 2
        ;;
esac

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"

if [[ "$action" == "--probe" ]]; then
    "$binary" --probe
    echo "KMS details require root access; requesting a read-only probe."
    exec pkexec "$binary" --kms-probe
fi

if [[ ! "$duration" =~ ^[0-9]+$ ]] || ((duration < 1 || duration > 30)); then
    echo "duration must be between 1 and 30 seconds" >&2
    exit 2
fi

if [[ "$action" == "--animate" ]]; then
    echo "The TouchBar logo will animate at the Touch Bar's vblank rate for $duration seconds."
else
    echo "The Touch Bar will show the TouchBar logo for $duration seconds."
fi
echo "The previously active Touch Bar service will be restored automatically on exit or interruption."
exec pkexec "$root_helper" "$binary" "$logo" "$duration" "$action"
