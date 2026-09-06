//! Small guest-side retained-UI builder using only semantic theme roles.

use crate::View;
pub use crate::bindings::touchbar::plugin::ui::{
    AnimationPlayback, CanvasColor, CanvasCommand, CanvasPaint, ColorRole, CrossAxisAlignment,
    Easing, Icon, ImageFit, ImageTint, InputEvent, InputKind, Representation, TextAlign,
    VisualTransform,
};
use crate::bindings::touchbar::plugin::ui::{
    AssetImageNode, ButtonNode, CanvasCircle, CanvasCubicSegment, CanvasFillPath,
    CanvasGradientRect, CanvasLine, CanvasLinearGradient, CanvasNode, CanvasPath,
    CanvasPathSegment, CanvasPoint, CanvasPolyline, CanvasQuadraticSegment, CanvasRect, CanvasText,
    ContainerNode, Flex, FlexChild, IconNode, LabelNode, MeterNode, MotionNode, Node, OpacityNode,
    PanelNode, PressableNode, ProgressNode, ResponsiveNode, ResponsiveVariant, Rgba,
    ShaderEffectNode, SliderNode,
};

pub type NodeId = u32;

#[derive(Default)]
pub struct ViewBuilder {
    nodes: Vec<Node>,
}

impl ViewBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, node: Node) -> NodeId {
        let id = u32::try_from(self.nodes.len()).expect("UI node count exceeds u32");
        self.nodes.push(node);
        id
    }
    pub fn finish(self, root: NodeId) -> View {
        View {
            root,
            nodes: self.nodes,
        }
    }
    pub fn empty(&mut self) -> NodeId {
        self.push(Node::Empty)
    }
    pub fn label(&mut self, text: impl Into<String>, size: f32, color: ColorRole) -> NodeId {
        self.push(Node::Label(LabelNode {
            text: text.into(),
            size,
            color,
            align: TextAlign::Center,
        }))
    }
    pub fn icon(&mut self, icon: Icon, color: ColorRole, label: impl Into<String>) -> NodeId {
        self.push(Node::Icon(IconNode {
            icon,
            color,
            label: label.into(),
        }))
    }
    pub fn image(
        &mut self,
        asset_id: impl Into<String>,
        label: impl Into<String>,
        fit: ImageFit,
        tint: ImageTint,
    ) -> NodeId {
        self.push(Node::Image(AssetImageNode {
            asset_id: asset_id.into(),
            label: label.into(),
            opacity: 1.0,
            fit,
            tint,
        }))
    }
    pub fn button(
        &mut self,
        id: u64,
        label: impl Into<String>,
        icon: Option<Icon>,
        pressed: bool,
        selected: bool,
    ) -> NodeId {
        self.push(Node::Button(ButtonNode {
            widget_id: id,
            label: label.into(),
            icon,
            pressed,
            selected,
        }))
    }
    pub fn row(&mut self, children: Vec<FlexChild>, gap: f32, padding: f32) -> NodeId {
        self.push(Node::Row(ContainerNode {
            gap,
            padding,
            align: CrossAxisAlignment::Stretch,
            children,
        }))
    }
    pub fn column(&mut self, children: Vec<FlexChild>, gap: f32, padding: f32) -> NodeId {
        self.push(Node::Column(ContainerNode {
            gap,
            padding,
            align: CrossAxisAlignment::Stretch,
            children,
        }))
    }
    pub fn layer(&mut self, children: Vec<NodeId>) -> NodeId {
        self.push(Node::Layer(children))
    }
    pub fn panel(&mut self, child: NodeId, color: ColorRole, radius: f32, padding: f32) -> NodeId {
        self.push(Node::Panel(PanelNode {
            radius,
            color,
            padding,
            child,
        }))
    }
    pub fn pressable(
        &mut self,
        id: u64,
        label: impl Into<String>,
        child: NodeId,
        style: Pressable,
    ) -> NodeId {
        self.push(Node::Pressable(PressableNode {
            widget_id: id,
            label: label.into(),
            pressed: style.pressed,
            selected: style.selected,
            hold_ms: style.hold_ms,
            background: style.background,
            pressed_background: style.pressed_background,
            content_foreground: style.content_foreground,
            corner_radius: style.corner_radius,
            padding: style.padding,
            child,
        }))
    }
    pub fn slider(&mut self, id: u64, label: impl Into<String>, value: f32) -> NodeId {
        self.push(Node::Slider(SliderNode {
            widget_id: id,
            label: label.into(),
            value,
        }))
    }
    pub fn meter(&mut self, label: impl Into<String>, value: f32, peak: Option<f32>) -> NodeId {
        self.push(Node::Meter(MeterNode {
            label: label.into(),
            value,
            peak,
        }))
    }
    pub fn progress(&mut self, label: impl Into<String>, value: f32, fill: ColorRole) -> NodeId {
        self.push(Node::Progress(ProgressNode {
            label: label.into(),
            value,
            track: ColorRole::Track,
            fill,
        }))
    }
    pub fn opacity(&mut self, child: NodeId, opacity: f32) -> NodeId {
        self.push(Node::Opacity(OpacityNode { opacity, child }))
    }
    pub fn motion(&mut self, child: NodeId, animation: Animation) -> NodeId {
        self.push(Node::Motion(MotionNode {
            animation_id: animation.id,
            start_transform: animation.from,
            end_transform: animation.to,
            duration_ms: animation.duration_ms,
            easing: animation.easing,
            playback: animation.playback,
            child,
        }))
    }
    pub fn canvas(
        &mut self,
        label: impl Into<String>,
        viewbox_width: f32,
        viewbox_height: f32,
        commands: Vec<CanvasCommand>,
    ) -> NodeId {
        self.push(Node::Canvas(CanvasNode {
            label: label.into(),
            viewbox_width,
            viewbox_height,
            commands,
        }))
    }
    pub fn shader_effect(&mut self, effect: GpuEffect) -> NodeId {
        self.push(Node::ShaderEffect(ShaderEffectNode {
            effect_id: effect.id,
            label: effect.label,
            source: effect.source,
            parameters: effect.parameters,
            opacity: effect.opacity,
            preferred_width: effect.preferred_width,
            preferred_height: effect.preferred_height,
            animation_period_ms: effect.animation_period_ms,
        }))
    }
    pub fn responsive(&mut self, id: u64, variants: Vec<(Representation, f32, NodeId)>) -> NodeId {
        self.push(Node::Responsive(ResponsiveNode {
            widget_id: id,
            variants: variants
                .into_iter()
                .map(|(representation, minimum_width, node)| ResponsiveVariant {
                    representation,
                    minimum_width,
                    node,
                })
                .collect(),
        }))
    }
}

