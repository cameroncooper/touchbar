use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use touchbar_component_sdk::{
    Guest, Item, NotificationSend, NotificationUrgencyValue, RenderRequest, Update, UriOpenRequest,
    View,
    bindings::touchbar::plugin::{broker, ui},
};

struct Demo;

const NOTIFIED: u8 = 1;
const OPENED: u8 = 2;
const FAILED: u8 = 4;
static STATE: AtomicU8 = AtomicU8::new(0);
static NOTIFICATION_REQUEST: AtomicU64 = AtomicU64::new(0);
static URI_REQUEST: AtomicU64 = AtomicU64::new(0);

impl Guest for Demo {
    fn items() -> Vec<Item> {
        vec![Item {
            id: "desktop-actions".into(),
            label: "Desktop actions".into(),
        }]
    }

    fn render(_request: RenderRequest) -> Result<View, String> {
        let state = STATE.load(Ordering::Relaxed);
        let failed = state & FAILED != 0;
        Ok(View {
            root: 2,
            nodes: vec![
                ui::Node::Button(ui::ButtonNode {
                    widget_id: 1,
                    label: if failed {
                        "FAILED"
                    } else if state & NOTIFIED != 0 {
                        "SENT"
                    } else {
                        "NOTIFY"
                    }
                    .into(),
                    icon: None,
                    pressed: false,
                    selected: NOTIFICATION_REQUEST.load(Ordering::Relaxed) != 0,
                }),
                ui::Node::Button(ui::ButtonNode {
                    widget_id: 2,
                    label: if failed {
                        "FAILED"
                    } else if state & OPENED != 0 {
                        "OPENED"
                    } else {
                        "OPEN DOCS"
                    }
                    .into(),
                    icon: None,
                    pressed: false,
                    selected: URI_REQUEST.load(Ordering::Relaxed) != 0,
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
        if event.kind != ui::InputKind::Activated {
            return Ok(Update {
                rerender: false,
                presentation: None,
            });
        }
        let submitted = match event.widget_id {
            1 => touchbar_component_sdk::send_notification(&NotificationSend {
                id: "demo-ready".into(),
                category: "status".into(),
                urgency: NotificationUrgencyValue::Normal,
                title: "TouchBar plugin ready".into(),
                body: "The sandboxed notification request completed.".into(),
            })
            .map(|request| {
                NOTIFICATION_REQUEST.store(request.into_raw(), Ordering::Relaxed);
            })
            .map_err(|_| ()),
            2 => touchbar_component_sdk::open_uri(&UriOpenRequest {
                uri:
                    "https://github.com/cameroncooper/touchbar/blob/main/docs/fixes/09-touch-bar.md"
                        .into(),
            })
            .map(|request| {
                URI_REQUEST.store(request.into_raw(), Ordering::Relaxed);
            })
            .map_err(|_| ()),
            _ => {
                return Ok(Update {
                    rerender: false,
                    presentation: None,
                });
            }
        };
        if submitted.is_err() {
            STATE.fetch_or(FAILED, Ordering::Relaxed);
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
        match event {
            broker::HostEvent::Completion((request, broker::OperationResult::Success(_)))
                if request == NOTIFICATION_REQUEST.load(Ordering::Relaxed) =>
            {
                NOTIFICATION_REQUEST.store(0, Ordering::Relaxed);
                STATE.fetch_or(NOTIFIED, Ordering::Relaxed);
            }
            broker::HostEvent::Completion((request, broker::OperationResult::Success(_)))
                if request == URI_REQUEST.load(Ordering::Relaxed) =>
            {
                URI_REQUEST.store(0, Ordering::Relaxed);
                STATE.fetch_or(OPENED, Ordering::Relaxed);
            }
            broker::HostEvent::Completion((request, broker::OperationResult::Error(_)))
                if request == NOTIFICATION_REQUEST.load(Ordering::Relaxed)
                    || request == URI_REQUEST.load(Ordering::Relaxed) =>
            {
                NOTIFICATION_REQUEST.store(0, Ordering::Relaxed);
                URI_REQUEST.store(0, Ordering::Relaxed);
                STATE.fetch_or(FAILED, Ordering::Relaxed);
            }
            broker::HostEvent::Shutdown(_) => {
                STATE.fetch_or(FAILED, Ordering::Relaxed);
            }
            broker::HostEvent::CapabilityChanged(_)
            | broker::HostEvent::Overflow(_)
            | broker::HostEvent::ResourceEvent(_)
            | broker::HostEvent::Completion(_) => {}
        }
        Ok(Update {
            rerender: true,
            presentation: None,
        })
    }
}

fn flex_child(node: u32) -> ui::FlexChild {
    ui::FlexChild {
        node,
        layout: ui::Flex {
            minimum: 72.0,
            basis: 150.0,
            maximum: 240.0,
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
    use touchbar_policy::{CapabilityId, CapabilityRegistry, CapabilityScope};

    #[test]
    fn manifest_has_narrow_desktop_action_scopes() {
        let manifest = PluginManifest::from_toml(include_str!("../touchbar-plugin.toml")).unwrap();
        let permissions = CapabilityRegistry::default().normalize(&manifest).unwrap();
        assert_eq!(permissions.len(), 2);
        assert_eq!(permissions[0].capability, CapabilityId::NotificationSendV1);
        assert_eq!(permissions[1].capability, CapabilityId::UriOpenV1);
        let CapabilityScope::NotificationSend(notification) = &permissions[0].scope else {
            panic!("wrong notification scope")
        };
        assert_eq!(notification.maximum_per_minute, 4);
        let CapabilityScope::UriOpen(uri) = &permissions[1].scope else {
            panic!("wrong URI scope")
        };
        assert_eq!(uri.origins.iter().next().unwrap().host, "github.com");
    }
}
