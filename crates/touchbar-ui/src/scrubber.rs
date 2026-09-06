use std::ops::Range;

use crate::{
    GestureAxis, GestureEvent, GestureRecognizer, GestureTarget, Node, Rect, Size, WidgetId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrubberMovement {
    Free,
    SnapToItem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrubberSelection {
    Continuous,
    OnRelease,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrubberItemState {
    pub selected: bool,
    pub highlighted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrubberPlacement {
    pub index: usize,
    pub frame: Rect,
    pub state: ScrubberItemState,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrubberEvent {
    OffsetChanged(f32),
    SelectionChanged(usize),
    InteractionEnded,
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
struct DragSnapshot {
    offset: f32,
    selected: Option<usize>,
    highlighted: Option<usize>,
}

/// A horizontally scrolling, fixed-extent collection that only instantiates
/// visible items. Plugins retain their data and provide item nodes lazily.
#[derive(Clone, Debug)]
pub struct Scrubber {
    id: WidgetId,
    item_count: usize,
    item_extent: f32,
    gap: f32,
    movement: ScrubberMovement,
    selection: ScrubberSelection,
    offset: f32,
    selected: Option<usize>,
    highlighted: Option<usize>,
    overscan: usize,
    drag: Option<DragSnapshot>,
}

impl Scrubber {
    pub fn new(id: WidgetId, item_count: usize, item_extent: f32) -> Self {
        Self {
            id,
            item_count,
            item_extent: item_extent.max(1.0),
            gap: 4.0,
            movement: ScrubberMovement::Free,
            selection: ScrubberSelection::OnRelease,
            offset: 0.0,
            selected: None,
            highlighted: None,
            overscan: 1,
            drag: None,
        }
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap.max(0.0);
        self
    }

    pub fn movement(mut self, movement: ScrubberMovement) -> Self {
        self.movement = movement;
        self
    }

    pub fn selection_behavior(mut self, selection: ScrubberSelection) -> Self {
        self.selection = selection;
        self
    }

    pub fn overscan(mut self, items: usize) -> Self {
        self.overscan = items;
        self
    }

    pub fn id(&self) -> WidgetId {
        self.id
    }

    pub fn offset(&self) -> f32 {
        self.offset
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn set_selected(&mut self, index: Option<usize>, viewport_width: f32) {
        self.selected = index.filter(|index| *index < self.item_count);
        self.highlighted = self.selected;
        if self.movement == ScrubberMovement::SnapToItem
            && let Some(index) = self.selected
        {
            self.offset = self.centered_offset(index, viewport_width);
        }
    }

    pub fn set_item_count(&mut self, item_count: usize, viewport_width: f32) {
        self.item_count = item_count;
        self.selected = self.selected.filter(|index| *index < item_count);
        self.highlighted = self.highlighted.filter(|index| *index < item_count);
        self.offset = self.offset.clamp(0.0, self.maximum_offset(viewport_width));
    }

    pub fn gesture_target(&self, bounds: Rect) -> GestureTarget {
        GestureTarget::new(
            self.id,
            bounds,
            [
                GestureRecognizer::tap(),
                GestureRecognizer::pan(GestureAxis::Horizontal),
                GestureRecognizer::swipe(GestureAxis::Horizontal),
            ],
        )
    }

    pub fn visible_range(&self, viewport_width: f32) -> Range<usize> {
        if self.item_count == 0 || viewport_width <= 0.0 {
            return 0..0;
        }
        let stride = self.stride();
        let edge = self.edge_inset(viewport_width);
        let first_visible = ((self.offset - edge) / stride).floor().max(0.0) as usize;
        let last_visible = (((self.offset + viewport_width - edge) / stride).ceil() as usize + 1)
            .min(self.item_count);
        first_visible.saturating_sub(self.overscan)
            ..last_visible
                .saturating_add(self.overscan)
                .min(self.item_count)
    }

    pub fn placements(&self, viewport: Size) -> Vec<ScrubberPlacement> {
        let edge = self.edge_inset(viewport.width);
        self.visible_range(viewport.width)
            .map(|index| ScrubberPlacement {
                index,
                frame: Rect::new(
                    edge + index as f32 * self.stride() - self.offset,
                    0.0,
                    self.item_extent,
                    viewport.height,
                ),
                state: ScrubberItemState {
                    selected: self.selected == Some(index),
                    highlighted: self.highlighted == Some(index),
                },
            })
            .collect()
    }

    /// Build a clipped retained node from only the current visible range.
    pub fn compose(
        &self,
        viewport: Size,
        mut item: impl FnMut(usize, ScrubberItemState) -> Node,
    ) -> Node {
        Node::Layer(
            self.placements(viewport)
                .into_iter()
                .map(|placement| {
                    Node::positioned(placement.frame, item(placement.index, placement.state))
                })
                .collect(),
        )
    }

    pub fn handle(&mut self, event: GestureEvent, viewport: Rect) -> Vec<ScrubberEvent> {
        if event_id(event) != self.id {
            return Vec::new();
        }
        let viewport_width = viewport.width;
        match event {
            GestureEvent::Pressed { .. } => Vec::new(),
            GestureEvent::Tap { position, .. } => {
                let Some(index) = self.item_at(position.x - viewport.x, viewport_width) else {
                    return Vec::new();
                };
                self.commit_selection(index, viewport_width)
            }
            GestureEvent::PanBegan { .. } => {
                self.drag = Some(DragSnapshot {
                    offset: self.offset,
                    selected: self.selected,
                    highlighted: self.highlighted,
                });
                Vec::new()
            }
            GestureEvent::PanChanged {
                position,
                translation,
                ..
            } => {
                let origin = self.drag.map_or(self.offset, |drag| drag.offset);
                self.offset =
                    (origin - translation.x).clamp(0.0, self.maximum_offset(viewport_width));
                let index = match self.movement {
                    ScrubberMovement::Free => self.item_at(position.x - viewport.x, viewport_width),
                    ScrubberMovement::SnapToItem => self.nearest_to_center(viewport_width),
                };
                self.highlighted = index;
                let mut events = vec![ScrubberEvent::OffsetChanged(self.offset)];
                if self.selection == ScrubberSelection::Continuous
                    && let Some(index) = index
                    && self.selected != Some(index)
                {
                    self.selected = Some(index);
                    events.push(ScrubberEvent::SelectionChanged(index));
                }
                events
            }
            GestureEvent::PanEnded { .. } => {
                let mut events = Vec::new();
                if let Some(index) = self.highlighted {
                    if self.selection == ScrubberSelection::OnRelease
                        && self.selected != Some(index)
                    {
                        self.selected = Some(index);
                        events.push(ScrubberEvent::SelectionChanged(index));
                    }
                    if self.movement == ScrubberMovement::SnapToItem {
                        self.offset = self.centered_offset(index, viewport_width);
                        events.push(ScrubberEvent::OffsetChanged(self.offset));
                    }
                }
                self.drag = None;
                events.push(ScrubberEvent::InteractionEnded);
                events
            }
            GestureEvent::Cancelled { .. } => {
                if let Some(drag) = self.drag.take() {
                    self.offset = drag.offset;
                    self.selected = drag.selected;
                    self.highlighted = drag.highlighted;
                }
                vec![ScrubberEvent::Cancelled]
            }
            GestureEvent::LongPressBegan { .. }
            | GestureEvent::LongPressEnded { .. }
            | GestureEvent::Swipe { .. }
            | GestureEvent::Released { .. } => Vec::new(),
        }
    }

    fn commit_selection(&mut self, index: usize, viewport_width: f32) -> Vec<ScrubberEvent> {
        self.selected = Some(index);
        self.highlighted = Some(index);
        let mut events = vec![ScrubberEvent::SelectionChanged(index)];
        if self.movement == ScrubberMovement::SnapToItem {
            self.offset = self.centered_offset(index, viewport_width);
            events.push(ScrubberEvent::OffsetChanged(self.offset));
        }
        events
    }

    fn item_at(&self, local_x: f32, viewport_width: f32) -> Option<usize> {
        let content_x = local_x + self.offset - self.edge_inset(viewport_width);
        if content_x < 0.0 {
            return None;
        }
        let index = (content_x / self.stride()).floor() as usize;
        let within_item = content_x - index as f32 * self.stride() <= self.item_extent;
        (index < self.item_count && within_item).then_some(index)
    }

    fn nearest_to_center(&self, viewport_width: f32) -> Option<usize> {
        if self.item_count == 0 {
            return None;
        }
        let edge = self.edge_inset(viewport_width);
        let content_center = viewport_width * 0.5 + self.offset - edge;
        let index = ((content_center - self.item_extent * 0.5) / self.stride())
            .round()
            .clamp(0.0, self.item_count.saturating_sub(1) as f32) as usize;
        Some(index)
    }

    fn centered_offset(&self, index: usize, viewport_width: f32) -> f32 {
        let edge = self.edge_inset(viewport_width);
        (edge + index as f32 * self.stride() + self.item_extent * 0.5 - viewport_width * 0.5)
            .clamp(0.0, self.maximum_offset(viewport_width))
    }

    fn maximum_offset(&self, viewport_width: f32) -> f32 {
        (self.content_width(viewport_width) - viewport_width).max(0.0)
    }

    fn content_width(&self, viewport_width: f32) -> f32 {
        if self.item_count == 0 {
            return 0.0;
        }
        self.edge_inset(viewport_width) * 2.0
            + self.item_count as f32 * self.item_extent
            + self.item_count.saturating_sub(1) as f32 * self.gap
    }

    fn edge_inset(&self, viewport_width: f32) -> f32 {
        if self.movement == ScrubberMovement::SnapToItem {
            ((viewport_width - self.item_extent) * 0.5).max(0.0)
        } else {
            0.0
        }
    }

    fn stride(&self) -> f32 {
        self.item_extent + self.gap
    }
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
    use crate::Point;

    fn viewport() -> Rect {
        Rect::new(0.0, 0.0, 120.0, 60.0)
    }

    fn pan_event(id: WidgetId, translation_x: f32) -> GestureEvent {
        GestureEvent::PanChanged {
            id,
            contact: 1,
            position: Point::new(60.0, 20.0),
            translation: Point::new(translation_x, 0.0),
            delta: Point::new(translation_x, 0.0),
        }
    }

    #[test]
    fn only_visible_items_are_instantiated() {
        let scrubber = Scrubber::new(WidgetId(1), 10_000, 40.0).gap(4.0);
        let placements = scrubber.placements(Size::new(200.0, 60.0));
        assert!(placements.len() <= 7);
        assert_eq!(placements[0].index, 0);
    }

    #[test]
    fn pan_updates_offset_and_continuous_selection() {
        let id = WidgetId(2);
        let mut scrubber = Scrubber::new(id, 20, 40.0)
            .movement(ScrubberMovement::SnapToItem)
            .selection_behavior(ScrubberSelection::Continuous);
        scrubber.handle(
            GestureEvent::PanBegan {
                id,
                contact: 1,
                position: Point::new(60.0, 20.0),
            },
            viewport(),
        );
        let events = scrubber.handle(pan_event(id, -90.0), viewport());
        assert!(scrubber.offset() > 0.0);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ScrubberEvent::SelectionChanged(_)))
        );
    }

    #[test]
    fn cancelled_pan_restores_state() {
        let id = WidgetId(3);
        let mut scrubber = Scrubber::new(id, 20, 40.0);
        scrubber.handle(
            GestureEvent::PanBegan {
                id,
                contact: 1,
                position: Point::new(60.0, 20.0),
            },
            viewport(),
        );
        scrubber.handle(pan_event(id, -90.0), viewport());
        scrubber.handle(GestureEvent::Cancelled { id, contact: 1 }, viewport());
        assert_eq!(scrubber.offset(), 0.0);
        assert_eq!(scrubber.selected(), None);
    }

    #[test]
    fn snapping_can_center_first_and_last_items() {
        let mut scrubber =
            Scrubber::new(WidgetId(4), 5, 40.0).movement(ScrubberMovement::SnapToItem);
        scrubber.set_selected(Some(0), 120.0);
        assert_eq!(scrubber.offset(), 0.0);
        scrubber.set_selected(Some(4), 120.0);
        assert_eq!(scrubber.offset(), 176.0);
    }

    #[test]
    fn compose_calls_builder_for_visible_items_only() {
        let scrubber = Scrubber::new(WidgetId(5), 1_000, 40.0);
        let mut built = 0;
        let _node = scrubber.compose(Size::new(160.0, 60.0), |index, _| {
            built += 1;
            Node::label(index.to_string(), 12.0)
        });
        assert!(built <= 6);
    }

    #[test]
    fn gesture_target_includes_tap_pan_and_swipe() {
        let scrubber = Scrubber::new(WidgetId(6), 10, 40.0);
        assert_eq!(
            scrubber
                .gesture_target(Rect::new(0.0, 0.0, 100.0, 60.0))
                .recognizers
                .len(),
            3
        );
    }
}
