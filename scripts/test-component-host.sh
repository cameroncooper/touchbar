#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
package_dir=$($root_dir/scripts/build-component-demo.sh)

hello=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" -p touchbar-plugin-host -- "$package_dir" hello 160 1)
[[ "$hello" == *'item: hello (Hello)'* ]]
[[ "$hello" == *'broker: unavailable (manual launch; default deny)'* ]]
[[ "$hello" == *'capability: context.read.v1 required=false status=Unavailable'* ]]
[[ "$hello" == *'Button "HELLO"'* ]]
[[ "$hello" == *'activation 1: rerender=true'* ]]
[[ "$hello" == *'Toggle "SELECTED"'* ]]

theme=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" -p touchbar-plugin-host -- "$package_dir" theme 100)
[[ "$theme" == *'Label "DARK #6BF240"'* ]]

replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" -p touchbar-plugin-host -- \
    "$package_dir" --replay "$root_dir/examples/component-plugin/tests/context-replay.json")
[[ "$replay" == *'"report_version": 1'* ]]
[[ "$replay" == *'"label": "APP terminal"'* ]]
[[ "$replay" == *'"label": "APP firefox"'* ]]
[[ "$replay" == *'"name": "focused-app"'* ]]

broker_package_dir=$($root_dir/scripts/build-broker-component-demo.sh)
broker_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$broker_package_dir" --replay \
    "$root_dir/examples/broker-component-plugin/tests/replay.json")
[[ "$broker_replay" == *'"fixture": "playback-status"'* ]]
[[ "$broker_replay" == *'"fixture": "playback-events"'* ]]
[[ "$broker_replay" == *'"fixture": "play-pause"'* ]]
[[ "$broker_replay" == *'"label": "PLAYING"'* ]]
[[ "$broker_replay" == *'"label": "PAUSED"'* ]]
[[ "$broker_replay" == *'"label": "TOGGLED"'* ]]

http_package_dir=$($root_dir/scripts/build-http-component-demo.sh)
http_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$http_package_dir" --replay \
    "$root_dir/examples/http-component-plugin/tests/replay.json")
[[ "$http_replay" == *'"fixture": "latest-commit"'* ]]
[[ "$http_replay" == *'"kind": "http-metadata"'* ]]
[[ "$http_replay" == *'"kind": "http-chunk"'* ]]
[[ "$http_replay" == *'"kind": "http-complete"'* ]]
[[ "$http_replay" == *'"label": "HTTP 200  13 B"'* ]]
http_inline_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$http_package_dir" --replay \
    "$root_dir/examples/http-component-plugin/tests/inline-replay.json")
[[ "$http_inline_replay" == *'"fixture": "latest-commit-inline"'* ]]
[[ "$http_inline_replay" == *'"operation": "request"'* ]]
[[ "$http_inline_replay" == *'"result": "success"'* ]]
[[ "$http_inline_replay" == *'"label": "HTTP 200  11 B"'* ]]

filesystem_package_dir=$($root_dir/scripts/build-filesystem-component-demo.sh)
filesystem_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$filesystem_package_dir" --replay \
    "$root_dir/examples/filesystem-component-plugin/tests/replay.json")
[[ "$filesystem_replay" == *'"fixture": "gallery-list"'* ]]
[[ "$filesystem_replay" == *'"operation": "list-directory"'* ]]
[[ "$filesystem_replay" == *'"fixture": "gallery-preview"'* ]]
[[ "$filesystem_replay" == *'"kind": "filesystem-metadata"'* ]]
[[ "$filesystem_replay" == *'"kind": "filesystem-chunk"'* ]]
[[ "$filesystem_replay" == *'"kind": "filesystem-complete"'* ]]
[[ "$filesystem_replay" == *'"label": "demo.txt  13 B"'* ]]
filesystem_write_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$filesystem_package_dir" --replay \
    "$root_dir/examples/filesystem-component-plugin/tests/write-replay.json")
