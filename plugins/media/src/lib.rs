use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use touchbar_component_sdk::kit::{
    Animation, AnimationPlayback, Canvas2d, ColorRole, GpuEffect, Icon, ImageFit, ImageTint,
    InputEvent, InputKind, Path2d, Pressable, ViewBuilder, content, fixed, flexible, theme_paint,
    theme_paint_with_opacity, visual_transform,
};
use touchbar_component_sdk::{
    DbusBus, DbusCall, DbusReplyKind, Guest, HostEvent, Item, PresentationEvent, RenderRequest,
    Update, View, dbus_call,
};

struct Media;
static PLAYING: AtomicBool = AtomicBool::new(false);
static POSITION: AtomicU32 = AtomicU32::new(36);

impl Guest for Media {
    fn items() -> Vec<Item> {
        vec![
            item("now-playing", "Now Playing"),
            item("transport", "Media Transport"),
            item("timeline", "Media Timeline"),
            item("touchbar-logo", "TouchBar Logo"),
        ]
    }
    fn render(request: RenderRequest) -> Result<View, String> {
        Ok(match request.item_id.as_str() {
            "now-playing" => now_playing(request.viewport.width),
            "transport" => transport(),
            "timeline" => timeline(request.viewport.width),
            "touchbar-logo" => touchbar_logo(),
            item => return Err(format!("unknown item {item}")),
        })
    }
    fn handle_event(event: InputEvent) -> Result<Update, String> {
        match (event.widget_id, event.kind) {
            (1, InputKind::Activated) => {
                PLAYING.fetch_xor(true, Ordering::Relaxed);
                call("PlayPause");
            }
            (2, InputKind::Activated) => call("Previous"),
            (3, InputKind::Activated) => call("Next"),
            (4, InputKind::ValueChanged) => {
                if let Some(value) = event.value {
                    POSITION.store(
                        (value.clamp(0.0, 1.0) * 100.0).round() as u32,
                        Ordering::Relaxed,
                    );
                }
            }
            _ => {}
        }
        Ok(Update {
            rerender: true,
            presentation: None,
        })
    }
    fn handle_presentation_event(_event: PresentationEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }
    fn handle_host_event(_event: HostEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }
}

fn item(id: &str, label: &str) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
    }
}
fn call(member: &str) {
    let _ = dbus_call(&DbusCall {
        bus: DbusBus::Session,
        destination: "org.mpris.MediaPlayer2.playerctld".into(),
        path: "/org/mpris/MediaPlayer2".into(),
        interface: "org.mpris.MediaPlayer2.Player".into(),
        member: member.into(),
        arguments: Vec::new(),
        reply: DbusReplyKind::Unit,
    });
}

fn now_playing(width: f32) -> View {
    let playing = PLAYING.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let backdrop = b.shader_effect(
        GpuEffect::new(
            2_001,
            "Theme-reactive ambient playback wave",
            r#"
let centered = uv - vec2<f32>(0.5, 0.5);
let carrier = sin(centered.x * 18.0 - time * 6.2831853);
let distance_to_wave = abs(centered.y - carrier * (0.08 + params0.x * 0.04));
let glow = 1.0 - smoothstep(0.01, 0.24, distance_to_wave);
let pulse = 0.5 + 0.5 * sin(time * 3.1415926 + params0.y * 2.0);
let base = mix(background, control, 0.42);
let color = mix(base, accent, glow * (0.05 + pulse * 0.10));
"#,
        )
        .parameters([
            POSITION.load(Ordering::Relaxed) as f32 / 100.0,
            if playing { 1.0 } else { 0.0 },
        ])
        .opacity(0.92)
        .preferred_size(320.0, 60.0)
        .animate(6_000),
    );
    let play = b.icon(
        if playing { Icon::Pause } else { Icon::Play },
        ColorRole::OnAccent,
        "Playback",
    );
    let play = b.pressable(1, "Play or pause", play, Pressable::accent());
    let title = b.label(
        if width < 150.0 {
            "NOW PLAYING"
        } else {
            "TouchBar Radio — Theme Signals"
        },
        11.0,
        ColorRole::Foreground,
    );
    let progress = b.progress(
        "Track progress",
        POSITION.load(Ordering::Relaxed) as f32 / 100.0,
        ColorRole::Accent,
    );
    let signal = playback_signal(&mut b, POSITION.load(Ordering::Relaxed));
    let signal = b.motion(
        signal,
        Animation::new(1_001, 900)
            .from(visual_transform(0.0, 0.0, 0.98, 0.72))
            .to(visual_transform(0.0, 0.0, 1.02, 1.0))
            .playback(AnimationPlayback::Alternate),
    );
    let info = b.column(
        vec![
            fixed(title, 20.0),
            if width < 130.0 {
                flexible(progress, 12.0, 20.0, 30.0)
            } else {
                flexible(signal, 20.0, 27.0, 30.0)
            },
        ],
        3.0,
        2.0,
    );
    let root = b.row(
        vec![fixed(play, 48.0), flexible(info, 50.0, 180.0, 900.0)],
        5.0,
        3.0,
    );
    let root = b.layer(vec![backdrop, root]);
    b.finish(root)
}

