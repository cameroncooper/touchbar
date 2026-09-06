use std::{
    collections::{BTreeSet, HashMap},
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Result as AnyResult, bail};
use touchbar_broker_schema::{
    ClipboardReadRequest, ClipboardValue, ClipboardWriteRequest, MAX_CLIPBOARD_BYTES,
    MAX_CLIPBOARD_MIME_BYTES, SchemaError,
};
use touchbar_policy::{CapabilityId, CapabilityScope, ClipboardBinding, ClipboardScope};
use touchbar_protocol::broker_ipc::{ActivationOrigin, BrokerErrorCode, BrokerResult};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
    protocol::{wl_callback, wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use zeroize::Zeroizing;

use crate::{
    ActivationExpectation, ActivationLedger, Backend, BackendRequest, CancellationToken,
    filesystem_write::PersistentQuota, local::connect_same_user_socket,
};

pub const CLIPBOARD_READ_OPERATION: &str = "read";
pub const CLIPBOARD_WRITE_OPERATION: &str = "write";

/// The bounded operation authorized before a clipboard transport is touched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardAuthorization {
    Read {
        maximum_bytes: usize,
        maximum_operations_per_minute: u16,
    },
    Write {
        maximum_bytes: usize,
        maximum_operations_per_minute: u16,
    },
}

impl ClipboardAuthorization {
    pub const fn maximum_bytes(self) -> usize {
        match self {
            Self::Read { maximum_bytes, .. } | Self::Write { maximum_bytes, .. } => maximum_bytes,
        }
    }

    pub const fn maximum_operations_per_minute(self) -> u16 {
        match self {
            Self::Read {
                maximum_operations_per_minute,
                ..
            }
            | Self::Write {
                maximum_operations_per_minute,
                ..
            } => maximum_operations_per_minute,
        }
    }
}

pub struct ClipboardMaterial {
    bytes: Zeroizing<Vec<u8>>,
}

impl ClipboardMaterial {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }
}

