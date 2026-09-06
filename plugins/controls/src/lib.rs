use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use touchbar_component_sdk::kit::{
    ColorRole, Icon, InputEvent, InputKind, Pressable, Representation, ViewBuilder, content, fixed,
    flexible,
};
use touchbar_component_sdk::{
    CommandRunRequest, CommandValue, Guest, HostEvent, Item, PresentationEvent, RenderRequest,
    Update, View, command_run,
    touchbar::plugin::ui::{
        PresentationBegin, PresentationCommand, PresentationDismissal, PresentationLifecycle,
        PresentationPlacement,
    },
};

struct Controls;
static VOLUME: AtomicU32 = AtomicU32::new(55);
static BRIGHTNESS: AtomicU32 = AtomicU32::new(70);
static MICROPHONE_MUTED: AtomicBool = AtomicBool::new(false);
static POWER_PROFILE: AtomicU32 = AtomicU32::new(1);
// 0 = compact, 1 = persistent, 2 = transient.
static VOLUME_PRESENTATION: AtomicU32 = AtomicU32::new(0);

impl Guest for Controls {
    fn items() -> Vec<Item> {
        vec![
            item("volume", "Volume"),
            item("brightness", "Brightness"),
            item("microphone", "Microphone"),
            item("battery", "Battery"),
            item("power-profile", "Power Profile"),
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        Ok(match request.item_id.as_str() {
            "volume" => continuous_view(
                100,
                Icon::Volume,
                "VOL",
                VOLUME.load(Ordering::Relaxed),
                request.viewport.width,
            ),
            "brightness" => continuous_view(
                200,
                Icon::ChevronRight,
                "LIGHT",
                BRIGHTNESS.load(Ordering::Relaxed),
                request.viewport.width,
            ),
            "microphone" => microphone_view(),
            "battery" => battery_view(request.viewport.width),
            "power-profile" => power_view(),
            item => return Err(format!("unknown item {item}")),
        })
    }

    fn handle_event(event: InputEvent) -> Result<Update, String> {
        let mut presentation = None;
        match (event.widget_id, event.kind) {
            (100, InputKind::Activated) => {
                VOLUME_PRESENTATION.store(1, Ordering::Relaxed);
                presentation = Some(PresentationCommand::Begin(PresentationBegin {
                    placement: PresentationPlacement::Anchored,
                    lifecycle: PresentationLifecycle::Persistent,
                    target: None,
                }));
            }
            (100, InputKind::LongPressed) => {
                VOLUME_PRESENTATION.store(2, Ordering::Relaxed);
                presentation = Some(PresentationCommand::Begin(PresentationBegin {
                    placement: PresentationPlacement::Anchored,
                    lifecycle: PresentationLifecycle::Transient,
                    target: None,
                }));
            }
            (101, InputKind::ValueChanged) => {
                if let Some(value) = event.value {
                    let next = percent(value);
                    VOLUME.store(next, Ordering::Relaxed);
                    set_percent("set-volume", next);
                }
            }
            (101, InputKind::Released) if VOLUME_PRESENTATION.load(Ordering::Relaxed) != 0 => {
                presentation = Some(PresentationCommand::End(PresentationDismissal::Selection));
            }
            (200, InputKind::Activated) => {
                let next = if BRIGHTNESS.load(Ordering::Relaxed) <= 10 {
                    70
                } else {
                    10
                };
                BRIGHTNESS.store(next, Ordering::Relaxed);
                set_percent("set-brightness", next);
            }
            (201, InputKind::ValueChanged) => {
                if let Some(value) = event.value {
                    let next = percent(value);
                    BRIGHTNESS.store(next, Ordering::Relaxed);
                    set_percent("set-brightness", next);
                }
            }
            (300, InputKind::Activated) => {
                MICROPHONE_MUTED.fetch_xor(true, Ordering::Relaxed);
                run("toggle-microphone", Vec::new());
            }
            (500, InputKind::Activated) => {
                let next = (POWER_PROFILE.load(Ordering::Relaxed) + 1) % 3;
                POWER_PROFILE.store(next, Ordering::Relaxed);
                run(
                    "set-power-profile",
                    vec![CommandValue::FixedEnum {
                        name: "profile".into(),
                        value: profile(next).into(),
                    }],
                );
            }
            _ => {}
        }
        Ok(Update {
            rerender: true,
            presentation,
        })
    }

    fn handle_presentation_event(event: PresentationEvent) -> Result<Update, String> {
        if matches!(event, PresentationEvent::Ended(_)) {
            VOLUME_PRESENTATION.store(0, Ordering::Relaxed);
        }
        Ok(Update {
            rerender: true,
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
fn percent(value: f32) -> u32 {
    (value.clamp(0.0, 1.0) * 100.0).round() as u32
}
fn set_percent(command: &str, value: u32) {
    run(
        command,
        vec![CommandValue::Text {
            name: "percent".into(),
            value: format!("{value}%"),
        }],
    );
}
fn run(command_id: &str, values: Vec<CommandValue>) {
    let _ = command_run(&CommandRunRequest {
        command_id: command_id.into(),
        values,
    });
}

fn continuous_view(base: u64, icon: Icon, short: &str, value: u32, width: f32) -> View {
    let mut b = ViewBuilder::new();
    let compact_icon = b.icon(icon, ColorRole::Foreground, short);
    let compact_label = b.label(format!("{short} {value}%"), 11.0, ColorRole::Foreground);
    let compact_content = b.row(
        vec![
            fixed(compact_icon, 24.0),
            flexible(compact_label, 36.0, 80.0, 160.0),
        ],
        3.0,
        2.0,
    );
    let compact = b.pressable(
        base,
        format!("Expand {short}"),
        compact_content,
        Pressable {
            selected: Some(value == 0),
            hold_ms: Some(420),
            ..Pressable::control()
        },
    );
    let icon = b.icon(icon, ColorRole::Foreground, short);
    let slider = b.slider(base + 1, short, value as f32 / 100.0);
    let label = b.label(format!("{value}%"), 11.0, ColorRole::Foreground);
    let full = b.row(
        vec![
            fixed(icon, 30.0),
            flexible(slider, 60.0, 120.0, 800.0),
            content(label, 34.0, 48.0),
        ],
        5.0,
        4.0,
    );
    let root = if width < 200.0 {
        compact
    } else {
        b.responsive(
            base + 10,
            vec![
                (Representation::Minimal, 0.0, compact),
                (Representation::Full, 120.0, full),
            ],
        )
    };
    b.finish(root)
}

fn microphone_view() -> View {
    let muted = MICROPHONE_MUTED.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let icon = b.icon(
        if muted { Icon::Muted } else { Icon::Volume },
        if muted {
            ColorRole::Destructive
        } else {
            ColorRole::Foreground
        },
        "Microphone",
    );
    let label = b.label(
        if muted { "MUTED" } else { "MIC" },
        11.0,
        if muted {
            ColorRole::Destructive
        } else {
            ColorRole::Foreground
        },
    );
    let row = b.row(
        vec![fixed(icon, 28.0), content(label, 28.0, 80.0)],
        4.0,
        3.0,
    );
    let root = b.pressable(
        300,
        "Toggle microphone",
        row,
        Pressable {
            selected: Some(muted),
            ..Pressable::control()
        },
    );
    b.finish(root)
}

fn battery_view(width: f32) -> View {
    let mut b = ViewBuilder::new();
    let level = 0.82;
    let label = b.label("82%", 11.0, ColorRole::Foreground);
    let progress = b.progress("Battery", level, ColorRole::Accent);
    let full = b.row(
        vec![
            content(label, 32.0, 42.0),
            flexible(progress, 40.0, 100.0, 800.0),
        ],
        5.0,
        5.0,
    );
    let root = if width < 100.0 { label } else { full };
    b.finish(root)
}

fn power_view() -> View {
    let current = POWER_PROFILE.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let label = b.label(profile(current).to_uppercase(), 10.0, ColorRole::OnAccent);
    let root = b.pressable(500, "Cycle power profile", label, Pressable::accent());
    b.finish(root)
}

fn profile(value: u32) -> &'static str {
    match value {
        0 => "power-saver",
        2 => "performance",
        _ => "balanced",
    }
}

touchbar_component_sdk::export!(Controls);
