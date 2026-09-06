#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"

sessiond="$project_dir/target/release/touchbar-sessiond"
ui_demo="$project_dir/target/release/touchbar-ui-demo"
gl_demo="$project_dir/target/release/touchbar-gl-demo"
for binary in "$sessiond" "$ui_demo" "$gl_demo"; do
    if [[ ! -x "$binary" ]]; then
        echo "release binary is missing: $binary" >&2
        exit 1
    fi
done

run_case() {
    local policy=$1
    local label=$2
    local expanded_width=$3
    local lifecycle=${4:-persistent}
    local policy_name=${policy%%:*}
    local demo_flag
    local expected_contact
    if [[ "$lifecycle" == transient ]]; then
        demo_flag=--demo-touch
        expected_contact=1
    else
        demo_flag=--demo-tap
        expected_contact=0
    fi
    local runtime_dir
    local socket_name
    local session_pid=
    local ui_pid=
    local companion_pid=
    runtime_dir=$(mktemp -d "$runtime_root/presentation-${label}.XXXXXX")
    socket_name=bar
    chmod 700 "$runtime_dir"

    cleanup_case() {
        local status=$?
        for pid in "$ui_pid" "$companion_pid" "$session_pid"; do
            if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
                wait "$pid" 2>/dev/null || true
            fi
        done
        if ((status != 0)) && [[ -f "$runtime_dir/sessiond.log" ]]; then
            sed -n '1,320p' "$runtime_dir/sessiond.log" >&2
        fi
        if [[ "$runtime_dir" == "$runtime_root"/presentation-"$label".* \
            && -d "$runtime_dir" ]]; then
            rm -r -- "$runtime_dir"
        fi
        return "$status"
    }
    trap cleanup_case RETURN

    env XDG_RUNTIME_DIR="$runtime_dir" \
        "$sessiond" --socket "$socket_name" --no-plugins "$demo_flag" \
        --profiles "$project_dir/tests/fixtures/presentations-v1.toml" \
        --exit-after-clients 2 >"$runtime_dir/sessiond.log" 2>&1 &
    session_pid=$!

    for _ in $(seq 1 500); do
        [[ -S "$runtime_dir/$socket_name" ]] && break
        kill -0 "$session_pid" 2>/dev/null || break
        sleep 0.01
    done
    [[ -S "$runtime_dir/$socket_name" ]]

    env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
        "$gl_demo" --plugin-id demo.companion --item-id status \
        --fixed-width 160 --frames 36000 --require-hardware \
        >"$runtime_dir/companion.log" 2>&1 &
    companion_pid=$!
    env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
        "$ui_demo" --frames 150 --require-hardware --presentation "$policy" \
        >"$runtime_dir/ui.log" 2>&1 &
    ui_pid=$!

    wait "$ui_pid"
    ui_pid=
    kill "$companion_pid" 2>/dev/null || true
    wait "$companion_pid" 2>/dev/null || true
    companion_pid=
    wait "$session_pid"
    session_pid=

    rg -q "presentation=${policy_name} mode=${lifecycle} contact=${expected_contact} session=1" \
        "$runtime_dir/sessiond.log"
    rg -q "configured plugin=touchbar.ui-demo region=${expanded_width}x60" \
        "$runtime_dir/ui.log"
    rg -q 'ui-presentation=ended session=1 reason=Selection' "$runtime_dir/ui.log"
    rg -q 'summary frames=.* invalid=0 .* presentation_changes=2' \
        "$runtime_dir/sessiond.log"
    if [[ "$policy_name" == slot || "$policy_name" == full-bar ]]; then
        rg -q 'visibility plugin=demo.companion item=status visible=0' \
            "$runtime_dir/sessiond.log"
        rg -q 'visibility plugin=demo.companion item=status visible=1' \
            "$runtime_dir/sessiond.log"
    fi

    trap - RETURN
    rm -r -- "$runtime_dir"
}

run_rejected_case() {
    local runtime_dir
    local socket_name
    local session_pid=
    local ui_pid=
    local companion_pid=
    runtime_dir=$(mktemp -d "$runtime_root/presentation-rejected.XXXXXX")
    socket_name=bar
    chmod 700 "$runtime_dir"

    cleanup_rejected_case() {
        local status=$?
        for pid in "$ui_pid" "$companion_pid" "$session_pid"; do
            if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
                wait "$pid" 2>/dev/null || true
            fi
        done
        if ((status != 0)) && [[ -f "$runtime_dir/sessiond.log" ]]; then
            sed -n '1,320p' "$runtime_dir/sessiond.log" >&2
        fi
        if [[ "$runtime_dir" == "$runtime_root"/presentation-rejected.* \
            && -d "$runtime_dir" ]]; then
            rm -r -- "$runtime_dir"
        fi
        return "$status"
    }
    trap cleanup_rejected_case RETURN

    env XDG_RUNTIME_DIR="$runtime_dir" \
        "$sessiond" --socket "$socket_name" --no-plugins --demo-tap \
        --profiles "$project_dir/tests/fixtures/presentations-v1.toml" \
        --exit-after-clients 2 >"$runtime_dir/sessiond.log" 2>&1 &
    session_pid=$!

    for _ in $(seq 1 500); do
        [[ -S "$runtime_dir/$socket_name" ]] && break
        kill -0 "$session_pid" 2>/dev/null || break
        sleep 0.01
    done
    [[ -S "$runtime_dir/$socket_name" ]]

    env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
        "$gl_demo" --plugin-id demo.companion --item-id status \
        --fixed-width 160 --frames 36000 --require-hardware \
        >"$runtime_dir/companion.log" 2>&1 &
    companion_pid=$!
    env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
        "$ui_demo" --frames 150 --require-hardware --presentation slot:missing \
        >"$runtime_dir/ui.log" 2>&1 &
    ui_pid=$!

    wait "$ui_pid"
    ui_pid=
    kill "$companion_pid" 2>/dev/null || true
    wait "$companion_pid" 2>/dev/null || true
    companion_pid=
    wait "$session_pid"
    session_pid=

    rg -q 'ignored slot presentation: active profile has no slot `missing`' \
        "$runtime_dir/sessiond.log"
    rg -q 'ui-presentation=ended session=1 reason=Rejected' "$runtime_dir/ui.log"
    if rg -q 'configured plugin=touchbar.ui-demo region=360x60' "$runtime_dir/ui.log"; then
        echo "rejected presentation was assigned expanded geometry" >&2
        return 1
    fi
    rg -q 'summary frames=.* invalid=0 .* presentation_changes=0' \
        "$runtime_dir/sessiond.log"

    trap - RETURN
    rm -r -- "$runtime_dir"
}

