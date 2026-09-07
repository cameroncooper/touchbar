# Drawing Component Demo

Custom GPU drawing from inside the WebAssembly sandbox.

A sandboxed component never receives a graphics driver API. It describes what it
wants drawn; trusted native code validates that description, resolves theme
colors and layout, and rasterizes it with the same GLES renderer used by every
built-in control.

**This package requests no capabilities at all.** Custom drawing needs none — a
plugin can be visually rich while remaining unable to touch the filesystem, the
network, or D-Bus.

## The two sandboxed paths

| Item | Path | Use it for |
|---|---|---|
| `canvas` | `Canvas2d` — retained vector commands | charts, meters, indicators, anything with structure |
| `effect` | `GpuEffect` — a validated WGSL body | animated backdrops, visualizers, glows |
| `combined` | both, layered | the common real case |

### Canvas2D

Rectangles, lines, polylines, circles, two-stop gradients, stroked and filled
Bézier paths, and clipped text, in a view box that scales into whatever bounds
layout assigns. Authors never predict a slot width.

### Validated effects

The source is not arbitrary WGSL. It is a straight-line list of `let` bindings
that must end by defining `color`. The host parses, validates, and translates
it, then supplies `uv`, `size`, `time`, up to eight scalar parameters, and every
theme role as a read-only uniform.

`animate(period_ms)` hands the clock to the host. Reduced-motion preferences and
hidden surfaces freeze `time` and stop scheduling frames, so honoring
accessibility settings is automatic rather than the plugin's job — the replay
scenario in `tests/` exercises exactly that.

### Colors are roles, not values

Every paint names a `ColorRole`. Theme values are uniforms resolved at paint
time, so a palette change recolors a drawing — and a *running* shader — with no
re-render and no recompilation.

## Try it

```bash
touchbarctl plugin build   --package examples/drawing-component-plugin
touchbarctl plugin test    --package examples/drawing-component-plugin
touchbarctl plugin replay  --package examples/drawing-component-plugin \
  --scenario examples/drawing-component-plugin/tests/replay.json \
  --screenshots /tmp/drawing
```

`plugin dev` opens the same items in the onscreen simulator.

## The third path: raw GLES

Native process plugins can take a real `glow::Context` through
`touchbar-client` and submit their own GLES draw calls — see
[`crates/touchbar-gl-demo`](../../crates/touchbar-gl-demo) for an animated
fragment shader built that way.

That path is an intentionally unrestricted escape hatch: a native plugin is
ordinary code with your full user authority, and none of the sandbox guarantees
above apply to it. Prefer Canvas2D or a validated effect unless you genuinely
need driver-level access.
