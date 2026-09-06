# TouchBar

An experimental, GPU-capable Touch Bar system for the 13-inch Apple-silicon
MacBook Pro.

The runtime is deliberately split at the login boundary. The user-facing
`touchbar.service` system unit runs the internal `touchbard` daemon, which
exclusively owns ADP DRM, raw Touch Bar/Fn input, backlight policy, and a tightly
restricted uinput keyboard. The `touchbar-session.service` user unit runs the
internal `touchbar-sessiond` daemon, which owns profiles, layout, themes,
plugins, GPU composition, and semantic interaction. Plugins are isolated
Wayland clients that render only into compositor-assigned surfaces.

If the user service is absent or fails, `touchbard` displays its built-in media
row and transient Fn-held F1–F12 row. A connected session replaces that
conservative scene with the user's themed composition. See the
[service architecture](docs/design/service-architecture.md) for ownership and
naming.

The guarded production-handoff preview exercises fallback, session attach, and
automatic fallback restoration without installing either service:

```bash
./scripts/run-service-handoff-physical.sh 20
```

To inspect the hardware fallback without a user compositor or a timed daemon
restart, run:

```bash
./scripts/run-hardware-fallback-physical.sh 15
```

The media row must appear as soon as `touchbard` finishes hardware setup. Fn
shows F1–F12 only while held, and release restores the media row.
Like tiny-dfr, the hardware policy dims after 30 seconds of inactivity and
turns the strip off after 60 seconds. Touch Bar contacts and ordinary `seat0`
keyboard, pointer, gesture, and touch activity reset that timer.
Every contact in the input batch that wakes a fully dark strip is consumed
through release, so an invisible control cannot activate. A touch on the still
visible dimmed strip remains an ordinary interaction.

If the user composition is broken or unusable, hold the physical Fn key for
eight seconds. `touchbard` disconnects the user compositor and latches its
trusted media/Fn fallback. Release Fn, then hold it for eight seconds again to
resume authenticated user sessions. The gesture does not depend on a visible
or plugin-owned Touch Bar target. `touchbarctl hardware status` reports
`recovery=fallback-locked` while the latch is active.

After installation, `touchbarctl hardware status` reports the system service
and hardware recovery mode,
and `touchbarctl session status` reports the user compositor and its supervised
plugins, including whether its authenticated hardware channel is connected.
Re-running `./scripts/install-development-build.sh` updates an active
development installation in place: it restarts only the hardware and user
services that were already active. It does not activate a previously inactive
installation.

See [PLAN.md](PLAN.md) for the architecture and milestone breakdown.
The decentralized GitHub package model and sandboxed Component runtime are in
[the plugin ecosystem plan](docs/design/plugin-ecosystem.md).
`touchbarctl plugin search` queries the reviewed catalog bundled with this core
build, while `touchbarctl plugin add github:owner/repository` or an exact GitHub
URL installs any compatible public pack without requiring catalog approval.
Publishers can generate a PR-ready listing with `touchbarctl plugin submit`;
the contribution contract lives in [catalog/README.md](catalog/README.md).
Generated release workflows use the native `touchbarctl plugin publish`
transaction, so neither plugin authors nor users need the `gh` executable.
Public installs work without credentials; an optional nonempty `GITHUB_TOKEN`
raises the GitHub API allowance, and rate-limit failures include the advertised
retry/reset information rather than hanging in an automatic retry loop.
The capability threat model, consent UX, and broker contract are in
[the capability and consent design](docs/design/capability-consent.md).
Its executable policy model lives in `crates/touchbar-policy`; it grants no OS
authority itself and is shared by the installer, supervisor, and CLI.
The supervised broker and first exact-scope D-Bus backend live in
`crates/touchbar-plugin-supervisor`; the component path can be
run with `./scripts/run-supervised-component-host.sh hello 160 1`.
Direct supervisor launches require a launcher-owned private `--state`
directory; restart-resistant security counters are stored there and packages
never receive its path or descriptor.

Review and change a sandboxed pack's authority without editing policy files:

```bash
touchbarctl plugin permissions github:cameroncooper/touchbar-controls
touchbarctl plugin permission github:cameroncooper/touchbar-controls \
  command.run.v1 allow --session
touchbarctl plugin permission github:cameroncooper/touchbar-controls \
  command.run.v1 reset --session
```

