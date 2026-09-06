use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use touchbar_component_sdk::kit::{
    ColorRole, InputEvent, InputKind, Pressable, ViewBuilder, flexible,
};
use touchbar_component_sdk::{
    CommandEvent, CommandRunRequest, Guest, HostEvent, Item, LocalConnect, PresentationEvent,
    ResourceId, Update, View, command_run, decode_command_event, decode_command_opened,
    decode_local_connection_opened, decode_local_frame, local_connect, local_send,
};

struct CommandDeck;
static CUSTOM_REQUEST: AtomicU64 = AtomicU64::new(0);
static CUSTOM_RESOURCE: AtomicU64 = AtomicU64::new(0);
static CUSTOM_RESPONSE_BYTES: AtomicU64 = AtomicU64::new(0);
static PENDING_ACTION: AtomicU32 = AtomicU32::new(0);
static PROBE_REQUEST: AtomicU64 = AtomicU64::new(0);
static PROBE_RESOURCE: AtomicU64 = AtomicU64::new(0);
static PROBE_STATE: AtomicU32 = AtomicU32::new(PROBE_IDLE);

const PROBE_IDLE: u32 = 0;
const PROBE_PENDING: u32 = 1;
const PROBE_RUNNING: u32 = 2;
const PROBE_SUCCEEDED: u32 = 3;
const PROBE_FAILED: u32 = 4;