run_region_invalidation_case() {
    local runtime_dir
    local socket_name
    local profile_file
    local session_pid=
    local ui_pid=
    local companion_pid=
    runtime_dir=$(mktemp -d "$runtime_root/presentation-invalidation.XXXXXX")
    socket_name=bar
    profile_file="$runtime_dir/profiles.toml"
    chmod 700 "$runtime_dir"
    install -m 0600 "$project_dir/tests/fixtures/presentations-v1.toml" "$profile_file"

    cleanup_invalidation_case() {
        local status=$?
        for pid in "$ui_pid" "$companion_pid" "$session_pid"; do
            if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
                wait "$pid" 2>/dev/null || true
            fi
        done
        if ((status != 0)) && [[ -f "$runtime_dir/sessiond.log" ]]; then
            sed -n '1,320p' "$runtime_dir/sessiond.log" >&2
        fi
        if [[ "$runtime_dir" == "$runtime_root"/presentation-invalidation.* \
            && -d "$runtime_dir" ]]; then
            rm -r -- "$runtime_dir"
        fi
        return "$status"
    }
    trap cleanup_invalidation_case RETURN

    env XDG_RUNTIME_DIR="$runtime_dir" \
        "$sessiond" --socket "$socket_name" --no-plugins --demo-tap \
        --profiles "$profile_file" --exit-after-clients 2 \
        >"$runtime_dir/sessiond.log" 2>&1 &
    session_pid=$!

    for _ in $(seq 1 500); do
        [[ -S "$runtime_dir/$socket_name" ]] && break
        kill -0 "$session_pid" 2>/dev/null || break
        sleep 0.01
    done
    [[ -S "$runtime_dir/$socket_name" ]]

    env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
        "$gl_demo" --plugin-id demo.companion --item-id status \
        --fixed-width 160 --frames 36000 --require-hardware \
        >"$runtime_dir/companion.log" 2>&1 &
    companion_pid=$!
    env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
        "$ui_demo" --frames 180 --require-hardware --presentation region:palette \
        >"$runtime_dir/ui.log" 2>&1 &
    ui_pid=$!

    for _ in $(seq 1 500); do
        rg -q 'ui-presentation=started session=1' "$runtime_dir/ui.log" && break
        kill -0 "$ui_pid" 2>/dev/null || break
        sleep 0.01
    done
    rg -q 'ui-presentation=started session=1' "$runtime_dir/ui.log"

    sed '/^\[\[region\]\]$/,/^width = 1000$/d' "$profile_file" \
        >"$runtime_dir/profiles.next"
    chmod 600 "$runtime_dir/profiles.next"
    mv "$runtime_dir/profiles.next" "$profile_file"

    for _ in $(seq 1 500); do
        rg -q 'ui-presentation=ended session=1 reason=SourceHidden' \
            "$runtime_dir/ui.log" && break
        kill -0 "$ui_pid" 2>/dev/null || break
        sleep 0.01
    done
    rg -q 'ui-presentation=ended session=1 reason=SourceHidden' "$runtime_dir/ui.log"

    wait "$ui_pid"
    ui_pid=
    kill "$companion_pid" 2>/dev/null || true
    wait "$companion_pid" 2>/dev/null || true
    companion_pid=
    wait "$session_pid"
    session_pid=

    rg -q 'profile-watch=reloaded' "$runtime_dir/sessiond.log"
    rg -q 'presentation=region refresh-failed error=unknown presentation region `palette`' \
        "$runtime_dir/sessiond.log"
    rg -q 'presentation=compact previous=region session=1' "$runtime_dir/sessiond.log"
    rg -q 'configured plugin=touchbar.ui-demo region=80x60' "$runtime_dir/ui.log"
    rg -q 'summary frames=.* invalid=0 .* presentation_changes=2' \
        "$runtime_dir/sessiond.log"

    trap - RETURN
    rm -r -- "$runtime_dir"
}

run_case in-place in-place 360
run_case slot:content slot 360
run_case region:palette region 1000
run_case full-bar full-bar 2008
run_case slot:content slot-transient 360 transient
run_rejected_case
run_region_invalidation_case

echo "presentation-policies=ok in-place=360 slot=content region=palette:1000 full-bar=2008 slot-transient=captured rejected=ok invalidated=source-hidden invalid=0"
