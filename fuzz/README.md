# TouchBar fuzz targets

These targets use `cargo-fuzz` and nightly Rust. Generated corpora, crash
artifacts, and build products are intentionally ignored; confirmed regressions
belong in deterministic unit tests.

Run a bounded local campaign from the repository root:

```sh
cargo +nightly fuzz run broker-schema --fuzz-dir fuzz -- \
  -max_total_time=60 -max_len=65536 -timeout=5 -rss_limit_mb=1024
cargo +nightly fuzz run broker-wire --fuzz-dir fuzz -- \
  -max_total_time=60 -max_len=65536 -timeout=5 -rss_limit_mb=1024
cargo +nightly fuzz run policy-inputs --fuzz-dir fuzz -- \
  -max_total_time=60 -max_len=65536 -timeout=5 -rss_limit_mb=1024
```

- `broker-schema` attacks every typed capability request/response decoder.
- `broker-wire` attacks both directions of the sequenced-packet IPC protocol.
- `policy-inputs` attacks manifest, grant-store, and effective-policy parsing.

Any crash or hang is a release blocker. Reduce it with `cargo fuzz tmin`, add
the reduced input as a deterministic regression test, then rerun all targets.