/// A sandbox-safe procedural GPU effect. The source is not arbitrary WGSL: it
/// is a straight-line list of `let` bindings ending in `let color = ...;`.
/// Stable IDs preserve host-owned phase and compiled programs across rerenders.
#[derive(Clone, Debug)]
pub struct GpuEffect {
    id: u64,
    label: String,
    source: String,
    parameters: Vec<f32>,
    opacity: f32,
    preferred_width: f32,
    preferred_height: f32,
    animation_period_ms: Option<u32>,
}

impl GpuEffect {
    pub fn new(id: u64, label: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            source: source.into(),
            parameters: Vec::new(),
            opacity: 1.0,
            preferred_width: 100.0,
            preferred_height: 60.0,
            animation_period_ms: None,
        }
    }

    pub fn parameters(mut self, values: impl IntoIterator<Item = f32>) -> Self {
        self.parameters = values.into_iter().collect();
        self
    }

    pub const fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    pub const fn preferred_size(mut self, width: f32, height: f32) -> Self {
        self.preferred_width = width;
        self.preferred_height = height;
        self
    }

    pub const fn animate(mut self, period_ms: u32) -> Self {
        self.animation_period_ms = Some(period_ms);
        self
    }
}

/// A host-timed paint transform. Stable IDs preserve phase when a component
/// rerenders the same description after input, resize, or a theme change.
#[derive(Clone, Copy, Debug)]
pub struct Animation {
    pub id: u64,
    pub from: VisualTransform,
    pub to: VisualTransform,
    pub duration_ms: u32,
    pub easing: Easing,
    pub playback: AnimationPlayback,
}

impl Animation {
    pub const fn new(id: u64, duration_ms: u32) -> Self {
        Self {
            id,
            from: visual_transform(0.0, 0.0, 1.0, 1.0),
            to: visual_transform(0.0, 0.0, 1.0, 1.0),
            duration_ms,
            easing: Easing::EaseInOut,
            playback: AnimationPlayback::Once,
        }
    }

    pub const fn from(mut self, transform: VisualTransform) -> Self {
        self.from = transform;
        self
    }

