use std::{collections::BTreeMap, time::Duration};

use crate::{Contact, ContactPhase, Point, Rect, WidgetId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GestureAxis {
    Any,
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GestureRecognizer {
    Tap {
        max_duration: Duration,
        max_movement: f32,
    },
    LongPress {
        minimum_duration: Duration,
        max_movement: f32,
    },
    Pan {
        axis: GestureAxis,
        minimum_distance: f32,
    },
    /// A swipe is evaluated at the end of a winning pan. This lets a widget
    /// observe continuous movement and still attach a velocity action.
    Swipe {
        axis: GestureAxis,
        minimum_distance: f32,
        minimum_velocity: f32,
    },
}

impl GestureRecognizer {
    pub const fn tap() -> Self {
        Self::Tap {
            max_duration: Duration::from_millis(350),
            max_movement: 8.0,
        }
    }

    pub const fn long_press(minimum_duration: Duration) -> Self {
        Self::LongPress {
            minimum_duration,
            max_movement: 8.0,
        }
    }

    pub const fn pan(axis: GestureAxis) -> Self {
        Self::Pan {
            axis,
            minimum_distance: 6.0,
        }
    }

    pub const fn swipe(axis: GestureAxis) -> Self {
        Self::Swipe {
            axis,
            minimum_distance: 24.0,
            minimum_velocity: 240.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GestureTarget {
    pub id: WidgetId,
    pub bounds: Rect,
    pub recognizers: Vec<GestureRecognizer>,
}

impl GestureTarget {
    pub fn new(
        id: WidgetId,
        bounds: Rect,
        recognizers: impl IntoIterator<Item = GestureRecognizer>,
    ) -> Self {
        Self {
            id,
            bounds,
            recognizers: recognizers.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GestureMap {
    targets: Vec<GestureTarget>,
}

impl GestureMap {
    pub fn add(&mut self, target: GestureTarget) {
        self.targets.push(target);
    }

    pub fn region(
        &mut self,
        id: WidgetId,
        bounds: Rect,
        recognizers: impl IntoIterator<Item = GestureRecognizer>,
    ) {
        self.add(GestureTarget::new(id, bounds, recognizers));
    }

    fn hit_test(&self, point: Point) -> Option<&GestureTarget> {
        self.targets
            .iter()
            .rev()
            .find(|target| target.bounds.contains(point))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GestureEvent {
    Pressed {
        id: WidgetId,
        contact: u32,
        position: Point,
    },
    Tap {
        id: WidgetId,
        contact: u32,
        position: Point,
    },
    LongPressBegan {
        id: WidgetId,
        contact: u32,
        position: Point,
    },
    LongPressEnded {
        id: WidgetId,
        contact: u32,
        position: Point,
    },
    PanBegan {
        id: WidgetId,
        contact: u32,
        position: Point,
    },
    PanChanged {
        id: WidgetId,
        contact: u32,
        position: Point,
        translation: Point,
        delta: Point,
    },
    PanEnded {
        id: WidgetId,
        contact: u32,
        position: Point,
        translation: Point,
        velocity: Point,
    },
    Swipe {
        id: WidgetId,
        contact: u32,
        translation: Point,
        velocity: Point,
    },
    Released {
        id: WidgetId,
        contact: u32,
        position: Point,
    },
    Cancelled {
        id: WidgetId,
        contact: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Winner {
    LongPress,
    Pan,
}

#[derive(Clone, Debug)]
struct Session {
    target: GestureTarget,
    began: Duration,
    origin: Point,
    last_position: Point,
    max_distance: f32,
    winner: Option<Winner>,
}

/// Resolves competing recognizers per contact and preserves capture after the
/// finger leaves the original region. Independent contacts have independent
/// arenas, which is the base needed for later multi-touch recognizers.
#[derive(Default)]
pub struct GestureArena {
    sessions: BTreeMap<u32, Session>,
}

impl GestureArena {
    pub fn handle(&mut self, map: &GestureMap, contact: Contact) -> Vec<GestureEvent> {
        match contact.phase {
            ContactPhase::Down => self.begin(map, contact),
            ContactPhase::Motion => self.motion(contact),
            ContactPhase::Up => self.end(contact),
            ContactPhase::Cancel => self.cancel(contact.id),
        }
    }

    fn begin(&mut self, map: &GestureMap, contact: Contact) -> Vec<GestureEvent> {
        let Some(target) = map.hit_test(contact.position).cloned() else {
            return Vec::new();
        };
        self.sessions.insert(
            contact.id,
            Session {
                target: target.clone(),
                began: contact.time,
                origin: contact.position,
                last_position: contact.position,
                max_distance: 0.0,
                winner: None,
            },
        );
        vec![GestureEvent::Pressed {
            id: target.id,
            contact: contact.id,
            position: contact.position,
        }]
    }

    fn motion(&mut self, contact: Contact) -> Vec<GestureEvent> {
        let Some(session) = self.sessions.get_mut(&contact.id) else {
            return Vec::new();
        };
        let delta = subtract(contact.position, session.last_position);
        let translation = subtract(contact.position, session.origin);
        session.max_distance = session.max_distance.max(distance(translation));
        session.last_position = contact.position;

        if session.winner == Some(Winner::Pan) {
            return vec![pan_changed(
                session,
                contact.id,
                contact.position,
                translation,
                delta,
            )];
        }
        if session.winner == Some(Winner::LongPress) {
            return Vec::new();
        }

        let pan_won = session.target.recognizers.iter().any(|recognizer| {
            let GestureRecognizer::Pan {
                axis,
                minimum_distance,
            } = *recognizer
            else {
                return false;
            };
            axis_distance(translation, axis) >= minimum_distance && axis_matches(translation, axis)
        });
        if pan_won {
            session.winner = Some(Winner::Pan);
            return vec![
                GestureEvent::PanBegan {
                    id: session.target.id,
                    contact: contact.id,
                    position: contact.position,
                },
                pan_changed(session, contact.id, contact.position, translation, delta),
            ];
        }
        Vec::new()
    }

    fn end(&mut self, contact: Contact) -> Vec<GestureEvent> {
        let Some(session) = self.sessions.remove(&contact.id) else {
            return Vec::new();
        };
        let translation = subtract(contact.position, session.origin);
        let velocity = velocity(translation, contact.time.saturating_sub(session.began));
        let mut events = match session.winner {
            Some(Winner::Pan) => {
                let mut events = vec![GestureEvent::PanEnded {
                    id: session.target.id,
                    contact: contact.id,
                    position: contact.position,
                    translation,
                    velocity,
                }];
                if session.target.recognizers.iter().any(|recognizer| {
                    let GestureRecognizer::Swipe {
                        axis,
                        minimum_distance,
                        minimum_velocity,
                    } = *recognizer
                    else {
                        return false;
                    };
                    axis_matches(translation, axis)
                        && axis_distance(translation, axis) >= minimum_distance
                        && axis_distance(velocity, axis) >= minimum_velocity
                }) {
                    events.push(GestureEvent::Swipe {
                        id: session.target.id,
                        contact: contact.id,
                        translation,
                        velocity,
                    });
                }
                events
            }
            Some(Winner::LongPress) => vec![GestureEvent::LongPressEnded {
                id: session.target.id,
                contact: contact.id,
                position: contact.position,
            }],
            None => session
                .target
                .recognizers
                .iter()
                .find_map(|recognizer| {
                    let GestureRecognizer::Tap {
                        max_duration,
                        max_movement,
                    } = *recognizer
                    else {
                        return None;
                    };
                    (contact.time.saturating_sub(session.began) <= max_duration
                        && session.max_distance.max(distance(translation)) <= max_movement)
                        .then_some(GestureEvent::Tap {
                            id: session.target.id,
                            contact: contact.id,
                            position: contact.position,
                        })
                })
                .into_iter()
                .collect(),
        };
        events.push(GestureEvent::Released {
            id: session.target.id,
            contact: contact.id,
            position: contact.position,
        });
        events
    }

    fn cancel(&mut self, contact: u32) -> Vec<GestureEvent> {
        self.sessions
            .remove(&contact)
            .map(|session| {
                vec![GestureEvent::Cancelled {
                    id: session.target.id,
                    contact,
                }]
            })
            .unwrap_or_default()
    }

    /// Advances stationary long presses even when the compositor sends no
    /// motion. Once one wins, competing tap and pan recognizers cannot fire.
    pub fn tick(&mut self, now: Duration) -> Vec<GestureEvent> {
        let mut events = Vec::new();
        for (contact, session) in &mut self.sessions {
            if session.winner.is_some() {
                continue;
            }
            let recognized = session.target.recognizers.iter().any(|recognizer| {
                let GestureRecognizer::LongPress {
                    minimum_duration,
                    max_movement,
                } = *recognizer
                else {
                    return false;
                };
                now.saturating_sub(session.began) >= minimum_duration
                    && session.max_distance <= max_movement
            });
            if recognized {
                session.winner = Some(Winner::LongPress);
                events.push(GestureEvent::LongPressBegan {
                    id: session.target.id,
                    contact: *contact,
                    position: session.last_position,
                });
            }
        }
        events
    }

    pub fn has_contact(&self, contact: u32) -> bool {
        self.sessions.contains_key(&contact)
    }
}

fn pan_changed(
    session: &Session,
    contact: u32,
    position: Point,
    translation: Point,
    delta: Point,
) -> GestureEvent {
    GestureEvent::PanChanged {
        id: session.target.id,
        contact,
        position,
        translation,
        delta,
    }
}

fn subtract(left: Point, right: Point) -> Point {
    Point::new(left.x - right.x, left.y - right.y)
}

fn distance(point: Point) -> f32 {
    point.x.hypot(point.y)
}

fn axis_distance(point: Point, axis: GestureAxis) -> f32 {
    match axis {
        GestureAxis::Any => distance(point),
        GestureAxis::Horizontal => point.x.abs(),
        GestureAxis::Vertical => point.y.abs(),
    }
}

fn axis_matches(point: Point, axis: GestureAxis) -> bool {
    match axis {
        GestureAxis::Any => true,
        GestureAxis::Horizontal => point.x.abs() >= point.y.abs(),
        GestureAxis::Vertical => point.y.abs() >= point.x.abs(),
    }
}

fn velocity(translation: Point, elapsed: Duration) -> Point {
    let seconds = elapsed.as_secs_f32();
    if seconds <= f32::EPSILON {
        Point::default()
    } else {
        Point::new(translation.x / seconds, translation.y / seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(id: u32, phase: ContactPhase, x: f32, y: f32, millis: u64) -> Contact {
        Contact {
            id,
            phase,
            position: Point::new(x, y),
            time: Duration::from_millis(millis),
        }
    }

    fn map(recognizers: Vec<GestureRecognizer>) -> GestureMap {
        let mut map = GestureMap::default();
        map.region(WidgetId(7), Rect::new(0.0, 0.0, 200.0, 60.0), recognizers);
        map
    }

    #[test]
    fn pan_wins_over_tap_after_threshold() {
        let map = map(vec![
            GestureRecognizer::tap(),
            GestureRecognizer::pan(GestureAxis::Horizontal),
        ]);
        let mut arena = GestureArena::default();
        arena.handle(&map, contact(1, ContactPhase::Down, 20.0, 20.0, 0));
        let moved = arena.handle(&map, contact(1, ContactPhase::Motion, 40.0, 21.0, 20));
        assert!(matches!(moved[0], GestureEvent::PanBegan { .. }));
        let ended = arena.handle(&map, contact(1, ContactPhase::Up, 50.0, 21.0, 50));
        assert!(
            ended
                .iter()
                .any(|event| matches!(event, GestureEvent::PanEnded { .. }))
        );
        assert!(
            !ended
                .iter()
                .any(|event| matches!(event, GestureEvent::Tap { .. }))
        );
    }

    #[test]
    fn long_press_wins_while_stationary_and_suppresses_tap() {
        let map = map(vec![
            GestureRecognizer::tap(),
            GestureRecognizer::long_press(Duration::from_millis(400)),
        ]);
        let mut arena = GestureArena::default();
        arena.handle(&map, contact(3, ContactPhase::Down, 20.0, 20.0, 0));
        assert!(matches!(
            arena.tick(Duration::from_millis(401))[0],
            GestureEvent::LongPressBegan { .. }
        ));
        let ended = arena.handle(&map, contact(3, ContactPhase::Up, 20.0, 20.0, 500));
        assert!(matches!(ended[0], GestureEvent::LongPressEnded { .. }));
        assert!(
            !ended
                .iter()
                .any(|event| matches!(event, GestureEvent::Tap { .. }))
        );
    }

    #[test]
    fn contacts_are_independent_and_cancel_is_terminal() {
        let map = map(vec![GestureRecognizer::tap()]);
        let mut arena = GestureArena::default();
        arena.handle(&map, contact(1, ContactPhase::Down, 10.0, 10.0, 0));
        arena.handle(&map, contact(2, ContactPhase::Down, 20.0, 20.0, 0));
        assert_eq!(arena.cancel(1).len(), 1);
        assert!(!arena.has_contact(1));
        assert!(arena.has_contact(2));
        let ended = arena.handle(&map, contact(2, ContactPhase::Up, 20.0, 20.0, 40));
        assert!(
            ended
                .iter()
                .any(|event| matches!(event, GestureEvent::Tap { contact: 2, .. }))
        );
    }

    #[test]
    fn swipe_is_emitted_after_a_fast_pan() {
        let map = map(vec![
            GestureRecognizer::pan(GestureAxis::Horizontal),
            GestureRecognizer::swipe(GestureAxis::Horizontal),
        ]);
        let mut arena = GestureArena::default();
        arena.handle(&map, contact(1, ContactPhase::Down, 10.0, 20.0, 0));
        arena.handle(&map, contact(1, ContactPhase::Motion, 30.0, 20.0, 20));
        let ended = arena.handle(&map, contact(1, ContactPhase::Up, 80.0, 20.0, 100));
        assert!(
            ended
                .iter()
                .any(|event| matches!(event, GestureEvent::Swipe { .. }))
        );
    }

    #[test]
    fn moving_away_then_back_cannot_resurrect_tap_or_hold() {
        let map = map(vec![
            GestureRecognizer::tap(),
            GestureRecognizer::long_press(Duration::from_millis(300)),
        ]);
        let mut arena = GestureArena::default();
        arena.handle(&map, contact(1, ContactPhase::Down, 20.0, 20.0, 0));
        arena.handle(&map, contact(1, ContactPhase::Motion, 50.0, 20.0, 100));
        arena.handle(&map, contact(1, ContactPhase::Motion, 20.0, 20.0, 200));
        assert!(arena.tick(Duration::from_millis(400)).is_empty());
        let ended = arena.handle(&map, contact(1, ContactPhase::Up, 20.0, 20.0, 410));
        assert!(
            !ended
                .iter()
                .any(|event| matches!(event, GestureEvent::Tap { .. }))
        );
    }
}
