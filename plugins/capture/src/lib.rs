use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use touchbar_component_sdk::kit::{
    ColorRole, InputEvent, InputKind, Pressable, ViewBuilder, flexible,
};
use touchbar_component_sdk::{
    Guest, HostEvent, Item, LocalConnect, PresentationEvent, RenderRequest, ResourceId, Update,
    View, decode_local_connection_opened, local_connect, local_send,
};

struct Capture;
static RECORDING: AtomicBool = AtomicBool::new(false);
static ELAPSED: AtomicU32 = AtomicU32::new(0);
static REQUEST: AtomicU64 = AtomicU64::new(0);
static RESOURCE: AtomicU64 = AtomicU64::new(0);
static PENDING_ACTION: AtomicU32 = AtomicU32::new(0);

impl Guest for Capture {
    fn items() -> Vec<Item> {
        vec![
            item("screenshots", "Screenshots"),
            item("recording", "Recording"),
            item("capture-tools", "Capture Tools"),
        ]
    }
    fn render(request: RenderRequest) -> Result<View, String> {
        Ok(match request.item_id.as_str() {
            "screenshots" => screenshots(),
            "recording" => recording(),
            "capture-tools" => tools(),
            item => return Err(format!("unknown item {item}")),
        })
    }
    fn handle_event(event: InputEvent) -> Result<Update, String> {
        if event.kind != InputKind::Activated {
            return Ok(Update {
                rerender: false,
                presentation: None,
            });
        }
        match event.widget_id {
            1..=4 => invoke(event.widget_id as u32),
            10 => {
                let was = RECORDING.fetch_xor(true, Ordering::Relaxed);
                invoke(if was { 11 } else { 10 });
                if was {
                    ELAPSED.store(0, Ordering::Relaxed);
                }
            }
            20..=22 => invoke(event.widget_id as u32),
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
    fn handle_host_event(event: HostEvent) -> Result<Update, String> {
        use touchbar_component_sdk::touchbar::plugin::broker::OperationResult;
        match event {
            HostEvent::Completion((request_id, OperationResult::Success(payload)))
                if request_id == REQUEST.load(Ordering::Relaxed) =>
            {
                if let Ok((resource, _)) = decode_local_connection_opened(&payload) {
                    RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                    REQUEST.store(0, Ordering::Relaxed);
                    send_action(resource, PENDING_ACTION.swap(0, Ordering::Relaxed));
                }
            }
            HostEvent::ResourceEvent((resource, _, _))
                if resource == RESOURCE.load(Ordering::Relaxed) => {}
            _ => {}
        }
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
fn invoke(action: u32) {
    let resource = RESOURCE.load(Ordering::Relaxed);
    if resource != 0 {
        send_action(ResourceId::from_raw(resource), action);
        return;
    }
    PENDING_ACTION.store(action, Ordering::Relaxed);
    if REQUEST.load(Ordering::Relaxed) == 0
        && let Ok(request) = local_connect(&LocalConnect {
            endpoint: "capture".into(),
            protocol: "io.github.cameroncooper.touchbar.capture.v1".into(),
        })
    {
        REQUEST.store(request.into_raw(), Ordering::Relaxed);
    }
}

fn send_action(resource: ResourceId, action: u32) {
    if action == 0 {
        return;
    }
    let action = match action {
        1 => "screenshot.smart",
        2 => "screenshot.region",
        3 => "screenshot.window",
        4 => "screenshot.fullscreen",
        10 => "record.start",
        11 => "record.stop",
        20 => "ocr",
        21 => "qr",
        22 => "color",
        _ => return,
    };
    let _ = local_send(resource, format!("{{\"action\":\"{action}\"}}").as_bytes());
}
fn button(b: &mut ViewBuilder, id: u64, label: &str) -> u32 {
    b.button(id, label, None, false, false)
}

fn screenshots() -> View {
    let mut b = ViewBuilder::new();
    let smart = button(&mut b, 1, "SMART");
    let region = button(&mut b, 2, "REGION");
    let window = button(&mut b, 3, "WINDOW");
    let full = button(&mut b, 4, "FULL");
    let root = b.row(
        vec![
            flexible(smart, 40.0, 80.0, 260.0),
            flexible(region, 40.0, 80.0, 260.0),
            flexible(window, 40.0, 80.0, 260.0),
            flexible(full, 40.0, 80.0, 260.0),
        ],
        4.0,
        2.0,
    );
    b.finish(root)
}
fn recording() -> View {
    let active = RECORDING.load(Ordering::Relaxed);
    let mut b = ViewBuilder::new();
    let dot = b.label(
        if active { "● REC" } else { "RECORD" },
        11.0,
        if active {
            ColorRole::OnDestructive
        } else {
            ColorRole::Foreground
        },
    );
    let style = if active {
        Pressable {
            background: Some(ColorRole::Destructive),
            pressed_background: Some(ColorRole::Destructive),
            content_foreground: Some(ColorRole::OnDestructive),
            ..Pressable::control()
        }
    } else {
        Pressable::control()
    };
    let root = b.pressable(
        10,
        if active {
            "Stop recording"
        } else {
            "Start recording"
        },
        dot,
        style,
    );
    b.finish(root)
}
fn tools() -> View {
    let mut b = ViewBuilder::new();
    let ocr = button(&mut b, 20, "OCR");
    let qr = button(&mut b, 21, "QR");
    let color = button(&mut b, 22, "COLOR");
    let root = b.row(
        vec![
            flexible(ocr, 40.0, 70.0, 300.0),
            flexible(qr, 40.0, 70.0, 300.0),
            flexible(color, 40.0, 70.0, 300.0),
        ],
        4.0,
        2.0,
    );
    b.finish(root)
}

touchbar_component_sdk::export!(Capture);