/// Narrow clipboard boundary. The broker selects one exact grant-owned
/// compositor socket and MIME type; transports never inspect ambient desktop
/// environment variables and never enumerate clipboard formats for a guest.
pub trait ClipboardTransport: Send + Sync + 'static {
    fn read(
        &self,
        binding: &ClipboardBinding,
        mime_type: &str,
        maximum_bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<ClipboardMaterial, BrokerErrorCode>;

    fn write(
        &self,
        binding: &ClipboardBinding,
        mime_type: &str,
        bytes: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<(), BrokerErrorCode>;
}

pub struct ClipboardBackend<T> {
    transport: T,
    quota: PersistentQuota,
}

impl<T> ClipboardBackend<T> {
    pub fn new(transport: T, state_directory: &Path, quota_namespace: &str) -> AnyResult<Self> {
        if quota_namespace.is_empty() {
            bail!("clipboard quota namespace is empty");
        }
        Ok(Self {
            transport,
            quota: PersistentQuota::new(
                state_directory,
                &format!("clipboard:{quota_namespace}"),
                60,
            )?,
        })
    }
}

impl<T: ClipboardTransport> Backend for ClipboardBackend<T> {
    fn authorize(
        &self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        let authorization =
            authorize_clipboard_request(request, activations, now_monotonic_micros)?;
        self.quota
            .reserve(1, u64::from(authorization.maximum_operations_per_minute()))
            .map_err(|error| {
                if error == BrokerErrorCode::QuotaExceeded {
                    BrokerErrorCode::RateLimited
                } else {
                    error
                }
            })
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            match decode_and_match(request)? {
                ClipboardOperation::Read {
                    value,
                    scope,
                    binding,
                } => {
                    let maximum_bytes = effective_maximum(scope)?;
                    let material = self.transport.read(
                        binding,
                        &value.mime_type,
                        maximum_bytes,
                        cancellation,
                    )?;
                    if material.bytes.len() > maximum_bytes {
                        return Err(BrokerErrorCode::QuotaExceeded);
                    }
                    if let Some(reason) = cancellation.reason() {
                        return Err(reason);
                    }
                    let payload = ClipboardValue {
                        mime_type: value.mime_type,
                        bytes: material.bytes.as_slice().to_vec(),
                    }
                    .encode()
                    .map_err(schema_error)?;
                    Ok(BrokerResult::Success { payload })
                }
                ClipboardOperation::Write {
                    value,
                    scope,
                    binding,
                } => {
                    let maximum_bytes = effective_maximum(scope)?;
                    let bytes = Zeroizing::new(value.bytes);
                    if bytes.len() > maximum_bytes {
                        return Err(BrokerErrorCode::QuotaExceeded);
                    }
                    self.transport.write(
                        binding,
                        &value.mime_type,
                        bytes.as_slice(),
                        cancellation,
                    )?;
                    cancellation.reason().map_or(
                        Ok(BrokerResult::Success {
                            payload: Vec::new(),
                        }),
                        Err,
                    )
                }
            }
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

pub fn authorize_clipboard_request(
    request: &BackendRequest,
    activations: &mut ActivationLedger,
    now_monotonic_micros: u64,
) -> Result<ClipboardAuthorization, BrokerErrorCode> {
    let operation = decode_and_match(request)?;
    let activation = request
        .activation
        .as_ref()
        .ok_or(BrokerErrorCode::ActivationRequired)?;
    let origin = activations.consume(
        Some(activation),
        ActivationExpectation {
            surface_instance: activation.surface_instance,
            item_id: &activation.item_id,
            widget_id: activation.widget_id,
            now_monotonic_micros,
        },
    )?;
    if origin != ActivationOrigin::Physical {
        return Err(BrokerErrorCode::ActivationRequired);
    }
    let scope = operation.scope();
    let maximum_bytes = effective_maximum(scope)?;
    let maximum_operations_per_minute = scope.maximum_operations_per_minute;
    if let ClipboardOperation::Write { value, .. } = &operation
        && value.bytes.len() > maximum_bytes
    {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(match operation {
        ClipboardOperation::Read { .. } => ClipboardAuthorization::Read {
            maximum_bytes,
            maximum_operations_per_minute,
        },
        ClipboardOperation::Write { .. } => ClipboardAuthorization::Write {
            maximum_bytes,
            maximum_operations_per_minute,
        },
    })
}

enum ClipboardOperation<'a> {
    Read {
        value: ClipboardReadRequest,
        scope: &'a ClipboardScope,
        binding: &'a ClipboardBinding,
    },
    Write {
        value: ClipboardWriteRequest,
        scope: &'a ClipboardScope,
        binding: &'a ClipboardBinding,
    },
}

impl ClipboardOperation<'_> {
    fn scope(&self) -> &ClipboardScope {
        match self {
            Self::Read { scope, .. } | Self::Write { scope, .. } => scope,
        }
    }
}

fn decode_and_match(request: &BackendRequest) -> Result<ClipboardOperation<'_>, BrokerErrorCode> {
    let CapabilityScope::Clipboard(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let binding = request
        .bindings
        .clipboard
        .as_ref()
        .ok_or(BrokerErrorCode::OutOfScope)?;
    match (&request.capability, request.operation.as_str()) {
        (CapabilityId::ClipboardReadV1, CLIPBOARD_READ_OPERATION) => {
            let value = ClipboardReadRequest::decode(&request.payload).map_err(schema_error)?;
            validate_mime(scope, &value.mime_type)?;
            Ok(ClipboardOperation::Read {
                value,
                scope,
                binding,
            })
        }
        (CapabilityId::ClipboardWriteV1, CLIPBOARD_WRITE_OPERATION) => {
            let value = ClipboardWriteRequest::decode(&request.payload).map_err(schema_error)?;
            validate_mime(scope, &value.mime_type)?;
            Ok(ClipboardOperation::Write {
                value,
                scope,
                binding,
            })
        }
        _ => Err(BrokerErrorCode::InvalidRequest),
    }
}

fn validate_mime(scope: &ClipboardScope, mime_type: &str) -> Result<(), BrokerErrorCode> {
    if mime_type.is_empty()
        || mime_type.len() > MAX_CLIPBOARD_MIME_BYTES
        || !mime_type.is_ascii()
        || mime_type
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || !mime_type.contains('/')
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    if !scope.mime_types.contains(mime_type) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(())
}

fn effective_maximum(scope: &ClipboardScope) -> Result<usize, BrokerErrorCode> {
    usize::try_from(scope.maximum_bytes)
        .map(|value| value.min(MAX_CLIPBOARD_BYTES))
        .map_err(|_| BrokerErrorCode::QuotaExceeded)
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

const WAYLAND_OPERATION_TIMEOUT: Duration = Duration::from_secs(4);
const WAYLAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_WAYLAND_OFFERS: usize = 32;
const MAX_WAYLAND_MIME_TYPES: usize = 64;

#[derive(Default)]
pub struct WaylandClipboard {
    owner: Mutex<Option<ClipboardOwner>>,
}

impl WaylandClipboard {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ClipboardTransport for WaylandClipboard {
    fn read(
        &self,
        binding: &ClipboardBinding,
        mime_type: &str,
        maximum_bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<ClipboardMaterial, BrokerErrorCode> {
        let mut session = WaylandSession::connect(binding)?;
        session.discover(cancellation)?;
        let manager = session
            .state
            .manager
            .clone()
            .ok_or(BrokerErrorCode::Unavailable)?;
        let seat = session
            .state
            .seat
            .clone()
            .ok_or(BrokerErrorCode::Unavailable)?;
        let _device = manager.get_data_device(&seat, &session.queue.handle(), ());
        session.pump_until(cancellation, |state| state.selection_seen)?;
        let offer = session
            .state
            .selection
            .clone()
            .ok_or(BrokerErrorCode::Unavailable)?;
        let offered = session
            .state
            .offers
            .get(&offer.id())
            .ok_or(BrokerErrorCode::Unavailable)?;
        if !offered.contains(mime_type) {
            return Err(BrokerErrorCode::Unavailable);
        }

        let (mut reader, writer) = nonblocking_pipe()?;
        offer.receive(mime_type.into(), writer.as_fd());
        session
            .connection
            .flush()
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        drop(writer);
        let bytes = read_bounded_pipe(&mut reader, maximum_bytes, cancellation)?;
        Ok(ClipboardMaterial::new(bytes))
    }

    fn write(
        &self,
        binding: &ClipboardBinding,
        mime_type: &str,
        bytes: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<(), BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let source = Arc::new(SourcePayload {
            mime_type: mime_type.into(),
            bytes: Zeroizing::new(bytes.to_vec()),
            stop: Arc::clone(&stop),
        });
        let binding = binding.clone();
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let owner_thread = thread::Builder::new()
            .name("touchbar-clipboard-owner".into())
            .spawn(move || own_selection(binding, source, result_sender))
            .map_err(|_| BrokerErrorCode::Unavailable)?;
        let mut owner = ClipboardOwner {
            stop,
            thread: Some(owner_thread),
        };
        let deadline = Instant::now() + WAYLAND_OPERATION_TIMEOUT;
        let result = loop {
            if let Some(reason) = cancellation.reason() {
                break Err(reason);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break Err(BrokerErrorCode::BackendFailed);
            };
            match result_receiver.recv_timeout(remaining.min(WAYLAND_POLL_INTERVAL)) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(BrokerErrorCode::BackendFailed);
                }
            }
        };
        if let Err(error) = result {
            owner.stop_and_join();
            return Err(error);
        }
        let previous = self
            .owner
            .lock()
            .map_err(|_| BrokerErrorCode::Internal)?
            .replace(owner);
        if let Some(mut previous) = previous {
            previous.stop_and_join();
        }
        Ok(())
    }
}

struct ClipboardOwner {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ClipboardOwner {
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

struct SourcePayload {
    mime_type: String,
    bytes: Zeroizing<Vec<u8>>,
    stop: Arc<AtomicBool>,
}

fn own_selection(
    binding: ClipboardBinding,
    source_data: Arc<SourcePayload>,
    result: mpsc::SyncSender<Result<(), BrokerErrorCode>>,
) {
    let startup = (|| {
        let mut session = WaylandSession::connect(&binding)?;
        session.discover_with_stop(&source_data.stop)?;
        let manager = session
            .state
            .manager
            .clone()
            .ok_or(BrokerErrorCode::Unavailable)?;
        let seat = session
            .state
            .seat
            .clone()
            .ok_or(BrokerErrorCode::Unavailable)?;
        let device = manager.get_data_device(&seat, &session.queue.handle(), ());
        let source = manager.create_data_source(&session.queue.handle(), Arc::clone(&source_data));
        source.offer(source_data.mime_type.clone());
        device.set_selection(Some(&source));
        session.state.waiting_for_sync = true;
        let _callback = session
            .connection
            .display()
            .sync(&session.queue.handle(), ());
        session.pump_until_with_stop(&source_data.stop, |state| !state.waiting_for_sync)?;
        Ok(session)
    })();

    let mut session = match startup {
        Ok(session) => {
            let _ = result.send(Ok(()));
            session
        }
        Err(error) => {
            let _ = result.send(Err(error));
            return;
        }
    };
    while !source_data.stop.load(Ordering::Acquire)
        && !session.state.source_cancelled
        && !session.state.finished
    {
        if session.pump_once().is_err() {
            break;
        }
    }
}

#[derive(Default)]
struct WaylandState {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ExtDataControlManagerV1>,
    offers: HashMap<wayland_client::backend::ObjectId, BTreeSet<String>>,
    selection: Option<ExtDataControlOfferV1>,
    selection_seen: bool,
    source_cancelled: bool,
    finished: bool,
    waiting_for_sync: bool,
    malformed: bool,
}

struct WaylandSession {
    connection: Connection,
    queue: EventQueue<WaylandState>,
    state: WaylandState,
}

impl WaylandSession {
    fn connect(binding: &ClipboardBinding) -> Result<Self, BrokerErrorCode> {
        let ClipboardBinding::WaylandDataControl { socket } = binding;
        let stream = connect_same_user_socket(socket)?;
        let connection =
            Connection::from_socket(stream).map_err(|_| BrokerErrorCode::Unavailable)?;
        let queue = connection.new_event_queue();
        let _registry = connection.display().get_registry(&queue.handle(), ());
        connection
            .flush()
            .map_err(|_| BrokerErrorCode::Unavailable)?;
        Ok(Self {
            connection,
            queue,
            state: WaylandState::default(),
        })
    }

    fn discover(&mut self, cancellation: &CancellationToken) -> Result<(), BrokerErrorCode> {
        self.pump_until(cancellation, |state| {
            state.seat.is_some() && state.manager.is_some()
        })
    }

    fn discover_with_stop(&mut self, stop: &AtomicBool) -> Result<(), BrokerErrorCode> {
        self.pump_until_with_stop(stop, |state| {
            state.seat.is_some() && state.manager.is_some()
        })
    }

    fn pump_until(
        &mut self,
        cancellation: &CancellationToken,
        predicate: impl Fn(&WaylandState) -> bool,
    ) -> Result<(), BrokerErrorCode> {
        let deadline = Instant::now() + WAYLAND_OPERATION_TIMEOUT;
        while !predicate(&self.state) {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            if Instant::now() >= deadline {
                return Err(BrokerErrorCode::BackendFailed);
            }
            self.pump_once()?;
        }
        Ok(())
    }

    fn pump_until_with_stop(
        &mut self,
        stop: &AtomicBool,
        predicate: impl Fn(&WaylandState) -> bool,
    ) -> Result<(), BrokerErrorCode> {
        let deadline = Instant::now() + WAYLAND_OPERATION_TIMEOUT;
        while !predicate(&self.state) {
            if stop.load(Ordering::Acquire) {
                return Err(BrokerErrorCode::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(BrokerErrorCode::BackendFailed);
            }
            self.pump_once()?;
        }
        Ok(())
    }

    fn pump_once(&mut self) -> Result<(), BrokerErrorCode> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        if self.state.malformed {
            return Err(BrokerErrorCode::BackendFailed);
        }
        self.connection
            .flush()
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        let mut descriptor = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for this call.
        let polled = unsafe { libc::poll(&mut descriptor, 1, 50) };
        if polled < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(BrokerErrorCode::BackendFailed);
        }
        if polled == 0 {
            drop(guard);
            return Ok(());
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(BrokerErrorCode::Unavailable);
        }
        guard.read().map_err(|_| BrokerErrorCode::BackendFailed)?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        if self.state.malformed {
            return Err(BrokerErrorCode::BackendFailed);
        }
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _connection: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(1), queue, ()));
                }
                "ext_data_control_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, version.min(1), queue, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandState: ignore ExtDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                if state.offers.len() >= MAX_WAYLAND_OFFERS {
                    state.malformed = true;
                    id.destroy();
                } else {
                    state.offers.insert(id.id(), BTreeSet::new());
                }
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                if let Some(previous) = state.selection.take() {
                    state.offers.remove(&previous.id());
                    previous.destroy();
                }
                state.selection = id;
                state.selection_seen = true;
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(id) = id {
                    state.offers.remove(&id.id());
                    id.destroy();
                }
            }
            ext_data_control_device_v1::Event::Finished => state.finished = true,
            _ => state.malformed = true,
        }
    }

    event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ())
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_offer_v1::Event::Offer { mime_type } => {
                let Some(types) = state.offers.get_mut(&proxy.id()) else {
                    state.malformed = true;
                    return;
                };
                if !valid_wayland_mime(&mime_type)
                    || types.len() >= MAX_WAYLAND_MIME_TYPES
                    || !types.insert(mime_type)
                {
                    state.malformed = true;
                }
            }
            _ => state.malformed = true,
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Arc<SourcePayload>> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &Arc<SourcePayload>,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                if mime_type == data.mime_type {
                    let _ = write_bounded_fd(fd, data.bytes.as_slice(), &data.stop);
                }
            }
            ext_data_control_source_v1::Event::Cancelled => state.source_cancelled = true,
            _ => state.malformed = true,
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            wl_callback::Event::Done { .. } => state.waiting_for_sync = false,
            _ => state.malformed = true,
        }
    }
}

