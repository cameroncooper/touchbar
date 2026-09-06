# Engineering plan

## Greenfield v1 rule

This project is unpublished and has one current contract. During development,
superseded APIs and formats are changed in place and removed completely. Do not
add backward-compatibility branches, legacy loaders, aliases, shims, migration
code, dual protocol events, or cached-artifact fallbacks. A compatibility policy
begins only after an explicit public release decision.

## Product boundary

TouchBar is a domain-specific compositor and plugin host. It is not a
general Wayland desktop compositor. The privileged `touchbard` service owns
only hardware, session arbitration, a restricted virtual keyboard, and the
emergency media/Fn scene. The unprivileged `touchbar-sessiond` owns the final
user scene, layout policy, input focus, gestures, animation scheduling, themes,
and plugins. Plugins own only the contents of surfaces assigned to them.

The logical coordinate system is always horizontal, 2008 by 60 pixels. The
ADP backend alone handles the physical 60 by 2008 orientation.

## Architectural decisions

1. **Wayland is the surface protocol.** Plugins use `wl_surface` semantics,
   buffer release, damage, and frame callbacks instead of a bespoke graphics
   IPC protocol.
2. **A custom Touch Bar shell assigns roles.** Plugins identify themselves;
   configuration and compositor policy determine their region and stacking.
   The private display does not expose `xdg-shell`.
3. **GPU rendering is opt-in per plugin.** Simple widgets may use shared
   memory. Advanced plugins use EGL/GLES and DMA-BUF-backed buffers.
4. **The compositor is never blocked by a plugin.** Late frames are skipped and
   the last completed buffer remains visible. Buffer counts, dimensions, and
   commit rates are bounded.
5. **The hardware service owns presentation.** Plugins and the user compositor
   never receive DRM master or raw Z2/keyboard input. `touchbar-sessiond`
   receives normalized contacts and a bounded ADP DMA-BUF swapchain only after
   `touchbard` authenticates it as the active seat user.
6. **Rendering is event driven.** A static scene consumes no frame loop.
   Animated clients receive compositor-paced frame callbacks at the effective
   theme and power cadence; physical scanout retires only at ADP vblank.
7. **Hardware and user policy stay separate.** `touchbard` remains useful
   before login and after a user-daemon crash; a headless backend keeps layout,
   protocol, and plugin tests independent of physical hardware.
8. **Every plugin surface is isolated from the compositor.** Sandboxed
   WebAssembly Components are the standard ecosystem runtime and run in a
   separately confined native host with explicit capability brokers. Native
   processes remain the trusted escape hatch for direct Wayland/GLES and
   ordinary session access. The daemon never loads plugin code into its own
   address space.
9. **The client SDK is optional.** It wraps Wayland/EGL lifecycle and offers a
   lightweight GPU UI kit, while raw Wayland and GLES remain supported escape
   hatches for advanced clients.

## Intended runtime pipeline

```text
plugin GLES contexts
       │
       │ ARGB8888 DMA-BUF + acquire point
       ▼
Wayland surface state
       │
       ▼
layout / clip / opacity / transforms
       │
       ▼
single GLES composition pass
       │
       │ final-scene DMA-BUF
       ▼
touchbard-owned XRGB8888 swapchain
       │ atomic page flip at vblank
       ▼
Touch Bar OLED
```

## Plugin contract

Every plugin pack registers stable items and package-local presentation bars.
The compositor sends configured dimensions, visibility, theme snapshots, and
surface-local touch. A native plugin may integrate directly with session
services. A component plugin receives only its retained UI WIT world and the
specific user-approved broker capabilities declared by its verified package;
`touchbar-sessiond` remains a compositor rather than an application-services
API.

The v1 custom protocol provides a `touchbar_surface` role with:

- plugin identity;
- configure/acknowledge serials;
- assigned width and height;
- visibility and active-layer state;
- explicit presentation-session requests and lifecycle events.

Standard Wayland objects continue to provide buffer attachment, damage,
commit, release, and frame callbacks.

## Milestones

### M1 — external GLES client and frame contract

**Status: complete (2026-09-03).** The optimized acceptance run used the
hardware `Apple M1 (G13G B1)` renderer and delivered 600 distinct valid frames
in 9.981 seconds (60.01 FPS), with zero rejected buffers.

- Start a private Wayland socket.
- Accept one plugin surface and assign a fixed test region.
- Render an animated GLES shader in an external process.
- Commit frames through a shared-memory bridge.
- Pace animation using compositor-issued frame callbacks at 60 Hz.
- Record frame count, damage, deadline misses, and a framebuffer checksum.
- Run without accessing ADP DRM or interrupting `tiny-dfr`.

Exit criteria: a 10-second integration test observes at least 550 valid,
changing frames from an external hardware-GLES client, with bounded memory and
clean process shutdown.

### M2 — GPU compositor and DMA-BUF plugin surfaces

**Status: complete (2026-09-03).** The first DMA-BUF vertical slice is
complete. Both client and compositor use `Apple M1 (G13G B1)`; a 600-frame
acceptance run delivered 600 changing DMA-BUF frames, zero SHM frames, and
zero invalid frames in 9.982 seconds (60.01 FPS). The v1 protocol's device and
format/modifier feedback, native Wayland EGL allocation, EGL-image import, and
the SHM server fallback are implemented.

The retained-layer slice is also complete. Two isolated GLES clients each
submitted 600 DMA-BUF frames into separate 502 by 60 regions. The compositor
retained each plugin in its own GPU texture and produced 600 complete scenes
in 9.981 seconds (60.01 FPS), with 1,200 accepted commits and zero invalid
buffers. The Rust GLES SDK now exports an `EGL_ANDROID_native_fence_sync`
`sync_file` for each DMA-BUF commit. `touchbar-sessiond` validates it with the
Linux `SYNC_IOC_FILE_INFO` UAPI and queues a non-blocking `eglWaitSync` before
sampling. Clients without that EGL extension may use implicit synchronization.
Compositor GPU-completion fences remain non-blocking before
`wl_buffer.release`.
A fast-plus-delayed-client run accepted and asynchronously released all 750
buffers while maintaining 600 changing scenes in 9.980 seconds (60.01 FPS),
with only two releases pending at peak. A fresh explicit-sync acceptance run
submitted 120 explicitly fenced frames at 60.16 FPS on the Apple M1 renderer,
then proved fatal protocol rejection of duplicate fences, fence-only commits,
fences on SHM buffers, and non-`sync_file` descriptors. See
`docs/design/dmabuf-synchronization.md`.

- Advertise `zwp_linux_dmabuf_v1` with formats/modifiers supported by AGX.
- Import plugin buffers as EGL images.
- Composite multiple premultiplied-ARGB surfaces with clipping and opacity.
- Add acquire/release synchronization and never block on a late client.
- Retain `wl_shm` as the software-plugin fallback.

Exit criteria: two isolated GLES clients animate simultaneously without a CPU
pixel readback between plugin and compositor.

### M3 — ADP output backend

**Status: complete (2026-09-03).** ADP discovery, guarded DRM takeover,
physical orientation, and service recovery are working. A root-gated smoke
test discovered `DSI-1` dynamically, selected its 60 by 2008 mode at 60 Hz,
displayed the TouchBar logo in the center of the physical Touch Bar
for 10 seconds, then returned DRM ownership to `tiny-dfr`. The recovery wrapper
verified that `tiny-dfr.service` was active and running afterward. The user
confirmed the physical orientation and placement visually.

The mapped-scanout animation path is also complete. Direct kernel vblank waits
measure 29.90 Hz even though the fixed mode metadata advertises 60 Hz. Atomic
framebuffer flips complete at the same 29.90 FPS, and combining a separate
vblank wait with a blocking flip halves that again. This is a limitation of the
current experimental ADP kernel driver, which relies on bootloader-initialized
display timing; it is not a compositor or GPU throughput limit. M3 will target
the measured hardware cadence while keeping plugin rendering independently
capable of 60 FPS. A true 60 Hz presentation path is tracked as a kernel-driver
follow-up.

The first end-to-end GPU-plugin-to-physical-output slice is complete. Two
isolated plugins submitted 933 DMA-BUF frames, `touchbar-sessiond` produced 467 changing
GPU-composited scenes in 7.766 seconds (60.00 FPS), and the ADP presenter showed
150 newest complete scenes during its five-second window. It intentionally
dropped 153 superseded source frames at the 60-to-29.9 Hz boundary rather than
blocking either plugin. The current handoff is a triple-buffered, seqlock-
protected RGBA shared-memory stream and a final mapped scanout copy.

The direct-render prerequisite is also proven without taking DRM master or
changing the display. An unused 64 by 2048 XRGB8888 dumb buffer allocated by
ADP was exported through PRIME, imported into an EGL image on
`Apple M1 (G13G B1)`, attached as a complete GLES framebuffer, and cleared by
the GPU. Mapping the same ADP buffer returned the expected BGRA bytes
`[40, 80, ff, ff]`. The compositor scene is now backed by its own GPU texture
and framebuffer, preparing it for a rotated final pass into an exported ADP
swapchain instead of readback.

