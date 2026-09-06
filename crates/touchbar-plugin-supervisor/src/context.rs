use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::Deserialize;
use touchbar_broker_schema::{
    ContextFact, ContextFactValue, ContextReadRequest, ContextSnapshot, ContextSubscriptionOpened,
    MAX_CONTEXT_VALUE_BYTES, SchemaError,
};
use touchbar_policy::{CapabilityId, CapabilityScope};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

use crate::{
    ActivationLedger, Backend, BackendRequest, CancellationToken, OpenedResource, ResourceBackend,
    ResourceEventSink, ResourceHandle, ResourceLimits, local::connect_same_user_socket,
};

pub const CONTEXT_SNAPSHOT_OPERATION: &str = "snapshot";
pub const CONTEXT_SUBSCRIBE_OPERATION: &str = "subscribe";
const CONTEXT_RESERVED_BYTES: usize = 32 * 1024;
const HYPRLAND_LINE_BYTES: usize = 4096;
const HYPRLAND_REPLY_BYTES: u64 = 64 * 1024;
const SOURCE_RETRY: Duration = Duration::from_millis(500);
const SOURCE_IO_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Default)]
struct ContextState {
    generation: u64,
    facts: BTreeMap<String, ContextFactValue>,
    subscriptions: BTreeMap<u64, ContextSubscription>,
}

struct ContextSubscription {
    facts: BTreeSet<String>,
    events: ResourceEventSink,
}

pub struct ContextReadBackend {
    state: Arc<Mutex<ContextState>>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    start_hyprland: bool,
}

impl ContextReadBackend {
    pub fn new() -> Self {
        Self::new_internal(false)
    }

    fn new_internal(start_hyprland: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(ContextState {
                generation: 1,
                ..Default::default()
            })),
            stop: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            start_hyprland,
        }
    }

    pub fn with_hyprland_source() -> Self {
        Self::new_internal(true)
    }

    pub fn publish(&self, key: &str, value: ContextFactValue) -> Result<(), BrokerErrorCode> {
        publish(&self.state, key, value)
    }

    fn snapshot(&self, requested: &BTreeSet<String>) -> Result<ContextSnapshot, BrokerErrorCode> {
        snapshot(&self.state, requested)
    }

    fn ensure_worker(&self) -> Result<(), BrokerErrorCode> {
        if !self.start_hyprland {
            return Ok(());
        }
        let mut worker = self.worker.lock().map_err(|_| BrokerErrorCode::Internal)?;
        if worker.is_some() {
            return Ok(());
        }
        let state = Arc::clone(&self.state);
        let stop = Arc::clone(&self.stop);
        *worker = Some(
            thread::Builder::new()
                .name("touchbar-context-hyprland".into())
                .spawn(move || run_hyprland_source(state, stop))
                .map_err(|_| BrokerErrorCode::Unavailable)?,
        );
        Ok(())
    }
}

impl Default for ContextReadBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for ContextReadBackend {
    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_and_authorize(request, CONTEXT_SNAPSHOT_OPERATION)?;
        self.ensure_worker()
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let requested = decode_and_authorize(request, CONTEXT_SNAPSHOT_OPERATION)?;
            let payload = self.snapshot(&requested)?.encode().map_err(schema_error)?;
            cancellation
                .reason()
                .map_or(Ok(BrokerResult::Success { payload }), Err)
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

impl ResourceBackend for ContextReadBackend {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        let CapabilityScope::ContextRead(scope) = &request.authorized_scope else {
            return None;
        };
        (request.capability == CapabilityId::ContextReadV1
            && request.operation == CONTEXT_SUBSCRIBE_OPERATION)
            .then_some(ResourceLimits {
                reserved_buffered_bytes: CONTEXT_RESERVED_BYTES,
                maximum_events_per_second: scope.maximum_updates_per_second,
            })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_and_authorize(request, CONTEXT_SUBSCRIBE_OPERATION)?;
        self.ensure_worker()
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        let requested = decode_and_authorize(request, CONTEXT_SUBSCRIBE_OPERATION)?;
        let mut state = self.state.lock().map_err(|_| BrokerErrorCode::Internal)?;
        if state.subscriptions.contains_key(&resource_id) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let current = snapshot_locked(&state, &requested);
        let response_payload = ContextSubscriptionOpened {
            resource_id,
            snapshot: current,
        }
        .encode()
        .map_err(schema_error)?;
        state.subscriptions.insert(
            resource_id,
            ContextSubscription {
                facts: requested,
                events,
            },
        );
        Ok(OpenedResource {
            handle: Box::new(ContextSubscriptionHandle {
                resource_id,
                state: Arc::clone(&self.state),
            }),
            response_payload,
        })
    }
}

