#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/context-ui"
socket_name="touchbar-context-ui-$$"
server_pid=
ui_pid=
terminal_pid=
browser_pid=
status_pid=

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$ui_pid" "$terminal_pid" "$browser_pid" "$status_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
}
trap cleanup EXIT

cargo build --release --workspace --manifest-path "$project_dir/Cargo.toml"

env XDG_RUNTIME_DIR="$runtime_dir" \
    "$project_dir/target/release/touchbar-sessiond" \
    --socket "$socket_name" --profile-demo --demo-focus --demo-touch \
    --no-plugins --exit-after-clients 4 \
    >"$runtime_dir/touchbar-sessiond.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" ]] && break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" ]]; then
    echo "touchbar-sessiond did not create its Wayland socket" >&2
    sed -n '1,220p' "$runtime_dir/touchbar-sessiond.log" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-ui-demo" \
    --frames 36000 --require-hardware >"$runtime_dir/ui.log" 2>&1 &
ui_pid=$!

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --plugin-id demo.terminal --fixed-width 320 --variant 0 \
    --frames 36000 --require-hardware >"$runtime_dir/terminal.log" 2>&1 &
terminal_pid=$!

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --plugin-id demo.browser --fixed-width 1780 --variant 1 \
    --frames 36000 --require-hardware >"$runtime_dir/browser.log" 2>&1 &
browser_pid=$!

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$project_dir/target/release/touchbar-gl-demo" \
    --plugin-id demo.status --fixed-width 140 --variant 0.5 \
    --frames 36000 --require-hardware >"$runtime_dir/status.log" 2>&1 &
status_pid=$!

for _ in $(seq 1 800); do
    rg -q "profile-layout generation=4 .*items=touchbar.ui-demo#audio.volume,demo.browser#demo.browser,demo.status#demo.status" \
        "$runtime_dir/touchbar-sessiond.log" && break
    sleep 0.01
done

rg -q "profile-runtime=ready" "$runtime_dir/touchbar-sessiond.log"
rg -q "profile-layout=deferred captured-contact-active" "$runtime_dir/touchbar-sessiond.log"
rg -q "profile-layout generation=1 .*items=touchbar.ui-demo#audio.volume,demo.terminal#demo.terminal,demo.status#demo.status" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "profile-layout generation=2 .*items=touchbar.ui-demo#audio.volume,demo.browser#demo.browser,demo.status#demo.status" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "profile-layout generation=3 .*items=touchbar.ui-demo#audio.volume,demo.terminal#demo.terminal,demo.status#demo.status" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "profile-layout generation=4 .*items=touchbar.ui-demo#audio.volume,demo.browser#demo.browser,demo.status#demo.status" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "visibility plugin=demo.browser item=demo.browser visible=1" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "visibility plugin=demo.terminal item=demo.terminal visible=0" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "configured plugin=touchbar.ui-demo region=72x60" "$runtime_dir/ui.log"
rg -q "configured plugin=touchbar.ui-demo region=80x60" "$runtime_dir/ui.log"
rg -q "ui-event=popover-request session=1 contact=1" "$runtime_dir/ui.log"
rg -q "ui-presentation=started session=1 lifecycle=Transient" "$runtime_dir/ui.log"
rg -q "ui-event=palette-selection index=4" "$runtime_dir/ui.log"
rg -q "ui-presentation=ended session=1" "$runtime_dir/ui.log"

kill "$ui_pid" "$terminal_pid" "$browser_pid" "$status_pid" 2>/dev/null || true
wait "$ui_pid" 2>/dev/null || true
wait "$terminal_pid" 2>/dev/null || true
wait "$browser_pid" 2>/dev/null || true
wait "$status_pid" 2>/dev/null || true
ui_pid=
terminal_pid=
browser_pid=
status_pid=
wait "$server_pid"
server_pid=

rg -q "summary frames=.* invalid=0 .* input_events=6 presentation_changes=2 profile_layouts=4 context_changes=3" \
    "$runtime_dir/touchbar-sessiond.log"

sed -n '1,180p' "$runtime_dir/ui.log"
sed -n '1,100p' "$runtime_dir/terminal.log"
sed -n '1,100p' "$runtime_dir/browser.log"
sed -n '1,100p' "$runtime_dir/status.log"
sed -n '1,320p' "$runtime_dir/touchbar-sessiond.log"