The privilege-boundary slice now passes as well. Unprivileged `touchbar-sessiond`
created a private Unix socket; a root ADP helper connected and transferred two
64 by 2008 PRIME descriptors with `SCM_RIGHTS`; and the compositor imported,
centered, rotated, and rendered into both buffers on the Apple GPU. The helper
mapped the same allocations and verified the expected BGR pixel
`[80, 40, 20]`. This test never acquired DRM master and left display state
unchanged. The descriptor protocol bounds the swapchain to two or three
buffers, validates all layout metadata, and applies close-on-exec when
receiving FDs. Bidirectional, fixed-size `buffer-ready` and `buffer-released`
events now carry buffer indices and monotonically increasing sequence numbers;
the same-stream hardware test passed for both buffers.

The live zero-copy acceptance run completed with a three-buffer ADP swapchain.
Two isolated Apple-GPU plugins submitted 613 DMA-BUF frames with zero invalid
buffers and all 613 plugin buffers released. The unprivileged compositor kept
the scene GPU-resident, applied the physical rotation in a final GLES pass, and
submitted only released ADP buffers. The privileged presenter completed 151
scanout updates in 5.015 seconds (29.91 FPS), returned retired buffers with
sequenced release messages, and restored `tiny-dfr.service` to active state.
No `glReadPixels` or shared-memory framebuffer transfer occurs during direct
presentation.

- ~~Discover the non-desktop ADP connector rather than relying on card numbers.~~
- ~~Characterize buffered flips and the driver-exposed vblank cadence.~~
- ~~First implement the reliable mapped-buffer copy path.~~
- ~~Test ADP-buffer export and AGX import for direct GPU rendering.~~
- ~~Add atomic page flips, vblank timestamps, and GPU rotation.~~
- Add production suspend and backlight policy in M4.
- ~~Restore the previous Touch Bar manager on every exit path.~~

Exit criteria: the composed scene runs on the physical strip at the maximum
cadence exposed by ADP and restores `tiny-dfr` after normal exit or failure.
Reaching a true measured 60 Hz additionally requires an ADP kernel fix.

The kernel investigation is documented in
`docs/design/adp-cadence.md`. The driver publishes a fixed 60 Hz software mode
but does not program timing, so firmware may actually have configured either a
30 Hz scan or a 60 Hz scan with half-rate front-end interrupts. A guarded probe
now correlates DRM timestamps and sequence numbers with raw `adp-fe` IRQ counts;
physical-frequency measurement and m1n1 tracing are required before changing
undocumented interrupt registers.

### M4 — layout, input, and plugin management

**Status: complete (2026-09-05).** Before defining manifests, the public
model was reconsidered against Apple's AppKit Touch Bar API. The resulting
architecture treats plugins as providers of stable item factories and bar
definitions, not owners of permanent left/center/right rectangles. Context
composition and user customization produce an ordered bar; a separate pure
layout resolver turns that bar into surface rectangles.

The first dependency-free `touchbar-layout` slice is implemented. It supports
stable item IDs, min/preferred/max sizing, compression and visibility
priorities, atomic visibility groups, policy-owned required items, fixed and
weighted flexible spaces, recursive natural/equal-width groups, collapsing
empty groups, and semantic principal-item centering (including a principal
inside a group). Seventeen tests cover centering, constrained compression,
deterministic overflow, grouped visibility, nested placement, equal-width
compatibility, depth/identity bounds, and unsatisfiable required content. It has no dependency on a
manifest encoding, process model, Wayland, or DRM.

The companion `touchbar-model` registry and active-chain composer are also in
place. Plugins can register labeled item factories and ordered bar definitions,
including expanded and press-and-hold bar references. A caller supplies active
bars from most-specific to least-specific; an outer bar participates only by
placing a context proxy, so focused content wins by default. The closest
visible principal item wins deterministically. Core model tests cover wrapping,
replacement behavior, principal selection, reference validation, duplicate
preservation, and composition through final layout.

The v1 protocol carries captured multitouch contacts and stable item
identities with compact and popover sizing constraints. Compact surfaces are
placed by `touchbar-layout`, replacing the fixed two-slot assignment for v3
clients. A modal popover is resolved to a bounded width, anchored to its source
item, and composed above—but does not hide—the normal bar. The compositor
accepts it only for a contact captured by that surface and preserves capture
across the geometry change. Configure geometry is promoted only after its
matching acknowledgement, so already-submitted old-size frames remain valid
while a resize is in flight. A two-process synthetic runner proves an 80-pixel
button opening a 360-pixel slider over a full-width animated backdrop while
1648 pixels remain visible, continuous slider motion, release-to-collapse, and
cancellation.
The live ADP presenter now reads the root-restricted `apple_z2` type-B
multitouch device and forwards normalized contacts over the existing private
presenter socket. Real press, hold, popover capture, slider drag, and release
were exercised on the physical Touch Bar while `touchbar-sessiond` remained
unprivileged. The presenter restored `tiny-dfr` after the guarded run.

The visual-composition foundation is now complete. The daemon and physical
backend use the native 2008 by 60 logical canvas. The v1 protocol includes one
non-interactive, bottom-stacked backdrop surface and atomic semantic appearance
snapshots. Appearance policy lives in `touchbar-sessiond`: it hot-reloads the
user's TouchBar theme file when available and otherwise supplies a built-in palette.
The client runtime delivers complete snapshots, while the optional UI kit maps
them to toolkit colors and radius. Colors remain straight-alpha in public APIs
and are premultiplied by renderers; plugin-local drawing and final composition
both use the corresponding premultiplied blend function. The M1 acceptance run
composed an animated 2008 by 60 DMA-BUF backdrop with translucent UI at 60.13
FPS with zero invalid buffers.

Context selection now has serialization-free facts, predicates, scope and
priority rules, immutable composition generations, and gesture-safe atomic
commit semantics in `touchbar-model`. Input replay tests prove that a newer
focus snapshot is deferred while any contact remains captured and commits only
after the final contact ends. Desktop focus discovery and selected-bar
visibility now run through that same transactional context engine.

The user-owned profile composer is now implemented above those primitives.
Reusable contribution definitions are bound into arbitrary named slots;
`fixed`, `collect`, and `select` policies cover always-present controls,
additive status content, and contextual replacement without imposing spatial
left/center/right roles. Multiple profiles reuse the same stable item IDs while
changing the complete slot template. A shared state controller resolves
automatic rules, explicit user overrides, and owner-scoped temporary mode
leases, and reports retained/entering/leaving items for live surface
reconciliation. Commands are rollback-safe and compositions remain deferred
during captured gestures. Twenty model tests now cover the original chain
model plus profile composition, ordering, precedence, reconciliation, invalid
transactions, multi-contact deferral, and stable paths into grouped slots.

Composition-level groups are now implemented end to end rather than inferred
from a plugin's private UI tree. Bar definitions, strict profile TOML, and
package presentation bars can recursively declare natural or equal-width
groups with stable IDs, spacing, compression priority, and atomic visibility
priority. Profile groups contain named slots or nested groups; every slot is a
collapsible addressed container, so an empty contextual slot consumes neither
width nor spacing but can be populated later. The resolver still returns only
leaf surface rectangles. Nested in-place and slot presentations edit those
stable container paths and remove duplicate surfaces across the whole tree.
Depth, node, identifier, sizing, and equal-width compatibility limits fail
closed before live presentation.

The live context-to-Wayland bridge and fresh-v1 profile adapter are complete.
A strict user-owned TOML document binds package-qualified items into fixed,
collecting, or selecting slots; the built-in four-client fixture now exercises
the same parser and model. Composition snapshots are
resolved against the native canvas and reconciled to connected surfaces by
stable `{ plugin, item }` identity; entering/leaving items receive visibility events and retained
items receive new configure dimensions only when required. Configure state now
keeps a bounded ordered queue, allowing rapid recomposition before earlier
serials are acknowledged. A replay source and optional Hyprland event-socket
adapter publish normalized application/workspace facts through the same state
controller. The packaged Hyprland source reconnects without restarting the
session compositor. A same-user v1 control interface reports profile readiness,
lists/selects profiles, restores automatic selection, and reloads the strict
configuration. A live release acceptance also proves duplicate local item names
across packages, required-item fallback, and reconnection. The Apple-GPU demo deferred a browser switch during an
active volume gesture, preserved volume and status across three focus changes,
resized volume 80→72→80→72, and completed at approximately 60 FPS with zero
invalid buffers.

- ~~Build stable item/bar registries and context-driven bar composition.~~
- ~~Persist user-owned default, allowed, required, and ordered item selections.~~
- ~~Add true nested/equal-width groups to the bar and profile model; nested UI
  rows and columns do not provide this composition-level contract.~~
- ~~Add multi-item presentation bars.~~ Package-declared multi-item content,
  all five placement policies, transient hold-slide and persistent tap-then-tap
  lifecycles, outside dismissal, and nested page navigation are covered by
  headless and Apple-GPU acceptance.
- ~~Connect resolved model rectangles to Wayland configure and visibility; the
  configure lifecycle itself now supports dynamic acknowledged geometry.~~
- ~~Connect physical Z2 multitouch to the tested surface-local capture router.~~
- ~~Add a hardware-owned global recovery gesture.~~ An eight-second physical
  Fn hold latches the trusted fallback and rejects user-session reconnects;
  release and repeat to resume. It uses no invisible bar target or additional
  keyboard data. Application and system actions remain ordinary plugin-process
  integrations rather than compositor-brokered capabilities.
