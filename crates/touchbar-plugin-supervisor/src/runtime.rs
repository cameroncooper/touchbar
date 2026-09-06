use std::{os::fd::RawFd, sync::Arc, time::Duration};

use touchbar_policy::CapabilityId;
use touchbar_protocol::broker_ipc::BrokerErrorCode;

use crate::{
    ActivationLedger, Backend, BackendExecutor, BackendRequest, ExecutorLimits, HostEvent,
    HostEventQueue, LifecycleLimits, LifecycleState, ResourceBackend, ResourceLimits,
    ResourceManager, RevocationEffect,
};

/// Backend-neutral composition of worker execution, connection quotas, and
/// bounded completion delivery.
pub struct AsyncBrokerRuntime {
    executor: BackendExecutor,
    lifecycle: LifecycleState,
    events: HostEventQueue,
    resources: ResourceManager,
}

impl AsyncBrokerRuntime {
    pub fn new(
        executor_limits: ExecutorLimits,
        lifecycle_limits: LifecycleLimits,
        maximum_state_events: usize,
        maximum_edge_events: usize,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            executor: BackendExecutor::new(executor_limits)?,
            lifecycle: LifecycleState::new(lifecycle_limits),
            events: HostEventQueue::new(maximum_state_events, maximum_edge_events),
            resources: ResourceManager::new(maximum_edge_events)?,
        })
    }

    pub fn register_resource(
        &mut self,
        capability: CapabilityId,
        backend: Arc<dyn ResourceBackend>,
    ) -> Option<Arc<dyn ResourceBackend>> {
        self.resources.register(capability, backend)
    }

    pub fn register(
        &mut self,
        capability: CapabilityId,
        backend: Arc<dyn Backend>,
    ) -> Option<Arc<dyn Backend>> {
        self.executor.register(capability, backend)
    }

    pub fn submit(
        &mut self,
        request: BackendRequest,
        timeout: Duration,
        deadline_monotonic_micros: u64,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        let request_id = request.request_id;
        self.lifecycle.begin(
            request_id,
            request.capability.clone(),
            deadline_monotonic_micros,
        )?;
        if let Err(error) =
            self.executor
                .submit(request, timeout, activations, now_monotonic_micros)
        {
            self.lifecycle.finish(request_id);
            return Err(error);
        }
        Ok(())
    }

    pub fn cancel(&mut self, request_id: u64) -> bool {
        self.executor.cancel(request_id)
    }

    pub fn open_resource(
        &mut self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(u64, Vec<u8>), BrokerErrorCode> {
        let limits = self
            .resources
            .limits(request)
            .ok_or(BrokerErrorCode::Unavailable)?;
        let resource_id = self
            .lifecycle
            .allocate_resource(request.capability.clone(), limits.reserved_buffered_bytes)?;
        let response_payload = match self.resources.open(
            resource_id,
            request,
            limits,
            activations,
            now_monotonic_micros,
        ) {
            Ok(payload) => payload,
            Err(error) => {
                self.lifecycle.close_resource(resource_id);
                return Err(error);
            }
        };
        Ok((resource_id, response_payload))
    }

    pub fn resource_limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        self.resources.limits(request)
    }

    pub fn close_resource(&mut self, resource_id: u64) -> bool {
        let managed = self.resources.close(resource_id);
        let tracked = self.lifecycle.close_resource(resource_id);
        debug_assert_eq!(managed, tracked, "resource manager and lifecycle diverged");
        managed && tracked
    }

    pub fn revoke(&mut self, capability: &CapabilityId, generation: u64) -> RevocationEffect {
        self.executor.revoke(capability);
        let effect = self.lifecycle.revoke(capability);
        self.resources
            .revoke(&effect.closed_resources, &mut self.events, generation);
        effect
    }

    pub fn revoke_all(&mut self) -> RevocationEffect {
        self.executor.cancel_all();
        let effect = self.lifecycle.revoke_all();
        self.resources.close_many(&effect.closed_resources);
        effect
    }

    pub fn pump_completions(&mut self, generation: u64) -> Result<usize, BrokerErrorCode> {
        self.executor.drain_completion_signal()?;
        let mut count = 0;
        while let Some(completion) = self.executor.try_completion()? {
            self.lifecycle.finish(completion.request_id);
            self.events
                .push_completion(completion.request_id, completion.result, generation);
            count += 1;
        }
        Ok(count)
    }

    pub fn pump_resource_events(&mut self, generation: u64) -> usize {
        let finished = self.resources.pump(&mut self.events, generation);
        for resource_id in &finished {
            self.lifecycle.close_resource(*resource_id);
        }
        finished.len()
    }

    pub fn pop_event(&mut self) -> Option<HostEvent> {
        self.events.pop()
    }

    pub fn lifecycle(&self) -> &LifecycleState {
        &self.lifecycle
    }

    pub fn lifecycle_mut(&mut self) -> &mut LifecycleState {
        &mut self.lifecycle
    }

    pub fn events_mut(&mut self) -> &mut HostEventQueue {
        &mut self.events
    }

    pub fn completion_fd(&self) -> RawFd {
        self.executor.completion_fd()
    }

    pub fn resource_event_fd(&self) -> RawFd {
        self.resources.event_fd()
    }

    pub fn active_count(&self) -> usize {
        self.executor.active_count()
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Instant};

    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{PackageInstance, Provenance, RuntimeKind};
    use touchbar_protocol::broker_ipc::BrokerResult;

    use crate::{
        CancellationToken, ConnectionIdentity, OpenedResource, ResourceEventSink, ResourceHandle,
    };

    use super::*;

    struct Echo;

    impl Backend for Echo {
        fn execute(
            &self,
            request: &BackendRequest,
            _cancellation: &CancellationToken,
        ) -> BrokerResult {
            BrokerResult::Success {
                payload: request.payload.clone(),
            }
        }
    }

    struct TestResource;

    impl ResourceHandle for TestResource {
        fn close(&mut self) {}
    }

    struct FixedResourceBackend;

    impl ResourceBackend for FixedResourceBackend {
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
            _events: ResourceEventSink,
        ) -> Result<OpenedResource, BrokerErrorCode> {
            Ok(OpenedResource {
                handle: Box::new(TestResource),
                response_payload: Vec::new(),
            })
        }
    }

    fn request(request_id: u64) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 8,
                package: PackageInstance {
                    source: GithubSource::new("alice", "media").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id,
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
            bindings: Default::default(),
            activation: None,
            operation: "get".into(),
            payload: b"response".to_vec(),
        }
    }

    #[test]
    fn completion_releases_lifecycle_and_enters_bounded_event_queue() {
        let mut runtime = AsyncBrokerRuntime::new(
            ExecutorLimits::default(),
            LifecycleLimits::default(),
            64,
            256,
        )
        .unwrap();
        runtime.register(CapabilityId::HttpRequestV1, Arc::new(Echo));
        let mut activations = ActivationLedger::new(8);
        runtime
            .submit(
                request(1),
                Duration::from_secs(1),
                1_000_000,
                &mut activations,
                0,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while runtime.pump_completions(1).unwrap() == 0 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(runtime.active_count(), 0);
        assert_eq!(runtime.lifecycle().pending_count(), 0);
        assert_eq!(
            runtime.pop_event(),
            Some(HostEvent::Completion {
                request_id: 1,
                result: BrokerResult::Success {
                    payload: b"response".to_vec(),
                },
            })
        );
    }

    #[test]
    fn resource_pressure_is_atomic_and_capacity_is_reusable_after_close() {
        let mut runtime = AsyncBrokerRuntime::new(
            ExecutorLimits::default(),
            LifecycleLimits {
                maximum_pending_operations: 1,
                maximum_resources: 2,
                maximum_buffered_bytes: 2048,
            },
            2,
            2,
        )
        .unwrap();
        runtime.register_resource(CapabilityId::HttpRequestV1, Arc::new(FixedResourceBackend));
        let mut activations = ActivationLedger::new(1);
        let (first, _) = runtime
            .open_resource(&request(1), &mut activations, 0)
            .unwrap();
        let (second, _) = runtime
            .open_resource(&request(2), &mut activations, 0)
            .unwrap();
        assert_eq!(runtime.lifecycle().resource_count(), 2);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 2048);
        assert_eq!(
            runtime.open_resource(&request(3), &mut activations, 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        assert_eq!(runtime.lifecycle().resource_count(), 2);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 2048);

        assert!(runtime.close_resource(first));
        let (third, _) = runtime
            .open_resource(&request(4), &mut activations, 0)
            .unwrap();
        assert_eq!((first, second, third), (1, 2, 3));
        assert_eq!(runtime.lifecycle().resource_count(), 2);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 2048);
    }
}
