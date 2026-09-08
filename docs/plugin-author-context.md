# TouchBar plugin author context

TouchBar packs are immutable local packages. A pack may contribute multiple stable items. The default runtime is a WebAssembly component; it has no ambient operating-system access and reaches the desktop only through explicitly requested, user-approved broker capabilities.

Packages may own bounded automatic profiles for their own items. This is the
zero-configuration path for an application integration: declare a `[[profile]]`
with exact `applications` and/or foreground-layer `activities`, and the host
merges it in memory when the plugin is enabled. The package cannot reference
another package's item, inspect arbitrary context, choose a competing priority,
or rewrite `profiles.toml`. User rules always take precedence.

```toml
[[items]]
id = "controls"
label = "Browser controls"
default_width = 320
show_in_default_profile = false

[[profile]]
id = "firefox"
label = "Firefox"
items = ["controls"]
principal_item = "controls"
applications = ["firefox", "org.mozilla.firefox"]
```

`show_in_default_profile = false` keeps a contextual item out of the generated
all-plugin fallback while leaving its process available for its automatic
profile. Both item and profile choices survive reinstall/update. A user can run
`touchbarctl plugin profile SOURCE PROFILE disable` without editing a profile
document. `plugin run --when-application CLASS` remains the disposable
on-device test path.

Use theme roles from the UI protocol rather than fixed foreground/background colors. The daemon sends a fresh theme snapshot whenever the active palette changes. Layouts must render at any assigned width; use responsive variants and keep item IDs stable across releases. The canvas follows the attached panel, so never assume a particular total strip width. The component host supplies the manifest's canonical `github:owner/repository` source as the runtime plugin identity. A native runtime must pass that exact manifest source to `ClientOptions::new`; profiles identify a surface with the collision-free pair `{ plugin = "github:owner/repository", item = "local-item-id" }`.

Use `touchbarctl plugin dev --package DIR --item ID` for the normal unprivileged
desktop preview. On Touch Bar hardware, use
`touchbarctl plugin run --package DIR --item ID --width PX`. It gives a new
workspace `touchbar-sessiond` a connection-scoped lease while the privileged
hardware service continues running, uses an isolated store, and restores the
installed user session on Ctrl-C, CLI failure, or disconnect. Trusted local
hosting is the default; add `--sandboxed` when testing production grants,
resource confinement, or broker events. Add
`--when-application CLASS` to test a real Hyprland focus-driven profile switch
without changing the user's installed plugins or profile configuration. The
target window must actually become focused: the runner checks the observed
context transition and fails if a launcher merely creates an unfocused window.

Use the bounded `Canvas2d` SDK node for custom graphs, indicators, drawings,
and other portable GPU content. Give it a local view box and let the host scale
and clip it to the assigned region. Prefer `theme_paint` or
`theme_paint_with_opacity` for text, strokes, fills, and tint-like effects so a
retained scene recolors immediately when the theme changes. Use `rgba_paint`
only when the literal color carries meaning that should not follow the theme.
Canvas geometry is visual; compose it with the standard pressable, slider, and
gesture controls for interaction. Native process plugins may use raw GLES when
the bounded component UI is insufficient.

Declare expandable content with package-local `[[bar]]` entries. Give the bar
container and each item element ordered min/preferred/max widths, then link an
item with `expanded_bar` and/or `press_and_hold_bar`. An input callback can
return `PresentationCommand::Begin`; the host binds transient requests to the
real contact and the compositor places every referenced surface. Handle
`PresentationEvent::Ended` to clear guest state after selection, outside press,
hot reload, or rejection. Every member receives its own live theme snapshot and
must remain responsive at its presentation width.

Wrap any retained subtree with `ViewBuilder::motion` for GPU animation. Give
each animation a stable nonzero ID; an unchanged ID and description preserve
host-owned phase through rerenders, while a changed description restarts it.
Use one-shot, loop, or alternate playback rather than guest timers. The host
paces visible frames, suspends hidden content, and settles motion when the
appearance policy is reduced or disabled, so do not implement a parallel Wasm
frame loop.

