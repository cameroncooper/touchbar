# UI Kit Foundation

The first retained UI foundation lives entirely inside the standalone plugin
process. `touchbar-sessiond` still receives only a Wayland surface, size and visibility
configuration, and raw captured contacts. It does not receive or execute the
widget tree.

## Data flow

```text
RetainedUi<Node>
      │ resolve_with_measurer(compositor-assigned bounds, Theme, renderer)
      │
      ├── Taffy layout        exact child rectangles
      ├── Scene                GPU drawing commands
      ├── InteractionMap       local hit targets
      └── InspectorSnapshot    semantics + responsive choices
```

One resolved geometry therefore drives pixels, hit testing, and semantic
inspection. An advanced plugin can issue raw GLES before or after the UI-kit
renderer without changing its compositor contract.

Sandboxed components do not receive raw GLES. They use the same retained
foundation through the bounded, theme-aware
[Canvas2D contract](canvas2d.md) and the separately bounded
[validated-effect contract](validated-effects.md); trusted native host code
validates and draws both on the GPU.

## Retained nodes

`Node` currently supports layers, locally positioned children, clipped rows
and columns, panels, labels, icons, cached RGBA images, composable pressables,
buttons, toggles, styled sliders, meters, tiny graphs, progress, opacity groups,
host-rendered Canvas2D, validated procedural effects, embedded custom GLES, and
responsive nodes. Paint-time `Motion` nodes add
translation, uniform scale, and opacity without changing layout. Rows and
columns contain `FlexItem` values whose
`Flex` constraints describe minimum, basis, maximum, grow/shrink weights,
visibility priority, whether an item is required, and whether its basis comes
from measured content.

Taffy 0.14 now performs flex sizing, padding, gaps, and cross-axis alignment.
The small original width distributor has been removed. Touch Bar-specific
priority hiding remains a policy pass immediately before Taffy: a general
layout engine cannot know that album artwork is optional while a transport
button is required. `Layer` deliberately gives children the same bounds and
`Positioned` uses explicit local coordinates, so those two primitives do not
need a layout solver.

`Flex::content` lets shaped text, image dimensions, controls, or a nested
container provide intrinsic size. `LayoutMeasurer` keeps the tree independent
of a renderer. The normal GPU path implements it using the renderer's existing
Cosmic Text engine; renderer-independent tests retain a deterministic fallback.
The Taffy measurement callback combines those leaf sizes with exact
compositor-assigned constraints.

When the minimum widths cannot fit, low-priority children are hidden first;
equal-priority ties hide the trailing child first. Remaining children shrink
to their minima or grow to their maxima. Clipping prevents required overflow
from drawing or receiving hits outside the plugin surface.

A responsive node contains full, compact, and minimal alternatives:

```rust
Node::responsive(
    WidgetId(1),
    vec![
        ResponsiveVariant::new(Representation::Full, 112.0, full_button),
        ResponsiveVariant::new(Representation::Compact, 52.0, icon_button),
        ResponsiveVariant::new(Representation::Minimal, 28.0, tiny_button),
    ],
)
```

The richest alternative whose minimum width fits is selected each time the
compositor configures a new width. The choice is included in the inspector
snapshot. Visual compression must not erase the semantic label; an icon-only
variant can still expose a complete accessible name.

`RetainedUi` keeps the node tree and a monotonically changing revision.
Mutation, replacement, theme changes, and visibility resume mark it dirty.
Resolving the tree clears that dirty state and retains the latest inspector
snapshot.

## Layout performance probe

Run the renderer-independent release probe with:

```bash
cargo run --release -p touchbar-ui --example layout_probe
```

The probe repeatedly resolves a media-style tree containing optional artwork,
a nested text column, responsive title variants, intrinsic text measurement,
and a required play button. On the development M1, 5,000 complete layouts at
80, 160, and 360 pixels measured approximately 7.7, 5.6, and 3.7 microseconds
per layout respectively. Layout therefore remains comfortably below either the
60 Hz plugin render budget or the current 30 Hz physical presentation cadence.

## Rendering

The GLES renderer now handles:

