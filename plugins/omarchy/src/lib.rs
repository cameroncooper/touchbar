//! Omarchy on the Touch Bar.
//!
//! `screensaver` adapts the pixel field from omarchy.org to the 2008x60 Touch
//! Bar. The site wordmark is represented by its native 81x19 bitmap and shares
//! one grid with a sparse, dithered field. A light crosses the long axis while
//! rings expand from the centre, lifting cells through the theme's accent
//! tones. Touching the strip retracts it — the field fades, the mark lifts
//! away, and an accent hairline arrives in its place.
//!
//! Two things make it fit the hardware rather than a phone screen. The strip is
//! 2008x60, a 33:1 ribbon, so the site's roaming light becomes a long sweep and
//! its circular pulse becomes a pair of expanding fronts. And it is OLED, so
//! the mark breathes and drifts slowly around centre instead of holding one
//! intensity and position forever.
//!
//! Every color is a semantic role. The component never learns the theme's name
//! or its hex values — the host resolves roles at paint time, so `omarchy theme
//! set` recolors both the drawing and the running shader without this code
//! rendering again. `palette` exists to make that visible in one glance.
//!
//! The drawing component requests no broker access. A separate sandboxed
//! appearance-provider worker watches Omarchy and publishes semantic colors;
//! it never exposes palette files or literal values to this visual component.

use std::sync::atomic::{AtomicBool, Ordering};

use touchbar_component_sdk::bindings::touchbar::plugin::ui::MotionPolicy;
use touchbar_component_sdk::kit::{
    Animation, AnimationPlayback, Canvas2d, ColorRole, Easing, GpuEffect, InputEvent, InputKind,
    NodeId, Pressable, TextAlign, ViewBuilder, theme_paint, theme_paint_with_opacity,
    visual_transform,
};
use touchbar_component_sdk::{
    Guest, HostEvent, Item, PresentationEvent, RenderRequest, Update, View,
};

const WAKE_WIDGET: u64 = 1;

const FIELD_EFFECT: u64 = 1;
const LIGHT_EFFECT: u64 = 2;
const RIPPLE_EFFECT: u64 = 3;
const DRIFT: u64 = 1;
const BREATH: u64 = 2;
const WAKE_FIELD: u64 = 3;
const WAKE_MARK: u64 = 4;
const WAKE_LINE: u64 = 5;

/// The field loops exactly after one minute: one light pass and four ripples.
const FIELD_PERIOD_MS: u32 = 60_000;
/// The mark breathes independently of the field, slowly enough to stay calm.
const BREATH_PERIOD_MS: u32 = 10_000;
/// A full there-and-back drift. The host caps a single animation at 60s.
const DRIFT_PERIOD_MS: u32 = 60_000;

const WORDMARK_WIDTH: usize = 81;
const WORDMARK_HEIGHT: usize = 19;

/// The bitmap used by the homepage effect. Keeping it as rows rather than an
/// image lets every horizontal run resolve through a live semantic theme paint.
const WORDMARK: [&str; WORDMARK_HEIGHT] = [
    "000000000000000001110000000000000000000000000000000000000000000000000000000000000",
    "001111100000011111111111000000111111100001111111000011111110000100010000001000100",
    "011111110000111111111111100001111111100011111111000111111110001100011000011000110",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001100011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001000011100011100111000111",
    "111000111001110001110001110011100011100111000111001110000000011100011100111000111",
    "111000111001110001110001110111111111101111111110001110000000111111111110111111111",
    "111000111001110001110001110111111111101111111100001110000001111111111100111111111",
    "111000111001110001110001110011100011100111000000001110000000011100011100000000111",
    "111000111001110001110001110011100011101111111111001110001000011100011100011000111",
    "111000111001110001110001110011100011101111111111001110001100011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "011111110000110001110001100011100011000111000111001111111100011100011000011111110",
    "001111100000010001110001000011100010000111000111001111111000011100010000001111100",
    "000000000000000000000000000000000000000111000110000000000000000000000000000000000",
    "000000000000000000000000000000000000000111000100000000000000000000000000000000000",
];

