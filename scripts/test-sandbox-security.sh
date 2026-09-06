#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
fuzz_seconds=${FUZZ_SECONDS:-0}

cd "$project_dir"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --quiet
# These longer deterministic campaigns use the production filesystem backends
# and async lifecycle. Keep them explicit so the release gate cannot silently
# regress to only the short ordinary unit suite.
cargo test -p touchbar-plugin-supervisor \
    security_campaign_ -- \
    --ignored --test-threads=1
cargo check --manifest-path fuzz/Cargo.toml --bins

if command -v cargo-audit >/dev/null 2>&1; then
    cargo audit
    cargo audit --file fuzz/Cargo.lock
else
    echo "cargo-audit is required: cargo install cargo-audit --locked" >&2
    exit 1
fi

if [[ ! "$fuzz_seconds" =~ ^[0-9]+$ ]]; then
    echo "FUZZ_SECONDS must be a nonnegative integer" >&2
    exit 2
fi
if ((fuzz_seconds > 0)); then
    if ! command -v cargo-fuzz >/dev/null 2>&1; then
        echo "cargo-fuzz is required: cargo install cargo-fuzz --locked" >&2
        exit 1
    fi
    for fuzz_target in broker-schema broker-wire policy-inputs; do
        cargo +nightly fuzz run "$fuzz_target" --fuzz-dir fuzz -- \
            -max_total_time="$fuzz_seconds" -max_len=65536 -timeout=5 \
            -rss_limit_mb=1024 -print_final_stats=1
    done
fi

./scripts/test-supervised-component-host.sh >/dev/null
./scripts/test-permission-cli.sh
if [[ ${RUN_APPLE_GPU:-0} == 1 ]]; then
    ./scripts/test-supervised-component-ui.sh 160 >/dev/null
fi

echo "sandbox security acceptance: PASS"