    pub const fn to(mut self, transform: VisualTransform) -> Self {
        self.to = transform;
        self
    }

    pub const fn easing(mut self, easing: Easing) -> Self {
        self.easing = easing;
        self
    }

    pub const fn playback(mut self, playback: AnimationPlayback) -> Self {
        self.playback = playback;
        self
    }
}

pub const fn visual_transform(
    translation_x: f32,
    translation_y: f32,
    scale: f32,
    opacity: f32,
) -> VisualTransform {
    VisualTransform {
        translation_x,
        translation_y,
        scale,
        opacity,
    }
}

/// Ergonomic builder for the sandbox-safe retained Canvas2D node. Coordinates
/// use a plugin-selected view box and are scaled into the final layout bounds
/// by the host.
pub struct Canvas2d {
    label: String,
    viewbox_width: f32,
    viewbox_height: f32,
    commands: Vec<CanvasCommand>,
}

impl Canvas2d {
    pub fn new(label: impl Into<String>, viewbox_width: f32, viewbox_height: f32) -> Self {
        Self {
            label: label.into(),
            viewbox_width,
            viewbox_height,
            commands: Vec::new(),
        }
    }

    pub fn fill_rect(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        radius: f32,
        paint: CanvasPaint,
    ) -> &mut Self {
        self.commands.push(CanvasCommand::FillRect(CanvasRect {
            x,
            y,
            width,
            height,
            radius,
            paint,
        }));
        self
    }

    pub fn line(
        &mut self,
        start: (f32, f32),
        end: (f32, f32),
        width: f32,
        paint: CanvasPaint,
    ) -> &mut Self {
        self.commands.push(CanvasCommand::Line(CanvasLine {
            start: point(start),
            end: point(end),
            width,
            paint,
        }));
        self
    }

    pub fn polyline(
        &mut self,
        points: impl IntoIterator<Item = (f32, f32)>,
        width: f32,
        paint: CanvasPaint,
    ) -> &mut Self {
        self.commands.push(CanvasCommand::Polyline(CanvasPolyline {
            points: points.into_iter().map(point).collect(),
            width,
            paint,
        }));
        self
    }

