use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthEvent {
    GuestTrap,
    Timeout,
    ProtocolViolation,
    QuotaViolation,
    CleanRun,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthState {
    Healthy,
    RestartAfter { milliseconds: u64 },
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthPolicy {
    pub window_millis: u64,
    pub failures_before_restart: usize,
    pub restarts_before_disable: usize,
    pub initial_backoff_millis: u64,
    pub maximum_backoff_millis: u64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            window_millis: 60_000,
            failures_before_restart: 3,
            restarts_before_disable: 5,
            initial_backoff_millis: 250,
            maximum_backoff_millis: 30_000,
        }
    }
}

pub struct HealthTracker {
    policy: HealthPolicy,
    failures: VecDeque<u64>,
    restart_count: usize,
    state: HealthState,
}

impl HealthTracker {
    pub fn new(policy: HealthPolicy) -> Self {
        Self {
            policy,
            failures: VecDeque::new(),
            restart_count: 0,
            state: HealthState::Healthy,
        }
    }

    pub fn record(&mut self, event: HealthEvent, now_millis: u64) -> HealthState {
        if event == HealthEvent::CleanRun {
            self.failures.clear();
            if self.state != HealthState::Disabled {
                self.state = HealthState::Healthy;
            }
            return self.state;
        }
        while self
            .failures
            .front()
            .is_some_and(|time| now_millis.saturating_sub(*time) > self.policy.window_millis)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now_millis);
        if self.failures.len() < self.policy.failures_before_restart {
            return self.state;
        }
        self.failures.clear();
        self.restart_count = self.restart_count.saturating_add(1);
        if self.restart_count >= self.policy.restarts_before_disable {
            self.state = HealthState::Disabled;
            return self.state;
        }
        let exponent = self.restart_count.saturating_sub(1).min(31) as u32;
        let backoff = self
            .policy
            .initial_backoff_millis
            .saturating_mul(1_u64 << exponent)
            .min(self.policy.maximum_backoff_millis);
        self.state = HealthState::RestartAfter {
            milliseconds: backoff,
        };
        self.state
    }

    pub fn state(&self) -> HealthState {
        self.state
    }

    pub fn acknowledge_restart(&mut self) {
        if self.state != HealthState::Disabled {
            self.state = HealthState::Healthy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_failures_back_off_then_disable() {
        let mut tracker = HealthTracker::new(HealthPolicy {
            failures_before_restart: 2,
            restarts_before_disable: 3,
            ..HealthPolicy::default()
        });
        assert_eq!(
            tracker.record(HealthEvent::GuestTrap, 0),
            HealthState::Healthy
        );
        assert_eq!(
            tracker.record(HealthEvent::GuestTrap, 1),
            HealthState::RestartAfter { milliseconds: 250 }
        );
        tracker.acknowledge_restart();
        tracker.record(HealthEvent::Timeout, 2);
        assert_eq!(
            tracker.record(HealthEvent::Timeout, 3),
            HealthState::RestartAfter { milliseconds: 500 }
        );
        tracker.acknowledge_restart();
        tracker.record(HealthEvent::ProtocolViolation, 4);
        assert_eq!(
            tracker.record(HealthEvent::ProtocolViolation, 5),
            HealthState::Disabled
        );
        tracker.acknowledge_restart();
        assert_eq!(tracker.state(), HealthState::Disabled);
    }

    #[test]
    fn old_failures_age_out_of_the_window() {
        let mut tracker = HealthTracker::new(HealthPolicy {
            window_millis: 10,
            failures_before_restart: 2,
            ..HealthPolicy::default()
        });
        tracker.record(HealthEvent::GuestTrap, 0);
        assert_eq!(
            tracker.record(HealthEvent::GuestTrap, 11),
            HealthState::Healthy
        );
    }
}
