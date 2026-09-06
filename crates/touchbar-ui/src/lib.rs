//! A small, optional GPU UI kit for Touch Bar plugin processes.
//!
//! The scene and interaction model remain plugin-side. `touchbar-sessiond` receives
//! pixels and input protocol messages, never a privileged widget tree.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

mod asset;
mod controls;
mod effect;
mod foundation;
mod gesture;
mod palette;
mod presentation;
mod scrubber;
mod symbols;
mod text;

pub use asset::{SvgAsset, SvgRasterizer};
pub use controls::{ContinuousValue, MeterStyle, ScrubberCellStyle, SliderStyle, TinyGraphStyle};
pub use effect::{
    EffectProgram, MAX_EFFECT_NODES, MAX_EFFECT_PARAMETERS, MAX_EFFECT_PROGRAMS,
    MAX_EFFECT_SOURCE_BYTES, ShaderEffect,
};
pub use foundation::{
    CanvasColor, CanvasCommand, CanvasLinearGradient, CanvasPaint, ColorRole, CrossAxisAlignment,
    Flex, FlexItem, FrameScheduler, Icon, ImageFit, ImageTint, InspectorSnapshot, LayoutMeasurer,
    Node, PressableStyle, ProgressValue, Representation, ResolvedUi, ResponsiveVariant, RetainedUi,
    SemanticNode, SemanticRole, TextAlign, TextMeasurement, resolve_flex_row,
};
pub use gesture::{
    GestureArena, GestureAxis, GestureEvent, GestureMap, GestureRecognizer, GestureTarget,
};
pub use palette::{
    ExpansionDirection, PaletteEvent, PaletteLayout, PaletteOptionPlacement, SelectionPalette,
};
pub use presentation::{
    DismissReason, DismissalPolicy, PresentationCommand, PresentationController,
    PresentationLifecycle, PresentationPolicy, PresentationSession, PresentationSessionId,
};
pub use scrubber::{
    Scrubber, ScrubberEvent, ScrubberItemState, ScrubberMovement, ScrubberPlacement,
    ScrubberSelection,
};
pub use symbols::{Symbol, SymbolCatalog};
pub use text::TextEngine;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MotionId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisualTransform {
    pub translation: Point,
    pub scale: f32,
    pub opacity: f32,
}

