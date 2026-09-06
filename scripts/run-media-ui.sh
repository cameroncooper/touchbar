#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_dir="$project_dir/run/media-ui"
frames=${1:-180}
width=${2:-160}
interaction=${3:-tap}
socket_name="touchbar-media-$$"
server_pid=
media_pid=

if [[ ! "$frames" =~ ^[0-9]+$ ]] || ((frames < 60)); then
    echo "frames must be an integer of at least 60" >&2
    exit 2
fi
if [[ ! "$width" =~ ^(80|160|420)$ ]]; then
    echo "width must be one of 80, 160, or 420" >&2
    exit 2
fi
case "$interaction" in
    tap) demo_flag=--demo-tap ;;
    hold) demo_flag=--demo-touch ;;
    *) echo "interaction must be tap or hold" >&2; exit 2 ;;
esac

mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

cleanup() {
    for pid in "$media_pid" "$server_pid"; do
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
    --socket "$socket_name" "$demo_flag" --no-plugins --exit-after-client \
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
    "$project_dir/target/release/touchbar-media-demo" \
    --frames "$frames" --width "$width" --require-hardware \
    >"$runtime_dir/media.log" 2>&1 &
media_pid=$!

wait "$media_pid"
media_pid=
wait "$server_pid"
server_pid=

sed -n '1,220p' "$runtime_dir/media.log"
sed -n '1,260p' "$runtime_dir/touchbar-sessiond.log"

rg -q "configured plugin=touchbar.media-demo region=${width}x60 renderer=Apple M1 .* transport=dmabuf expanded=false" \
    "$runtime_dir/media.log"
rg -q "configured plugin=touchbar.media-demo region=420x60 renderer=Apple M1 .* transport=dmabuf expanded=true" \
    "$runtime_dir/media.log"
rg -q "media-summary plugin=touchbar.media-demo frames=$frames renderer=Apple M1" \
    "$runtime_dir/media.log"
rg -q "media-action=seek" "$runtime_dir/media.log"
rg -q "summary frames=$frames .* invalid=0 dmabuf=$frames shm=0" \
    "$runtime_dir/touchbar-sessiond.log"
rg -q "assigned plugin=touchbar.media-demo item=media.now-playing x=0 region=${width}x60" \
    "$runtime_dir/touchbar-sessiond.log"
if [[ "$interaction" == tap ]]; then
    rg -q "media-presentation=request session=1 lifecycle=Persistent" "$runtime_dir/media.log"
    rg -q "presentation=anchored mode=persistent" "$runtime_dir/touchbar-sessiond.log"
else
    rg -q "media-presentation=request session=1 lifecycle=Transient" "$runtime_dir/media.log"
    rg -q "media-capture=timeline contact=1 transferred=true" "$runtime_dir/media.log"
    rg -q "presentation=anchored mode=transient" "$runtime_dir/touchbar-sessiond.log"
    rg -q "media-presentation=ended session=1 reason=Requested" "$runtime_dir/media.log"
fi