Every mutation requires an explicit `--session` or `--persistent` lifetime.
Session decisions override durable policy, are accepted only while the
same-user compositor is live, and are erased when it restarts. Use
`--format json` for agent and automation workflows. Filesystem, local-socket,
secret, and clipboard grants additionally require explicit host-owned bindings;
package path hints are never trusted as authority.
The current layer, alpha, appearance, and context-transaction contracts are in
[the visual composition note](docs/design/visual-composition.md).
Sandboxed plugins can build responsive custom GPU drawings with the bounded,
theme-aware [Canvas2D API](docs/design/canvas2d.md); native plugins retain raw
GLES access. Declarative [host-timed animation](docs/design/host-timed-animation.md)
lets those retained scenes run at compositor cadence without calling Wasm on
every frame.
For procedural backdrops and visualizers, sandboxed plugins can use the
[validated GPU-effect tier](docs/design/validated-effects.md): a small
straight-line WGSL body receives normalized geometry, host time, parameters,
and every semantic theme color, then is statically audited and translated to
GLES 3.0 before the Apple driver sees it. Loops, mutable state, textures,
storage, atomics, and raw GPU handles are unrepresentable. The Media pack is a
physical demo with `./scripts/run-first-party-physical.sh media 15`.
Static scenes use an event-driven session loop rather than a background frame
or millisecond polling loop. The wake/deadline contract and measured M1 idle
behavior are documented in
[the performance and power note](docs/design/performance-and-power.md).
Manifest-declared [PNG and symbolic SVG assets](docs/design/package-assets.md)
cross the sandbox boundary as verified sealed bytes and are rendered by logical
ID. Semantic multiply/mask tint follows live host theme changes without
guest-side decoding or GPU access. Run
`./scripts/run-logo-physical.sh 15` for the centered project-wordmark
demo.
The Apple API and ecosystem study is in
[the framework research note](docs/design/touchbar-framework-research.md), and
the retained plugin-side UI contract is in
[the UI Kit Foundation note](docs/design/ui-kit-foundation.md).

## Milestone 1

The first vertical slice proves:

- a private Wayland display used only by Touch Bar plugins;
- a compositor-assigned plugin region;
- an external client rendering an animated shader with GLES;
- damage/commit handling and compositor-driven 60 Hz frame callbacks;
- software readback as a deliberately simple bridge before DMA-BUF zero-copy.

It does not take over the physical Touch Bar yet, so it can be developed
without stopping `tiny-dfr`.

Run the hardware-GLES integration test from a normal user session:

```bash
./scripts/run-m1.sh 600
```

The runner creates a private socket under `run/m1`, starts `touchbar-sessiond`, runs
the external shader plugin, and checks that every submitted frame changed and
that no invalid buffers were accepted. The default 600-frame run takes about
10 seconds on the 60 Hz frame clock.

## Milestone 2 DMA-BUF slice

The current default rendering path removes the plugin-side readback and SHM
copy. The demo renders into a native Wayland EGL window; Mesa allocates Apple
GPU buffers and submits them with `zwp_linux_dmabuf_v1`. `touchbar-sessiond` advertises
v4 device/format feedback, imports each buffer as an EGL image, and samples it
in its own GLES context. The `wl_shm` server path remains available for simple
CPU-rendered plugins.

Run the Apple-GPU/DMA-BUF acceptance test:

```bash
./scripts/run-m2.sh 600
```

This headless test reads back the compositor's final 2008 by 60 target only to
produce a changing-frame checksum. There is no CPU pixel transfer between the
plugin and compositor. Physical ADP presentation remains an M3 task, so this
test does not stop or replace `tiny-dfr`.

The retained-layer test runs two independent plugin processes and composites
both into one 2008 by 60 scene at the panel refresh rate:

```bash
./scripts/run-m2-dual.sh 600
```

Each plugin is isolated behind its own Wayland surface and GPU texture. A
plugin only replaces its own retained layer when it commits a completed frame;
the compositor builds the scene once per refresh tick, so one late plugin does
not erase the other plugin's content.

Buffer release is asynchronous. After copying a submitted DMA-BUF into its
retained layer, `touchbar-sessiond` inserts a GPU-completion fence, polls it with a zero
timeout, and sends `wl_buffer.release` only after it signals. This avoids the
old per-commit `glFinish` stall. Test isolation with one 60 FPS plugin and one
deliberately delayed plugin:

```bash
./scripts/run-m2-slow-client.sh 600
```

DMA-BUF acquire synchronization is explicit when the client EGL stack supports
`EGL_ANDROID_native_fence_sync`. The Rust GLES runtime exports one Linux
`sync_file` after rendering each frame; the session compositor validates the
descriptor and queues a GPU-side wait before sampling it. There is no CPU wait
in the Wayland event loop. Implicit synchronization remains the fallback for
clients whose EGL stack cannot export native fences.

Run the nonphysical Apple-GPU conformance and abuse test:

```bash
./scripts/test-explicit-sync.sh
```

It requires 120/120 explicitly synchronized DMA-BUF frames at least 55 FPS,
then verifies fatal rejection of duplicate fences, fence-only commits, fences
on SHM buffers, and descriptors that are not Linux `sync_file` objects. The
complete ownership and error contract is in
[DMA-BUF synchronization](docs/design/dmabuf-synchronization.md).

## Milestone 3 physical ADP smoke test

The first physical-output slice discovers the DRM card by its Apple `adp`
driver rather than assuming a card number. Its default probe is read-only and
does not interrupt the existing Touch Bar manager:

```bash
./scripts/run-m3-logo.sh --probe
```

The display mode is deliberately guarded. The following command asks for root
authorization, stops `tiny-dfr`, renders `/usr/share/touchbar/icon.png` in the
center of the physical Touch Bar for 10 seconds, and restores `tiny-dfr` from
an exit trap:

```bash
./scripts/run-m3-logo.sh --display 10
```

Durations are limited to 30 seconds. The current demo uses one mapped
XRGB8888 scanout buffer. A paced physical animation is available with:

```bash
./scripts/run-m3-logo.sh --animate 5
```

On the current experimental ADP kernel, both direct vblank waits and atomic
framebuffer flips measure 29.90 Hz even though the mode metadata says 60 Hz.
Plugin GLES rendering remains capable of 60 FPS; physical presentation is
temporarily limited by the ADP driver.

The mode is fixed software metadata and the driver inherits firmware display
timing, so the panel's physical scan frequency is not yet proven. The guarded
correlation probe records DRM vblank timestamps, contiguous sequence numbers,
and the raw `adp-fe` IRQ count in one run:

```bash
./scripts/run-adp-cadence-physical.sh 5
```

See [the ADP cadence investigation](docs/design/adp-cadence.md) for the
evidence, remaining ambiguity, and safe kernel-fix sequence.

Audit the exact kernel-source assumptions behind that investigation without
touching hardware:

```bash
./scripts/audit-adp-cadence-source.sh /path/to/linux
```

The static native GLES path is also regression-tested end to end: it commits
one Apple-GPU DMA-BUF frame, remains connected, and waits without a frame loop.

```bash
./scripts/test-static-native-client.sh
```

Once the fresh services are installed and the laptop can be unplugged, inspect
or explicitly run the controlled ABBA energy comparison with:

```bash
./scripts/measure-installed-power.sh --check
./scripts/measure-installed-power.sh --run 20
```

The end-to-end runner starts two isolated GLES/DMA-BUF plugins, composites them
at 60 FPS, publishes complete scenes through a triple-buffered shared-memory
handoff, and displays the newest scene on the physical strip:

```bash
./scripts/run-m3-scene.sh 5
```

The current scene uses the Touch Bar's complete 2008 by 60 logical canvas. The
physical backend consumes at the kernel's measured 29.9 Hz cadence and drops
superseded frames without slowing plugins. This is the reliable-copy baseline.

The next zero-copy prerequisite has also passed: an off-screen probe exported
an unused ADP XRGB8888 buffer through PRIME, imported it as an EGL render target
on the Apple M1 GPU, and observed the GPU-written pixel through ADP's mapping.
The probe neither acquires DRM master nor changes the physical display:

```bash
sudo target/debug/touchbard --prime-probe
```

The remaining integration step is to pass a small ADP swapchain from the
privileged presenter to unprivileged `touchbar-sessiond`, render its GPU-resident scene
into those buffers with the physical rotation, and atomically flip completed
buffers. Until that path lands, `run-m3-scene.sh` continues to use the reliable
shared-memory handoff.

