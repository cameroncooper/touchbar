//! Supervisor-owned broker connection state and default-deny routing.
//!
//! The connection identity is immutable and never decoded from host traffic.
//! Production authority exists only in explicitly registered, typed operation
//! and resource backends; every unregistered capability remains unavailable.

mod activation;
mod appearance;
mod audit;
mod clipboard;
mod command;
mod component_cgroup;
mod context;
mod dbus;
mod desktop;
mod events;
mod executor;
mod filesystem;
mod filesystem_write;
mod health;
mod http;
mod lifecycle;
mod local;
mod resources;
mod runtime;
mod secret;
mod watch;

use std::{
    collections::BTreeMap,
    io,
    os::fd::{AsRawFd, RawFd},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use touchbar_policy::{
    CapabilityId, CapabilityStatus, EffectivePolicy, GrantBindings, PackageInstance,
};
use touchbar_protocol::broker_ipc::{
    ActivationContext, BrokerErrorCode, BrokerResult, CallbackPhase, CapabilityState,
    DEFAULT_ACTIVATION_LIFETIME, HostMessage, PeerCredentials, Seqpacket, SupervisorMessage,
    TransportError, WireCapabilityStatus, monotonic_micros,
};

pub use activation::{ActivationExpectation, ActivationLedger};
pub use appearance::{
    APPEARANCE_PUBLISH_OPERATION, APPEARANCE_READ_FILE_OPERATION, AppearanceProviderBackend,
};
pub use audit::{
    AuditActivation, AuditDecision, AuditFile, AuditFileError, AuditFileLimits, AuditInput,
    AuditLog, AuditRecord, AuditResult,
};
pub use clipboard::{
    CLIPBOARD_READ_OPERATION, CLIPBOARD_WRITE_OPERATION, ClipboardAuthorization, ClipboardBackend,
    ClipboardMaterial, ClipboardTransport, WaylandClipboard, authorize_clipboard_request,
};
pub use command::{COMMAND_RUN_OPERATION, CommandRunBackend};
pub use component_cgroup::{ComponentCgroup, component_task_limit};
pub use context::{CONTEXT_SNAPSHOT_OPERATION, CONTEXT_SUBSCRIBE_OPERATION, ContextReadBackend};
pub use dbus::{
    DBUS_CALL_OPERATION, DBUS_SUBSCRIBE_OPERATION, DBUS_SUBSCRIPTION_RESERVED_BYTES,
    DbusCallBackend, DbusSubscriptionBackend, DbusSubscriptionTransport, DbusTransport,
    ZbusTransport,
};
pub use desktop::{
    NOTIFICATION_REMOVE_OPERATION, NOTIFICATION_SEND_OPERATION, NotificationAuthorization,
    NotificationBackend, NotificationTransport, URI_OPEN_OPERATION, UriOpenBackend,
    UriOpenTransport, ZbusDesktopPortal, authorize_notification_request,
};
pub use events::{HostEvent, HostEventQueue};
pub use executor::{
    Backend, BackendCompletion, BackendExecutor, BackendRequest, CancellationToken, ExecutorLimits,
};
pub use filesystem::{
    FILESYSTEM_LIST_DIRECTORY_OPERATION, FILESYSTEM_READ_FILE_OPERATION,
    FILESYSTEM_READ_STREAM_OPERATION, FilesystemReadBackend,
};
pub use filesystem_write::{
    FILESYSTEM_APPEND_FILE_OPERATION, FILESYSTEM_APPEND_FILE_STREAM_OPERATION,
    FILESYSTEM_CREATE_DIRECTORY_OPERATION, FILESYSTEM_CREATE_FILE_OPERATION,
    FILESYSTEM_CREATE_FILE_STREAM_OPERATION, FILESYSTEM_DELETE_FILE_OPERATION,
    FILESYSTEM_RENAME_OPERATION, FILESYSTEM_REPLACE_FILE_OPERATION,
    FILESYSTEM_REPLACE_FILE_STREAM_OPERATION, FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
    FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION, FilesystemWriteBackend,
    FilesystemWriteStreamCommandAuthorization, authorize_filesystem_write_mutation_request,
    authorize_filesystem_write_stream_begin_request, authorize_filesystem_write_stream_command,
};
pub use health::{HealthEvent, HealthPolicy, HealthState, HealthTracker};
pub use http::{
    HTTP_REQUEST_OPERATION, HTTP_STREAM_OPERATION, HttpRequestBackend, HttpResolver,
    HttpStreamCallbacks, HttpTransport, HttpTransportResponse, ReqwestHttpTransport,
    SystemHttpResolver,
};
pub use lifecycle::{
    BrokerResource, LifecycleLimits, LifecycleState, PendingOperation, RevocationEffect,
};
pub use local::{
    LOCAL_CONNECT_OPERATION, LOCAL_MAXIMUM_EVENTS_PER_SECOND, LOCAL_SEND_OPERATION,
    LocalConnectionAuthorization, LocalIpcBackend, authorize_local_connect_request,
    authorize_local_send_request,
};
pub use resources::{
    OpenedResource, ResourceBackend, ResourceEventSink, ResourceHandle, ResourceLimits,
    ResourceManager,
};
pub use runtime::AsyncBrokerRuntime;
pub use secret::{
    SECRET_READ_OPERATION, SecretMaterial, SecretReadBackend, SecretTransport, ZbusSecretService,
    authorize_secret_read_request, run_secret_transport_helper, validate_secret_value,
};
pub use watch::GrantStoreWatcher;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionIdentity {
    pub instance_id: u64,
    pub package: PackageInstance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionExit {
    HostDisconnected,
    PolicyBlocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionLimits {
    pub maximum_audit_records: usize,
    pub maximum_consumed_activations: usize,
    pub maximum_activation_lifetime: Duration,
    pub maximum_state_events: usize,
    pub maximum_edge_events: usize,
    pub executor: ExecutorLimits,
    pub backend_timeout: Duration,
    pub lifecycle: LifecycleLimits,
    pub health: HealthPolicy,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            maximum_audit_records: 1024,
            maximum_consumed_activations: 4096,
            maximum_activation_lifetime: DEFAULT_ACTIVATION_LIFETIME,
            maximum_state_events: 64,
            maximum_edge_events: 256,
            executor: ExecutorLimits::default(),
            backend_timeout: Duration::from_secs(5),
            lifecycle: LifecycleLimits::default(),
            health: HealthPolicy::default(),
        }
    }
}

struct PendingAudit {
    started: Instant,
    wall_clock_unix_millis: u64,
    capability: String,
    operation: String,
    activation: Option<touchbar_protocol::broker_ipc::ActivationOrigin>,
    request_bytes: u64,
}

type PolicyReload<'a> = &'a mut dyn FnMut() -> Result<Option<EffectivePolicy>, TransportError>;

pub struct SupervisorConnection {
    channel: Seqpacket,
    identity: ConnectionIdentity,
    policy: EffectivePolicy,
    generation: u64,
    limits: ConnectionLimits,
    last_request_id: u64,
    runtime: AsyncBrokerRuntime,
    activations: ActivationLedger,
    audit: AuditLog,
    audit_file: Option<AuditFile>,
    pending_audit: BTreeMap<u64, PendingAudit>,
    health: HealthTracker,
    started: Instant,
    peer: PeerCredentials,
    stopped: bool,
}

impl SupervisorConnection {
    pub fn new(
        channel: Seqpacket,
        identity: ConnectionIdentity,
        policy: EffectivePolicy,
        limits: ConnectionLimits,
    ) -> Result<Self, TransportError> {
        if limits.maximum_edge_events < limits.executor.maximum_active_operations {
            return Err(TransportError::Malformed(
                "edge event capacity must cover every active backend operation",
            ));
        }
        if limits.backend_timeout.is_zero() {
            return Err(TransportError::Malformed(
                "backend timeout must be greater than zero",
            ));
        }
        if limits.maximum_activation_lifetime.is_zero() {
            return Err(TransportError::Malformed(
                "activation lifetime must be greater than zero",
            ));
        }
        let peer = channel.peer_credentials()?;
        // SAFETY: geteuid has no preconditions and retains no pointers.
        let current_uid = unsafe { libc::geteuid() };
        if peer.uid != current_uid {
            return Err(TransportError::Malformed(
                "broker peer has a different user",
            ));
        }
        let audit = AuditLog::new(
            identity.package.source.clone(),
            identity.package.version.to_string(),
            identity.package.digest.clone(),
            identity.instance_id,
            limits.maximum_audit_records,
        );
        let runtime = AsyncBrokerRuntime::new(
            limits.executor,
            limits.lifecycle,
            limits.maximum_state_events,
            limits.maximum_edge_events,
        )
        .map_err(TransportError::Malformed)?;
        Ok(Self {
            channel,
            identity,
            policy,
            generation: 1,
            limits,
            last_request_id: 0,
            runtime,
            activations: ActivationLedger::new(limits.maximum_consumed_activations),
            audit,
            audit_file: None,
            pending_audit: BTreeMap::new(),
            health: HealthTracker::new(limits.health),
            started: Instant::now(),
            peer,
            stopped: false,
        })
    }

    pub fn identity(&self) -> &ConnectionIdentity {
        &self.identity
    }

    pub fn peer_credentials(&self) -> PeerCredentials {
        self.peer
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn policy(&self) -> &EffectivePolicy {
        &self.policy
    }

    pub fn lifecycle(&self) -> &LifecycleState {
        self.runtime.lifecycle()
    }

    pub fn lifecycle_mut(&mut self) -> &mut LifecycleState {
        self.runtime.lifecycle_mut()
    }

    pub fn activations_mut(&mut self) -> &mut ActivationLedger {
        &mut self.activations
    }

    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    pub fn set_audit_file(&mut self, audit_file: AuditFile) {
        self.audit_file = Some(audit_file);
    }

    pub fn health(&self) -> &HealthTracker {
        &self.health
    }

    pub fn events_mut(&mut self) -> &mut HostEventQueue {
        self.runtime.events_mut()
    }

    pub fn register_backend(
        &mut self,
        capability: CapabilityId,
        backend: Arc<dyn Backend>,
    ) -> Option<Arc<dyn Backend>> {
        self.runtime.register(capability, backend)
    }

    pub fn register_resource_backend(
        &mut self,
        capability: CapabilityId,
        backend: Arc<dyn ResourceBackend>,
    ) -> Option<Arc<dyn ResourceBackend>> {
        self.runtime.register_resource(capability, backend)
    }

    pub fn serve_one(&mut self) -> Result<(), TransportError> {
        if self.stopped {
            return Err(TransportError::Disconnected);
        }
        let request = match self.channel.recv_host() {
            Ok(request) => request,
            Err(error @ (TransportError::Malformed(_) | TransportError::Oversized)) => {
                self.health
                    .record(HealthEvent::ProtocolViolation, self.elapsed_millis());
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        self.handle_request(request)?;
        self.pump_completions()?;
        self.pump_resource_events();
        self.flush_events()
    }

    /// Serves the private broker channel until the host disconnects or policy
    /// revocation shuts the connection down. Worker completion uses an eventfd,
    /// so idle connections do not need a polling timer.
    pub fn serve_until_disconnect(&mut self) -> Result<(), TransportError> {
        self.serve_loop(None).map(|_| ())
    }

    pub fn serve_with_policy_updates(
        &mut self,
        update_fd: RawFd,
        reload: &mut dyn FnMut() -> Result<Option<EffectivePolicy>, TransportError>,
    ) -> Result<ConnectionExit, TransportError> {
        self.serve_loop(Some((update_fd, reload)))
    }

    fn serve_loop(
        &mut self,
        mut policy_updates: Option<(RawFd, PolicyReload<'_>)>,
    ) -> Result<ConnectionExit, TransportError> {
        while !self.stopped {
            self.pump_completions()?;
            self.pump_resource_events();
            self.flush_events()?;
            let update_fd = policy_updates.as_ref().map_or(-1, |(fd, _)| *fd);
            let mut descriptors = [
                libc::pollfd {
                    fd: self.channel.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.runtime.completion_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: update_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.runtime.resource_event_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: descriptors is writable for three pollfd values and all
            // nonnegative descriptors remain live for the duration of this call.
            let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if descriptors[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(TransportError::Malformed("broker completion signal failed"));
            }
            if descriptors[2].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(TransportError::Malformed(
                    "broker policy update signal failed",
                ));
            }
            if descriptors[3].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(TransportError::Malformed(
                    "broker resource event signal failed",
                ));
            }
            if descriptors[2].revents & libc::POLLIN != 0 {
                let (_, reload) = policy_updates
                    .as_mut()
                    .expect("active policy descriptor has a reload callback");
                if let Some(policy) = reload()? {
                    self.replace_policy(policy)?;
                    if self.stopped {
                        return Ok(ConnectionExit::PolicyBlocked);
                    }
                }
            }
            if descriptors[1].revents & libc::POLLIN != 0 {
                self.pump_completions()?;
                self.flush_events()?;
            }
            if descriptors[3].revents & libc::POLLIN != 0 {
                self.pump_resource_events();
                self.flush_events()?;
            }
            if descriptors[0].revents & libc::POLLIN != 0 {
                match self.channel.recv_host() {
                    Ok(request) => self.handle_request(request)?,
                    Err(TransportError::Disconnected) => {
                        return Ok(ConnectionExit::HostDisconnected);
                    }
                    Err(error @ (TransportError::Malformed(_) | TransportError::Oversized)) => {
                        self.health
                            .record(HealthEvent::ProtocolViolation, self.elapsed_millis());
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                }
                self.pump_completions()?;
                self.pump_resource_events();
                self.flush_events()?;
            }
            if descriptors[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(TransportError::Malformed("broker socket poll failed"));
            }
            if descriptors[0].revents & libc::POLLHUP != 0
                && descriptors[0].revents & libc::POLLIN == 0
            {
                return Ok(ConnectionExit::HostDisconnected);
            }
        }
        Ok(ConnectionExit::PolicyBlocked)
    }

    fn handle_request(&mut self, request: HostMessage) -> Result<(), TransportError> {
        let request_id = request.request_id();
        if request_id == 0 || request_id <= self.last_request_id {
            self.health
                .record(HealthEvent::ProtocolViolation, self.elapsed_millis());
            self.channel.send_supervisor(&SupervisorMessage::Response {
                request_id,
                result: BrokerResult::Error(BrokerErrorCode::InvalidRequest),
            })?;
            return Ok(());
        }
        self.last_request_id = request_id;

        let response = match request {
            HostMessage::GetCapabilities { request_id } => Some(SupervisorMessage::Capabilities {
                request_id,
                generation: self.generation,
                states: capability_states(&self.policy),
            }),
            HostMessage::Request {
                request_id,
                phase,
                capability,
                operation,
                payload,
                activation,
            } => {
                let authorized = self
                    .authorize_request(phase, &capability, &operation)
                    .and_then(|authorized| {
                        self.validate_activation_envelope(phase, activation.as_ref())?;
                        Ok(authorized)
                    });
                let audit = PendingAudit {
                    started: Instant::now(),
                    wall_clock_unix_millis: wall_clock_unix_millis(),
                    capability: audit_identifier(&capability),
                    operation: audit_identifier(&operation),
                    activation: activation.as_ref().map(|value| value.origin),
                    request_bytes: payload.len() as u64,
                };
                match authorized.and_then(|(capability, authorized_scope, bindings)| {
                    let deadline = self
                        .elapsed_micros()
                        .saturating_add(duration_micros(self.limits.backend_timeout));
                    let backend_request = BackendRequest {
                        identity: self.identity.clone(),
                        request_id,
                        capability,
                        authorized_scope,
                        bindings,
                        activation,
                        operation,
                        payload,
                    };
                    let now = monotonic_micros().map_err(|_| BrokerErrorCode::Internal)?;
                    if self.runtime.resource_limits(&backend_request).is_some() {
                        let (_, payload) = self.runtime.open_resource(
                            &backend_request,
                            &mut self.activations,
                            now,
                        )?;
                        Ok(Some(payload))
                    } else {
                        self.runtime.submit(
                            backend_request,
                            self.limits.backend_timeout,
                            deadline,
                            &mut self.activations,
                            now,
                        )?;
                        Ok(None)
                    }
                }) {
                    Ok(None) => {
                        self.pending_audit.insert(request_id, audit);
                        None
                    }
                    Ok(Some(payload)) => {
                        self.push_audit(audit, AuditDecision::Allowed, None, payload.len() as u64)?;
                        Some(SupervisorMessage::Response {
                            request_id,
                            result: BrokerResult::Success { payload },
                        })
                    }
                    Err(error) => {
                        self.push_audit(audit, audit_decision(error), Some(error), 0)?;
                        Some(SupervisorMessage::Response {
                            request_id,
                            result: BrokerResult::Error(error),
                        })
                    }
                }
            }
            HostMessage::Cancel {
                request_id,
                target_request_id,
            } => Some(SupervisorMessage::Response {
                request_id,
                result: if self.runtime.cancel(target_request_id) {
                    BrokerResult::Success {
                        payload: Vec::new(),
                    }
                } else {
                    BrokerResult::Error(BrokerErrorCode::InvalidRequest)
                },
            }),
            HostMessage::Close {
                request_id,
                resource_id,
            } => {
                let result = if self.runtime.close_resource(resource_id) {
                    BrokerResult::Success {
                        payload: Vec::new(),
                    }
                } else {
                    BrokerResult::Error(BrokerErrorCode::InvalidRequest)
                };
                Some(SupervisorMessage::Response { request_id, result })
            }
        };
        if let Some(response) = response {
            self.channel.send_supervisor(&response)?;
        }
        Ok(())
    }

    pub fn replace_policy(&mut self, policy: EffectivePolicy) -> Result<(), TransportError> {
        let old = capability_states(&self.policy);
        let new = capability_states(&policy);
        let revoked = self
            .policy
            .grants
            .iter()
            .filter(|old| old.status == CapabilityStatus::Granted)
            .filter(|old| {
                !policy.grants.iter().any(|new| {
                    new.request.capability == old.request.capability
                        && new.status == CapabilityStatus::Granted
                        && new.request.scope == old.request.scope
                        && new.bindings == old.bindings
                })
            })
            .map(|grant| grant.request.capability.clone())
            .collect::<Vec<_>>();
        let next_generation = self.generation.saturating_add(1);
        for capability in &revoked {
            self.runtime.revoke(capability, next_generation);
        }
        self.policy = policy;
        self.generation = next_generation;

        if self.policy.blocked {
            self.runtime.revoke_all();
            self.activations.clear();
            self.stopped = true;
            return self.channel.send_supervisor(&SupervisorMessage::Shutdown {
                generation: self.generation,
                reason: BrokerErrorCode::Denied,
            });
        }

        for state in new {
            if !old.contains(&state)
                || revoked
                    .iter()
                    .any(|capability| capability.to_string() == state.capability)
            {
                self.runtime
                    .events_mut()
                    .push_capability(self.generation, state)
                    .map_err(|_| TransportError::Malformed("capability event queue exhausted"))?;
            }
        }
        self.flush_events()
    }

    fn authorize_request(
        &self,
        phase: CallbackPhase,
        capability: &str,
        operation: &str,
    ) -> Result<
        (
            CapabilityId,
            touchbar_policy::CapabilityScope,
            GrantBindings,
        ),
        BrokerErrorCode,
    > {
        if matches!(phase, CallbackPhase::Items | CallbackPhase::Render) {
            return Err(BrokerErrorCode::InvalidPhase);
        }
        if !valid_wire_identifier(operation) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let Ok(capability) = capability.parse::<CapabilityId>() else {
            return Err(BrokerErrorCode::InvalidRequest);
        };
        let Some(grant) = self
            .policy
            .grants
            .iter()
            .find(|grant| grant.request.capability == capability)
        else {
            return Err(BrokerErrorCode::Denied);
        };
        match grant.status {
            CapabilityStatus::Granted => Ok((
                capability,
                grant.request.scope.clone(),
                grant.bindings.clone(),
            )),
            CapabilityStatus::Unsupported => Err(BrokerErrorCode::Unsupported),
            CapabilityStatus::Denied | CapabilityStatus::NeedsConsent => {
                Err(BrokerErrorCode::Denied)
            }
            CapabilityStatus::DisclosureOnly => Err(BrokerErrorCode::Unsupported),
        }
    }

    fn validate_activation_envelope(
        &self,
        phase: CallbackPhase,
        activation: Option<&ActivationContext>,
    ) -> Result<(), BrokerErrorCode> {
        let Some(activation) = activation else {
            return Ok(());
        };
        if phase != CallbackPhase::Input {
            return Err(BrokerErrorCode::InvalidPhase);
        }
        if activation.surface_instance == 0
            || activation.input_sequence == 0
            || !valid_wire_identifier(&activation.item_id)
        {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        if activation.origin == touchbar_protocol::broker_ipc::ActivationOrigin::Synthetic {
            return Err(BrokerErrorCode::ActivationRequired);
        }
        let now = monotonic_micros().map_err(|_| BrokerErrorCode::Internal)?;
        if activation.deadline_monotonic_micros < now {
            return Err(BrokerErrorCode::ActivationRequired);
        }
        let maximum_lifetime =
            u64::try_from(self.limits.maximum_activation_lifetime.as_micros()).unwrap_or(u64::MAX);
        if activation.deadline_monotonic_micros.saturating_sub(now) > maximum_lifetime {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        Ok(())
    }

    fn elapsed_millis(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn elapsed_micros(&self) -> u64 {
        self.started.elapsed().as_micros().min(u64::MAX as u128) as u64
    }

    fn pump_completions(&mut self) -> Result<(), TransportError> {
        self.runtime
            .pump_completions(self.generation)
            .map(|_| ())
            .map_err(|_| TransportError::Malformed("broker completion channel failed"))
    }

    fn pump_resource_events(&mut self) {
        self.runtime.pump_resource_events(self.generation);
    }

    pub fn flush_events(&mut self) -> Result<(), TransportError> {
        while let Some(event) = self.runtime.pop_event() {
            if let HostEvent::Completion { request_id, result } = &event
                && let Some(audit) = self.pending_audit.remove(request_id)
            {
                let (error, response_bytes) = match result {
                    BrokerResult::Success { payload } => (None, payload.len() as u64),
                    BrokerResult::Error(error) => (Some(*error), 0),
                };
                self.push_audit(audit, AuditDecision::Allowed, error, response_bytes)?;
            }
            self.channel.send_supervisor(&event.into_message())?;
        }
        Ok(())
    }

    fn push_audit(
        &mut self,
        audit: PendingAudit,
        decision: AuditDecision,
        error: Option<BrokerErrorCode>,
        response_bytes: u64,
    ) -> Result<(), TransportError> {
        let record = self.audit.push(AuditInput {
            wall_clock_unix_millis: audit.wall_clock_unix_millis,
            capability: audit.capability,
            operation: audit.operation,
            activation: audit.activation,
            decision,
            error,
            duration_micros: audit.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            request_bytes: audit.request_bytes,
            response_bytes,
            resource_count: self.runtime.lifecycle().resource_count() as u32,
        });
        if let Some(audit_file) = &self.audit_file {
            audit_file.append(record).map_err(audit_transport_error)?;
        }
        Ok(())
    }
}

fn audit_transport_error(error: AuditFileError) -> TransportError {
    match error {
        AuditFileError::Io(error) => TransportError::Io(error),
        AuditFileError::Encode(_) | AuditFileError::Invalid(_) => {
            TransportError::Malformed("durable audit write failed")
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

fn audit_decision(error: BrokerErrorCode) -> AuditDecision {
    match error {
        BrokerErrorCode::Unavailable | BrokerErrorCode::Unsupported => AuditDecision::Unavailable,
        BrokerErrorCode::Denied | BrokerErrorCode::ActivationRequired => AuditDecision::Denied,
        BrokerErrorCode::OutOfScope => AuditDecision::OutOfScope,
        BrokerErrorCode::InvalidRequest | BrokerErrorCode::InvalidPhase => AuditDecision::Invalid,
        BrokerErrorCode::QuotaExceeded
        | BrokerErrorCode::RateLimited
        | BrokerErrorCode::Timeout
        | BrokerErrorCode::Cancelled
        | BrokerErrorCode::BackendFailed
        | BrokerErrorCode::Internal => AuditDecision::PolicyBlocked,
    }
}

fn wall_clock_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn audit_identifier(value: &str) -> String {
    if valid_wire_identifier(value) {
        value.into()
    } else {
        "invalid".into()
    }
}

fn valid_wire_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment.as_bytes()[0].is_ascii_lowercase()
                && segment.as_bytes()[segment.len() - 1].is_ascii_alphanumeric()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

pub fn capability_states(policy: &EffectivePolicy) -> Vec<CapabilityState> {
    policy
        .grants
        .iter()
        .map(|grant| CapabilityState {
            capability: grant.request.capability.to_string(),
            required: grant.request.required,
            status: wire_status(grant.status),
        })
        .collect()
}

fn wire_status(status: CapabilityStatus) -> WireCapabilityStatus {
    match status {
        CapabilityStatus::Granted => WireCapabilityStatus::Granted,
        CapabilityStatus::Denied => WireCapabilityStatus::Denied,
        CapabilityStatus::NeedsConsent => WireCapabilityStatus::NeedsConsent,
        CapabilityStatus::Unsupported => WireCapabilityStatus::Unsupported,
        CapabilityStatus::DisclosureOnly => WireCapabilityStatus::DisclosureOnly,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::fs::PermissionsExt,
        },
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use semver::Version;
    use touchbar_broker_schema::{
        DbusBus as WireDbusBus, DbusPropertiesChanged, DbusSubscription, DbusSubscriptionOpened,
    };
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        CapabilityRegistry, CapabilityRequest, CapabilityScope, DbusBus, DbusSignalRule,
        DbusSubscribeScope, Decision, GrantRecord, GrantStore, HttpMethod, HttpOriginRule,
        HttpRequestScope, Provenance, ReusePolicy, RuntimeKind, SessionGrants,
        calculate_effective_policy,
    };
    use touchbar_protocol::broker_ipc::{BrokerResult, HostMessage, SupervisorMessage};

    use super::*;

    fn package() -> PackageInstance {
        PackageInstance {
            source: GithubSource::new("alice", "media").unwrap(),
            version: Version::new(1, 0, 0),
            digest: format!("sha256:{}", "a".repeat(64)),
            provenance: Provenance::VerifiedRelease,
            runtime: RuntimeKind::Component,
        }
    }

    fn request(required: bool) -> CapabilityRequest {
        CapabilityRequest {
            capability: CapabilityId::HttpRequestV1,
            required,
            reason: "metadata".into(),
            scope: CapabilityScope::HttpRequest(HttpRequestScope {
                origins: BTreeSet::from([HttpOriginRule {
                    scheme: "https".into(),
                    host: "example.com".into(),
                    port: 443,
                    path_prefixes: BTreeSet::from(["/api/".into()]),
                }]),
                methods: BTreeSet::from([HttpMethod::Get]),
                private_network: false,
                maximum_request_bytes: 1024,
                maximum_response_bytes: 1024,
                maximum_requests_per_minute: 10,
            }),
        }
    }

    fn policy(required: bool) -> EffectivePolicy {
        calculate_effective_policy(
            &package(),
            &[request(required)],
            &GrantStore::default(),
            &SessionGrants::default(),
            &CapabilityRegistry::default(),
        )
    }

    fn granted_policy(required: bool) -> EffectivePolicy {
        granted_policy_for(&package(), required)
    }

    fn granted_policy_for(package: &PackageInstance, required: bool) -> EffectivePolicy {
        let request = request(required);
        let mut grants = GrantStore::default();
        grants
            .insert(GrantRecord {
                source: package.source.clone(),
                capability: CapabilityId::HttpRequestV1,
                approved_scope: request.scope.clone(),
                bindings: GrantBindings::default(),
                decision: Decision::Allow,
                reuse: ReusePolicy::ExactDigest,
                approved_version: package.version.clone(),
                approved_digest: package.digest.clone(),
            })
            .unwrap();
        calculate_effective_policy(
            package,
            &[request],
            &grants,
            &SessionGrants::default(),
            &CapabilityRegistry::default(),
        )
    }

    fn subscription_request() -> CapabilityRequest {
        CapabilityRequest {
            capability: CapabilityId::DbusSubscribeV1,
            required: false,
            reason: "playback updates".into(),
            scope: CapabilityScope::DbusSubscribe(DbusSubscribeScope {
                rules: BTreeSet::from([DbusSignalRule {
                    bus: DbusBus::Session,
                    sender: "org.mpris.MediaPlayer2.playerctld".into(),
                    path: "/org/mpris/MediaPlayer2".into(),
                    interface: "org.freedesktop.DBus.Properties".into(),
                    member: "PropertiesChanged".into(),
                    signature: "sa{sv}as".into(),
                    argument_zero: Some("org.mpris.MediaPlayer2.Player".into()),
                }]),
                maximum_events_per_second: 30,
            }),
        }
    }

    fn subscription_policy(granted: bool) -> EffectivePolicy {
        let request = subscription_request();
        let mut grants = GrantStore::default();
        if granted {
            grants
                .insert(GrantRecord {
                    source: package().source,
                    capability: CapabilityId::DbusSubscribeV1,
                    approved_scope: request.scope.clone(),
                    bindings: GrantBindings::default(),
                    decision: Decision::Allow,
                    reuse: ReusePolicy::ExactDigest,
                    approved_version: Version::new(1, 0, 0),
                    approved_digest: package().digest,
                })
                .unwrap();
        }
        calculate_effective_policy(
            &package(),
            &[request],
            &grants,
            &SessionGrants::default(),
            &CapabilityRegistry::default(),
        )
    }

    fn connection(required: bool) -> (Seqpacket, SupervisorConnection, ConnectionIdentity) {
        let (host, supervisor) = Seqpacket::pair().unwrap();
        let identity = ConnectionIdentity {
            instance_id: 42,
            package: package(),
        };
        let connection = SupervisorConnection::new(
            supervisor,
            identity.clone(),
            policy(required),
            ConnectionLimits::default(),
        )
        .unwrap();
        (host, connection, identity)
    }

    fn granted_connection() -> (Seqpacket, SupervisorConnection) {
        let (host, supervisor) = Seqpacket::pair().unwrap();
        let connection = SupervisorConnection::new(
            supervisor,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            granted_policy(false),
            ConnectionLimits::default(),
        )
        .unwrap();
        (host, connection)
    }

    struct EchoBackend {
        calls: Arc<AtomicUsize>,
    }

    struct BoundIdentityBackend;

    impl Backend for BoundIdentityBackend {
        fn execute(
            &self,
            request: &BackendRequest,
            _cancellation: &CancellationToken,
        ) -> BrokerResult {
            BrokerResult::Success {
                payload: format!(
                    "{}:{}",
                    request.identity.package.source, request.identity.instance_id
                )
                .into_bytes(),
            }
        }
    }

    impl Backend for EchoBackend {
        fn execute(
            &self,
            backend_request: &BackendRequest,
            _cancellation: &CancellationToken,
        ) -> BrokerResult {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(backend_request.identity.instance_id, 42);
            assert_eq!(backend_request.authorized_scope, request(false).scope);
            BrokerResult::Success {
                payload: backend_request.payload.clone(),
            }
        }
    }

    struct CancellationBackend;

    impl Backend for CancellationBackend {
        fn execute(
            &self,
            _request: &BackendRequest,
            cancellation: &CancellationToken,
        ) -> BrokerResult {
            while !cancellation.is_cancelled() {
                thread::yield_now();
            }
            BrokerResult::Success {
                payload: Vec::new(),
            }
        }
    }

    struct TestResourceHandle {
        closes: Arc<AtomicUsize>,
    }

    impl ResourceHandle for TestResourceHandle {
        fn close(&mut self) {
            self.closes.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct TestSubscriptionTransport {
        opens: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
    }

    impl DbusSubscriptionTransport for TestSubscriptionTransport {
        fn open(
            &self,
            _subscription: &DbusSubscription,
            events: ResourceEventSink,
        ) -> Result<Box<dyn ResourceHandle>, BrokerErrorCode> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            events
                .emit(
                    DbusPropertiesChanged {
                        interface_name: "org.mpris.MediaPlayer2.Player".into(),
                        changed_properties: Vec::new(),
                        invalidated_properties: vec!["PlaybackStatus".into()],
                    }
                    .encode()
                    .unwrap(),
                )
                .unwrap();
            Ok(Box::new(TestResourceHandle {
                closes: Arc::clone(&self.closes),
            }))
        }
    }

    fn subscription_payload() -> Vec<u8> {
        DbusSubscription {
            bus: WireDbusBus::Session,
            sender: "org.mpris.MediaPlayer2.playerctld".into(),
            path: "/org/mpris/MediaPlayer2".into(),
            interface: "org.freedesktop.DBus.Properties".into(),
            member: "PropertiesChanged".into(),
            signature: "sa{sv}as".into(),
            argument_zero: Some("org.mpris.MediaPlayer2.Player".into()),
        }
        .encode()
        .unwrap()
    }

    #[test]
    fn live_subscription_delivers_ordered_event_and_close_releases_resource() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let opens = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            subscription_policy(true),
            ConnectionLimits::default(),
        )
        .unwrap();
        supervisor.register_resource_backend(
            CapabilityId::DbusSubscribeV1,
            Arc::new(DbusSubscriptionBackend::new(TestSubscriptionTransport {
                opens: Arc::clone(&opens),
                closes: Arc::clone(&closes),
            })),
        );

        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::HostEvent,
            capability: "dbus.subscribe.v1".into(),
            operation: "subscribe".into(),
            payload: subscription_payload(),
            activation: None,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        let SupervisorMessage::Response {
            request_id: 1,
            result: BrokerResult::Success { payload },
        } = host.recv_supervisor().unwrap()
        else {
            panic!("subscription did not open")
        };
        let resource_id = DbusSubscriptionOpened::decode(&payload)
            .unwrap()
            .resource_id;
        assert!(matches!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::ResourceEvent {
                resource_id: event_resource,
                sequence: 1,
                result: BrokerResult::Success { .. },
            } if event_resource == resource_id
        ));
        assert_eq!(supervisor.lifecycle().resource_count(), 1);

        host.send_host(&HostMessage::Close {
            request_id: 2,
            resource_id,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 2,
                result: BrokerResult::Success {
                    payload: Vec::new()
                },
            }
        );
        assert_eq!(supervisor.lifecycle().resource_count(), 0);
        assert_eq!(opens.load(Ordering::Relaxed), 1);
        assert_eq!(closes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn subscription_scope_narrowing_closes_live_transport_and_notifies_guest() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let closes = Arc::new(AtomicUsize::new(0));
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            subscription_policy(true),
            ConnectionLimits::default(),
        )
        .unwrap();
        supervisor.register_resource_backend(
            CapabilityId::DbusSubscribeV1,
            Arc::new(DbusSubscriptionBackend::new(TestSubscriptionTransport {
                opens: Arc::new(AtomicUsize::new(0)),
                closes: Arc::clone(&closes),
            })),
        );
        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::HostEvent,
            capability: "dbus.subscribe.v1".into(),
            operation: "subscribe".into(),
            payload: subscription_payload(),
            activation: None,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        let _ = host.recv_supervisor().unwrap();
        let _ = host.recv_supervisor().unwrap();

        let mut narrowed = subscription_policy(true);
        let CapabilityScope::DbusSubscribe(scope) = &mut narrowed.grants[0].request.scope else {
            panic!("expected subscription scope")
        };
        scope.maximum_events_per_second = 10;
        supervisor.replace_policy(narrowed).unwrap();
        assert_eq!(supervisor.lifecycle().resource_count(), 0);
        assert_eq!(closes.load(Ordering::Relaxed), 1);
        assert!(matches!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::CapabilityChanged {
                generation: 2,
                state: CapabilityState {
                    status: WireCapabilityStatus::Granted,
                    ..
                }
            }
        ));
        assert!(matches!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::ResourceEvent {
                sequence: 2,
                result: BrokerResult::Error(BrokerErrorCode::Denied),
                ..
            }
        ));
    }

    #[test]
    fn live_loop_routes_completion_and_audits_without_polling() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            granted_policy(false),
            ConnectionLimits::default(),
        )
        .unwrap();
        supervisor.register_backend(
            CapabilityId::HttpRequestV1,
            Arc::new(EchoBackend {
                calls: Arc::clone(&calls),
            }),
        );
        let server = thread::spawn(move || {
            supervisor.serve_until_disconnect().unwrap();
            supervisor.audit.records().cloned().collect::<Vec<_>>()
        });

        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "http.request.v1".into(),
            operation: "get".into(),
            payload: b"hello".to_vec(),
            activation: None,
        })
        .unwrap();
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Success {
                    payload: b"hello".to_vec(),
                },
            }
        );
        drop(host);

        let audit = server.join().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].decision, AuditDecision::Allowed);
        assert_eq!(audit[0].result, AuditResult::Success);
        assert_eq!(audit[0].request_bytes, 5);
        assert_eq!(audit[0].response_bytes, 5);
    }

    #[test]
    fn live_loop_cancels_an_inflight_request_and_answers_both_ids() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            granted_policy(false),
            ConnectionLimits {
                backend_timeout: Duration::from_secs(1),
                ..ConnectionLimits::default()
            },
        )
        .unwrap();
        supervisor.register_backend(CapabilityId::HttpRequestV1, Arc::new(CancellationBackend));
        let server = thread::spawn(move || supervisor.serve_until_disconnect());

        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "http.request.v1".into(),
            operation: "get".into(),
            payload: Vec::new(),
            activation: None,
        })
        .unwrap();
        host.send_host(&HostMessage::Cancel {
            request_id: 2,
            target_request_id: 1,
        })
        .unwrap();

        let first = host.recv_supervisor().unwrap();
        let second = host.recv_supervisor().unwrap();
        let responses = BTreeMap::from([
            (response_id(&first), response_result(first)),
            (response_id(&second), response_result(second)),
        ]);
        assert_eq!(
            responses.get(&1),
            Some(&BrokerResult::Error(BrokerErrorCode::Cancelled))
        );
        assert_eq!(
            responses.get(&2),
            Some(&BrokerResult::Success {
                payload: Vec::new()
            })
        );
        drop(host);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn live_loop_denies_before_a_registered_backend_can_execute() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            policy(false),
            ConnectionLimits::default(),
        )
        .unwrap();
        supervisor.register_backend(
            CapabilityId::HttpRequestV1,
            Arc::new(EchoBackend {
                calls: Arc::clone(&calls),
            }),
        );
        let server = thread::spawn(move || supervisor.serve_until_disconnect());

        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "http.request.v1".into(),
            operation: "get".into(),
            payload: Vec::new(),
            activation: None,
        })
        .unwrap();
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Error(BrokerErrorCode::Denied),
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        drop(host);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn policy_signal_wakes_idle_connection_and_updates_optional_capability() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            policy(false),
            ConnectionLimits::default(),
        )
        .unwrap();
        // SAFETY: eventfd has no pointer arguments and returns an owned fd.
        let signal = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(signal >= 0);
        // SAFETY: successful eventfd returned a uniquely owned descriptor.
        let signal = unsafe { OwnedFd::from_raw_fd(signal) };
        let notifier = signal.try_clone().unwrap();
        let server = thread::spawn(move || {
            let signal_fd = signal.as_raw_fd();
            let mut reload = || {
                let mut count = 0_u64;
                // SAFETY: count is writable for eight bytes and signal is a
                // readable eventfd kept alive by this closure.
                let read = unsafe {
                    libc::read(
                        signal_fd,
                        (&mut count as *mut u64).cast(),
                        std::mem::size_of::<u64>(),
                    )
                };
                if read != std::mem::size_of::<u64>() as isize {
                    return Err(TransportError::Malformed("test policy signal failed"));
                }
                Ok(Some(granted_policy(false)))
            };
            supervisor
                .serve_with_policy_updates(signal_fd, &mut reload)
                .unwrap()
        });

        let one = 1_u64;
        // SAFETY: one is readable for eight bytes and notifier is a writable
        // duplicate of the live eventfd.
        assert_eq!(
            unsafe {
                libc::write(
                    notifier.as_raw_fd(),
                    (&one as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            },
            std::mem::size_of::<u64>() as isize
        );
        assert!(matches!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::CapabilityChanged {
                generation: 2,
                state: CapabilityState {
                    status: WireCapabilityStatus::Granted,
                    ..
                }
            }
        ));
        drop(host);
        assert_eq!(server.join().unwrap(), ConnectionExit::HostDisconnected);
    }

    #[test]
    fn live_policy_revocation_cancels_only_the_matching_backend_work() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            granted_policy(false),
            ConnectionLimits::default(),
        )
        .unwrap();
        supervisor.register_backend(CapabilityId::HttpRequestV1, Arc::new(CancellationBackend));
        supervisor
            .handle_request(HostMessage::Request {
                request_id: 1,
                phase: CallbackPhase::Input,
                capability: "http.request.v1".into(),
                operation: "get".into(),
                payload: Vec::new(),
                activation: None,
            })
            .unwrap();
        assert_eq!(supervisor.lifecycle().pending_count(), 1);

        supervisor.replace_policy(policy(false)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while supervisor.runtime.active_count() != 0 {
            supervisor.pump_completions().unwrap();
            supervisor.flush_events().unwrap();
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        supervisor.flush_events().unwrap();

        assert!(matches!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::CapabilityChanged {
                generation: 2,
                state: CapabilityState {
                    status: WireCapabilityStatus::NeedsConsent,
                    ..
                }
            }
        ));
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Error(BrokerErrorCode::Cancelled),
            }
        );
        let audit = supervisor.audit.records().last().unwrap();
        assert_eq!(audit.decision, AuditDecision::Allowed);
        assert_eq!(audit.result, AuditResult::Cancelled);
    }

    #[test]
    fn connection_rejects_an_event_queue_smaller_than_maximum_inflight_work() {
        let (_host, supervisor_channel) = Seqpacket::pair().unwrap();
        let result = SupervisorConnection::new(
            supervisor_channel,
            ConnectionIdentity {
                instance_id: 42,
                package: package(),
            },
            policy(false),
            ConnectionLimits {
                maximum_edge_events: 1,
                executor: ExecutorLimits {
                    maximum_active_operations: 2,
                    ..ExecutorLimits::default()
                },
                ..ConnectionLimits::default()
            },
        );
        assert!(matches!(result, Err(TransportError::Malformed(_))));
    }

    fn response_id(message: &SupervisorMessage) -> u64 {
        match message {
            SupervisorMessage::Response { request_id, .. } => *request_id,
            _ => panic!("expected broker response"),
        }
    }

    fn response_result(message: SupervisorMessage) -> BrokerResult {
        match message {
            SupervisorMessage::Response { result, .. } => result,
            _ => panic!("expected broker response"),
        }
    }

    #[test]
    fn connection_identity_never_comes_from_host_messages() {
        let (host, mut supervisor, identity) = connection(false);
        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "secret.read.v1".into(),
            operation: "read".into(),
            payload: b"github:mallory/forged".to_vec(),
            activation: None,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        assert_eq!(supervisor.identity(), &identity);
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Error(BrokerErrorCode::Denied),
            }
        );
    }

    #[test]
    fn pure_callbacks_are_rejected_before_capability_lookup() {
        let (host, mut supervisor, _) = connection(false);
        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Render,
            capability: "http.request.v1".into(),
            operation: "get".into(),
            payload: Vec::new(),
            activation: None,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Error(BrokerErrorCode::InvalidPhase),
            }
        );
    }

    #[test]
    fn activation_envelopes_are_phase_bounded_short_lived_and_non_synthetic() {
        let (host, mut supervisor) = granted_connection();
        let now = monotonic_micros().unwrap();
        let activation = |origin, deadline_monotonic_micros| ActivationContext {
            origin,
            surface_instance: 7,
            item_id: "media".into(),
            widget_id: 11,
            input_sequence: 3,
            deadline_monotonic_micros,
        };
        let cases = [
            (
                CallbackPhase::HostEvent,
                activation(
                    touchbar_protocol::broker_ipc::ActivationOrigin::Physical,
                    now + 1_000_000,
                ),
                BrokerErrorCode::InvalidPhase,
            ),
            (
                CallbackPhase::Input,
                activation(
                    touchbar_protocol::broker_ipc::ActivationOrigin::Synthetic,
                    now + 1_000_000,
                ),
                BrokerErrorCode::ActivationRequired,
            ),
            (
                CallbackPhase::Input,
                activation(
                    touchbar_protocol::broker_ipc::ActivationOrigin::Physical,
                    now.saturating_sub(1),
                ),
                BrokerErrorCode::ActivationRequired,
            ),
            (
                CallbackPhase::Input,
                activation(
                    touchbar_protocol::broker_ipc::ActivationOrigin::Physical,
                    now + 3_000_000,
                ),
                BrokerErrorCode::InvalidRequest,
            ),
        ];
        for (index, (phase, activation, expected)) in cases.into_iter().enumerate() {
            let request_id = index as u64 + 1;
            supervisor
                .handle_request(HostMessage::Request {
                    request_id,
                    phase,
                    capability: "http.request.v1".into(),
                    operation: "get".into(),
                    payload: Vec::new(),
                    activation: Some(activation),
                })
                .unwrap();
            assert_eq!(
                host.recv_supervisor().unwrap(),
                SupervisorMessage::Response {
                    request_id,
                    result: BrokerResult::Error(expected),
                }
            );
        }
    }

    struct ActivationBackend {
        expected: ActivationContext,
    }

    impl Backend for ActivationBackend {
        fn execute(
            &self,
            request: &BackendRequest,
            _cancellation: &CancellationToken,
        ) -> BrokerResult {
            assert_eq!(request.activation.as_ref(), Some(&self.expected));
            BrokerResult::Success {
                payload: Vec::new(),
            }
        }
    }

    #[test]
    fn validated_physical_activation_reaches_the_backend_and_audit() {
        let (host, mut supervisor) = granted_connection();
        let activation = ActivationContext {
            origin: touchbar_protocol::broker_ipc::ActivationOrigin::Physical,
            surface_instance: 7,
            item_id: "media".into(),
            widget_id: 11,
            input_sequence: 3,
            deadline_monotonic_micros: monotonic_micros().unwrap() + 1_000_000,
        };
        supervisor.register_backend(
            CapabilityId::HttpRequestV1,
            Arc::new(ActivationBackend {
                expected: activation.clone(),
            }),
        );
        let server = thread::spawn(move || {
            supervisor.serve_until_disconnect().unwrap();
            supervisor.audit.records().cloned().collect::<Vec<_>>()
        });
        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "http.request.v1".into(),
            operation: "get".into(),
            payload: Vec::new(),
            activation: Some(activation),
        })
        .unwrap();
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Success {
                    payload: Vec::new(),
                },
            }
        );
        drop(host);
        let audit = server.join().unwrap();
        assert_eq!(audit[0].activation, AuditActivation::Physical);
    }

    #[test]
    fn no_backend_means_no_os_authority_even_if_a_grant_later_exists() {
        let (host, mut supervisor, _) = connection(false);
        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "http.request.v1".into(),
            operation: "get".into(),
            payload: Vec::new(),
            activation: None,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Error(BrokerErrorCode::Denied),
            }
        );
    }

    #[test]
    fn required_revocation_requests_shutdown_and_closes_resources() {
        let (host, mut supervisor, _) = connection(false);
        supervisor.replace_policy(policy(true)).unwrap();
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Shutdown {
                generation: 2,
                reason: BrokerErrorCode::Denied,
            }
        );
        assert!(supervisor.serve_one().is_err());
    }

    #[test]
    fn connections_do_not_share_identity_or_request_sequences() {
        let (first_host, mut first, _) = connection(false);
        let (second_host, mut second, _) = connection(false);
        second.identity.instance_id = 99;
        first_host
            .send_host(&HostMessage::GetCapabilities { request_id: 1 })
            .unwrap();
        second_host
            .send_host(&HostMessage::GetCapabilities { request_id: 1 })
            .unwrap();
        first.serve_one().unwrap();
        second.serve_one().unwrap();
        assert_eq!(first.identity.instance_id, 42);
        assert_eq!(second.identity.instance_id, 99);
        assert!(matches!(
            first_host.recv_supervisor().unwrap(),
            SupervisorMessage::Capabilities { request_id: 1, .. }
        ));
        assert!(matches!(
            second_host.recv_supervisor().unwrap(),
            SupervisorMessage::Capabilities { request_id: 1, .. }
        ));
    }

    #[test]
    fn hostile_wire_requests_cannot_cross_plugin_identity_boundaries() {
        let alice = package();
        let bob = PackageInstance {
            source: GithubSource::new("mallory", "lookalike").unwrap(),
            version: Version::new(9, 9, 9),
            digest: format!("sha256:{}", "b".repeat(64)),
            provenance: Provenance::VerifiedRelease,
            runtime: RuntimeKind::Component,
        };
        let (alice_host, alice_channel) = Seqpacket::pair().unwrap();
        let (bob_host, bob_channel) = Seqpacket::pair().unwrap();
        let mut alice_supervisor = SupervisorConnection::new(
            alice_channel,
            ConnectionIdentity {
                instance_id: 10,
                package: alice.clone(),
            },
            granted_policy_for(&alice, false),
            ConnectionLimits::default(),
        )
        .unwrap();
        let mut bob_supervisor = SupervisorConnection::new(
            bob_channel,
            ConnectionIdentity {
                instance_id: 20,
                package: bob.clone(),
            },
            granted_policy_for(&bob, false),
            ConnectionLimits::default(),
        )
        .unwrap();
        alice_supervisor
            .register_backend(CapabilityId::HttpRequestV1, Arc::new(BoundIdentityBackend));
        bob_supervisor
            .register_backend(CapabilityId::HttpRequestV1, Arc::new(BoundIdentityBackend));
        let alice_thread = thread::spawn(move || alice_supervisor.serve_until_disconnect());
        let bob_thread = thread::spawn(move || bob_supervisor.serve_until_disconnect());

        for host in [&alice_host, &bob_host] {
            host.send_host(&HostMessage::Request {
                request_id: 1,
                phase: CallbackPhase::Input,
                capability: "http.request.v1".into(),
                operation: "get".into(),
                payload: b"attempted-forged-identity".to_vec(),
                activation: None,
            })
            .unwrap();
        }
        assert_eq!(
            alice_host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Success {
                    payload: b"github:alice/media:10".to_vec(),
                },
            }
        );
        assert_eq!(
            bob_host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Success {
                    payload: b"github:mallory/lookalike:20".to_vec(),
                },
            }
        );
        drop((alice_host, bob_host));
        alice_thread.join().unwrap().unwrap();
        bob_thread.join().unwrap().unwrap();
    }

    #[test]
    fn request_ids_are_monotonic_and_cannot_be_replayed() {
        let (host, mut supervisor, _) = connection(false);
        for _ in 0..2 {
            host.send_host(&HostMessage::GetCapabilities { request_id: 1 })
                .unwrap();
            supervisor.serve_one().unwrap();
        }
        assert!(matches!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Capabilities { .. }
        ));
        assert_eq!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::Response {
                request_id: 1,
                result: BrokerResult::Error(BrokerErrorCode::InvalidRequest),
            }
        );
    }

    #[test]
    fn monotonically_increasing_request_ids_do_not_expire() {
        let (host, mut supervisor, _) = connection(false);

        for request_id in 1..=5_000 {
            host.send_host(&HostMessage::GetCapabilities { request_id })
                .unwrap();
            supervisor.serve_one().unwrap();
            assert!(matches!(
                host.recv_supervisor().unwrap(),
                SupervisorMessage::Capabilities {
                    request_id: response_id,
                    ..
                } if response_id == request_id
            ));
        }
    }

    #[test]
    fn optional_revocation_cancels_only_capability_owned_lifecycle_state() {
        let (host, supervisor_channel) = Seqpacket::pair().unwrap();
        let identity = ConnectionIdentity {
            instance_id: 42,
            package: package(),
        };
        let mut supervisor = SupervisorConnection::new(
            supervisor_channel,
            identity,
            granted_policy(false),
            ConnectionLimits::default(),
        )
        .unwrap();
        supervisor
            .lifecycle_mut()
            .begin(9, CapabilityId::HttpRequestV1, 100)
            .unwrap();
        supervisor
            .lifecycle_mut()
            .begin(10, CapabilityId::DbusCallV1, 100)
            .unwrap();
        supervisor
            .lifecycle_mut()
            .allocate_resource(CapabilityId::HttpRequestV1, 100)
            .unwrap();
        supervisor
            .lifecycle_mut()
            .allocate_resource(CapabilityId::DbusCallV1, 200)
            .unwrap();

        supervisor.replace_policy(policy(false)).unwrap();
        assert_eq!(supervisor.lifecycle().pending_count(), 1);
        assert_eq!(supervisor.lifecycle().resource_count(), 1);
        assert_eq!(supervisor.lifecycle().buffered_bytes(), 200);
        assert!(matches!(
            host.recv_supervisor().unwrap(),
            SupervisorMessage::CapabilityChanged {
                generation: 2,
                state: CapabilityState {
                    status: WireCapabilityStatus::NeedsConsent,
                    ..
                }
            }
        ));
    }

    #[test]
    fn untrusted_identifiers_and_payloads_cannot_enter_audit_output() {
        let (host, mut supervisor, _) = connection(false);
        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "secret value from payload".into(),
            operation: "authorization=super-secret-value".into(),
            payload: b"super-secret-value".to_vec(),
            activation: None,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        let _ = host.recv_supervisor().unwrap();
        let audit = supervisor.audit().to_json().unwrap();
        assert!(!audit.contains("super-secret-value"));
        assert!(!audit.contains("secret value from payload"));
        assert!(audit.contains("\"capability\": \"invalid\""));
    }

    #[test]
    fn connection_writes_the_same_redacted_record_to_durable_audit() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("audit.jsonl");
        let (host, mut supervisor, _) = connection(false);
        supervisor.set_audit_file(AuditFile::new(&path, AuditFileLimits::default()).unwrap());
        host.send_host(&HostMessage::Request {
            request_id: 1,
            phase: CallbackPhase::Input,
            capability: "secret value from payload".into(),
            operation: "authorization=super-secret-value".into(),
            payload: b"super-secret-value".to_vec(),
            activation: None,
        })
        .unwrap();
        supervisor.serve_one().unwrap();
        let _ = host.recv_supervisor().unwrap();

        let encoded = fs::read_to_string(path).unwrap();
        let record: AuditRecord = serde_json::from_str(encoded.trim()).unwrap();
        assert_eq!(record.capability, "invalid");
        assert_eq!(record.operation, "invalid");
        assert!(!encoded.contains("super-secret-value"));
        assert!(!encoded.contains("payload"));
    }

    #[test]
    fn repeated_protocol_replays_feed_the_circuit_breaker() {
        let (host, mut supervisor, _) = connection(false);
        host.send_host(&HostMessage::GetCapabilities { request_id: 1 })
            .unwrap();
        supervisor.serve_one().unwrap();
        let _ = host.recv_supervisor().unwrap();
        for _ in 0..3 {
            host.send_host(&HostMessage::GetCapabilities { request_id: 1 })
                .unwrap();
            supervisor.serve_one().unwrap();
            let _ = host.recv_supervisor().unwrap();
        }
        assert!(matches!(
            supervisor.health().state(),
            HealthState::RestartAfter { .. }
        ));
    }
}
