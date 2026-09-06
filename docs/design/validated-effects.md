# Validated GPU effects

Sandboxed components need a procedural GPU path for animated backdrops,
visualizers, meters, touch feedback, and shader-gallery plugins. They do not
receive a GLES context or submit driver source directly. Native plugins remain
the explicit unrestricted escape hatch.

## Fresh-v1 contract

An `effect` node is a normal dynamically sized retained leaf. It carries a
stable nonzero ID, an accessibility label, a small straight-line WGSL body,
zero to eight finite scalar parameters, opacity, preferred size, and an
optional animation period. The body may contain `let` bindings and must define
one final `color: vec4<f32>` value. The host supplies these read-only names:

- `uv`, normalized across the assigned bounds, and `size` in logical pixels;
- `time`, a host-owned monotonic value wrapped by the declared period;
- `params0` and `params1`, each a four-scalar vector;
- `background`, `control`, `control_pressed`, `track`, `foreground`, `muted`,
  `accent`, `destructive`, `on_accent`, and `on_destructive`.

Theme values are uniforms resolved at paint time. A theme change therefore
updates a running effect without shader recompilation; the ordinary component
appearance callback may still rebuild the surrounding retained tree.
The fixed wrapper rejects non-finite output, clamps straight RGBA, applies
node/ancestor opacity, and emits premultiplied color for the compositor.

An absent animation period freezes `time` at zero and does not schedule
frames. A period from 100 ms through 60 seconds advances at the compositor's
output cadence, currently up to 60 Hz. Reduced or disabled motion freezes time
and stops frame scheduling. Hidden surfaces likewise receive no frames.

## Trust boundary and limits

The Component host—not the guest—wraps, parses, validates, translates, and
caches the program. [Naga](https://github.com/gfx-rs/wgpu/tree/trunk/naga)
parses WGSL, performs full semantic validation with no optional capabilities,
and emits GLSL ES 3.00. Before any driver compilation,
the host additionally requires:

- at most 4 KiB of printable source and eight unique programs per component;
- no wrapper delimiters, directives, comments, or reserved `otb_` names;
- exactly the fixed fragment entry point, uniform block, and final return;
- no guest functions, constants, overrides, mutable locals, or extra globals;
- at most 64 types, 256 expressions, and 64 straight-line statements;
- only scalar/vector construction, fixed indexing, swizzles, arithmetic,
  comparisons, selection, derivatives, and a curated floating-point math set;
- no loops, branches, switches, calls, stores, discard, textures, storage,
  atomics, subgroup/workgroup operations, ray operations, or dynamic indexing;
- at most four effect nodes in one accepted view.

The driver sees only Naga-generated, already validated GLSL ES, never the
component's unparsed WGSL module. A renderer keeps
at most eight compiled programs and one fixed-size std140 uniform buffer. Each
draw is a clipped full-screen triangle whose scissor is intersected with the
effect's assigned bounds and all parent clips. A source change under the same
stable ID replaces the program and restarts that effect's phase; an unchanged
description preserves phase across guest rerenders.

This tier is intentionally not general compute, raw OpenGL, arbitrary WGSL, or
a texture API. Package images, Canvas2D, retained controls, and unrestricted
native processes remain separate tools. Future texture sampling can be added
as a new bounded contract over sealed logical asset IDs without weakening this
one.

## Acceptance

The milestone is complete when adversarial host tests reject every forbidden
control-flow/resource family, valid source reaches a real GLES program on the
Apple M1 renderer, a themed first-party component produces 60 DMA-BUF frames
from one Wasm render call, reduced motion settles to one frame, and the broad
packaging and sandbox suites remain green.
