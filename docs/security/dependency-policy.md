# Dependency security policy

The v1 release gate scans both lockfiles with the current RustSec advisory
database:

```sh
cargo audit
cargo audit --file fuzz/Cargo.lock
```

Known vulnerabilities are release blockers. Yanked, unsound, or unmaintained
dependencies are reviewed individually and remain visible in scanner output;
they are not silently ignored. Lockfile updates require the full workspace,
supervised-host, and fuzz-harness acceptance runs.

## Current review — 2026-09-04

RustSec scanned 408 main-workspace dependencies and 56 fuzz-harness
dependencies. It reported no known vulnerabilities. The main graph has one
allowed warning, `RUSTSEC-2026-0192`: `ttf-parser` 0.25.1 is unmaintained. It
is pulled in only by the current upstream `cosmic-text` 0.19 -> `fontdb` 0.23
stack. `cosmic-text` has not yet moved to `fontdb` 0.24, which removed that
dependency.

This warning is not a vulnerability and is not in a broker or permission
parser. In a supervised host, Landlock permits fonts only from the fixed
system font directories; plugins cannot provide a font path. Plugin SVG is
bounded and rasterized without loading plugin-supplied fonts. We will replace
the parser as soon as the text stack supports `fontdb` 0.24 or an equivalent
maintained backend. Until then, every audit continues to print the warning so
it cannot become an invisible permanent exception.

The workspace also intentionally omits Wasmtime's `parallel-compilation`
feature. A supervised headless host creates no threads; live GPU mode permits
only thread-style `clone` for Mesa, bounded by a per-host task rlimit and a
private cgroup leaf.