- ~~Discover package/launch metadata, supervise native and Component processes,
  hot-reload configuration, expose typed availability diagnostics, and show
  host-owned placeholders for required items waiting on consent, restarting,
  or stopped in a crash loop.~~ Deterministic unit coverage proves the bounded
  restart transition and clean reload state; live profile acceptance proves
  placeholder appearance and removal on reconnection.

### M5 — SDK and ecosystem hardening

**Status: complete (2026-09-05).** The first Rust SDK vertical slice is
extracted from the hardware-proven GLES demo. The SDK owns private
Wayland discovery, role/configure lifecycle, EGL setup, DMA-BUF submission,
and frame callback pacing. Its graphics context exposes raw GLES. A separate
optional UI crate provides GPU-rendered primitives and local interaction state
without sending a widget tree to the compositor. The original animated shader
client now runs through the SDK, and a second client exercises the UI renderer.
Eight UI tests cover alpha premultiplication, capture transfer, slider
clamping, overlapping hit targets, press-and-hold timing, row layout, and
animation interpolation.

The retained UI Kit Foundation v1 is now implemented. A plugin-local `Node`
tree resolves compositor-assigned geometry into the GPU scene, interaction
map, and semantic inspector snapshot from the same bounds. It supports
full/compact/minimal responsive representations, clipped flex rows with
grow/shrink and priority hiding, layers, panels, buttons, toggles, sliders,
theme-tinted procedural icons, and revision-cached RGBA images.
`FrameScheduler` separates dirty static content, bounded animation, and
continuous animation. `touchbar-client` now stops an animated callback chain
while its surface is invisible, remembers a pending redraw, and resumes on
visibility.

The Collection and Gesture Foundation is also complete. `cosmic-text` provides
Unicode shaping, fallback, bidi layout, and ellipsis; bounded run eviction also
releases paired GLES textures. A per-contact arena composes tap, long-press,
axis pan, and swipe without removing the raw input path. The fixed-extent
horizontal scrubber lazily creates only visible retained nodes, supports free
or centered snapping and continuous or release-time selection, and rolls back
cancelled drags. The hardware-GPU demo exercises responsive expansion, Unicode
labels, virtualized composition, and captured scrubber panning through DMA-BUF
at approximately 60 FPS.

Dynamic Layout v1 replaces the UI kit's hand-written flex distribution with
Taffy while retaining semantic priority hiding and responsive variants. Rows
and columns now support nested intrinsic content, padding, gaps, grow/shrink,
required items, and cross-axis alignment. The live GLES renderer supplies
Cosmic Text measurements through a toolkit boundary; images and controls expose
intrinsic sizes. A reproducible media-style release probe resolves complete
80/160/360-pixel trees in single-digit microseconds on the development M1.

UI Foundation v2 makes complex visual controls composable without moving UI
ownership into the daemon. Any node tree can now become a themed pressable;
images support contain/cover/stretch fitting and live theme multiply or alpha-
mask tinting; progress and nested opacity provide reusable state and transition
paint; and `CustomGles` embeds a plugin callback at an exact retained-layout
leaf. The callback receives bounds, clip, opacity, surface size, and the same
dynamic `Theme` snapshot as built-in text, lines, shapes, and icons. The live
volume demo exercises the generic pressable and a theme-aware custom GLES
meter on the normal GPU/DMA-BUF path.

The Media Widget Showcase is complete as a deterministic standalone plugin.
One stable item resolves minimal, compact, and full forms at 80/160/420 pixels,
then requests a bounded 420-pixel presentation for transport and timeline
interaction. It proves nested pressable hit priority, both persistent tap and
transient hold-slide expansion, capture transfer into a seek slider, cached
revisioned artwork, progress, readable accent foreground derivation, and a
theme-aware clipped GLES waveform. Playback is
simulated by design; replacing that state adapter with direct MPRIS access does
not change the compositor or UI contracts.

UI Foundation v3 adds the asset, text, and motion substrate needed by richer
community widgets. Static SVGs are parsed and rasterized through a bounded
`resvg` cache, while semantic mask tinting remains a live GLES operation so
themes propagate without asset churn. Labels now choose ellipsis, clip, or a
draw-time marquee and can reserve a stable measurement sample for changing
numeric content. Stable-ID motion nodes apply translation, uniform scale, and
opacity at render time with full/reduced/disabled policies, leaving Taffy
layout, semantics, and hit geometry unchanged. The media showcase exercises
all three capabilities on the hardware-GPU path.

UI Foundation v4 turns that substrate into reusable plugin controls. A common
finite range maps normalized touch input into arbitrary stepped values. Styled
sliders expose themed track, fill, thumb, and tick geometry; continuous or
segmented meters add peak indication; tiny graphs perform bounded,
peak-preserving downsampling; and scrubbers gain a standard themed cell while
keeping their existing virtualized gesture model. A cached 16-symbol SVG
catalog covers the first system, media, workspace, capture, and developer
plugins. The media showcase now exercises these controls without private GLES
widget code.

- ~~A Rust native plugin SDK with a GLES surface wrapper.~~ Components use the
  language-neutral WIT ABI and do not require a second native C-specific SDK.
- ~~A lightweight 2D drawing helper plus raw GLES access.~~
- ~~Resource limits, protocol versioning, crash placeholders, frame budgeting,
  and battery-aware animation policy.~~ Raw clients share a 120 Hz commit
  budget with burst headroom; excess work is dropped before import, sustained
  flooding disconnects the client, and callback queues are bounded. Animation
  callback cadence defaults to 60 Hz on external/unknown power and 30 Hz on
  battery, with strict theme overrides. Real Wayland abuse and fake-sysfs
  Apple-GPU clients cover both paths.
- ~~Headless golden tests, input replay, example plugins, and documentation.~~

### M6 — Plugin packaging and sandboxed ecosystem

**Status: in progress (2026-09-05).** The ecosystem uses GitHub Releases for
decentralized package storage and permits installation from any GitHub source.
A small pull-request-driven catalog supplies discovery, aliases, and explicit
listed/verified/curated/first-party signals without becoming a binary registry.
Stable installs consume release artifacts rather than repository branches and
are locked to their verified digest.

Native process plugins remain the trusted raw-Wayland/GLES and ordinary-session
escape hatch. Community distribution defaults to language-neutral WebAssembly
Components hosted by a separate, resource-bounded `touchbar-plugin-host` that
renders through the native GPU UI kit. Rust is the first SDK. Generic scoped
capabilities, explicit consent, and effective-access labels preserve useful
system integration without pretending broad command, filesystem, or D-Bus
grants are low risk.

The complete design and engineering sequence live in
[`docs/design/plugin-ecosystem.md`](docs/design/plugin-ecosystem.md). E1 defines
a shared strict manifest and validator for native and component packages;
release provenance and catalog review remain installer-owned metadata. E1–E4
are complete: the reviewed catalog is bundled into the core build, supports
deterministic search and fresh-install aliases, permanently binds every
accepted alias to one GitHub identity, validates base-to-head retirement
transitions, and runs release, package, attestation, and component conformance
checks in CI. Publishers receive a standalone theme-aware scaffold plus release
and submission automation; catalog inclusion is never required for a direct
GitHub install.

E3 capability work is governed by
[`docs/design/capability-consent.md`](docs/design/capability-consent.md). It
defines the threat model, separates package requests from user grants and
effective authority, keeps brokers outside both the compositor and renderer
host, specifies asynchronous phase-restricted broker operations, and makes the
adversarial acceptance matrix part of the milestone exit criteria.

E3.1 is complete. `touchbar-policy` implements all fourteen v1 typed scope
families, canonical manifest normalization and validation, semantic update
subsets, provenance-bound persistent and session grants, effective policy and
risk summaries, machine-readable permission diffs, and a private atomic grant
store. The next slice is the immutable-identity private supervisor transport;
it deliberately adds no live broker authority yet.

E3.2 now has its first transport slice. `touchbar-plugin-supervisor` verifies
the installer-owned source, version, and artifact digest, creates an inherited
close-on-exec sequenced-packet channel, and owns the immutable connection
policy. The strict bounded binary protocol carries status and generic
operations but has no host-authored identity or grant fields. Routing currently
has zero OS backends and remains default-deny while lifecycle, audit, and WIT
event plumbing are completed. Peer EOF, hangup, reset, and broken-pipe signals
now converge on one normal host-disconnect state while the supervisor preserves
the actual child exit status; other transport errors remain fatal.

The next E3.2 slice adds backend-neutral lifecycle primitives: pending-operation
and resource quotas, capability-selective revocation, deadlines, single-use
trusted activation, bounded/coalesced events with overflow markers, structurally
redacted audit records, and deterministic circuit-breaker backoff. What remains
is connecting those primitives to asynchronous workers, live restart, durable
audit storage, and the versioned component WIT surface.

The bounded asynchronous worker runtime is now complete. Jobs inherit the
supervisor connection identity, queue and total-active limits provide
backpressure, deadlines and revocation cancel cooperatively, panics are
contained, and completion releases lifecycle accounting before entering the
bounded event queue.

