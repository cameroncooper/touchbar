#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
guest_manifest="$root_dir/examples/component-plugin/Cargo.toml"
guest_wasm="$root_dir/examples/component-plugin/target/wasm32-wasip2/release/touchbar_component_demo.wasm"
package_dir="$root_dir/target/component-demo/component"

target_libdir=$(rustc --print target-libdir --target wasm32-wasip2 2>/dev/null || true)
if [[ ! -d "$target_libdir" ]] || ! compgen -G "$target_libdir/libcore-*.rlib" >/dev/null; then
    echo "The wasm32-wasip2 Rust standard library is not installed." >&2
    echo "Install it with: rustup target add wasm32-wasip2" >&2
    exit 1
fi

cargo build --release --target wasm32-wasip2 --manifest-path "$guest_manifest"
mkdir -p "$package_dir"
install -m 0644 "$guest_wasm" "$package_dir/plugin.wasm"
install -m 0644 "$root_dir/examples/component-plugin/touchbar-plugin.toml" \
    "$root_dir/target/component-demo/touchbar-plugin.toml"
echo "$root_dir/target/component-demo"
