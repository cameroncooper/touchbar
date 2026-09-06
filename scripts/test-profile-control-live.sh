#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
runtime_dir=$(mktemp -d "$runtime_root/profile-control.XXXXXX")
socket_name="profile-control-$$"
store_dir="$runtime_dir/store"
control_socket="$store_dir/control.sock"
profile_file="$runtime_dir/profiles.toml"
session_pid=
alpha_pid=
beta_pid=
gamma_pid=
replacement_beta_pid=
chmod 700 "$runtime_dir"

cleanup() {
    status=$?
    for pid in "$alpha_pid" "$beta_pid" "$gamma_pid" "$replacement_beta_pid" "$session_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if ((status != 0)) && [[ -f "$runtime_dir/sessiond.log" ]]; then
        sed -n '1,320p' "$runtime_dir/sessiond.log" >&2
    fi
    if [[ "$runtime_dir" == "$runtime_root"/profile-control.* && -d "$runtime_dir" ]]; then
        rm -r -- "$runtime_dir"
    fi
}
trap cleanup EXIT INT TERM

sessiond="$project_dir/target/release/touchbar-sessiond"
demo="$project_dir/target/release/touchbar-gl-demo"
ctl="$project_dir/target/release/touchbarctl"
for binary in "$sessiond" "$demo" "$ctl"; do
    if [[ ! -x "$binary" ]]; then
        echo "release binary is missing: $binary" >&2
        exit 1
    fi
done
install -m 0600 "$project_dir/tests/fixtures/profiles-v1.toml" "$profile_file"

env XDG_RUNTIME_DIR="$runtime_dir" TOUCHBAR_HOME="$store_dir" \
    "$sessiond" --socket "$socket_name" \
    --profiles "$profile_file" \
    --control-socket "$control_socket" --system-bar --no-plugins \
    --exit-after-clients 4 >"$runtime_dir/sessiond.log" 2>&1 &
session_pid=$!

for _ in $(seq 1 500); do
    [[ -S "$runtime_dir/$socket_name" && -S "$control_socket" ]] && break
    kill -0 "$session_pid" 2>/dev/null || break
    sleep 0.01
done
if [[ ! -S "$runtime_dir/$socket_name" || ! -S "$control_socket" ]]; then
    echo "profile compositor did not become ready" >&2
    exit 1
fi

env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$demo" --plugin-id demo.alpha --item-id shared --fixed-width 120 \
    --frames 36000 --require-hardware >"$runtime_dir/alpha.log" 2>&1 &
alpha_pid=$!
env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$demo" --plugin-id demo.beta --item-id shared --fixed-width 320 \
    --frames 36000 --require-hardware >"$runtime_dir/beta.log" 2>&1 &
beta_pid=$!
env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$demo" --plugin-id demo.gamma --item-id extra --fixed-width 180 \
    --frames 36000 --require-hardware >"$runtime_dir/gamma.log" 2>&1 &
gamma_pid=$!

for _ in $(seq 1 800); do
    if rg -q 'profile-layout .* profile=default .*items=demo.alpha#shared,demo.beta#shared,demo.gamma#extra' "$runtime_dir/sessiond.log"; then
        break
    fi
    sleep 0.01
done
rg -q 'assigned plugin=demo.alpha item=shared' "$runtime_dir/sessiond.log"
rg -q 'assigned plugin=demo.beta item=shared' "$runtime_dir/sessiond.log"
rg -q 'profile-layout .* profile=default .*items=demo.alpha#shared,demo.beta#shared,demo.gamma#extra' "$runtime_dir/sessiond.log"

status=$(env TOUCHBAR_HOME="$store_dir" "$ctl" session status --format json)
[[ "$status" == *'"configured": true'* ]]
[[ "$status" == *'"ready": true'* ]]
[[ "$status" == *'"automatic": true'* ]]
[[ "$status" == *'"active": "default"'* ]]

selected=$(env TOUCHBAR_HOME="$store_dir" \
    "$ctl" session profile select minimal --format json)
[[ "$selected" == *'"automatic": false'* ]]
[[ "$selected" == *'"active": "minimal"'* ]]
rg -q 'profile-layout .* profile=minimal .*items=demo.alpha#shared' "$runtime_dir/sessiond.log"
rg -q 'visibility plugin=demo.beta item=shared visible=0' "$runtime_dir/sessiond.log"

automatic=$(env TOUCHBAR_HOME="$store_dir" \
    "$ctl" session profile automatic --format json)
[[ "$automatic" == *'"automatic": true'* ]]
[[ "$automatic" == *'"active": "default"'* ]]

reloaded=$(env TOUCHBAR_HOME="$store_dir" \
    "$ctl" session reload --format json)
[[ "$reloaded" == *'"ok": true'* ]]
[[ "$reloaded" == *'"message": "runtime configuration reloaded"'* ]]

profile_list=$(env TOUCHBAR_HOME="$store_dir" "$ctl" session profile list)
[[ "$profile_list" == *'mode=automatic state=ready'* ]]
[[ "$profile_list" == *'* default'* ]]
[[ "$profile_list" == *'  minimal'* ]]

sed 's/fallback = "default"/fallback = "minimal"/' "$profile_file" \
    >"$runtime_dir/profiles.next"