The runtime is now connected to the executable supervisor. A blocking `poll`
loop watches both the inherited sequenced-packet socket and a worker `eventfd`,
so completions are delivered without timer polling or requiring another host
packet. Granted work carries immutable package identity plus normalized scope;
denied work never reaches a backend, cancellation and optional revocation reach
in-flight work, and completion outcomes enter the redacted audit log. Production
still registers no OS backend. Permission lifecycle, durable audit files, and
WIT callbacks remain.

Live grant watching and required-capability restart are now complete. The
watcher follows directory events so atomic store replacement is detected,
reloads through the same ownership/mode/symlink validation as startup, and
treats invalid replacements as no grants. Optional changes update the existing
connection. Required revocation shuts down and kills the current host; the
supervisor waits without executing it until policy is valid, then launches a
new instance. Durable audit-file rotation and the WIT broker bridge are the
final E3.2 layers.

Durable audit-file rotation is now complete. The optional supervisor
`--audit FILE` sink writes synced JSON-lines records under a cross-process lock,
reopens the active inode for every append, rotates on size or age, bounds archive
count, rejects unsafe paths/modes/ownership/links, and assigns one monotonic
sequence across rotation, concurrent supervisors, and restarted host instances.
The record type remains structurally payload-free.

The WIT broker bridge is now complete. The single
`plugin@1.0.0` world requires a bounded host-event callback. Typed imports
provide capability snapshots, asynchronous request IDs, cancellation, resource
close, and stable results. The broker
endpoint and actual callback phase live only in trusted host state, so pure
`items`/`render` calls cannot initiate work. The native client waits on Wayland
and broker readiness in one event loop, and the new `touchbar-component-sdk`
plus broker demo provide the Rust authoring path.

E3.2 is now complete. Every v1 touch event carries a compositor-issued 64-bit
sequence for its captured gesture. Only the
resulting trusted `Activated` callback receives a private two-second activation
context; synthetic, render, initialization, and host-event paths receive none.
The supervisor checks its phase, bounds, origin, and absolute monotonic deadline
before a backend can observe it.

The first E3.3 slice is complete. A shared bounded binary schema represents
structured D-Bus calls and typed replies without exposing raw messages. The
production supervisor registers one `dbus.call.v1` backend backed by zbus. It
matches every call field and constrained string argument against the normalized
grant inside the only queue-submission API. By default it resolves the approved
well-known name and sends to that unique owner, preventing both activation and
owner-handoff redirection. Calls have a two-second transport timeout and a
one-message receive queue; raw replies over 64 KiB, Unix FDs, and unexpected
signatures are rejected before application decoding. It consumes one fresh
physical activation when the matched rule requires it. No
supervisor caller can enqueue work while bypassing backend preflight. The Rust SDK
provides the same encoder/decoder, and the reference component reads MPRIS
`PlaybackStatus` and invokes activation-gated `PlayPause`. Fake-transport tests
prove out-of-scope destinations and arguments never reach I/O and activation
cannot be replayed.

The second E3.3 slice is complete. `dbus.subscribe.v1` opens only exact,
grant-matched signal rules, pins the current unique service owner, and returns an
opaque connection-scoped resource ID.
The first typed decoder covers `PropertiesChanged` (`sa{sv}as`) and forwards
bounded scalar property values while rejecting unexpected signatures and
interface arguments. Resource events carry a per-resource sequence through the
single host callback; ingress, payload size, rate, resource count, and buffered
memory are bounded with explicit overflow reporting. Guest close, transport
failure, capability loss, and even a still-granted scope narrowing close the
live zbus match and release its lifecycle reservation. A one-message transport
queue plus pre-decode body, header, signature, FD, sender, and current-owner
checks bound and authenticate ingress. The reference component
now combines an initial MPRIS read with ongoing playback-state events. A real
zbus integration test runs both operations against an isolated private daemon
and fake MPRIS service. The release gate repeats deterministic one-message
ingress overflow and well-known-name owner loss for 32 private-bus subscriptions
each, then fills all 16 lifecycle slots and proves atomic overflow, revocation,
64 close/reopen cycles, monotonic IDs, and runtime-drop cleanup. A physical
runner now connects that deterministic
service to the sandboxed component, production supervisor, compositor, and ADP
presenter. The physical Apple M1 pass received 43 touch events across both
widgets, produced 17 changing broker-driven scenes over 20 seconds through
DMA-BUF, reported zero invalid frames, and restored `tiny-dfr`. E3.3 is
complete.

The first E3.4 filesystem slice is implemented. A package requests only logical
mount labels; an installer/user-owned grant binds each label to one canonical
absolute directory and its device/inode identity, and that mapping never appears
in the package manifest. Every later use reopens the symlink-free root and
requires the recorded identity, so replacing or redirecting the selected path
revokes authority rather than retargeting it.
The production supervisor now registers `filesystem.read.v1` with typed,
bounded `list-directory` and chunked `read-file` operations. Linux `openat2`
resolves every target beneath an `O_PATH` root with no symlinks, magic links,
mount crossings, or parent traversal; file-kind and whole-file size limits are
checked before reads, directory scanning is capped, and only regular files and
directories are disclosed. Mount rebinding counts as authority revocation.
The Rust SDK exposes the same schemas and helpers, and a reference sandboxed
component plus physical runner demonstrates listing a selected gallery and
streaming its first regular file. Inline previews and large reads share the
same pinned-descriptor checks; multi-link files are rejected to close hard-link
confusion. Stream metadata, 12 KiB chunks, byte totals, explicit termination,
backpressure, close, and revocation use the common broker resource lifecycle.
The release gate now adds 32 rounds each of final-entry, intermediate-parent,
and grant-root replacement after a stream opens; every byte continues to come
from the pinned inode and outside sentinels remain untouched. Its pressure
campaign fills all 16 streams and the complete 4 MiB buffered lifecycle budget,
proves atomic overflow and revocation, performs 64 immediate open/close cycles,
checks producer-thread exit under backpressure, and then reuses the same runtime
for a complete ordered stream.

The bounded inline HTTP slice is also implemented. `http.request.v1` accepts a
canonical typed method, URL, optional `Accept`/`Content-Type`, and body; the
schema cannot represent cookies, authorization, proxy credentials, or
hop-by-hop headers. Every hop rechecks method, normalized origin, path prefix,
and resolved addresses. DNS answers are validated as a set and pinned into a
fresh no-pool client, closing mixed-answer and rebinding paths. Redirects are
manual and bounded; proxies, referers, retries, cookies, automatic
decompression, and ambient client identity are disabled. Public/private IPv4,
IPv6, mapped IPv4, NAT64, and 6to4 cases have adversarial coverage, as do
userinfo, encoded path separators, header injection, redirect escape/loops,
rate limits, oversized/compressed responses, and redirect-amplified uploads.
Large responses now use broker-owned resources: an opening completion is
followed by typed metadata, bounded 12 KiB chunks, and an explicit terminal
event. Lossless stream producers use cancellation-aware bounded backpressure;
close and revocation release them without blocking the supervisor loop. The
production transport reads incrementally, enforces the aggregate response cap,
and never decompresses. The SDK and reference Wasm component exercise the same
streaming schema. DNS now runs through a fixed two-worker/eight-job resolver
pool with a two-second result deadline, so a stuck libc lookup cannot leak one
thread per request or occupy the broker indefinitely. Together with the
filesystem stream, this completes the E3.4 data-plane implementation.

The bounded inline `filesystem.write.v1` slice is implemented. The package
still names only a logical mount; the user-owned grant supplies the host root.
Create, replace, append, delete, no-overwrite rename, and create-directory are
independent operations. All paths are normalized relative components and all
parents/existing files are resolved beneath pinned directory descriptors with
no symlinks, magic links, mount crossing, or multi-link regular files. Create
stages complete content before a no-overwrite commit; replace and append write
and sync private files before atomic exchange; delete quarantines and
inode-checks before unlink; and topology mutations sync their parent
directories. One plugin's mutations are serialized and request paths cannot
enter the unpredictable broker-temporary namespace. Per-envelope and file-size
caps plus a source-bound, cross-process-locked rolling-hour ledger are checked
before I/O. The private durable ledger survives supervisor restarts and fails
closed on malformed state. The SDK carries only typed bounded payloads.
Adversarial tests cover scope confusion, absolute/parent paths, wrong mounts,
symlinks, hard links, FIFOs, overwrite attempts, append growth, cancellation,
quota exhaustion/restart/corruption, temporary-name forgery and cleanup,
concurrent appends, and parent-directory replacement. Large create, replace,
and append requests now use broker-owned staging resources with exact-offset
12 KiB chunks, one-at-a-time acknowledgements, current-grant/inode rechecks,
explicit complete-size commits, and close/revocation cleanup. The release gate
adds deterministic production-backend campaigns: 32 rounds each replace an
opened stream's parent path, grant-root path, target inode, and append snapshot;
the pressure campaign fills all 16 resource slots, proves atomic overflow,
batched chunk/commit handling, partial revocation, monotonic slot reuse, 64
cancel/close cycles, and supervisor-drop cleanup without a staging-file leak.
Deterministic replay now covers every inline and streaming mutation shape using
the production decoders and authorization functions, exact offsets, rolling
quota, commit/abort lifecycle, and typed mutation receipts without opening a
host path. The filesystem reference component includes an executable
create-file scenario.

