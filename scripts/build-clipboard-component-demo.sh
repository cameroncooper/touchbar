#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
account_dir=$(getent passwd "$UID" | cut -d: -f6)
rustup_tools="${CARGO_HOME:-$account_dir/.cargo}/bin"
if [[ -x "$rustup_tools/rustup" ]]; then
    export PATH="$rustup_tools:$PATH"
fi
guest_wasm="$project_dir/target/wasm32-wasip2/release/touchbar_clipboard_component_demo.wasm"
package_dir="$project_dir/target/clipboard-component-demo"
target_libdir=$(rustc --print target-libdir --target wasm32-wasip2 2>/dev/null || true)
host_triple=$(rustc -vV | sed -n 's/^host: //p')
bundled_component_linker="$(rustc --print sysroot)/lib/rustlib/$host_triple/bin/wasm-component-ld"

if [[ ! -d "$target_libdir" ]] || ! compgen -G "$target_libdir/libcore-*.rlib" >/dev/null; then
    echo "The wasm32-wasip2 Rust standard library is not installed." >&2
    echo "Install it with: rustup target add wasm32-wasip2" >&2
    exit 1
fi
if ! command -v wasm-component-ld >/dev/null 2>&1 && [[ ! -x "$bundled_component_linker" ]]; then
    echo "wasm-component-ld is required to link Component Model plugins." >&2
    echo "On Arch Linux, install it with: sudo pacman -S wasm-component-ld" >&2
    exit 1
fi

cargo build --release --target wasm32-wasip2 \
    --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-clipboard-component-demo
mkdir -p "$package_dir/component"
install -m 0644 "$guest_wasm" "$package_dir/component/plugin.wasm"
install -m 0644 "$project_dir/examples/clipboard-component-plugin/touchbar-plugin.toml" \
    "$package_dir/touchbar-plugin.toml"
echo "$package_dir"