impl Guest for CommandDeck {
    fn items() -> Vec<Item> {
        vec![
            item("quick-actions", "Quick Actions"),
            item("launchers", "Launchers"),
            item("session", "Session Actions"),
            item("custom-actions", "Custom Actions"),
        ]
    }
    fn render(request: touchbar_component_sdk::RenderRequest) -> Result<View, String> {
        Ok(match request.item_id.as_str() {
            "quick-actions" => quick_actions(),
            "launchers" => launchers(),
            "session" => session(),
            "custom-actions" => custom(),
            item => return Err(format!("unknown item {item}")),
        })
    }
    fn handle_event(event: InputEvent) -> Result<Update, String> {
        match (event.widget_id, event.kind) {
            (1..=3, InputKind::Activated) => invoke_custom(event.widget_id as u32),
            (10, InputKind::Activated) => run("terminal", Vec::new()),
            (11, InputKind::Activated) => invoke_custom(11),
            (12, InputKind::Activated) => run_probe(),
            (20, InputKind::LongPressed) => run("lock", Vec::new()),
            (21, InputKind::Activated) => invoke_custom(21),
            (100..=103, InputKind::Activated) => invoke_custom((event.widget_id - 99) as u32),
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
                if request_id == PROBE_REQUEST.load(Ordering::Relaxed) =>
            {
                PROBE_REQUEST.store(0, Ordering::Relaxed);
                match decode_command_opened(&payload) {
                    Ok(resource) => {
                        PROBE_RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                        PROBE_STATE.store(PROBE_RUNNING, Ordering::Relaxed);
                    }
                    Err(_) => PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed),
                }
            }
            HostEvent::Completion((request_id, OperationResult::Error(_)))
                if request_id == PROBE_REQUEST.load(Ordering::Relaxed) =>
            {
                PROBE_REQUEST.store(0, Ordering::Relaxed);
                PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed);
            }
            HostEvent::ResourceEvent((resource, _, OperationResult::Success(payload)))
                if resource == PROBE_RESOURCE.load(Ordering::Relaxed) =>
            {
                if let Ok(CommandEvent::Exited {
                    exit_code, signal, ..
                }) = decode_command_event(&payload)
                {
                    PROBE_RESOURCE.store(0, Ordering::Relaxed);
                    PROBE_STATE.store(
                        if exit_code == Some(0) && signal.is_none() {
                            PROBE_SUCCEEDED
                        } else {
                            PROBE_FAILED
                        },
                        Ordering::Relaxed,
                    );
                }
            }
            HostEvent::ResourceEvent((resource, _, OperationResult::Error(_)))
                if resource == PROBE_RESOURCE.load(Ordering::Relaxed) =>
            {
                PROBE_RESOURCE.store(0, Ordering::Relaxed);
                PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed);
            }
            HostEvent::Completion((request_id, OperationResult::Success(payload)))
                if request_id == CUSTOM_REQUEST.load(Ordering::Relaxed) =>
            {
                if let Ok((resource, _)) = decode_local_connection_opened(&payload) {
                    CUSTOM_RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                    CUSTOM_REQUEST.store(0, Ordering::Relaxed);
                    send_custom(resource, PENDING_ACTION.swap(0, Ordering::Relaxed));
                }
            }
            HostEvent::ResourceEvent((resource, _, OperationResult::Success(payload)))
                if resource == CUSTOM_RESOURCE.load(Ordering::Relaxed) =>
            {
                if payload.is_empty() {
                    CUSTOM_RESOURCE.store(0, Ordering::Relaxed);
                } else if let Ok(frame) = decode_local_frame(&payload) {
                    CUSTOM_RESPONSE_BYTES.store(frame.bytes.len() as u64, Ordering::Relaxed);
                }
            }
            HostEvent::ResourceEvent((resource, _, OperationResult::Error(_)))
                if resource == CUSTOM_RESOURCE.load(Ordering::Relaxed) =>
            {
                CUSTOM_RESOURCE.store(0, Ordering::Relaxed);
                CUSTOM_RESPONSE_BYTES.store(u64::MAX, Ordering::Relaxed);
            }
            _ => {}
        }
        Ok(Update {
            rerender: true,
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
fn run(command_id: &str, values: Vec<touchbar_component_sdk::CommandValue>) {
    let _ = command_run(&CommandRunRequest {
        command_id: command_id.into(),
        values,
    });
}
fn run_probe() {
    if PROBE_REQUEST.load(Ordering::Relaxed) != 0 || PROBE_RESOURCE.load(Ordering::Relaxed) != 0 {
        return;
    }
    PROBE_STATE.store(PROBE_PENDING, Ordering::Relaxed);
    match command_run(&CommandRunRequest {
        command_id: "probe".into(),
        values: Vec::new(),
    }) {
        Ok(request) => PROBE_REQUEST.store(request.into_raw(), Ordering::Relaxed),
        Err(_) => PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed),
    }
}
fn invoke_custom(action: u32) {
    let resource = CUSTOM_RESOURCE.load(Ordering::Relaxed);
    if resource != 0 {
        send_custom(ResourceId::from_raw(resource), action);
        return;
    }
    PENDING_ACTION.store(action, Ordering::Relaxed);
    if CUSTOM_REQUEST.load(Ordering::Relaxed) == 0
        && let Ok(request) = local_connect(&LocalConnect {
            endpoint: "command-deck".into(),
            protocol: "io.github.cameroncooper.touchbar.command-deck.v1".into(),
        })
    {
        CUSTOM_REQUEST.store(request.into_raw(), Ordering::Relaxed);
    }
}
fn send_custom(resource: ResourceId, action: u32) {
    if action != 0 {
        let _ = local_send(resource, format!("{{\"action\":{action}}}").as_bytes());
    }
}
fn row_of(b: &mut ViewBuilder, values: &[(u64, &str)]) -> u32 {
    let nodes = values
        .iter()
        .map(|(id, label)| b.button(*id, *label, None, false, false))
        .map(|node| flexible(node, 40.0, 80.0, 300.0))
        .collect();
    b.row(nodes, 4.0, 2.0)
}
fn quick_actions() -> View {
    let mut b = ViewBuilder::new();
    let root = row_of(&mut b, &[(1, "MENU"), (2, "THEME"), (3, "SYSTEM")]);
    b.finish(root)
}
fn launchers() -> View {
    let mut b = ViewBuilder::new();
    let probe = match PROBE_STATE.load(Ordering::Relaxed) {
        PROBE_PENDING | PROBE_RUNNING => "RUN",
        PROBE_SUCCEEDED => "OK",
        PROBE_FAILED => "ERR",
        _ => "PROBE",
    };
    let root = row_of(&mut b, &[(10, "TERMINAL"), (11, "APPS"), (12, probe)]);
    b.finish(root)
}
fn session() -> View {
    let mut b = ViewBuilder::new();
    let lock = b.label("HOLD LOCK", 10.0, ColorRole::OnDestructive);
    let lock = b.pressable(
        20,
        "Hold to lock",
        lock,
        Pressable {
            hold_ms: Some(800),
            background: Some(ColorRole::Destructive),
            pressed_background: Some(ColorRole::Destructive),
            content_foreground: Some(ColorRole::OnDestructive),
            ..Pressable::control()
        },
    );
    let idle = b.button(21, "IDLE", None, false, false);
    let root = b.row(
        vec![
            flexible(lock, 60.0, 100.0, 500.0),
            flexible(idle, 44.0, 80.0, 400.0),
        ],
        4.0,
        2.0,
    );
    b.finish(root)
}
fn custom() -> View {
    let mut b = ViewBuilder::new();
    let response_bytes = CUSTOM_RESPONSE_BYTES.load(Ordering::Relaxed);
    let first_label = match response_bytes {
        0 => "1".into(),
        u64::MAX => "ERR".into(),
        bytes => format!("OK {bytes}B"),
    };
    let first = b.button(100, &first_label, None, false, false);
    let second = b.button(101, "2", None, false, false);
    let third = b.button(102, "3", None, false, false);
    let fourth = b.button(103, "4", None, false, false);
    let root = b.row(
        vec![first, second, third, fourth]
            .into_iter()
            .map(|node| flexible(node, 40.0, 80.0, 300.0))
            .collect(),
        4.0,
        2.0,
    );
    b.finish(root)
}

touchbar_component_sdk::export!(CommandDeck);