fn valid_wayland_mime(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CLIPBOARD_MIME_BYTES
        && value.is_ascii()
        && value.contains('/')
        && value
            .bytes()
            .all(|byte| !byte.is_ascii_control() && !byte.is_ascii_whitespace())
}

fn nonblocking_pipe() -> Result<(File, OwnedFd), BrokerErrorCode> {
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors has room for two file descriptors.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(BrokerErrorCode::Unavailable);
    }
    // SAFETY: successful pipe2 returned two unique owned descriptors.
    Ok(unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn read_bounded_pipe(
    reader: &mut File,
    maximum_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, BrokerErrorCode> {
    let deadline = Instant::now() + WAYLAND_OPERATION_TIMEOUT;
    let mut bytes = Vec::with_capacity(maximum_bytes.min(4096));
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(read) => {
                if bytes.len().saturating_add(read) > maximum_bytes {
                    return Err(BrokerErrorCode::QuotaExceeded);
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(BrokerErrorCode::BackendFailed);
                }
                poll_descriptor(reader.as_raw_fd(), libc::POLLIN)?;
            }
            Err(_) => return Err(BrokerErrorCode::BackendFailed),
        }
    }
}

fn write_bounded_fd(
    descriptor: OwnedFd,
    bytes: &[u8],
    stop: &AtomicBool,
) -> Result<(), BrokerErrorCode> {
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                descriptor.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } < 0
    {
        return Err(BrokerErrorCode::BackendFailed);
    }
    let deadline = Instant::now() + WAYLAND_OPERATION_TIMEOUT;
    let mut descriptor = File::from(descriptor);
    let mut offset = 0;
    while offset < bytes.len() {
        if stop.load(Ordering::Acquire) {
            return Err(BrokerErrorCode::Cancelled);
        }
        match descriptor.write(&bytes[offset..]) {
            Ok(0) => return Err(BrokerErrorCode::BackendFailed),
            Ok(written) => offset += written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(BrokerErrorCode::BackendFailed);
                }
                poll_descriptor(descriptor.as_raw_fd(), libc::POLLOUT)?;
            }
            Err(_) => return Err(BrokerErrorCode::BackendFailed),
        }
    }
    Ok(())
}

