# Native GLES Plugin

A native plugin that takes a real GLES context and renders an animated fragment
shader onto the Touch Bar.

This is the **unrestricted escape hatch**. A native plugin is an ordinary
executable running with your full user authority. None of the sandbox
guarantees that apply to WebAssembly component packs apply here: it can read
your files, open sockets, and talk to any service you can.

Prefer [`examples/drawing-component-plugin`](../drawing-component-plugin)
unless you genuinely need driver-level access. Canvas2D and validated WGSL
effects give sandboxed components real GPU drawing without any of this
authority, and they are the default path for anything you intend to share.

## What it shows

`touchbar-client` removes the Wayland and EGL boilerplate. Implement the
`Application` trait and you get a current GLES3 context, compositor-driven
frame callbacks, and buffer submission:

```rust
impl Application for ShaderDemo {
    fn configured(&mut self, graphics: &Graphics, config: SurfaceConfig) -> Result<()> {
        // Compile a program against graphics.gl(); called on resize too.
    }

    fn render(&mut self, graphics: &Graphics, frame: FrameInfo) -> Result<FrameFlow> {
        // Draw, then say whether you want another frame.
        Ok(FrameFlow::Animate)
    }
}
```

Returning `FrameFlow::Wait` instead of `Animate` lets a static scene sleep: the
compositor stops scheduling frames until something changes. The demo also
consumes the live `AppearanceSnapshot`, passing background, accent, and
foreground into the shader as uniforms, so a theme change recolors it the same
way it recolors every built-in control.

## Run it

Headless, against the private Wayland display:

```bash
cargo run -p touchbar-gl-demo -- --frames 600
```

Useful flags: `--variant 0..1` selects between two shaders, `--backdrop`
requests the full-width backdrop surface instead of an item slot,
`--static` renders one frame and waits, and `--require-hardware` fails rather
than falling back when no GPU path is available.

Several acceptance runners drive this binary — `scripts/run-m1.sh`,
`run-m2.sh`, `run-m2-dual.sh`, and `run-sdk-ui.sh` among them — so it doubles
as the reference client for the compositor's frame contract.
