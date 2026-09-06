use std::collections::{BTreeMap, VecDeque};

use touchbar_protocol::broker_ipc::{
    BrokerErrorCode, BrokerResult, CapabilityState, SupervisorMessage,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostEvent {
    CapabilityChanged {
        generation: u64,
        state: CapabilityState,
    },
    Completion {
        request_id: u64,
        result: BrokerResult,
    },
    ResourceEvent {
        resource_id: u64,
        sequence: u64,
        result: BrokerResult,
    },
    Overflow {
        generation: u64,
        dropped_events: u64,
    },
}

impl HostEvent {
    pub fn into_message(self) -> SupervisorMessage {
        match self {
            Self::CapabilityChanged { generation, state } => {
                SupervisorMessage::CapabilityChanged { generation, state }
            }
            Self::Completion { request_id, result } => {
                SupervisorMessage::Response { request_id, result }
            }
            Self::ResourceEvent {
                resource_id,
                sequence,
                result,
            } => SupervisorMessage::ResourceEvent {
                resource_id,
                sequence,
                result,
            },
            Self::Overflow {
                generation,
                dropped_events,
            } => SupervisorMessage::Overflow {
                generation,
                dropped_events,
            },
        }
    }
}

pub struct HostEventQueue {
    maximum_state_events: usize,
    maximum_edge_events: usize,
    state_events: BTreeMap<String, (u64, CapabilityState)>,
    edge_events: VecDeque<HostEvent>,
    dropped_edge_events: u64,
    overflow_generation: u64,
}

impl HostEventQueue {
    pub fn new(maximum_state_events: usize, maximum_edge_events: usize) -> Self {
        Self {
            maximum_state_events,
            maximum_edge_events,
            state_events: BTreeMap::new(),
            edge_events: VecDeque::new(),
            dropped_edge_events: 0,
            overflow_generation: 0,
        }
    }

    pub fn push_capability(
        &mut self,
        generation: u64,
        state: CapabilityState,
    ) -> Result<(), BrokerErrorCode> {
        if !self.state_events.contains_key(&state.capability)
            && self.state_events.len() >= self.maximum_state_events
        {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        self.state_events
            .insert(state.capability.clone(), (generation, state));
        Ok(())
    }

    pub fn push_completion(&mut self, request_id: u64, result: BrokerResult, generation: u64) {
        self.push_edge(HostEvent::Completion { request_id, result }, generation);
    }

    pub fn push_resource(
        &mut self,
        resource_id: u64,
        sequence: u64,
        result: BrokerResult,
        generation: u64,
    ) {
        self.push_edge(
            HostEvent::ResourceEvent {
                resource_id,
                sequence,
                result,
            },
            generation,
        );
    }

    pub fn record_dropped(&mut self, dropped_events: u64, generation: u64) {
        self.dropped_edge_events = self.dropped_edge_events.saturating_add(dropped_events);
        self.overflow_generation = self.overflow_generation.max(generation);
    }

    fn push_edge(&mut self, event: HostEvent, generation: u64) {
        if self.edge_events.len() == self.maximum_edge_events {
            self.record_dropped(1, generation);
            return;
        }
        self.edge_events.push_back(event);
    }

    pub fn pop(&mut self) -> Option<HostEvent> {
        if let Some(capability) = self.state_events.keys().next().cloned() {
            let (generation, state) = self
                .state_events
                .remove(&capability)
                .expect("key came from state event map");
            return Some(HostEvent::CapabilityChanged { generation, state });
        }
        if let Some(event) = self.edge_events.pop_front() {
            return Some(event);
        }
        if self.dropped_edge_events > 0 {
            let event = HostEvent::Overflow {
                generation: self.overflow_generation,
                dropped_events: self.dropped_edge_events,
            };
            self.dropped_edge_events = 0;
            self.overflow_generation = 0;
            return Some(event);
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.state_events.is_empty() && self.edge_events.is_empty() && self.dropped_edge_events == 0
    }
}

#[cfg(test)]
mod tests {
    use touchbar_protocol::broker_ipc::WireCapabilityStatus;

    use super::*;

    fn state(status: WireCapabilityStatus) -> CapabilityState {
        CapabilityState {
            capability: "media.read.v1".into(),
            required: false,
            status,
        }
    }

    #[test]
    fn state_events_coalesce_to_the_latest_generation() {
        let mut queue = HostEventQueue::new(2, 2);
        queue
            .push_capability(1, state(WireCapabilityStatus::Granted))
            .unwrap();
        queue
            .push_capability(2, state(WireCapabilityStatus::Denied))
            .unwrap();
        assert_eq!(
            queue.pop(),
            Some(HostEvent::CapabilityChanged {
                generation: 2,
                state: state(WireCapabilityStatus::Denied),
            })
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn edge_overflow_is_explicit_and_preserves_queued_order() {
        let mut queue = HostEventQueue::new(2, 2);
        queue.push_completion(1, BrokerResult::Error(BrokerErrorCode::Denied), 1);
        queue.push_completion(2, BrokerResult::Error(BrokerErrorCode::Denied), 1);
        queue.push_completion(3, BrokerResult::Error(BrokerErrorCode::Denied), 4);
        assert!(matches!(
            queue.pop(),
            Some(HostEvent::Completion { request_id: 1, .. })
        ));
        assert!(matches!(
            queue.pop(),
            Some(HostEvent::Completion { request_id: 2, .. })
        ));
        assert_eq!(
            queue.pop(),
            Some(HostEvent::Overflow {
                generation: 4,
                dropped_events: 1,
            })
        );
    }

    #[test]
    fn resource_events_preserve_resource_sequence() {
        let mut queue = HostEventQueue::new(2, 2);
        queue.push_resource(
            7,
            3,
            BrokerResult::Success {
                payload: vec![1, 2],
            },
            1,
        );
        assert_eq!(
            queue.pop(),
            Some(HostEvent::ResourceEvent {
                resource_id: 7,
                sequence: 3,
                result: BrokerResult::Success {
                    payload: vec![1, 2]
                },
            })
        );
    }
}
