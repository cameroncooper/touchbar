use std::{
    collections::BTreeMap,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use touchbar_policy::{CapabilityId, CapabilityScope, GrantBindings};
use touchbar_protocol::broker_ipc::{ActivationContext, BrokerErrorCode, BrokerResult};

use crate::{ActivationLedger, ConnectionIdentity};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutorLimits {
    pub workers: usize,
    pub queued_operations: usize,
    pub maximum_active_operations: usize,
}

impl Default for ExecutorLimits {
    fn default() -> Self {
        Self {
            workers: 2,
            queued_operations: 32,
            maximum_active_operations: 32,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendRequest {
    pub identity: ConnectionIdentity,
    pub request_id: u64,
    pub capability: CapabilityId,
    /// The normalized manifest scope accepted by policy. Backends must validate
    /// each decoded operation against this scope before touching the OS.
    pub authorized_scope: CapabilityScope,
    /// Supervisor-owned host authority. This comes from the grant store, never
    /// from the package manifest or request payload.
    pub bindings: GrantBindings,
    /// Host-attached activation for operation-specific validation. Backends
    /// must never infer physical activation from phase alone.
    pub activation: Option<ActivationContext>,
    pub operation: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendCompletion {
    pub request_id: u64,
    pub capability: CapabilityId,
    pub result: BrokerResult,
}

pub trait Backend: Send + Sync + 'static {
    /// Decode and authorize one operation before it can enter the worker queue.
    /// Capability backends consume trusted activation here when their matched
    /// operation rule requires it.
    fn authorize(
        &self,
        _request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        Ok(())
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult;
}

#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl CancellationToken {
    /// Creates a cancellation token backed by a shared flag and hard deadline.
    /// Transport integration harnesses use this same constructor as the
    /// production executor, avoiding a separate test-only cancellation path.
    pub fn new(cancelled: Arc<AtomicBool>, timeout: Duration) -> Result<Self, BrokerErrorCode> {
        Ok(Self {
            cancelled,
            deadline: Instant::now()
                .checked_add(timeout)
                .ok_or(BrokerErrorCode::InvalidRequest)?,
        })
    }

    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }

    pub fn reason(&self) -> Option<BrokerErrorCode> {
        if self.cancelled.load(Ordering::Acquire) {
            Some(BrokerErrorCode::Cancelled)
        } else if Instant::now() >= self.deadline {
            Some(BrokerErrorCode::Timeout)
        } else {
            None
        }
    }
}

struct ActiveOperation {
    capability: CapabilityId,
    cancelled: Arc<AtomicBool>,
}

struct Job {
    request: BackendRequest,
    backend: Arc<dyn Backend>,
    cancellation: CancellationToken,
}

pub struct BackendExecutor {
    backends: BTreeMap<CapabilityId, Arc<dyn Backend>>,
    jobs: Option<SyncSender<Job>>,
    completions: Receiver<BackendCompletion>,
    completion_signal: OwnedFd,
    active: BTreeMap<u64, ActiveOperation>,
    maximum_active_operations: usize,
    workers: Vec<JoinHandle<()>>,
}

impl BackendExecutor {
    pub fn new(limits: ExecutorLimits) -> Result<Self, &'static str> {
        if limits.workers == 0
            || limits.queued_operations == 0
            || limits.maximum_active_operations == 0
        {
            return Err("executor limits must be nonzero");
        }
        let (job_sender, job_receiver) = mpsc::sync_channel::<Job>(limits.queued_operations);
        // The completion channel is memory-bounded by maximum_active_operations:
        // no more jobs can exist than entries in `active`.
        let (completion_sender, completion_receiver) = mpsc::channel::<BackendCompletion>();
        // SAFETY: eventfd has no pointer arguments and returns a uniquely owned
        // descriptor on success.
        let completion_signal = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if completion_signal < 0 {
            return Err("could not create broker completion signal");
        }
        // SAFETY: successful eventfd returned a uniquely owned descriptor.
        let completion_signal = unsafe { OwnedFd::from_raw_fd(completion_signal) };
        let completion_signal_fd = completion_signal.as_raw_fd();
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        let mut workers = Vec::with_capacity(limits.workers);
        for index in 0..limits.workers {
            let jobs = Arc::clone(&job_receiver);
            let completions = completion_sender.clone();
            workers.push(
                thread::Builder::new()
                    .name(format!("touchbar-broker-{index}"))
                    .spawn(move || worker(jobs, completions, completion_signal_fd))
                    .map_err(|_| "could not spawn broker worker")?,
            );
        }
        drop(completion_sender);
        Ok(Self {
            backends: BTreeMap::new(),
            jobs: Some(job_sender),
            completions: completion_receiver,
            completion_signal,
            active: BTreeMap::new(),
            maximum_active_operations: limits.maximum_active_operations,
            workers,
        })
    }

    pub fn register(
        &mut self,
        capability: CapabilityId,
        backend: Arc<dyn Backend>,
    ) -> Option<Arc<dyn Backend>> {
        self.backends.insert(capability, backend)
    }

    pub fn submit(
        &mut self,
        request: BackendRequest,
        timeout: Duration,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        if request.request_id == 0 || self.active.contains_key(&request.request_id) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        if self.active.len() >= self.maximum_active_operations {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        let backend = self
            .backends
            .get(&request.capability)
            .cloned()
            .ok_or(BrokerErrorCode::Unavailable)?;
        backend.authorize(&request, activations, now_monotonic_micros)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new(Arc::clone(&cancelled), timeout)?;
        let request_id = request.request_id;
        let capability = request.capability.clone();
        let job = Job {
            request,
            backend,
            cancellation,
        };
        match self
            .jobs
            .as_ref()
            .expect("executor sender exists until drop")
            .try_send(job)
        {
            Ok(()) => {
                self.active.insert(
                    request_id,
                    ActiveOperation {
                        capability,
                        cancelled,
                    },
                );
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(BrokerErrorCode::QuotaExceeded),
            Err(TrySendError::Disconnected(_)) => Err(BrokerErrorCode::BackendFailed),
        }
    }

    pub fn cancel(&self, request_id: u64) -> bool {
        let Some(operation) = self.active.get(&request_id) else {
            return false;
        };
        operation.cancelled.store(true, Ordering::Release);
        true
    }

    pub fn revoke(&self, capability: &CapabilityId) -> Vec<u64> {
        self.active
            .iter()
            .filter(|(_, operation)| &operation.capability == capability)
            .map(|(request_id, operation)| {
                operation.cancelled.store(true, Ordering::Release);
                *request_id
            })
            .collect()
    }

    pub fn cancel_all(&self) -> Vec<u64> {
        self.active
            .iter()
            .map(|(request_id, operation)| {
                operation.cancelled.store(true, Ordering::Release);
                *request_id
            })
            .collect()
    }

    pub fn try_completion(&mut self) -> Result<Option<BackendCompletion>, BrokerErrorCode> {
        match self.completions.try_recv() {
            Ok(completion) => {
                self.active.remove(&completion.request_id);
                Ok(Some(completion))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(BrokerErrorCode::BackendFailed),
        }
    }

    pub fn completion_fd(&self) -> RawFd {
        self.completion_signal.as_raw_fd()
    }

    pub fn drain_completion_signal(&self) -> Result<(), BrokerErrorCode> {
        let mut count = 0_u64;
        loop {
            // SAFETY: count is writable for eight bytes and completion_signal
            // is a live nonblocking eventfd descriptor.
            let read = unsafe {
                libc::read(
                    self.completion_signal.as_raw_fd(),
                    (&mut count as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read == std::mem::size_of::<u64>() as isize {
                continue;
            }
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
            }
            return Err(BrokerErrorCode::BackendFailed);
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

impl Drop for BackendExecutor {
    fn drop(&mut self) {
        for operation in self.active.values() {
            operation.cancelled.store(true, Ordering::Release);
        }
        self.jobs.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker(
    jobs: Arc<Mutex<Receiver<Job>>>,
    completions: Sender<BackendCompletion>,
    completion_signal: RawFd,
) {
    loop {
        let job = {
            let receiver = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            receiver.recv()
        };
        let Ok(job) = job else {
            break;
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            job.backend.execute(&job.request, &job.cancellation)
        }))
        .unwrap_or(BrokerResult::Error(BrokerErrorCode::BackendFailed));
        let result = job
            .cancellation
            .reason()
            .map(BrokerResult::Error)
            .unwrap_or(result);
        if completions
            .send(BackendCompletion {
                request_id: job.request.request_id,
                capability: job.request.capability,
                result,
            })
            .is_err()
        {
            break;
        }
        let one = 1_u64;
        // SAFETY: one is readable for eight bytes and the supervisor owns the
        // eventfd for at least as long as its joined workers.
        let _ = unsafe {
            libc::write(
                completion_signal,
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Condvar, mpsc};

    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{PackageInstance, Provenance, RuntimeKind};

    use super::*;

    fn identity(instance_id: u64) -> ConnectionIdentity {
        ConnectionIdentity {
            instance_id,
            package: PackageInstance {
                source: GithubSource::new("alice", "media").unwrap(),
                version: Version::new(1, 0, 0),
                digest: format!("sha256:{}", "a".repeat(64)),
                provenance: Provenance::VerifiedRelease,
                runtime: RuntimeKind::Component,
            },
        }
    }

    fn request(id: u64, instance_id: u64) -> BackendRequest {
        BackendRequest {
            identity: identity(instance_id),
            request_id: id,
            capability: CapabilityId::HttpRequestV1,
            authorized_scope: touchbar_policy::CapabilityScope::HttpRequest(
                touchbar_policy::HttpRequestScope {
                    origins: std::collections::BTreeSet::new(),
                    methods: std::collections::BTreeSet::new(),
                    private_network: false,
                    maximum_request_bytes: 0,
                    maximum_response_bytes: 0,
                    maximum_requests_per_minute: 0,
                },
            ),
            bindings: GrantBindings::default(),
            activation: None,
            operation: "get".into(),
            payload: Vec::new(),
        }
    }

    fn wait_completion(executor: &mut BackendExecutor) -> BackendCompletion {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(completion) = executor.try_completion().unwrap() {
                return completion;
            }
            assert!(
                Instant::now() < deadline,
                "backend completion did not arrive"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn submit(
        executor: &mut BackendExecutor,
        request: BackendRequest,
        timeout: Duration,
    ) -> Result<(), BrokerErrorCode> {
        let mut activations = ActivationLedger::new(8);
        executor.submit(request, timeout, &mut activations, 0)
    }

    struct IdentityBackend;

    impl Backend for IdentityBackend {
        fn execute(
            &self,
            request: &BackendRequest,
            _cancellation: &CancellationToken,
        ) -> BrokerResult {
            BrokerResult::Success {
                payload: request.identity.instance_id.to_le_bytes().to_vec(),
            }
        }
    }

    struct DenyingBackend;

    impl Backend for DenyingBackend {
        fn authorize(
            &self,
            _request: &BackendRequest,
            _activations: &mut ActivationLedger,
            _now_monotonic_micros: u64,
        ) -> Result<(), BrokerErrorCode> {
            Err(BrokerErrorCode::OutOfScope)
        }

        fn execute(
            &self,
            _request: &BackendRequest,
            _cancellation: &CancellationToken,
        ) -> BrokerResult {
            panic!("denied work must never enter the worker queue")
        }
    }

    #[test]
    fn submission_cannot_bypass_backend_authorization() {
        let mut executor = BackendExecutor::new(ExecutorLimits::default()).unwrap();
        executor.register(CapabilityId::HttpRequestV1, Arc::new(DenyingBackend));
        let mut activations = ActivationLedger::new(8);
        assert_eq!(
            executor.submit(request(1, 77), Duration::from_secs(1), &mut activations, 0,),
            Err(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(executor.active_count(), 0);
    }

    #[test]
    fn jobs_carry_the_supervisor_bound_identity() {
        let mut executor = BackendExecutor::new(ExecutorLimits::default()).unwrap();
        executor.register(CapabilityId::HttpRequestV1, Arc::new(IdentityBackend));
        submit(&mut executor, request(1, 77), Duration::from_secs(1)).unwrap();
        assert_eq!(
            wait_completion(&mut executor).result,
            BrokerResult::Success {
                payload: 77_u64.to_le_bytes().to_vec()
            }
        );
    }

    struct GateBackend {
        entered: mpsc::Sender<()>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Backend for GateBackend {
        fn execute(
            &self,
            _request: &BackendRequest,
            cancellation: &CancellationToken,
        ) -> BrokerResult {
            self.entered.send(()).unwrap();
            let (lock, wake) = &*self.gate;
            let mut open = lock.lock().unwrap();
            while !*open && !cancellation.is_cancelled() {
                let waited = wake.wait_timeout(open, Duration::from_millis(2)).unwrap();
                open = waited.0;
            }
            BrokerResult::Success {
                payload: Vec::new(),
            }
        }
    }

    #[test]
    fn cancellation_and_deadline_override_backend_success() {
        let (entered_sender, entered) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut executor = BackendExecutor::new(ExecutorLimits {
            workers: 1,
            queued_operations: 2,
            maximum_active_operations: 2,
        })
        .unwrap();
        executor.register(
            CapabilityId::HttpRequestV1,
            Arc::new(GateBackend {
                entered: entered_sender,
                gate: Arc::clone(&gate),
            }),
        );
        submit(&mut executor, request(1, 1), Duration::from_secs(1)).unwrap();
        entered.recv().unwrap();
        assert!(executor.cancel(1));
        gate.1.notify_all();
        assert_eq!(
            wait_completion(&mut executor).result,
            BrokerResult::Error(BrokerErrorCode::Cancelled)
        );

        submit(&mut executor, request(2, 1), Duration::from_millis(1)).unwrap();
        entered.recv().unwrap();
        assert_eq!(
            wait_completion(&mut executor).result,
            BrokerResult::Error(BrokerErrorCode::Timeout)
        );
    }

    struct PanicBackend;

    impl Backend for PanicBackend {
        fn execute(
            &self,
            _request: &BackendRequest,
            _cancellation: &CancellationToken,
        ) -> BrokerResult {
            panic!("hostile backend panic")
        }
    }

    #[test]
    fn backend_panics_become_non_sensitive_failures() {
        let mut executor = BackendExecutor::new(ExecutorLimits::default()).unwrap();
        executor.register(CapabilityId::HttpRequestV1, Arc::new(PanicBackend));
        submit(&mut executor, request(1, 1), Duration::from_secs(1)).unwrap();
        assert_eq!(
            wait_completion(&mut executor).result,
            BrokerResult::Error(BrokerErrorCode::BackendFailed)
        );
    }

    #[test]
    fn queue_pressure_fails_before_unbounded_work_accumulates() {
        let (entered_sender, entered) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut executor = BackendExecutor::new(ExecutorLimits {
            workers: 1,
            queued_operations: 1,
            maximum_active_operations: 3,
        })
        .unwrap();
        executor.register(
            CapabilityId::HttpRequestV1,
            Arc::new(GateBackend {
                entered: entered_sender,
                gate: Arc::clone(&gate),
            }),
        );
        submit(&mut executor, request(1, 1), Duration::from_secs(1)).unwrap();
        entered.recv().unwrap();
        submit(&mut executor, request(2, 1), Duration::from_secs(1)).unwrap();
        assert_eq!(
            submit(&mut executor, request(3, 1), Duration::from_secs(1)),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    #[test]
    fn capability_revocation_cancels_every_matching_job() {
        let (entered_sender, entered) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut executor = BackendExecutor::new(ExecutorLimits::default()).unwrap();
        executor.register(
            CapabilityId::HttpRequestV1,
            Arc::new(GateBackend {
                entered: entered_sender,
                gate: Arc::clone(&gate),
            }),
        );
        submit(&mut executor, request(1, 1), Duration::from_secs(1)).unwrap();
        entered.recv().unwrap();
        assert_eq!(executor.revoke(&CapabilityId::HttpRequestV1), vec![1]);
        gate.1.notify_all();
        assert_eq!(
            wait_completion(&mut executor).result,
            BrokerResult::Error(BrokerErrorCode::Cancelled)
        );
    }
}
