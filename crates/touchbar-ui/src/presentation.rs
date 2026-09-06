use std::{collections::VecDeque, time::Duration};

use crate::Rect;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationSessionId(pub u32);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentationPolicy {
    Anchored,
    InPlace,
    Slot(String),
    Region(String),
    FullBar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationLifecycle {
    Transient { contact: u32 },
    Persistent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DismissReason {
    Requested,
    Selection,
    OutsidePress,
    Timeout,
    SourceHidden,
    Replaced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DismissalPolicy {
    pub on_release: bool,
    pub on_selection: bool,
    pub on_outside_press: bool,
    pub timeout: Option<Duration>,
    pub back_at_root: bool,
}

impl DismissalPolicy {
    pub const fn transient() -> Self {
        Self {
            on_release: true,
            on_selection: true,
            on_outside_press: false,
            timeout: None,
            back_at_root: true,
        }
    }

    pub const fn persistent(timeout: Option<Duration>) -> Self {
        Self {
            on_release: false,
            on_selection: true,
            on_outside_press: true,
            timeout,
            back_at_root: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentationCommand {
    Present {
        session: PresentationSessionId,
        policy: PresentationPolicy,
        lifecycle: PresentationLifecycle,
    },
    Dismiss {
        session: PresentationSessionId,
        reason: DismissReason,
    },
}

#[derive(Clone, Debug)]
pub struct PresentationSession<Page> {
    id: PresentationSessionId,
    policy: PresentationPolicy,
    lifecycle: PresentationLifecycle,
    dismissal: DismissalPolicy,
    anchor: Option<Rect>,
    pages: Vec<Page>,
    deadline: Option<Duration>,
    dismiss_requested: bool,
}

impl<Page> PresentationSession<Page> {
    pub fn id(&self) -> PresentationSessionId {
        self.id
    }

    pub fn policy(&self) -> &PresentationPolicy {
        &self.policy
    }

    pub fn lifecycle(&self) -> PresentationLifecycle {
        self.lifecycle
    }

    pub fn anchor(&self) -> Option<Rect> {
        self.anchor
    }

    pub fn current_page(&self) -> &Page {
        self.pages
            .last()
            .expect("a presentation always has a root page")
    }

    pub fn depth(&self) -> usize {
        self.pages.len()
    }

    pub fn dismiss_requested(&self) -> bool {
        self.dismiss_requested
    }
}

/// Plugin-side presentation and navigation state. It emits declarative
/// commands while keeping page state, timing, and stale-session checks local.
#[derive(Clone, Debug)]
pub struct PresentationController<Page> {
    next_session: u32,
    active: Option<PresentationSession<Page>>,
    commands: VecDeque<PresentationCommand>,
}

impl<Page> Default for PresentationController<Page> {
    fn default() -> Self {
        Self {
            next_session: 1,
            active: None,
            commands: VecDeque::new(),
        }
    }
}

impl<Page> PresentationController<Page> {
    pub fn present(
        &mut self,
        policy: PresentationPolicy,
        lifecycle: PresentationLifecycle,
        dismissal: DismissalPolicy,
        root: Page,
        now: Duration,
    ) -> PresentationSessionId {
        if let Some(previous) = self.active.take() {
            self.commands.push_back(PresentationCommand::Dismiss {
                session: previous.id,
                reason: DismissReason::Replaced,
            });
        }
        let id = PresentationSessionId(self.next_session.max(1));
        self.next_session = self.next_session.wrapping_add(1).max(1);
        let deadline = dismissal.timeout.map(|timeout| now.saturating_add(timeout));
        self.active = Some(PresentationSession {
            id,
            policy: policy.clone(),
            lifecycle,
            dismissal,
            anchor: None,
            pages: vec![root],
            deadline,
            dismiss_requested: false,
        });
        self.commands.push_back(PresentationCommand::Present {
            session: id,
            policy,
            lifecycle,
        });
        id
    }

    pub fn active(&self) -> Option<&PresentationSession<Page>> {
        self.active.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn set_anchor(&mut self, session: PresentationSessionId, anchor: Rect) -> bool {
        let Some(active) = self.active.as_mut().filter(|active| active.id == session) else {
            return false;
        };
        active.anchor = Some(anchor);
        true
    }

    pub fn push(&mut self, page: Page) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        active.pages.push(page);
        true
    }

    pub fn back(&mut self) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        if active.pages.len() > 1 {
            active.pages.pop();
            true
        } else if active.dismissal.back_at_root {
            self.dismiss(DismissReason::Requested)
        } else {
            false
        }
    }

    pub fn released(&mut self, contact: u32) -> bool {
        let should_dismiss = self.active.as_ref().is_some_and(|active| {
            active.dismissal.on_release
                && active.lifecycle == PresentationLifecycle::Transient { contact }
        });
        should_dismiss && self.dismiss(DismissReason::Requested)
    }

    pub fn selected(&mut self) -> bool {
        let should_dismiss = self
            .active
            .as_ref()
            .is_some_and(|active| active.dismissal.on_selection);
        should_dismiss && self.dismiss(DismissReason::Selection)
    }

    pub fn outside_pressed(&mut self) -> bool {
        let should_dismiss = self
            .active
            .as_ref()
            .is_some_and(|active| active.dismissal.on_outside_press);
        should_dismiss && self.dismiss(DismissReason::OutsidePress)
    }

    pub fn tick(&mut self, now: Duration) -> bool {
        let timed_out = self
            .active
            .as_ref()
            .and_then(|active| active.deadline)
            .is_some_and(|deadline| now >= deadline);
        timed_out && self.dismiss(DismissReason::Timeout)
    }

    pub fn activity(&mut self, now: Duration) {
        let Some(active) = &mut self.active else {
            return;
        };
        active.deadline = active
            .dismissal
            .timeout
            .map(|timeout| now.saturating_add(timeout));
    }

    pub fn dismiss_if_current(
        &mut self,
        session: PresentationSessionId,
        reason: DismissReason,
    ) -> bool {
        if self.active.as_ref().map(|active| active.id) != Some(session) {
            return false;
        }
        self.dismiss(reason)
    }

    pub fn compositor_dismissed(&mut self, session: PresentationSessionId) -> bool {
        if self.active.as_ref().map(|active| active.id) != Some(session) {
            return false;
        }
        self.active = None;
        true
    }

    pub fn take_command(&mut self) -> Option<PresentationCommand> {
        self.commands.pop_front()
    }

    fn dismiss(&mut self, reason: DismissReason) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.dismiss_requested {
            return false;
        }
        active.dismiss_requested = true;
        self.commands.push_back(PresentationCommand::Dismiss {
            session: active.id,
            reason,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Page {
        Root,
        Detail,
    }

    #[test]
    fn nested_back_pops_then_dismisses_at_root() {
        let mut controller = PresentationController::default();
        let session = controller.present(
            PresentationPolicy::Anchored,
            PresentationLifecycle::Persistent,
            DismissalPolicy::persistent(None),
            Page::Root,
            Duration::ZERO,
        );
        assert!(controller.push(Page::Detail));
        assert_eq!(controller.active().unwrap().depth(), 2);
        assert!(controller.back());
        assert_eq!(controller.active().unwrap().current_page(), &Page::Root);
        assert!(controller.back());
        assert!(controller.active().unwrap().dismiss_requested());
        assert_eq!(
            controller.take_command(),
            Some(PresentationCommand::Present {
                session,
                policy: PresentationPolicy::Anchored,
                lifecycle: PresentationLifecycle::Persistent,
            })
        );
        assert_eq!(
            controller.take_command(),
            Some(PresentationCommand::Dismiss {
                session,
                reason: DismissReason::Requested,
            })
        );
    }

    #[test]
    fn transient_release_only_matches_its_contact() {
        let mut controller = PresentationController::default();
        controller.present(
            PresentationPolicy::Anchored,
            PresentationLifecycle::Transient { contact: 7 },
            DismissalPolicy::transient(),
            Page::Root,
            Duration::ZERO,
        );
        assert!(!controller.released(8));
        assert!(controller.released(7));
        assert!(controller.active().unwrap().dismiss_requested());
    }

    #[test]
    fn timeout_and_stale_session_are_safe() {
        let mut controller = PresentationController::default();
        let old = controller.present(
            PresentationPolicy::Anchored,
            PresentationLifecycle::Persistent,
            DismissalPolicy::persistent(Some(Duration::from_secs(4))),
            Page::Root,
            Duration::from_secs(2),
        );
        let current = controller.present(
            PresentationPolicy::FullBar,
            PresentationLifecycle::Persistent,
            DismissalPolicy::persistent(Some(Duration::from_secs(4))),
            Page::Root,
            Duration::from_secs(3),
        );
        assert!(!controller.dismiss_if_current(old, DismissReason::Requested));
        assert_eq!(controller.active().unwrap().id(), current);
        assert!(!controller.tick(Duration::from_secs(6)));
        assert!(controller.tick(Duration::from_secs(7)));
        assert!(controller.active().unwrap().dismiss_requested());
    }

    #[test]
    fn stale_anchor_does_not_move_current_session() {
        let mut controller = PresentationController::default();
        let old = controller.present(
            PresentationPolicy::Anchored,
            PresentationLifecycle::Persistent,
            DismissalPolicy::persistent(None),
            Page::Root,
            Duration::ZERO,
        );
        let current = controller.present(
            PresentationPolicy::Anchored,
            PresentationLifecycle::Persistent,
            DismissalPolicy::persistent(None),
            Page::Root,
            Duration::ZERO,
        );
        assert!(!controller.set_anchor(old, Rect::new(1.0, 0.0, 40.0, 60.0)));
        assert!(controller.set_anchor(current, Rect::new(80.0, 0.0, 40.0, 60.0)));
        assert_eq!(controller.active().unwrap().anchor().unwrap().x, 80.0);
    }
}
