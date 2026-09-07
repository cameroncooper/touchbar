//! Custom GPU drawing from inside the WebAssembly sandbox.
//!
//! A sandboxed component never receives a graphics driver API. It describes
//! what it wants drawn and trusted native code validates, resolves, and
//! rasterizes that description with the same GLES renderer used by every
//! built-in control. There are two ways to describe it:
//!
//! * [`Canvas2d`] — retained vector commands. Rectangles, lines, polylines,
//!   circles, gradients, stroked and filled Bézier paths, and clipped text.
//!   This is the default advanced-visual path for portable plugins.
//! * [`GpuEffect`] — a small straight-line WGSL body evaluated per pixel, for
//!   animated backdrops and visualizers.
//!
//! Both resolve theme colors as *roles* rather than literal values, so a
//! palette change recolors the drawing without the component re-rendering or
//! the shader recompiling.
//!
//! Native process plugins can still take a raw `glow::Context` through
//! `touchbar-client`; see `crates/touchbar-gl-demo`. That path is an
//! unrestricted escape hatch and is not what most plugins should use.

use touchbar_component_sdk::kit::{
    Canvas2d, ColorRole, GpuEffect, InputEvent, Path2d, TextAlign, ViewBuilder, theme_paint,
    theme_paint_with_opacity,
};
use touchbar_component_sdk::{
    Guest, HostEvent, Item, PresentationEvent, RenderRequest, Update, View,
};

struct Drawing;

impl Guest for Drawing {
    fn items() -> Vec<Item> {
        vec![
            item("canvas", "Canvas Drawing"),
            item("effect", "Shader Effect"),
            item("combined", "Canvas Over Effect"),
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        // The assigned width is whatever the profile gave this item; the
        // canvas view box scales into it, so nothing here predicts a slot size.
        let width = request.viewport.width;
        Ok(match request.item_id.as_str() {
            "canvas" => canvas_item(width),
            "effect" => effect_item(width),
            "combined" => combined_item(width),
            other => return Err(format!("unknown item {other}")),
        })
    }

    // This example is purely visual: nothing here reacts to touch.
    fn handle_event(_event: InputEvent) -> Result<Update, String> {
        Ok(idle())
    }

    fn handle_presentation_event(_event: PresentationEvent) -> Result<Update, String> {
        Ok(idle())
    }

    // A theme change arrives here. Returning `rerender: true` rebuilds the
    // retained tree; a running effect would pick up the new palette either
    // way, because theme roles are uniforms resolved at paint time.
    fn handle_host_event(_event: HostEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: true,
            presentation: None,
        })
    }
}

fn idle() -> Update {
    Update {
        rerender: false,
        presentation: None,
    }
}

fn item(id: &str, label: &str) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
    }
}

/// Vector drawing. The view box is a fixed 100x20 coordinate space that scales
/// into whatever bounds layout assigns, which is why this function only needs
/// the width to decide how much detail to show.
fn canvas_item(width: f32) -> View {
    let mut b = ViewBuilder::new();
    let mut canvas = Canvas2d::new("Signal trace with a marked peak", 100.0, 20.0);

    // A gradient bed. Paints name theme roles, so this tracks the palette.
    canvas.fill_linear_gradient_rect(
        0.0,
        8.5,
        100.0,
        3.0,
        1.5,
        (0.0, 10.0),
        (100.0, 10.0),
        theme_paint_with_opacity(ColorRole::Track, 0.85),
        theme_paint_with_opacity(ColorRole::Control, 0.30),
    );

    // A stroked Bézier path.
    let mut trace = Path2d::new();
    trace
        .move_to((0.0, 10.0))
        .cubic_to((14.0, 2.0), (24.0, 18.0), (36.0, 10.0))
        .cubic_to((50.0, 1.0), (60.0, 19.0), (72.0, 10.0))
        .cubic_to((84.0, 3.0), (92.0, 16.0), (100.0, 10.0));
    canvas.stroke_path(trace, 1.4, theme_paint(ColorRole::Foreground));

    // A filled path: a triangular marker on the trace.
    let mut marker = Path2d::new();
    marker
        .move_to((33.0, 6.0))
        .line_to((39.0, 10.0))
        .line_to((33.0, 14.0))
        .close();
    canvas.fill_path(marker, theme_paint(ColorRole::Accent));
    canvas.fill_circle((36.0, 10.0), 1.6, theme_paint(ColorRole::OnAccent));

    // Text is clipped to the canvas leaf like every other command. Only draw
    // it when the item is wide enough for it to be legible.
    if width >= 240.0 {
        canvas.text(
            2.0,
            1.0,
            96.0,
            5.0,
            "PEAK 36%",
            4.5,
            theme_paint(ColorRole::Muted),
            TextAlign::Leading,
        );
    }

    let node = canvas.finish(&mut b);
    b.finish(node)
}

/// A procedural effect. The source is not arbitrary WGSL: it is a straight-line
/// list of `let` bindings that must end by defining `color`. The host parses,
/// validates, and translates it, then supplies `uv`, `size`, `time`, up to
/// eight scalar `params`, and every theme role as a read-only uniform.
///
/// `animate` declares a period the host owns. Reduced-motion preferences and
/// hidden surfaces freeze `time` and stop scheduling frames, so honoring
/// accessibility settings is automatic rather than the plugin's job.
fn effect_item(_width: f32) -> View {
    let mut b = ViewBuilder::new();
    let node = b.shader_effect(
        GpuEffect::new(
            9_001,
            "Theme-reactive interference bands",
            r#"
let centered = uv - vec2<f32>(0.5, 0.5);
let radius = length(centered * vec2<f32>(1.0, 3.0));
let ring = sin(radius * 26.0 - time * 6.2831853);
let band = 1.0 - smoothstep(0.0, 0.55, abs(ring));
let sweep = 0.5 + 0.5 * sin(time * 6.2831853 + centered.x * 4.0);
let bed = mix(background, control, 0.35);
let color = mix(bed, accent, band * (0.12 + sweep * 0.22));
"#,
        )
        .preferred_size(320.0, 60.0)
        .animate(4_000),
    );
    b.finish(node)
}

/// The two compose: an effect underneath, vector work on top, in one layer.
fn combined_item(width: f32) -> View {
    let mut b = ViewBuilder::new();

    let backdrop = b.shader_effect(
        GpuEffect::new(
            9_002,
            "Ambient gradient wash",
            r#"
let centered = uv - vec2<f32>(0.5, 0.5);
let falloff = 1.0 - smoothstep(0.0, 0.7, length(centered * vec2<f32>(0.6, 2.0)));
let drift = 0.5 + 0.5 * sin(time * 6.2831853 + uv.x * 3.0);
let color = mix(background, accent, falloff * 0.10 * (0.4 + drift * 0.6));
"#,
        )
        .preferred_size(width.max(80.0), 60.0)
        .animate(9_000),
    );

    let mut canvas = Canvas2d::new("Level bars", 100.0, 20.0);
    for (index, height) in [6.0_f32, 11.0, 8.0, 14.0, 9.0, 12.0, 7.0]
        .iter()
        .enumerate()
    {
        let x = 6.0 + index as f32 * 13.0;
        let role = if *height > 10.0 {
            ColorRole::Accent
        } else {
            ColorRole::Muted
        };
        canvas.fill_rect(x, 17.0 - height, 7.0, *height, 1.5, theme_paint(role));
    }
    let bars = canvas.finish(&mut b);

    let node = b.layer(vec![backdrop, bars]);
    b.finish(node)
}

touchbar_component_sdk::export!(Drawing);
