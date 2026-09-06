use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use touchbar_component_sdk::{
    Guest, HttpRequest, HttpRequestMethod, HttpStreamEvent, Item, RenderRequest, ResourceId,
    Update, View,
    bindings::touchbar::plugin::{broker, ui},
};

struct Demo;

const IDLE: u16 = 0;
const PENDING: u16 = 1;
const FAILED: u16 = 2;
const INLINE_MODE: u16 = 1;
const STREAM_MODE: u16 = 2;
static STATE: AtomicU16 = AtomicU16::new(IDLE);
static MODE: AtomicU16 = AtomicU16::new(0);
static REQUEST_ID: AtomicU64 = AtomicU64::new(0);
static RESOURCE_ID: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BYTES: AtomicU64 = AtomicU64::new(0);

impl Guest for Demo {
    fn items() -> Vec<Item> {
        vec![
            Item {
                id: "http-demo".into(),
                label: "HTTP Stream".into(),
            },
            Item {
                id: "http-inline".into(),
                label: "HTTP Inline".into(),
            },
        ]
    }

    fn render(_request: RenderRequest) -> Result<View, String> {
        let state = STATE.load(Ordering::Relaxed);
        let label = match state {
            PENDING => "FETCHING…".into(),
            FAILED => "UNAVAILABLE".into(),
            status if status >= 100 => format!(
                "HTTP {status}  {} B",
                RESPONSE_BYTES.load(Ordering::Relaxed)
            ),
            _ => "FETCH COMMIT".into(),
        };
        Ok(View {
            root: 0,
            nodes: vec![ui::Node::Button(ui::ButtonNode {
                widget_id: 1,
                label,
                icon: None,
                pressed: false,
                selected: state == PENDING,
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
        let previous = RESOURCE_ID.swap(0, Ordering::Relaxed);
        if previous != 0 {
            let _ = touchbar_component_sdk::close(ResourceId::from_raw(previous));
        }
        let request = HttpRequest {
            method: HttpRequestMethod::Get,
            url: "https://api.github.com/repos/cameroncooper/touchbar/commits/?per_page=1".into(),
            accept: Some("application/vnd.github+json".into()),
            content_type: None,
            body: Vec::new(),
        };
        RESPONSE_BYTES.store(0, Ordering::Relaxed);
        let (mode, submitted) = if event.item_id == "http-inline" {
            (INLINE_MODE, touchbar_component_sdk::http_request(&request))
        } else {
            (
                STREAM_MODE,
                touchbar_component_sdk::http_request_stream(&request),
            )
        };
        MODE.store(mode, Ordering::Relaxed);
        match submitted {
            Ok(request_id) => {
                REQUEST_ID.store(request_id.into_raw(), Ordering::Relaxed);
                STATE.store(PENDING, Ordering::Relaxed);
            }
            Err(_) => STATE.store(FAILED, Ordering::Relaxed),
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
            broker::HostEvent::Completion((
                request_id,
                broker::OperationResult::Success(payload),
            )) if request_id == REQUEST_ID.load(Ordering::Relaxed) => {
                REQUEST_ID.store(0, Ordering::Relaxed);
                if MODE.load(Ordering::Relaxed) == INLINE_MODE {
                    match touchbar_component_sdk::decode_http_response(&payload) {
                        Ok(response) => {
                            RESPONSE_BYTES.store(response.body.len() as u64, Ordering::Relaxed);
                            STATE.store(response.status, Ordering::Relaxed);
                        }
                        Err(_) => STATE.store(FAILED, Ordering::Relaxed),
                    }
                } else {
                    match touchbar_component_sdk::decode_http_stream_opened(&payload) {
                        Ok(resource) => {
                            RESOURCE_ID.store(resource.into_raw(), Ordering::Relaxed);
                        }
                        Err(_) => STATE.store(FAILED, Ordering::Relaxed),
                    }
                }
            }
            broker::HostEvent::ResourceEvent((
                resource_id,
                _,
                broker::OperationResult::Success(payload),
            )) if resource_id == RESOURCE_ID.load(Ordering::Relaxed) => {
                match touchbar_component_sdk::decode_http_stream_event(&payload) {
                    Ok(HttpStreamEvent::Metadata { status, .. }) => {
                        STATE.store(status, Ordering::Relaxed);
                    }
                    Ok(HttpStreamEvent::Chunk(chunk)) => {
                        RESPONSE_BYTES.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    }
                    Ok(HttpStreamEvent::Complete { total_bytes }) => {
                        RESOURCE_ID.store(0, Ordering::Relaxed);
                        if total_bytes != RESPONSE_BYTES.load(Ordering::Relaxed) {
                            STATE.store(FAILED, Ordering::Relaxed);
                        }
                    }
                    Err(_) => STATE.store(FAILED, Ordering::Relaxed),
                }
            }
            broker::HostEvent::ResourceEvent((
                resource_id,
                _,
                broker::OperationResult::Error(_),
            )) if resource_id == RESOURCE_ID.load(Ordering::Relaxed) => {
                RESOURCE_ID.store(0, Ordering::Relaxed);
                STATE.store(FAILED, Ordering::Relaxed);
            }
            broker::HostEvent::Completion((request_id, broker::OperationResult::Error(_)))
                if request_id == REQUEST_ID.load(Ordering::Relaxed) =>
            {
                REQUEST_ID.store(0, Ordering::Relaxed);
                STATE.store(FAILED, Ordering::Relaxed);
            }
            broker::HostEvent::Shutdown(_) => STATE.store(FAILED, Ordering::Relaxed),
            broker::HostEvent::CapabilityChanged(_)
            | broker::HostEvent::Overflow(_)
            | broker::HostEvent::ResourceEvent(_)
            | broker::HostEvent::Completion((_, broker::OperationResult::Error(_)))
            | broker::HostEvent::Completion((_, broker::OperationResult::Success(_))) => {}
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
    use touchbar_policy::{CapabilityId, CapabilityRegistry, CapabilityScope, HttpMethod};

    #[test]
    fn manifest_has_one_public_get_only_origin() {
        let manifest = PluginManifest::from_toml(include_str!("../touchbar-plugin.toml")).unwrap();
        let permissions = CapabilityRegistry::default().normalize(&manifest).unwrap();
        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions[0].capability, CapabilityId::HttpRequestV1);
        let CapabilityScope::HttpRequest(scope) = &permissions[0].scope else {
            panic!("wrong scope")
        };
        assert_eq!(scope.methods, [HttpMethod::Get].into_iter().collect());
        assert!(!scope.private_network);
        assert_eq!(scope.origins.iter().next().unwrap().host, "api.github.com");
    }
}