The descriptor-transfer and direct-render portion can be tested without
stopping `tiny-dfr` or changing the display:

```bash
./scripts/run-m3-prime-ipc.sh
```

`touchbar-sessiond` owns the private socket. The authorized ADP helper connects and
lends two unused buffers via `SCM_RIGHTS`; it retains DRM ownership and never
exposes a card descriptor. The compositor renders a deterministic centered
scene into both buffers on AGX, and the helper maps them only to verify the
expected output bytes. The test also exercises sequenced `buffer-ready` and
`buffer-released` messages in both directions. Live presentation can now use
this same bounded ownership protocol for atomic flips.

## Milestone 3 live zero-copy output

The live path is now operational. This runner temporarily stops `tiny-dfr`,
starts two independent GLES plugins, passes a three-buffer ADP swapchain to
unprivileged `touchbar-sessiond`, and restores `tiny-dfr` through the guarded root
wrapper:

```bash
./scripts/run-m3-direct.sh 5
```

Unlike `run-m3-scene.sh`, this path does not publish or copy an RGBA frame
stream. Plugin DMA-BUFs are retained in the compositor, the final scene stays
in a GLES texture, and AGX renders the centered 90-degree transform directly
into released ADP XRGB8888 buffers. The presenter alone retains DRM master and
returns a buffer only after its replacement flip completes.

The first acceptance run produced 151 physical updates in 5.015 seconds
(29.91 FPS), accepted 613 plugin DMA-BUF commits with zero invalid frames, and
released all 613 plugin buffers. The measured presentation rate remains the
limit of the current experimental ADP kernel driver; plugin rendering and
Wayland commits continue independently at roughly 60 FPS per plugin.

## Milestone 4 model-first layout

M4 deliberately starts with semantics rather than a plugin manifest format or
hard-coded left/center/right zones. The design is informed by AppKit's Touch
Bar concepts: ordered stable item identifiers, user customization, visibility
priority, flexible spacing, principal-item centering, contextual composition,
groups, and expanded bars. See
[the Apple model study](docs/design/apple-touchbar-model.md).

The dependency-free `touchbar-layout` crate resolves an already composed bar
into deterministic rectangles. It knows nothing about plugin discovery,
serialization, Wayland, or DRM. This lets us test layout policy before making
the plugin package format public:

```bash
cargo test -p touchbar-layout
```

The `touchbar-model` crate sits one level above the resolver. It registers
stable, accessibility-labeled item definitions and supports user-owned named
profiles made from arbitrary slots. `fixed` slots retain user-selected content,
`collect` slots merge all matching contributions, and `select` slots choose one
matching contribution by priority and scope. Applications therefore replace
only contextual content while global controls can remain visible. Profiles
reuse contribution definitions, so changing the complete slot arrangement does
not change shared item identities.

The in-process composition controller applies automatic profile rules, manual
overrides, and owner-scoped temporary mode leases through one command boundary.
Every result includes retained, entering, and leaving item IDs for future live
surface reconciliation. Updates are atomic, invalid commands roll back, and the
newest pending composition waits until all captured contacts end:

```bash
cargo test -p touchbar-model
```

See [the profile composition contract](docs/design/profile-composition.md).

The first live bridge reconciles those snapshots against connected Wayland
surfaces by stable item ID. It sends visibility changes, resolves new widths,
and uses the existing configure/acknowledge lifecycle for dynamic resizing.
Rapid changes retain a bounded ordered configure queue, so acknowledging an
older valid resize cannot be mistaken for a stale protocol request.

Run the hardware-GPU context acceptance demo without taking over the physical
Touch Bar:

```bash
./scripts/run-context-ui.sh
```

Four independent clients provide a persistent volume item, terminal content,
browser content, and persistent status. A deterministic focus replay changes
only the application slot. One change occurs during a captured volume gesture
and is deferred until release; the wide browser layout also resizes the same
volume surface from 80 to 72 pixels. `--hyprland-context` replaces replay with
normalized `activewindow`, `workspace`, and `focusedmon` events from the live
Hyprland event socket.

Production profiles use the strict fresh-v1 TOML contract shown in
[`config/profiles.toml.example`](config/profiles.toml.example). Items are
identified by a structured `{ plugin, item }` pair, so two packages can safely
reuse the same local item name. The packaged session service reconnects to
Hyprland after either side restarts. Profiles can be inspected, selected, and
returned to automatic context selection at runtime:

