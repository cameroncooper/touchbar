# Contributing

## Building

```bash
cargo build --workspace
```

System libraries: libwayland, EGL/GLES, libdrm, libudev, libinput, and
libsystemd. `ripgrep` is a hard dependency of the acceptance scripts. Component
plugin work additionally needs the `wasm32-wasip2` Rust target and
`wasm-component-ld`.

## The release gate

```bash
./scripts/test-sandbox-security.sh
```

This is the gate that matters. It checks formatting and lint policy, runs the
complete workspace suite and the longer ignored security campaigns, compiles
every fuzz target, audits both lockfiles, and exercises a real supervised
component host and the permission CLI.

Add `FUZZ_SECONDS=60` for fresh sanitizer fuzz campaigns and `RUN_APPLE_GPU=1`
to include the Apple GPU DMA-BUF rendering path without taking over the Touch
Bar. `./scripts/test-packaging.sh` validates both systemd units, the udev
rules, and the installer without writing to `/usr`.

### It needs a real login session

Run it from a graphical user session. The supervisor's confinement is not
optional and not simulated: it requires its own cgroup to be a private,
user-owned directory, plus Landlock, seccomp, `pidfd`, and a session bus.
Twelve `touchbar-plugin-supervisor` tests assert exactly those properties.

A container or a hosted CI runner does not provide them. A job cgroup owned by
root fails the ownership check, `systemd-analyze --user` has no user manager to
talk to, and Landlock reports unavailable.

## What CI covers, and what it does not

GitHub Actions runs formatting, lint, the workspace test suite excluding
`touchbar-plugin-supervisor`, fuzz-target compilation, and a dependency audit
of both lockfiles.

`touchbar-plugin-supervisor` is excluded on purpose. Its tests assert operating
system confinement that a hosted runner cannot provide, so running them there
would assert nothing while reporting green — worse than not running them. The
same applies to the supervised component host, the permission CLI, and
packaging.

**Run `./scripts/test-sandbox-security.sh` locally before opening a pull
request.** Its output is the evidence that the sandbox still holds; a green CI
badge is not.

## Documentation images

```bash
./scripts/generate-doc-images.sh
```

Renders every first-party item at each responsive width in both themes through
the production renderer. Replay is deterministic, but rasterization runs on
whatever GPU is present, so images differ between renderers. Regenerate on the
same machine that last did, and `--check` confirms a tree is current locally.

## Commits

Commit messages explain why a change was made, not only what changed. The
project follows a greenfield v1 rule while unpublished: superseded APIs and
formats are changed in place and removed completely rather than kept behind
compatibility shims.