[[ "$filesystem_write_replay" == *'"fixture": "create-demo"'* ]]
[[ "$filesystem_write_replay" == *'"capability": "filesystem.write.v1"'* ]]
[[ "$filesystem_write_replay" == *'"operation": "create-file"'* ]]
[[ "$filesystem_write_replay" == *'"label": "SAVED 16 B"'* ]]

cargo run --quiet --manifest-path "$root_dir/Cargo.toml" -p touchbar-cli -- \
    plugin build --package "$root_dir/plugins/command-deck" >/dev/null
command_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$root_dir/plugins/command-deck" --replay \
    "$root_dir/plugins/command-deck/tests/replay.json")
[[ "$command_replay" == *'"fixture": "command-probe"'* ]]
[[ "$command_replay" == *'"capability": "command.run.v1"'* ]]
[[ "$command_replay" == *'"kind": "command-stdout"'* ]]
[[ "$command_replay" == *'"kind": "command-exited"'* ]]
[[ "$command_replay" == *'"label": "OK"'* ]]
local_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$root_dir/plugins/command-deck" --replay \
    "$root_dir/plugins/command-deck/tests/custom-replay.json")
[[ "$local_replay" == *'"fixture": "custom-action-service"'* ]]
[[ "$local_replay" == *'"capability": "local.connect.v1"'* ]]
[[ "$local_replay" == *'"operation": "connect"'* ]]
[[ "$local_replay" == *'"operation": "send"'* ]]
[[ "$local_replay" == *'"kind": "local-frame"'* ]]
[[ "$local_replay" == *'"kind": "local-closed"'* ]]
[[ "$local_replay" == *'"label": "OK 22B"'* ]]

desktop_action_package_dir=$($root_dir/scripts/build-desktop-action-component-demo.sh)
desktop_action_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$desktop_action_package_dir" --replay \
    "$root_dir/examples/desktop-action-component-plugin/tests/replay.json")
[[ "$desktop_action_replay" == *'"fixture": "ready-notification"'* ]]
[[ "$desktop_action_replay" == *'"capability": "notification.send.v1"'* ]]
[[ "$desktop_action_replay" == *'"fixture": "touchbar-docs"'* ]]
[[ "$desktop_action_replay" == *'"capability": "uri.open.v1"'* ]]
[[ "$desktop_action_replay" == *'"label": "SENT"'* ]]
[[ "$desktop_action_replay" == *'"label": "OPENED"'* ]]

clipboard_package_dir=$($root_dir/scripts/build-clipboard-component-demo.sh)
clipboard_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$clipboard_package_dir" --replay \
    "$root_dir/examples/clipboard-component-plugin/tests/replay.json")
[[ "$clipboard_replay" == *'"fixture": "read-text"'* ]]
[[ "$clipboard_replay" == *'"capability": "clipboard.read.v1"'* ]]
[[ "$clipboard_replay" == *'"fixture": "write-text"'* ]]
[[ "$clipboard_replay" == *'"capability": "clipboard.write.v1"'* ]]
[[ "$clipboard_replay" == *'"label": "READ 17 B"'* ]]
[[ "$clipboard_replay" == *'"label": "WROTE"'* ]]

secret_package_dir=$($root_dir/scripts/build-secret-component-demo.sh)
secret_replay=$(cargo run --quiet --manifest-path "$root_dir/Cargo.toml" \
    -p touchbar-plugin-host -- "$secret_package_dir" --replay \
    "$root_dir/examples/secret-component-plugin/tests/replay.json")
[[ "$secret_replay" == *'"fixture": "demo-credential"'* ]]
[[ "$secret_replay" == *'"capability": "secret.read.v1"'* ]]
[[ "$secret_replay" == *'"label": "SECRET 18 B"'* ]]
[[ "$secret_replay" != *'fixture-only-token'* ]]

printf '%s\n' "$hello"
printf '%s\n' "$theme"
echo "component host integration: PASS"
