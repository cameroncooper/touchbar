use std::{
    collections::BTreeMap,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use touchbar_policy::CapabilityId;
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult, MAX_INLINE_PAYLOAD_BYTES};

use crate::{ActivationLedger, BackendRequest, HostEventQueue};

const MAX_RESOURCE_EVENT_BYTES: usize = 16 * 1024;

pub trait ResourceHandle: Send + 'static {
    fn close(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    pub reserved_buffered_bytes: usize,
    pub maximum_events_per_second: u16,
}

pub struct OpenedResource {
    pub handle: Box<dyn ResourceHandle>,
    pub response_payload: Vec<u8>,
}

pub trait ResourceBackend: Send + Sync + 'static {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits>;

    fn authorize(
        &self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode>;

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode>;
}

struct IncomingEvent {
    resource_id: u64,
    result: BrokerResult,
}

struct RateWindow {
    started: Instant,
    emitted: u16,
}

#[derive(Clone)]
pub struct ResourceEventSink {
    resource_id: u64,
    sender: SyncSender<IncomingEvent>,
    signal: RawFd,
    dropped: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
    terminal: Arc<Mutex<Option<BrokerResult>>>,
    maximum_events_per_second: u16,
    rate: Arc<Mutex<RateWindow>>,
}

impl ResourceEventSink {
    pub fn resource_id(&self) -> u64 {
        self.resource_id
    }