The first safe desktop-portal adapters are implemented. `uri.open.v1` accepts
one bounded typed URI, consumes a fresh physical activation, rechecks exact
scheme plus optional normalized origin/path scope, and supports only HTTP(S)
and `mailto`; credentials, encoded traversal, local files, and executable or
custom schemes fail before the portal call. Each open uses a dedicated bus
connection, activates and pins the portal's unique owner, supplies an
unpredictable request token, subscribes before `OpenURI`, requires the exact
returned handle, and treats the bounded `Request.Response` status—not the
method return—as completion. User cancellation is distinct from backend
failure; local cancellation and deadlines send `Request.Close`, close the
connection, and join the response worker. A private portal integration covers
immediate responses, all status codes, malformed responses, mismatched handles,
cancel/timeout teardown, and 32 repeated lifecycle rounds. `notification.send.v1` accepts
bounded plain text with exact category/urgency, uses immutable source labeling
and source-namespaced IDs, hides content on the lock screen, rejects control
and bidirectional spoofing characters, and enforces a durable rolling rate
across supervisor restarts. Rich notification actions/icons/sounds and portal
notification responses remain later bounded extensions.

Grant authority is now represented by one typed v1 `GrantBindings` object
rather than a filesystem-specific side map. Allowed grants must bind exactly
their approved filesystem labels, logical Secret Service items, local endpoint
labels, or desktop clipboard socket and cannot carry cross-capability
authority; denied grants must bind nothing. Effective-policy replacement compares the whole object, so a
target change revokes live resources. There are deliberately no compatibility
fields or migration paths for the earlier development-only shape.

`local.connect.v1` now provides a constrained framed Unix-stream resource. The
manifest endpoint is only a logical label/protocol; the user-owned grant
supplies the exact pathname. `openat2` pins a symlink-free socket inode before
connect, ownership/link/type and same-user peer credentials are checked, and a
path replacement cannot redirect the connection. The API has no raw socket or
descriptor-passing surface: it accepts and emits nonempty four-byte-length
prefixed frames capped at 12 KiB. Sends recheck instance, scope, protocol,
binding, and limits; inbound and outbound traffic share a durable source-bound
rolling-minute quota. Adversarial tests cover hints-as-authority, parent
symlinks, hardlinks, regular files, path replacement, cross-instance/binding
confusion, oversized peer lengths, resource teardown, quota restart, and
source isolation.

`secret.read.v1` now resolves a plugin-local logical name only through an exact
user-owned Secret Service item binding. Each read consumes a fresh physical
activation. The adapter never searches, enumerates, unlocks, or prompts; it
opens a short-lived plain session, rejects locked items and mismatched session
metadata, caps the complete inline value at 48 KiB, zeroes its transport copy,
and closes the session on every path. Secret Service traffic runs in the
separate `touchbar-secret-helper`, not in the supervisor. The helper has a 128
MiB address-space limit, two-second method limits, one-message queues, raw
reply validation before typed decoding, unique-owner pinning, and a bounded
private pipe. Landlock permits only the pinned same-user session-bus socket;
seccomp permits AF_UNIX and runtime threads while denying ambient files,
network, process creation, cross-domain signals, and kernel-administration
surfaces. Cancellation kills and reaps the helper. Both processes disable
dumpability and core files before processing broker data. Fake-transport tests prove
out-of-scope requests never cross the boundary, and a private D-Bus service
exercises the real OpenSession/Locked/GetSecret/Close signatures. Hostile
integration tests cover oversized and wrong-signature replies, observed kernel
limits, cancellation, confinement, and repeated process/thread reclamation. The XDG
Secret portal was deliberately not used because it returns an application
master key rather than a user-selected logical secret.

`context.read.v1` now has exact-key snapshots and live subscriptions. The
schema caps each request at 32 keys and each text value at 256 bytes; the
resource uses the approved update rate and atomically registers against its
initial snapshot. Production deliberately implements only the public
`application.id` and `workspace.id` facts. It connects to same-user pinned
Hyprland sockets, bounds command JSON and event lines before parsing, coalesces
unchanged state, normalizes application IDs, and discards titles entirely.
Unknown or future sensitive fact providers fail as unsupported rather than
returning partial or guessed data. Tests cover exact filtering, initial/update
ordering, unsupported and out-of-scope keys, title/control/oversize rejection,
and the real bounded Unix-socket parsing path.

The clipboard slice is implemented without an ambient desktop escape. Both
clipboard capabilities use an exact grant-owned compositor socket, fresh
physical activation, exact MIME matching, a 48 KiB inline ceiling, and a
restart-resistant source rate limit. The production transport targets
`ext-data-control-v1`, ignores primary selection, pins and verifies the
same-user socket, bounds every protocol and pipe wait, and keeps write
ownership in a bounded broker thread. A private Wayland compositor integration
test covers the actual read and write descriptor paths. Generic input
synthesis has no v1 schema or registry entry; a future fixed named-action
design must be implemented and reviewed as new authority rather than appearing
as a dormant permission.

Supervisor launch now closes the verified-artifact TOCTOU gap. Manifest and
component members are opened beneath the canonical package directory with
`openat2`, copied while hashing into separate memfds, sealed against write,
growth, shrink, and further seal changes, and passed in fixed descriptor slots.
The host requires those sealed descriptors whenever the private broker is
present and instantiates from the verified bytes rather than reopening the
component pathname. A launch integration test replaces the package artifact
after host exec and proves the inherited bytes retain the approved digest and
cannot be written. The supervisor also sets `no_new_privs` before exec.

The first E3.5 confinement slice is implemented. Supervised launch now clears
ambient environment values, closes every descriptor above the broker and two
sealed artifact slots, disables Mesa's on-disk shader cache, and the host
disables dumpability and core files before guest execution. Mandatory Landlock
denies ambient filesystem writes and TCP while allowing read-only runtime
libraries/fonts/GPU discovery, required GPU device access, and only the exact
live Wayland socket. Architecture-checked seccomp denies non-Unix sockets,
execution, tracing, cross-process memory, mounts/namespaces, keyrings, modules,
BPF/perf/userfaultfd, and handle-based filesystem bypasses. Adversarial child
probes cover these denials. Headless mode denies all new sockets and task
creation. Live mode permits only Mesa-compatible thread clones, never child
processes, and Landlock ABI 9 confines its AF_UNIX connection to the exact
Wayland socket. Every host enters a private cgroup leaf with recursive kill and
parent-death handling; task, address-space, descriptor, and available cgroup
controller limits are applied before exec. A supervised headless component and
the confined Apple M1 Wayland/EGL/DMA-BUF path both pass end to end.
If a supervisor is itself killed before normal teardown, the next launch
removes only an empty same-user leaf whose encoded owner PID is dead; parallel
launch/disappearance races are explicitly tolerated and tested.

The constrained `command.run.v1` E3.5 slice is now implemented. A command is a
broker-owned streaming resource selected by one exact, consented rule. Dynamic
slots are type checked as bounded integers, fixed enums, bounded text,
approved files, or scheme-limited URLs and are assembled directly into
`argv`; there is no shell interpretation or `PATH` search. Executables and
approved files are opened and descriptor-pinned before launch, package-owned
executables and ambiguous multi-link files are rejected, stdin is `/dev/null`,
and only fixed approved environment entries are present. Stdout and stderr use
bounded 12 KiB events with one shared byte cap and explicit exit status.
Deadlines, close, revocation, process-group termination, and per-connection
parallel quotas are enforced. Every command is now moved before exec into a
mandatory private cgroup-v2 leaf and opened through a pidfd. A command-specific
Landlock layer denies all paths to the cgroup control mount, a narrow seccomp
filter denies namespace/mount/handle/tracing and kernel-administration escape
syscalls, and RLIMITs bound address space, descriptors, CPU time, core dumps,
and additional user tasks. Available cgroup pids/memory controllers are also
configured. Tests cover metacharacters, type/shape confusion, path traversal
and replacement races, empty ambient environment, output floods, timeouts,
parallel quotas, a `setsid` process-group escape, attempted migration into the
parent cgroup, and blocked kernel escape syscalls. The final resource-pressure
matrix also verifies atomic resource-cap failure/reuse, host crash cleanup, and
cross-plugin identity separation.

The E3.5 sandbox acceptance gate is complete on the Apple M1 target as of
2026-09-04. Three sanitizer-backed libFuzzer targets exercise every typed
broker schema, both sequenced-packet directions, and manifest/grant/effective
policy parsing. The latest bounded campaigns ran 1,399,314, 530,130, and
318,248 inputs respectively with no crash, hang, or artifact. The stable test
suite retains deterministic mutation corpora, while the reusable fuzz package
and `scripts/test-sandbox-security.sh` provide the release gate. RustSec found
no known vulnerabilities in either lockfile; the one visible unmaintained font
metadata parser warning and its constrained exposure are recorded in
`docs/security/dependency-policy.md`. Generic input synthesis has no v1 schema
or registry entry rather than shipping as a partially trusted stub.

E1 and the first E2 vertical slice are now complete. The versioned WIT world
passes full semantic appearance snapshots, viewport constraints, a bounded flat
retained node arena, and input events. A Wasmtime host runs component packs with
deterministic fuel, memory/object limits, and hostile view validation before
converting output to `touchbar-ui`. A default-deny WASI context makes ordinary
Rust `std` usable without inheriting arguments, environment, filesystem paths,
network access, or session services. The stateful Rust demo exports two
manifest-matched items, responds to activation, and resolves theme-aware
responsive content through the normal UI scene and semantics path.