```bash
touchbarctl session profile list
touchbarctl session profile select minimal
touchbarctl session profile automatic
touchbarctl session reload
```

The release acceptance covers package-qualified identity, manual and automatic
selection, required-item fallback, reload, and item reconnection:

```bash
./scripts/test-profile-control-live.sh
```

## Rust plugin SDK and UI kit

`touchbar-client` removes Wayland and EGL boilerplate from standalone Rust
plugins. It connects to the private display, creates the managed surface,
acknowledges compositor configuration, owns the GLES3 context, submits native
Wayland EGL buffers, and schedules rendering through frame callbacks. A plugin
implements the `Application` trait and retains direct access to
`glow::Context` for custom shaders:

```rust
impl touchbar_client::Application for MyPlugin {
    fn render(
        &mut self,
        graphics: &touchbar_client::Graphics,
        frame: touchbar_client::FrameInfo,
    ) -> anyhow::Result<touchbar_client::FrameFlow> {
        // Render through graphics.gl().
        Ok(touchbar_client::FrameFlow::Animate)
    }
}
```

`touchbar-ui` is an optional plugin-side layer. Its retained foundation uses
Taffy for dynamically constrained rows and columns while preserving
Touch-Bar-specific responsive representations and priority hiding. It provides
renderer-measured Unicode text, intrinsic image sizing, semantic inspection
nodes, composable pressables, buttons, toggles, sliders, determinate and
indeterminate progress, procedural icons, fitted/tintable cached RGBA images,
nested opacity, validated procedural-effect leaves, embedded custom-GLES
leaves, tweening, composable gestures,
stationary selection palettes, and a virtualized horizontal scrubber. Every
built-in paint accepts a semantic theme role; custom GLES leaves receive the
same live `Theme` snapshot and resolved clip as their surrounding nodes. A
visibility-aware scheduler lets static content sleep
while bounded or continuous animation requests frame callbacks. It does not
send a widget tree to `touchbar-sessiond`; it renders pixels through the same DMA-BUF
path as a raw GLES plugin. See [the complete foundation
contract](docs/design/ui-kit-foundation.md) and the [collection/gesture
milestone](docs/design/collection-gesture-foundation.md).

Run the headless UI rendering acceptance demo without taking over the physical
Touch Bar:

```bash
./scripts/run-sdk-ui.sh 120
```

Exercise the tap-then-tap path with `./scripts/run-sdk-ui.sh 120 tap`, or the
two-page navigation path with `./scripts/run-sdk-ui.sh 120 nested`.

For interactive development, open an entire component pack in the real
compositor without taking over the physical Touch Bar:

```bash
target/debug/touchbarctl plugin dev --package plugins/controls
```

The command uses an isolated disposable plugin store, runs every contribution
through its production sandbox/supervisor and the M1 GPU compositor, and opens
the final composed 2008×60 scene as a desktop Wayland window. Mouse drags and
native touch use the production input router but are marked synthetic, so they
cannot authorize OS actions. Use `--item volume --width 240` to isolate a
responsive widget, `--scale 1|2|4` to change display density, and Escape to
close. See the [onscreen simulator design](docs/design/onscreen-simulator.md).

Sandboxed WebAssembly plugins use `touchbar-component-sdk`. It exports the
single `touchbar:plugin/plugin@1.0.0` world plus small typed
wrappers for asynchronous request and resource IDs. Broker calls made from an
input or host-event callback return immediately; completion, capability
changes, overflow, and shutdown arrive through `Guest::handle_host_event` on
the same UI thread. The broker socket, package identity, grants, and raw OS
handles never enter WASI. See
[`examples/broker-component-plugin`](examples/broker-component-plugin) for the
minimal compile-checked package. Every component implements the callback; there
is no polling world or compatibility adapter.

The local lifecycle is now executable through `touchbarctl plugin`: scaffold, build, agent context,
strict check, four-width headless test, supervised live development, package, install, inspect,
enable/disable, per-item sizing, and removal. `touchbar-sessiond` reconciles the durable content-addressed
store over a versioned same-user control socket and exposes process health through
`touchbarctl session status`. See the [local lifecycle](docs/design/local-plugin-lifecycle.md) and the
[five first-party pack slices](docs/design/first-party-packs.md). To render a pack on hardware:

