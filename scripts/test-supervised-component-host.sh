#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output=$($root_dir/scripts/run-supervised-component-host.sh hello 160 1)

[[ "$output" == *'supervisor: source=github:cameroncooper/touchbar-component-demo'* ]]
[[ "$output" == *'broker: supervised generation=1'* ]]
[[ "$output" == *'capability: context.read.v1 required=false status=NeedsConsent'* ]]
[[ "$output" == *'Button "HELLO"'* ]]
[[ "$output" == *'activation 1: rerender=true'* ]]
[[ "$output" == *'Toggle "SELECTED"'* ]]

printf '%s\n' "$output"
echo "supervised component host integration: PASS"
