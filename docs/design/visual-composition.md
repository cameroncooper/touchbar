# Visual composition contract

The Touch Bar scene is 60 logical pixels tall and as wide as the attached
panel; the Apple silicon strip is 2008. The ADP
presenter alone handles the panel's rotated physical orientation. This keeps
plugin coordinates, layout, input, and headless tests independent of scanout.

## Layer roles

The first role is a singleton **backdrop**. It is always configured to the
complete canvas, stacked below ordinary content, and excluded from hit testing.
It can provide static artwork or an animated GLES scene, but cannot choose item
geometry or intercept touches.

Normal **item** surfaces participate in semantic bar layout. A popover is an
anchored presentation of its originating item: it receives bounded geometry
and is stacked above normal items. A transient popover retains its opening
contact across the resize for hold-slide-release interaction. A persistent
popover outlives the opening tap, accepts a later contact, and consumes an
outside press to dismiss. Future overlays should use distinct policy-owned
roles rather than overloading the backdrop.

Before expansion, the compositor translates the compact item's bounds into
the expanded surface and reports that anchor to the plugin. A plugin can
therefore pin its activator and grow fixed options away from the nearest edge;
it never needs to infer global placement from touch coordinates.

Presentations are identified sessions. The plugin owns rendering and any
nested page stack inside the assigned rectangle; the compositor owns the
rectangle, modal input routing, and restoration. If context recomposition moves
the source, the anchor and presentation geometry are recomputed. If it removes
the source, the session ends with `SourceHidden` rather than becoming an
orphaned overlay. The full lifecycle contract is documented in
[`presentation-sessions.md`](presentation-sessions.md).

```text
popover / policy overlays
item surfaces selected by active bar composition
singleton full-canvas backdrop
daemon fallback background color
```

## Alpha

Protocol colors and UI-kit `Color` values use straight (unpremultiplied) RGBA
because that is the least surprising authoring format. Renderers produce
premultiplied pixels, including fractional edge coverage, and blend with
`source=ONE` and `destination=ONE_MINUS_SRC_ALPHA`.

The final ADP buffer is opaque XRGB. Transparency therefore means “show the
layers beneath me,” ending at the daemon's opaque fallback background. It does
not make the physical OLED transparent.

## Appearance ownership

`touchbar-sessiond` selects appearance policy and sends immutable, generation-numbered
snapshots. Each snapshot includes a dark/light scheme, corner radius, and the
semantic roles background, surface, surface-hover, surface-pressed, foreground,
muted, accent, and destructive. A client activates a snapshot only after its
`done` event, so theme changes cannot produce mixed-generation frames.

The Rust client SDK exposes the snapshot through
`Application::appearance_changed`. The UI kit converts it directly to `Theme`.
Raw GLES plugins can use the same values in shaders or ignore them. This
division keeps all plugins visually consistent by default without making the
UI kit responsible for filesystem discovery or giving different SDKs
different theme policy.

Source selection is host policy. `TOUCHBAR_THEME` has highest precedence,
followed by an existing `~/.config/touchbar/theme.toml`, an eligible installed
appearance provider, and finally the built-in dark palette. A package declares
an `[[appearance-provider]]` with an exact desktop-session matcher, one
permission-bound source mount and a relative TOML path. The provider is usable
only while `appearance.provide.v1` authorizes both that mount and its
package-local provider ID. This purpose-specific binding is not a generic
filesystem capability available to rendering code.

The daemon securely reopens the installer-bound root, verifies its device and
inode, and resolves the source without symlinks, parent traversal, magic links,
or mount crossings. It accepts only complete bounded palettes and retains the
last valid snapshot through incomplete atomic replacements. Providers supply
scheme and semantic colors; the host owns motion, cadence, derived control
roles, generation assignment, arbitration, and atomic publication. Revoking
the grant removes the provider from selection without disabling its drawing
items.

## Context transactions

Context is represented as an immutable generation of typed facts. Rules select
at most one bar per scope (window, application, workspace, global) using
priority and stable declaration order; the result is composed most-specific
first with a fallback bar.

Selection and layout are prepared as a new composition snapshot. If no touch
is captured, it may commit immediately. If a gesture is active, only the newest
pending snapshot is retained and the current composition stays live until all
captured contacts end. This prevents a focus change from deleting or moving the
control underneath a finger midway through a drag.