- antialiased rounded rectangles with premultiplied-alpha output;
- nested rectangular clipping through the scissor test;
- Unicode shaping, system-font fallback, bidi layout, ellipsis, and alignment;
- explicit ellipsis, clip, and continuously scrolling marquee overflow;
- theme-tinted procedural icons;
- bounded static-SVG rasterization for plugin assets;
- a cached, theme-tintable built-in SVG symbol catalog;
- RGBA images cached by stable resource ID and revision;
- contain, cover, and stretch image fitting;
- original, theme-multiply, and theme-mask image coloring;
- nested opacity groups;
- arbitrary interleaving with plugin-authored GLES callbacks.

Text is shaped and rasterized on first use, then cached as a texture for GPU
composition on subsequent frames. The coordinated CPU/GPU cache is bounded to
256 runs so changing labels do not leak resources. See the [collection and
gesture milestone](collection-gesture-foundation.md) for the text and input
contracts.

`TextOverflow` makes constrained behavior explicit. Ellipsis is the safe
default, clip retains the complete run behind the label bounds, and marquee
shapes the complete single-line run once and translates its cached texture at
render time. `TextMeasurement::Reserve` measures a stable sample such as
`00:00` while displaying a changing value, preventing numeric jitter without
requiring every font to support tabular figures.

Image pixels use straight RGBA in the public API. The shader premultiplies them
when sampling so they compose with the same `ONE, ONE_MINUS_SRC_ALPHA` blend
contract as every other plugin surface. Reusing an ID and revision reuses the
GPU texture; changing the revision replaces it. `ImageTint::Multiply` preserves
image color while applying a semantic theme color. `ImageTint::Mask` ignores
RGB and fills the image's alpha mask, which is appropriate for tintable logos
and symbolic SVG assets. `SvgRasterizer` parses static SVG with `resvg`,
preserves aspect ratio, produces straight-RGBA `Image` data, and uses a bounded
cache keyed by asset ID, revision, and raster size. Its deliberately small
profile does not enable SVG text, system fonts, compressed SVG, or embedded
raster-image loading. Symbolic SVGs stay theme-neutral in that cache and use
`ImageTint::Mask` in the GLES shader, so theme changes do not rerasterize them.
Theme roles are resolved on every
UI pass, so retained nodes do not need reconstruction when theme changes
theme.

## Render-time motion

`Motion` has a stable ID, start time, duration, easing curve, and `from`/`to`
visual transforms. `Renderer::draw_at` samples it using the frame's monotonic
time, then applies translation, uniform scale, and opacity in the paint pass.
Geometry, flex measurement, semantics, and hit targets remain unchanged. Text
is rasterized at its settled layout size and its cached texture is scaled
during composition, avoiding a new glyph texture for every animation frame.

The renderer exposes `MotionPolicy::Full`, `Reduced`, and `Disabled`. Reduced
and disabled policies settle motion immediately. The compositor carries that
policy in each atomic appearance snapshot, and sandboxed components receive it
in their WIT theme. host theme files may select it with
`motion = "full"`, `"reduced"`, or `"disabled"`.
The same theme can set `animation_hz` and `battery_animation_hz`; the compositor
paces frame callbacks at the effective AC/battery rate while motion sampling
continues to use host-monotonic time.

## Composable controls and custom rendering

`Pressable` attaches button or toggle identity, tap/hold behavior, semantic
state, themed background, padding, and a minimum touch target to any child
tree. Its child can therefore combine artwork, nested text, progress, icons,
or a custom renderer. The older `Button` and `Toggle` nodes remain convenience
controls rather than defining the limit of interactive content.

Accent and destructive content can select the derived `OnAccent` and
`OnDestructive` roles. These choose black or white from the live background's
relative luminance, avoiding unreadable labels when a theme switches between a
light and dark accent. `PressableStyle::accent` propagates `OnAccent` into the
child subtree automatically, and selected convenience toggles use the same
contrast rule.

`ProgressValue` supports determinate state and a plugin-supplied normalized
phase for indeterminate animation. Track and fill are semantic `ColorRole`
values. `Opacity` multiplies all enclosed built-in paint. Use `Motion` for timed
cross-fades and entrance transitions without changing layout, hit testing, or
semantic identity.

### Continuous controls and compact data

