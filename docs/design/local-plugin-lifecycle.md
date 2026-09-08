# Local plugin lifecycle

The local lifecycle is complete enough to author, validate, render-test, package, install, configure,
and run a component pack without GitHub or a catalog.

## Author loop

```text
touchbarctl plugin new my-pack --source github:me/my-pack
cd my-pack
touchbarctl plugin build
touchbarctl plugin check --format json
touchbarctl plugin test
touchbarctl plugin dev
touchbarctl plugin pack
```

`new` produces a standalone Rust `wasm32-wasip2` component project tied to the SDK in this source
tree. The starter contains two items and one theme-aware presentation, demonstrating
tap-to-open, hold-slide, semantic colors, responsive sizing, and lifecycle handling.
Component builds select cargo and rustc as one rustup-managed toolchain when
rustup is available, even if `/usr/bin` appears first in `PATH`, and verify that
the selected compiler actually contains the `wasm32-wasip2` standard library.
Advanced callers may instead set both `CARGO` and `RUSTC`; setting only one is
rejected so a mixed sysroot cannot produce a misleading missing-`std` error.
`test` renders every declared item at 80, 160, 320, 1004, and 2008 pixels,
plus every presentation-element min/preferred/max width, through the real component
host. `touchbarctl plugin test --format json` returns a versioned report containing
the exact per-item matrix and complete presentation declarations. `dev` opens
the whole pack in a desktop window using the same supervisor, broker,
confinement, responsive composition, and Wayland/GPU path as an installed pack.
It uses a disposable private store and synthetic input, so preview clicks can
exercise UI but cannot satisfy physical-activation checks. Use `--item ID` to
isolate one contribution, `--width PX` to override its compact width, and
`--scale 1|2|4` to select the desktop display scale. See the
[onscreen simulator](onscreen-simulator.md).

`scripts/test-plugin-scaffold.sh` exercises the generated project as an external
package: create, Wasm build, check, responsive host rendering, JSON diagnostics,
deterministic tap/hold/theme replay, and archive creation. The generated
`tests/interaction.json` is a working starting point; see
[`plugin-replay.md`](plugin-replay.md). This keeps the instructions emitted for
agents executable rather than documentation-only.
Pass `--screenshots screenshots` to render named checkpoints as create-only GPU
PNGs while retaining the complete machine-readable report on standard output.

## Local user loop

```text
touchbarctl plugin add --path ./touchbar-plugin.touchbar
touchbarctl plugin inspect github:me/my-pack
touchbarctl plugin enable github:me/my-pack
touchbarctl plugin item github:me/my-pack main width 240
touchbarctl plugin profile github:me/my-pack firefox disable
touchbarctl plugin list
touchbarctl session status
touchbarctl plugin disable github:me/my-pack
touchbarctl plugin remove github:me/my-pack
```

Install is disabled by default. Packages are copied into an immutable content-addressed directory;
the installer-owned lock stores the canonical source, version, origin, package and artifact digests,
enabled state, enabled items, item widths, and package-profile choices. Reinstalling the same source
preserves matching item and profile choices; newly declared profiles follow their manifest default.
Local packages are explicitly recorded as `local-development`; they cannot claim release
provenance in their manifest.
Only manifest-selected artifacts and documentation/assets enter the package. Symlinks, hard links,
special files, traversal, duplicate archive paths, oversized entries, and trailing archive data are
rejected.

## Runtime ownership

`touchbar-sessiond` owns one supervised host process per enabled component item. It revalidates the complete
installed snapshot against the lock before every launch. This process-per-item choice matches the
current Wayland client, which exposes one surface per live host; it is an implementation detail and
does not change the pack-level manifest or installation model.

Enabled package-owned profiles are merged with the optional user profile
document in memory. They use the existing focus/context composition controller,
so application handoff is seamless and has no lease or auxiliary-session
lifecycle. Exact Hyprland `openlayer`/`closelayer` namespaces are exposed as
reference-counted boolean `activity.NAME` facts for transient pickers and
launchers.

The daemon restarts failed processes with bounded exponential backoff and stops a crash loop after
eight attempts until an explicit reload. Native packs remain an intentionally unrestricted escape
hatch and are launched directly from the exact target artifact selected by the manifest.

Required profile items now fail visibly as well as safely. The session compositor keeps the profile
unready, shows the trusted system row, and draws a theme-aware, host-owned, noninteractive status
layer for a missing item. Component processes waiting on required consent report
`awaiting-consent`; restarting and crash-loop-stopped processes have distinct messages. Pressing Fn
temporarily removes the status layer so the complete trusted function row remains available, and
the status disappears automatically when the real item reconnects.

Every native Wayland client shares one compositor-enforced commit budget across all of its surfaces:
120 commits per second with an eight-commit burst. Excess commits release their pending buffer before
SHM readback or DMA-BUF import, and 256 rejected commits inside one second disconnect the client.
Pending frame callbacks are capped at eight per surface and replaced pending buffers are released.
This keeps raw native/GLES access useful without allowing one client to turn the compositor into an
unbounded import or allocation loop.

The versioned control protocol uses a private `0600` Unix socket inside the private plugin-store
directory. Both client and server verify peer credentials. Messages are strictly decoded and size-
bounded. Linux pathname sockets are limited to 107 bytes; clients and the
server reject an oversized control path before connecting or mutating the
filesystem and identify a shorter `TOUCHBAR_HOME` or `--control-socket` as the
remedy. `enable`, `disable`, item changes, install, and removal ask a running daemon to reconcile;
the durable lock remains the source of truth when the daemon is offline.

Use `TOUCHBAR_HOME` to isolate a development store. Run
`scripts/run-installed-plugin-physical.sh 15` for the end-to-end physical path.
`scripts/test-profile-control-live.sh` proves placeholder appearance and recovery; 
`scripts/test-native-commit-budget.sh` drives an actual abusive Wayland client and proves the
rate-limited disconnect without touching physical hardware.