The E2 host is also connected to the production client path: a selected
component item receives compositor-assigned geometry and live appearance,
resolves through the native UI kit, renders into a DMA-BUF-backed GLES surface,
and receives semantic touch events through WIT. The headless runner remains the
fast deterministic test path, while a guarded physical runner proves the same
component on hardware.

## Immediate implementation sequence

1. ~~Implement the M1 server loop and one-surface state machine.~~
2. ~~Implement the external surfaceless-EGL demo client.~~
3. ~~Verify the client is using the Apple M1 renderer rather than llvmpipe.~~
4. ~~Add a deterministic 10-second integration runner and metrics assertion.~~
5. ~~Replace the M1 shared-memory bridge with Linux DMA-BUF in M2.~~
6. ~~Add retained per-plugin GPU layers and compose two clients.~~
7. ~~Add asynchronous GPU release fences and late-client tests.~~
8. ~~Add Wayland acquire fences with the future manual DMA-BUF SDK allocator.~~
9. ~~Prove guarded ADP discovery, static scanout, rotation, and recovery.~~
10. ~~Characterize atomic flips and physical vblank timing.~~
11. ~~Feed the composed M2 scene into the ADP scanout backend.~~
12. ~~Investigate an ADP kernel fix for the 29.90 Hz vblank cadence.~~ The
    panel advertises a computed 60 Hz fixed mode, but ADP does not program its
    timing; firmware owns the active pipe state. The requested front-end IRQ
    supplies DRM vblank at the measured half rate, while the discovered backend
    IRQ is unused and its status/acknowledgement semantics remain undocumented.
    `scripts/audit-adp-cadence-source.sh` makes those source assumptions
    reproducible. A guessed IRQ or synthetic-release patch would risk an
    interrupt storm or early scanout-buffer reuse, so the next kernel step is
    explicitly optical refresh measurement plus m1n1/macOS setup tracing, not
    an unsafe patch in this repository.
13. ~~Export an ADP buffer with PRIME, import it into AGX, and prove GPU writes
    are visible through ADP.~~
14. ~~Pass an ADP swapchain to the unprivileged compositor with `SCM_RIGHTS`,
    render the rotated final scene into it, and flip completed buffers.~~
15. ~~Study AppKit's Touch Bar object model and implement a serialization-free
    ordered-bar layout resolver.~~
16. ~~Add item/bar registries and an active-chain composer before choosing a
    manifest serialization format.~~
17. ~~Extract a Rust Wayland/EGL client runtime and prove the raw-GLES demo on
    top of it.~~
18. ~~Add the first plugin-local GPU UI and interaction slice, including
    press-and-hold and captured slider behavior.~~
19. ~~Carry synthetic multitouch through the v1 protocol and prove acknowledged
    compact-to-expanded-to-compact presentation across live GPU processes.~~
20. ~~Replace binary full-width expansion with v1 item sizing,
    resolver-driven compact geometry, and an anchored bounded popover.~~
21. ~~Connect physical `apple_z2` multitouch through the privileged presenter
    to the unprivileged surface-local capture router.~~
22. ~~Add the generic context-fact selector and gesture-safe transactional
    composition snapshots.~~
23. ~~Add native-width backdrops, premultiplied alpha, and daemon-owned
    semantic appearance snapshots with host theme hot reload.~~
24. ~~Add reusable contributions, user-owned profile slots, dynamic profile
    selection, reconciliation metadata, and a shared transactional state
    controller.~~
25. ~~Add replay and Hyprland focus sources, then wire profile composition
    snapshots to live Wayland visibility and dynamic configuration.~~
26. ~~Shift to the UI kit: establish retained responsive drawing/layout,
    semantic inspection, core GPU resources, scheduling, and a production-style
    volume control on the live composition runtime.~~
27. ~~Replace the bootstrap font with cached production shaping, add a
    composable gesture arena, and implement the virtualized horizontal
    scrubber.~~
28. ~~Generalize presentations to anchored, in-place, slot, region, and
    full-bar policies with nested navigation. Identified transient and
    persistent sessions, all five daemon-owned placement contracts, compact
    restoration, modal input, named-target validation, explicit dismissal
    reasons, stale-message protection, and plugin-local nested navigation are
    covered by live GPU acceptance.~~
29. ~~Replace the UI kit's custom flex algorithm with Taffy-backed dynamic rows
    and columns, measured text/images, nested content, and responsive identity
    preservation.~~
30. ~~Add UI Foundation v2 composable pressables, fitted/tintable images,
    progress, opacity groups, and retained-layout custom GLES callbacks.~~
31. ~~Build the first complex media-widget showcase on Dynamic Layout v1/v2.~~
32. ~~Add UI Foundation v3 static SVG assets, explicit text overflow and stable
    measurements, plus policy-aware render-time motion.~~
33. ~~Add UI Foundation v4 ranged controls, styled sliders, meters, tiny graphs,
    themed scrubber cells, and a cached built-in symbol catalog.~~
34. ~~Expose installed plugin state through a versioned same-user control socket
    and `touchbarctl`, with persistent content-addressed configuration.~~
35. ~~Define the shared plugin package manifest and strict validator for both
    native and WebAssembly Component runtimes.~~
36. ~~Build the resource-bounded component-host vertical slice and drive the GPU
    UI kit through a versioned WIT world.~~
37. ~~Add capability consent and brokers, beginning with the pure policy core,
    private supervisor channel, and an exact-scope MPRIS/D-Bus proof; then add
    GitHub release installation, digest locking, rollback, and the
    pull-request-driven discovery catalog.~~ The capability and sandbox layers
    plus GitHub delivery are complete: stable installs resolve
    one fixed release asset, verify GitHub's streamed SHA-256 and package
    identity, record immutable/mutable provenance, disable authority-expanding
    updates, retain prior content-addressed versions, and roll back offline.
    The standalone scaffold now generates CI and semantic-release workflows,
    the fixed package asset, and GitHub provenance attestations; installation
    verifies any advertised attestation against the exact repository and tag.
    Publication is also native Rust: `touchbarctl plugin publish` verifies the
    existing tag, creates a draft, uploads only the staged fixed-name asset to
    the exact repository endpoint, checks its returned size and digest, and
    publishes only after success, deleting incomplete drafts on failure. No
    generated or runtime path requires the `gh` executable. Anonymous installs
    remain credential-free; an optional nonempty `GITHUB_TOKEN` raises the API
    allowance, while bounded 403/429 decoding reports GitHub's request and
    retry/reset metadata without an unbounded automatic sleep.
    The strict catalog now provides bundled local search, fresh-install aliases,
    permanent alias-to-source identity and retired tombstones, base-transition
    CI, isolated online release/conformance checks, and PR-ready submission UX.
38. ~~Add the fresh v1 component UI controls and initial Controls, Media,
    Hyprland, Capture, and Command Deck pack vertical slices.~~
39. ~~Split the runtime at the login boundary: rename the user compositor to
    `touchbar-sessiond`, promote `touchbard` to the system hardware service,
    authenticate the active seat over a fresh directional v1 protocol, and
    restrict key injection to the fixed system-key enum.~~ The clean rename,
    protocol, active-user authentication, restricted uinput path, and explicit
    `touchbarctl session`/`touchbarctl hardware` control namespaces are
    complete.
40. ~~Replace tiny-dfr safely: keep a hardware-owned media/Fn fallback active
    without a user session, restore it after disconnect, add backlight and
    suspend/resume lifecycle policy, and ship systemd/udev installation plus a
    rollback path.~~ The media/Fn state machine, shared renderer, pre-session
    fallback, backlight bootstrap, hotplug rules, inert development installer,
    explicit activation marker, and transactional tiny-dfr rollback are
    complete. `scripts/test-packaging.sh` validates both staged service units,
    the udev rules, release artifacts, and their exact installer mapping without
    changing `/usr` or service state. Production handoff now retains one
    hardware ownership context and a dedicated fallback scanout across every
    session transition. Physical
    input hangups now fail the hardware process cleanly instead of spinning,
    and the user compositor discards a retired swapchain and reconnects without
    restarting its profiles or plugins when the hardware service returns. The
    guarded 2026-09-05 M1 acceptance run proved the complete physical
    fallback → authenticated Apple-GPU session → fallback sequence, clean
    signal shutdown, and automatic restoration of active tiny-dfr. The
    strengthened acceptance also proved a complete hardware-daemon restart,
    automatic user-compositor reconnection to a fresh swapchain, and live v1
    status with `hardware_connected: true`. A 2026-09-06 installed-service run
    then proved a real ten-second s2idle suspend/resume with both service
    processes retained and the authenticated hardware channel still connected.
    The same installed build held full brightness through 30 seconds, dimmed to
    level 1 by 35 seconds, and switched fully off at 60 seconds. A physical
    dark-panel tap then restored full brightness without activating a control or
    changing audio state. The
    status gate requires the session daemon's mandatory v1 runtime report to
    show an authenticated hardware connection, rather than inferring readiness
    from process state or socket existence. The user unit follows the desktop's
    `graphical-session.target` across logout/login, and rollback now refuses to
    start tiny-dfr unless `touchbard` is proven stopped—even if the replacement
    binary or unit has been damaged. The user compositor's
    empty-profile/Fn scene also yields to real DMA-BUF plugin
    content and returns after client teardown; the Apple-GPU integration test
    covers the disconnect path. Installed activation additionally proved the
    additive late-sorting udev aliases and targeted existing-device retrigger.
    The user compositor now selects surfaceless EGL so `PrivateTmp=true` does
    not hide an accidentally selected X11 socket; a transient-unit regression
    probe exercises the complete packaged hardening policy. First-frame
    handoff wakes an idle panel, while subsequent animation frames do not keep
    the OLED awake. The hardware fallback now draws, modesets, and then
    explicitly flushes its first dumb-buffer frame, preventing the first Fn
    redraw from being the event that makes the pre-session media row visible.
    A fully dark panel now consumes every contact in the waking input batch
    through release. The fallback's spacing, 15%/85% corner centers, 8-pixel
    radii, near-full-height antialiased pills, separate 10%/90% hit band,
    full-height content centering, gray levels, 48-pixel icons, and bold
    32-pixel function labels match Tiny DFR's platform styling.