    pub fn emit(&self, payload: Vec<u8>) -> Result<(), BrokerErrorCode> {
        if self.is_finished() {
            return Err(BrokerErrorCode::Unavailable);
        }
        if payload.len() > MAX_RESOURCE_EVENT_BYTES || payload.len() > MAX_INLINE_PAYLOAD_BYTES {
            self.finish(BrokerErrorCode::QuotaExceeded);
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        if !self.take_rate_slot() {
            self.record_drop();
            return Err(BrokerErrorCode::RateLimited);
        }
        match self.try_send(BrokerResult::Success { payload }) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.finish(error);
                Err(error)
            }
        }
    }

    /// Emits lossless stream data with bounded-memory backpressure. Closing or
    /// revoking the resource flips `finished`, which releases a producer even
    /// if the shared queue remains full.
    pub fn emit_buffered(&self, payload: Vec<u8>) -> Result<(), BrokerErrorCode> {
        if payload.len() > MAX_RESOURCE_EVENT_BYTES || payload.len() > MAX_INLINE_PAYLOAD_BYTES {
            self.finish(BrokerErrorCode::QuotaExceeded);
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        if !self.take_rate_slot() {
            self.finish(BrokerErrorCode::RateLimited);
            return Err(BrokerErrorCode::RateLimited);
        }
        let mut incoming = IncomingEvent {
            resource_id: self.resource_id,
            result: BrokerResult::Success { payload },
        };
        loop {
            if self.is_finished() {
                return Err(BrokerErrorCode::Unavailable);
            }
            match self.sender.try_send(incoming) {
                Ok(()) => {
                    self.wake();
                    return Ok(());
                }
                Err(TrySendError::Full(returned)) => {
                    incoming = returned;
                    std::thread::park_timeout(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(BrokerErrorCode::Unavailable);
                }
            }
        }
    }

    pub fn complete(&self, payload: Vec<u8>) -> Result<(), BrokerErrorCode> {
        if payload.len() > MAX_RESOURCE_EVENT_BYTES || payload.len() > MAX_INLINE_PAYLOAD_BYTES {
            self.finish(BrokerErrorCode::QuotaExceeded);
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        self.set_terminal(BrokerResult::Success { payload })
    }

    pub fn finish(&self, error: BrokerErrorCode) {
        let _ = self.set_terminal(BrokerResult::Error(error));
    }

    fn set_terminal(&self, result: BrokerResult) -> Result<(), BrokerErrorCode> {
        let mut terminal = self
            .terminal
            .lock()
            .map_err(|_| BrokerErrorCode::Internal)?;
        if self.finished.load(Ordering::Acquire) {
            return Err(BrokerErrorCode::Unavailable);
        }
        *terminal = Some(result);
        self.finished.store(true, Ordering::Release);
        drop(terminal);
        self.wake();
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    fn take_rate_slot(&self) -> bool {
        let Ok(mut rate) = self.rate.lock() else {
            return false;
        };
        if rate.started.elapsed() >= Duration::from_secs(1) {
            rate.started = Instant::now();
            rate.emitted = 0;
        }
        if rate.emitted >= self.maximum_events_per_second {
            return false;
        }
        rate.emitted += 1;
        true
    }

    fn try_send(&self, result: BrokerResult) -> Result<(), BrokerErrorCode> {
        match self.sender.try_send(IncomingEvent {
            resource_id: self.resource_id,
            result,
        }) {
            Ok(()) => {
                self.wake();
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.record_drop();
                Err(BrokerErrorCode::QuotaExceeded)
            }
            Err(TrySendError::Disconnected(_)) => Err(BrokerErrorCode::Unavailable),
        }
    }

    fn record_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        self.wake();
    }

    fn wake(&self) {
        let one = 1_u64;
        // SAFETY: one is readable for eight bytes and ResourceManager owns the
        // eventfd for at least as long as every sink clone.
        let _ = unsafe {
            libc::write(
                self.signal,
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }
}

struct ManagedResource {
    handle: Box<dyn ResourceHandle>,
    finished: Arc<AtomicBool>,
    terminal: Arc<Mutex<Option<BrokerResult>>>,
    next_sequence: u64,
}

pub struct ResourceManager {
    backends: BTreeMap<CapabilityId, Arc<dyn ResourceBackend>>,
    sender: SyncSender<IncomingEvent>,
    receiver: Receiver<IncomingEvent>,
    signal: OwnedFd,
    dropped: Arc<AtomicU64>,
    resources: BTreeMap<u64, ManagedResource>,
}

impl ResourceManager {
    pub fn new(maximum_queued_events: usize) -> Result<Self, &'static str> {
        if maximum_queued_events == 0 {
            return Err("resource event capacity must be nonzero");
        }
        let (sender, receiver) = mpsc::sync_channel(maximum_queued_events);
        // SAFETY: eventfd has no pointer arguments and returns a uniquely owned
        // descriptor on success.
        let signal = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if signal < 0 {
            return Err("could not create resource event signal");
        }
        // SAFETY: successful eventfd returned a uniquely owned descriptor.
        let signal = unsafe { OwnedFd::from_raw_fd(signal) };
        Ok(Self {
            backends: BTreeMap::new(),
            sender,
            receiver,
            signal,
            dropped: Arc::new(AtomicU64::new(0)),
            resources: BTreeMap::new(),
        })
    }

    pub fn register(
        &mut self,
        capability: CapabilityId,
        backend: Arc<dyn ResourceBackend>,
    ) -> Option<Arc<dyn ResourceBackend>> {
        self.backends.insert(capability, backend)
    }

    pub fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        self.backends
            .get(&request.capability)
            .and_then(|backend| backend.limits(request))
    }

    pub fn open(
        &mut self,
        resource_id: u64,
        request: &BackendRequest,
        limits: ResourceLimits,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<Vec<u8>, BrokerErrorCode> {
        if resource_id == 0
            || limits.maximum_events_per_second == 0
            || self.resources.contains_key(&resource_id)
        {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let backend = self
            .backends
            .get(&request.capability)
            .cloned()
            .ok_or(BrokerErrorCode::Unavailable)?;
        if backend.limits(request) != Some(limits) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        backend.authorize(request, activations, now_monotonic_micros)?;
        let finished = Arc::new(AtomicBool::new(false));
        let terminal = Arc::new(Mutex::new(None));
        let sink = ResourceEventSink {
            resource_id,
            sender: self.sender.clone(),
            signal: self.signal.as_raw_fd(),
            dropped: Arc::clone(&self.dropped),
            finished: Arc::clone(&finished),
            terminal: Arc::clone(&terminal),
            maximum_events_per_second: limits.maximum_events_per_second,
            rate: Arc::new(Mutex::new(RateWindow {
                started: Instant::now(),
                emitted: 0,
            })),
        };
        let mut opened = backend.open(resource_id, request, sink)?;
        if opened.response_payload.len() > MAX_INLINE_PAYLOAD_BYTES {
            opened.handle.close();
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        self.resources.insert(
            resource_id,
            ManagedResource {
                handle: opened.handle,
                finished,
                terminal,
                next_sequence: 1,
            },
        );
        Ok(opened.response_payload)
    }

    pub fn close(&mut self, resource_id: u64) -> bool {
        let Some(mut resource) = self.resources.remove(&resource_id) else {
            return false;
        };
        resource.finished.store(true, Ordering::Release);
        resource.handle.close();
        true
    }

    pub fn close_many(&mut self, resource_ids: &[u64]) {
        for resource_id in resource_ids {
            self.close(*resource_id);
        }
    }

    pub fn revoke(&mut self, resource_ids: &[u64], events: &mut HostEventQueue, generation: u64) {
        for resource_id in resource_ids {
            if let Some(resource) = self.resources.get(resource_id) {
                events.push_resource(
                    *resource_id,
                    resource.next_sequence,
                    BrokerResult::Error(BrokerErrorCode::Denied),
                    generation,
                );
            }
            self.close(*resource_id);
        }
    }

    pub fn pump(&mut self, events: &mut HostEventQueue, generation: u64) -> Vec<u64> {
        self.drain_signal();
        while let Ok(incoming) = self.receiver.try_recv() {
            let Some(resource) = self.resources.get_mut(&incoming.resource_id) else {
                continue;
            };
            let sequence = resource.next_sequence;
            resource.next_sequence = resource.next_sequence.saturating_add(1);
            events.push_resource(incoming.resource_id, sequence, incoming.result, generation);
        }
        let finished = self
            .resources
            .iter()
            .filter(|(_, resource)| resource.finished.load(Ordering::Acquire))
            .map(|(resource_id, _)| *resource_id)
            .collect::<Vec<_>>();
        for resource_id in &finished {
            let Some(resource) = self.resources.get_mut(resource_id) else {
                continue;
            };
            let terminal = resource
                .terminal
                .lock()
                .ok()
                .and_then(|mut terminal| terminal.take())
                .unwrap_or(BrokerResult::Error(BrokerErrorCode::Internal));
            let sequence = resource.next_sequence;
            resource.next_sequence = resource.next_sequence.saturating_add(1);
            events.push_resource(*resource_id, sequence, terminal, generation);
        }
        let dropped = self.dropped.swap(0, Ordering::AcqRel);
        if dropped > 0 {
            events.record_dropped(dropped, generation);
        }
        self.close_many(&finished);
        finished
    }

    pub fn event_fd(&self) -> RawFd {
        self.signal.as_raw_fd()
    }

    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    #[cfg(test)]
    pub(crate) fn resource_finished(&self, resource_id: u64) -> Option<bool> {
        self.resources
            .get(&resource_id)
            .map(|resource| resource.finished.load(Ordering::Acquire))
    }

    fn drain_signal(&self) {
        let mut value = 0_u64;
        loop {
            // SAFETY: value is writable for eight bytes and signal is a live,
            // nonblocking eventfd owned by this manager.
            let read = unsafe {
                libc::read(
                    self.signal.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read == std::mem::size_of::<u64>() as isize {
                continue;
            }
            if read < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                break;
            }
            break;
        }
    }
}

impl Drop for ResourceManager {
    fn drop(&mut self) {
        let resource_ids = self.resources.keys().copied().collect::<Vec<_>>();
        self.close_many(&resource_ids);
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        CapabilityScope, DbusSubscribeScope, PackageInstance, Provenance, RuntimeKind,
    };

    use super::*;
    use crate::ConnectionIdentity;

    struct Handle;

    impl ResourceHandle for Handle {
        fn close(&mut self) {}
    }

    struct FakeBackend;

    impl ResourceBackend for FakeBackend {
        fn limits(&self, _request: &BackendRequest) -> Option<ResourceLimits> {
            Some(ResourceLimits {
                reserved_buffered_bytes: 1024,
                maximum_events_per_second: 4,
            })
        }

        fn authorize(
            &self,
            _request: &BackendRequest,
            _activations: &mut ActivationLedger,
            _now_monotonic_micros: u64,
        ) -> Result<(), BrokerErrorCode> {
            Ok(())
        }

        fn open(
            &self,
            _resource_id: u64,
            _request: &BackendRequest,
            events: ResourceEventSink,
        ) -> Result<OpenedResource, BrokerErrorCode> {
            events.emit(vec![1, 2, 3])?;
            Ok(OpenedResource {
                handle: Box::new(Handle),
                response_payload: vec![9],
            })
        }
    }

    struct BurstBackend;

    impl ResourceBackend for BurstBackend {
        fn limits(&self, _request: &BackendRequest) -> Option<ResourceLimits> {
            Some(ResourceLimits {
                reserved_buffered_bytes: 1024,
                maximum_events_per_second: 1,
            })
        }

        fn authorize(
            &self,
            _request: &BackendRequest,
            _activations: &mut ActivationLedger,
            _now_monotonic_micros: u64,
        ) -> Result<(), BrokerErrorCode> {
            Ok(())
        }

        fn open(
            &self,
            _resource_id: u64,
            _request: &BackendRequest,
            events: ResourceEventSink,
        ) -> Result<OpenedResource, BrokerErrorCode> {
            events.emit(vec![1]).unwrap();
            assert_eq!(events.emit(vec![2]), Err(BrokerErrorCode::RateLimited));
            assert_eq!(events.emit(vec![3]), Err(BrokerErrorCode::RateLimited));
            Ok(OpenedResource {
                handle: Box::new(Handle),
                response_payload: Vec::new(),
            })
        }
    }

    struct OverflowBackend;

    impl ResourceBackend for OverflowBackend {
        fn limits(&self, _request: &BackendRequest) -> Option<ResourceLimits> {
            Some(ResourceLimits {
                reserved_buffered_bytes: 1024,
                maximum_events_per_second: 10,
            })
        }

        fn authorize(
            &self,
            _request: &BackendRequest,
            _activations: &mut ActivationLedger,
            _now_monotonic_micros: u64,
        ) -> Result<(), BrokerErrorCode> {
            Ok(())
        }

        fn open(
            &self,
            _resource_id: u64,
            _request: &BackendRequest,
            events: ResourceEventSink,
        ) -> Result<OpenedResource, BrokerErrorCode> {
            events.emit(vec![1])?;
            assert_eq!(events.emit(vec![2]), Err(BrokerErrorCode::QuotaExceeded));
            Ok(OpenedResource {
                handle: Box::new(Handle),
                response_payload: Vec::new(),
            })
        }
    }

    fn request() -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 1,
                package: PackageInstance {
                    source: GithubSource::new("alice", "media").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::DbusSubscribeV1,
            authorized_scope: CapabilityScope::DbusSubscribe(DbusSubscribeScope::default()),
            bindings: Default::default(),
            activation: None,
            operation: "subscribe".into(),
            payload: Vec::new(),
        }
    }

    #[test]
    fn open_emit_and_close_are_resource_scoped() {
        let mut manager = ResourceManager::new(4).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(FakeBackend));
        manager
            .open(
                9,
                &request(),
                FakeBackend.limits(&request()).unwrap(),
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();
        let mut events = HostEventQueue::new(2, 4);
        assert!(manager.pump(&mut events, 3).is_empty());
        assert!(matches!(
            events.pop(),
            Some(crate::HostEvent::ResourceEvent {
                resource_id: 9,
                sequence: 1,
                ..
            })
        ));
        assert!(manager.close(9));
        assert_eq!(manager.resource_count(), 0);
    }

    #[test]
    fn rate_limited_resource_events_report_explicit_overflow() {
        let mut manager = ResourceManager::new(4).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(BurstBackend));
        manager
            .open(
                9,
                &request(),
                BurstBackend.limits(&request()).unwrap(),
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();
        let mut events = HostEventQueue::new(2, 4);
        manager.pump(&mut events, 7);
        assert!(matches!(
            events.pop(),
            Some(crate::HostEvent::ResourceEvent { sequence: 1, .. })
        ));
        assert_eq!(
            events.pop(),
            Some(crate::HostEvent::Overflow {
                generation: 7,
                dropped_events: 2,
            })
        );
    }

    #[test]
    fn full_data_queue_still_delivers_an_ordered_terminal_error() {
        let mut manager = ResourceManager::new(1).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(OverflowBackend));
        let limits = OverflowBackend.limits(&request()).unwrap();
        manager
            .open(9, &request(), limits, &mut ActivationLedger::new(1), 0)
            .unwrap();
        let mut events = HostEventQueue::new(2, 4);
        assert_eq!(manager.pump(&mut events, 7), vec![9]);
        assert!(matches!(
            events.pop(),
            Some(crate::HostEvent::ResourceEvent {
                sequence: 1,
                result: BrokerResult::Success { .. },
                ..
            })
        ));
        assert!(matches!(
            events.pop(),
            Some(crate::HostEvent::ResourceEvent {
                sequence: 2,
                result: BrokerResult::Error(BrokerErrorCode::QuotaExceeded),
                ..
            })
        ));
        assert_eq!(manager.resource_count(), 0);
    }
}