/// The quiet layer of the homepage field. `params0.x` is the shared
/// cell pitch and `params0.yz` is the wordmark's grid origin. Each fragment is
/// quantized into that grid, so the field stays visibly pixelated at 2008x60.
///
const FIELD_SOURCE: &str = r#"
let grid_uv = (uv * size - params0.yz) / vec2<f32>(max(params0.x, 0.75));
let cell = floor(grid_uv);
let inside = fract(grid_uv);
let tile = step(inside.x, 0.80) * step(inside.y, 0.80);
let hash = fract(sin(dot(cell, vec2<f32>(12.9898, 78.233))) * 43758.5453);
let phase = time * 0.104719755;
let energy = hash * 0.48 + (0.5 + 0.5 * sin(cell.x * 0.071 + cell.y * 0.73 + phase * 3.0)) * 0.30 + (0.5 + 0.5 * sin(cell.x * 0.037 - cell.y * 0.41 - phase * 2.0)) * 0.22;
let dim_color = mix(background, accent, 0.23);
let mid_color = mix(background, accent, 0.55);
let tier = mix(dim_color, mid_color, step(0.84, energy));
let color = vec4<f32>(tier.rgb, tile * step(0.68, energy));
"#;

/// The site's roaming light stretched into a slow pass along the Touch Bar.
/// It is separate from the quiet field so both stay inside the validated
/// effect grammar's deliberately small statement and expression budgets.
const LIGHT_SOURCE: &str = r#"
let grid_uv = (uv * size - params0.yz) / vec2<f32>(max(params0.x, 0.75));
let cell = floor(grid_uv);
let inside = fract(grid_uv);
let tile = step(inside.x, 0.80) * step(inside.y, 0.80);
let hash = fract(sin(dot(cell, vec2<f32>(39.3468, 11.135))) * 24634.6345);
let phase = time * 0.104719755;
let delta = vec2<f32>((uv.x - (fract(time * 0.016666667 + 0.5) * 1.32 - 0.16)) * size.x / 118.0, (uv.y - (0.5 + 0.17 * sin(phase * 2.0))) * 3.4);
let energy = exp(-dot(delta, delta));
let visible = tile * step(0.14 + hash * 0.66, energy);
let tier = mix(accent, mix(accent, foreground, 0.70), step(0.76, energy));
let color = vec4<f32>(tier.rgb, visible * (0.42 + energy * 0.58));
"#;

/// Four centre pulses per field period become paired fronts on the 33:1 panel.
const RIPPLE_SOURCE: &str = r#"
let grid_uv = (uv * size - params0.yz) / vec2<f32>(max(params0.x, 0.75));
let cell = floor(grid_uv);
let inside = fract(grid_uv);
let tile = step(inside.x, 0.80) * step(inside.y, 0.80);
let hash = fract(sin(dot(cell, vec2<f32>(19.19, 73.73))) * 19341.731);
let phase = fract(time * 0.066666667);
let distance = length(vec2<f32>((uv.x * size.x - size.x * 0.5) * 0.70, (uv.y * size.y - size.y * 0.5) * 4.0));
let energy = (1.0 - smoothstep(0.0, 24.0, abs(distance - phase * size.x * 0.42))) * (1.0 - phase);
let visible = tile * step(0.24 + hash * 0.52, energy);
let tier = mix(mix(background, accent, 0.55), accent, step(0.68, energy));
let color = vec4<f32>(tier.rgb, visible * 0.72);
"#;

/// Whether the strip is mid-handback. In a real session the wake is a profile
/// switch driven by seat activity, not a press; the press is how the simulator
/// gets to see it.
static AWAKE: AtomicBool = AtomicBool::new(false);

struct Omarchy;

