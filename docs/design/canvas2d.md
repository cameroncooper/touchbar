# Sandboxed Canvas2D

**Status:** E5.1 and E5.3 implemented and Apple-GPU verified, 2026-09-05.

Canvas2D gives WebAssembly Component plugins a useful custom-drawing surface
without placing a graphics driver API inside the sandbox. The component returns
a bounded retained command list through the existing WIT `view`; trusted native
code validates it, resolves live theme colors and layout, and draws it with the
same GLES renderer as ordinary UI-kit controls.

Native process plugins still have direct GLES access through
`touchbar-client`. Canvas2D is the default advanced-visual path for portable,
sandboxed plugins.

## Contract

A canvas node contains:

- an accessible semantic label;
- a finite positive local view-box width and height;
- an ordered list of solid or two-stop linear-gradient rectangles,
  round-capped lines, polylines, bounded stroked Bézier paths, simple filled
  Bézier paths, filled circles, and clipped text commands.

The view box scales into whatever bounds Taffy and the compositor assign. This
keeps complex drawings responsive without forcing their authors to predict a
slot width. All output is clipped to the resolved canvas leaf. Geometry is
visual only; authors wrap a canvas in a normal `Pressable` or layer it with
standard controls when interaction is needed, keeping one shared hit-testing
model.

Paint is either a semantic `color-role` or literal straight-alpha RGBA, plus an
additional opacity. Semantic paint is resolved from every new appearance
snapshot, so theme changes recolor cached component state without requiring the
plugin to copy palette values into commands. Literal color is intended for
meaningful data colors and remains bounded to finite `[0, 1]` channels. The GLES
renderer premultiplies only at draw time and uses the project's standard
premultiplied-alpha blend mode.

The Rust SDK exposes `Canvas2d`, `theme_paint`,
`theme_paint_with_opacity`, and `rgba_paint`:

```rust
let mut canvas = Canvas2d::new("CPU history", 100.0, 20.0);
canvas
    .fill_rect(
        0.0,
        8.0,
        100.0,
        4.0,
        2.0,
        theme_paint_with_opacity(ColorRole::Track, 0.7),
    )
    .polyline(
        [(0.0, 15.0), (50.0, 3.0), (100.0, 12.0)],
        1.5,
        theme_paint(ColorRole::Accent),
    );
let graph = canvas.finish(&mut view_builder);
```

`Path2d` provides `move_to`, `line_to`, `quadratic_to`, `cubic_to`, and `close`.
`Canvas2d::stroke_path` converts that grammar into a host-bounded curve, while
`fill_linear_gradient_rect` accepts independent semantic or literal paints for
its two endpoints. Theme roles remain live at both gradient stops.
`Canvas2d::fill_path` accepts one closed simple subpath. The host rejects holes,
self-intersections, repeated vertices, open paths, and degenerate area, then
triangulates the fixed-flattened polygon. This intentionally narrow contract is
predictable for icons, chart areas, badges, pointers, and decorative shapes.

## GPU execution

Rectangles and circles use the existing antialiased signed-distance shape
shader. Arbitrary lines and polylines use a dedicated GLES3 signed-distance
segment shader with round caps. Each segment is scissored to its conservative
pixel bounds and the enclosing canvas clip, avoiding full-surface fragment work.
Linear gradients use a dedicated signed-distance rounded-rectangle shader,
interpolate straight-alpha stops along a plugin-selected local axis, and emit
premultiplied output. Quadratic
and cubic paths are validated and deterministically flattened by trusted host
code into the same round-capped GPU segments; guest-provided tessellation sizes
or allocations are never accepted.
Filled paths use the same fixed flattening, host-owned simple-polygon validation,
and bounded ear clipping. Their triangles are uploaded to one transient GLES
buffer and submitted in one draw per path; neither raw guest indices nor a
guest-selected vertex count reaches the driver.
Text uses the existing bounded Cosmic Text cache and image shader. No canvas
command performs CPU framebuffer rasterization or crosses into
`touchbar-sessiond`; the component host submits the final DMA-BUF as before.

The first-party Media pack uses Canvas2D for its theme-aware playback waveform,
progress fill, and cursor. `scripts/test-canvas-component-ui.sh` runs that exact
Wasm component through the supervisor and host, validates the retained scene,
and proves an Apple M1 renderer plus DMA-BUF frame with zero compositor rejects.
`touchbarctl plugin test --package plugins/media` separately renders every item
at 80, 160, 320, 1004, and 2008 pixels plus its declared presentation widths.

## Resource limits and validation

Canvas input is untrusted. Validation occurs before Taffy, text shaping, scene
allocation, or GLES:

- at most 512 expanded canvas commands per returned view;
- at most 2,048 expanded polyline points total and 256 points per polyline;
- at most 256 raw path segments total and 128 per path; quadratic and cubic
  segments use fixed 8- and 12-step host flattening within the shared 2,048
  point budget;
- a filled path has one closed simple polygon, at most 258 flattened vertices
  and 256 triangles; the complete view has at most 512 filled triangles;
- coordinates are finite and bounded to `-4096..8192` view-box units;
- view-box dimensions, shape dimensions, radii, stroke widths, text sizes,
  colors, and opacity all have finite host bounds;
- canvas text shares the existing 16 KiB complete-view text budget;
- shared/cyclic arena references remain subject to the existing node, expanded
  node, and depth budgets;
- invalid input rejects the entire render rather than dropping only the hostile
  command.

These budgets are host policy, not fields controlled by the package. The WIT
surface was updated directly as fresh v1 code and every checked-in component was
rebuilt; there is no old command format, adapter, or migration path.

## Next E5 slices

Canvas2D can be wrapped in a bounded
[host-timed animation](host-timed-animation.md), and manifest-declared
[package images and symbolic SVG](package-assets.md) cross the sandbox as
verified sealed bytes. The separate
[validated shader-effect tier](validated-effects.md) now builds on the same
semantic-paint, clipping, scheduling, and quota model without exposing raw
GLES to a sandboxed component.