chmod 600 "$runtime_dir/profiles.next"
mv "$runtime_dir/profiles.next" "$profile_file"
for _ in $(seq 1 500); do
    rg -q 'profile-watch=reloaded' "$runtime_dir/sessiond.log" \
        && rg -q 'profile-layout .* profile=minimal' "$runtime_dir/sessiond.log" \
        && break
    sleep 0.01
done
hot_reloaded=$(env TOUCHBAR_HOME="$store_dir" "$ctl" session status --format json)
[[ "$hot_reloaded" == *'"automatic": true'* ]]
[[ "$hot_reloaded" == *'"active": "minimal"'* ]]

install -m 0600 "$project_dir/tests/fixtures/profiles-v1.toml" "$profile_file"
for _ in $(seq 1 500); do
    restored_default=$(env TOUCHBAR_HOME="$store_dir" \
        "$ctl" session status --format json)
    [[ "$restored_default" == *'"active": "default"'* ]] && break
    sleep 0.01
done
[[ "$restored_default" == *'"active": "default"'* ]]

sed 's/version = 1/version = 999/' "$profile_file" >"$runtime_dir/profiles.invalid"
chmod 600 "$runtime_dir/profiles.invalid"
mv "$runtime_dir/profiles.invalid" "$profile_file"
for _ in $(seq 1 500); do
    rg -q 'profile-watch=reload-rejected' "$runtime_dir/sessiond.log" && break
    sleep 0.01
done
rg -q 'profile-watch=reload-rejected' "$runtime_dir/sessiond.log"
preserved=$(env TOUCHBAR_HOME="$store_dir" "$ctl" session status --format json)
[[ "$preserved" == *'"ready": true'* ]]
[[ "$preserved" == *'"active": "default"'* ]]
install -m 0600 "$project_dir/tests/fixtures/profiles-v1.toml" "$profile_file"
for _ in $(seq 1 500); do
    valid_again=$(env TOUCHBAR_HOME="$store_dir" \
        "$ctl" session status --format json)
    [[ "$valid_again" == *'"ready": true'* ]] \
        && [[ "$valid_again" == *'"active": "default"'* ]] \
        && break
    sleep 0.01
done

placeholder_visible_before=$(rg -c '^plugin-placeholder=visible$' "$runtime_dir/sessiond.log" || true)
kill "$beta_pid"
wait "$beta_pid" 2>/dev/null || true
beta_pid=
for _ in $(seq 1 500); do
    placeholder_visible_now=$(rg -c '^plugin-placeholder=visible$' "$runtime_dir/sessiond.log" || true)
    rg -q 'profile-runtime=waiting missing=demo.beta#shared' "$runtime_dir/sessiond.log" \
        && ((placeholder_visible_now > placeholder_visible_before)) \
        && break
    sleep 0.01
done
placeholder_visible_now=$(rg -c '^plugin-placeholder=visible$' "$runtime_dir/sessiond.log" || true)
((placeholder_visible_now > placeholder_visible_before))
waiting=$(env TOUCHBAR_HOME="$store_dir" "$ctl" session status --format json)
[[ "$waiting" == *'"ready": false'* ]]
[[ "$waiting" == *'"demo.beta#shared"'* ]]
[[ "$waiting" == *'"system_scene_visible": true'* ]]
[[ "$waiting" == *'"plugin_placeholder": {'* ]]
[[ "$waiting" == *'"item": "demo.beta#shared"'* ]]

placeholder_hidden_before=$(rg -c '^plugin-placeholder=hidden$' "$runtime_dir/sessiond.log" || true)
env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$socket_name" \
    "$demo" --plugin-id demo.beta --item-id shared --fixed-width 320 \
    --frames 36000 --require-hardware >"$runtime_dir/replacement-beta.log" 2>&1 &
replacement_beta_pid=$!
for _ in $(seq 1 800); do
    ready_count=$(rg -c '^profile-runtime=ready' "$runtime_dir/sessiond.log" || true)
    placeholder_hidden_now=$(rg -c '^plugin-placeholder=hidden$' "$runtime_dir/sessiond.log" || true)
    ((ready_count >= 2 && placeholder_hidden_now > placeholder_hidden_before)) && break
    sleep 0.01
done
placeholder_hidden_now=$(rg -c '^plugin-placeholder=hidden$' "$runtime_dir/sessiond.log" || true)
((placeholder_hidden_now > placeholder_hidden_before))
restored=$(env TOUCHBAR_HOME="$store_dir" "$ctl" session status --format json)
[[ "$restored" == *'"ready": true'* ]]
[[ "$restored" == *'"active": "default"'* ]]
[[ "$restored" == *'"plugin_placeholder": null'* ]]

kill "$alpha_pid" "$gamma_pid" "$replacement_beta_pid" 2>/dev/null || true
wait "$alpha_pid" 2>/dev/null || true
wait "$gamma_pid" 2>/dev/null || true
wait "$replacement_beta_pid" 2>/dev/null || true
alpha_pid=
gamma_pid=
replacement_beta_pid=
wait "$session_pid"
session_pid=

echo "profile-control=ok identities=qualified profiles=2 manual=ok automatic=ok reload=ok hot_reload=ok invalid_preserved=ok placeholder=host-owned reconnect=ok"