impl VisualTransform {
    pub const IDENTITY: Self = Self {
        translation: Point::new(0.0, 0.0),
        scale: 1.0,
        opacity: 1.0,
    };
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Easing {
    Linear,
    #[default]
    EaseInOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionPolicy {
    Full,
    Reduced,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MotionPlayback {
    #[default]
    Once,
    Loop,
    Alternate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    pub id: MotionId,
    pub from: VisualTransform,
    pub to: VisualTransform,
    pub started: Duration,
    pub duration: Duration,
    pub easing: Easing,
    pub playback: MotionPlayback,
}

impl Motion {
    pub fn sample(self, now: Duration, policy: MotionPolicy) -> VisualTransform {
        if policy != MotionPolicy::Full || self.duration.is_zero() {
            return self.to;
        }
        let elapsed = now.saturating_sub(self.started).as_secs_f32();
        let duration = self.duration.as_secs_f32();
        let linear = match self.playback {
            MotionPlayback::Once => (elapsed / duration).clamp(0.0, 1.0),
            MotionPlayback::Loop => (elapsed / duration).rem_euclid(1.0),
            MotionPlayback::Alternate => {
                let cycle = elapsed / duration;
                let fraction = cycle.rem_euclid(1.0);
                if (cycle.floor() as u64).is_multiple_of(2) {
                    fraction
                } else {
                    1.0 - fraction
                }
            }
        };
        let amount = match self.easing {
            Easing::Linear => linear,
            Easing::EaseInOut => linear * linear * (3.0 - 2.0 * linear),
        };
        VisualTransform {
            translation: Point::new(
                self.from.translation.x
                    + (self.to.translation.x - self.from.translation.x) * amount,
                self.from.translation.y
                    + (self.to.translation.y - self.from.translation.y) * amount,
            ),
            scale: self.from.scale + (self.to.scale - self.from.scale) * amount,
            opacity: self.from.opacity + (self.to.opacity - self.from.opacity) * amount,
        }
    }

    pub fn is_active(self, now: Duration, policy: MotionPolicy) -> bool {
        if policy != MotionPolicy::Full || self.duration.is_zero() {
            return false;
        }
        match self.playback {
            MotionPlayback::Once => now < self.started.saturating_add(self.duration),
            MotionPlayback::Loop | MotionPlayback::Alternate => true,
        }
    }
}

impl Size {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.x <= self.x + self.width
            && point.y >= self.y
            && point.y <= self.y + self.height
    }

    pub fn inset(self, amount: f32) -> Self {
        Self::new(
            self.x + amount,
            self.y + amount,
            (self.width - amount * 2.0).max(0.0),
            (self.height - amount * 2.0).max(0.0),
        )
    }

    pub const fn size(self) -> Size {
        Size::new(self.width, self.height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl Color {
    pub const TRANSPARENT: Self = Self::rgba(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Self = Self::rgb(0.0, 0.0, 0.0);
    pub const WHITE: Self = Self::rgb(1.0, 1.0, 1.0);

    pub const fn rgb(red: f32, green: f32, blue: f32) -> Self {
        Self {
            red,
            green,
            blue,
            alpha: 1.0,
        }
    }

    pub const fn rgba(red: f32, green: f32, blue: f32, alpha: f32) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    pub fn mix(self, other: Self, amount: f32) -> Self {
        let t = amount.clamp(0.0, 1.0);
        Self {
            red: self.red + (other.red - self.red) * t,
            green: self.green + (other.green - self.green) * t,
            blue: self.blue + (other.blue - self.blue) * t,
            alpha: self.alpha + (other.alpha - self.alpha) * t,
        }
    }

    pub fn premultiplied(self) -> Self {
        let alpha = self.alpha.clamp(0.0, 1.0);
        Self {
            red: self.red.clamp(0.0, 1.0) * alpha,
            green: self.green.clamp(0.0, 1.0) * alpha,
            blue: self.blue.clamp(0.0, 1.0) * alpha,
            alpha,
        }
    }

    /// Choose black or white text using WCAG relative luminance contrast.
    pub fn contrasting_foreground(self) -> Self {
        let channel = |value: f32| {
            let value = value.clamp(0.0, 1.0);
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance =
            0.2126 * channel(self.red) + 0.7152 * channel(self.green) + 0.0722 * channel(self.blue);
        let white_contrast = 1.05 / (luminance + 0.05);
        let black_contrast = (luminance + 0.05) / 0.05;
        if black_contrast >= white_contrast {
            Self::BLACK
        } else {
            Self::WHITE
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Primitive {
    RoundedRect {
        rect: Rect,
        radius: f32,
        color: Color,
    },
    Text {
        rect: Rect,
        text: String,
        size: f32,
        color: Color,
        align: TextAlign,
        overflow: TextOverflow,
    },
    Icon {
        rect: Rect,
        icon: Icon,
        color: Color,
    },
    Image {
        rect: Rect,
        image: Image,
        opacity: f32,
        coloring: ImageColoring,
    },
    PushOpacity {
        opacity: f32,
    },
    PopOpacity,
    PushMotion {
        origin: Point,
        motion: Motion,
    },
    PopMotion,
    CustomGles {
        id: CustomGlesId,
        rect: Rect,
        theme: Theme,
    },
    ShaderEffect {
        effect: ShaderEffect,
        rect: Rect,
        theme: Theme,
    },
    Line {
        start: Point,
        end: Point,
        width: f32,
        color: Color,
    },
    LinearGradientRect {
        rect: Rect,
        radius: f32,
        start: Point,
        end: Point,
        start_color: Color,
        end_color: Color,
    },
    TriangleMesh {
        triangles: Arc<[Point]>,
        color: Color,
    },
    PushClip {
        rect: Rect,
    },
    PopClip,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TextOverflow {
    #[default]
    Ellipsis,
    Clip,
    /// Scroll overflowing single-line text without changing layout.
    Marquee {
        speed: f32,
        gap: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageColoring {
    Original,
    Multiply(Color),
    Mask(Color),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CustomGlesId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomGlesFrame {
    /// Resolved local bounds in top-left UI coordinates.
    pub bounds: Rect,
    /// Effective rectangular clip after intersecting all parent containers.
    pub clip: Rect,
    pub surface_size: Size,
    pub theme: Theme,
    /// Product of all enclosing opacity nodes.
    pub opacity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub id: u64,
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[u8]>,
}

impl Image {
    pub fn rgba8(
        id: u64,
        revision: u64,
        width: u32,
        height: u32,
        pixels: impl Into<Arc<[u8]>>,
    ) -> anyhow::Result<Self> {
        let pixels = pixels.into();
        let expected = width as usize * height as usize * 4;
        if width == 0 || height == 0 || pixels.len() != expected {
            anyhow::bail!(
                "RGBA image dimensions require {expected} bytes, received {}",
                pixels.len()
            );
        }
        Ok(Self {
            id,
            revision,
            width,
            height,
            pixels,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub clear: Color,
    pub primitives: Vec<Primitive>,
}

impl Scene {
    pub fn new(clear: Color) -> Self {
        Self {
            clear,
            primitives: Vec::new(),
        }
    }

    pub fn rounded_rect(&mut self, rect: Rect, radius: f32, color: Color) {
        if rect.width > 0.0 && rect.height > 0.0 && color.alpha > 0.0 {
            self.primitives.push(Primitive::RoundedRect {
                rect,
                radius: radius.max(0.0).min(rect.width.min(rect.height) * 0.5),
                color,
            });
        }
    }

    pub fn text(
        &mut self,
        rect: Rect,
        text: impl Into<String>,
        size: f32,
        color: Color,
        align: TextAlign,
    ) {
        self.styled_text(rect, text, size, color, align, TextOverflow::Ellipsis);
    }

    pub fn styled_text(
        &mut self,
        rect: Rect,
        text: impl Into<String>,
        size: f32,
        color: Color,
        align: TextAlign,
        overflow: TextOverflow,
    ) {
        let text = text.into();
        if rect.width > 0.0
            && rect.height > 0.0
            && size > 0.0
            && color.alpha > 0.0
            && !text.is_empty()
        {
            self.primitives.push(Primitive::Text {
                rect,
                text,
                size,
                color,
                align,
                overflow,
            });
        }
    }

    pub fn icon(&mut self, rect: Rect, icon: Icon, color: Color) {
        if rect.width > 0.0 && rect.height > 0.0 && color.alpha > 0.0 {
            self.primitives.push(Primitive::Icon { rect, icon, color });
        }
    }

    pub fn image(&mut self, rect: Rect, image: Image, opacity: f32) {
        self.colored_image(rect, image, opacity, ImageColoring::Original);
    }

    pub fn colored_image(
        &mut self,
        rect: Rect,
        image: Image,
        opacity: f32,
        coloring: ImageColoring,
    ) {
        if rect.width > 0.0 && rect.height > 0.0 && opacity > 0.0 {
            self.primitives.push(Primitive::Image {
                rect,
                image,
                opacity: opacity.clamp(0.0, 1.0),
                coloring,
            });
        }
    }

    pub fn push_opacity(&mut self, opacity: f32) {
        self.primitives.push(Primitive::PushOpacity {
            opacity: opacity.clamp(0.0, 1.0),
        });
    }

    pub fn pop_opacity(&mut self) {
        self.primitives.push(Primitive::PopOpacity);
    }

    pub fn push_motion(&mut self, origin: Point, motion: Motion) {
        self.primitives
            .push(Primitive::PushMotion { origin, motion });
    }

    pub fn has_active_motion(&self, now: Duration, policy: MotionPolicy) -> bool {
        self.primitives.iter().any(|primitive| {
            matches!(primitive, Primitive::PushMotion { motion, .. } if motion.is_active(now, policy))
                || matches!(primitive, Primitive::ShaderEffect { effect, .. } if effect.is_active(policy))
        })
    }

    pub fn pop_motion(&mut self) {
        self.primitives.push(Primitive::PopMotion);
    }

    pub fn custom_gles(&mut self, id: CustomGlesId, rect: Rect, theme: Theme) {
        if rect.width > 0.0 && rect.height > 0.0 {
            self.primitives
                .push(Primitive::CustomGles { id, rect, theme });
        }
    }

    pub fn shader_effect(&mut self, effect: ShaderEffect, rect: Rect, theme: Theme) {
        if rect.width > 0.0 && rect.height > 0.0 && effect.opacity > 0.0 {
            self.primitives.push(Primitive::ShaderEffect {
                effect,
                rect,
                theme,
            });
        }
    }

    pub fn line(&mut self, start: Point, end: Point, width: f32, color: Color) {
        if width > 0.0 && color.alpha > 0.0 && start != end {
            self.primitives.push(Primitive::Line {
                start,
                end,
                width,
                color,
            });
        }
    }

    pub fn linear_gradient_rect(
        &mut self,
        rect: Rect,
        radius: f32,
        start: Point,
        end: Point,
        start_color: Color,
        end_color: Color,
    ) {
        if rect.width > 0.0
            && rect.height > 0.0
            && start != end
            && (start_color.alpha > 0.0 || end_color.alpha > 0.0)
        {
            self.primitives.push(Primitive::LinearGradientRect {
                rect,
                radius,
                start,
                end,
                start_color,
                end_color,
            });
        }
    }

    pub fn triangle_mesh(&mut self, triangles: Arc<[Point]>, color: Color) {
        if triangles.len() >= 3 && triangles.len().is_multiple_of(3) && color.alpha > 0.0 {
            self.primitives
                .push(Primitive::TriangleMesh { triangles, color });
        }
    }

    pub fn push_clip(&mut self, rect: Rect) {
        self.primitives.push(Primitive::PushClip { rect });
    }

    pub fn pop_clip(&mut self) {
        self.primitives.push(Primitive::PopClip);
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WidgetId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionKind {
    Button { hold: Option<Duration> },
    Slider,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitTarget {
    pub id: WidgetId,
    pub bounds: Rect,
    pub kind: InteractionKind,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InteractionMap {
    targets: Vec<HitTarget>,
}

impl InteractionMap {
    pub fn add(&mut self, target: HitTarget) {
        self.targets.push(target);
    }

    pub fn button(&mut self, id: WidgetId, bounds: Rect) {
        self.add(HitTarget {
            id,
            bounds,
            kind: InteractionKind::Button { hold: None },
        });
    }

    pub fn press_and_hold(&mut self, id: WidgetId, bounds: Rect, threshold: Duration) {
        self.add(HitTarget {
            id,
            bounds,
            kind: InteractionKind::Button {
                hold: Some(threshold),
            },
        });
    }

    pub fn slider(&mut self, id: WidgetId, bounds: Rect) {
        self.add(HitTarget {
            id,
            bounds,
            kind: InteractionKind::Slider,
        });
    }

    pub fn target(&self, id: WidgetId) -> Option<HitTarget> {
        self.targets
            .iter()
            .rev()
            .find(|target| target.id == id)
            .copied()
    }

    fn hit_test(&self, point: Point) -> Option<HitTarget> {
        self.targets
            .iter()
            .rev()
            .find(|target| target.bounds.contains(point))
            .copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContactPhase {
    Down,
    Motion,
    Up,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contact {
    pub id: u32,
    pub phase: ContactPhase,
    pub position: Point,
    pub time: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UiEvent {
    Pressed { id: WidgetId },
    Activated { id: WidgetId },
    LongPressed { id: WidgetId, contact: u32 },
    ValueChanged { id: WidgetId, value: f32 },
    Released { id: WidgetId },
    Cancelled { id: WidgetId },
}

#[derive(Clone, Copy, Debug)]
struct Capture {
    target: HitTarget,
    began: Duration,
    long_press_sent: bool,
}

/// Converts compositor contacts into local widget events while preserving
/// capture when a finger moves outside the original hit rectangle.
#[derive(Default)]
pub struct InteractionState {
    captures: BTreeMap<u32, Capture>,
}

impl InteractionState {
    pub fn has_captures(&self) -> bool {
        !self.captures.is_empty()
    }

    pub fn handle(&mut self, map: &InteractionMap, contact: Contact) -> Vec<UiEvent> {
        match contact.phase {
            ContactPhase::Down => {
                let Some(target) = map.hit_test(contact.position) else {
                    return Vec::new();
                };
                self.captures.insert(
                    contact.id,
                    Capture {
                        target,
                        began: contact.time,
                        long_press_sent: false,
                    },
                );
                let mut events = vec![UiEvent::Pressed { id: target.id }];
                if target.kind == InteractionKind::Slider {
                    events.push(slider_event(target, contact.position));
                }
                events
            }
            ContactPhase::Motion => self
                .captures
                .get(&contact.id)
                .filter(|capture| capture.target.kind == InteractionKind::Slider)
                .map(|capture| vec![slider_event(capture.target, contact.position)])
                .unwrap_or_default(),
            ContactPhase::Up => {
                let Some(capture) = self.captures.remove(&contact.id) else {
                    return Vec::new();
                };
                let mut events = Vec::new();
                match capture.target.kind {
                    InteractionKind::Slider => {
                        events.push(slider_event(capture.target, contact.position));
                    }
                    InteractionKind::Button { .. }
                        if !capture.long_press_sent
                            && capture.target.bounds.contains(contact.position) =>
                    {
                        events.push(UiEvent::Activated {
                            id: capture.target.id,
                        });
                    }
                    InteractionKind::Button { .. } => {}
                }
                events.push(UiEvent::Released {
                    id: capture.target.id,
                });
                events
            }
            ContactPhase::Cancel => self
                .captures
                .remove(&contact.id)
                .map(|capture| {
                    vec![UiEvent::Cancelled {
                        id: capture.target.id,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    /// Advance hold recognizers even when a stationary finger produces no
    /// motion event. The caller normally invokes this from a frame callback.
    pub fn tick(&mut self, now: Duration) -> Vec<UiEvent> {
        let mut events = Vec::new();
        for (contact, capture) in &mut self.captures {
            let InteractionKind::Button {
                hold: Some(threshold),
            } = capture.target.kind
            else {
                continue;
            };
            if !capture.long_press_sent && now.saturating_sub(capture.began) >= threshold {
                capture.long_press_sent = true;
                events.push(UiEvent::LongPressed {
                    id: capture.target.id,
                    contact: *contact,
                });
            }
        }
        events
    }

    pub fn is_pressed(&self, id: WidgetId) -> bool {
        self.captures
            .values()
            .any(|capture| capture.target.id == id)
    }

    /// Transfer an existing compositor-captured finger to a new local widget.
    /// This is used when a compact button expands into a slider without
    /// requiring the user to lift and touch again.
    pub fn transfer_capture(&mut self, contact_id: u32, target: HitTarget) -> bool {
        let Some(capture) = self.captures.get_mut(&contact_id) else {
            return false;
        };
        capture.target = target;
        capture.long_press_sent = true;
        true
    }
}

fn slider_event(target: HitTarget, position: Point) -> UiEvent {
    let value = if target.bounds.width <= 0.0 {
        0.0
    } else {
        ((position.x - target.bounds.x) / target.bounds.width).clamp(0.0, 1.0)
    };
    UiEvent::ValueChanged {
        id: target.id,
        value,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub background: Color,
    pub control: Color,
    pub control_pressed: Color,
    pub accent: Color,
    pub track: Color,
    pub foreground: Color,
    pub muted: Color,
    pub destructive: Color,
    pub corner_radius: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Color::rgb(0.005, 0.008, 0.006),
            control: Color::rgb(0.13, 0.13, 0.15),
            control_pressed: Color::rgb(0.24, 0.24, 0.28),
            accent: Color::rgb(0.42, 0.95, 0.25),
            track: Color::rgb(0.22, 0.22, 0.25),
            foreground: Color::WHITE,
            muted: Color::rgb(0.55, 0.55, 0.58),
            destructive: Color::rgb(0.95, 0.25, 0.25),
            corner_radius: 9.0,
        }
    }
}

impl From<touchbar_protocol::appearance::Rgba8> for Color {
    fn from(color: touchbar_protocol::appearance::Rgba8) -> Self {
        Self::rgba(
            f32::from(color.red) / 255.0,
            f32::from(color.green) / 255.0,
            f32::from(color.blue) / 255.0,
            f32::from(color.alpha) / 255.0,
        )
    }
}

impl From<touchbar_protocol::appearance::AppearanceSnapshot> for Theme {
    fn from(appearance: touchbar_protocol::appearance::AppearanceSnapshot) -> Self {
        Self {
            background: appearance.background.into(),
            control: appearance.surface.into(),
            control_pressed: appearance.surface_pressed.into(),
            accent: appearance.accent.into(),
            track: appearance.surface_hover.into(),
            foreground: appearance.foreground.into(),
            muted: appearance.muted.into(),
            destructive: appearance.destructive.into(),
            corner_radius: appearance.corner_radius_millipixels as f32 / 1000.0,
        }
    }
}

pub fn button(
    scene: &mut Scene,
    interactions: &mut InteractionMap,
    id: WidgetId,
    bounds: Rect,
    pressed: bool,
    hold: Option<Duration>,
    theme: Theme,
) {
    scene.rounded_rect(
        bounds,
        theme.corner_radius,
        if pressed {
            theme.control_pressed
        } else {
            theme.control
        },
    );
    match hold {
        Some(threshold) => interactions.press_and_hold(id, bounds, threshold),
        None => interactions.button(id, bounds),
    }
}

pub fn slider(
    scene: &mut Scene,
    interactions: &mut InteractionMap,
    id: WidgetId,
    bounds: Rect,
    value: f32,
    theme: Theme,
) {
    let value = value.clamp(0.0, 1.0);
    let track = Rect::new(
        bounds.x,
        bounds.y + bounds.height * 0.4,
        bounds.width,
        bounds.height * 0.2,
    );
    scene.rounded_rect(track, track.height * 0.5, theme.track);
    scene.rounded_rect(
        Rect::new(track.x, track.y, track.width * value, track.height),
        track.height * 0.5,
        theme.accent,
    );
    let knob_radius = bounds.height * 0.24;
    let knob_x = bounds.x + bounds.width * value;
    scene.rounded_rect(
        Rect::new(
            knob_x - knob_radius,
            bounds.y + bounds.height * 0.5 - knob_radius,
            knob_radius * 2.0,
            knob_radius * 2.0,
        ),
        knob_radius,
        Color::WHITE,
    );
    interactions.slider(id, bounds);
}

/// Return equal-width horizontal cells inside a rectangle.
pub fn equal_row(bounds: Rect, count: usize, gap: f32) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let total_gap = gap.max(0.0) * count.saturating_sub(1) as f32;
    let width = ((bounds.width - total_gap) / count as f32).max(0.0);
    (0..count)
        .map(|index| {
            Rect::new(
                bounds.x + index as f32 * (width + gap.max(0.0)),
                bounds.y,
                width,
                bounds.height,
            )
        })
        .collect()
}

/// Measure the dependency-free bootstrap font used by [`Scene::text`].
///
/// The first renderer intentionally supports compact ASCII labels. A future
/// shaper can implement the same measurement boundary for localized text.
pub fn measure_text(text: &str, size: f32) -> Size {
    if text.is_empty() || size <= 0.0 {
        return Size::default();
    }
    let cell = (size / 7.0).max(0.75);
    Size::new(cell * (text.chars().count() as f32 * 6.0 - 1.0), cell * 7.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tween {
    pub from: f32,
    pub to: f32,
    pub started: Duration,
    pub duration: Duration,
}

impl Tween {
    pub fn sample(self, now: Duration) -> f32 {
        if self.duration.is_zero() {
            return self.to;
        }
        let linear = now.saturating_sub(self.started).as_secs_f32() / self.duration.as_secs_f32();
        let t = linear.clamp(0.0, 1.0);
        // Smoothstep has zero velocity at both ends and is sufficient for the
        // first UI slice without imposing a global animation system.
        let eased = t * t * (3.0 - 2.0 * t);
        self.from + (self.to - self.from) * eased
    }

    pub fn finished(self, now: Duration) -> bool {
        now.saturating_sub(self.started) >= self.duration
    }
}

pub mod gles {
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
        rc::Rc,
        sync::Arc,
        time::Duration,
    };

    use anyhow::{Result, anyhow, bail};
    use glow::HasContext as _;

    use super::{
        Color, CustomGlesFrame, CustomGlesId, EffectProgram, Icon, Image, ImageColoring,
        LayoutMeasurer, MAX_EFFECT_PROGRAMS, MotionPolicy, Primitive, Rect, Scene, ShaderEffect,
        Size, TextAlign, TextEngine, TextOverflow, Theme, text::TextRasterMode,
    };

    type CustomCallback = dyn Fn(&glow::Context, CustomGlesFrame) -> Result<()>;

    struct CachedImage {
        texture: glow::Texture,
        revision: u64,
    }

    #[derive(Clone, Copy)]
    struct CachedEffect {
        program: glow::Program,
    }

    #[derive(Clone, Copy)]
    struct Affine {
        scale: f32,
        translation_x: f32,
        translation_y: f32,
    }

    impl Affine {
        const IDENTITY: Self = Self {
            scale: 1.0,
            translation_x: 0.0,
            translation_y: 0.0,
        };

        fn apply_rect(self, rect: Rect) -> Rect {
            Rect::new(
                rect.x * self.scale + self.translation_x,
                rect.y * self.scale + self.translation_y,
                rect.width * self.scale,
                rect.height * self.scale,
            )
        }

        fn apply_point(self, point: super::Point) -> super::Point {
            super::Point::new(
                point.x * self.scale + self.translation_x,
                point.y * self.scale + self.translation_y,
            )
        }

        fn then_local(self, origin: super::Point, transform: super::VisualTransform) -> Self {
            let local_scale = transform.scale.max(0.001);
            let local_x = origin.x * (1.0 - local_scale) + transform.translation.x;
            let local_y = origin.y * (1.0 - local_scale) + transform.translation.y;
            Self {
                scale: self.scale * local_scale,
                translation_x: self.scale * local_x + self.translation_x,
                translation_y: self.scale * local_y + self.translation_y,
            }
        }
    }

    /// Minimal GLES3 renderer for the UI scene. Raw GLES remains available
    /// from `touchbar-client` before or after this pass.
    pub struct Renderer {
        shape_program: glow::Program,
        shape_resolution: Option<glow::UniformLocation>,
        shape_rect: Option<glow::UniformLocation>,
        shape_radius: Option<glow::UniformLocation>,
        shape_color: Option<glow::UniformLocation>,
        gradient_program: glow::Program,
        gradient_resolution: Option<glow::UniformLocation>,
        gradient_rect: Option<glow::UniformLocation>,
        gradient_radius: Option<glow::UniformLocation>,
        gradient_start: Option<glow::UniformLocation>,
        gradient_end: Option<glow::UniformLocation>,
        gradient_start_color: Option<glow::UniformLocation>,
        gradient_end_color: Option<glow::UniformLocation>,
        line_program: glow::Program,
        line_resolution: Option<glow::UniformLocation>,
        line_start: Option<glow::UniformLocation>,
        line_end: Option<glow::UniformLocation>,
        line_width: Option<glow::UniformLocation>,
        line_color: Option<glow::UniformLocation>,
        mesh_program: glow::Program,
        mesh_resolution: Option<glow::UniformLocation>,
        mesh_color: Option<glow::UniformLocation>,
        mesh_buffer: glow::Buffer,
        mesh_vertex_array: glow::VertexArray,
        image_program: glow::Program,
        image_resolution: Option<glow::UniformLocation>,
        image_rect: Option<glow::UniformLocation>,
        image_opacity: Option<glow::UniformLocation>,
        image_tint: Option<glow::UniformLocation>,
        image_tint_mode: Option<glow::UniformLocation>,
        image_sampler: Option<glow::UniformLocation>,
        images: RefCell<HashMap<u64, CachedImage>>,
        effect_uniform_buffer: glow::Buffer,
        effects: RefCell<HashMap<Arc<str>, CachedEffect>>,
        custom: RefCell<HashMap<CustomGlesId, Rc<CustomCallback>>>,
        text: RefCell<TextEngine>,
        motion_policy: Cell<MotionPolicy>,
    }

    impl Renderer {
        /// The supplied GLES context must be current.
        pub fn new(gl: &glow::Context) -> Result<Self> {
            // SAFETY: the caller guarantees that `gl` is current.
            unsafe {
                let shape_program = compile_program(gl, VERTEX, SHAPE_FRAGMENT, "shape")?;
                let line_program = match compile_program(gl, VERTEX, LINE_FRAGMENT, "line") {
                    Ok(program) => program,
                    Err(error) => {
                        gl.delete_program(shape_program);
                        return Err(error);
                    }
                };
                let gradient_program =
                    match compile_program(gl, VERTEX, GRADIENT_FRAGMENT, "gradient") {
                        Ok(program) => program,
                        Err(error) => {
                            gl.delete_program(line_program);
                            gl.delete_program(shape_program);
                            return Err(error);
                        }
                    };
                let mesh_program =
                    match compile_program(gl, MESH_VERTEX, MESH_FRAGMENT, "triangle mesh") {
                        Ok(program) => program,
                        Err(error) => {
                            gl.delete_program(gradient_program);
                            gl.delete_program(line_program);
                            gl.delete_program(shape_program);
                            return Err(error);
                        }
                    };
                let image_program = match compile_program(gl, VERTEX, IMAGE_FRAGMENT, "image") {
                    Ok(program) => program,
                    Err(error) => {
                        gl.delete_program(mesh_program);
                        gl.delete_program(gradient_program);
                        gl.delete_program(line_program);
                        gl.delete_program(shape_program);
                        return Err(error);
                    }
                };
                let mesh_buffer = match gl.create_buffer() {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        gl.delete_program(image_program);
                        gl.delete_program(mesh_program);
                        gl.delete_program(gradient_program);
                        gl.delete_program(line_program);
                        gl.delete_program(shape_program);
                        bail!("create UI triangle mesh buffer: {error}");
                    }
                };
                let mesh_vertex_array = match gl.create_vertex_array() {
                    Ok(array) => array,
                    Err(error) => {
                        gl.delete_buffer(mesh_buffer);
                        gl.delete_program(image_program);
                        gl.delete_program(mesh_program);
                        gl.delete_program(gradient_program);
                        gl.delete_program(line_program);
                        gl.delete_program(shape_program);
                        bail!("create UI triangle mesh vertex array: {error}");
                    }
                };
                let effect_uniform_buffer = match gl.create_buffer() {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        gl.delete_vertex_array(mesh_vertex_array);
                        gl.delete_buffer(mesh_buffer);
                        gl.delete_program(image_program);
                        gl.delete_program(mesh_program);
                        gl.delete_program(gradient_program);
                        gl.delete_program(line_program);
                        gl.delete_program(shape_program);
                        bail!("create UI shader-effect uniform buffer: {error}");
                    }
                };
                Ok(Self {
                    shape_resolution: gl.get_uniform_location(shape_program, "u_resolution"),
                    shape_rect: gl.get_uniform_location(shape_program, "u_rect"),
                    shape_radius: gl.get_uniform_location(shape_program, "u_radius"),
                    shape_color: gl.get_uniform_location(shape_program, "u_color"),
                    gradient_resolution: gl.get_uniform_location(gradient_program, "u_resolution"),
                    gradient_rect: gl.get_uniform_location(gradient_program, "u_rect"),
                    gradient_radius: gl.get_uniform_location(gradient_program, "u_radius"),
                    gradient_start: gl.get_uniform_location(gradient_program, "u_start"),
                    gradient_end: gl.get_uniform_location(gradient_program, "u_end"),
                    gradient_start_color: gl
                        .get_uniform_location(gradient_program, "u_start_color"),
                    gradient_end_color: gl.get_uniform_location(gradient_program, "u_end_color"),
                    line_resolution: gl.get_uniform_location(line_program, "u_resolution"),
                    line_start: gl.get_uniform_location(line_program, "u_start"),
                    line_end: gl.get_uniform_location(line_program, "u_end"),
                    line_width: gl.get_uniform_location(line_program, "u_width"),
                    line_color: gl.get_uniform_location(line_program, "u_color"),
                    mesh_resolution: gl.get_uniform_location(mesh_program, "u_resolution"),
                    mesh_color: gl.get_uniform_location(mesh_program, "u_color"),
                    image_resolution: gl.get_uniform_location(image_program, "u_resolution"),
                    image_rect: gl.get_uniform_location(image_program, "u_rect"),
                    image_opacity: gl.get_uniform_location(image_program, "u_opacity"),
                    image_tint: gl.get_uniform_location(image_program, "u_tint"),
                    image_tint_mode: gl.get_uniform_location(image_program, "u_tint_mode"),
                    image_sampler: gl.get_uniform_location(image_program, "u_image"),
                    shape_program,
                    gradient_program,
                    line_program,
                    mesh_program,
                    mesh_buffer,
                    mesh_vertex_array,
                    image_program,
                    images: RefCell::new(HashMap::new()),
                    effect_uniform_buffer,
                    effects: RefCell::new(HashMap::new()),
                    custom: RefCell::new(HashMap::new()),
                    text: RefCell::new(TextEngine::new()),
                    motion_policy: Cell::new(MotionPolicy::Full),
                })
            }
        }

        pub fn set_motion_policy(&self, policy: MotionPolicy) {
            self.motion_policy.set(policy);
        }

        pub fn motion_policy(&self) -> MotionPolicy {
            self.motion_policy.get()
        }

        /// Register a custom draw callback that can be embedded as a normal
        /// retained UI leaf. Re-registering the ID replaces the callback.
        pub fn register_custom_gles(
            &self,
            id: CustomGlesId,
            callback: impl Fn(&glow::Context, CustomGlesFrame) -> Result<()> + 'static,
        ) {
            self.custom.borrow_mut().insert(id, Rc::new(callback));
        }

        pub fn unregister_custom_gles(&self, id: CustomGlesId) -> bool {
            self.custom.borrow_mut().remove(&id).is_some()
        }

        pub fn draw(
            &self,
            gl: &glow::Context,
            scene: &Scene,
            width: u32,
            height: u32,
        ) -> Result<()> {
            self.draw_at(gl, scene, width, height, Duration::ZERO)
        }

        /// Draw the scene at a monotonic animation time. This advances
        /// renderer-owned effects such as marquees without rebuilding layout.
        pub fn draw_at(
            &self,
            gl: &glow::Context,
            scene: &Scene,
            width: u32,
            height: u32,
            now: Duration,
        ) -> Result<()> {
            // SAFETY: creation and drawing occur on the plugin's current GL context.
            unsafe {
                gl.viewport(0, 0, width as i32, height as i32);
                let clear = scene.clear.premultiplied();
                gl.clear_color(clear.red, clear.green, clear.blue, clear.alpha);
                gl.clear(glow::COLOR_BUFFER_BIT);
                gl.enable(glow::BLEND);
                gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                let mut clips = Vec::new();
                let mut opacity_stack = Vec::new();
                let mut opacity = 1.0_f32;
                let mut transform_stack = Vec::new();
                let mut transform = Affine::IDENTITY;
                for primitive in &scene.primitives {
                    match primitive {
                        Primitive::RoundedRect {
                            rect,
                            radius,
                            color,
                        } => self.draw_shape(
                            gl,
                            transform.apply_rect(*rect),
                            *radius * transform.scale,
                            Color {
                                alpha: color.alpha * opacity,
                                ..*color
                            },
                            width,
                            height,
                        ),
                        Primitive::LinearGradientRect {
                            rect,
                            radius,
                            start,
                            end,
                            start_color,
                            end_color,
                        } => self.draw_gradient_rect(
                            gl,
                            transform.apply_rect(*rect),
                            *radius * transform.scale,
                            transform.apply_point(*start),
                            transform.apply_point(*end),
                            Color {
                                alpha: start_color.alpha * opacity,
                                ..*start_color
                            },
                            Color {
                                alpha: end_color.alpha * opacity,
                                ..*end_color
                            },
                            width,
                            height,
                        ),
                        Primitive::TriangleMesh { triangles, color } => self.draw_triangle_mesh(
                            gl,
                            triangles,
                            Color {
                                alpha: color.alpha * opacity,
                                ..*color
                            },
                            transform,
                            clips.last().copied(),
                            width,
                            height,
                        ),
                        Primitive::Text {
                            rect,
                            text,
                            size,
                            color,
                            align,
                            overflow,
                        } => self.draw_text(
                            gl,
                            *rect,
                            text,
                            *size,
                            *color,
                            opacity,
                            *align,
                            *overflow,
                            transform,
                            clips.last().copied(),
                            now,
                            width,
                            height,
                        )?,
                        Primitive::Icon { rect, icon, color } => self.draw_icon(
                            gl,
                            transform.apply_rect(*rect),
                            *icon,
                            Color {
                                alpha: color.alpha * opacity,
                                ..*color
                            },
                            width,
                            height,
                        ),
                        Primitive::Image {
                            rect,
                            image,
                            opacity: image_opacity,
                            coloring,
                        } => self.draw_image(
                            gl,
                            transform.apply_rect(*rect),
                            image,
                            *image_opacity * opacity,
                            *coloring,
                            width,
                            height,
                        )?,
                        Primitive::PushOpacity {
                            opacity: child_opacity,
                        } => {
                            opacity_stack.push(opacity);
                            opacity *= child_opacity;
                        }
                        Primitive::PopOpacity => {
                            opacity = opacity_stack.pop().unwrap_or(1.0);
                        }
                        Primitive::PushMotion { origin, motion } => {
                            transform_stack.push((transform, opacity));
                            let visual = motion.sample(now, self.motion_policy.get());
                            transform = transform.then_local(*origin, visual);
                            opacity *= visual.opacity.clamp(0.0, 1.0);
                        }
                        Primitive::PopMotion => {
                            if let Some((parent_transform, parent_opacity)) = transform_stack.pop()
                            {
                                transform = parent_transform;
                                opacity = parent_opacity;
                            }
                        }
                        Primitive::CustomGles { id, rect, theme } => {
                            let rect = transform.apply_rect(*rect);
                            let parent_clip = clips.last().copied().unwrap_or(Rect::new(
                                0.0,
                                0.0,
                                width as f32,
                                height as f32,
                            ));
                            let clip = intersect(parent_clip, rect);
                            self.draw_custom(
                                gl,
                                *id,
                                CustomGlesFrame {
                                    bounds: rect,
                                    clip,
                                    surface_size: Size::new(width as f32, height as f32),
                                    theme: *theme,
                                    opacity,
                                },
                                width,
                                height,
                            )?;
                            if let Some(clip) = clips.last().copied() {
                                set_scissor(gl, clip, width, height);
                            } else {
                                gl.disable(glow::SCISSOR_TEST);
                            }
                            gl.enable(glow::BLEND);
                            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                        }
                        Primitive::ShaderEffect {
                            effect,
                            rect,
                            theme,
                        } => {
                            let rect = transform.apply_rect(*rect);
                            let parent_clip = clips.last().copied().unwrap_or(Rect::new(
                                0.0,
                                0.0,
                                width as f32,
                                height as f32,
                            ));
                            self.draw_shader_effect(
                                gl,
                                effect,
                                rect,
                                intersect(parent_clip, rect),
                                *theme,
                                opacity,
                                now,
                                width,
                                height,
                            )?;
                            if let Some(clip) = clips.last().copied() {
                                set_scissor(gl, clip, width, height);
                            } else {
                                gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                        Primitive::Line {
                            start,
                            end,
                            width: line_width,
                            color,
                        } => self.draw_line(
                            gl,
                            transform.apply_point(*start),
                            transform.apply_point(*end),
                            *line_width * transform.scale,
                            Color {
                                alpha: color.alpha * opacity,
                                ..*color
                            },
                            clips.last().copied(),
                            width,
                            height,
                        ),
                        Primitive::PushClip { rect } => {
                            let rect = transform.apply_rect(*rect);
                            let clip = clips
                                .last()
                                .copied()
                                .map(|parent| intersect(parent, rect))
                                .unwrap_or(rect);
                            clips.push(clip);
                            set_scissor(gl, clip, width, height);
                        }
                        Primitive::PopClip => {
                            clips.pop();
                            if let Some(clip) = clips.last().copied() {
                                set_scissor(gl, clip, width, height);
                            } else {
                                gl.disable(glow::SCISSOR_TEST);
                            }
                        }
                    }
                }
                gl.disable(glow::SCISSOR_TEST);
                gl.bind_texture(glow::TEXTURE_2D, None);
                gl.disable(glow::BLEND);
            }
            Ok(())
        }

        unsafe fn draw_custom(
            &self,
            gl: &glow::Context,
            id: CustomGlesId,
            frame: CustomGlesFrame,
            width: u32,
            height: u32,
        ) -> Result<()> {
            let callback = self.custom.borrow().get(&id).cloned();
            let Some(callback) = callback else {
                return Ok(());
            };
            // Establish the callback's layout clip. The callback remains an
            // unrestricted in-process renderer, and the built-in state is
            // restored before processing the next scene primitive.
            unsafe {
                gl.viewport(0, 0, width as i32, height as i32);
                set_scissor(gl, frame.clip, width, height);
            }
            callback(gl, frame)
                .map_err(|error| anyhow!("custom GLES renderer {} failed: {error}", id.0))
        }

        #[allow(clippy::too_many_arguments)]
        unsafe fn draw_shader_effect(
            &self,
            gl: &glow::Context,
            effect: &ShaderEffect,
            rect: Rect,
            clip: Rect,
            theme: Theme,
            parent_opacity: f32,
            now: Duration,
            width: u32,
            height: u32,
        ) -> Result<()> {
            if clip.width <= 0.0 || clip.height <= 0.0 {
                return Ok(());
            }
            // SAFETY: the validated program and buffer belong to this current context.
            unsafe {
                let program = self.shader_effect_program(gl, &effect.program)?;
                let mut values = Vec::with_capacity(14 * 4);
                values.extend_from_slice(&[rect.x, rect.y, rect.width, rect.height]);
                values.extend_from_slice(&[
                    width as f32,
                    height as f32,
                    effect.time(now, self.motion_policy.get()),
                    (effect.opacity * parent_opacity).clamp(0.0, 1.0),
                ]);
                for color in [
                    theme.background,
                    theme.control,
                    theme.control_pressed,
                    theme.track,
                    theme.foreground,
                    theme.muted,
                    theme.accent,
                    theme.destructive,
                    theme.accent.contrasting_foreground(),
                    theme.destructive.contrasting_foreground(),
                ] {
                    values.extend_from_slice(&[color.red, color.green, color.blue, color.alpha]);
                }
                values.extend_from_slice(&effect.parameters[..4]);
                values.extend_from_slice(&effect.parameters[4..]);
                let mut bytes = Vec::with_capacity(values.len() * std::mem::size_of::<f32>());
                for value in values {
                    bytes.extend_from_slice(&value.to_ne_bytes());
                }
                gl.bind_buffer(glow::UNIFORM_BUFFER, Some(self.effect_uniform_buffer));
                gl.buffer_data_u8_slice(glow::UNIFORM_BUFFER, &bytes, glow::STREAM_DRAW);
                gl.bind_buffer_base(glow::UNIFORM_BUFFER, 0, Some(self.effect_uniform_buffer));
                gl.use_program(Some(program));
                set_scissor(gl, clip, width, height);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            Ok(())
        }

        unsafe fn shader_effect_program(
            &self,
            gl: &glow::Context,
            effect: &EffectProgram,
        ) -> Result<glow::Program> {
            let key = effect.fragment_source();
            if let Some(cached) = self.effects.borrow().get(key.as_ref()) {
                return Ok(cached.program);
            }
            let mut effects = self.effects.borrow_mut();
            if effects.len() >= MAX_EFFECT_PROGRAMS {
                bail!("UI shader-effect program cache is full");
            }
            // SAFETY: source is generated from audited Naga IR and this context is current.
            let program = unsafe { compile_program(gl, VERTEX, key, "validated shader effect")? };
            let Some(block_index) =
                (unsafe { gl.get_uniform_block_index(program, effect.uniform_block_name()) })
            else {
                // SAFETY: the just-created program belongs to this context.
                unsafe { gl.delete_program(program) };
                bail!("validated shader effect lost its reflected uniform block");
            };
            // SAFETY: the queried block belongs to this program.
            unsafe { gl.uniform_block_binding(program, block_index, 0) };
            effects.insert(key.clone(), CachedEffect { program });
            println!(
                "shader-effect=ready backend=gles300 cache_entries={}",
                effects.len()
            );
            Ok(program)
        }

        unsafe fn draw_shape(
            &self,
            gl: &glow::Context,
            rect: Rect,
            radius: f32,
            color: Color,
            width: u32,
            height: u32,
        ) {
            // SAFETY: the renderer's GLES context is current for the complete draw call.
            unsafe {
                gl.use_program(Some(self.shape_program));
                gl.uniform_2_f32(self.shape_resolution.as_ref(), width as f32, height as f32);
                gl.uniform_4_f32(
                    self.shape_rect.as_ref(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                );
                gl.uniform_1_f32(self.shape_radius.as_ref(), radius);
                gl.uniform_4_f32(
                    self.shape_color.as_ref(),
                    color.red,
                    color.green,
                    color.blue,
                    color.alpha,
                );
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
        }

        #[allow(clippy::too_many_arguments)]
        unsafe fn draw_gradient_rect(
            &self,
            gl: &glow::Context,
            rect: Rect,
            radius: f32,
            start: super::Point,
            end: super::Point,
            start_color: Color,
            end_color: Color,
            width: u32,
            height: u32,
        ) {
            // SAFETY: the renderer's GLES context is current for the complete draw call.
            unsafe {
                gl.use_program(Some(self.gradient_program));
                gl.uniform_2_f32(
                    self.gradient_resolution.as_ref(),
                    width as f32,
                    height as f32,
                );
                gl.uniform_4_f32(
                    self.gradient_rect.as_ref(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                );
                gl.uniform_1_f32(self.gradient_radius.as_ref(), radius);
                gl.uniform_2_f32(self.gradient_start.as_ref(), start.x, start.y);
                gl.uniform_2_f32(self.gradient_end.as_ref(), end.x, end.y);
                gl.uniform_4_f32(
                    self.gradient_start_color.as_ref(),
                    start_color.red,
                    start_color.green,
                    start_color.blue,
                    start_color.alpha,
                );
                gl.uniform_4_f32(
                    self.gradient_end_color.as_ref(),
                    end_color.red,
                    end_color.green,
                    end_color.blue,
                    end_color.alpha,
                );
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
        }

        #[allow(clippy::too_many_arguments)]
        unsafe fn draw_line(
            &self,
            gl: &glow::Context,
            start: super::Point,
            end: super::Point,
            line_width: f32,
            color: Color,
            parent_clip: Option<Rect>,
            width: u32,
            height: u32,
        ) {
            let padding = line_width * 0.5 + 1.0;
            let bounds = Rect::new(
                start.x.min(end.x) - padding,
                start.y.min(end.y) - padding,
                (start.x - end.x).abs() + padding * 2.0,
                (start.y - end.y).abs() + padding * 2.0,
            );
            let clip = parent_clip.map_or(bounds, |parent| intersect(parent, bounds));
            // SAFETY: the renderer's GLES context is current for the complete draw call.
            unsafe {
                set_scissor(gl, clip, width, height);
                gl.use_program(Some(self.line_program));
                gl.uniform_2_f32(self.line_resolution.as_ref(), width as f32, height as f32);
                gl.uniform_2_f32(self.line_start.as_ref(), start.x, start.y);
                gl.uniform_2_f32(self.line_end.as_ref(), end.x, end.y);
                gl.uniform_1_f32(self.line_width.as_ref(), line_width);
                gl.uniform_4_f32(
                    self.line_color.as_ref(),
                    color.red,
                    color.green,
                    color.blue,
                    color.alpha,
                );
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
                if let Some(parent) = parent_clip {
                    set_scissor(gl, parent, width, height);
                } else {
                    gl.disable(glow::SCISSOR_TEST);
                }
            }
        }

        #[allow(clippy::too_many_arguments)]
        unsafe fn draw_triangle_mesh(
            &self,
            gl: &glow::Context,
            triangles: &[super::Point],
            color: Color,
            transform: Affine,
            parent_clip: Option<Rect>,
            width: u32,
            height: u32,
        ) {
            if triangles.is_empty() || color.alpha <= 0.0 {
                return;
            }
            let mut bytes = Vec::with_capacity(triangles.len() * 8);
            let mut minimum_x = f32::INFINITY;
            let mut minimum_y = f32::INFINITY;
            let mut maximum_x = f32::NEG_INFINITY;
            let mut maximum_y = f32::NEG_INFINITY;
            for point in triangles
                .iter()
                .copied()
                .map(|point| transform.apply_point(point))
            {
                minimum_x = minimum_x.min(point.x);
                minimum_y = minimum_y.min(point.y);
                maximum_x = maximum_x.max(point.x);
                maximum_y = maximum_y.max(point.y);
                bytes.extend_from_slice(&point.x.to_ne_bytes());
                bytes.extend_from_slice(&point.y.to_ne_bytes());
            }
            let bounds = Rect::new(
                minimum_x,
                minimum_y,
                (maximum_x - minimum_x).max(0.0),
                (maximum_y - minimum_y).max(0.0),
            );
            let clip = parent_clip.map_or(bounds, |parent| intersect(parent, bounds));
            // SAFETY: the renderer's GLES objects belong to the current context
            // and the byte stream is tightly packed native-endian f32 pairs.
            unsafe {
                set_scissor(gl, clip, width, height);
                gl.use_program(Some(self.mesh_program));
                gl.uniform_2_f32(self.mesh_resolution.as_ref(), width as f32, height as f32);
                gl.uniform_4_f32(
                    self.mesh_color.as_ref(),
                    color.red,
                    color.green,
                    color.blue,
                    color.alpha,
                );
                gl.bind_vertex_array(Some(self.mesh_vertex_array));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.mesh_buffer));
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STREAM_DRAW);
                gl.enable_vertex_attrib_array(0);
                gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
                gl.draw_arrays(glow::TRIANGLES, 0, triangles.len() as i32);
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
                gl.bind_vertex_array(None);
                if let Some(parent) = parent_clip {
                    set_scissor(gl, parent, width, height);
                } else {
                    gl.disable(glow::SCISSOR_TEST);
                }
            }
        }

        #[allow(clippy::too_many_arguments)]
        unsafe fn draw_text(
            &self,
            gl: &glow::Context,
            rect: Rect,
            text: &str,
            size: f32,
            color: Color,
            opacity: f32,
            align: TextAlign,
            overflow: TextOverflow,
            transform: Affine,
            parent_clip: Option<Rect>,
            now: Duration,
            width: u32,
            height: u32,
        ) -> Result<()> {
            let raster_width = match overflow {
                TextOverflow::Marquee { .. } => 8192,
                TextOverflow::Ellipsis | TextOverflow::Clip => rect.width.ceil().max(1.0) as u32,
            };
            let raster_mode = match overflow {
                TextOverflow::Ellipsis => TextRasterMode::Ellipsis,
                TextOverflow::Clip | TextOverflow::Marquee { .. } => TextRasterMode::Clip,
            };
            let (image, measured, evicted) = {
                let mut text_engine = self.text.borrow_mut();
                let measured = text_engine.measure_with_mode(
                    text,
                    raster_width,
                    rect.height.ceil().max(1.0) as u32,
                    size,
                    color,
                    if matches!(overflow, TextOverflow::Marquee { .. }) {
                        TextAlign::Leading
                    } else {
                        align
                    },
                    raster_mode,
                    false,
                );
                let image = text_engine
                    .rasterize_with_mode(
                        text,
                        if matches!(overflow, TextOverflow::Marquee { .. }) {
                            measured.width.ceil().max(1.0) as u32
                        } else {
                            raster_width
                        },
                        rect.height.ceil().max(1.0) as u32,
                        size,
                        color,
                        if matches!(overflow, TextOverflow::Marquee { .. }) {
                            TextAlign::Leading
                        } else {
                            align
                        },
                        raster_mode,
                        false,
                    )
                    .clone();
                let evicted = text_engine.take_evicted_image_ids();
                (image, measured, evicted)
            };
            if !evicted.is_empty() {
                let mut images = self.images.borrow_mut();
                for image_id in evicted {
                    if let Some(stale) = images.remove(&image_id) {
                        // SAFETY: text textures belong to the current renderer context.
                        unsafe { gl.delete_texture(stale.texture) };
                    }
                }
            }
            let visual_rect = transform.apply_rect(rect);
            let local_clip =
                parent_clip.map_or(visual_rect, |parent| intersect(parent, visual_rect));
            unsafe { set_scissor(gl, local_clip, width, height) };
            let destination = Rect::new(rect.x, rect.y, image.width as f32, rect.height);
            unsafe {
                match overflow {
                    TextOverflow::Marquee { speed, gap } if measured.width > rect.width => {
                        let gap = gap.max(1.0);
                        let cycle = image.width as f32 + gap;
                        let offset = (now.as_secs_f32() * speed.max(0.0)) % cycle;
                        let first = Rect::new(
                            destination.x - offset,
                            destination.y,
                            destination.width,
                            destination.height,
                        );
                        let second = Rect::new(
                            first.x + cycle,
                            destination.y,
                            destination.width,
                            destination.height,
                        );
                        self.draw_image(
                            gl,
                            transform.apply_rect(first),
                            &image,
                            opacity,
                            ImageColoring::Original,
                            width,
                            height,
                        )?;
                        self.draw_image(
                            gl,
                            transform.apply_rect(second),
                            &image,
                            opacity,
                            ImageColoring::Original,
                            width,
                            height,
                        )?;
                    }
                    TextOverflow::Marquee { .. } => {
                        self.draw_image(
                            gl,
                            transform.apply_rect(destination),
                            &image,
                            opacity,
                            ImageColoring::Original,
                            width,
                            height,
                        )?;
                    }
                    TextOverflow::Ellipsis | TextOverflow::Clip => {
                        self.draw_image(
                            gl,
                            visual_rect,
                            &image,
                            opacity,
                            ImageColoring::Original,
                            width,
                            height,
                        )?;
                    }
                }
                if let Some(clip) = parent_clip {
                    set_scissor(gl, clip, width, height);
                } else {
                    gl.disable(glow::SCISSOR_TEST);
                }
            }
            Ok(())
        }

        unsafe fn draw_icon(
            &self,
            gl: &glow::Context,
            rect: Rect,
            icon: Icon,
            color: Color,
            width: u32,
            height: u32,
        ) {
            let side = rect.width.min(rect.height).min(28.0);
            let x = rect.x + (rect.width - side) * 0.5;
            let y = rect.y + (rect.height - side) * 0.5;
            let unit = side / 7.0;
            let blocks: &[(f32, f32, f32, f32)] = match icon {
                Icon::Volume => &[
                    (0.5, 2.5, 1.5, 2.0),
                    (2.0, 1.5, 1.5, 4.0),
                    (4.1, 2.1, 0.6, 2.8),
                    (5.3, 1.3, 0.6, 4.4),
                ],
                Icon::Muted => &[
                    (0.5, 2.5, 1.5, 2.0),
                    (2.0, 1.5, 1.5, 4.0),
                    (4.3, 2.0, 0.6, 3.0),
                    (5.4, 2.0, 0.6, 3.0),
                ],
                Icon::Play => &[
                    (1.5, 1.2, 1.0, 4.6),
                    (2.5, 2.0, 1.0, 3.0),
                    (3.5, 2.8, 1.0, 1.4),
                ],
                Icon::Pause => &[(1.6, 1.3, 1.2, 4.4), (4.1, 1.3, 1.2, 4.4)],
                Icon::Check => &[
                    (1.1, 3.6, 1.1, 1.1),
                    (2.1, 4.5, 1.1, 1.1),
                    (3.0, 3.5, 1.1, 1.1),
                    (3.9, 2.6, 1.1, 1.1),
                    (4.8, 1.7, 1.1, 1.1),
                ],
                Icon::ChevronLeft => &[
                    (3.8, 1.2, 1.0, 1.0),
                    (3.0, 2.0, 1.0, 1.0),
                    (2.2, 2.8, 1.0, 1.4),
                    (3.0, 4.0, 1.0, 1.0),
                    (3.8, 4.8, 1.0, 1.0),
                ],
                Icon::ChevronRight => &[
                    (2.2, 1.2, 1.0, 1.0),
                    (3.0, 2.0, 1.0, 1.0),
                    (3.8, 2.8, 1.0, 1.4),
                    (3.0, 4.0, 1.0, 1.0),
                    (2.2, 4.8, 1.0, 1.0),
                ],
            };
            for &(bx, by, bw, bh) in blocks {
                // SAFETY: forwarded from the current renderer draw call.
                unsafe {
                    self.draw_shape(
                        gl,
                        Rect::new(x + bx * unit, y + by * unit, bw * unit, bh * unit),
                        unit * 0.35,
                        color,
                        width,
                        height,
                    )
                };
            }
        }

        #[allow(clippy::too_many_arguments)]
        unsafe fn draw_image(
            &self,
            gl: &glow::Context,
            rect: Rect,
            image: &Image,
            opacity: f32,
            coloring: ImageColoring,
            width: u32,
            height: u32,
        ) -> Result<()> {
            // SAFETY: texture creation and use occur on the current renderer context.
            unsafe {
                let texture = self.image_texture(gl, image)?;
                gl.use_program(Some(self.image_program));
                gl.uniform_2_f32(self.image_resolution.as_ref(), width as f32, height as f32);
                gl.uniform_4_f32(
                    self.image_rect.as_ref(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                );
                gl.uniform_1_f32(self.image_opacity.as_ref(), opacity);
                let (tint, tint_mode) = match coloring {
                    ImageColoring::Original => (Color::WHITE, 0),
                    ImageColoring::Multiply(color) => (color, 1),
                    ImageColoring::Mask(color) => (color, 2),
                };
                gl.uniform_4_f32(
                    self.image_tint.as_ref(),
                    tint.red,
                    tint.green,
                    tint.blue,
                    tint.alpha,
                );
                gl.uniform_1_i32(self.image_tint_mode.as_ref(), tint_mode);
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.uniform_1_i32(self.image_sampler.as_ref(), 0);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            Ok(())
        }

        unsafe fn image_texture(&self, gl: &glow::Context, image: &Image) -> Result<glow::Texture> {
            let mut images = self.images.borrow_mut();
            if let Some(cached) = images.get(&image.id)
                && cached.revision == image.revision
            {
                return Ok(cached.texture);
            }
            if let Some(stale) = images.remove(&image.id) {
                // SAFETY: the texture belongs to this current context.
                unsafe { gl.delete_texture(stale.texture) };
            }
            // SAFETY: the renderer context is current and the byte slice matches dimensions.
            let texture = unsafe {
                let texture = gl
                    .create_texture()
                    .map_err(|error| anyhow!("create UI image texture: {error}"))?;
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    image.width as i32,
                    image.height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(image.pixels.as_ref())),
                );
                texture
            };
            images.insert(
                image.id,
                CachedImage {
                    texture,
                    revision: image.revision,
                },
            );
            Ok(texture)
        }

        /// Explicitly free GL resources before the client context is dropped.
        pub fn destroy(self, gl: &glow::Context) {
            // SAFETY: the renderer's creating context is current.
            unsafe {
                for cached in self.images.into_inner().into_values() {
                    gl.delete_texture(cached.texture);
                }
                for cached in self.effects.into_inner().into_values() {
                    gl.delete_program(cached.program);
                }
                gl.delete_vertex_array(self.mesh_vertex_array);
                gl.delete_buffer(self.mesh_buffer);
                gl.delete_buffer(self.effect_uniform_buffer);
                gl.delete_program(self.shape_program);
                gl.delete_program(self.gradient_program);
                gl.delete_program(self.line_program);
                gl.delete_program(self.mesh_program);
                gl.delete_program(self.image_program);
            }
        }
    }

    impl LayoutMeasurer for Renderer {
        fn measure_text(&self, text: &str, size: f32, maximum: Size) -> Size {
            let width = maximum.width.ceil().clamp(1.0, 8192.0) as u32;
            let height = maximum.height.ceil().clamp(1.0, 512.0) as u32;
            self.text.borrow_mut().measure(
                text,
                width,
                height,
                size,
                Color::WHITE,
                TextAlign::Leading,
            )
        }
    }

    unsafe fn compile_program(
        gl: &glow::Context,
        vertex_source: &str,
        fragment_source: &str,
        label: &str,
    ) -> Result<glow::Program> {
        // SAFETY: the caller guarantees that this GL context is current.
        unsafe {
            let vertex = compile_shader(gl, glow::VERTEX_SHADER, vertex_source)?;
            let fragment = compile_shader(gl, glow::FRAGMENT_SHADER, fragment_source)?;
            let program = gl
                .create_program()
                .map_err(|error| anyhow!("create UI {label} program: {error}"))?;
            gl.attach_shader(program, vertex);
            gl.attach_shader(program, fragment);
            gl.link_program(program);
            gl.detach_shader(program, vertex);
            gl.detach_shader(program, fragment);
            gl.delete_shader(vertex);
            gl.delete_shader(fragment);
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                gl.delete_program(program);
                bail!("UI {label} shader link failed: {log}");
            }
            Ok(program)
        }
    }

    unsafe fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::Shader> {
        // SAFETY: the caller guarantees that this GL context is current.
        unsafe {
            let shader = gl
                .create_shader(kind)
                .map_err(|error| anyhow!("create UI shader: {error}"))?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                bail!("UI shader compilation failed: {log}");
            }
            Ok(shader)
        }
    }

    const VERTEX: &str = r#"#version 300 es
precision highp float;
void main() {
    vec2 positions[3] = vec2[3](vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    gl_Position = vec4(positions[gl_VertexID], 0.0, 1.0);
}
"#;

    const MESH_VERTEX: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_position;
uniform vec2 u_resolution;
void main() {
    vec2 normalized = a_position / u_resolution;
    vec2 clip = normalized * 2.0 - 1.0;
    gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
}
"#;

    const MESH_FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform vec4 u_color;
out vec4 output_color;
void main() {
    output_color = vec4(u_color.rgb * u_color.a, u_color.a);
}
"#;

    fn intersect(left: Rect, right: Rect) -> Rect {
        let x = left.x.max(right.x);
        let y = left.y.max(right.y);
        let far_x = (left.x + left.width).min(right.x + right.width);
        let far_y = (left.y + left.height).min(right.y + right.height);
        Rect::new(x, y, (far_x - x).max(0.0), (far_y - y).max(0.0))
    }

    unsafe fn set_scissor(gl: &glow::Context, rect: Rect, width: u32, height: u32) {
        let left = rect.x.floor().clamp(0.0, width as f32) as i32;
        let right = (rect.x + rect.width).ceil().clamp(0.0, width as f32) as i32;
        let top = rect.y.floor().clamp(0.0, height as f32) as i32;
        let bottom = (rect.y + rect.height).ceil().clamp(0.0, height as f32) as i32;
        // SAFETY: the caller owns the current GL context.
        unsafe {
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(left, height as i32 - bottom, right - left, bottom - top);
        }
    }

    const SHAPE_FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform vec2 u_resolution;
uniform vec4 u_rect;
uniform float u_radius;
uniform vec4 u_color;
out vec4 output_color;

void main() {
    vec2 point = vec2(gl_FragCoord.x, u_resolution.y - gl_FragCoord.y);
    vec2 center = u_rect.xy + u_rect.zw * 0.5;
    vec2 half_size = u_rect.zw * 0.5;
    float radius = min(u_radius, min(half_size.x, half_size.y));
    vec2 distance_to_edge = abs(point - center) - (half_size - vec2(radius));
    float signed_distance = length(max(distance_to_edge, 0.0))
        + min(max(distance_to_edge.x, distance_to_edge.y), 0.0) - radius;
    float coverage = 1.0 - smoothstep(-0.75, 0.75, signed_distance);
    float alpha = u_color.a * coverage;
    output_color = vec4(u_color.rgb * alpha, alpha);
}
"#;

    const LINE_FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform vec2 u_resolution;
uniform vec2 u_start;
uniform vec2 u_end;
uniform float u_width;
uniform vec4 u_color;
out vec4 output_color;

void main() {
    vec2 point = vec2(gl_FragCoord.x, u_resolution.y - gl_FragCoord.y);
    vec2 segment = u_end - u_start;
    float squared_length = max(dot(segment, segment), 0.000001);
    float amount = clamp(dot(point - u_start, segment) / squared_length, 0.0, 1.0);
    float distance_to_line = length(point - (u_start + segment * amount));
    float coverage = 1.0 - smoothstep(u_width * 0.5 - 0.75, u_width * 0.5 + 0.75, distance_to_line);
    float alpha = u_color.a * coverage;
    output_color = vec4(u_color.rgb * alpha, alpha);
}
"#;

    const GRADIENT_FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform vec2 u_resolution;
uniform vec4 u_rect;
uniform float u_radius;
uniform vec2 u_start;
uniform vec2 u_end;
uniform vec4 u_start_color;
uniform vec4 u_end_color;
out vec4 output_color;

void main() {
    vec2 point = vec2(gl_FragCoord.x, u_resolution.y - gl_FragCoord.y);
    vec2 center = u_rect.xy + u_rect.zw * 0.5;
    vec2 half_size = u_rect.zw * 0.5;
    float radius = min(u_radius, min(half_size.x, half_size.y));
    vec2 distance_to_edge = abs(point - center) - (half_size - vec2(radius));
    float signed_distance = length(max(distance_to_edge, 0.0))
        + min(max(distance_to_edge.x, distance_to_edge.y), 0.0) - radius;
    float coverage = 1.0 - smoothstep(-0.75, 0.75, signed_distance);
    vec2 axis = u_end - u_start;
    float amount = clamp(dot(point - u_start, axis) / max(dot(axis, axis), 0.000001), 0.0, 1.0);
    vec4 color = mix(u_start_color, u_end_color, amount);
    float alpha = color.a * coverage;
    output_color = vec4(color.rgb * alpha, alpha);
}
"#;

    const IMAGE_FRAGMENT: &str = r#"#version 300 es
precision highp float;
uniform vec2 u_resolution;
uniform vec4 u_rect;
uniform sampler2D u_image;
uniform float u_opacity;
uniform vec4 u_tint;
uniform int u_tint_mode;
out vec4 output_color;

void main() {
    vec2 point = vec2(gl_FragCoord.x, u_resolution.y - gl_FragCoord.y);
    vec2 uv = (point - u_rect.xy) / u_rect.zw;
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        discard;
    }
    vec4 sampled = texture(u_image, uv);
    vec3 rgb = sampled.rgb;
    float tint_alpha = 1.0;
    if (u_tint_mode == 1) {
        rgb *= u_tint.rgb;
        tint_alpha = u_tint.a;
    } else if (u_tint_mode == 2) {
        rgb = u_tint.rgb;
        tint_alpha = u_tint.a;
    }
    float alpha = sampled.a * tint_alpha * u_opacity;
    output_color = vec4(rgb * alpha, alpha);
}
"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_premultiply_for_wayland_argb_composition() {
        assert_eq!(
            Color::rgba(0.8, 0.4, 0.2, 0.5).premultiplied(),
            Color::rgba(0.4, 0.2, 0.1, 0.5)
        );
        assert_eq!(Color::TRANSPARENT.premultiplied(), Color::TRANSPARENT);
    }

    #[test]
    fn contrasting_foreground_tracks_light_and_dark_theme_colors() {
        assert_eq!(
            Color::rgb(0.95, 0.9, 0.2).contrasting_foreground(),
            Color::BLACK
        );
        assert_eq!(
            Color::rgb(0.05, 0.08, 0.12).contrasting_foreground(),
            Color::WHITE
        );
    }

    #[test]
    fn text_measurement_and_image_validation_are_deterministic() {
        assert_eq!(measure_text("VOL", 14.0), Size::new(34.0, 14.0));
        assert!(Image::rgba8(1, 1, 2, 2, vec![0; 16]).is_ok());
        assert!(Image::rgba8(1, 1, 2, 2, vec![0; 15]).is_err());
    }

    fn contact(id: u32, phase: ContactPhase, x: f32, time_ms: u64) -> Contact {
        Contact {
            id,
            phase,
            position: Point::new(x, 20.0),
            time: Duration::from_millis(time_ms),
        }
    }

    #[test]
    fn button_activates_on_release_inside() {
        let mut map = InteractionMap::default();
        map.button(WidgetId(1), Rect::new(0.0, 0.0, 50.0, 40.0));
        let mut state = InteractionState::default();
        assert_eq!(
            state.handle(&map, contact(7, ContactPhase::Down, 20.0, 0)),
            [UiEvent::Pressed { id: WidgetId(1) }]
        );
        assert_eq!(
            state.handle(&map, contact(7, ContactPhase::Up, 20.0, 50)),
            [
                UiEvent::Activated { id: WidgetId(1) },
                UiEvent::Released { id: WidgetId(1) }
            ]
        );
    }

    #[test]
    fn slider_keeps_capture_and_clamps_outside_motion() {
        let mut map = InteractionMap::default();
        map.slider(WidgetId(2), Rect::new(10.0, 0.0, 100.0, 40.0));
        let mut state = InteractionState::default();
        state.handle(&map, contact(3, ContactPhase::Down, 60.0, 0));
        assert_eq!(
            state.handle(&map, contact(3, ContactPhase::Motion, 200.0, 10)),
            [UiEvent::ValueChanged {
                id: WidgetId(2),
                value: 1.0
            }]
        );
    }

    #[test]
    fn long_press_fires_once_and_suppresses_activation() {
        let mut map = InteractionMap::default();
        map.press_and_hold(
            WidgetId(4),
            Rect::new(0.0, 0.0, 50.0, 40.0),
            Duration::from_millis(300),
        );
        let mut state = InteractionState::default();
        state.handle(&map, contact(9, ContactPhase::Down, 20.0, 100));
        assert!(state.tick(Duration::from_millis(399)).is_empty());
        assert_eq!(
            state.tick(Duration::from_millis(400)),
            [UiEvent::LongPressed {
                id: WidgetId(4),
                contact: 9
            }]
        );
        assert!(state.tick(Duration::from_millis(800)).is_empty());
        assert_eq!(
            state.handle(&map, contact(9, ContactPhase::Up, 20.0, 900)),
            [UiEvent::Released { id: WidgetId(4) }]
        );
    }

    #[test]
    fn captured_button_can_transfer_to_an_expanded_slider() {
        let mut compact = InteractionMap::default();
        compact.press_and_hold(
            WidgetId(1),
            Rect::new(0.0, 0.0, 50.0, 40.0),
            Duration::from_millis(300),
        );
        let mut state = InteractionState::default();
        state.handle(&compact, contact(5, ContactPhase::Down, 20.0, 0));
        assert!(state.transfer_capture(
            5,
            HitTarget {
                id: WidgetId(2),
                bounds: Rect::new(10.0, 0.0, 200.0, 40.0),
                kind: InteractionKind::Slider,
            }
        ));
        assert_eq!(
            state.handle(
                &InteractionMap::default(),
                contact(5, ContactPhase::Motion, 160.0, 400)
            ),
            [UiEvent::ValueChanged {
                id: WidgetId(2),
                value: 0.75,
            }]
        );
    }

    #[test]
    fn topmost_overlapping_target_wins() {
        let mut map = InteractionMap::default();
        map.button(WidgetId(1), Rect::new(0.0, 0.0, 50.0, 40.0));
        map.button(WidgetId(2), Rect::new(0.0, 0.0, 50.0, 40.0));
        let events =
            InteractionState::default().handle(&map, contact(1, ContactPhase::Down, 10.0, 0));
        assert_eq!(events, [UiEvent::Pressed { id: WidgetId(2) }]);
    }

    #[test]
    fn row_layout_is_deterministic() {
        assert_eq!(
            equal_row(Rect::new(0.0, 0.0, 100.0, 20.0), 3, 5.0),
            [
                Rect::new(0.0, 0.0, 30.0, 20.0),
                Rect::new(35.0, 0.0, 30.0, 20.0),
                Rect::new(70.0, 0.0, 30.0, 20.0),
            ]
        );
    }

    #[test]
    fn tween_uses_smooth_endpoints() {
        let tween = Tween {
            from: 10.0,
            to: 20.0,
            started: Duration::from_secs(1),
            duration: Duration::from_secs(2),
        };
        assert_eq!(tween.sample(Duration::from_secs(1)), 10.0);
        assert_eq!(tween.sample(Duration::from_secs(2)), 15.0);
        assert_eq!(tween.sample(Duration::from_secs(3)), 20.0);
        assert!(tween.finished(Duration::from_secs(3)));
    }

    #[test]
    fn motion_samples_at_render_time_and_reduced_motion_settles_immediately() {
        let motion = Motion {
            id: MotionId(3),
            from: VisualTransform {
                translation: Point::new(10.0, -4.0),
                scale: 0.8,
                opacity: 0.0,
            },
            to: VisualTransform::IDENTITY,
            started: Duration::from_secs(1),
            duration: Duration::from_secs(2),
            easing: Easing::Linear,
            playback: MotionPlayback::Once,
        };
        assert_eq!(
            motion.sample(Duration::from_secs(2), MotionPolicy::Full),
            VisualTransform {
                translation: Point::new(5.0, -2.0),
                scale: 0.9,
                opacity: 0.5,
            }
        );
        assert_eq!(
            motion.sample(Duration::from_secs(1), MotionPolicy::Reduced),
            VisualTransform::IDENTITY
        );
        assert_eq!(
            motion.sample(Duration::from_secs(1), MotionPolicy::Disabled),
            VisualTransform::IDENTITY
        );
        assert!(!motion.is_active(Duration::from_secs(3), MotionPolicy::Full));
        assert!(!motion.is_active(Duration::from_secs(2), MotionPolicy::Reduced));

        let repeating = Motion {
            playback: MotionPlayback::Loop,
            started: Duration::ZERO,
            duration: Duration::from_secs(1),
            from: VisualTransform {
                opacity: 0.0,
                ..VisualTransform::IDENTITY
            },
            to: VisualTransform::IDENTITY,
            ..motion
        };
        assert_eq!(
            repeating
                .sample(Duration::from_millis(1_250), MotionPolicy::Full)
                .opacity,
            0.25
        );
        assert!(repeating.is_active(Duration::from_secs(100), MotionPolicy::Full));

        let alternating = Motion {
            playback: MotionPlayback::Alternate,
            ..repeating
        };
        assert_eq!(
            alternating
                .sample(Duration::from_millis(1_250), MotionPolicy::Full)
                .opacity,
            0.75
        );
    }
}