41. ~~Add user-facing permission review, grant, denial, and revocation commands
    that write the already-hardened typed policy store, clearly separate
    persistent and session-only decisions, resolve host-owned bindings without
    accepting package-selected paths or sockets, and make first-party Controls
    and Media actions safely usable without hand-authoring grant files.~~ The
    CLI now exposes normalized text/JSON inspection and explicit
    `allow|deny|reset (--session|--persistent)` mutations. Persistent and
    runtime stores use locked atomic updates; sessiond clears runtime authority
    before launching plugins; supervisors watch both directories, apply session
    precedence live, and revoke/restart affected components. Filesystem, local
    socket, secret, and clipboard bindings are created only from explicit,
    canonical, same-user objects. Local and session authority is exact-digest,
    verified immutable releases may opt into safe same-source subset reuse, and
    native processes remain disclosure-only.
42. ~~Add a bounded, responsive, theme-aware Canvas2D node for sandboxed
    Component plugins.~~ The fresh v1 WIT and Rust SDK expose clipped view-box
    rectangles, circles, round-capped lines and polylines, and shaped text with
    semantic or literal alpha paint. The native host validates command, point,
    geometry, color, and shared text budgets before retained layout or GLES;
    the first-party Media pack exercises the path through Wasm on the Apple M1
    renderer and DMA-BUF output at four conformance widths.
43. ~~Add host-timed bounded animation descriptions so static sandboxed scenes
    can animate at compositor-approved cadence without executing guest code on
    every frame, while preserving reduced-motion, idle, damage, and resource
    policy.~~ Stable IDs retain native phase across rerenders; bounded
    one-shot, loop, and alternate transforms schedule only visible active
    frames. Motion policy now travels in the atomic appearance snapshot, and
    the Media acceptance proves 60 Apple-GPU DMA-BUF frames from one Wasm
    render call.
44. ~~Extend the standard sandboxed visual path with bounded gradients and
    vector paths before introducing a separately validated shader-effect
    tier.~~ Canvas2D now supports theme-live two-stop linear-gradient rounded
    rectangles and stroked move/line/quadratic/cubic/close paths. Trusted host
    flattening has fixed subdivisions plus raw-segment and expanded-point
    quotas; the Media waveform exercises both through Wasm and 60 Apple-GPU
    DMA-BUF frames.
45. ~~Add sandboxed package images and symbolic SVG assets through sealed
    host-owned descriptors, strict decoded-size/type limits, theme tinting,
    and bounded GPU caches; never give the component ambient package paths.~~
    Fresh v1 manifests now declare typed, dimensioned assets by stable logical
    ID. The installer locks every asset digest; sessiond forwards only those
    installer-owned values; the supervisor verifies `openat2`-pinned files and
    transfers a versioned, sealed FD 6 bundle. The confined host strictly
    decodes bounded PNG or self-contained symbolic SVG content and maps WIT
    image nodes to cached GLES textures with live semantic mask/multiply tint.
    The TouchBar wordmark proves the complete Wasm → sealed asset →
    Apple-M1 DMA-BUF path, with adversarial digest, path-replacement, seal,
    dimension, active-SVG, unknown-ID, truncation, and trailing-byte coverage.
46. ~~Add theme-aware filled Bézier paths without accepting guest-provided
    tessellation or GPU indices.~~ The host now accepts one closed simple
    polygon, applies the existing fixed quadratic/cubic flattening, rejects
    holes, crossings, repeated vertices, open contours, and degenerate area,
    and ear-clips within per-path and per-view triangle budgets. The retained
    scene scales and clips semantic paint live, and the GLES renderer uploads
    one transient vertex mesh per path. Media's animated position cursor proves
    the fresh WIT/SDK command through Wasm and 60 Apple-M1 DMA-BUF frames.
47. ~~Add a separately validated procedural GPU-effect tier for sandboxed
    components without exposing raw GLES or arbitrary shader modules.~~ A
    dynamically sized retained node now accepts a 4 KiB straight-line WGSL
    body with fixed UV, size, host-time, two parameter vectors, and all live
    semantic theme colors. Naga performs full validation and GLES 3.0
    translation after a second IR allowlist rejects guest functions, branches,
    loops, mutation, resources, atomics, and dynamic indexing. Independent
    node/program/type/expression/statement quotas bound compilation and draw
    work; host-owned phase survives rerenders, while reduced motion freezes
    time and settles after one DMA-BUF frame. Media's theme wave proves 60
    Apple-M1 frames from one Wasm render and one cached driver program.
48. ~~Add physical performance and power acceptance.~~ The first slice replaces
    `touchbar-sessiond`'s fixed one-millisecond poll with one event/deadline-
    driven `ppoll` loop. Wayland, control, hardware, and context descriptors
    wake immediately; only frame, fence, theme, child, reconnect, and demo
    deadlines remain. A release Apple-M1 static-scene probe dropped from about
    1,800 to eight voluntary event-loop switches over two seconds, used zero
    CPU ticks, and rendered exactly once. The animated effect path still
    produces 60 DMA-BUF frames and reduced motion still produces one.
    Theme-configurable power pacing now reads bounded kernel power-supply
    attributes, uses 60 Hz external/unknown and 30 Hz battery defaults, reports
    the effective state over the control protocol, and is proven on the Apple
    GPU with a fake sysfs tree. Raw native GLES clients can now submit once and
    return `FrameFlow::Wait`; live Apple-M1 acceptance proves the client remains
    connected for two seconds with exactly one DMA-BUF scene, zero CPU ticks,
    and zero voluntary context switches. A guarded ABBA runner compares the
    identical full-width shader frozen versus compositor-paced while enforcing
    battery discharge, constant panel brightness, and sub-dim-time samples. The
    installed suspend/resume gate passed on 2026-09-06. Controlled
    battery-energy measurement was explicitly waived for this development
    machine because its failing battery cannot provide a valid unplugged sample;
    the guarded runner remains available for a suitable target.
49. ~~Make package-declared multi-item presentations executable from sandboxed
    components.~~ Fresh v1 manifests now give each bar bounded container
    sizing, each item element independent min/preferred/max sizing, explicit
    selection-dismissal policy, and package-local tap/hold references. The
    verified installed catalog feeds `touchbar-sessiond`; anchored and region
    overlays preserve unrelated compact content, while in-place, slot, and
    full-bar presentations splice the same content through the ordinary
    resolver. Modal input spans every presented surface and transient capture
    transfers with cancel/down semantics for hold-slide selection. The v1 WIT
    SDK carries physical-contact-bound presentation commands plus lifecycle
    callbacks. The real sandboxed Controls pack proves 360-pixel tap and hold
    expansion, cross-process selection, compact restoration, and hot-reload
    `SourceHidden` dismissal on the Apple GPU path.
50. ~~Make the agent-facing starter and conformance loop presentation-aware and
    full-bar correct.~~ Component viewports, installed widths, and package asset
    dimensions now share the 2008×60 logical canvas limit. The generated plugin
    demonstrates semantic theming, persistent tap expansion, transient
    hold-slide expansion, two-item sizing, and lifecycle handling. `plugin test`
    executes standard compact/half/full-bar widths plus every declared item
    min/preferred/max width and emits either a readable matrix or a versioned
    JSON report. `scripts/test-plugin-scaffold.sh` proves a newly generated
    standalone project through Wasm build, manifest check, 16 real-host render
    cases, and archive creation.
51. ~~Add deterministic headless interaction replay and semantic snapshots for
    component authors and agents.~~ Strict v1 JSON scenarios drive the production
    coordinate hit tester, contact capture, sliders, stationary hold timing,
    presentation lifecycle callbacks, and complete dynamic theme snapshots.
    Reports contain exact guest events and presentation commands plus named
    semantic trees, responsive representations, primitive counts, effective
    palettes, and guest-render counts. Every generated package includes a
    tap/hold/theme scenario, and the external scaffold acceptance executes it.
    Replay has no OS broker authority; broker fixtures use a private fake
    endpoint rather than contacting desktop services.