fn poll_descriptor(descriptor: i32, events: i16) -> Result<(), BrokerErrorCode> {
    let mut poll = libc::pollfd {
        fd: descriptor,
        events,
        revents: 0,
    };
    // SAFETY: poll points to one initialized descriptor for this call.
    let result = unsafe { libc::poll(&mut poll, 1, 50) };
    if result < 0 {
        return if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(BrokerErrorCode::BackendFailed)
        };
    }
    if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(BrokerErrorCode::BackendFailed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap, VecDeque},
        fs,
        os::unix::fs::PermissionsExt,
        os::unix::net::UnixListener,
        path::PathBuf,
        sync::{Arc, Mutex, atomic::AtomicBool},
        thread,
        time::Instant,
    };

    use semver::Version;
    use tempfile::tempdir;
    use touchbar_package::GithubSource;
    use touchbar_policy::{GrantBindings, PackageInstance, Provenance, RuntimeKind};
    use touchbar_protocol::broker_ipc::{ActivationContext, ActivationOrigin};
    use wayland_protocols::ext::data_control::v1::server::{
        ext_data_control_device_v1::{
            self as server_device, ExtDataControlDeviceV1 as ServerDevice,
        },
        ext_data_control_manager_v1::{
            self as server_manager, ExtDataControlManagerV1 as ServerManager,
        },
        ext_data_control_offer_v1::{self as server_offer, ExtDataControlOfferV1 as ServerOffer},
        ext_data_control_source_v1::{
            self as server_source, ExtDataControlSourceV1 as ServerSource,
        },
    };
    use wayland_server::{
        Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
        Resource, protocol::wl_seat as server_seat,
    };

    use super::*;
    use crate::ConnectionIdentity;

    #[derive(Default)]
    struct FakeClipboard {
        reads: Mutex<Vec<(ClipboardBinding, String, usize)>>,
        writes: Mutex<Vec<(ClipboardBinding, String, Vec<u8>)>>,
        results: Mutex<VecDeque<Result<Vec<u8>, BrokerErrorCode>>>,
    }

    impl ClipboardTransport for Arc<FakeClipboard> {
        fn read(
            &self,
            binding: &ClipboardBinding,
            mime_type: &str,
            maximum_bytes: usize,
            _cancellation: &CancellationToken,
        ) -> Result<ClipboardMaterial, BrokerErrorCode> {
            self.reads
                .lock()
                .unwrap()
                .push((binding.clone(), mime_type.into(), maximum_bytes));
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(b"clipboard".to_vec()))
                .map(ClipboardMaterial::new)
        }

        fn write(
            &self,
            binding: &ClipboardBinding,
            mime_type: &str,
            bytes: &[u8],
            _cancellation: &CancellationToken,
        ) -> Result<(), BrokerErrorCode> {
            self.writes
                .lock()
                .unwrap()
                .push((binding.clone(), mime_type.into(), bytes.to_vec()));
            Ok(())
        }
    }

    fn binding(path: &str) -> ClipboardBinding {
        ClipboardBinding::WaylandDataControl {
            socket: PathBuf::from(path),
        }
    }

    fn scope(maximum_bytes: u64, maximum_operations_per_minute: u16) -> ClipboardScope {
        ClipboardScope {
            mime_types: BTreeSet::from(["text/plain;charset=utf-8".into()]),
            maximum_bytes,
            maximum_operations_per_minute,
        }
    }

    fn request(
        capability: CapabilityId,
        operation: &str,
        payload: Vec<u8>,
        socket: &str,
        sequence: u64,
        origin: ActivationOrigin,
        scope: ClipboardScope,
    ) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 4,
                package: PackageInstance {
                    source: GithubSource::new("alice", "clipboard-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: sequence,
            capability,
            operation: operation.into(),
            payload,
            authorized_scope: CapabilityScope::Clipboard(scope),
            bindings: GrantBindings {
                clipboard: Some(binding(socket)),
                ..Default::default()
            },
            activation: Some(ActivationContext {
                origin,
                surface_instance: 7,
                item_id: "clipboard".into(),
                widget_id: 9,
                input_sequence: sequence,
                deadline_monotonic_micros: 100,
            }),
        }
    }

    fn cancellation() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)).unwrap()
    }

    #[test]
    fn exact_mime_read_uses_grant_owned_binding_and_physical_activation() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let fake = Arc::new(FakeClipboard::default());
        let backend = ClipboardBackend::new(fake.clone(), directory.path(), "read-test").unwrap();
        let request = request(
            CapabilityId::ClipboardReadV1,
            CLIPBOARD_READ_OPERATION,
            ClipboardReadRequest {
                mime_type: "text/plain;charset=utf-8".into(),
            }
            .encode()
            .unwrap(),
            "/run/user/1000/wayland-7",
            1,
            ActivationOrigin::Physical,
            scope(4096, 4),
        );
        let mut activations = ActivationLedger::new(8);
        assert_eq!(
            Backend::authorize(&backend, &request, &mut activations, 99),
            Ok(())
        );
        let result = Backend::execute(&backend, &request, &cancellation());
        let BrokerResult::Success { payload } = result else {
            panic!("read failed: {result:?}");
        };
        assert_eq!(
            ClipboardValue::decode(&payload).unwrap(),
            ClipboardValue {
                mime_type: "text/plain;charset=utf-8".into(),
                bytes: b"clipboard".to_vec(),
            }
        );
        assert_eq!(
            *fake.reads.lock().unwrap(),
            vec![(
                binding("/run/user/1000/wayland-7"),
                "text/plain;charset=utf-8".into(),
                4096,
            )]
        );
    }

    #[test]
    fn trusted_control_replay_and_unapproved_mime_never_reach_transport() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let fake = Arc::new(FakeClipboard::default());
        let backend = ClipboardBackend::new(fake.clone(), directory.path(), "gate-test").unwrap();
        let trusted = request(
            CapabilityId::ClipboardReadV1,
            CLIPBOARD_READ_OPERATION,
            ClipboardReadRequest {
                mime_type: "text/plain;charset=utf-8".into(),
            }
            .encode()
            .unwrap(),
            "/run/user/1000/wayland-1",
            1,
            ActivationOrigin::TrustedControl,
            scope(1024, 4),
        );
        let mut activations = ActivationLedger::new(8);
        assert_eq!(
            Backend::authorize(&backend, &trusted, &mut activations, 99),
            Err(BrokerErrorCode::ActivationRequired)
        );
        let mut unapproved = request(
            CapabilityId::ClipboardReadV1,
            CLIPBOARD_READ_OPERATION,
            ClipboardReadRequest {
                mime_type: "image/png".into(),
            }
            .encode()
            .unwrap(),
            "/run/user/1000/wayland-1",
            2,
            ActivationOrigin::Physical,
            scope(1024, 4),
        );
        assert_eq!(
            Backend::authorize(&backend, &unapproved, &mut activations, 99),
            Err(BrokerErrorCode::OutOfScope)
        );
        unapproved.payload = ClipboardReadRequest {
            mime_type: "text/plain;charset=utf-8".into(),
        }
        .encode()
        .unwrap();
        assert_eq!(
            Backend::authorize(&backend, &unapproved, &mut activations, 99),
            Ok(())
        );
        assert_eq!(
            Backend::authorize(&backend, &unapproved, &mut activations, 99),
            Err(BrokerErrorCode::ActivationRequired)
        );
        assert!(fake.reads.lock().unwrap().is_empty());
        assert!(fake.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn read_and_write_overages_return_no_partial_content_or_side_effect() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let fake = Arc::new(FakeClipboard::default());
        fake.results.lock().unwrap().push_back(Ok(vec![7; 9]));
        let backend = ClipboardBackend::new(fake.clone(), directory.path(), "limit-test").unwrap();
        let read = request(
            CapabilityId::ClipboardReadV1,
            CLIPBOARD_READ_OPERATION,
            ClipboardReadRequest {
                mime_type: "text/plain;charset=utf-8".into(),
            }
            .encode()
            .unwrap(),
            "/run/user/1000/wayland-1",
            1,
            ActivationOrigin::Physical,
            scope(8, 4),
        );
        assert_eq!(
            Backend::execute(&backend, &read, &cancellation()),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );

        let write = request(
            CapabilityId::ClipboardWriteV1,
            CLIPBOARD_WRITE_OPERATION,
            ClipboardWriteRequest {
                mime_type: "text/plain;charset=utf-8".into(),
                bytes: vec![8; 9],
            }
            .encode()
            .unwrap(),
            "/run/user/1000/wayland-1",
            2,
            ActivationOrigin::Physical,
            scope(8, 4),
        );
        assert_eq!(
            Backend::execute(&backend, &write, &cancellation()),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );
        assert!(fake.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn operation_quota_survives_restart_and_is_source_isolated() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let fake = Arc::new(FakeClipboard::default());
        let request = request(
            CapabilityId::ClipboardReadV1,
            CLIPBOARD_READ_OPERATION,
            ClipboardReadRequest {
                mime_type: "text/plain;charset=utf-8".into(),
            }
            .encode()
            .unwrap(),
            "/run/user/1000/wayland-1",
            1,
            ActivationOrigin::Physical,
            scope(1024, 1),
        );
        let backend =
            ClipboardBackend::new(fake.clone(), directory.path(), "quota-source").unwrap();
        assert_eq!(
            Backend::authorize(&backend, &request, &mut ActivationLedger::new(8), 99),
            Ok(())
        );
        drop(backend);
        let restarted =
            ClipboardBackend::new(fake.clone(), directory.path(), "quota-source").unwrap();
        let mut second = request.clone();
        second.activation.as_mut().unwrap().input_sequence = 2;
        assert_eq!(
            Backend::authorize(&restarted, &second, &mut ActivationLedger::new(8), 99),
            Err(BrokerErrorCode::RateLimited)
        );
        let isolated = ClipboardBackend::new(fake, directory.path(), "other-source").unwrap();
        assert_eq!(
            Backend::authorize(&isolated, &second, &mut ActivationLedger::new(8), 99),
            Ok(())
        );
    }

    struct MockWaylandState {
        mime_type: String,
        clipboard: Vec<u8>,
        source_mimes: HashMap<wayland_server::backend::ObjectId, String>,
        received: Arc<Mutex<Vec<u8>>>,
    }

    impl GlobalDispatch<server_seat::WlSeat, ()> for MockWaylandState {
        fn bind(
            _state: &mut Self,
            _handle: &DisplayHandle,
            _client: &Client,
            resource: New<server_seat::WlSeat>,
            _global_data: &(),
            data_init: &mut DataInit<'_, Self>,
        ) {
            let seat = data_init.init(resource, ());
            seat.name("mock-seat".into());
            seat.capabilities(server_seat::Capability::empty());
        }
    }

    impl ServerDispatch<server_seat::WlSeat, ()> for MockWaylandState {
        fn request(
            _state: &mut Self,
            _client: &Client,
            _resource: &server_seat::WlSeat,
            _request: server_seat::Request,
            _data: &(),
            _handle: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerManager, ()> for MockWaylandState {
        fn bind(
            _state: &mut Self,
            _handle: &DisplayHandle,
            _client: &Client,
            resource: New<ServerManager>,
            _global_data: &(),
            data_init: &mut DataInit<'_, Self>,
        ) {
            data_init.init(resource, ());
        }
    }

    impl ServerDispatch<ServerManager, ()> for MockWaylandState {
        fn request(
            state: &mut Self,
            client: &Client,
            _resource: &ServerManager,
            request: server_manager::Request,
            _data: &(),
            handle: &DisplayHandle,
            data_init: &mut DataInit<'_, Self>,
        ) {
            match request {
                server_manager::Request::CreateDataSource { id } => {
                    data_init.init(id, ());
                }
                server_manager::Request::GetDataDevice { id, .. } => {
                    let device = data_init.init(id, ());
                    let offer = client
                        .create_resource::<ServerOffer, (), Self>(handle, 1, ())
                        .unwrap();
                    device.data_offer(&offer);
                    offer.offer(state.mime_type.clone());
                    device.selection(Some(&offer));
                    device.primary_selection(None);
                }
                server_manager::Request::Destroy => {}
                _ => {}
            }
        }
    }

    impl ServerDispatch<ServerOffer, ()> for MockWaylandState {
        fn request(
            state: &mut Self,
            _client: &Client,
            _resource: &ServerOffer,
            request: server_offer::Request,
            _data: &(),
            _handle: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            if let server_offer::Request::Receive { mime_type, fd } = request
                && mime_type == state.mime_type
            {
                let mut file = File::from(fd);
                file.write_all(&state.clipboard).unwrap();
            }
        }
    }

    impl ServerDispatch<ServerSource, ()> for MockWaylandState {
        fn request(
            state: &mut Self,
            _client: &Client,
            resource: &ServerSource,
            request: server_source::Request,
            _data: &(),
            _handle: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            match request {
                server_source::Request::Offer { mime_type } => {
                    state.source_mimes.insert(resource.id(), mime_type);
                }
                server_source::Request::Destroy => {
                    state.source_mimes.remove(&resource.id());
                }
                _ => {}
            }
        }
    }

    impl ServerDispatch<ServerDevice, ()> for MockWaylandState {
        fn request(
            state: &mut Self,
            _client: &Client,
            _resource: &ServerDevice,
            request: server_device::Request,
            _data: &(),
            _handle: &DisplayHandle,
            _data_init: &mut DataInit<'_, Self>,
        ) {
            if let server_device::Request::SetSelection {
                source: Some(source),
            } = request
            {
                let mime_type = state.source_mimes.get(&source.id()).unwrap().clone();
                let (reader, writer) = nonblocking_pipe().unwrap();
                source.send(mime_type, writer.as_fd());
                let received = Arc::clone(&state.received);
                thread::spawn(move || {
                    drop(writer);
                    let mut reader = reader;
                    let deadline = Instant::now() + Duration::from_secs(2);
                    let mut bytes = Vec::new();
                    loop {
                        let mut chunk = [0_u8; 256];
                        match reader.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                if Instant::now() >= deadline {
                                    break;
                                }
                                thread::yield_now();
                            }
                            Err(error) => panic!("mock clipboard pipe failed: {error}"),
                        }
                    }
                    *received.lock().unwrap() = bytes;
                });
            }
        }
    }

    struct MockWaylandServer {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        received: Arc<Mutex<Vec<u8>>>,
        thread: JoinHandle<()>,
    }

    fn mock_wayland_server(directory: &Path, clipboard: &[u8]) -> MockWaylandServer {
        let socket = directory.join("desktop-wayland");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let received = Arc::new(Mutex::new(Vec::new()));
        let thread_received = Arc::clone(&received);
        let initial = clipboard.to_vec();
        let thread = thread::spawn(move || {
            let mut display = Display::<MockWaylandState>::new().unwrap();
            let mut handle = display.handle();
            handle.create_global::<MockWaylandState, server_seat::WlSeat, _>(1, ());
            handle.create_global::<MockWaylandState, ServerManager, _>(1, ());
            let mut state = MockWaylandState {
                mime_type: "text/plain;charset=utf-8".into(),
                clipboard: initial,
                source_mimes: HashMap::new(),
                received: thread_received,
            };
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle.insert_client(stream, Arc::new(())).unwrap();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("mock Wayland accept failed: {error}"),
                }
                display.dispatch_clients(&mut state).unwrap();
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        MockWaylandServer {
            socket,
            stop,
            received,
            thread,
        }
    }

    #[test]
    fn production_wayland_transport_reads_and_owns_bounded_regular_selection() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let server = mock_wayland_server(directory.path(), b"from-desktop");
        let clipboard = WaylandClipboard::new();
        let binding = ClipboardBinding::WaylandDataControl {
            socket: server.socket.clone(),
        };
        let material = clipboard
            .read(&binding, "text/plain;charset=utf-8", 64, &cancellation())
            .unwrap();
        assert_eq!(material.bytes.as_slice(), b"from-desktop");
        clipboard
            .write(
                &binding,
                "text/plain;charset=utf-8",
                b"from-plugin",
                &cancellation(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while server.received.lock().unwrap().as_slice() != b"from-plugin"
            && Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert_eq!(server.received.lock().unwrap().as_slice(), b"from-plugin");
        drop(clipboard);
        server.stop.store(true, Ordering::Release);
        server.thread.join().unwrap();
    }

    #[test]
    fn cancelled_owner_startup_joins_promptly_even_if_compositor_never_replies() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("silent-wayland");
        let _silent_listener = UnixListener::bind(&socket).unwrap();
        let clipboard = WaylandClipboard::new();
        let token =
            CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_millis(75))
                .unwrap();
        let started = Instant::now();

        assert_eq!(
            clipboard.write(
                &ClipboardBinding::WaylandDataControl { socket },
                "text/plain;charset=utf-8",
                b"secret",
                &token,
            ),
            Err(BrokerErrorCode::Timeout)
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation waited for a second Wayland startup timeout"
        );
    }
}
