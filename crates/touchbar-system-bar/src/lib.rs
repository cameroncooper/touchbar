//! Trusted baseline media and function-key layers shared by both daemons.
//!
//! This crate decides what a touch means but performs no I/O. The user-session
//! compositor renders the normal themed scene; `touchbard` can render the same
//! placements as its conservative fallback. Only the hardware daemon maps the
//! resulting typed keys to Linux input codes.

use std::collections::BTreeMap;

use touchbar_protocol::hardware_ipc::{KeyPhase, SystemKey, TouchEvent, TouchPhase};

mod render;

pub use render::SystemBarRenderer;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SystemLayer {
    #[default]
    Media,
    Function,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemBarConfig {
    pub default_layer: SystemLayer,
}

impl Default for SystemBarConfig {
    fn default() -> Self {
        Self {
            default_layer: SystemLayer::Media,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonVisual {
    Text(&'static str),
    Icon(SystemIcon),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemIcon {
    BrightnessDown,
    BrightnessUp,
    KeyboardIlluminationDown,
    KeyboardIlluminationUp,
    MicrophoneMute,
    Search,
    Previous,
    PlayPause,
    Next,
    Mute,
    VolumeDown,
    VolumeUp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SystemButton {
    pub key: SystemKey,
    pub visual: ButtonVisual,
    /// Rounded platform-style background, including its corner radius.
    pub visual_bounds: Rect,
    /// Full-height viewport used to center text and 48-pixel symbols.
    pub content_bounds: Rect,
    /// Conservative interaction band used by Tiny DFR on the 60-pixel strip.
    pub hit_bounds: Rect,
    pub pressed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyTransition {
    pub key: SystemKey,
    pub phase: KeyPhase,
}

#[derive(Clone, Copy)]
struct ContactState {
    key: SystemKey,
    hit_bounds: Rect,
    active: bool,
}

pub struct SystemBar {
    config: SystemBarConfig,
    active_layer: SystemLayer,
    fn_pressed: bool,
    width: f32,
    height: f32,
    contacts: BTreeMap<u32, ContactState>,
    pressed: BTreeMap<SystemKey, usize>,
}

impl SystemBar {
    pub fn new(config: SystemBarConfig, width: f32, height: f32) -> Self {
        Self {
            active_layer: config.default_layer,
            config,
            fn_pressed: false,
            width: width.max(1.0),
            height: height.max(1.0),
            contacts: BTreeMap::new(),
            pressed: BTreeMap::new(),
        }
    }

    pub fn active_layer(&self) -> SystemLayer {
        self.active_layer
    }

    pub fn fn_pressed(&self) -> bool {
        self.fn_pressed
    }

    pub fn buttons(&self) -> Vec<SystemButton> {
        let definitions = definitions(self.active_layer);
        // Match Tiny DFR's proven twelve-key geometry. Its 15% and 85%
        // coordinates are the centers of 8-pixel corner arcs, rather than the
        // top and bottom of a 42-pixel rectangle. Content is centered against
        // the complete strip and hit testing uses the separate 10%..90% band.
        let gap = 16.0;
        let outer = 0.0;
        let count = definitions.len() as f32;
        let available = (self.width - outer * 2.0 - gap * (count - 1.0)).max(count);
        let extent = available / count;
        let radius = 8.0_f32.min(self.height * 0.15);
        let arc_top = self.height * 0.15;
        let arc_bottom = self.height * 0.85;
        let visual_top = (arc_top - radius).max(0.0);
        let visual_height = (arc_bottom - arc_top + radius * 2.0).max(1.0);
        let hit_top = self.height * 0.10;
        let hit_height = (self.height * 0.80).max(1.0);
        definitions
            .iter()
            .enumerate()
            .map(|(index, definition)| {
                let x = (outer + index as f32 * (extent + gap)).floor();
                SystemButton {
                    key: definition.key,
                    visual: definition.visual,
                    visual_bounds: Rect {
                        x,
                        y: visual_top,
                        width: extent.ceil(),
                        height: visual_height,
                    },
                    content_bounds: Rect {
                        x,
                        y: 0.0,
                        width: extent.ceil(),
                        height: self.height,
                    },
                    hit_bounds: Rect {
                        x,
                        y: hit_top,
                        width: extent,
                        height: hit_height,
                    },
                    pressed: self.pressed.contains_key(&definition.key),
                }
            })
            .collect()
    }

    /// Fn is an exclusive transient layer, not a long-lived profile. Switching
    /// it cancels every active Touch Bar key before changing hit geometry.
    pub fn set_fn_pressed(&mut self, pressed: bool) -> Vec<KeyTransition> {
        self.set_fn_override(pressed.then(|| alternate(self.config.default_layer)))
    }

    /// Selects an explicit layer while Fn is physically held. The session
    /// compositor uses this for gestures such as double-tap-and-hold without
    /// moving timing or policy into the privileged hardware daemon.
    pub fn set_fn_override(&mut self, layer: Option<SystemLayer>) -> Vec<KeyTransition> {
        let pressed = layer.is_some();
        let next = layer.unwrap_or(self.config.default_layer);
        if self.fn_pressed == pressed && self.active_layer == next {
            return Vec::new();
        }
        let releases = self.cancel_all();
        self.fn_pressed = pressed;
        self.active_layer = next;
        releases
    }

    pub fn handle_touch(&mut self, event: TouchEvent) -> Vec<KeyTransition> {
        let x = event.x_millipixels as f32 / 1000.0;
        let y = event.y_millipixels as f32 / 1000.0;
        match event.phase {
            TouchPhase::Down => {
                if self.contacts.contains_key(&event.contact_id) {
                    return Vec::new();
                }
                let Some(button) = self
                    .buttons()
                    .into_iter()
                    .find(|button| button.hit_bounds.contains(x, y))
                else {
                    return Vec::new();
                };
                self.contacts.insert(
                    event.contact_id,
                    ContactState {
                        key: button.key,
                        hit_bounds: button.hit_bounds,
                        active: true,
                    },
                );
                self.activate(button.key)
            }
            TouchPhase::Motion => {
                let Some(state) = self.contacts.get_mut(&event.contact_id) else {
                    return Vec::new();
                };
                let inside = state.hit_bounds.contains(x, y);
                if inside == state.active {
                    return Vec::new();
                }
                state.active = inside;
                let key = state.key;
                if inside {
                    self.activate(key)
                } else {
                    self.deactivate(key)
                }
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                let Some(state) = self.contacts.remove(&event.contact_id) else {
                    return Vec::new();
                };
                if state.active {
                    self.deactivate(state.key)
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn cancel_all(&mut self) -> Vec<KeyTransition> {
        self.contacts.clear();
        let keys = self.pressed.keys().copied().collect::<Vec<_>>();
        self.pressed.clear();
        keys.into_iter()
            .map(|key| KeyTransition {
                key,
                phase: KeyPhase::Released,
            })
            .collect()
    }

    fn activate(&mut self, key: SystemKey) -> Vec<KeyTransition> {
        let count = self.pressed.entry(key).or_default();
        *count += 1;
        if *count == 1 {
            vec![KeyTransition {
                key,
                phase: KeyPhase::Pressed,
            }]
        } else {
            Vec::new()
        }
    }

    fn deactivate(&mut self, key: SystemKey) -> Vec<KeyTransition> {
        let Some(count) = self.pressed.get_mut(&key) else {
            return Vec::new();
        };
        *count -= 1;
        if *count == 0 {
            self.pressed.remove(&key);
            vec![KeyTransition {
                key,
                phase: KeyPhase::Released,
            }]
        } else {
            Vec::new()
        }
    }
}

#[derive(Clone, Copy)]
struct Definition {
    key: SystemKey,
    visual: ButtonVisual,
}

const MEDIA: [Definition; 12] = [
    icon(SystemKey::BrightnessDown, SystemIcon::BrightnessDown),
    icon(SystemKey::BrightnessUp, SystemIcon::BrightnessUp),
    icon(SystemKey::MicrophoneMute, SystemIcon::MicrophoneMute),
    icon(SystemKey::Search, SystemIcon::Search),
    icon(
        SystemKey::KeyboardIlluminationDown,
        SystemIcon::KeyboardIlluminationDown,
    ),
    icon(
        SystemKey::KeyboardIlluminationUp,
        SystemIcon::KeyboardIlluminationUp,
    ),
    icon(SystemKey::PreviousSong, SystemIcon::Previous),
    icon(SystemKey::PlayPause, SystemIcon::PlayPause),
    icon(SystemKey::NextSong, SystemIcon::Next),
    icon(SystemKey::Mute, SystemIcon::Mute),
    icon(SystemKey::VolumeDown, SystemIcon::VolumeDown),
    icon(SystemKey::VolumeUp, SystemIcon::VolumeUp),
];

const FUNCTION: [Definition; 12] = [
    text(SystemKey::F1, "F1"),
    text(SystemKey::F2, "F2"),
    text(SystemKey::F3, "F3"),
    text(SystemKey::F4, "F4"),
    text(SystemKey::F5, "F5"),
    text(SystemKey::F6, "F6"),
    text(SystemKey::F7, "F7"),
    text(SystemKey::F8, "F8"),
    text(SystemKey::F9, "F9"),
    text(SystemKey::F10, "F10"),
    text(SystemKey::F11, "F11"),
    text(SystemKey::F12, "F12"),
];

const fn icon(key: SystemKey, icon: SystemIcon) -> Definition {
    Definition {
        key,
        visual: ButtonVisual::Icon(icon),
    }
}

const fn text(key: SystemKey, label: &'static str) -> Definition {
    Definition {
        key,
        visual: ButtonVisual::Text(label),
    }
}

fn definitions(layer: SystemLayer) -> &'static [Definition] {
    match layer {
        SystemLayer::Media => &MEDIA,
        SystemLayer::Function => &FUNCTION,
    }
}

const fn alternate(layer: SystemLayer) -> SystemLayer {
    match layer {
        SystemLayer::Media => SystemLayer::Function,
        SystemLayer::Function => SystemLayer::Media,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn touch(phase: TouchPhase, contact_id: u32, x: f32) -> TouchEvent {
        TouchEvent {
            phase,
            contact_id,
            time_ms: 0,
            x_millipixels: (x * 1000.0) as i32,
            y_millipixels: 30_000,
        }
    }

    #[test]
    fn media_is_default_and_fn_is_an_exclusive_function_layer() {
        let mut bar = SystemBar::new(SystemBarConfig::default(), 2008.0, 60.0);
        assert_eq!(bar.active_layer(), SystemLayer::Media);
        assert_eq!(bar.buttons()[0].key, SystemKey::BrightnessDown);

        let first = bar.buttons()[0].hit_bounds;
        assert_eq!(
            bar.handle_touch(touch(TouchPhase::Down, 7, first.x + 1.0)),
            vec![KeyTransition {
                key: SystemKey::BrightnessDown,
                phase: KeyPhase::Pressed,
            }]
        );
        assert_eq!(
            bar.set_fn_pressed(true),
            vec![KeyTransition {
                key: SystemKey::BrightnessDown,
                phase: KeyPhase::Released,
            }]
        );
        assert_eq!(bar.active_layer(), SystemLayer::Function);
        assert_eq!(bar.buttons()[0].key, SystemKey::F1);
        assert!(
            bar.handle_touch(touch(TouchPhase::Up, 7, first.x + 1.0))
                .is_empty()
        );
        assert!(bar.set_fn_pressed(false).is_empty());
        assert_eq!(bar.active_layer(), SystemLayer::Media);
    }

    #[test]
    fn function_default_reverses_the_held_layer() {
        let mut bar = SystemBar::new(
            SystemBarConfig {
                default_layer: SystemLayer::Function,
            },
            2008.0,
            60.0,
        );
        assert_eq!(bar.active_layer(), SystemLayer::Function);
        bar.set_fn_pressed(true);
        assert_eq!(bar.active_layer(), SystemLayer::Media);
    }

    #[test]
    fn explicit_media_override_is_still_fn_held_and_restores_default() {
        let mut bar = SystemBar::new(SystemBarConfig::default(), 2008.0, 60.0);
        assert!(bar.set_fn_override(Some(SystemLayer::Media)).is_empty());
        assert!(bar.fn_pressed());
        assert_eq!(bar.active_layer(), SystemLayer::Media);
        assert!(bar.set_fn_override(None).is_empty());
        assert!(!bar.fn_pressed());
        assert_eq!(bar.active_layer(), SystemLayer::Media);
    }

    #[test]
    fn leaving_and_reentering_a_button_balances_key_state() {
        let mut bar = SystemBar::new(SystemBarConfig::default(), 2008.0, 60.0);
        let first = bar.buttons()[0].hit_bounds;
        assert_eq!(
            bar.handle_touch(touch(TouchPhase::Down, 1, first.x + 1.0))
                .len(),
            1
        );
        assert_eq!(
            bar.handle_touch(touch(TouchPhase::Motion, 1, first.x - 20.0))
                .len(),
            1
        );
        assert_eq!(
            bar.handle_touch(touch(TouchPhase::Motion, 1, first.x + 1.0))
                .len(),
            1
        );
        assert_eq!(
            bar.handle_touch(touch(TouchPhase::Up, 1, first.x + 1.0))
                .len(),
            1
        );
        assert!(bar.cancel_all().is_empty());
    }

    #[test]
    fn multiple_contacts_on_one_key_emit_one_balanced_pair() {
        let mut bar = SystemBar::new(SystemBarConfig::default(), 2008.0, 60.0);
        let first = bar.buttons()[0].hit_bounds;
        assert_eq!(
            bar.handle_touch(touch(TouchPhase::Down, 1, first.x + 1.0))
                .len(),
            1
        );
        assert!(
            bar.handle_touch(touch(TouchPhase::Down, 2, first.x + 2.0))
                .is_empty()
        );
        assert!(
            bar.handle_touch(touch(TouchPhase::Up, 1, first.x + 1.0))
                .is_empty()
        );
        assert_eq!(
            bar.handle_touch(touch(TouchPhase::Up, 2, first.x + 2.0))
                .len(),
            1
        );
    }

    #[test]
    fn both_layers_have_twelve_unique_keys_with_valid_geometry() {
        for default_layer in [SystemLayer::Media, SystemLayer::Function] {
            let bar = SystemBar::new(SystemBarConfig { default_layer }, 2008.0, 60.0);
            let buttons = bar.buttons();
            assert_eq!(buttons.len(), 12);
            assert_eq!(
                buttons
                    .iter()
                    .map(|button| button.key)
                    .collect::<BTreeSet<_>>()
                    .len(),
                12
            );
            assert!(buttons.iter().all(|button| {
                button.visual_bounds.x >= 0.0
                    && button.visual_bounds.width > 0.0
                    && button.visual_bounds.x + button.visual_bounds.width <= 2008.0
                    && button.visual_bounds.y == 1.0
                    && button.visual_bounds.height == 58.0
                    && button.content_bounds.y == 0.0
                    && button.content_bounds.height == 60.0
                    && button.hit_bounds.y == 6.0
                    && button.hit_bounds.height == 48.0
            }));
        }
    }

    #[test]
    fn twelve_key_horizontal_geometry_matches_tiny_dfr_rounding() {
        let bar = SystemBar::new(SystemBarConfig::default(), 2008.0, 60.0);
        let buttons = bar.buttons();
        assert_eq!(buttons[0].visual_bounds.x, 0.0);
        assert_eq!(buttons[0].visual_bounds.width, 153.0);
        assert_eq!(buttons[1].visual_bounds.x, 168.0);
        assert_eq!(buttons[11].visual_bounds.x, 1855.0);
        assert_eq!(
            buttons[11].visual_bounds.x + buttons[11].visual_bounds.width,
            2008.0
        );
        assert!(buttons[0].hit_bounds.width > 152.0);
        assert!(buttons[0].hit_bounds.width < 153.0);
    }
}
