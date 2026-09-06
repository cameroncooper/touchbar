use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use touchbar_component_sdk::{
    DbusBus, DbusCall, DbusReply, DbusReplyKind, DbusSubscription, DbusValue, Guest, Item,
    RenderRequest, Update, View,
    bindings::touchbar::plugin::{broker, ui},
};

struct Demo;

const IDLE: u8 = 0;
const PENDING: u8 = 1;
const PLAYING: u8 = 2;
const PAUSED: u8 = 3;
const TOGGLED: u8 = 4;
const FAILED: u8 = 5;

static STATE: AtomicU8 = AtomicU8::new(IDLE);
static SUBSCRIBE_REQUEST: AtomicU64 = AtomicU64::new(0);
static SUBSCRIPTION_RESOURCE: AtomicU64 = AtomicU64::new(0);

impl Guest for Demo {
    fn items() -> Vec<Item> {
        vec![Item {
            id: "media-broker".into(),
            label: "Media broker".into(),
        }]
    }

    fn render(_request: RenderRequest) -> Result<View, String> {
        let (status, color) = match STATE.load(Ordering::Relaxed) {
            PENDING => ("WAITING", ui::ColorRole::Accent),
            PLAYING => ("PLAYING", ui::ColorRole::Foreground),
            PAUSED => ("PAUSED", ui::ColorRole::Muted),
            TOGGLED => ("TOGGLED", ui::ColorRole::Accent),
            FAILED => ("UNAVAILABLE", ui::ColorRole::Muted),
            _ => ("STATUS", ui::ColorRole::Foreground),
        };
        Ok(View {
            root: 2,
            nodes: vec![
                ui::Node::Button(ui::ButtonNode {
                    widget_id: 1,
                    label: status.into(),
                    icon: None,
                    pressed: false,
                    selected: color == ui::ColorRole::Accent,
                }),
                ui::Node::Button(ui::ButtonNode {
                    widget_id: 2,
                    label: "PLAY/PAUSE".into(),
                    icon: Some(ui::Icon::Play),
                    pressed: false,
                    selected: false,
                }),
                ui::Node::Row(ui::ContainerNode {
                    gap: 4.0,
                    padding: 2.0,
                    align: ui::CrossAxisAlignment::Stretch,
                    children: vec![flex_child(0), flex_child(1)],
                }),
            ],
        })
    }

    fn handle_event(event: ui::InputEvent) -> Result<Update, String> {
        if event.kind != ui::InputKind::Activated || !matches!(event.widget_id, 1 | 2) {
            return Ok(Update {
                rerender: false,
                presentation: None,
            });
        }
        STATE.store(PENDING, Ordering::Relaxed);
        if event.widget_id == 1 {
            if SUBSCRIPTION_RESOURCE.load(Ordering::Relaxed) == 0
                && SUBSCRIBE_REQUEST.load(Ordering::Relaxed) == 0
            {
                let subscription = DbusSubscription {
                    bus: DbusBus::Session,
                    sender: "org.mpris.MediaPlayer2.playerctld".into(),
                    path: "/org/mpris/MediaPlayer2".into(),
                    interface: "org.freedesktop.DBus.Properties".into(),
                    member: "PropertiesChanged".into(),
                    signature: "sa{sv}as".into(),
                    argument_zero: Some("org.mpris.MediaPlayer2.Player".into()),
                };
                match touchbar_component_sdk::dbus_subscribe(&subscription) {
                    Ok(request) => {
                        SUBSCRIBE_REQUEST.store(request.into_raw(), Ordering::Relaxed);
                    }
                    Err(_) => {
                        STATE.store(FAILED, Ordering::Relaxed);
                        return Ok(Update {
                            rerender: true,
                            presentation: None,
                        });
                    }
                }
            }
            let call = DbusCall {
                bus: DbusBus::Session,
                destination: "org.mpris.MediaPlayer2.playerctld".into(),
                path: "/org/mpris/MediaPlayer2".into(),
                interface: "org.freedesktop.DBus.Properties".into(),
                member: "Get".into(),
                arguments: vec![
                    "org.mpris.MediaPlayer2.Player".into(),
                    "PlaybackStatus".into(),
                ],
                reply: DbusReplyKind::VariantString,
            };
            if touchbar_component_sdk::dbus_call(&call).is_err() {
                STATE.store(FAILED, Ordering::Relaxed);
            }
        } else {
            let call = DbusCall {
                bus: DbusBus::Session,
                destination: "org.mpris.MediaPlayer2.playerctld".into(),
                path: "/org/mpris/MediaPlayer2".into(),
                interface: "org.mpris.MediaPlayer2.Player".into(),
                member: "PlayPause".into(),
                arguments: Vec::new(),
                reply: DbusReplyKind::Unit,
            };
            if touchbar_component_sdk::dbus_call(&call).is_err() {
                STATE.store(FAILED, Ordering::Relaxed);
            }
        }
        Ok(Update {
            rerender: true,
            presentation: None,
        })
    }