For richer Canvas2D art, use `fill_linear_gradient_rect` with semantic paints
at both stops, and build stroked curves with `Path2d`. The host validates path
grammar and coordinates, flattens quadratic and cubic segments within a fixed
global point budget, and renders them with native GLES. Do not pre-tessellate a
large curve into a polyline or attempt to choose host subdivision counts.
Use `fill_path` for one closed simple polygon such as an icon, chart area,
badge, or pointer. Filled paths deliberately reject holes, self-intersections,
repeated vertices, and open contours so host triangulation stays deterministic
and bounded.

Declare static PNG or symbolic SVG files with `[[asset]]` entries under
`assets/`, including a stable kebab-case ID and exact decoded dimensions. Use
`ViewBuilder::image` with that ID; the component never opens or decodes the
file. Prefer `ImageTint::Mask(ColorRole::...)` for one-color symbols and
`ImageTint::Multiply` for theme-reactive artwork. Both remain live semantic
theme references. SVG assets must be self-contained and cannot use external
images, hrefs, scripts, foreign objects, CSS URLs/imports, doctypes, or entities.
See `docs/design/package-assets.md` for quotas and the sealed handoff.

Use `GpuEffect` when Canvas2D is not expressive enough for a procedural
backdrop, visualizer, glow, or touch response. Its source is a restricted
straight-line WGSL body, not a complete shader: write immutable `let` bindings
and finish with `let color: vec4<f32> = ...;`. The host supplies normalized
`uv`, logical `size`, monotonic wrapped `time`, `params0`/`params1`, and the
semantic colors `background`, `control`, `control_pressed`, `track`,
`foreground`, `muted`, `accent`, `destructive`, `on_accent`, and
`on_destructive`. Prefer those theme variables over literals. Give each effect
a stable nonzero ID, use at most eight scalar parameters, and request an
animation period only when pixels actually move. Reduced-motion policy freezes
time automatically. The host parses and fully validates the source, rejects
control flow/resources/mutation, and sends only translated bounded GLSL ES to
the driver. See `docs/design/validated-effects.md`.

The development loop is:

1. `touchbarctl plugin new my-plugin --source github:owner/repository`
2. `touchbarctl plugin build`
3. `touchbarctl plugin check`
4. `touchbarctl plugin test --format json` (renders the compact matrix and every
   declared presentation width through the real component host)
5. `touchbarctl plugin replay --scenario tests/interaction.json` (deterministic
   contact, hold, presentation lifecycle, theme, and semantic snapshots; add
   `--screenshots screenshots` for host-controlled GPU PNGs at named checkpoints;
   exact D-Bus call/subscription, HTTP inline/stream, constrained-command, and
   filesystem-read, local-service, notification, URI-open, clipboard, and secret-read fixtures exercise asynchronous state
   without contacting the desktop/network, launching a process, reading host
   files, or opening a Unix socket)
6. Run `touchbarctl plugin dev` for the complete compositor desktop preview.
7. On supported hardware, run `touchbarctl plugin run --item ID --width PX` for
   an isolated workspace-session preview on the physical strip.
8. `touchbarctl plugin pack`
9. Test the resulting `touchbar-plugin.touchbar` with
   `touchbarctl plugin add --path touchbar-plugin.touchbar`
10. Attach that exact asset name to a canonical `vMAJOR.MINOR.PATCH` GitHub
   Release. Users install it with `touchbarctl plugin add github:owner/repository`.
11. Optionally run `touchbarctl plugin submit --alias ALIAS --categories a,b`
   and propose the emitted entry for the reviewed discovery catalog.

