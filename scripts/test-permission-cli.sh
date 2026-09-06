#!/bin/bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
runtime_root="$project_dir/run"
mkdir -p "$runtime_root"
probe_dir=$(mktemp -d "$runtime_root/permission-cli.XXXXXX")

cleanup() {
    if [[ "$probe_dir" == "$runtime_root"/permission-cli.* && -d "$probe_dir" ]]; then
        rm -r -- "$probe_dir"
    fi
}
trap cleanup EXIT INT TERM

cargo build --quiet --manifest-path "$project_dir/Cargo.toml" -p touchbar-cli
cli="$project_dir/target/debug/touchbarctl"
store="$probe_dir/store"
source=github:cameroncooper/touchbar-controls
archive="$probe_dir/controls.touchbar"

"$cli" plugin pack --package "$project_dir/plugins/controls" --output "$archive" >/dev/null
TOUCHBAR_HOME="$store" "$cli" plugin add --path "$archive" >/dev/null

initial=$(TOUCHBAR_HOME="$store" "$cli" plugin permissions "$source" --format json)
rg -q '"status": "needs-consent"' <<<"$initial"

if TOUCHBAR_HOME="$store" "$cli" plugin permission \
    "$source" command.run.v1 allow --session >"$probe_dir/session.out" 2>"$probe_dir/session.err"; then
    echo "session-only permission unexpectedly succeeded without touchbar-sessiond" >&2
    exit 1
fi
rg -q 'session-only decisions require a running touchbar-sessiond' "$probe_dir/session.err"
[[ ! -e "$store/session-permissions.toml" ]]

TOUCHBAR_HOME="$store" "$cli" plugin permission \
    "$source" command.run.v1 allow --persistent >/dev/null
allowed=$(TOUCHBAR_HOME="$store" "$cli" plugin permissions "$source" --format json)
rg -q '"status": "granted"' <<<"$allowed"
rg -q '"from_session": false' <<<"$allowed"
[[ $(stat -c %a "$store/permissions.toml") == 600 ]]
[[ $(stat -c %a "$store") == 700 ]]

TOUCHBAR_HOME="$store" "$cli" plugin permission \
    "$source" command.run.v1 deny --persistent >/dev/null
denied=$(TOUCHBAR_HOME="$store" "$cli" plugin permissions "$source" --format json)
rg -q '"status": "denied"' <<<"$denied"

TOUCHBAR_HOME="$store" "$cli" plugin permission \
    "$source" command.run.v1 reset --persistent >/dev/null
reset=$(TOUCHBAR_HOME="$store" "$cli" plugin permissions "$source" --format json)
rg -q '"status": "needs-consent"' <<<"$reset"

echo "permission-cli=ok persistent=allow-deny-reset session-without-owner=denied"
