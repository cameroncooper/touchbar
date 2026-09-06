use std::sync::{
    Mutex,
    atomic::{AtomicU8, AtomicU64, Ordering},
};

use touchbar_component_sdk::{
    FilesystemEntryKind, FilesystemListDirectory, FilesystemReadStream, FilesystemStreamEvent,
    FilesystemWriteFile, Guest, Item, RenderRequest, ResourceId, Update, View,
    bindings::touchbar::plugin::{broker, ui},
};

struct Demo;

const READY: u8 = 0;
const LISTING: u8 = 1;
const READING: u8 = 2;
const COMPLETE: u8 = 3;
const EMPTY: u8 = 4;
const FAILED: u8 = 5;

static STATE: AtomicU8 = AtomicU8::new(READY);
static LIST_REQUEST: AtomicU64 = AtomicU64::new(0);
static READ_REQUEST: AtomicU64 = AtomicU64::new(0);
static FILE_RESOURCE: AtomicU64 = AtomicU64::new(0);
static RESPONSE_BYTES: AtomicU64 = AtomicU64::new(0);
static WRITE_REQUEST: AtomicU64 = AtomicU64::new(0);
static WRITE_STATE: AtomicU8 = AtomicU8::new(READY);
static WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
static DETAIL: Mutex<String> = Mutex::new(String::new());

impl Guest for Demo {
    fn items() -> Vec<Item> {
        vec![
            Item {
                id: "filesystem-demo".into(),
                label: "Files".into(),
            },
            Item {
                id: "filesystem-write-demo".into(),
                label: "Save File".into(),
            },
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        if request.item_id == "filesystem-write-demo" {
            return Ok(write_view());
        }
        if request.item_id != "filesystem-demo" {
            return Err(format!("unknown item {}", request.item_id));
        }
        let state = STATE.load(Ordering::Relaxed);
        let label = match state {
            LISTING => "LISTING…".into(),
            READING => "READING…".into(),
            COMPLETE => DETAIL.lock().map_err(|_| "detail state poisoned")?.clone(),
            EMPTY => "NO FILES".into(),
            FAILED => "DENIED".into(),
            _ => "OPEN GALLERY".into(),
        };
        Ok(View {
            root: 0,
            nodes: vec![ui::Node::Button(ui::ButtonNode {
                widget_id: 1,
                label,
                icon: None,
                pressed: false,
                selected: matches!(state, LISTING | READING),
            })],
        })
    }

    fn handle_event(event: ui::InputEvent) -> Result<Update, String> {
        if event.kind != ui::InputKind::Activated {
            return Ok(Update {
                rerender: false,
                presentation: None,
            });
        }
        if event.widget_id == 2 {
            match touchbar_component_sdk::filesystem_create_file(&FilesystemWriteFile {
                mount: "workspace".into(),
                path: "touchbar-replay.txt".into(),
                bytes: b"TouchBar replay\n".to_vec(),
            }) {
                Ok(request) => {
                    WRITE_REQUEST.store(request.into_raw(), Ordering::Relaxed);
                    WRITE_STATE.store(READING, Ordering::Relaxed);
                }
                Err(_) => WRITE_STATE.store(FAILED, Ordering::Relaxed),
            }
            return Ok(Update {
                rerender: true,
                presentation: None,
            });
        }
        if event.widget_id != 1 {
            return Ok(Update {
                rerender: false,
                presentation: None,
            });
        }
        let previous = FILE_RESOURCE.swap(0, Ordering::Relaxed);
        if previous != 0 {
            let _ = touchbar_component_sdk::close(ResourceId::from_raw(previous));
        }
        let request = touchbar_component_sdk::filesystem_list_directory(&FilesystemListDirectory {
            mount: "gallery".into(),
            path: String::new(),
            maximum_entries: 64,
        });
        match request {
            Ok(request) => {
                LIST_REQUEST.store(request.into_raw(), Ordering::Relaxed);
                STATE.store(LISTING, Ordering::Relaxed);
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
            )) if request_id == WRITE_REQUEST.load(Ordering::Relaxed) => {
                WRITE_REQUEST.store(0, Ordering::Relaxed);
                let result = touchbar_component_sdk::decode_filesystem_mutation_result(&payload)
                    .map_err(|_| "invalid write response")?;
                WRITE_BYTES.store(result.bytes_written, Ordering::Relaxed);
                WRITE_STATE.store(COMPLETE, Ordering::Relaxed);
            }
            broker::HostEvent::Completion((
                request_id,
                broker::OperationResult::Success(payload),
            )) if request_id == LIST_REQUEST.load(Ordering::Relaxed) => {
                LIST_REQUEST.store(0, Ordering::Relaxed);
                let entries = touchbar_component_sdk::decode_filesystem_directory_entries(&payload)
                    .map_err(|_| "invalid directory response")?;
                let Some(file) = entries
                    .entries
                    .into_iter()
                    .find(|entry| entry.kind == FilesystemEntryKind::RegularFile)
                else {
                    STATE.store(EMPTY, Ordering::Relaxed);
                    return Ok(Update {
                        rerender: true,
                        presentation: None,
                    });
                };
                *DETAIL.lock().map_err(|_| "detail state poisoned")? = file.name.clone();
                if file.size == 0 {
                    *DETAIL.lock().map_err(|_| "detail state poisoned")? =
                        format!("{}  0 B", file.name);
                    STATE.store(COMPLETE, Ordering::Relaxed);
                    return Ok(Update {
                        rerender: true,
                        presentation: None,
                    });
                }
                RESPONSE_BYTES.store(0, Ordering::Relaxed);
                match touchbar_component_sdk::filesystem_read_stream(&FilesystemReadStream {
                    mount: "gallery".into(),
                    path: file.name,
                    offset: 0,
                    maximum_bytes: file.size.min(4 * 1024 * 1024),
                }) {
                    Ok(request) => {
                        READ_REQUEST.store(request.into_raw(), Ordering::Relaxed);
                        STATE.store(READING, Ordering::Relaxed);
                    }
                    Err(_) => STATE.store(FAILED, Ordering::Relaxed),
                }
            }
            broker::HostEvent::Completion((
                request_id,
                broker::OperationResult::Success(payload),
            )) if request_id == READ_REQUEST.load(Ordering::Relaxed) => {
                READ_REQUEST.store(0, Ordering::Relaxed);
                let resource = touchbar_component_sdk::decode_filesystem_stream_opened(&payload)
                    .map_err(|_| "invalid stream-open response")?;
                FILE_RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
            }
            broker::HostEvent::ResourceEvent((
                resource_id,
                _,
                broker::OperationResult::Success(payload),
            )) if resource_id == FILE_RESOURCE.load(Ordering::Relaxed) => {
                match touchbar_component_sdk::decode_filesystem_stream_event(&payload)
                    .map_err(|_| "invalid file stream event")?
                {
                    FilesystemStreamEvent::Metadata { total_size, .. } => {
                        let name = DETAIL.lock().map_err(|_| "detail state poisoned")?.clone();
                        *DETAIL.lock().map_err(|_| "detail state poisoned")? =
                            format!("{name}  {total_size} B");
                    }
                    FilesystemStreamEvent::Chunk { bytes, .. } => {
                        RESPONSE_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    }
                    FilesystemStreamEvent::Complete { total_bytes, .. } => {
                        FILE_RESOURCE.store(0, Ordering::Relaxed);
                        if total_bytes == RESPONSE_BYTES.load(Ordering::Relaxed) {
                            STATE.store(COMPLETE, Ordering::Relaxed);
                        } else {
                            STATE.store(FAILED, Ordering::Relaxed);
                        }
                    }
                }
            }
            broker::HostEvent::ResourceEvent((
                resource_id,
                _,
                broker::OperationResult::Error(_),
            )) if resource_id == FILE_RESOURCE.load(Ordering::Relaxed) => {
                FILE_RESOURCE.store(0, Ordering::Relaxed);
                STATE.store(FAILED, Ordering::Relaxed);
            }
            broker::HostEvent::Completion((_, broker::OperationResult::Error(_)))
            | broker::HostEvent::Shutdown(_) => STATE.store(FAILED, Ordering::Relaxed),
            broker::HostEvent::CapabilityChanged(_)
            | broker::HostEvent::Overflow(_)
            | broker::HostEvent::ResourceEvent(_) => {}
            broker::HostEvent::Completion((_, broker::OperationResult::Success(_))) => {}
        }
        Ok(Update {
            rerender: true,
            presentation: None,
        })
    }
}

