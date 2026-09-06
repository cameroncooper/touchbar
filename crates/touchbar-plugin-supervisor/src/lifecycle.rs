use std::collections::BTreeMap;

use touchbar_policy::CapabilityId;
use touchbar_protocol::broker_ipc::BrokerErrorCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleLimits {
    pub maximum_pending_operations: usize,
    pub maximum_resources: usize,
    pub maximum_buffered_bytes: usize,
}

impl Default for LifecycleLimits {
    fn default() -> Self {
        Self {
            maximum_pending_operations: 32,
            maximum_resources: 16,
            maximum_buffered_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperation {
    pub request_id: u64,
    pub capability: CapabilityId,
    pub deadline_monotonic_micros: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerResource {
    pub resource_id: u64,
    pub capability: CapabilityId,
    pub buffered_bytes: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevocationEffect {
    pub cancelled_requests: Vec<u64>,
    pub closed_resources: Vec<u64>,
    pub released_buffered_bytes: usize,
}

pub struct LifecycleState {
    limits: LifecycleLimits,
    pending: BTreeMap<u64, PendingOperation>,
    resources: BTreeMap<u64, BrokerResource>,
    buffered_bytes: usize,
    next_resource_id: u64,
}

impl LifecycleState {
    pub fn new(limits: LifecycleLimits) -> Self {
        Self {
            limits,
            pending: BTreeMap::new(),
            resources: BTreeMap::new(),
            buffered_bytes: 0,
            next_resource_id: 1,
        }
    }

    pub fn begin(
        &mut self,
        request_id: u64,
        capability: CapabilityId,
        deadline_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        if request_id == 0 || self.pending.contains_key(&request_id) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        if self.pending.len() >= self.limits.maximum_pending_operations {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        self.pending.insert(
            request_id,
            PendingOperation {
                request_id,
                capability,
                deadline_monotonic_micros,
            },
        );
        Ok(())
    }

    pub fn finish(&mut self, request_id: u64) -> Option<PendingOperation> {
        self.pending.remove(&request_id)
    }

    pub fn cancel(&mut self, request_id: u64) -> bool {
        self.pending.remove(&request_id).is_some()
    }

    pub fn expire(&mut self, now_monotonic_micros: u64) -> Vec<u64> {
        let expired = self
            .pending
            .values()
            .filter(|operation| operation.deadline_monotonic_micros <= now_monotonic_micros)
            .map(|operation| operation.request_id)
            .collect::<Vec<_>>();
        for request_id in &expired {
            self.pending.remove(request_id);
        }
        expired
    }

    pub fn allocate_resource(
        &mut self,
        capability: CapabilityId,
        buffered_bytes: usize,
    ) -> Result<u64, BrokerErrorCode> {
        if self.resources.len() >= self.limits.maximum_resources
            || self
                .buffered_bytes
                .checked_add(buffered_bytes)
                .is_none_or(|total| total > self.limits.maximum_buffered_bytes)
        {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        let resource_id = self.next_resource_id;
        self.next_resource_id = self
            .next_resource_id
            .checked_add(1)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
        self.buffered_bytes += buffered_bytes;
        self.resources.insert(
            resource_id,
            BrokerResource {
                resource_id,
                capability,
                buffered_bytes,
            },
        );
        Ok(resource_id)
    }

    pub fn close_resource(&mut self, resource_id: u64) -> bool {
        let Some(resource) = self.resources.remove(&resource_id) else {
            return false;
        };
        self.buffered_bytes = self.buffered_bytes.saturating_sub(resource.buffered_bytes);
        true
    }

    pub fn revoke(&mut self, capability: &CapabilityId) -> RevocationEffect {
        let cancelled_requests = self
            .pending
            .values()
            .filter(|operation| &operation.capability == capability)
            .map(|operation| operation.request_id)
            .collect::<Vec<_>>();
        let closed_resources = self
            .resources
            .values()
            .filter(|resource| &resource.capability == capability)
            .map(|resource| resource.resource_id)
            .collect::<Vec<_>>();
        for request_id in &cancelled_requests {
            self.pending.remove(request_id);
        }
        let mut released_buffered_bytes = 0;
        for resource_id in &closed_resources {
            if let Some(resource) = self.resources.remove(resource_id) {
                released_buffered_bytes += resource.buffered_bytes;
            }
        }
        self.buffered_bytes = self.buffered_bytes.saturating_sub(released_buffered_bytes);
        RevocationEffect {
            cancelled_requests,
            closed_resources,
            released_buffered_bytes,
        }
    }

    pub fn revoke_all(&mut self) -> RevocationEffect {
        let cancelled_requests = self.pending.keys().copied().collect();
        let closed_resources = self.resources.keys().copied().collect();
        let released_buffered_bytes = self.buffered_bytes;
        self.pending.clear();
        self.resources.clear();
        self.buffered_bytes = 0;
        RevocationEffect {
            cancelled_requests,
            closed_resources,
            released_buffered_bytes,
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffered_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotas_are_checked_before_allocation() {
        let mut state = LifecycleState::new(LifecycleLimits {
            maximum_pending_operations: 1,
            maximum_resources: 1,
            maximum_buffered_bytes: 8,
        });
        state.begin(1, CapabilityId::HttpRequestV1, 100).unwrap();
        assert_eq!(
            state.begin(2, CapabilityId::HttpRequestV1, 100),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        state.finish(1);
        let resource = state
            .allocate_resource(CapabilityId::HttpRequestV1, 8)
            .unwrap();
        assert_eq!(
            state.allocate_resource(CapabilityId::HttpRequestV1, 1),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        assert!(state.close_resource(resource));
        assert_eq!(state.buffered_bytes(), 0);
    }

    #[test]
    fn revocation_only_removes_authority_owned_by_that_capability() {
        let mut state = LifecycleState::new(LifecycleLimits::default());
        state.begin(1, CapabilityId::HttpRequestV1, 100).unwrap();
        state.begin(2, CapabilityId::DbusCallV1, 100).unwrap();
        let network = state
            .allocate_resource(CapabilityId::HttpRequestV1, 1024)
            .unwrap();
        let bus = state
            .allocate_resource(CapabilityId::DbusCallV1, 512)
            .unwrap();
        let effect = state.revoke(&CapabilityId::HttpRequestV1);
        assert_eq!(effect.cancelled_requests, vec![1]);
        assert_eq!(effect.closed_resources, vec![network]);
        assert_eq!(state.pending_count(), 1);
        assert_eq!(state.resource_count(), 1);
        assert_eq!(state.buffered_bytes(), 512);
        assert!(state.cancel(2));
        assert!(state.close_resource(bus));
    }

    #[test]
    fn deadlines_expire_deterministically() {
        let mut state = LifecycleState::new(LifecycleLimits::default());
        state.begin(1, CapabilityId::HttpRequestV1, 10).unwrap();
        state.begin(2, CapabilityId::HttpRequestV1, 20).unwrap();
        assert_eq!(state.expire(10), vec![1]);
        assert_eq!(state.pending_count(), 1);
    }
}