```bash
./scripts/run-first-party-physical.sh controls 20
```

That safe default leaves OS authority denied. To test real Controls commands or
Media D-Bus actions, opt into the exact grant for the disposable demo session:

```bash
./scripts/run-first-party-physical.sh controls 20 --allow-session-actions
./scripts/run-first-party-physical.sh media 20 --allow-session-actions
```

The first production capability backend is `dbus.call.v1`. Its SDK encoder
accepts structured call fields and string arguments, never raw D-Bus messages.
The supervisor matches the bus, destination, path, interface, member,
signature, and constrained arguments before queueing work; activation-gated
rules consume one compositor-issued physical gesture. The reference component
reads MPRIS `PlaybackStatus` and invokes `PlayPause`. Unless the exact grant
allows service activation, the transport resolves the approved well-known name
and calls its unique current owner; absent names are not started and owner
handoffs cannot redirect a call. Calls time out after two seconds, and raw
replies are size/signature/descriptor checked before typed decoding.
It can also open an exact `dbus.subscribe.v1` `PropertiesChanged` resource.
Ordered, typed signal events arrive through the same host callback; close,
transport failure, owner change, revocation, or scope narrowing tears down the
live match.

The production zbus path has a deterministic integration test that creates a
private D-Bus daemon and fake MPRIS service, so it cannot read from or control a
real media player:

```bash
cargo test -p touchbar-plugin-supervisor \
  dbus::tests::private_bus_exercises_real_call_and_subscription_transports
```

The matching physical acceptance demo uses the sandboxed reference component,
exact digest-bound development grants, that same isolated service model, and
the real supervisor/host/compositor/presenter pipeline:

```bash
./scripts/run-broker-ui-physical.sh 20 320
```

Tap `STATUS` once to open the subscription; it then alternates between
`PAUSED` and `PLAYING`. Tap `PLAY/PAUSE` to exercise the call that requires a
fresh physical activation. The build requires a Rust toolchain with the
`wasm32-wasip2` target and `wasm-component-ld`; the script reports the exact
missing prerequisite before taking over the Touch Bar. On x86-64 Arch,
`rust-wasm` supplies the target. Arch Linux ARM does not currently publish that
package for `aarch64`, so those systems use a rustup-managed toolchain; its
`wasm32-wasip2` target includes the component linker. The project scripts detect
the user-local rustup tools even before a new login shell refreshes `PATH`.

The v1 compositor protocol includes two visual-composition facilities alongside
stable item identity, compact/expanded sizing, captured input, and bounded
presentation lifecycle:

- A single non-interactive backdrop surface receives the complete 2008 by 60
  canvas below every item and presentation. It may render static art or a continuous
  GPU animation without owning layout or input.
- `touchbar-sessiond` publishes atomic semantic appearance snapshots. The Rust UI kit
  maps background, surface states, foreground, muted, accent, destructive, and
  corner radius values into `Theme`; raw GLES plugins may consume or ignore the
  same toolkit-neutral values.

Temporary UI uses identified presentation sessions. They separate transient
hold-slide-release from persistent tap-then-tap interaction, report
compact-anchor geometry where applicable, carry explicit dismissal reasons,
reject stale session IDs, and keep nested page navigation inside the plugin.
All five policies have compositor-owned contracts: bounded anchored overlays,
normal-layout in-place expansion, active-profile slot replacement, exact named
regions, and explicit full-bar replacement. Invalid targets are rejected rather
than silently changing policy. See
[`docs/design/presentation-sessions.md`](docs/design/presentation-sessions.md).

Every v1 touch event carries a compositor-issued 64-bit gesture sequence.
Sandboxed component hosts use it to attach a short-lived physical activation
only while handling the corresponding activated widget.

Colors are unpremultiplied at the protocol/UI API boundary and premultiplied at
rasterization. Both the UI renderer and compositor use `ONE, ONE_MINUS_SRC_ALPHA`,
so translucent controls compose correctly over animated backdrops without
dark fringes. `touchbar-sessiond` watches
`~/.config/touchbar/theme.toml` by default, uses a built-in palette when that
file is absent, and accepts `TOUCHBAR_THEME` as an explicit source.
The same file may set `animation_hz` and `battery_animation_hz` from 1 through
60. Defaults are 60 Hz on external or unknown power and 30 Hz on battery. This
caps compositor-issued animation callbacks; static and input-driven content
remain event-driven. `touchbarctl session status` reports the detected power
source and current effective animation cadence.

