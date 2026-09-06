# Host-timed component animation

**Status:** E5.2 implemented and Apple-GPU verified, 2026-09-05.

Sandboxed components describe animation; they do not run a Wasm callback on
every display frame. A `motion` node wraps any retained UI subtree and supplies
a stable animation ID, start and end visual transforms, duration, easing, and
one-shot, looping, or alternating playback. The native host validates that
description, owns its monotonic start time, samples it during GLES painting,
and requests compositor-paced frame callbacks only while visible motion is
active.

This makes animated Canvas2D drawings, icons, labels, controls, and complete
compound widgets use the same scheduler and GPU renderer. Layout, hit testing,
and semantic bounds remain settled while translation, uniform scale, and
opacity change at paint time.

## SDK contract

The Rust component SDK exposes `Animation`, `AnimationPlayback`, `Easing`,
`visual_transform`, and `ViewBuilder::motion`:

```rust
let artwork = canvas.finish(&mut view);
let pulsing = view.motion(
    artwork,
    Animation::new(42, 900)
        .from(visual_transform(0.0, 0.0, 0.98, 0.72))
        .to(visual_transform(0.0, 0.0, 1.02, 1.0))
        .playback(AnimationPlayback::Alternate),
);
```

Animation IDs are scoped to one item. Reusing an ID with the same description
preserves its host-owned phase across input-driven rerenders, size changes, and
theme changes. Reusing it with a changed description intentionally restarts
the animation when the new view is accepted. The same ID and description may
appear in responsive alternatives to keep them synchronized; conflicting
descriptions for one ID in a single view are rejected.

## Scheduling and policy

The component's `render` export is called only when its retained state must be
rebuilt. Native frames resolve and draw that retained tree at the host's
monotonic time. Active motion returns `FrameFlow::Animate`, which asks Wayland
for the next compositor-approved frame callback; a completed one-shot returns
`Wait`. Hidden surfaces stop receiving animation frames, and become current on
the first frame after they are made visible again.

Motion policy travels in the compositor's atomic appearance snapshot, through
`touchbar-client`, into the WIT theme and GLES renderer. `full` samples normally;
`reduced` and `disabled` settle immediately and stop continuous scheduling. An
theme palette may set `motion = "reduced"` or `"disabled"`; changing
that file publishes a new appearance generation just like a color change.

Frame callback cadence is a separate theme and power policy. `animation_hz`
defaults to 60 on external or unknown power; `battery_animation_hz` defaults to
30 on battery. Both accept 1–60, hot reload with the palette, and cap continuous
animation without changing its host-monotonic time base. A theme may explicitly
keep 60 Hz on battery. Reduced and disabled motion remain stronger: they settle
and stop scheduling rather than merely lowering cadence.

The hardware service wakes the OLED for a first new user frame or physical
input, but ordinary animation frames do not reset its idle timer. Thus a
visible animated plugin can run at compositor cadence without preventing the
hardware-owned backlight policy from sleeping the strip.

## Bounds and abuse resistance

- at most 64 unique animations may be reachable in one returned view;
- IDs must be nonzero and descriptions sharing an ID must agree;
- durations are limited to 1–60,000 milliseconds;
- translations are finite and limited to `-4096..4096` pixels;
- scale is finite and limited to `0..8`;
- opacity is finite and limited to `0..1`;
- animation registries retain only IDs reachable from the latest valid view;
- a rejected view cannot mutate the live phase registry;
- component memory, fuel, node, depth, and expanded-node limits still apply.

The first-party Media pack wraps its now-playing Canvas2D waveform in a subtle
alternating pulse. `scripts/test-canvas-component-ui.sh` requires the Canvas
semantic node, then renders 60 DMA-BUF frames with the Apple M1 renderer and
asserts `guest-renders=1` and zero compositor rejects. Unit tests cover stable
phase retention, deliberate restart, conflicting IDs, non-finite transforms,
quota abuse, loop sampling, and reduced-motion settlement.

The WIT and Wayland appearance schema were updated directly as fresh v1. There
is no compatibility adapter, deprecated representation, or migration path.
