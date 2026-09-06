use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use touchbar_component_sdk::{
    Guest, Item, RenderRequest, Update, View,
    bindings::touchbar::plugin::{broker, ui},
};

struct Demo;

const READY: u8 = 1;
const FAILED: u8 = 2;
static STATE: AtomicU8 = AtomicU8::new(0);
static REQUEST: AtomicU64 = AtomicU64::new(0);
static VALUE_BYTES: AtomicU64 = AtomicU64::new(0);

impl Guest for Demo {
    fn items() -> Vec<Item> {
        vec![Item {
            id: "secret-demo".into(),
            label: "Secret demo".into(),
        }]
    }

    fn render(_request: RenderRequest) -> Result<View, String> {
        let state = STATE.load(Ordering::Relaxed);
        let label = if state & FAILED != 0 {
            "FAILED".into()
        } else if state & READY != 0 {
            format!("SECRET {} B", VALUE_BYTES.load(Ordering::Relaxed))
        } else {
            "READ SECRET".into()
        };
        Ok(View {
            root: 0,
            nodes: vec![ui::Node::Button(ui::ButtonNode {
                widget_id: 1,
                label,
                icon: None,
                pressed: false,
                selected: REQUEST.load(Ordering::Relaxed) != 0,
            })],
        })
    }

    fn handle_event(event: ui::InputEvent) -> Result<Update, String> {
        if event.widget_id != 1 || event.kind != ui::InputKind::Activated {
            return Ok(Update {
                rerender: false,
                presentation: None,
            });
        }
        match touchbar_component_sdk::read_secret("demo-token") {
            Ok(request) => REQUEST.store(request.into_raw(), Ordering::Relaxed),
            Err(_) => {
                STATE.fetch_or(FAILED, Ordering::Relaxed);
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
        match event {
            broker::HostEvent::Completion((request, broker::OperationResult::Success(payload)))
                if request == REQUEST.load(Ordering::Relaxed) =>
            {
                REQUEST.store(0, Ordering::Relaxed);
                match touchbar_component_sdk::decode_secret(&payload) {
                    Ok(value) if value.content_type == "application/octet-stream" => {
                        VALUE_BYTES.store(value.bytes.len() as u64, Ordering::Relaxed);
                        STATE.fetch_or(READY, Ordering::Relaxed);
                    }
                    _ => {
                        STATE.fetch_or(FAILED, Ordering::Relaxed);
                    }
                }
            }
            broker::HostEvent::Completion((request, broker::OperationResult::Error(_)))
                if request == REQUEST.load(Ordering::Relaxed) =>
            {
                REQUEST.store(0, Ordering::Relaxed);
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

touchbar_component_sdk::export!(Demo);

#[cfg(test)]
mod tests {
    use touchbar_package::PluginManifest;
    use touchbar_policy::{CapabilityId, CapabilityRegistry, CapabilityScope};

    #[test]
    fn manifest_requests_only_one_logical_secret() {
        let manifest = PluginManifest::from_toml(include_str!("../touchbar-plugin.toml")).unwrap();
        let permissions = CapabilityRegistry::default().normalize(&manifest).unwrap();
        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions[0].capability, CapabilityId::SecretReadV1);
        let CapabilityScope::SecretRead(scope) = &permissions[0].scope else {
            panic!("wrong secret scope")
        };
        assert_eq!(
            scope.logical_names,
            ["demo-token".into()].into_iter().collect()
        );
    }
}
