use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use touchbar_broker_schema::AppearancePublish;
use touchbar_policy::{CapabilityId, CapabilityScope};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult, Seqpacket};

use crate::{
    ActivationLedger, Backend, BackendRequest, CancellationToken,
    filesystem::{authorize_appearance_read, execute_appearance_read},
};

pub const APPEARANCE_READ_FILE_OPERATION: &str = "read-file";
pub const APPEARANCE_PUBLISH_OPERATION: &str = "publish";

pub struct AppearanceProviderBackend {
    provider: String,
    sink: Mutex<Seqpacket>,
    publications: Mutex<VecDeque<Instant>>,
}

impl AppearanceProviderBackend {
    pub fn new(provider: impl Into<String>, sink: Seqpacket) -> Self {
        Self {
            provider: provider.into(),
            sink: Mutex::new(sink),
            publications: Mutex::new(VecDeque::new()),
        }
    }

    fn decode_publish(
        &self,
        request: &BackendRequest,
    ) -> Result<AppearancePublish, BrokerErrorCode> {
        if request.capability != CapabilityId::AppearanceProvideV1
            || request.operation != APPEARANCE_PUBLISH_OPERATION
        {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let CapabilityScope::AppearanceProvide(scope) = &request.authorized_scope else {
            return Err(BrokerErrorCode::InvalidRequest);
        };
        let publication = AppearancePublish::decode(&request.payload)
            .map_err(|_| BrokerErrorCode::InvalidRequest)?;
        if publication.provider != self.provider || !scope.providers.contains(&self.provider) {
            return Err(BrokerErrorCode::OutOfScope);
        }
        Ok(publication)
    }

    fn scope<'a>(
        &self,
        request: &'a BackendRequest,
    ) -> Result<&'a touchbar_policy::AppearanceProvideScope, BrokerErrorCode> {
        if request.capability != CapabilityId::AppearanceProvideV1 {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let CapabilityScope::AppearanceProvide(scope) = &request.authorized_scope else {
            return Err(BrokerErrorCode::InvalidRequest);
        };
        scope
            .providers
            .contains(&self.provider)
            .then_some(scope)
            .ok_or(BrokerErrorCode::OutOfScope)
    }
}

impl Backend for AppearanceProviderBackend {
    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        self.scope(request)?;
        match request.operation.as_str() {
            APPEARANCE_READ_FILE_OPERATION => authorize_appearance_read(request).map(|_| ()),
            APPEARANCE_PUBLISH_OPERATION => self.decode_publish(request).map(|_| ()),
            _ => Err(BrokerErrorCode::InvalidRequest),
        }
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let scope = self.scope(request)?;
            match request.operation.as_str() {
                APPEARANCE_READ_FILE_OPERATION => {
                    let read = authorize_appearance_read(request)?;
                    let payload = execute_appearance_read(request, &read, cancellation, scope)?
                        .encode()
                        .map_err(|_| BrokerErrorCode::BackendFailed)?;
                    Ok(BrokerResult::Success { payload })
                }
                APPEARANCE_PUBLISH_OPERATION => {
                    let publication = self.decode_publish(request)?;
                    let now = Instant::now();
                    let mut publications = self
                        .publications
                        .lock()
                        .map_err(|_| BrokerErrorCode::Internal)?;
                    while publications
                        .front()
                        .is_some_and(|sent| now.duration_since(*sent) >= Duration::from_secs(1))
                    {
                        publications.pop_front();
                    }
                    if publications.len() >= usize::from(scope.maximum_updates_per_second) {
                        return Err(BrokerErrorCode::RateLimited);
                    }
                    let payload = publication
                        .encode()
                        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
                    self.sink
                        .lock()
                        .map_err(|_| BrokerErrorCode::Internal)?
                        .send_payload(&payload)
                        .map_err(|_| BrokerErrorCode::Unavailable)?;
                    publications.push_back(now);
                    Ok(BrokerResult::Success {
                        payload: Vec::new(),
                    })
                }
                _ => Err(BrokerErrorCode::InvalidRequest),
            }
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        sync::{Arc, atomic::AtomicBool},
    };

    use semver::Version;
    use tempfile::tempdir;
    use touchbar_broker_schema::{FilesystemFileChunk, FilesystemReadFile};
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        AppearanceProvideScope, FilesystemMountBinding, FilesystemMountRequest, GrantBindings,
        PackageInstance, Provenance, RuntimeKind,
    };

    use crate::ConnectionIdentity;

    use super::*;

    fn publication(provider: &str) -> AppearancePublish {
        let color = touchbar_broker_schema::AppearanceColor {
            red: 1,
            green: 2,
            blue: 3,
        };
        AppearancePublish {
            provider: provider.into(),
            scheme: touchbar_broker_schema::AppearanceScheme::Dark,
            background: color,
            foreground: color,
            accent: color,
            selection: color,
            muted: color,
            destructive: color,
        }
    }

    fn request(root: &std::path::Path, operation: &str, payload: Vec<u8>) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 1,
                package: PackageInstance {
                    source: GithubSource::new("alice", "appearance-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::AppearanceProvideV1,
            authorized_scope: CapabilityScope::AppearanceProvide(AppearanceProvideScope {
                providers: BTreeSet::from(["desktop".into()]),
                mounts: BTreeSet::from([FilesystemMountRequest {
                    label: "desktop-state".into(),
                    suggested_location: None,
                }]),
                maximum_file_bytes: 4096,
                maximum_updates_per_second: 2,
            }),
            bindings: GrantBindings {
                filesystem_mounts: BTreeMap::from([(
                    "desktop-state".into(),
                    FilesystemMountBinding::from_directory(root).unwrap(),
                )]),
                ..GrantBindings::default()
            },
            activation: None,
            operation: operation.into(),
            payload,
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn provider_backend_exposes_only_bounded_reads_and_authenticated_publication() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("colors.toml"), b"mode = \"dark\"\n").unwrap();
        let (sink, receiver) = Seqpacket::pair().unwrap();
        let backend = AppearanceProviderBackend::new("desktop", sink);

        let read = request(
            root.path(),
            APPEARANCE_READ_FILE_OPERATION,
            FilesystemReadFile {
                mount: "desktop-state".into(),
                path: "colors.toml".into(),
                offset: 0,
                maximum_bytes: 4096,
            }
            .encode()
            .unwrap(),
        );
        assert!(matches!(
            backend.execute(&read, &token()),
            BrokerResult::Success { payload }
                if FilesystemFileChunk::decode(&payload).unwrap().bytes == b"mode = \"dark\"\n"
        ));

        let publish = publication("desktop");
        let publish_request = request(
            root.path(),
            APPEARANCE_PUBLISH_OPERATION,
            publish.encode().unwrap(),
        );
        assert!(matches!(
            backend.execute(&publish_request, &token()),
            BrokerResult::Success { .. }
        ));
        assert_eq!(
            AppearancePublish::decode(&receiver.try_recv_payload().unwrap().unwrap()).unwrap(),
            publish
        );

        let wrong = request(
            root.path(),
            APPEARANCE_PUBLISH_OPERATION,
            publication("other").encode().unwrap(),
        );
        assert_eq!(
            backend.execute(&wrong, &token()),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
    }
}
