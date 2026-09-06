use std::{
    collections::BTreeMap,
    ffi::CString,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Result as AnyResult, bail};
use touchbar_broker_schema::{
    LocalConnect, LocalConnectionOpened, LocalFrameEvent, LocalSendFrame, MAX_LOCAL_FRAME_BYTES,
    SchemaError,
};
use touchbar_policy::{CapabilityId, CapabilityScope, LocalEndpointBinding};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

use crate::{
    ActivationLedger, Backend, BackendRequest, CancellationToken, OpenedResource, ResourceBackend,
    ResourceEventSink, ResourceHandle, ResourceLimits, filesystem_write::PersistentQuota,
};

pub const LOCAL_CONNECT_OPERATION: &str = "connect";
pub const LOCAL_SEND_OPERATION: &str = "send";

pub const LOCAL_MAXIMUM_EVENTS_PER_SECOND: u16 = 60;
const LOCAL_RESERVED_FRAMES: usize = 8;
const SOCKET_IO_TIMEOUT: Duration = Duration::from_millis(250);
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

struct LocalConnection {
    instance_id: u64,
    endpoint: String,
    protocol: String,
    binding: LocalEndpointBinding,
    maximum_frame_bytes: usize,
    maximum_bytes_per_minute: u64,
    stream: Mutex<UnixStream>,
    closed: AtomicBool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalConnectionAuthorization {
    instance_id: u64,
    endpoint: String,
    protocol: String,
    binding: LocalEndpointBinding,
    maximum_frame_bytes: usize,
    maximum_bytes_per_minute: u64,
}

impl LocalConnectionAuthorization {
    pub fn maximum_frame_bytes(&self) -> u32 {
        u32::try_from(self.maximum_frame_bytes)
            .expect("authorized local frame limit is bounded by the wire maximum")
    }
}

impl LocalConnection {
    fn authorization(&self) -> LocalConnectionAuthorization {
        LocalConnectionAuthorization {
            instance_id: self.instance_id,
            endpoint: self.endpoint.clone(),
            protocol: self.protocol.clone(),
            binding: self.binding.clone(),
            maximum_frame_bytes: self.maximum_frame_bytes,
            maximum_bytes_per_minute: self.maximum_bytes_per_minute,
        }
    }
}

pub struct LocalIpcBackend {
    quota: Arc<PersistentQuota>,
    connections: Arc<Mutex<BTreeMap<u64, Arc<LocalConnection>>>>,
}

impl LocalIpcBackend {
    pub fn new(state_directory: &Path, quota_namespace: &str) -> AnyResult<Self> {
        if quota_namespace.is_empty() {
            bail!("local IPC quota namespace is empty");
        }
        Ok(Self {
            quota: Arc::new(PersistentQuota::new(
                state_directory,
                &format!("local-connect:{quota_namespace}"),
                60,
            )?),
            connections: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
}

impl ResourceBackend for LocalIpcBackend {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        let CapabilityScope::LocalConnect(scope) = &request.authorized_scope else {
            return None;
        };
        if request.capability != CapabilityId::LocalConnectV1
            || request.operation != LOCAL_CONNECT_OPERATION
        {
            return None;
        }
        let maximum_frame_bytes = usize::try_from(scope.maximum_frame_bytes)
            .ok()?
            .min(MAX_LOCAL_FRAME_BYTES);
        maximum_frame_bytes
            .checked_mul(LOCAL_RESERVED_FRAMES)
            .map(|reserved_buffered_bytes| ResourceLimits {
                reserved_buffered_bytes,
                maximum_events_per_second: LOCAL_MAXIMUM_EVENTS_PER_SECOND,
            })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        authorize_local_connect_request(request).map(|_| ())
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        let (connect, authorization) = authorize_local_connect_request(request)?;
        let maximum_frame_bytes = authorization.maximum_frame_bytes;
        let stream = connect_bound_socket(&authorization.binding)?;
        stream
            .set_read_timeout(Some(SOCKET_IO_TIMEOUT))
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        stream
            .set_write_timeout(Some(SOCKET_IO_TIMEOUT))
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        let reader = stream
            .try_clone()
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        let connection = Arc::new(LocalConnection {
            instance_id: request.identity.instance_id,
            endpoint: connect.endpoint,
            protocol: authorization.protocol,
            binding: authorization.binding,
            maximum_frame_bytes,
            maximum_bytes_per_minute: authorization.maximum_bytes_per_minute,
            stream: Mutex::new(stream),
            closed: AtomicBool::new(false),
        });
        let response_payload = LocalConnectionOpened {
            resource_id,
            maximum_frame_bytes: u32::try_from(maximum_frame_bytes)
                .map_err(|_| BrokerErrorCode::Internal)?,
        }
        .encode()
        .map_err(schema_error)?;
        {
            let mut connections = self
                .connections
                .lock()
                .map_err(|_| BrokerErrorCode::Internal)?;
            if connections.contains_key(&resource_id) {
                return Err(BrokerErrorCode::InvalidRequest);
            }
            connections.insert(resource_id, Arc::clone(&connection));
        }
        let quota = Arc::clone(&self.quota);
        let reader_connection = Arc::clone(&connection);
        let reader_thread = match thread::Builder::new()
            .name(format!("touchbar-local-{resource_id}"))
            .spawn(move || read_frames(reader, reader_connection, quota, events))
        {
            Ok(thread) => thread,
            Err(_) => {
                self.connections
                    .lock()
                    .map_err(|_| BrokerErrorCode::Internal)?
                    .remove(&resource_id);
                return Err(BrokerErrorCode::Unavailable);
            }
        };
        Ok(OpenedResource {
            handle: Box::new(LocalConnectionHandle {
                resource_id,
                connection,
                connections: Arc::clone(&self.connections),
                reader_thread: Some(reader_thread),
            }),
            response_payload,
        })
    }
}

impl Backend for LocalIpcBackend {
    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        self.match_send(request).map(|_| ())
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let (frame, connection) = self.match_send(request)?;
            reserve_traffic(
                &self.quota,
                frame.bytes.len(),
                connection.maximum_bytes_per_minute,
            )?;
            let length = u32::try_from(frame.bytes.len())
                .map_err(|_| BrokerErrorCode::QuotaExceeded)?
                .to_be_bytes();
            let mut stream = connection
                .stream
                .lock()
                .map_err(|_| BrokerErrorCode::Internal)?;
            if connection.closed.load(Ordering::Acquire) {
                return Err(BrokerErrorCode::Unavailable);
            }
            write_all_interruptible(&mut stream, &length, cancellation)?;
            write_all_interruptible(&mut stream, &frame.bytes, cancellation)?;
            Ok(BrokerResult::Success {
                payload: Vec::new(),
            })
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

impl LocalIpcBackend {
    fn match_send(
        &self,
        request: &BackendRequest,
    ) -> Result<(LocalSendFrame, Arc<LocalConnection>), BrokerErrorCode> {
        if request.capability != CapabilityId::LocalConnectV1
            || request.operation != LOCAL_SEND_OPERATION
        {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let frame = LocalSendFrame::decode(&request.payload).map_err(schema_error)?;
        let connection = self
            .connections
            .lock()
            .map_err(|_| BrokerErrorCode::Internal)?
            .get(&frame.resource_id)
            .cloned()
            .ok_or(BrokerErrorCode::Unavailable)?;
        if connection.closed.load(Ordering::Acquire) {
            return Err(BrokerErrorCode::Unavailable);
        }
        authorize_local_send_frame(request, &frame, &connection.authorization())?;
        Ok((frame, connection))
    }
}

pub fn authorize_local_send_request(
    request: &BackendRequest,
    authorization: &LocalConnectionAuthorization,
) -> Result<LocalSendFrame, BrokerErrorCode> {
    if request.capability != CapabilityId::LocalConnectV1
        || request.operation != LOCAL_SEND_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let frame = LocalSendFrame::decode(&request.payload).map_err(schema_error)?;
    authorize_local_send_frame(request, &frame, authorization)?;
    Ok(frame)
}

fn authorize_local_send_frame(
    request: &BackendRequest,
    frame: &LocalSendFrame,
    authorization: &LocalConnectionAuthorization,
) -> Result<(), BrokerErrorCode> {
    let CapabilityScope::LocalConnect(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let endpoint = scope
        .endpoints
        .iter()
        .find(|candidate| {
            candidate.label == authorization.endpoint
                && candidate.protocol == authorization.protocol
        })
        .ok_or(BrokerErrorCode::OutOfScope)?;
    let binding = request
        .bindings
        .local_endpoints
        .get(&endpoint.label)
        .ok_or(BrokerErrorCode::OutOfScope)?;
    if request.identity.instance_id != authorization.instance_id
        || binding != &authorization.binding
        || scope.maximum_bytes_per_minute != authorization.maximum_bytes_per_minute
        || usize::try_from(scope.maximum_frame_bytes)
            .map_err(|_| BrokerErrorCode::QuotaExceeded)?
            .min(MAX_LOCAL_FRAME_BYTES)
            != authorization.maximum_frame_bytes
        || frame.bytes.len() > authorization.maximum_frame_bytes
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(())
}

pub fn authorize_local_connect_request(
    request: &BackendRequest,
) -> Result<(LocalConnect, LocalConnectionAuthorization), BrokerErrorCode> {
    if request.capability != CapabilityId::LocalConnectV1
        || request.operation != LOCAL_CONNECT_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let connect = LocalConnect::decode(&request.payload).map_err(schema_error)?;
    let CapabilityScope::LocalConnect(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let endpoint = scope
        .endpoints
        .iter()
        .find(|endpoint| {
            endpoint.label == connect.endpoint && endpoint.protocol == connect.protocol
        })
        .ok_or(BrokerErrorCode::OutOfScope)?;
    let binding = request
        .bindings
        .local_endpoints
        .get(&endpoint.label)
        .ok_or(BrokerErrorCode::OutOfScope)?;
    let maximum_frame_bytes = usize::try_from(scope.maximum_frame_bytes)
        .map_err(|_| BrokerErrorCode::QuotaExceeded)?
        .min(MAX_LOCAL_FRAME_BYTES);
    Ok((
        connect.clone(),
        LocalConnectionAuthorization {
            instance_id: request.identity.instance_id,
            endpoint: connect.endpoint,
            protocol: endpoint.protocol.clone(),
            binding: binding.clone(),
            maximum_frame_bytes,
            maximum_bytes_per_minute: scope.maximum_bytes_per_minute,
        },
    ))
}

fn connect_bound_socket(binding: &LocalEndpointBinding) -> Result<UnixStream, BrokerErrorCode> {
    let LocalEndpointBinding::UnixStream { path } = binding;
    let descriptor = open_pinned_socket(path)?;
    connect_pinned_socket(&descriptor)
}

pub(crate) fn connect_same_user_socket(path: &Path) -> Result<UnixStream, BrokerErrorCode> {
    connect_bound_socket(&LocalEndpointBinding::UnixStream {
        path: path.to_owned(),
    })
}

fn connect_pinned_socket(descriptor: &OwnedFd) -> Result<UnixStream, BrokerErrorCode> {
    let before = socket_metadata(descriptor.as_raw_fd())?;
    validate_socket_metadata(&before)?;
    let pinned_path = format!("/proc/self/fd/{}", descriptor.as_raw_fd());
    let stream = UnixStream::connect(&pinned_path).map_err(|_| BrokerErrorCode::Unavailable)?;
    let after = socket_metadata(descriptor.as_raw_fd())?;
    if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
        return Err(BrokerErrorCode::OutOfScope);
    }
    validate_connected_peer(&stream)?;
    Ok(stream)
}

fn open_pinned_socket(path: &Path) -> Result<OwnedFd, BrokerErrorCode> {
    let path =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| BrokerErrorCode::OutOfScope)?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: path and how are readable for the supplied sizes and the returned
    // descriptor is uniquely owned on success.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor = i32::try_from(descriptor).map_err(|_| BrokerErrorCode::Unavailable)?;
    if descriptor < 0 {
        return Err(BrokerErrorCode::OutOfScope);
    }
    // SAFETY: successful openat2 returned a uniquely owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn socket_metadata(descriptor: i32) -> Result<libc::stat, BrokerErrorCode> {
    // SAFETY: zero is a valid initialization for stat and fstat fills it.
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: descriptor is live and metadata is writable.
    if unsafe { libc::fstat(descriptor, &mut metadata) } != 0 {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(metadata)
}

fn validate_socket_metadata(metadata: &libc::stat) -> Result<(), BrokerErrorCode> {
    // SAFETY: geteuid has no preconditions.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFSOCK
        || metadata.st_uid != effective_uid
        || metadata.st_nlink != 1
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(())
}

fn validate_connected_peer(stream: &UnixStream) -> Result<(), BrokerErrorCode> {
    // SAFETY: zero is a valid initialization for ucred.
    let mut credentials = unsafe { std::mem::zeroed::<libc::ucred>() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials and length are writable and stream is a live socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of::<libc::ucred>()
    {
        return Err(BrokerErrorCode::Unavailable);
    }
    // SAFETY: geteuid has no preconditions.
    if credentials.uid != unsafe { libc::geteuid() } || credentials.pid <= 0 {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(())
}

fn read_frames(
    mut stream: UnixStream,
    connection: Arc<LocalConnection>,
    quota: Arc<PersistentQuota>,
    events: ResourceEventSink,
) {
    let result = (|| loop {
        let mut length = [0_u8; 4];
        if !read_exact_interruptible(&mut stream, &mut length, &connection.closed)? {
            return Ok(());
        }
        let length = usize::try_from(u32::from_be_bytes(length))
            .map_err(|_| BrokerErrorCode::QuotaExceeded)?;
        if length == 0 || length > connection.maximum_frame_bytes {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        reserve_traffic(&quota, length, connection.maximum_bytes_per_minute)?;
        let mut bytes = vec![0_u8; length];
        if !read_exact_interruptible(&mut stream, &mut bytes, &connection.closed)? {
            return Err(BrokerErrorCode::BackendFailed);
        }
        let payload = LocalFrameEvent { bytes }.encode().map_err(schema_error)?;
        events.emit_buffered(payload)?;
    })();
    match result {
        Ok(()) if !events.is_finished() => {
            let _ = events.complete(Vec::new());
        }
        Err(error) if !events.is_finished() => events.finish(error),
        _ => {}
    }
}

fn read_exact_interruptible(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    closed: &AtomicBool,
) -> Result<bool, BrokerErrorCode> {
    let mut offset = 0;
    while offset < bytes.len() {
        if closed.load(Ordering::Acquire) {
            return Err(BrokerErrorCode::Cancelled);
        }
        match stream.read(&mut bytes[offset..]) {
            Ok(0) if offset == 0 => return Ok(false),
            Ok(0) => return Err(BrokerErrorCode::BackendFailed),
            Ok(read) => offset += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(BrokerErrorCode::BackendFailed),
        }
    }
    Ok(true)
}

fn write_all_interruptible(
    stream: &mut UnixStream,
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<(), BrokerErrorCode> {
    let mut offset = 0;
    while offset < bytes.len() {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        match stream.write(&bytes[offset..]) {
            Ok(0) => return Err(BrokerErrorCode::BackendFailed),
            Ok(written) => offset += written,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(BrokerErrorCode::BackendFailed),
        }
    }
    Ok(())
}

fn reserve_traffic(
    quota: &PersistentQuota,
    bytes: usize,
    maximum_bytes_per_minute: u64,
) -> Result<(), BrokerErrorCode> {
    match quota.reserve(
        u64::try_from(bytes).map_err(|_| BrokerErrorCode::QuotaExceeded)?,
        maximum_bytes_per_minute,
    ) {
        Err(BrokerErrorCode::QuotaExceeded) => Err(BrokerErrorCode::RateLimited),
        result => result,
    }
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

struct LocalConnectionHandle {
    resource_id: u64,
    connection: Arc<LocalConnection>,
    connections: Arc<Mutex<BTreeMap<u64, Arc<LocalConnection>>>>,
    reader_thread: Option<JoinHandle<()>>,
}

impl ResourceHandle for LocalConnectionHandle {
    fn close(&mut self) {
        if !self.connection.closed.swap(true, Ordering::AcqRel)
            && let Ok(stream) = self.connection.stream.lock()
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Ok(mut connections) = self.connections.lock() {
            connections.remove(&self.resource_id);
        }
        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LocalConnectionHandle {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };

    use semver::Version;
    use tempfile::tempdir;
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        GrantBindings, LocalConnectScope, LocalEndpointBinding, LocalEndpointRequest,
        PackageInstance, Provenance, RuntimeKind,
    };

    use super::*;
    use crate::{ConnectionIdentity, HostEvent, HostEventQueue, ResourceManager};

    fn identity(instance_id: u64) -> ConnectionIdentity {
        ConnectionIdentity {
            instance_id,
            package: PackageInstance {
                source: GithubSource::new("alice", "local-test").unwrap(),
                version: Version::new(1, 0, 0),
                digest: format!("sha256:{}", "a".repeat(64)),
                provenance: Provenance::VerifiedRelease,
                runtime: RuntimeKind::Component,
            },
        }
    }

    fn scope() -> LocalConnectScope {
        LocalConnectScope {
            endpoints: BTreeSet::from([LocalEndpointRequest {
                label: "test-service".into(),
                protocol: "test.frames.v1".into(),
                suggested_endpoint: Some("/attacker/manifest.sock".into()),
            }]),
            maximum_frame_bytes: 1024,
            maximum_bytes_per_minute: 4096,
        }
    }

    fn request(
        request_id: u64,
        operation: &str,
        payload: Vec<u8>,
        socket: PathBuf,
        instance_id: u64,
    ) -> BackendRequest {
        BackendRequest {
            identity: identity(instance_id),
            request_id,
            capability: CapabilityId::LocalConnectV1,
            authorized_scope: CapabilityScope::LocalConnect(scope()),
            bindings: GrantBindings {
                local_endpoints: BTreeMap::from([(
                    "test-service".into(),
                    LocalEndpointBinding::UnixStream { path: socket },
                )]),
                ..Default::default()
            },
            activation: None,
            operation: operation.into(),
            payload,
        }
    }

    #[test]
    fn exact_bound_socket_round_trips_frames_and_ignores_manifest_hint() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut bytes = vec![0_u8; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, b"hello");
            stream.write_all(&(5_u32.to_be_bytes())).unwrap();
            stream.write_all(b"world").unwrap();
        });

        let backend =
            Arc::new(LocalIpcBackend::new(directory.path(), "github:alice/local-test").unwrap());
        let connect = request(
            1,
            LOCAL_CONNECT_OPERATION,
            LocalConnect {
                endpoint: "test-service".into(),
                protocol: "test.frames.v1".into(),
            }
            .encode()
            .unwrap(),
            socket.clone(),
            7,
        );
        let mut resources = ResourceManager::new(16).unwrap();
        resources.register(CapabilityId::LocalConnectV1, backend.clone());
        let limits = ResourceBackend::limits(backend.as_ref(), &connect).unwrap();
        let opened = resources
            .open(9, &connect, limits, &mut ActivationLedger::new(8), 1)
            .unwrap();
        assert_eq!(
            LocalConnectionOpened::decode(&opened).unwrap(),
            LocalConnectionOpened {
                resource_id: 9,
                maximum_frame_bytes: 1024,
            }
        );

        let send = request(
            2,
            LOCAL_SEND_OPERATION,
            LocalSendFrame {
                resource_id: 9,
                bytes: b"hello".to_vec(),
            }
            .encode()
            .unwrap(),
            socket,
            7,
        );
        let cancellation =
            CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(2))
                .unwrap();
        assert_eq!(
            Backend::execute(backend.as_ref(), &send, &cancellation),
            BrokerResult::Success {
                payload: Vec::new()
            }
        );
        server.join().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = HostEventQueue::new(8, 16);
        let mut received = None;
        while Instant::now() < deadline {
            resources.pump(&mut events, 1);
            while let Some(event) = events.pop() {
                if let HostEvent::ResourceEvent {
                    resource_id: 9,
                    result: BrokerResult::Success { payload },
                    ..
                } = event
                    && !payload.is_empty()
                {
                    received = Some(LocalFrameEvent::decode(&payload).unwrap().bytes);
                }
            }
            if received.is_some() && resources.resource_count() == 0 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(received, Some(b"world".to_vec()));
        assert_eq!(resources.resource_count(), 0);
    }

    #[test]
    fn symlinked_parent_regular_file_and_hardlinked_socket_are_rejected() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let real = directory.path().join("real");
        fs::create_dir(&real).unwrap();
        let socket = real.join("service.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert!(matches!(
            connect_bound_socket(&LocalEndpointBinding::UnixStream {
                path: alias.join("service.sock"),
            }),
            Err(BrokerErrorCode::OutOfScope)
        ));

        let regular = directory.path().join("regular");
        fs::write(&regular, b"not a socket").unwrap();
        assert!(matches!(
            connect_bound_socket(&LocalEndpointBinding::UnixStream { path: regular }),
            Err(BrokerErrorCode::OutOfScope)
        ));

        let hardlink = real.join("hardlink.sock");
        fs::hard_link(&socket, &hardlink).unwrap();
        assert!(matches!(
            connect_bound_socket(&LocalEndpointBinding::UnixStream { path: socket }),
            Err(BrokerErrorCode::OutOfScope)
        ));
    }

    #[test]
    fn pathname_replacement_cannot_redirect_a_pinned_connection() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("service.sock");
        let moved = directory.path().join("original.sock");
        let original = UnixListener::bind(&socket).unwrap();
        let descriptor = open_pinned_socket(&socket).unwrap();
        fs::rename(&socket, &moved).unwrap();
        let attacker = UnixListener::bind(&socket).unwrap();
        attacker.set_nonblocking(true).unwrap();

        let connection = connect_pinned_socket(&descriptor).unwrap();
        let (_accepted, _) = original.accept().unwrap();
        assert_eq!(
            attacker.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(connection);
    }

    #[test]
    fn send_rechecks_instance_scope_binding_and_frame_limit() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let accept = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let backend = Arc::new(LocalIpcBackend::new(directory.path(), "scope-test").unwrap());
        let connect = request(
            1,
            LOCAL_CONNECT_OPERATION,
            LocalConnect {
                endpoint: "test-service".into(),
                protocol: "test.frames.v1".into(),
            }
            .encode()
            .unwrap(),
            socket.clone(),
            7,
        );
        let mut resources = ResourceManager::new(16).unwrap();
        resources.register(CapabilityId::LocalConnectV1, backend.clone());
        let limits = ResourceBackend::limits(backend.as_ref(), &connect).unwrap();
        resources
            .open(9, &connect, limits, &mut ActivationLedger::new(8), 1)
            .unwrap();

        let payload = LocalSendFrame {
            resource_id: 9,
            bytes: b"x".to_vec(),
        }
        .encode()
        .unwrap();
        let wrong_instance = request(2, LOCAL_SEND_OPERATION, payload.clone(), socket.clone(), 8);
        assert_eq!(
            Backend::authorize(
                backend.as_ref(),
                &wrong_instance,
                &mut ActivationLedger::new(8),
                1,
            ),
            Err(BrokerErrorCode::OutOfScope)
        );
        let mut wrong_binding = request(3, LOCAL_SEND_OPERATION, payload, socket, 7);
        wrong_binding.bindings.local_endpoints.insert(
            "test-service".into(),
            LocalEndpointBinding::UnixStream {
                path: "/run/user/1000/other.sock".into(),
            },
        );
        assert_eq!(
            Backend::authorize(
                backend.as_ref(),
                &wrong_binding,
                &mut ActivationLedger::new(8),
                1,
            ),
            Err(BrokerErrorCode::OutOfScope)
        );
        resources.close(9);
        accept.join().unwrap();
    }

    #[test]
    fn oversized_inbound_length_is_terminal_without_allocating_the_claimed_frame() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(&u32::MAX.to_be_bytes()).unwrap();
        });
        let backend = Arc::new(LocalIpcBackend::new(directory.path(), "oversized-test").unwrap());
        let connect = request(
            1,
            LOCAL_CONNECT_OPERATION,
            LocalConnect {
                endpoint: "test-service".into(),
                protocol: "test.frames.v1".into(),
            }
            .encode()
            .unwrap(),
            socket,
            7,
        );
        let mut resources = ResourceManager::new(16).unwrap();
        resources.register(CapabilityId::LocalConnectV1, backend.clone());
        let limits = ResourceBackend::limits(backend.as_ref(), &connect).unwrap();
        resources
            .open(9, &connect, limits, &mut ActivationLedger::new(8), 1)
            .unwrap();
        server.join().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = HostEventQueue::new(8, 16);
        let mut terminal = None;
        while Instant::now() < deadline {
            resources.pump(&mut events, 1);
            while let Some(event) = events.pop() {
                if let HostEvent::ResourceEvent {
                    resource_id: 9,
                    result: BrokerResult::Error(error),
                    ..
                } = event
                {
                    terminal = Some(error);
                }
            }
            if resources.resource_count() == 0 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(terminal, Some(BrokerErrorCode::QuotaExceeded));
        assert_eq!(resources.resource_count(), 0);
    }

    #[test]
    fn outbound_traffic_uses_restart_resistant_source_quota() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let accept = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, b"four");
            thread::sleep(Duration::from_millis(100));
        });
        let mut narrow_scope = scope();
        narrow_scope.maximum_bytes_per_minute = 4;
        let make_request = |request_id, operation: &str, payload| {
            let mut value = request(request_id, operation, payload, socket.clone(), 7);
            value.authorized_scope = CapabilityScope::LocalConnect(narrow_scope.clone());
            value
        };
        let backend = Arc::new(LocalIpcBackend::new(directory.path(), "restart-quota").unwrap());
        let connect = make_request(
            1,
            LOCAL_CONNECT_OPERATION,
            LocalConnect {
                endpoint: "test-service".into(),
                protocol: "test.frames.v1".into(),
            }
            .encode()
            .unwrap(),
        );
        let mut resources = ResourceManager::new(16).unwrap();
        resources.register(CapabilityId::LocalConnectV1, backend.clone());
        let limits = ResourceBackend::limits(backend.as_ref(), &connect).unwrap();
        resources
            .open(9, &connect, limits, &mut ActivationLedger::new(8), 1)
            .unwrap();
        let send = make_request(
            2,
            LOCAL_SEND_OPERATION,
            LocalSendFrame {
                resource_id: 9,
                bytes: b"four".to_vec(),
            }
            .encode()
            .unwrap(),
        );
        let cancellation =
            CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(2))
                .unwrap();
        assert_eq!(
            Backend::execute(backend.as_ref(), &send, &cancellation),
            BrokerResult::Success {
                payload: Vec::new()
            }
        );
        resources.close(9);
        accept.join().unwrap();
        drop(backend);

        let restarted = LocalIpcBackend::new(directory.path(), "restart-quota").unwrap();
        assert_eq!(
            restarted.quota.reserve(1, 4),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        let isolated = LocalIpcBackend::new(directory.path(), "different-source").unwrap();
        assert_eq!(isolated.quota.reserve(1, 4), Ok(()));
    }
}
