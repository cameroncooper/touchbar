use std::{
    collections::BTreeSet,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use touchbar_component_sdk::{
    ContextFactValue, ContextReadRequest, Guest, HostEvent, Item, RenderRequest, Update, View,
    bindings::touchbar::plugin::{broker, ui},
    context_subscribe, decode_context_snapshot, decode_context_subscription_opened,
};
use ui::{
    ButtonNode, ColorRole, Flex, FlexChild, Icon, LabelNode, Node, Representation, ResponsiveNode,
    ResponsiveVariant, TextAlign,
};

struct Demo;

static SELECTED: AtomicBool = AtomicBool::new(false);
static PRESSED: AtomicBool = AtomicBool::new(false);
static CONTEXT_REQUEST: AtomicU64 = AtomicU64::new(0);
static CONTEXT_RESOURCE: AtomicU64 = AtomicU64::new(0);
static APPLICATION: Mutex<Option<String>> = Mutex::new(None);

impl Guest for Demo {
    fn items() -> Vec<Item> {
        vec![
            Item {
                id: "hello".into(),
                label: "Hello".into(),
            },
            Item {
                id: "theme".into(),
                label: "Theme".into(),
            },
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        match request.item_id.as_str() {
            "hello" => Ok(hello_view()),
            "theme" => Ok(theme_view(request)),
            unknown => Err(format!("unknown item {unknown}")),
        }
    }

    fn handle_event(event: ui::InputEvent) -> Result<Update, String> {
        if event.item_id != "hello" || event.widget_id != 1 {
            return Ok(Update {
                rerender: false,
                presentation: None,
            });
        }
        let rerender = match event.kind {
            ui::InputKind::Pressed => {
                !PRESSED.swap(true, Ordering::Relaxed)
            }
            ui::InputKind::Activated => {
                SELECTED.fetch_xor(true, Ordering::Relaxed);
                if CONTEXT_REQUEST.load(Ordering::Relaxed) == 0
                    && CONTEXT_RESOURCE.load(Ordering::Relaxed) == 0
                    && let Ok(request) = context_subscribe(&ContextReadRequest {
                        facts: BTreeSet::from(["application.id".into()]),
                    })
                {
                    CONTEXT_REQUEST.store(request.into_raw(), Ordering::Relaxed);
                }
                true
            }
            ui::InputKind::LongPressed | ui::InputKind::ValueChanged => false,
            ui::InputKind::Released | ui::InputKind::Cancelled => {
                PRESSED.swap(false, Ordering::Relaxed)
            }
        };
        Ok(Update {
            rerender,
            presentation: None,
        })
    }

    fn handle_presentation_event(
        _event: ui::PresentationEvent,
    ) -> Result<Update, String> {
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }

    fn handle_host_event(event: HostEvent) -> Result<Update, String> {
        let rerender = match event {
            broker::HostEvent::Completion((request_id, broker::OperationResult::Success(payload)))
                if request_id == CONTEXT_REQUEST.load(Ordering::Relaxed) =>
            {
                if let Ok((resource, snapshot)) = decode_context_subscription_opened(&payload) {
                    CONTEXT_RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                    set_application(snapshot);
                }
                CONTEXT_REQUEST.store(0, Ordering::Relaxed);
                true
            }
            broker::HostEvent::ResourceEvent((
                resource_id,
                _,
                broker::OperationResult::Success(payload),
            )) if resource_id == CONTEXT_RESOURCE.load(Ordering::Relaxed) => {
                if let Ok(snapshot) = decode_context_snapshot(&payload) {
                    set_application(snapshot);
                }
                true
            }
            _ => false,
        };
        Ok(Update {
            rerender,
            presentation: None,
        })
    }
}

fn set_application(snapshot: touchbar_component_sdk::ContextSnapshot) {
    let application = snapshot
        .facts
        .into_iter()
        .find_map(|fact| (fact.key == "application.id").then_some(fact.value))
        .and_then(|value| match value {
            ContextFactValue::Text(value) => Some(value),
            ContextFactValue::Boolean(_) => None,
        });
    if let Ok(mut current) = APPLICATION.lock() {
        *current = application;
    }
}

fn hello_view() -> View {
    let selected = SELECTED.load(Ordering::Relaxed);
    let pressed = PRESSED.load(Ordering::Relaxed);
    let application = APPLICATION.lock().ok().and_then(|value| value.clone());
    let label = application
        .as_deref()
        .map(|application| format!("APP {application}"))
        .unwrap_or_else(|| {
            if selected {
                "SELECTED".into()
            } else {
                "HELLO".into()
            }
        });
    View {
        root: 2,
        nodes: vec![
            Node::Button(ButtonNode {
                widget_id: 1,
                label,
                icon: Some(Icon::Check),
                pressed,
                selected,
            }),
            Node::Button(ButtonNode {
                widget_id: 1,
                label: if selected { "✓" } else { "HI" }.into(),
                icon: None,
                pressed,
                selected,
            }),
            Node::Responsive(ResponsiveNode {
                widget_id: 100,
                variants: vec![
                    ResponsiveVariant {
                        representation: Representation::Minimal,
                        minimum_width: 0.0,
                        node: 1,
                    },
                    ResponsiveVariant {
                        representation: Representation::Full,
                        minimum_width: 92.0,
                        node: 0,
                    },
                ],
            }),
        ],
    }
}

fn theme_view(request: RenderRequest) -> View {
    let light = request.theme.scheme == ui::ColorScheme::Light;
    let accent = request.theme.accent;
    let text = format!(
        "{} #{:02X}{:02X}{:02X}",
        if light { "LIGHT" } else { "DARK" },
        channel(accent.red),
        channel(accent.green),
        channel(accent.blue),
    );
    View {
        root: 0,
        nodes: vec![Node::Label(LabelNode {
            text,
            size: if request.viewport.width < 120.0 {
                10.0
            } else {
                12.0
            },
            color: ColorRole::Foreground,
            align: TextAlign::Center,
        })],
    }
}

fn channel(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[allow(dead_code)]
fn equal_child(node: u32) -> FlexChild {
    FlexChild {
        node,
        layout: Flex {
            minimum: 44.0,
            basis: 80.0,
            maximum: 200.0,
            grow: 1.0,
            shrink: 1.0,
            visibility_priority: 0,
            required: false,
            intrinsic: false,
        },
    }
}

touchbar_component_sdk::export!(Demo);
