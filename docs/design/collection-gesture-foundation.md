# Collection and Gesture Foundation

This milestone adds production text, composable gesture recognition, and a
virtualized horizontal scrubber to the plugin-side `touchbar-ui` crate. None of
these types cross the Wayland protocol: plugins still receive raw contacts and
submit pixels, and unrestricted plugins can mix the retained UI with raw GLES.

## Shaped text

`TextEngine` uses `cosmic-text` advanced shaping and `swash` rasterization. It
supports Unicode shaping, system-font fallback, bidirectional layout,
alignment, one-line clipping, and visual-end ellipsis. The GLES renderer owns
one engine and transparently uses it for every `Primitive::Text`.

Shaping and glyph rasterization happen on the CPU when a run first appears.
The completed straight-RGBA run is cached and uploaded once; subsequent frames
blend that texture on the GPU. This is intentionally optimized for short,
mostly stable Touch Bar labels. The CPU and GPU caches are bounded to 256 runs,
and CPU eviction tells the renderer to delete the corresponding GLES texture.
Plugins that need shader-generated typography or a specialized atlas still
retain unrestricted GLES access.

`TextEngine` is public for measurement and prewarming:

```rust
let image = text.rasterize(
    "TouchBar مرحبا",
    180,
    32,
    16.0,
    Color::WHITE,
    TextAlign::Center,
);
```

## Gesture arena

`GestureMap` contains overlapping local regions and their recognizers. The
last matching region is topmost. `GestureArena` creates one independent arena
per compositor contact, captures that region through up or cancel, and lets
the first eligible recognizer win.

The initial recognizers are tap, long press, axis-constrained pan, and swipe.
A swipe is an end condition attached to a winning pan, allowing one widget to
receive continuous pan updates and a final velocity action. Long presses are
advanced by `tick` so stationary fingers do not depend on motion events. Once
a contact exceeds a tap or hold movement tolerance, returning to its origin
cannot resurrect that gesture. Cancellation is terminal and explicitly
reported.

```rust
let mut gestures = GestureMap::default();
gestures.region(
    WidgetId(42),
    bounds,
    [
        GestureRecognizer::tap(),
        GestureRecognizer::pan(GestureAxis::Horizontal),
        GestureRecognizer::swipe(GestureAxis::Horizontal),
    ],
);

for event in arena.handle(&gestures, contact) {
    // Update plugin-owned state.
}
```

The arena is additive. The original `InteractionState` remains the concise API
for ordinary buttons and sliders, and plugins can always consume raw contacts.
Concurrent contacts already have independent capture; multi-contact
recognizers such as pinch can be layered onto this foundation later.

## Virtualized scrubber

`Scrubber` is a plugin-owned state model for a large, fixed-extent horizontal
collection. `visible_range` and `placements` calculate only visible items plus
a small overscan. `compose` invokes the plugin's item builder only for that
range and returns clipped, positioned retained nodes. A ten-thousand-item data
set therefore does not create ten thousand nodes or textures.

```rust
let node = scrubber.compose(viewport_size, |index, state| Node::Toggle {
    id: WidgetId(1_000 + index as u64),
    label: labels[index].clone(),
    icon: None,
    selected: state.selected || state.highlighted,
    pressed: false,
});
```

Movement and selection are independent policies:

- `Free` permits an arbitrary bounded offset; `SnapToItem` centers the nearest
  item and adds symmetric edge space so the first and last items can center.
- `Continuous` commits selection as the pan crosses items; `OnRelease` exposes
  highlight during movement and commits at the end.

The scrubber supplies its own tap/pan/swipe `GestureTarget`. The caller feeds
the resulting events back through `Scrubber::handle`. A cancelled drag restores
its starting offset, highlight, and selection rather than leaving partial
state behind. Item data and actions remain in the unrestricted plugin process.

## Stationary selection palette

`SelectionPalette` is deliberately separate from `Scrubber`. It pins the
compact activator at its compositor-reported anchor and lays fixed option cells
into the larger side of the expanded region. An anchor near the left edge grows
right; an anchor near the right edge grows left. Its gesture target supports
hold-and-slide with release-to-commit as well as a later tap in a persistent
popover. Tapping the pinned activator requests dismissal.

## Live demonstration

Run the hardware-GPU acceptance test without taking over the physical display:

```bash
./scripts/run-sdk-ui.sh 120
```

The synthetic hold expands the 80-pixel volume item into a 360-pixel fixed
palette. The captured contact transfers into the new arena, slides over
stationary choices, commits one, and collapses on release. A physical short tap
instead opens a persistent palette that accepts a later tap, closes from its
pinned activator or an outside press, and times out after four idle seconds.
The runner validates the Apple M1 renderer, DMA-BUF-only transport, buffer
release, bounded presentation, and approximately 60 composed frames per
second.

For physical input use `./scripts/run-sdk-ui-physical.sh 15`, authorize the
guarded presenter, hold the volume icon, and drag through the expanded items.

## Deliberate next boundaries

- variable item extents and reusable item-view pools;
- inertial scrolling with scheduler-owned bounded animation;
- multi-tap, repeat, pinch, and recognizer dependency/failure relationships;
- collection semantics and a public accessibility/inspector transport;
- reusable controls that exercise nested presentation navigation;
- daemon-owned placement for in-place, named-slot, named-region, and explicit
  full-bar policies.
