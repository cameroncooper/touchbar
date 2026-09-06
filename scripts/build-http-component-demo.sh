#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
account_dir=$(getent passwd "$UID" | cut -d: -f6)
rustup_tools="${CARGO_HOME:-$account_dir/.cargo}/bin"
if [[ -x "$rustup_tools/rustup" ]]; then
    export PATH="$rustup_tools:$PATH"
fi
guest_wasm="$project_dir/target/wasm32-wasip2/release/touchbar_http_component_demo.wasm"
package_dir="$project_dir/target/http-component-demo"

if ! rustc --print target-libdir --target wasm32-wasip2 >/dev/null 2>&1; then
    echo "wasm32-wasip2 is unavailable; run: rustup target add wasm32-wasip2" >&2
    exit 1
fi

cargo build --release --target wasm32-wasip2 \
    --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-http-component-demo
mkdir -p "$package_dir/component"
install -m 0644 "$guest_wasm" "$package_dir/component/plugin.wasm"
install -m 0644 "$project_dir/examples/http-component-plugin/touchbar-plugin.toml" \
    "$package_dir/touchbar-plugin.toml"
echo "$package_dir"