    fn handle_presentation_event(_event: ui::PresentationEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }

    fn handle_host_event(event: broker::HostEvent) -> Result<Update, String> {
        let rerender = match event {
            broker::HostEvent::Completion((
                request_id,
                broker::OperationResult::Success(payload),
            )) if request_id == SUBSCRIBE_REQUEST.load(Ordering::Relaxed) => {
                match touchbar_component_sdk::decode_dbus_subscription_opened(&payload) {
                    Ok(resource) => {
                        SUBSCRIPTION_RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                        SUBSCRIBE_REQUEST.store(0, Ordering::Relaxed);
                    }
                    Err(_) => STATE.store(FAILED, Ordering::Relaxed),
                }
                true
            }
            broker::HostEvent::Completion((_, broker::OperationResult::Success(payload))) => {
                let state = match touchbar_component_sdk::decode_dbus_reply(&payload) {
                    Ok(DbusReply::Unit) => TOGGLED,
                    Ok(DbusReply::String(status)) if status == "Playing" => PLAYING,
                    Ok(DbusReply::String(_)) => PAUSED,
                    _ => FAILED,
                };
                STATE.store(state, Ordering::Relaxed);
                true
            }
            broker::HostEvent::ResourceEvent((
                resource_id,
                _,
                broker::OperationResult::Success(payload),
            )) if resource_id == SUBSCRIPTION_RESOURCE.load(Ordering::Relaxed) => {
                let state = touchbar_component_sdk::decode_dbus_properties_changed(&payload)
                    .ok()
                    .and_then(|event| {
                        event.changed_properties.into_iter().find_map(|property| {
                            if property.name != "PlaybackStatus" {
                                return None;
                            }
                            match property.value {
                                DbusValue::String(status) => Some(status),
                                _ => None,
                            }
                        })
                    })
                    .map(|status| if status == "Playing" { PLAYING } else { PAUSED });
                if let Some(state) = state {
                    STATE.store(state, Ordering::Relaxed);
                }
                true
            }
            broker::HostEvent::ResourceEvent((resource_id, _, _)) => {
                if resource_id == SUBSCRIPTION_RESOURCE.load(Ordering::Relaxed) {
                    SUBSCRIPTION_RESOURCE.store(0, Ordering::Relaxed);
                    STATE.store(FAILED, Ordering::Relaxed);
                }
                true
            }
            broker::HostEvent::Completion((_, broker::OperationResult::Error(_)))
            | broker::HostEvent::Shutdown(_) => {
                STATE.store(FAILED, Ordering::Relaxed);
                true
            }
            broker::HostEvent::CapabilityChanged(_) | broker::HostEvent::Overflow(_) => true,
        };
        Ok(Update {
            rerender,
            presentation: None,
        })
    }
}

fn flex_child(node: u32) -> ui::FlexChild {
    ui::FlexChild {
        node,
        layout: ui::Flex {
            minimum: 40.0,
            basis: 90.0,
            maximum: 180.0,
            grow: 1.0,
            shrink: 1.0,
            visibility_priority: 0,
            required: true,
            intrinsic: false,
        },
    }
}

touchbar_component_sdk::export!(Demo);

#[cfg(test)]
mod tests {
    use touchbar_package::PluginManifest;
    use touchbar_policy::{CapabilityId, CapabilityRegistry};

    #[test]
    fn reference_manifest_is_current_and_normalizes_to_the_dbus_call_scope() {
        let manifest = PluginManifest::from_toml(include_str!("../touchbar-plugin.toml")).unwrap();
        let permissions = CapabilityRegistry::default().normalize(&manifest).unwrap();
        assert_eq!(permissions.len(), 2);
        assert_eq!(permissions[0].capability, CapabilityId::DbusCallV1);
        assert_eq!(permissions[1].capability, CapabilityId::DbusSubscribeV1);
    }
}