`ContinuousValue` is the common scalar adapter for controls. It clamps a
finite minimum/value/maximum triple, converts to and from normalized touch
coordinates, and can quantize to a plugin-selected step. The interaction event
remains normalized, so the same slider input contract works for volume,
decibels, color temperature, playback seconds, or split ratios:

```rust
let range = ContinuousValue::new(-60.0, 6.0, -12.0).step(0.5);
let decibels = range.value_at(normalized_touch_value);
```

`StyledSlider` adds semantic track, fill, and thumb colors; independent track
and thumb sizing; optional ticks; and an optional thumb. Its complete assigned
bounds remain the touch target even when the visible track is thin.

`Meter` uses the same range and can be continuous or segmented. Active
segments interpolate between semantic low/high colors and an optional peak
marker has its own theme role. `TinyGraph` accepts arbitrary scalar history,
reduces excess samples to the available pixel width using peak-preserving
groups, and paints theme-derived bars plus an optional baseline. These are
ordinary retained nodes, so clipping, opacity, motion, and appearance changes
work exactly like they do for labels and images.

The virtualized `Scrubber` remains the collection state and gesture model.
`Node::scrubber_cell` supplies its common selected/highlighted visual treatment
without restricting plugins that need artwork or complex child trees.

### Symbol catalog

`SymbolCatalog` currently provides volume, mute, transport, microphone,
brightness, battery, Wi-Fi, Bluetooth, workspace, capture, timer, terminal,
and graph symbols. Each is a static SVG cached through `SvgRasterizer` and
returned as an alpha-mask image with a semantic `ColorRole`. Adding catalog
coverage therefore does not grow the renderer's shader or procedural-icon
match, and theme changes recolor existing textures without asset work.

`CustomGles` is a retained layout leaf keyed by `CustomGlesId`. The plugin
registers its callback on the GLES renderer and receives `CustomGlesFrame` with
resolved bounds, the intersected parent clip, surface dimensions, accumulated
opacity, and the exact live `Theme` used by adjacent toolkit nodes. The
renderer establishes scissoring and restores the toolkit's viewport, clip,
premultiplied-alpha blend contract, and subsequent program bindings after the
callback. The callback owns any additional GLES resources and must render into
the current framebuffer.

## Semantics and inspection

Every resolved control produces a `SemanticNode` with its stable widget ID,
role, label, optional value and hint, bounds, enabled/selected state, and
children. Responsive choices and the semantic root are returned in an
`InspectorSnapshot`.

This remains local to the plugin process. It is the stable basis for:

- an onscreen debug/layout inspector;
- accessibility and zoom bridges;
- customization previews;
- golden snapshots and input replay diagnostics.

The headless component replay tool now serializes this local inspector data for
author tests; it never sends semantics to the compositor. Publishing live
semantic snapshots across that trust boundary would require a separately
versioned protocol. The compositor must not infer semantics from pixels.

## Scheduling and visibility

`FrameScheduler` distinguishes dirty static content, bounded animation, and
continuous animation. Invisible UI is suspended and becomes dirty when shown
again. `touchbar-client` now applies the same rule to Wayland frame callbacks:
a hidden animated surface stops submitting frames, remembers pending redraws,
and resumes when it becomes visible.

The UI demo deliberately enables continuous animation to exercise 60 Hz GPU
submission. Production widgets should remain idle unless state changes or a
time-bounded interaction is animating.

## Demonstration

Run the headless hardware-GPU integration:

```bash
./scripts/run-sdk-ui.sh 120
```

The compact volume item resolves to its compact icon representation. Synthetic
press-and-hold opens a 360-pixel fixed palette, keeps its activator pinned to
the expansion edge, and selects a stationary option on release. A short tap
opens the same palette persistently for a later tap. The
runner validates DMA-BUF transport, hardware GLES, appearance delivery,
captured interaction, presentation changes, clean buffer release, and the
compositor's 60 Hz headless cadence.

## Next UI work

The asset/text/motion substrate and first reusable control family now support
rich widgets without requiring a general desktop GUI framework. The next UI
slice should build a deterministic visual/input harness with snapshots, fake
state feeds, and touch replay, then use it for the first production System
Controls plugin. A public inspector/accessibility transport remains separately
versioned from rendering; in-place, named-slot, named-region, and explicit
full-bar placement remain compositor-layout work.
