#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
account_dir=$(getent passwd "$UID" | cut -d: -f6)
rustup_tools="${CARGO_HOME:-$account_dir/.cargo}/bin"
if [[ -x "$rustup_tools/rustup" ]]; then
    export PATH="$rustup_tools:$PATH"
fi
guest_wasm="$project_dir/target/wasm32-wasip2/release/touchbar_broker_component_demo.wasm"
package_dir="$project_dir/target/broker-component-demo"
target_libdir=$(rustc --print target-libdir --target wasm32-wasip2 2>/dev/null || true)
host_triple=$(rustc -vV | sed -n 's/^host: //p')
bundled_component_linker="$(rustc --print sysroot)/lib/rustlib/$host_triple/bin/wasm-component-ld"

if [[ ! -d "$target_libdir" ]] || ! compgen -G "$target_libdir/libcore-*.rlib" >/dev/null; then
    echo "The wasm32-wasip2 Rust standard library is not installed." >&2
    if command -v rustup >/dev/null 2>&1; then
        echo "Install it with: rustup target add wasm32-wasip2" >&2
    elif [[ $(uname -m) == aarch64 ]] && command -v pacman >/dev/null 2>&1; then
        echo "Arch Linux ARM does not package rust-wasm for aarch64." >&2
        echo "Switch to the packaged rustup manager:" >&2
        echo "  sudo pacman -S rustup wasm-component-ld" >&2
        echo "Approve replacing the system rust package, then run:" >&2
        echo "  rustup default stable" >&2
        echo "  rustup target add wasm32-wasip2" >&2
    else
        echo "With rustup: rustup target add wasm32-wasip2" >&2
        echo "On x86-64 Arch Linux: sudo pacman -S rust-wasm" >&2
    fi
    exit 1
fi
if ! command -v wasm-component-ld >/dev/null 2>&1 && [[ ! -x "$bundled_component_linker" ]]; then
    echo "wasm-component-ld is required to link Component Model plugins." >&2
    echo "On Arch Linux, install it with: sudo pacman -S wasm-component-ld" >&2
    exit 1
fi

cargo build --release --target wasm32-wasip2 \
    --manifest-path "$project_dir/Cargo.toml" \
    -p touchbar-broker-component-demo
mkdir -p "$package_dir/component"
install -m 0644 "$guest_wasm" "$package_dir/component/plugin.wasm"
install -m 0644 "$project_dir/examples/broker-component-plugin/touchbar-plugin.toml" \
    "$package_dir/touchbar-plugin.toml"
echo "$package_dir"