impl Guest for Omarchy {
    fn items() -> Vec<Item> {
        vec![
            item("screensaver", "Screensaver"),
            item("palette", "Theme Palette"),
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        let width = request.viewport.width;
        let height = request.viewport.height;
        Ok(match request.item_id.as_str() {
            "screensaver" => screensaver(width, height, request.theme.motion),
            "palette" => palette(width),
            other => return Err(format!("unknown item {other}")),
        })
    }

    fn handle_event(event: InputEvent) -> Result<Update, String> {
        if event.widget_id == WAKE_WIDGET && event.kind == InputKind::Activated {
            AWAKE.fetch_xor(true, Ordering::Relaxed);
            return Ok(rerender());
        }
        Ok(idle_update())
    }

    fn handle_presentation_event(_event: PresentationEvent) -> Result<Update, String> {
        Ok(idle_update())
    }

    // A theme revision lands here. Rebuilding is cheap and keeps the retained
    // tree honest, though a running effect would pick the palette up either
    // way: roles are uniforms resolved at paint time.
    fn handle_host_event(_event: HostEvent) -> Result<Update, String> {
        Ok(rerender())
    }
}

fn screensaver(width: f32, height: f32, motion: MotionPolicy) -> View {
    let mut b = ViewBuilder::new();
    let animated = motion == MotionPolicy::Full;
    let width = width.max(1.0);
    let height = height.max(1.0);
    let geometry = wordmark_geometry(width, height);

    // An opaque bed in `background`, so the field has somewhere to fade to
    // when the strip is handed back.
    let mut ground = Canvas2d::new("Ground", width, height);
    ground.fill_rect(
        0.0,
        0.0,
        width,
        height,
        0.0,
        theme_paint(ColorRole::Background),
    );
    let bed = ground.finish(&mut b);
    let effect_parameters = [geometry.cell, geometry.left, geometry.top];
    let quiet = b.shader_effect(
        GpuEffect::new(FIELD_EFFECT, "Omarchy pixel field", FIELD_SOURCE)
            .parameters(effect_parameters)
            .preferred_size(width, height)
            .animate(FIELD_PERIOD_MS),
    );
    let light = b.shader_effect(
        GpuEffect::new(LIGHT_EFFECT, "Roaming pixel light", LIGHT_SOURCE)
            .parameters(effect_parameters)
            .preferred_size(width, height)
            .animate(FIELD_PERIOD_MS),
    );
    let ripple = b.shader_effect(
        GpuEffect::new(RIPPLE_EFFECT, "Expanding pixel ripple", RIPPLE_SOURCE)
            .parameters(effect_parameters)
            .preferred_size(width, height)
            .animate(FIELD_PERIOD_MS),
    );
    let field = b.layer(vec![quiet, light, ripple]);
    let scene = if AWAKE.load(Ordering::Relaxed) {
        waking(&mut b, field, width, height, geometry, animated)
    } else {
        resting(&mut b, field, width, height, geometry, animated)
    };
    let content = b.layer(vec![bed, scene]);
    // Full-bleed and silent under touch: a screensaver that flashes a control
    // background when you wake it has already broken the illusion.
    let root = b.pressable(
        WAKE_WIDGET,
        "Wake the Touch Bar",
        content,
        Pressable {
            background: None,
            pressed_background: None,
            padding: 0.0,
            ..Pressable::plain()
        },
    );
    b.finish(root)
}

/// Idle: a five-band pixel wordmark breathes and drifts over the animated grid.
/// Under reduced motion, both the shader clock and retained motion hold still.
fn resting(
    b: &mut ViewBuilder,
    field: NodeId,
    width: f32,
    height: f32,
    geometry: WordmarkGeometry,
    animated: bool,
) -> NodeId {
    let mark = pixel_wordmark(b, width, height, geometry);
    if !animated {
        return b.layer(vec![field, mark]);
    }
    let breath = b.motion(
        mark,
        Animation::new(BREATH, BREATH_PERIOD_MS)
            .from(visual_transform(0.0, 0.0, 1.0, 0.78))
            .to(visual_transform(0.0, 0.0, 1.0, 1.0))
            .easing(Easing::EaseInOut)
            .playback(AnimationPlayback::Alternate),
    );
    let reach = drift(width);
    let drifting = b.motion(
        breath,
        Animation::new(DRIFT, DRIFT_PERIOD_MS)
            .from(visual_transform(-reach, 0.0, 1.0, 1.0))
            .to(visual_transform(reach, 0.0, 1.0, 1.0))
            .easing(Easing::Linear)
            .playback(AnimationPlayback::Alternate),
    );
    b.layer(vec![field, drifting])
}

/// Waking: the field falls away, the mark lifts and dissolves, and a hairline
/// in `accent` arrives — the strip handing itself back to the bar.
fn waking(
    b: &mut ViewBuilder,
    field: NodeId,
    width: f32,
    height: f32,
    geometry: WordmarkGeometry,
    animated: bool,
) -> NodeId {
    let mark = pixel_wordmark(b, width, height, geometry);
    let mut rule = Canvas2d::new("Handback rule", width, height);
    let rule_height = geometry.cell.max(1.0).min(3.0);
    rule.fill_rect(
        0.0,
        ((height - rule_height) * 0.5).floor(),
        width,
        rule_height,
        0.0,
        theme_paint(ColorRole::Accent),
    );
    let hairline = rule.finish(b);
    if !animated {
        return b.layer(vec![hairline]);
    }
    let falling = b.motion(
        field,
        Animation::new(WAKE_FIELD, 420)
            .from(visual_transform(0.0, 0.0, 1.0, 1.0))
            .to(visual_transform(0.0, 0.0, 1.0, 0.0))
            .easing(Easing::EaseInOut),
    );
    let lifting = b.motion(
        mark,
        Animation::new(WAKE_MARK, 300)
            .from(visual_transform(0.0, 0.0, 1.0, 1.0))
            .to(visual_transform(0.0, -8.0, 1.06, 0.0))
            .easing(Easing::EaseInOut),
    );
    let arriving = b.motion(
        hairline,
        Animation::new(WAKE_LINE, 520)
            .from(visual_transform(0.0, 0.0, 1.0, 0.0))
            .to(visual_transform(0.0, 0.0, 1.0, 1.0))
            .easing(Easing::EaseInOut),
    );
    b.layer(vec![falling, lifting, arriving])
}

#[derive(Clone, Copy)]
struct WordmarkGeometry {
    cell: f32,
    left: f32,
    top: f32,
}

/// Fit whole grid cells to the strip. A full-height Touch Bar gets a 3px pitch;
/// narrow slots shrink only when the 81-column wordmark would no longer fit.
fn wordmark_geometry(width: f32, height: f32) -> WordmarkGeometry {
    let height_pitch = (height / (WORDMARK_HEIGHT as f32 + 1.0)).floor().max(1.0);
    let width_pitch = width / (WORDMARK_WIDTH as f32 + 4.0);
    let cell = height_pitch.min(width_pitch).max(0.75);
    let mark_width = cell * WORDMARK_WIDTH as f32;
    let mark_height = cell * WORDMARK_HEIGHT as f32;
    WordmarkGeometry {
        cell,
        left: ((width - mark_width) * 0.5).floor(),
        top: ((height - mark_height) * 0.5).floor(),
    }
}

/// Draw contiguous horizontal runs from the site's bitmap. Opacity over the
/// opaque bed produces the dim and mid bands; foreground overlays lift the top
/// seven rows from accent to the site's hover and crest tones.
fn pixel_wordmark(
    b: &mut ViewBuilder,
    width: f32,
    height: f32,
    geometry: WordmarkGeometry,
) -> NodeId {
    let mut canvas = Canvas2d::new("Pixelated Omarchy wordmark", width, height);
    for (row_index, row) in WORDMARK.iter().enumerate() {
        let (accent_opacity, lift_opacity) = match row_index {
            0..=4 => (1.0, 0.62),
            5..=6 => (1.0, 0.34),
            7..=10 => (1.0, 0.0),
            11..=13 => (0.66, 0.0),
            _ => (0.34, 0.0),
        };
        let bytes = row.as_bytes();
        let mut column = 0;
        while column < WORDMARK_WIDTH {
            if bytes[column] != b'1' {
                column += 1;
                continue;
            }
            let start = column;
            while column < WORDMARK_WIDTH && bytes[column] == b'1' {
                column += 1;
            }
            let x = geometry.left + start as f32 * geometry.cell;
            let y = geometry.top + row_index as f32 * geometry.cell;
            let run_width = (column - start) as f32 * geometry.cell;
            canvas.fill_rect(
                x,
                y,
                run_width,
                geometry.cell,
                0.0,
                theme_paint_with_opacity(ColorRole::Accent, accent_opacity),
            );
            if lift_opacity > 0.0 {
                canvas.fill_rect(
                    x,
                    y,
                    run_width,
                    geometry.cell,
                    0.0,
                    theme_paint_with_opacity(ColorRole::Foreground, lift_opacity),
                );
            }
        }
    }
    canvas.finish(b)
}

/// Drift proportional to the space available, capped so the mark never leaves
/// a narrow slot. The field itself remains full-bleed while the mark shifts
/// around centre by a few grid cells to reduce a fixed OLED exposure.
fn drift(width: f32) -> f32 {
    (width * 0.012).clamp(0.0, 24.0)
}

/// Every semantic role the host resolves, side by side. Point it at a strip,
/// run `omarchy theme set`, and the provider either works or visibly does not.
fn palette(width: f32) -> View {
    let mut b = ViewBuilder::new();
    let roles = [
        (ColorRole::Background, "bg"),
        (ColorRole::Control, "control"),
        (ColorRole::Track, "track"),
        (ColorRole::Muted, "muted"),
        (ColorRole::Foreground, "fg"),
        (ColorRole::Accent, "accent"),
        (ColorRole::Destructive, "red"),
    ];
    let labelled = width >= 640.0;
    let height = if labelled { 11.0 } else { 15.0 };
    let step = 100.0 / roles.len() as f32;
    let mut canvas = Canvas2d::new("Resolved theme palette", 100.0, 20.0);
    for (index, (role, name)) in roles.iter().enumerate() {
        let x = index as f32 * step + 1.0;
        let swatch = step - 2.0;
        // A muted keyline, so the `background` swatch is still a swatch.
        canvas.fill_rect(
            x - 0.5,
            2.0,
            swatch + 1.0,
            height + 1.0,
            2.4,
            theme_paint_with_opacity(ColorRole::Muted, 0.45),
        );
        canvas.fill_rect(x, 2.5, swatch, height, 2.0, theme_paint(*role));
        if labelled {
            canvas.text(
                x,
                15.0,
                swatch,
                4.5,
                *name,
                3.4,
                theme_paint(ColorRole::Muted),
                TextAlign::Center,
            );
        }
    }
    let node = canvas.finish(&mut b);
    b.finish(node)
}

fn item(id: &str, label: &str) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
    }
}

