use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use touchbar_component_sdk::{
    Guest, Item, RenderRequest, Update, View,
    bindings::touchbar::plugin::{broker, ui},
};

struct Demo;

const READ: u8 = 1;
const WROTE: u8 = 2;
const FAILED: u8 = 4;
static STATE: AtomicU8 = AtomicU8::new(0);
static READ_REQUEST: AtomicU64 = AtomicU64::new(0);
static WRITE_REQUEST: AtomicU64 = AtomicU64::new(0);
static READ_BYTES: AtomicU64 = AtomicU64::new(0);

impl Guest for Demo {
    fn items() -> Vec<Item> {
        vec![Item {
            id: "clipboard-demo".into(),
            label: "Clipboard demo".into(),
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
                        "FAILED".into()
                    } else if state & READ != 0 {
                        format!("READ {} B", READ_BYTES.load(Ordering::Relaxed))
                    } else {
                        "READ TEXT".into()
                    },
                    icon: None,
                    pressed: false,
                    selected: READ_REQUEST.load(Ordering::Relaxed) != 0,
                }),
                ui::Node::Button(ui::ButtonNode {
                    widget_id: 2,
                    label: if failed {
                        "FAILED"
                    } else if state & WROTE != 0 {
                        "WROTE"
                    } else {
                        "WRITE TEXT"
                    }
                    .into(),
                    icon: None,
                    pressed: false,
                    selected: WRITE_REQUEST.load(Ordering::Relaxed) != 0,
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
            1 => touchbar_component_sdk::clipboard_read("text/plain;charset=utf-8")
                .map(|request| READ_REQUEST.store(request.into_raw(), Ordering::Relaxed)),
            2 => touchbar_component_sdk::clipboard_write(
                "text/plain;charset=utf-8",
                b"copied by plugin",
            )
            .map(|request| WRITE_REQUEST.store(request.into_raw(), Ordering::Relaxed)),
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
            broker::HostEvent::Completion((request, broker::OperationResult::Success(payload)))
                if request == READ_REQUEST.load(Ordering::Relaxed) =>
            {
                READ_REQUEST.store(0, Ordering::Relaxed);
                match touchbar_component_sdk::decode_clipboard(&payload) {
                    Ok(value) if value.mime_type == "text/plain;charset=utf-8" => {
                        READ_BYTES.store(value.bytes.len() as u64, Ordering::Relaxed);
                        STATE.fetch_or(READ, Ordering::Relaxed);
                    }
                    _ => {
                        STATE.fetch_or(FAILED, Ordering::Relaxed);
                    }
                }
            }
            broker::HostEvent::Completion((request, broker::OperationResult::Success(_)))
                if request == WRITE_REQUEST.load(Ordering::Relaxed) =>
            {
                WRITE_REQUEST.store(0, Ordering::Relaxed);
                STATE.fetch_or(WROTE, Ordering::Relaxed);
            }
            broker::HostEvent::Completion((request, broker::OperationResult::Error(_)))
                if request == READ_REQUEST.load(Ordering::Relaxed)
                    || request == WRITE_REQUEST.load(Ordering::Relaxed) =>
            {
                READ_REQUEST.store(0, Ordering::Relaxed);
                WRITE_REQUEST.store(0, Ordering::Relaxed);
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
    fn manifest_separates_read_and_write_authority() {
        let manifest = PluginManifest::from_toml(include_str!("../touchbar-plugin.toml")).unwrap();
        let permissions = CapabilityRegistry::default().normalize(&manifest).unwrap();
        assert_eq!(permissions.len(), 2);
        assert_eq!(permissions[0].capability, CapabilityId::ClipboardReadV1);
        assert_eq!(permissions[1].capability, CapabilityId::ClipboardWriteV1);
        for permission in permissions {
            let CapabilityScope::Clipboard(scope) = permission.scope else {
                panic!("wrong clipboard scope")
            };
            assert_eq!(scope.maximum_bytes, 4096);
            assert_eq!(scope.mime_types.len(), 1);
        }
    }
}
