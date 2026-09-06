use crate::{
    GestureAxis, GestureEvent, GestureRecognizer, GestureTarget, Node, Point, Rect, WidgetId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpansionDirection {
    Leading,
    Trailing,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaletteLayout {
    pub activator: Rect,
    pub direction: ExpansionDirection,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaletteOptionPlacement {
    pub index: usize,
    pub bounds: Rect,
    pub selected: bool,
    pub highlighted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PaletteEvent {
    HighlightChanged(Option<usize>),
    SelectionChanged(usize),
    DismissRequested,
    Cancelled,
}

/// A stationary option strip pinned to the compact control that opened it.
/// Unlike a scrubber, its cells never move during a press-drag selection.
#[derive(Clone, Debug)]
pub struct SelectionPalette {
    id: WidgetId,
    option_count: usize,
    gap: f32,
    selected: Option<usize>,
    highlighted: Option<usize>,
    starting_selection: Option<Option<usize>>,
}

impl SelectionPalette {
    pub fn new(id: WidgetId, option_count: usize) -> Self {
        Self {
            id,
            option_count,
            gap: 4.0,
            selected: None,
            highlighted: None,
            starting_selection: None,
        }
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap.max(0.0);
        self
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn set_selected(&mut self, selected: Option<usize>) {
        self.selected = selected.filter(|index| *index < self.option_count);
        self.highlighted = self.selected;
    }

    pub fn layout(&self, viewport: Rect, compact_anchor: Rect) -> PaletteLayout {
        let anchor = intersect(viewport, compact_anchor);
        let leading_space = (anchor.x - viewport.x).max(0.0);
        let trailing_space = (viewport.x + viewport.width - anchor.x - anchor.width).max(0.0);
        PaletteLayout {
            activator: anchor,
            direction: if trailing_space >= leading_space {
                ExpansionDirection::Trailing
            } else {
                ExpansionDirection::Leading
            },
        }
    }

    pub fn placements(&self, viewport: Rect, compact_anchor: Rect) -> Vec<PaletteOptionPlacement> {
        if self.option_count == 0 {
            return Vec::new();
        }
        let layout = self.layout(viewport, compact_anchor);
        let (start, available) = match layout.direction {
            ExpansionDirection::Leading => (viewport.x, (layout.activator.x - viewport.x).max(0.0)),
            ExpansionDirection::Trailing => {
                let start = layout.activator.x + layout.activator.width;
                (start, (viewport.x + viewport.width - start).max(0.0))
            }
        };
        let gaps = self.gap * self.option_count.saturating_sub(1) as f32;
        let extent = ((available - gaps).max(0.0) / self.option_count as f32).max(0.0);
        (0..self.option_count)
            .map(|index| PaletteOptionPlacement {
                index,
                bounds: Rect::new(
                    start + index as f32 * (extent + self.gap),
                    viewport.y,
                    extent,
                    viewport.height,
                ),
                selected: self.selected == Some(index),
                highlighted: self.highlighted == Some(index),
            })
            .collect()
    }

    pub fn compose(
        &self,
        viewport: Rect,
        compact_anchor: Rect,
        activator: impl FnOnce() -> Node,
        mut option: impl FnMut(PaletteOptionPlacement) -> Node,
    ) -> Node {
        let layout = self.layout(viewport, compact_anchor);
        let mut nodes = vec![Node::positioned(
            localize(layout.activator, viewport),
            activator(),
        )];
        nodes.extend(
            self.placements(viewport, compact_anchor)
                .into_iter()
                .map(|placement| {
                    Node::positioned(localize(placement.bounds, viewport), option(placement))
                }),
        );
        Node::Layer(nodes)
    }

    pub fn gesture_target(&self, viewport: Rect) -> GestureTarget {
        GestureTarget::new(
            self.id,
            viewport,
            [
                GestureRecognizer::tap(),
                GestureRecognizer::pan(GestureAxis::Horizontal),
            ],
        )
    }

    pub fn handle(
        &mut self,
        event: GestureEvent,
        viewport: Rect,
        compact_anchor: Rect,
    ) -> Vec<PaletteEvent> {
        if event_id(event) != self.id {
            return Vec::new();
        }
        match event {
            GestureEvent::Pressed { .. } => {
                self.starting_selection = Some(self.selected);
                Vec::new()
            }
            GestureEvent::Tap { position, .. } => {
                self.starting_selection = None;
                if self
                    .layout(viewport, compact_anchor)
                    .activator
                    .contains(position)
                {
                    return vec![PaletteEvent::DismissRequested];
                }
                self.option_at(position, viewport, compact_anchor)
                    .map(|index| {
                        self.selected = Some(index);
                        self.highlighted = Some(index);
                        vec![
                            PaletteEvent::SelectionChanged(index),
                            PaletteEvent::DismissRequested,
                        ]
                    })
                    .unwrap_or_default()
            }
            GestureEvent::PanChanged { position, .. } => {
                let next = self.option_at(position, viewport, compact_anchor);
                if self.highlighted == next {
                    Vec::new()
                } else {
                    self.highlighted = next;
                    vec![PaletteEvent::HighlightChanged(next)]
                }
            }
            GestureEvent::PanEnded { position, .. } => {
                let Some(index) = self.option_at(position, viewport, compact_anchor) else {
                    self.starting_selection = None;
                    return vec![PaletteEvent::DismissRequested];
                };
                self.selected = Some(index);
                self.highlighted = Some(index);
                self.starting_selection = None;
                vec![
                    PaletteEvent::SelectionChanged(index),
                    PaletteEvent::DismissRequested,
                ]
            }
            GestureEvent::Cancelled { .. } => {
                if let Some(selected) = self.starting_selection.take() {
                    self.selected = selected;
                    self.highlighted = selected;
                }
                vec![PaletteEvent::Cancelled]
            }
            GestureEvent::LongPressBegan { .. }
            | GestureEvent::LongPressEnded { .. }
            | GestureEvent::Swipe { .. }
            | GestureEvent::Released { .. }
            | GestureEvent::PanBegan { .. } => Vec::new(),
        }
    }

    fn option_at(&self, point: Point, viewport: Rect, compact_anchor: Rect) -> Option<usize> {
        self.placements(viewport, compact_anchor)
            .into_iter()
            .find_map(|placement| placement.bounds.contains(point).then_some(placement.index))
    }
}

fn localize(bounds: Rect, viewport: Rect) -> Rect {
    Rect::new(
        bounds.x - viewport.x,
        bounds.y - viewport.y,
        bounds.width,
        bounds.height,
    )
}

fn intersect(left: Rect, right: Rect) -> Rect {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = (left.x + left.width).min(right.x + right.width);
    let bottom = (left.y + left.height).min(right.y + right.height);
    Rect::new(x, y, (right_edge - x).max(0.0), (bottom - y).max(0.0))
}

fn event_id(event: GestureEvent) -> WidgetId {
    match event {
        GestureEvent::Pressed { id, .. }
        | GestureEvent::Tap { id, .. }
        | GestureEvent::LongPressBegan { id, .. }
        | GestureEvent::LongPressEnded { id, .. }
        | GestureEvent::PanBegan { id, .. }
        | GestureEvent::PanChanged { id, .. }
        | GestureEvent::PanEnded { id, .. }
        | GestureEvent::Swipe { id, .. }
        | GestureEvent::Released { id, .. }
        | GestureEvent::Cancelled { id, .. } => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: Rect = Rect::new(4.0, 4.0, 352.0, 52.0);

    #[test]
    fn left_anchor_stays_fixed_and_options_expand_right() {
        let palette = SelectionPalette::new(WidgetId(1), 5);
        let layout = palette.layout(VIEWPORT, Rect::new(0.0, 0.0, 80.0, 60.0));
        assert_eq!(layout.direction, ExpansionDirection::Trailing);
        assert_eq!(layout.activator.x, 4.0);
        assert_eq!(layout.activator.width, 76.0);
        let first = palette.placements(VIEWPORT, Rect::new(0.0, 0.0, 80.0, 60.0))[0];
        assert_eq!(first.bounds.x, 80.0);
    }

    #[test]
    fn right_anchor_stays_fixed_and_options_expand_left() {
        let palette = SelectionPalette::new(WidgetId(1), 4);
        let layout = palette.layout(VIEWPORT, Rect::new(280.0, 0.0, 80.0, 60.0));
        assert_eq!(layout.direction, ExpansionDirection::Leading);
        assert_eq!(layout.activator.x, 280.0);
        assert_eq!(
            palette.placements(VIEWPORT, Rect::new(280.0, 0.0, 80.0, 60.0))[0]
                .bounds
                .x,
            4.0
        );
    }

    #[test]
    fn tap_option_selects_and_requests_dismiss() {
        let id = WidgetId(2);
        let mut palette = SelectionPalette::new(id, 4);
        let events = palette.handle(
            GestureEvent::Tap {
                id,
                contact: 3,
                position: Point::new(120.0, 30.0),
            },
            VIEWPORT,
            Rect::new(0.0, 0.0, 80.0, 60.0),
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PaletteEvent::SelectionChanged(_)))
        );
        assert!(events.contains(&PaletteEvent::DismissRequested));
    }

    #[test]
    fn tap_activator_only_requests_dismiss() {
        let id = WidgetId(3);
        let mut palette = SelectionPalette::new(id, 4);
        assert_eq!(
            palette.handle(
                GestureEvent::Tap {
                    id,
                    contact: 1,
                    position: Point::new(30.0, 30.0),
                },
                VIEWPORT,
                Rect::new(0.0, 0.0, 80.0, 60.0),
            ),
            vec![PaletteEvent::DismissRequested]
        );
    }

    #[test]
    fn press_drag_highlights_then_commits_a_stationary_option() {
        let id = WidgetId(4);
        let anchor = Rect::new(0.0, 0.0, 80.0, 60.0);
        let mut palette = SelectionPalette::new(id, 5);
        palette.handle(
            GestureEvent::Pressed {
                id,
                contact: 1,
                position: Point::new(30.0, 30.0),
            },
            VIEWPORT,
            anchor,
        );
        assert_eq!(
            palette.handle(
                GestureEvent::PanChanged {
                    id,
                    contact: 1,
                    position: Point::new(250.0, 30.0),
                    translation: Point::new(220.0, 0.0),
                    delta: Point::new(220.0, 0.0),
                },
                VIEWPORT,
                anchor,
            ),
            vec![PaletteEvent::HighlightChanged(Some(3))]
        );
        let events = palette.handle(
            GestureEvent::PanEnded {
                id,
                contact: 1,
                position: Point::new(250.0, 30.0),
                translation: Point::new(220.0, 0.0),
                velocity: Point::new(400.0, 0.0),
            },
            VIEWPORT,
            anchor,
        );
        assert_eq!(palette.selected(), Some(3));
        assert_eq!(
            events,
            vec![
                PaletteEvent::SelectionChanged(3),
                PaletteEvent::DismissRequested
            ]
        );
    }
}