fn rerender() -> Update {
    Update {
        rerender: true,
        presentation: None,
    }
}

fn idle_update() -> Update {
    Update {
        rerender: false,
        presentation: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homepage_wordmark_is_an_81_by_19_bitmap() {
        assert_eq!(WORDMARK.len(), WORDMARK_HEIGHT);
        assert!(WORDMARK.iter().all(|row| row.len() == WORDMARK_WIDTH
            && row.bytes().all(|cell| matches!(cell, b'0' | b'1'))));
    }

    #[test]
    fn full_touchbar_uses_crisp_three_pixel_cells() {
        let geometry = wordmark_geometry(2008.0, 60.0);
        assert_eq!(geometry.cell, 3.0);
        assert_eq!(geometry.left, 882.0);
        assert_eq!(geometry.top, 1.0);

        let narrow = wordmark_geometry(80.0, 60.0);
        assert!(drift(80.0) <= narrow.left);
    }

    #[test]
    fn wordmark_run_count_stays_inside_canvas_budget() {
        let runs: usize = WORDMARK
            .iter()
            .map(|row| {
                row.as_bytes()
                    .windows(2)
                    .filter(|pair| pair[0] == b'0' && pair[1] == b'1')
                    .count()
                    + usize::from(row.starts_with('1'))
            })
            .sum();
        let lifted_runs: usize = WORDMARK[..7]
            .iter()
            .map(|row| {
                row.as_bytes()
                    .windows(2)
                    .filter(|pair| pair[0] == b'0' && pair[1] == b'1')
                    .count()
                    + usize::from(row.starts_with('1'))
            })
            .sum();
        assert_eq!(runs, 211);
        assert_eq!(lifted_runs, 79);
        assert!(runs + lifted_runs + 1 <= 512);
    }
}

touchbar_component_sdk::export!(Omarchy);