fn decode_and_authorize(
    request: &BackendRequest,
    operation: &str,
) -> Result<BTreeSet<String>, BrokerErrorCode> {
    if request.capability != CapabilityId::ContextReadV1 || request.operation != operation {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let requested = ContextReadRequest::decode(&request.payload).map_err(schema_error)?;
    let CapabilityScope::ContextRead(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    if !requested.facts.is_subset(&scope.facts) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    if requested.facts.iter().any(|fact| !supported_fact(fact)) {
        return Err(BrokerErrorCode::Unsupported);
    }
    Ok(requested.facts)
}

fn supported_fact(fact: &str) -> bool {
    matches!(fact, "application.id" | "workspace.id")
}

fn snapshot(
    state: &Mutex<ContextState>,
    requested: &BTreeSet<String>,
) -> Result<ContextSnapshot, BrokerErrorCode> {
    let state = state.lock().map_err(|_| BrokerErrorCode::Internal)?;
    Ok(snapshot_locked(&state, requested))
}

fn snapshot_locked(state: &ContextState, requested: &BTreeSet<String>) -> ContextSnapshot {
    ContextSnapshot {
        generation: state.generation,
        facts: requested
            .iter()
            .filter_map(|key| {
                state.facts.get(key).cloned().map(|value| ContextFact {
                    key: key.clone(),
                    value,
                })
            })
            .collect(),
    }
}

fn publish(
    state: &Mutex<ContextState>,
    key: &str,
    value: ContextFactValue,
) -> Result<(), BrokerErrorCode> {
    if !supported_fact(key) || !valid_value(&value) {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let mut state = state.lock().map_err(|_| BrokerErrorCode::Internal)?;
    if state.facts.get(key) == Some(&value) {
        return Ok(());
    }
    state.generation = state.generation.saturating_add(1);
    state.facts.insert(key.into(), value.clone());
    let payload = ContextSnapshot {
        generation: state.generation,
        facts: vec![ContextFact {
            key: key.into(),
            value,
        }],
    }
    .encode()
    .map_err(schema_error)?;
    for subscription in state.subscriptions.values() {
        if subscription.facts.contains(key) && !subscription.events.is_finished() {
            let _ = subscription.events.emit(payload.clone());
        }
    }
    Ok(())
}

fn valid_value(value: &ContextFactValue) -> bool {
    match value {
        ContextFactValue::Text(value) => {
            !value.is_empty()
                && value.len() <= MAX_CONTEXT_VALUE_BYTES
                && !value.chars().any(char::is_control)
        }
        ContextFactValue::Boolean(_) => true,
    }
}

struct ContextSubscriptionHandle {
    resource_id: u64,
    state: Arc<Mutex<ContextState>>,
}

impl ResourceHandle for ContextSubscriptionHandle {
    fn close(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.subscriptions.remove(&self.resource_id);
        }
    }
}

impl Drop for ContextReadBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn run_hyprland_source(state: Arc<Mutex<ContextState>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        let Some((command_socket, event_socket)) = hyprland_sockets() else {
            interruptible_pause(&stop, SOURCE_RETRY);
            continue;
        };
        if let Some(application) = query_hyprland(&command_socket, "j/activewindow")
            .and_then(|reply| parse_active_window(&reply))
        {
            let _ = publish(
                &state,
                "application.id",
                ContextFactValue::Text(application),
            );
        }
        if let Some(workspace) = query_hyprland(&command_socket, "j/activeworkspace")
            .and_then(|reply| parse_active_workspace(&reply))
        {
            let _ = publish(&state, "workspace.id", ContextFactValue::Text(workspace));
        }
        let Ok(mut stream) = connect_same_user_socket(&event_socket) else {
            interruptible_pause(&stop, SOURCE_RETRY);
            continue;
        };
        let _ = stream.set_read_timeout(Some(SOURCE_IO_TIMEOUT));
        let _ = read_hyprland_events(&mut stream, &state, &stop);
        interruptible_pause(&stop, SOURCE_RETRY);
    }
}

fn hyprland_sockets() -> Option<(PathBuf, PathBuf)> {
    let signature = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if signature.is_empty()
        || signature.len() > 128
        || !signature
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    [
        PathBuf::from(format!("/run/user/{uid}/hypr/{signature}")),
        PathBuf::from(format!("/tmp/hypr/{signature}")),
    ]
    .into_iter()
    .find_map(|root| {
        let command = root.join(".socket.sock");
        let events = root.join(".socket2.sock");
        (command.exists() && events.exists()).then_some((command, events))
    })
}

fn query_hyprland(socket: &Path, command: &str) -> Option<Vec<u8>> {
    let mut stream = connect_same_user_socket(socket).ok()?;
    stream.set_read_timeout(Some(SOURCE_IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(SOURCE_IO_TIMEOUT)).ok()?;
    stream.write_all(command.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut bytes = Vec::new();
    stream
        .take(HYPRLAND_REPLY_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= HYPRLAND_REPLY_BYTES).then_some(bytes)
}

#[derive(Deserialize)]
struct ActiveWindow {
    class: String,
}

fn parse_active_window(bytes: &[u8]) -> Option<String> {
    let value: ActiveWindow = serde_json::from_slice(bytes).ok()?;
    normalize_application(&value.class)
}

#[derive(Deserialize)]
struct ActiveWorkspace {
    id: i64,
    name: String,
}

fn parse_active_workspace(bytes: &[u8]) -> Option<String> {
    let value: ActiveWorkspace = serde_json::from_slice(bytes).ok()?;
    normalize_text(&value.name).or_else(|| Some(value.id.to_string()))
}

fn read_hyprland_events(
    stream: &mut UnixStream,
    state: &Mutex<ContextState>,
    stop: &AtomicBool,
) -> Result<(), ()> {
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !stop.load(Ordering::Acquire) {
        match stream.read(&mut buffer) {
            Ok(0) => return Err(()),
            Ok(count) => {
                pending.extend_from_slice(&buffer[..count]);
                if pending.len() > HYPRLAND_LINE_BYTES && !pending.contains(&b'\n') {
                    return Err(());
                }
                while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                    let line = pending.drain(..=newline).collect::<Vec<_>>();
                    if line.len() > HYPRLAND_LINE_BYTES {
                        return Err(());
                    }
                    if let Ok(line) = std::str::from_utf8(&line[..line.len() - 1]) {
                        publish_hyprland_event(state, line);
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

fn publish_hyprland_event(state: &Mutex<ContextState>, line: &str) {
    let Some((name, payload)) = line.split_once(">>") else {
        return;
    };
    let update = match name {
        "activewindow" => {
            normalize_application(payload.split_once(',').map_or(payload, |(class, _)| class))
                .map(|value| ("application.id", value))
        }
        "workspace" => normalize_text(payload).map(|value| ("workspace.id", value)),
        "workspacev2" => {
            normalize_text(payload.rsplit_once(',').map_or(payload, |(_, value)| value))
                .map(|value| ("workspace.id", value))
        }
        "focusedmon" => payload
            .split_once(',')
            .and_then(|(_, workspace)| normalize_text(workspace))
            .map(|value| ("workspace.id", value)),
        _ => None,
    };
    if let Some((key, value)) = update {
        let _ = publish(state, key, ContextFactValue::Text(value));
    }
}

fn normalize_application(value: &str) -> Option<String> {
    normalize_text(value).map(|value| value.to_ascii_lowercase())
}

fn normalize_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_CONTEXT_VALUE_BYTES
        && !value.chars().any(char::is_control))
    .then(|| value.to_owned())
}

fn interruptible_pause(stop: &AtomicBool, duration: Duration) {
    let slices = duration.as_millis().div_ceil(50) as usize;
    for _ in 0..slices {
        if stop.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        sync::{Arc, atomic::AtomicBool},
    };

    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        ContextReadScope, GrantBindings, PackageInstance, Provenance, RuntimeKind,
    };

    use super::*;
    use crate::{ConnectionIdentity, HostEvent, HostEventQueue, ResourceManager};

    fn request(
        operation: &str,
        requested: &[&str],
        approved: &[&str],
        rate: u16,
    ) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 5,
                package: PackageInstance {
                    source: GithubSource::new("alice", "context-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::ContextReadV1,
            authorized_scope: CapabilityScope::ContextRead(ContextReadScope {
                facts: approved.iter().map(|value| (*value).into()).collect(),
                maximum_updates_per_second: rate,
            }),
            bindings: GrantBindings::default(),
            activation: None,
            operation: operation.into(),
            payload: ContextReadRequest {
                facts: requested.iter().map(|value| (*value).into()).collect(),
            }
            .encode()
            .unwrap(),
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn snapshot_returns_only_exact_requested_public_facts() {
        let backend = ContextReadBackend::new();
        backend
            .publish("application.id", ContextFactValue::Text("terminal".into()))
            .unwrap();
        backend
            .publish("workspace.id", ContextFactValue::Text("coding".into()))
            .unwrap();
        let request = request(
            CONTEXT_SNAPSHOT_OPERATION,
            &["application.id"],
            &["application.id", "workspace.id"],
            10,
        );
        let BrokerResult::Success { payload } = backend.execute(&request, &token()) else {
            panic!("snapshot failed");
        };
        assert_eq!(
            ContextSnapshot::decode(&payload).unwrap().facts,
            vec![ContextFact {
                key: "application.id".into(),
                value: ContextFactValue::Text("terminal".into()),
            }]
        );
    }

    #[test]
    fn subscription_initial_snapshot_and_updates_are_filtered_and_ordered() {
        let backend = Arc::new(ContextReadBackend::new());
        backend
            .publish("application.id", ContextFactValue::Text("terminal".into()))
            .unwrap();
        let request = request(
            CONTEXT_SUBSCRIBE_OPERATION,
            &["application.id"],
            &["application.id", "workspace.id"],
            10,
        );
        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::ContextReadV1, backend.clone());
        let limits = ResourceBackend::limits(backend.as_ref(), &request).unwrap();
        let opened = resources
            .open(7, &request, limits, &mut ActivationLedger::new(1), 0)
            .unwrap();
        let opened = ContextSubscriptionOpened::decode(&opened).unwrap();
        assert_eq!(opened.resource_id, 7);
        assert_eq!(opened.snapshot.facts.len(), 1);

        backend
            .publish("workspace.id", ContextFactValue::Text("web".into()))
            .unwrap();
        backend
            .publish("application.id", ContextFactValue::Text("firefox".into()))
            .unwrap();
        let mut events = HostEventQueue::new(4, 8);
        resources.pump(&mut events, 1);
        let Some(HostEvent::ResourceEvent {
            resource_id: 7,
            sequence: 1,
            result: BrokerResult::Success { payload },
        }) = events.pop()
        else {
            panic!("filtered context update missing");
        };
        assert_eq!(
            ContextSnapshot::decode(&payload).unwrap().facts,
            vec![ContextFact {
                key: "application.id".into(),
                value: ContextFactValue::Text("firefox".into()),
            }]
        );
        assert!(events.pop().is_none());
        assert!(resources.close(7));
    }

    #[test]
    fn out_of_scope_and_unimplemented_facts_fail_before_any_snapshot() {
        let backend = ContextReadBackend::new();
        let outside = request(
            CONTEXT_SNAPSHOT_OPERATION,
            &["workspace.id"],
            &["application.id"],
            10,
        );
        assert_eq!(
            Backend::authorize(&backend, &outside, &mut ActivationLedger::new(1), 0,),
            Err(BrokerErrorCode::OutOfScope)
        );
        let unavailable = request(
            CONTEXT_SNAPSHOT_OPERATION,
            &["power.profile"],
            &["power.profile"],
            10,
        );
        assert_eq!(
            Backend::authorize(&backend, &unavailable, &mut ActivationLedger::new(1), 0,),
            Err(BrokerErrorCode::Unsupported)
        );
    }

    #[test]
    fn hyprland_parser_discards_titles_controls_and_oversized_values() {
        assert_eq!(
            parse_active_window(br#"{"class":"Firefox","title":"secret title"}"#),
            Some("firefox".into())
        );
        assert_eq!(
            parse_active_workspace(br#"{"id":3,"name":"coding"}"#),
            Some("coding".into())
        );
        assert_eq!(
            parse_active_window(format!(r#"{{"class":"{}"}}"#, "x".repeat(300)).as_bytes()),
            None
        );
        let backend = ContextReadBackend::new();
        assert_eq!(
            backend.publish(
                "application.id",
                ContextFactValue::Text("safe\nspoof".into())
            ),
            Err(BrokerErrorCode::InvalidRequest)
        );
        assert_eq!(
            backend.publish(
                "window.title",
                ContextFactValue::Text("must never flow".into())
            ),
            Err(BrokerErrorCode::InvalidRequest)
        );
    }

    #[test]
    fn pinned_hyprland_command_and_bounded_event_stream_feed_public_facts() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("command.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut command = Vec::new();
            stream.read_to_end(&mut command).unwrap();
            assert_eq!(command, b"j/activewindow");
            stream
                .write_all(br#"{"class":"Firefox","title":"never forwarded"}"#)
                .unwrap();
        });
        let reply = query_hyprland(&socket, "j/activewindow").unwrap();
        server.join().unwrap();
        assert_eq!(parse_active_window(&reply), Some("firefox".into()));

        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer
            .write_all(b"activewindow>>Terminal,Highly Sensitive Title\n")
            .unwrap();
        drop(writer);
        let state = Mutex::new(ContextState {
            generation: 1,
            ..Default::default()
        });
        assert_eq!(
            read_hyprland_events(&mut reader, &state, &AtomicBool::new(false)),
            Err(())
        );
        let snapshot = snapshot(
            &state,
            &BTreeSet::from(["application.id".into(), "window.title".into()]),
        )
        .unwrap();
        assert_eq!(
            snapshot.facts,
            vec![ContextFact {
                key: "application.id".into(),
                value: ContextFactValue::Text("terminal".into()),
            }]
        );

        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer
            .write_all(&vec![b'x'; HYPRLAND_LINE_BYTES + 1])
            .unwrap();
        drop(writer);
        assert_eq!(
            read_hyprland_events(&mut reader, &state, &AtomicBool::new(false)),
            Err(())
        );
    }
}
