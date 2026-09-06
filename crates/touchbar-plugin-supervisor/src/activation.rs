use std::collections::BTreeSet;

use touchbar_protocol::broker_ipc::{ActivationContext, ActivationOrigin, BrokerErrorCode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationExpectation<'a> {
    pub surface_instance: u64,
    pub item_id: &'a str,
    pub widget_id: u64,
    pub now_monotonic_micros: u64,
}

pub struct ActivationLedger {
    consumed_sequences: BTreeSet<u64>,
    maximum_consumed_sequences: usize,
}

impl ActivationLedger {
    pub fn new(maximum_consumed_sequences: usize) -> Self {
        Self {
            consumed_sequences: BTreeSet::new(),
            maximum_consumed_sequences,
        }
    }

    pub fn consume(
        &mut self,
        activation: Option<&ActivationContext>,
        expected: ActivationExpectation<'_>,
    ) -> Result<ActivationOrigin, BrokerErrorCode> {
        let activation = activation.ok_or(BrokerErrorCode::ActivationRequired)?;
        if activation.origin == ActivationOrigin::Synthetic
            || activation.surface_instance != expected.surface_instance
            || activation.item_id != expected.item_id
            || activation.widget_id != expected.widget_id
            || activation.deadline_monotonic_micros < expected.now_monotonic_micros
            || activation.input_sequence == 0
            || self.consumed_sequences.contains(&activation.input_sequence)
        {
            return Err(BrokerErrorCode::ActivationRequired);
        }
        if self.consumed_sequences.len() >= self.maximum_consumed_sequences {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        self.consumed_sequences.insert(activation.input_sequence);
        Ok(activation.origin)
    }

    pub fn clear(&mut self) {
        self.consumed_sequences.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activation(origin: ActivationOrigin) -> ActivationContext {
        ActivationContext {
            origin,
            surface_instance: 4,
            item_id: "media".into(),
            widget_id: 9,
            input_sequence: 12,
            deadline_monotonic_micros: 100,
        }
    }

    fn expected(now: u64) -> ActivationExpectation<'static> {
        ActivationExpectation {
            surface_instance: 4,
            item_id: "media",
            widget_id: 9,
            now_monotonic_micros: now,
        }
    }

    #[test]
    fn physical_activation_is_single_use() {
        let mut ledger = ActivationLedger::new(8);
        let activation = activation(ActivationOrigin::Physical);
        assert_eq!(
            ledger.consume(Some(&activation), expected(99)),
            Ok(ActivationOrigin::Physical)
        );
        assert_eq!(
            ledger.consume(Some(&activation), expected(99)),
            Err(BrokerErrorCode::ActivationRequired)
        );
    }

    #[test]
    fn synthetic_expired_and_cross_widget_activations_fail() {
        let mut ledger = ActivationLedger::new(8);
        assert_eq!(
            ledger.consume(Some(&activation(ActivationOrigin::Synthetic)), expected(99)),
            Err(BrokerErrorCode::ActivationRequired)
        );
        assert_eq!(
            ledger.consume(Some(&activation(ActivationOrigin::Physical)), expected(101)),
            Err(BrokerErrorCode::ActivationRequired)
        );
        let activation = activation(ActivationOrigin::Physical);
        let mut wrong_widget = expected(99);
        wrong_widget.widget_id = 10;
        assert_eq!(
            ledger.consume(Some(&activation), wrong_widget),
            Err(BrokerErrorCode::ActivationRequired)
        );
    }
}