fn write_view() -> View {
    let label = match WRITE_STATE.load(Ordering::Relaxed) {
        READING => "SAVING…".into(),
        COMPLETE => format!("SAVED {} B", WRITE_BYTES.load(Ordering::Relaxed)),
        FAILED => "DENIED".into(),
        _ => "SAVE DEMO".into(),
    };
    View {
        root: 0,
        nodes: vec![ui::Node::Button(ui::ButtonNode {
            widget_id: 2,
            label,
            icon: None,
            pressed: false,
            selected: WRITE_STATE.load(Ordering::Relaxed) == READING,
        })],
    }
}

touchbar_component_sdk::export!(Demo);

#[cfg(test)]
mod tests {
    use touchbar_package::PluginManifest;
    use touchbar_policy::{CapabilityId, CapabilityRegistry, CapabilityScope, FileKind};

    #[test]
    fn manifest_requests_one_bounded_logical_gallery_mount() {
        let manifest = PluginManifest::from_toml(include_str!("../touchbar-plugin.toml")).unwrap();
        let permissions = CapabilityRegistry::default().normalize(&manifest).unwrap();
        assert_eq!(permissions.len(), 2);
        let read = permissions
            .iter()
            .find(|permission| permission.capability == CapabilityId::FilesystemReadV1)
            .unwrap();
        let CapabilityScope::FilesystemRead(scope) = &read.scope else {
            panic!("wrong scope");
        };
        assert!(scope.enumerate);
        assert!(scope.kinds.contains(&FileKind::RegularFile));
        assert!(scope.kinds.contains(&FileKind::Directory));
        assert_eq!(scope.mounts.iter().next().unwrap().label, "gallery");

        let write = permissions
            .iter()
            .find(|permission| permission.capability == CapabilityId::FilesystemWriteV1)
            .unwrap();
        let CapabilityScope::FilesystemWrite(scope) = &write.scope else {
            panic!("wrong write scope");
        };
        assert_eq!(scope.mounts.iter().next().unwrap().label, "workspace");
        assert_eq!(scope.maximum_file_bytes, 4096);
    }
}