The acceptance runner starts an 80-pixel responsive volume item above a
full-width GLES backdrop whose shader consumes the current background, accent,
and foreground roles. The compact control is a generic pressable containing a
theme-aware icon and an embedded GLES meter; wider variants add retained text
and progress layout. A theme switch therefore recolors the complete strip
rather than only the small control. A hold opens a 360-pixel fixed option
palette while the remaining 1648 pixels of the backdrop stay visible. The
volume activator stays pinned at the expansion edge, the captured finger slides
across stationary options, and release restores the compact item. A short tap
instead opens a persistent palette for a second-tap selection. The nested demo
pushes a detail palette and returns without replacing the compositor session.
On this M1, the test completes at about 60 composed frames per second with
DMA-BUF buffers remaining on the GPU.

Run the same UI on the physical Touch Bar with real Z2 multitouch input:

```bash
./scripts/run-sdk-ui-physical.sh 15
```

This guarded runner asks for authorization, temporarily stops `tiny-dfr`, and
restores it on normal exit, failure, or interruption. The scene fills the
2,008-pixel display. Either hold the 80-pixel volume button and slide to a fixed
choice before releasing, or tap it and then tap a choice. Tap the pinned volume
button again to close; an idle persistent palette also times out. The
privileged presenter owns DRM and reads the root-restricted `apple_z2` evdev
device; it sends only normalized contacts and swapchain lifecycle messages to
unprivileged `touchbar-sessiond`.

## Media UI showcase

`touchbar-media-demo` is the first complex reference plugin built from the UI
kit rather than a new daemon-side widget type. It combines cached artwork,
responsive metadata, nested pressables, progress, a captured timeline,
transport controls, and a theme-aware custom GLES waveform. Fake playback data
keeps this milestone deterministic; an eventual MPRIS source will remain in
the standalone plugin process.

Run its tap-to-expand and hold-slide paths on the Apple GPU:

```bash
./scripts/run-media-ui.sh 180 160 tap
./scripts/run-media-ui.sh 180 160 hold
```

The second argument may be `80`, `160`, or `420` to force each responsive
compact representation. Use `./scripts/run-media-ui-physical.sh 15 160` for a
real Touch Bar run. See the [media widget design and interaction
contract](docs/design/media-widget-showcase.md).

## Sandboxed Component plugin slice

Community-oriented plugins can now use the versioned WebAssembly Component
interface instead of receiving Wayland, GLES, filesystem, network, environment,
or session-service access directly. `touchbar-plugin-host` validates the
package's retained UI tree, applies deterministic resource limits, and renders
it with the same theme-aware native GLES UI kit used by trusted process
plugins. The original semantic headless harness remains available for fast
agent-authored plugin tests.

Run the complete component-to-Wayland-to-DMA-BUF path with a deterministic
synthetic tap:

```bash
./scripts/run-component-ui.sh 160
```

Run it on the physical Touch Bar with real input:

```bash
./scripts/run-component-ui-physical.sh 15 160 hello
```

The `theme` item demonstrates live scheme/accent propagation:

```bash
./scripts/run-component-ui-physical.sh 15 160 theme
```

## Sandbox v1 security gate

Run the complete deterministic release gate with:

```bash
./scripts/test-sandbox-security.sh
```

It checks formatting and lint policy, runs the complete workspace suite plus
the release-only filesystem mutation/pressure, D-Bus signal/ownership,
command fork/memory/reclamation, desktop-portal response/lifecycle, and
isolated Secret Service campaigns, compiles every fuzz target, audits
both dependency graphs, and exercises a real supervised component host. Add
`FUZZ_SECONDS=60` for fresh
sanitizer fuzz campaigns and `RUN_APPLE_GPU=1` to include the supervised Apple
M1 DMA-BUF rendering path without taking over the physical Touch Bar:

```bash
FUZZ_SECONDS=60 RUN_APPLE_GPU=1 ./scripts/test-sandbox-security.sh
```

The [adversarial review](docs/security/sandbox-abuse-review.md) records the
accepted abuse cases and residual risks. The [dependency
policy](docs/security/dependency-policy.md) records the audit result and the
one visible, non-vulnerable maintenance warning.