fn playback_signal(builder: &mut ViewBuilder, position: u32) -> u32 {
    let cursor = position.min(100) as f32;
    let mut canvas = Canvas2d::new("Playback signal and position", 100.0, 20.0);
    let mut waveform = Path2d::new();
    waveform
        .move_to((0.0, 10.0))
        .cubic_to((12.0, 1.0), (22.0, 18.0), (34.0, 10.0))
        .cubic_to((48.0, -1.0), (57.0, 19.0), (68.0, 10.0))
        .cubic_to((79.0, 2.0), (91.0, 17.0), (100.0, 10.0));
    let mut cursor_marker = Path2d::new();
    cursor_marker
        .move_to((cursor - 2.8, 5.5))
        .line_to((cursor + 2.8, 10.0))
        .line_to((cursor - 2.8, 14.5))
        .close();
    canvas
        .fill_linear_gradient_rect(
            0.0,
            8.5,
            100.0,
            3.0,
            1.5,
            (0.0, 10.0),
            (100.0, 10.0),
            theme_paint_with_opacity(ColorRole::Track, 0.8),
            theme_paint_with_opacity(ColorRole::Control, 0.35),
        )
        .stroke_path(
            waveform,
            1.4,
            theme_paint_with_opacity(ColorRole::Muted, 0.8),
        )
        .fill_rect(0.0, 9.0, cursor, 2.0, 1.0, theme_paint(ColorRole::Accent))
        .fill_path(cursor_marker, theme_paint(ColorRole::Accent));
    canvas.finish(builder)
}

fn transport() -> View {
    let playing = PLAYING.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let previous = b.button(2, "Previous", Some(Icon::ChevronLeft), false, false);
    let toggle = b.button(
        1,
        if playing { "Pause" } else { "Play" },
        Some(if playing { Icon::Pause } else { Icon::Play }),
        false,
        playing,
    );
    let next = b.button(3, "Next", Some(Icon::ChevronRight), false, false);
    let root = b.row(
        vec![
            flexible(previous, 40.0, 60.0, 200.0),
            flexible(toggle, 44.0, 70.0, 220.0),
            flexible(next, 40.0, 60.0, 200.0),
        ],
        4.0,
        2.0,
    );
    b.finish(root)
}

fn timeline(width: f32) -> View {
    let position = POSITION.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let elapsed = b.label(
        format!("{}:{:02}", position / 10, (position % 10) * 6),
        10.0,
        ColorRole::Muted,
    );
    let slider = b.slider(4, "Timeline", position as f32 / 100.0);
    let remaining = b.label(
        if width < 150.0 {
            "".into()
        } else {
            format!(
                "-{}:{:02}",
                (100 - position) / 10,
                ((100 - position) % 10) * 6
            )
        },
        10.0,
        ColorRole::Muted,
    );
    let root = b.row(
        vec![
            content(elapsed, 30.0, 44.0),
            flexible(slider, 45.0, 180.0, 850.0),
            content(remaining, 0.0, 44.0),
        ],
        4.0,
        4.0,
    );
    b.finish(root)
}

fn touchbar_logo() -> View {
    let mut b = ViewBuilder::new();
    let logo = b.image(
        "touchbar-wordmark",
        "TouchBar",
        ImageFit::Contain,
        ImageTint::Mask(ColorRole::Accent),
    );
    let root = b.row(vec![flexible(logo, 80.0, 171.0, 1004.0)], 0.0, 8.0);
    b.finish(root)
}

touchbar_component_sdk::export!(Media);
