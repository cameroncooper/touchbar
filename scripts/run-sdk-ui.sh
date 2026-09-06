#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/sdk-ui"
frames=${1:-120}
interaction=${2:-hold}
case "$interaction" in
    hold) demo_flag=--demo-touch ;;
    tap) demo_flag=--demo-tap ;;
    nested) demo_flag=--demo-nested ;;
    *) echo "usage: $0 [frames] [hold|tap|nested]" >&2; exit 2 ;;
esac
server_pid=
ui_pid=
companion_pid=
socket_name="touchbar-sdk-ui-$$"

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$ui_pid" "$companion_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid"
            wait "$pid" 2>/dev/null || true
        fi
    done
}
trap cleanup EXIT

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" "$demo_flag" --no-plugins --exit-after-clients 2 \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    sleep 0.01
done

if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "touchbar-sessiond did not create its Wayland socket" >&2
    sed -n '1,160p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" \
    WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-ui-demo" \
    --frames "$frames" --require-hardware \
    >"$runtime_dir/ui.log" 2>&1 &
ui_pid=$!

for _ in $(seq 1 500); do
    rg -q "assigned plugin=touchbar.ui-demo" "$runtime_dir/touchbar-sessiond.log" && break
    sleep 0.01
done

env XDG_RUNTIME_DIR="$runtime_dir" \
    WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --plugin-id touchbar.backdrop --backdrop --variant 1 \
    --frames "$frames" --require-hardware \
    >"$runtime_dir/companion.log" 2>&1 &
companion_pid=$!

wait "$ui_pid"
ui_pid=
wait "$companion_pid"
companion_pid=
wait "$server_pid"
server_pid=

sed -n '1,160p' "$runtime_dir/ui.log"
sed -n '1,80p' "$runtime_dir/companion.log"
sed -n '1,240p' "$runtime_dir/touchbar-sessiond.log"

total_frames=$((frames * 2))

rg -q "renderer=Apple M1 .* transport=dmabuf ui=touchbar-ui" "$runtime_dir/ui.log"
rg -q "client-summary plugin=touchbar.ui-demo frames=$frames .* ui=touchbar-ui" \
    "$runtime_dir/ui.log"
rg -q "ui-appearance generation=1 scheme=Dark" "$runtime_dir/ui.log"
rg -q "client-summary plugin=touchbar.backdrop frames=$frames transport=dmabuf" \
    "$runtime_dir/companion.log"
rg -q "shader-appearance generation=1 scheme=Dark" "$runtime_dir/companion.log"
rg -q "summary frames=$total_frames .* invalid=0 dmabuf=$total_frames shm=0" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "releases=$total_frames" "$runtime_dir/touchbar-sessiond.log"
rg -q "ui-popover-anchor session=1 x=0.0 width=80.0" "$runtime_dir/ui.log"
rg -q "region=360x60" "$runtime_dir/ui.log"
rg -q "ui-presentation=ended session=1" "$runtime_dir/ui.log"
rg -q "assigned plugin=touchbar.ui-demo item=audio.volume x=0 region=80x60" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "assigned plugin=touchbar.backdrop role=backdrop x=0 region=2008x60" \
    "$runtime_dir/touchbar-sessiond.log"
if [[ "$interaction" == hold ]]; then
    rg -q "ui-event=popover-request session=1 contact=1" "$runtime_dir/ui.log"
    rg -q "ui-presentation=started session=1 lifecycle=Transient" "$runtime_dir/ui.log"
    rg -q "ui-event=palette-selection index=4" "$runtime_dir/ui.log"
    rg -q "presentation=anchored mode=transient contact=1 session=1" \
        "$runtime_dir/touchbar-sessiond.log"
    rg -q "input_events=4 presentation_changes=2" "$runtime_dir/touchbar-sessiond.log"
elif [[ "$interaction" == tap ]]; then
    rg -q "ui-event=persistent-popover-request session=1" "$runtime_dir/ui.log"
    rg -q "ui-presentation=started session=1 lifecycle=Persistent" "$runtime_dir/ui.log"
    rg -q "ui-event=palette-selection index=3" "$runtime_dir/ui.log"
    rg -q "presentation=anchored mode=persistent contact=0 session=1" \
        "$runtime_dir/touchbar-sessiond.log"
    rg -q "input_events=4 presentation_changes=2" "$runtime_dir/touchbar-sessiond.log"
else
    rg -q "ui-event=persistent-popover-request session=1" "$runtime_dir/ui.log"
    rg -q "ui-navigation=push page=more" "$runtime_dir/ui.log"
    rg -q "ui-navigation=back page=volume" "$runtime_dir/ui.log"
    rg -q "ui-event=palette-selection index=3" "$runtime_dir/ui.log"
    rg -q "presentation=anchored mode=persistent contact=0 session=1" \
        "$runtime_dir/touchbar-sessiond.log"
    rg -q "input_events=8 presentation_changes=2" "$runtime_dir/touchbar-sessiond.log"
fi