52. ~~Add scoped fake focus context to deterministic component replay.~~ A
    scenario may initialize and update `application.id` and `workspace.id`, but
    only when the package declares `context.read.v1` and only within its exact
    manifest fact allowlist. The fake service speaks the production private
    sequenced-packet ABI and implements async snapshots, subscriptions, and
    ordered resource events while denying every other capability. The component
    demo proves a real guest subscription changing its rendered semantic label
    from terminal to Firefox without Hyprland or desktop authority.
53. ~~Add bounded GPU raster snapshots to deterministic replay.~~ An explicit
    host-controlled output directory enables at most 64 named checkpoints per
    scenario. A surfaceless GLES3 pbuffer runs the same native UI renderer used
    by live components, reads back and vertically normalizes RGBA pixels, and
    writes create-only PNGs without exposing a path or filesystem capability to
    the guest. The report records the renderer and artifact name; generated-pack
    acceptance verifies a 160×60 RGBA image, and visual M1 inspection confirmed
    the correctly oriented themed `APP firefox` state.
54. ~~Add exact, typed D-Bus fixtures to deterministic component replay.~~
    Named call and subscription expectations traverse the real private broker
    ABI and the production decoder, identifier, shape, manifest-scope, and
    physical-activation authorizers without contacting the desktop. Responses
    are typed and consumed in order; `PropertiesChanged` steps target only an
    already-open fixture resource. Unexpected or unused traffic fails the run,
    and the report records stable fixture IDs and outcomes. The sandboxed MPRIS
    example proves initial state, a live signal update, and activation-gated
    PlayPause through the complete host callback path.
55. ~~Add a production-compositor onscreen plugin simulator.~~ `touchbar-sessiond`
    now exposes an optional triple-buffered desktop Wayland output for its exact
    final 2008×60 GPU scene and maps mouse/native-touch input through the normal
    capture and presentation router. The fresh v1 surface protocol carries an
    explicit physical/synthetic contact origin end to end, so simulator input
    remains interactive but cannot authorize sensitive broker operations.
    `touchbarctl plugin dev` installs the complete local pack into a disposable
    private store, starts its production supervisors, previews every item by
    default, supports focused item/width and display-scale overrides, and cleans
    up processes, sockets, and store state on close or interruption.
56. ~~Add exact typed HTTP fixtures to deterministic component replay.~~ Both
    inline and streaming requests match one complete method/URL/header/body
    shape and traverse the production HTTP decoder, normalized manifest scope,
    path, private-network, byte, rate, and operation authorizer without DNS or
    network access. Typed response metadata, bounded text/binary chunks,
    completion, and terminal errors use the production wire schema and ordered
    resource events. Unexpected requests, unmodeled redirects, mismatched
    operation/response kinds, and unused responses fail closed. The sandboxed
    HTTP example proves an exact fake GitHub stream through the host callback
    path and rendered semantic result.
57. ~~Add exact typed command fixtures to deterministic component replay.~~
    Complete command IDs and typed named values traverse the production wire
    decoder and normalized manifest-rule authorizer without resolving an
    executable or launching a process. Ordered stdout/stderr chunks, typed
    exits, and terminal errors use the production resource-event schema;
    aggregate output is checked against the matched command's declared limit.
    Unexpected and unused runs fail closed. The sandboxed Command Deck pack
    proves a harmless fixed command through the complete host callback path
    and visibly renders its pending/running/success/failure lifecycle.
58. ~~Add exact typed filesystem-read fixtures to deterministic component
    replay.~~ Logical-mount list, inline-read, and streamed-read requests pass
    through the production decoder and normalized path/kind/scope authorizer
    using non-openable synthetic bindings. Typed directory entries, file
    chunks, metadata, completion, and terminal errors enforce request and
    manifest limits without selecting or reading a host directory. Traversal,
    inconsistent ranges, unsorted entries, unexpected calls, and unused
    responses fail closed. The sandboxed filesystem example proves a complete
    list-then-stream guest callback flow and rendered result.
59. ~~Add exact framed local-service fixtures to deterministic component
    replay.~~ The real Unix backend and replay now share pure connect/send
    authorization against opaque connection authority. Replay uses synthetic
    non-connectable endpoint bindings, exact ordered outbound frames, bounded
    inbound frames, clean/error termination, resource sequences, and manifest
    frame/traffic/rate limits without opening a socket. Unopened connections,
    mismatched or unused exchanges, and events after termination fail closed.
    Command Deck proves connect-send-receive-close through the guest callback
    path and visibly renders the received byte count.
60. ~~Add exact notification and URI-open fixtures to deterministic component
    replay.~~ Notification send/remove and URI-open requests traverse the
    production typed decoder and scope authorizers. URI opening still consumes
    a physical-origin activation derived only from replayed touch, while a
    deterministic replay-clock ledger enforces the manifest notification rate.
    Rejecting transports guarantee that no portal, notification daemon, or
    browser is contacted. Exact ordered outcomes, unsafe scopes, mismatched
    requests, and unused responses fail closed; a sandboxed reference component
    proves both async completion paths and rendered results.
61. ~~Add exact clipboard read/write fixtures to deterministic component
    replay.~~ The production boundary now exposes one reusable typed authorizer
    covering MIME, user-owned binding presence, byte ceilings, and fresh
    physical activation. Production retains its durable source-bound quota;
    replay applies the same manifest operation limit to its deterministic clock
    and never constructs a Wayland transport. Exact read values and write
    outcomes enforce MIME/byte/operation shape, unexpected and unused traffic
    fails closed, and a sandboxed component proves both async paths while
    rendering only a fake value's byte count.
62. ~~Add exact secret-read fixtures to deterministic component replay.~~ The
    production name/scope/binding and physical-activation check is now one
    reusable authorizer. Replay synthesizes only non-resolvable Secret Service
    item bindings, validates fake value size/content type with production code,
    and never constructs a D-Bus transport. Ordered responses remain private to
    the broker/guest ABI; reports and errors contain no value material. Scope,
    binding, metadata, size, unexpected-request, and unused-response failures
    close the run, and the sandboxed reference asserts that its conspicuously
    fake credential is absent from the JSON report.
63. ~~Complete the `command.run.v1` reference and resource-pressure gate.~~
    Command Deck now includes an exact `/usr/bin/printf` probe with visible
    `PROBE`/`RUN`/`OK`/`ERR` state, deterministic typed replay, and explicit
    session-only consent through the guarded first-party physical runner. The
    production backend campaign repeats touched bounded allocations,
    over-limit virtual-memory reservations, fork saturation, cancellation,
    descendant death, worker reclamation, and cgroup removal while asserting
    the configured task, hierarchy, memory, and swap ceilings. The physical
    runner is prepared but was not invoked by automated verification.
64. ~~Match Tiny DFR's full-seat idle activity semantics without widening any
    protocol.~~ The hardware service now owns a discard-only libinput `seat0`
    observer covering keyboard, pointer, gesture, and touch activity in both
    fallback and authenticated-session loops. Its opener accepts only exact
    `/dev/input/eventN` paths, refuses write access, and uses `O_NOFOLLOW`; the
    service unit independently permits only read access to input devices. Raw
    event content never leaves the module, while dedicated Touch Bar and Fn
    readers remain authoritative. Unit tests cover path and access rejection,
    and an opt-in live test proved seat discovery and event draining without
    taking control of the Touch Bar. Installed suspend/resume acceptance passed
    on 2026-09-06 with a real ten-second s2idle cycle and an intact authenticated
    hardware session after resume.
65. ~~Add a hardware-owned recovery latch and finish the fresh-v1 naming
    sweep.~~ Holding physical Fn for eight seconds now disconnects the user
    compositor, releases injected keys, retains the trusted fallback, and
    rejects reconnects until Fn is released and held again. A root-owned,
    symlink-rejecting runtime marker makes the state visible through
    `touchbarctl hardware status`; pure-clock and adversarial marker tests cover
    its lifecycle. Physical installed-service acceptance passed on 2026-09-06:
    the first hold disconnected the session and latched the fallback, and the
    second hold restored `normal` state with automatic authenticated reconnect
    and a balanced final Fn release.
    The remaining old project-prefixed frame and hardware-message magic values
    were replaced in place with fresh TouchBar values, with no compatibility
    decoder or migration path.
66. ~~Make plugin failure visible and bound raw native rendering traffic.~~
    Required missing contributions retain fail-closed profile readiness while a
    theme-aware, host-owned, noninteractive status layer distinguishes consent,
    startup/restart, and stopped crash loops. Fn temporarily reveals the full
    trusted function row, and live reconnection removes the placeholder. A
    per-client 120 Hz token bucket with eight-commit burst drops buffers before
    CPU/GPU import, bounds pending frame callbacks, and disconnects after 256
    rejected commits in one second. Pure-clock tests, lifecycle transition
    tests, live profile teardown/reconnect, and an actual 640-commit Wayland
    flood cover the behavior without physical hardware.
67. ~~Add theme-configurable battery-aware animation pacing.~~ The compositor
    reads bounded kernel `type`/`online` attributes once per second without
    mutating power state, treats malformed or unavailable data as unknown,
    and dynamically selects strict 1–60 Hz `animation_hz` or
    `battery_animation_hz` theme caps. Defaults preserve 60 Hz on external or
    unknown power and use 30 Hz on battery; the effective source and cadence
    are machine-readable. Pure discovery/change tests and a fake-sysfs
    Apple-GPU integration prove both rates without touching physical hardware.