    pub fn fill_circle(
        &mut self,
        center: (f32, f32),
        radius: f32,
        paint: CanvasPaint,
    ) -> &mut Self {
        self.commands.push(CanvasCommand::FillCircle(CanvasCircle {
            center: point(center),
            radius,
            paint,
        }));
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn fill_linear_gradient_rect(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        radius: f32,
        start: (f32, f32),
        end: (f32, f32),
        start_color: CanvasPaint,
        end_color: CanvasPaint,
    ) -> &mut Self {
        self.commands
            .push(CanvasCommand::FillLinearGradientRect(CanvasGradientRect {
                x,
                y,
                width,
                height,
                radius,
                gradient: CanvasLinearGradient {
                    start: point(start),
                    end: point(end),
                    start_color,
                    end_color,
                },
            }));
        self
    }

    pub fn stroke_path(&mut self, path: Path2d, width: f32, paint: CanvasPaint) -> &mut Self {
        self.commands.push(CanvasCommand::StrokePath(CanvasPath {
            segments: path.segments,
            width,
            paint,
        }));
        self
    }

    pub fn fill_path(&mut self, path: Path2d, paint: CanvasPaint) -> &mut Self {
        self.commands.push(CanvasCommand::FillPath(CanvasFillPath {
            segments: path.segments,
            paint,
        }));
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        text: impl Into<String>,
        size: f32,
        paint: CanvasPaint,
        align: TextAlign,
    ) -> &mut Self {
        self.commands.push(CanvasCommand::Text(CanvasText {
            x,
            y,
            width,
            height,
            text: text.into(),
            size,
            color: paint,
            align,
        }));
        self
    }

    pub fn finish(self, builder: &mut ViewBuilder) -> NodeId {
        builder.canvas(
            self.label,
            self.viewbox_width,
            self.viewbox_height,
            self.commands,
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Path2d {
    segments: Vec<CanvasPathSegment>,
}

impl Path2d {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn move_to(&mut self, point_value: (f32, f32)) -> &mut Self {
        self.segments
            .push(CanvasPathSegment::MoveTo(point(point_value)));
        self
    }

    pub fn line_to(&mut self, point_value: (f32, f32)) -> &mut Self {
        self.segments
            .push(CanvasPathSegment::LineTo(point(point_value)));
        self
    }

    pub fn quadratic_to(&mut self, control: (f32, f32), endpoint: (f32, f32)) -> &mut Self {
        self.segments
            .push(CanvasPathSegment::QuadraticTo(CanvasQuadraticSegment {
                control: point(control),
                endpoint: point(endpoint),
            }));
        self
    }

    pub fn cubic_to(
        &mut self,
        control_one: (f32, f32),
        control_two: (f32, f32),
        endpoint: (f32, f32),
    ) -> &mut Self {
        self.segments
            .push(CanvasPathSegment::CubicTo(CanvasCubicSegment {
                control_one: point(control_one),
                control_two: point(control_two),
                endpoint: point(endpoint),
            }));
        self
    }

    pub fn close(&mut self) -> &mut Self {
        self.segments.push(CanvasPathSegment::Close);
        self
    }
}

pub const fn theme_paint(color: ColorRole) -> CanvasPaint {
    CanvasPaint {
        color: CanvasColor::Role(color),
        opacity: 1.0,
    }
}

pub const fn theme_paint_with_opacity(color: ColorRole, opacity: f32) -> CanvasPaint {
    CanvasPaint {
        color: CanvasColor::Role(color),
        opacity,
    }
}

pub const fn rgba_paint(red: f32, green: f32, blue: f32, alpha: f32) -> CanvasPaint {
    CanvasPaint {
        color: CanvasColor::Rgba(Rgba {
            red,
            green,
            blue,
            alpha,
        }),
        opacity: 1.0,
    }
}

fn point((x, y): (f32, f32)) -> CanvasPoint {
    CanvasPoint { x, y }
}

#[derive(Clone, Copy, Debug)]
pub struct Pressable {
    pub pressed: bool,
    pub selected: Option<bool>,
    pub hold_ms: Option<u32>,
    pub background: Option<ColorRole>,
    pub pressed_background: Option<ColorRole>,
    pub content_foreground: Option<ColorRole>,
    pub corner_radius: Option<f32>,
    pub padding: f32,
}

impl Pressable {
    pub const fn control() -> Self {
        Self {
            pressed: false,
            selected: None,
            hold_ms: None,
            background: Some(ColorRole::Control),
            pressed_background: Some(ColorRole::ControlPressed),
            content_foreground: None,
            corner_radius: None,
            padding: 6.0,
        }
    }
    pub const fn accent() -> Self {
        Self {
            background: Some(ColorRole::Accent),
            content_foreground: Some(ColorRole::OnAccent),
            ..Self::control()
        }
    }
    pub const fn plain() -> Self {
        Self {
            background: None,
            padding: 0.0,
            ..Self::control()
        }
    }
}

pub fn fixed(node: NodeId, width: f32) -> FlexChild {
    FlexChild {
        node,
        layout: Flex {
            minimum: width,
            basis: width,
            maximum: width,
            grow: 0.0,
            shrink: 0.0,
            visibility_priority: 0,
            required: false,
            intrinsic: false,
        },
    }
}
pub fn flexible(node: NodeId, minimum: f32, basis: f32, maximum: f32) -> FlexChild {
    FlexChild {
        node,
        layout: Flex {
            minimum,
            basis,
            maximum,
            grow: 1.0,
            shrink: 1.0,
            visibility_priority: 0,
            required: false,
            intrinsic: false,
        },
    }
}
pub fn content(node: NodeId, minimum: f32, maximum: f32) -> FlexChild {
    FlexChild {
        node,
        layout: Flex {
            minimum,
            basis: minimum,
            maximum,
            grow: 0.0,
            shrink: 1.0,
            visibility_priority: 0,
            required: false,
            intrinsic: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_builder_emits_semantic_and_literal_commands() {
        let mut canvas = Canvas2d::new("Build activity", 100.0, 50.0);
        let mut path = Path2d::new();
        path.move_to((2.0, 25.0))
            .quadratic_to((25.0, 2.0), (50.0, 25.0))
            .cubic_to((60.0, 40.0), (80.0, 4.0), (98.0, 25.0));
        let mut fill = Path2d::new();
        fill.move_to((42.0, 8.0))
            .line_to((58.0, 25.0))
            .line_to((42.0, 42.0))
            .close();
        canvas
            .fill_rect(0.0, 0.0, 100.0, 50.0, 5.0, theme_paint(ColorRole::Track))
            .polyline(
                [(0.0, 40.0), (50.0, 10.0), (100.0, 30.0)],
                2.0,
                theme_paint_with_opacity(ColorRole::Accent, 0.8),
            )
            .fill_circle((50.0, 10.0), 3.0, rgba_paint(1.0, 0.2, 0.1, 1.0))
            .fill_linear_gradient_rect(
                0.0,
                44.0,
                100.0,
                6.0,
                3.0,
                (0.0, 47.0),
                (100.0, 47.0),
                theme_paint(ColorRole::Accent),
                theme_paint_with_opacity(ColorRole::Muted, 0.3),
            )
            .stroke_path(path, 1.5, theme_paint(ColorRole::Foreground))
            .fill_path(fill, theme_paint(ColorRole::Accent))
            .text(
                4.0,
                4.0,
                40.0,
                12.0,
                "42%",
                10.0,
                theme_paint(ColorRole::Foreground),
                TextAlign::Leading,
            );
        let mut builder = ViewBuilder::new();
        let root = canvas.finish(&mut builder);
        let view = builder.finish(root);
        let Node::Canvas(canvas) = &view.nodes[0] else {
            panic!("builder did not emit a canvas node");
        };
        assert_eq!(canvas.label, "Build activity");
        assert_eq!(canvas.commands.len(), 7);
        assert!(matches!(canvas.commands[1], CanvasCommand::Polyline(_)));
        assert!(matches!(canvas.commands[2], CanvasCommand::FillCircle(_)));
        assert!(matches!(
            canvas.commands[3],
            CanvasCommand::FillLinearGradientRect(_)
        ));
        assert!(matches!(canvas.commands[4], CanvasCommand::StrokePath(_)));
        assert!(matches!(canvas.commands[5], CanvasCommand::FillPath(_)));
    }

    #[test]
    fn animation_builder_wraps_any_retained_node() {
        let mut builder = ViewBuilder::new();
        let label = builder.label("Listening", 11.0, ColorRole::Foreground);
        let root = builder.motion(
            label,
            Animation::new(7, 800)
                .from(visual_transform(-4.0, 0.0, 0.9, 0.0))
                .to(visual_transform(0.0, 0.0, 1.0, 1.0))
                .easing(Easing::Linear)
                .playback(AnimationPlayback::Alternate),
        );
        let view = builder.finish(root);
        let Node::Motion(motion) = &view.nodes[root as usize] else {
            panic!("builder did not emit a motion node");
        };
        assert_eq!(motion.animation_id, 7);
        assert_eq!(motion.duration_ms, 800);
        assert_eq!(motion.start_transform.translation_x, -4.0);
        assert_eq!(motion.playback, AnimationPlayback::Alternate);
        assert_eq!(motion.child, label);
    }

    #[test]
    fn image_builder_references_only_a_logical_asset_and_semantic_tint() {
        let mut builder = ViewBuilder::new();
        let root = builder.image(
            "touchbar-wordmark",
            "TouchBar",
            ImageFit::Contain,
            ImageTint::Mask(ColorRole::Accent),
        );
        let view = builder.finish(root);
        let Node::Image(image) = &view.nodes[root as usize] else {
            panic!("builder did not emit an image node")
        };
        assert_eq!(image.asset_id, "touchbar-wordmark");
        assert_eq!(image.label, "TouchBar");
        assert_eq!(image.opacity, 1.0);
        assert!(matches!(image.tint, ImageTint::Mask(ColorRole::Accent)));
    }

    #[test]
    fn gpu_effect_builder_exposes_only_bounded_source_parameters_and_timing() {
        let source = "let amount = sin(uv.x + time); let color = mix(background, accent, amount);";
        let mut builder = ViewBuilder::new();
        let root = builder.shader_effect(
            GpuEffect::new(9, "Ambient wave", source)
                .parameters([0.25, 0.75])
                .opacity(0.6)
                .preferred_size(240.0, 60.0)
                .animate(4_000),
        );
        let view = builder.finish(root);
        let Node::ShaderEffect(effect) = &view.nodes[root as usize] else {
            panic!("builder did not emit a shader-effect node")
        };
        assert_eq!(effect.effect_id, 9);
        assert_eq!(effect.source, source);
        assert_eq!(effect.parameters, [0.25, 0.75]);
        assert_eq!(effect.opacity, 0.6);
        assert_eq!(effect.preferred_width, 240.0);
        assert_eq!(effect.animation_period_ms, Some(4_000));
    }
}
