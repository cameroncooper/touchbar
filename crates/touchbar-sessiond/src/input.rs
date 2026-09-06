use std::{collections::HashMap, hash::Hash};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Down,
    Motion,
    Up,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contact {
    pub id: u32,
    pub phase: Phase,
    pub x: f64,
    pub y: f64,
    pub time_ms: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Target<K> {
    pub key: K,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub layer: u64,
    pub visible: bool,
}

impl<K> Target<K> {
    fn contains(&self, x: f64, y: f64) -> bool {
        self.visible
            && x >= self.x
            && x <= self.x + self.width
            && y >= self.y
            && y <= self.y + self.height
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoutedContact<K> {
    pub target: K,
    pub id: u32,
    pub phase: Phase,
    pub local_x: f64,
    pub local_y: f64,
    pub time_ms: u32,
}

/// Global hit testing plus contact capture. A captured target is retained by
/// identity, while its latest geometry is used for every event; this is what
/// lets a press continue as a slider drag after the surface expands.
pub struct Router<K> {
    captures: HashMap<u32, K>,
}

impl<K> Default for Router<K> {
    fn default() -> Self {
        Self {
            captures: HashMap::new(),
        }
    }
}

impl<K: Clone + Eq + Hash> Router<K> {
    pub fn hit_target(&self, x: f64, y: f64, targets: &[Target<K>]) -> Option<K> {
        targets
            .iter()
            .filter(|target| target.contains(x, y))
            .max_by_key(|target| target.layer)
            .map(|target| target.key.clone())
    }

    pub fn route(&mut self, contact: Contact, targets: &[Target<K>]) -> Option<RoutedContact<K>> {
        let key = match contact.phase {
            Phase::Down => {
                if self.captures.contains_key(&contact.id) {
                    return None;
                }
                let key = self.hit_target(contact.x, contact.y, targets)?;
                self.captures.insert(contact.id, key.clone());
                key
            }
            Phase::Motion | Phase::Up | Phase::Cancel => self.captures.get(&contact.id)?.clone(),
        };
        let target = targets.iter().find(|target| target.key == key)?;
        let routed = RoutedContact {
            target: key,
            id: contact.id,
            phase: contact.phase,
            local_x: contact.x - target.x,
            local_y: contact.y - target.y,
            time_ms: contact.time_ms,
        };
        if matches!(contact.phase, Phase::Up | Phase::Cancel) {
            self.captures.remove(&contact.id);
        }
        Some(routed)
    }

    pub fn is_captured_by(&self, contact_id: u32, target: &K) -> bool {
        self.captures.get(&contact_id) == Some(target)
    }

    pub fn captured_target(&self, contact_id: u32) -> Option<&K> {
        self.captures.get(&contact_id)
    }

    pub fn retarget(&mut self, contact_id: u32, target: K) -> Option<K> {
        let captured = self.captures.get_mut(&contact_id)?;
        Some(std::mem::replace(captured, target))
    }

    pub fn cancel_target(&mut self, target: &K) -> Vec<u32> {
        let contacts = self
            .captures
            .iter()
            .filter_map(|(contact, captured)| (captured == target).then_some(*contact))
            .collect::<Vec<_>>();
        self.captures.retain(|_, captured| captured != target);
        contacts
    }

    pub fn cancel_all(&mut self) -> Vec<(u32, K)> {
        self.captures.drain().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(key: u8, x: f64, width: f64, layer: u64) -> Target<u8> {
        Target {
            key,
            x,
            y: 0.0,
            width,
            height: 60.0,
            layer,
            visible: true,
        }
    }

    #[test]
    fn down_uses_topmost_hit_and_returns_local_coordinates() {
        let targets = [target(1, 0.0, 80.0, 1), target(2, 20.0, 80.0, 2)];
        let routed = Router::default()
            .route(
                Contact {
                    id: 7,
                    phase: Phase::Down,
                    x: 30.0,
                    y: 12.0,
                    time_ms: 10,
                },
                &targets,
            )
            .unwrap();
        assert_eq!(routed.target, 2);
        assert_eq!((routed.local_x, routed.local_y), (10.0, 12.0));
    }

    #[test]
    fn captured_contact_uses_new_geometry_after_popover() {
        let mut router = Router::default();
        router.route(
            Contact {
                id: 4,
                phase: Phase::Down,
                x: 120.0,
                y: 20.0,
                time_ms: 0,
            },
            &[target(1, 100.0, 100.0, 1)],
        );
        let routed = router
            .route(
                Contact {
                    id: 4,
                    phase: Phase::Motion,
                    x: 250.0,
                    y: 20.0,
                    time_ms: 400,
                },
                &[target(1, 0.0, 300.0, 1)],
            )
            .unwrap();
        assert_eq!(routed.target, 1);
        assert_eq!(routed.local_x, 250.0);
    }

    #[test]
    fn capture_can_transfer_to_an_expanded_neighbor() {
        let mut router = Router::default();
        let compact = [target(1, 0.0, 100.0, 1)];
        router.route(
            Contact {
                id: 4,
                phase: Phase::Down,
                x: 20.0,
                y: 20.0,
                time_ms: 0,
            },
            &compact,
        );
        let expanded = [target(1, 0.0, 100.0, 2), target(2, 100.0, 100.0, 2)];
        assert_eq!(router.hit_target(150.0, 20.0, &expanded), Some(2));
        assert_eq!(router.retarget(4, 2), Some(1));
        let routed = router
            .route(
                Contact {
                    id: 4,
                    phase: Phase::Motion,
                    x: 150.0,
                    y: 20.0,
                    time_ms: 400,
                },
                &expanded,
            )
            .unwrap();
        assert_eq!(routed.target, 2);
        assert_eq!(routed.local_x, 50.0);
    }

    #[test]
    fn release_ends_capture() {
        let mut router = Router::default();
        let targets = [target(1, 0.0, 100.0, 1)];
        router.route(
            Contact {
                id: 2,
                phase: Phase::Down,
                x: 10.0,
                y: 10.0,
                time_ms: 0,
            },
            &targets,
        );
        assert!(router.is_captured_by(2, &1));
        router.route(
            Contact {
                id: 2,
                phase: Phase::Up,
                x: 10.0,
                y: 10.0,
                time_ms: 20,
            },
            &targets,
        );
        assert!(!router.is_captured_by(2, &1));
    }

    #[test]
    fn cancellation_ends_capture() {
        let mut router = Router::default();
        let targets = [target(1, 0.0, 100.0, 1)];
        router.route(
            Contact {
                id: 8,
                phase: Phase::Down,
                x: 10.0,
                y: 10.0,
                time_ms: 0,
            },
            &targets,
        );
        let cancelled = router
            .route(
                Contact {
                    id: 8,
                    phase: Phase::Cancel,
                    x: 10.0,
                    y: 10.0,
                    time_ms: 20,
                },
                &targets,
            )
            .unwrap();
        assert_eq!(cancelled.phase, Phase::Cancel);
        assert!(!router.is_captured_by(8, &1));
    }

    #[test]
    fn cancel_all_returns_every_capture_and_empties_the_router() {
        let mut router = Router::default();
        let targets = [target(1, 0.0, 100.0, 1), target(2, 100.0, 100.0, 1)];
        for (id, x) in [(4, 10.0), (7, 110.0)] {
            router.route(
                Contact {
                    id,
                    phase: Phase::Down,
                    x,
                    y: 10.0,
                    time_ms: 0,
                },
                &targets,
            );
        }
        let mut cancelled = router.cancel_all();
        cancelled.sort_by_key(|(id, _)| *id);
        assert_eq!(cancelled, vec![(4, 1), (7, 2)]);
        assert!(!router.is_captured_by(4, &1));
        assert!(!router.is_captured_by(7, &2));
    }
}
