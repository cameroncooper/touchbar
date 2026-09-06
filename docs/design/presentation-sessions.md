# Presentation sessions

The v1 protocol models temporary UI as an identified session rather than as a
binary compact/expanded flag. This lets a plugin build rich nested content
without taking ownership of global placement or leaving a delayed close able
to dismiss a newer presentation.

## Ownership

The plugin owns the contents of its surface, its page stack, control state,
animation, and the actions produced by selection. The compositor owns whether
the presentation is allowed, its global rectangle and stacking, input capture,
outside-touch behavior, context invalidation, and the transition back to the
normal bar.

The optional `touchbar-ui` controller keeps this split explicit. A plugin asks
it to present a root page, pushes and pops arbitrary plugin-defined page values,
and drains declarative `Present` or `Dismiss` commands. It is not a remote
widget tree: rendering and navigation remain entirely inside the unrestricted
plugin process.

A package may also declare an ordered multi-item presentation bar. Bar and item
references are package-local kebab-case identifiers; the installer validates
every reference before execution and the session compositor qualifies them
with the immutable package source. A bar can contain package items, bounded
fixed spaces, weighted flexible spaces, recursive natural/equal-width groups,
and one principal item at any depth. A group has a stable package-local ID,
spacing, visibility/compression priorities, and item/group children; spaces
remain top-level because a group owns its internal geometry. It cannot inject
another package's controls or a profile context proxy. The bar declares its
container sizing, every item declares presentation sizing, and
`dismiss_on_selection` lets a cross-process palette close without private IPC.

```toml
[[items]]
id = "volume"
label = "Volume"
expanded_bar = "volume-controls"
press_and_hold_bar = "volume-controls"

[[items]]
id = "microphone"
label = "Microphone"

[[bar]]
id = "volume-controls"
minimum_width = 280
preferred_width = 360
maximum_width = 560
dismiss_on_selection = true
principal_item = "volume"

[[bar.element]]
kind = "group"
id = "audio-actions"
layout = "equal-width"
spacing = 8

[[bar.element.element]]
kind = "item"
item = "volume"
minimum_width = 92
preferred_width = 120
maximum_width = 180

[[bar.element.element]]
kind = "item"
item = "microphone"
minimum_width = 92
preferred_width = 112
maximum_width = 140
```

## Identified lifecycle

Each surface uses nonzero, increasing session IDs. Starting an anchored session
produces `presentation_anchor`; every accepted policy then produces
`presentation_started`. Ending it produces exactly one `presentation_ended`
carrying the reason. The plugin keeps its active content until that final
compositor confirmation arrives.

A stale `end_presentation` cannot close the current session. Replayed or
out-of-order begin IDs are rejected, while a duplicate begin for the currently
active session is idempotent. This matters when dismissal and a replacement are
queued together or compositor events cross a client render callback.

The supported dismissal reasons are requested, selection, outside press,
timeout, source hidden, replaced, and rejected. A `DismissalPolicy` lets the UI
controller combine those mechanisms without baking a volume-control-specific
interaction into either the SDK or compositor.

## Lifecycles and navigation

A transient session is tied to one already captured contact. As the finger
slides, the compositor transfers capture between surfaces in the declared bar
with cancel/down transitions, then dismisses on release. It is appropriate for
hold, slide, and release interactions. A persistent session has no opening
capture, accepts later contacts, and can close on selection, outside press,
timeout, toggle, or an explicit application request.

Sandboxed components return an optional presentation command from their normal
input callback. The trusted host assigns session IDs, binds transient requests
to the callback's physical contact, and maps the package-declared bar; the
guest cannot name arbitrary surfaces. Anchor, started, and ended events return
through the component world so guest state follows compositor acceptance.

Nested navigation does not start another compositor session. Pushing a detail
page changes the plugin's local page stack and redraws the same assigned
surface. Back pops one page; at the root it may dismiss according to policy.
This avoids protocol round trips for ordinary UI navigation and lets plugins
use any internal state model or rendering toolkit.

## Placement policies

The protocol and both Rust APIs name five implemented policies:

| Policy | Compositor behavior | Width contract |
| --- | --- | --- |
| Anchored | Overlay centered on and clamped around the source item's compact rectangle | Declared bar sizing, or source expanded sizing for one surface |
| In-place | Replace the source with all declared bar elements and reflow neighbors | Per-element presentation sizing and ordinary compression rules |
| Slot | Replace every element in a named slot and remove presented surfaces from former slots | Per-element presentation sizing and ordinary compression rules |
| Region | Overlay at a named compositor-owned rectangle | Exact configured region width |
| Full bar | Explicitly replace the complete active bar | Exact logical canvas width |

An item opts into presentation by publishing nonzero expanded sizing metadata.
Region and full-bar requests still require that opt-in, but their daemon-owned
rectangle intentionally overrides those constraints. Slot and region targets
are bounded lowercase identifiers; path-like targets, missing slots, missing
regions, a slot request without an active configured profile, and every other
invalid request end with `Rejected`. A request never silently changes policy or
falls back to a full-bar takeover.

Only one presentation is active across the bar. It is modal: all touch input is
routed to surfaces in the presented bar until the session ends, even when an
anchored, in-place, or region presentation leaves neighboring controls visible. A
persistent outside press dismisses and consumes that press rather than
activating the underlying control. Compact geometry and visibility are restored
transactionally after dismissal.

Named slots are part of the active profile's ordered template. Named regions
are independent top-level profile configuration so a theme or profile file can
reserve an exact global rectangle without pretending it is ordinary bar
content:

```toml
[[region]]
id = "palette"
x = 504
width = 1000
```

## Context and geometry changes

Captured interactions continue to defer profile commits. If a committed
profile retains but moves or resizes the source item, `touchbar-sessiond`
recomputes the active placement; an anchored session also publishes the new
compact anchor. If the source no longer participates in the composition, or a
live configuration change removes the requested slot or region, the session
ends with `SourceHidden`. Always-visible user widgets therefore survive
ordinary application changes, while application-owned presentations cannot
remain detached from their source or target.

## Demo

The nested acceptance path opens the persistent volume presentation, enters a
second page, navigates back, makes a selection, and verifies that the entire
sequence used one session:

```bash
./scripts/run-sdk-ui.sh 120 nested
```

Use `hold` for transient hold-slide-release and `tap` for persistent
tap-then-tap. All three paths use the hardware GLES renderer and DMA-BUF
transport without taking over the physical Touch Bar.

The policy acceptance runs independent native GPU sessions for in-place, slot,
region, and full-bar placement, checks restoration after selection, proves that
a captured hold/slide/release remains valid while slot placement relocates its
source, proves that a missing target is rejected without receiving expanded
geometry, and removes an active named region through hot reload to verify
`SourceHidden` dismissal:

```bash
./scripts/test-presentation-policies.sh
```

The component acceptance installs the real sandboxed Controls pack, expands
Volume into a two-surface 360-pixel bar, exercises tap-then-tap and hold-slide
selection, verifies compact restoration, and disables the source during an
active session to prove hot-reload invalidation:

```bash
./scripts/test-component-presentations.sh
```