Replay broker fixtures are strict expectations, not mocks with ambient
authority. Use `broker.dbus_calls`/`dbus_subscriptions` for MPRIS-style state,
`broker.http_requests` for exact inline or streaming network responses, and
`broker.command_runs` for exact manifest command IDs and typed values. Use
`broker.filesystem_reads` for logical-mount directory, inline file, and streamed
file responses without opening a host path, and `broker.local_connections` for
exact framed connect/send/receive/close flows without opening a socket. Use
`broker.notification_requests` and `broker.uri_opens` for exact portal-free
desktop outcomes, and `broker.clipboard_requests` for separately scoped read and
write outcomes without contacting Wayland. URI and clipboard requests still
require a replayed physical-origin tap. `broker.secret_reads` supplies exact
logical-name responses without contacting Secret Service and has the same tap
requirement. Use fake clipboard and secret values only.
Command responses preserve
stdout/stderr ordering and end in a typed exit or broker error; their aggregate
bytes cannot exceed the matched manifest rule. See the working fixtures under
`examples/broker-component-plugin/tests`, `examples/http-component-plugin/tests`,
`examples/filesystem-component-plugin/tests`,
`examples/desktop-action-component-plugin/tests`, and `plugins/command-deck/tests`.
The clipboard reference fixture is under
`examples/clipboard-component-plugin/tests`; the redacted secret reference is
under `examples/secret-component-plugin/tests`.

Stable installation never builds a branch. The installer queries GitHub's
release API without following repository redirects, selects exactly one
`touchbar-plugin.touchbar` asset, requires GitHub's SHA-256 digest,
checks the downloaded size and bytes, then validates the package source,
version, host API, permissions, archive structure, and artifacts before
changing the active lock. Immutable releases receive verified-release
provenance; mutable releases are visibly unverified. Use
`touchbarctl plugin update SOURCE` and `touchbarctl plugin rollback SOURCE` for
locked upgrades and retained-version recovery.

The generated release workflow creates a GitHub artifact attestation for the
fixed package asset. During installation, an absent attestation is reported but
does not exclude an independent publisher. If GitHub advertises one, the
installer's native Rust verifier proves the artifact digest, exact repository
and numeric identity, release tag, canonical release workflow, Fulcio chain,
SCT, and Rekor transparency evidence. It has no `gh` executable dependency;
invalid advertised attestations fail closed.

Publication is native as well. `touchbarctl plugin publish --tag
vMAJOR.MINOR.PATCH --repository OWNER/REPO` requires `GITHUB_TOKEN`, verifies
that the exact tag already exists, stages a bounded immutable copy of
`touchbar-plugin.touchbar`, creates a draft, validates GitHub's repository-bound
upload URL, checks the uploaded size and digest, and only then publishes the
release. A failed upload or finalization removes the incomplete draft. The
generated workflow invokes this command after package attestation and never
requires the GitHub CLI.

Packages declare permissions in `touchbar-plugin.toml`. Required permissions block startup until granted; optional permissions appear unavailable to the component and must degrade gracefully. Never put secrets in the manifest or package. Filesystem paths, D-Bus calls, HTTP origins, command templates, local sockets, clipboard access, notifications, URI opening, and secrets are mediated by the native supervisor and validated against the approved scope.

Authors never write grant files or initiate arbitrary runtime prompts. Users
inspect normalized requests with `touchbarctl plugin permissions SOURCE` and
choose `allow`, `deny`, or `reset` with an explicit `--session` or
`--persistent` lifetime. Keep author reasons short and factual. Treat every
optional capability as dynamically revocable: finish rendering immediately,
submit broker work only from input or host-event callbacks, and respond to the
capability-change event by closing affected UI state or showing a safe disabled
state. Use `--format json` in agent-driven setup and tests.

Run `touchbarctl plugin context --format json` when an agent needs a compact machine-readable summary.
Use `touchbarctl plugin search` for the bundled reviewed catalog. Catalog
aliases are conveniences only: direct GitHub URLs always work, and installed
state, updates, grants, and rollback remain keyed by canonical repository
identity.
