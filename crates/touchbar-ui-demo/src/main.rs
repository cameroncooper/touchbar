use std::time::Duration;

use anyhow::{Result, bail};
use glow::HasContext as _;
use touchbar_client::{
    AppearanceSnapshot, Application, ClientOptions, ContactPhase as ClientContactPhase, FrameFlow,
    FrameInfo, Graphics, PresentationDismissReason as ClientDismissReason,
    PresentationSessionEvent as ClientPresentationEvent,
    PresentationSessionRequest as ClientPresentationRequest,
    SessionPresentationLifecycle as ClientLifecycle, SessionPresentationPolicy as ClientPolicy,
    Sizing, SurfaceConfig, TouchContact, run,
};
use touchbar_ui::{
    ColorRole, CrossAxisAlignment, CustomGlesId, DismissReason, DismissalPolicy, Flex, FlexItem,
    FrameScheduler, GestureArena, GestureEvent, GestureMap, Icon, InteractionMap, Node,
    PaletteEvent, PresentationCommand, PresentationController, PresentationLifecycle,
    PresentationPolicy, PresentationSessionId, PressableStyle, ProgressValue, Rect, Representation,
    ResponsiveVariant, RetainedUi, SelectionPalette, Size, Theme, WidgetId, gles,
};
use touchbar_ui::{Contact, ContactPhase, InteractionState, Point, UiEvent};

const VOLUME_BUTTON: WidgetId = WidgetId(1);
const PALETTE: WidgetId = WidgetId(2);
const THEMED_METER: CustomGlesId = CustomGlesId(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DemoPage {
    Volume,
    More,
}

struct UiDemo {
    renderer: Option<gles::Renderer>,
    max_frames: u64,
    interactions: InteractionState,
    interaction_map: InteractionMap,
    gestures: GestureArena,
    gesture_map: GestureMap,
    palette: SelectionPalette,
    width: u32,
    height: u32,
    popover: bool,
    popover_anchor: Rect,
    presentations: PresentationController<DemoPage>,
    presentation_policy: PresentationPolicy,
    last_contact: Option<Contact>,
    received_input: u64,
    theme: Theme,
    ui: RetainedUi,
    scheduler: FrameScheduler,
}

impl UiDemo {
    fn content_bounds(&self) -> Rect {
        Rect::new(
            4.0,
            4.0,
            (self.width as f32 - 8.0).max(1.0),
            (self.height as f32 - 8.0).max(1.0),
        )
    }

    fn compact_node(pressed: bool) -> Node {
        let compact_button = |label: &str, detailed: bool| {
            let content = if detailed {
                Node::row_aligned(
                    5.0,
                    0.0,
                    CrossAxisAlignment::Center,
                    vec![
                        FlexItem::new(
                            Flex::fixed(24.0),
                            Node::Icon {
                                icon: Icon::Volume,
                                color: ColorRole::Foreground,
                                label: "Volume".into(),
                            },
                        ),
                        FlexItem::new(
                            Flex::flexible(20.0, 72.0, 180.0).grow(1.0),
                            Node::column_aligned(
                                2.0,
                                0.0,
                                CrossAxisAlignment::Stretch,
                                vec![
                                    FlexItem::new(
                                        Flex::content(10.0, 20.0),
                                        Node::Label {
                                            text: label.into(),
                                            size: 11.0,
                                            color: ColorRole::Foreground,
                                            align: touchbar_ui::TextAlign::Leading,
                                            overflow: touchbar_ui::TextOverflow::Ellipsis,
                                            measurement: touchbar_ui::TextMeasurement::Content,
                                        },
                                    ),
                                    FlexItem::new(
                                        Flex::fixed(4.0),
                                        Node::Progress {
                                            label: "Volume level".into(),
                                            value: ProgressValue::Determinate(0.62),
                                            track: ColorRole::Track,
                                            fill: ColorRole::Accent,
                                        },
                                    ),
                                ],
                            ),
                        ),
                    ],
                )
            } else {
                Node::Layer(vec![
                    Node::Icon {
                        icon: Icon::Volume,
                        color: ColorRole::Foreground,
                        label: "Volume".into(),
                    },
                    Node::positioned(
                        Rect::new(0.0, 30.0, 64.0, 3.0),
                        Node::custom_gles(THEMED_METER, Size::new(64.0, 3.0), "GPU volume meter"),
                    ),
                ])
            };
            Node::Pressable {
                id: VOLUME_BUTTON,
                label: "Volume".into(),
                pressed,
                hold: Some(Duration::from_millis(300)),
                selected: None,
                style: PressableStyle {
                    padding: 6.0,
                    ..PressableStyle::control()
                },
                child: Box::new(content),
            }
        };
        Node::responsive(
            VOLUME_BUTTON,
            vec![
                ResponsiveVariant::new(Representation::Full, 112.0, compact_button("VOLUME", true)),
                ResponsiveVariant::new(
                    Representation::Compact,
                    52.0,
                    compact_button("VOLUME", false),
                ),
                ResponsiveVariant::new(
                    Representation::Minimal,
                    28.0,
                    compact_button("VOLUME", false),
                ),
            ],
        )
    }

    fn rebuild_tree(&mut self) {
        let node = if self.popover {
            let labels = match self.current_page() {
                DemoPage::Volume => ["MUTE", "25", "50", "75", "MORE"],
                DemoPage::More => ["BACK", "BAL", "L/R", "RESET", "DONE"],
            };
            let anchor = self.popover_anchor;
            self.palette.compose(
                self.content_bounds(),
                anchor,
                || Node::button(VOLUME_BUTTON, "Close volume", Some(Icon::Volume), false),
                |placement| {
                    Node::row(
                        0.0,
                        0.0,
                        vec![FlexItem::new(
                            Flex::content(0.0, f32::INFINITY).grow(1.0).required(),
                            Node::Toggle {
                                id: WidgetId(1_000 + placement.index as u64),
                                label: labels[placement.index].into(),
                                icon: None,
                                selected: placement.selected || placement.highlighted,
                                pressed: false,
                            },
                        )],
                    )
                },
            )
        } else {
            Self::compact_node(self.interactions.is_pressed(VOLUME_BUTTON))
        };
        self.ui.replace(node);
        self.scheduler.invalidate();
    }

    fn consume_events(&mut self, events: Vec<UiEvent>) {
        let now = self
            .last_contact
            .map_or(Duration::ZERO, |contact| contact.time);
        for event in events {
            if let UiEvent::LongPressed { contact, .. } = event {
                let session = self.presentations.present(
                    self.presentation_policy.clone(),
                    PresentationLifecycle::Transient { contact },
                    DismissalPolicy::transient(),
                    DemoPage::Volume,
                    now,
                );
                println!(
                    "ui-event=popover-request session={} contact={contact}",
                    session.0
                );
            } else if let UiEvent::Activated { .. } = event {
                let session = self.presentations.present(
                    self.presentation_policy.clone(),
                    PresentationLifecycle::Persistent,
                    DismissalPolicy::persistent(Some(Duration::from_secs(4))),
                    DemoPage::Volume,
                    now,
                );
                println!("ui-event=persistent-popover-request session={}", session.0);
            }
        }
    }

    fn consume_gestures(&mut self, events: Vec<GestureEvent>) {
        let bounds = self.content_bounds();
        for event in events {
            let released = match event {
                GestureEvent::Released { contact, .. } => Some(contact),
                _ => None,
            };
            let page = self.current_page();
            let persistent = self
                .presentations
                .active()
                .is_some_and(|session| session.lifecycle() == PresentationLifecycle::Persistent);
            let mut navigated = false;
            for palette_event in self.palette.handle(event, bounds, self.popover_anchor) {
                match palette_event {
                    PaletteEvent::HighlightChanged(index) => {
                        println!("ui-event=palette-highlight index={index:?}");
                    }
                    PaletteEvent::SelectionChanged(index) => {
                        println!("ui-event=palette-selection index={index}");
                        if persistent && page == DemoPage::Volume && index == 4 {
                            navigated = self.presentations.push(DemoPage::More);
                            println!("ui-navigation=push page=more");
                        } else if page == DemoPage::More && index == 0 {
                            navigated = self.presentations.back();
                            println!("ui-navigation=back page=volume");
                        } else {
                            self.presentations.selected();
                        }
                    }
                    PaletteEvent::DismissRequested if !navigated => self.request_compact(),
                    PaletteEvent::DismissRequested => {}
                    PaletteEvent::Cancelled => println!("ui-event=palette-cancelled"),
                }
            }
            if let Some(contact) = released {
                self.presentations.released(contact);
            }
        }
    }

    fn request_compact(&mut self) {
        let requested = self
            .presentations
            .active()
            .map(|session| session.id())
            .is_some_and(|session| {
                self.presentations
                    .dismiss_if_current(session, DismissReason::Requested)
            });
        if requested {
            println!("ui-event=compact-request");
        }
    }

    fn current_page(&self) -> DemoPage {
        self.presentations
            .active()
            .map_or(DemoPage::Volume, |session| *session.current_page())
    }

    fn configure_gestures(&mut self) {
        self.gesture_map = GestureMap::default();
        self.gesture_map
            .add(self.palette.gesture_target(self.content_bounds()));
        let Some(PresentationLifecycle::Transient {
            contact: contact_id,
        }) = self
            .presentations
            .active()
            .map(|session| session.lifecycle())
        else {
            return;
        };
        let Some(contact) = self.last_contact.filter(|contact| contact.id == contact_id) else {
            return;
        };
        if !self.gestures.has_contact(contact.id)
            && !matches!(contact.phase, ContactPhase::Up | ContactPhase::Cancel)
        {
            let mut seed = contact;
            seed.phase = ContactPhase::Down;
            seed.position.x += self.popover_anchor.x;
            let events = self.gestures.handle(&self.gesture_map, seed);
            self.consume_gestures(events);
        }
    }
}

impl Application for UiDemo {
    fn appearance_changed(&mut self, appearance: AppearanceSnapshot) -> Result<bool> {
        self.theme = appearance.into();
        self.ui.invalidate();
        self.scheduler.invalidate();
        println!(
            "ui-appearance generation={} scheme={:?} accent=#{:08x}",
            appearance.generation,
            appearance.scheme,
            appearance.accent.packed()
        );
        Ok(true)
    }

    fn configured(&mut self, graphics: &Graphics, config: SurfaceConfig) -> Result<()> {
        if self.renderer.is_none() {
            let renderer = gles::Renderer::new(graphics.gl())?;
            renderer.register_custom_gles(THEMED_METER, |gl, frame| {
                let color = frame.theme.accent.premultiplied();
                let filled = frame.bounds.width * 0.62;
                let left = frame.bounds.x.max(frame.clip.x);
                let right = (frame.bounds.x + filled).min(frame.clip.x + frame.clip.width);
                let top = frame.bounds.y.max(frame.clip.y);
                let bottom =
                    (frame.bounds.y + frame.bounds.height).min(frame.clip.y + frame.clip.height);
                if right > left && bottom > top {
                    // SAFETY: the SDK keeps this context current for the complete callback.
                    unsafe {
                        gl.enable(glow::SCISSOR_TEST);
                        gl.scissor(
                            left.floor() as i32,
                            (frame.surface_size.height - bottom).floor() as i32,
                            (right - left).ceil() as i32,
                            (bottom - top).ceil() as i32,
                        );
                        gl.clear_color(
                            color.red * frame.opacity,
                            color.green * frame.opacity,
                            color.blue * frame.opacity,
                            color.alpha * frame.opacity,
                        );
                        gl.clear(glow::COLOR_BUFFER_BIT);
                    }
                }
                Ok(())
            });
            self.renderer = Some(renderer);
        }
        self.width = config.width;
        self.height = config.height;
        if self.popover {
            self.configure_gestures();
        }
        self.rebuild_tree();
        println!(
            "configured plugin=touchbar.ui-demo region={}x{} renderer={} transport=dmabuf ui=touchbar-ui",
            config.width,
            config.height,
            graphics.renderer_name()
        );
        Ok(())
    }

    fn render(&mut self, graphics: &Graphics, frame: FrameInfo) -> Result<FrameFlow> {
        let now = Duration::from_secs_f32(frame.elapsed_seconds);
        if self.presentations.tick(now) {
            println!("ui-event=persistent-popover-timeout");
        }
        let width = frame.surface.width as f32;
        let height = frame.surface.height as f32;
        let pulse = frame.elapsed_seconds.sin() * 0.08 + 0.92;
        let theme = Theme {
            accent: self.theme.accent.mix(self.theme.foreground, 1.0 - pulse),
            ..self.theme
        };

        let resolved = {
            let renderer = self
                .renderer
                .as_ref()
                .expect("renderer initialized during configure");
            self.ui.resolve_with_measurer(
                Rect::new(4.0, 4.0, width - 8.0, height - 8.0),
                theme,
                renderer,
            )
        };
        self.interaction_map = resolved.interactions;
        if frame.number.is_multiple_of(120) {
            let representation = resolved
                .inspector
                .representations
                .get(&VOLUME_BUTTON)
                .copied()
                .unwrap_or(Representation::Full);
            println!(
                "ui-inspector revision={} role={:?} label={} representation={representation:?}",
                resolved.inspector.revision,
                resolved.inspector.semantics.role,
                resolved.inspector.semantics.label
            );
        }
        let timed = self
            .interactions
            .tick(Duration::from_secs_f32(frame.elapsed_seconds));
        self.consume_events(timed);
        self.renderer
            .as_ref()
            .expect("renderer initialized during configure")
            .draw(
                graphics.gl(),
                &resolved.scene,
                frame.surface.width,
                frame.surface.height,
            )?;

        self.scheduler.frame_rendered(now);

        Ok(if frame.number + 1 >= self.max_frames {
            FrameFlow::Exit
        } else if self.scheduler.wants_next_frame(now) {
            FrameFlow::Animate
        } else {
            FrameFlow::Wait
        })
    }

    fn visibility_changed(&mut self, visible: bool) -> Result<bool> {
        self.ui.set_visible(visible);
        self.scheduler.set_visible(visible);
        println!("ui-visibility visible={visible}");
        Ok(self.ui.needs_render())
    }

    fn touch(&mut self, contact: TouchContact) -> Result<bool> {
        self.received_input += 1;
        let phase = match contact.phase {
            ClientContactPhase::Down => ContactPhase::Down,
            ClientContactPhase::Motion => ContactPhase::Motion,
            ClientContactPhase::Up => ContactPhase::Up,
            ClientContactPhase::Cancel => ContactPhase::Cancel,
        };
        let ui_contact = Contact {
            id: contact.id,
            phase,
            position: Point::new(contact.x, contact.y),
            time: contact.time,
        };
        self.last_contact = Some(ui_contact);
        if phase == ContactPhase::Down {
            self.presentations.activity(contact.time);
        }
        let events = if self.popover {
            // Clear the compact recognizer's retained capture on release while
            // the richer arena owns the expanded interaction.
            let _ = self.interactions.handle(&self.interaction_map, ui_contact);
            let gestures = self.gestures.handle(&self.gesture_map, ui_contact);
            let count = gestures.len();
            self.consume_gestures(gestures);
            println!(
                "ui-touch phase={:?} contact={} local={:.1},{:.1} gesture-events={count}",
                contact.phase, contact.id, contact.x, contact.y
            );
            Vec::new()
        } else {
            self.interactions.handle(&self.interaction_map, ui_contact)
        };
        println!(
            "ui-touch phase={:?} contact={} local={:.1},{:.1} events={}",
            contact.phase,
            contact.id,
            contact.x,
            contact.y,
            events.len()
        );
        self.consume_events(events);
        self.rebuild_tree();
        Ok(true)
    }

    fn presentation_session_changed(&mut self, event: ClientPresentationEvent) -> Result<()> {
        match event {
            ClientPresentationEvent::Anchor { session_id, anchor } => {
                let session = PresentationSessionId(session_id);
                if self.presentations.set_anchor(
                    session,
                    Rect::new(anchor.x, 0.0, anchor.width, self.height as f32),
                ) {
                    self.popover_anchor =
                        Rect::new(anchor.x, 0.0, anchor.width, self.height as f32);
                    println!(
                        "ui-popover-anchor session={session_id} x={:.1} width={:.1}",
                        anchor.x, anchor.width
                    );
                }
            }
            ClientPresentationEvent::Started {
                session_id,
                lifecycle,
                ..
            } => {
                if self.presentations.active().map(|session| session.id())
                    == Some(PresentationSessionId(session_id))
                {
                    self.popover = true;
                    self.configure_gestures();
                    println!(
                        "ui-presentation=started session={session_id} lifecycle={lifecycle:?}"
                    );
                }
            }
            ClientPresentationEvent::Ended { session_id, reason } => {
                if self
                    .presentations
                    .compositor_dismissed(PresentationSessionId(session_id))
                {
                    self.popover = false;
                    println!("ui-presentation=ended session={session_id} reason={reason:?}");
                }
            }
        }
        self.rebuild_tree();
        Ok(())
    }

    fn take_presentation_session_request(&mut self) -> Option<ClientPresentationRequest> {
        self.presentations
            .take_command()
            .map(|command| match command {
                PresentationCommand::Present {
                    session,
                    policy,
                    lifecycle,
                } => ClientPresentationRequest::Begin {
                    session_id: session.0,
                    policy: match policy {
                        PresentationPolicy::Anchored => ClientPolicy::Anchored,
                        PresentationPolicy::InPlace => ClientPolicy::InPlace,
                        PresentationPolicy::Slot(target) => ClientPolicy::Slot(target),
                        PresentationPolicy::Region(target) => ClientPolicy::Region(target),
                        PresentationPolicy::FullBar => ClientPolicy::FullBar,
                    },
                    lifecycle: match lifecycle {
                        PresentationLifecycle::Transient { contact } => {
                            ClientLifecycle::Transient {
                                contact_id: contact,
                            }
                        }
                        PresentationLifecycle::Persistent => ClientLifecycle::Persistent,
                    },
                },
                PresentationCommand::Dismiss { session, reason } => {
                    ClientPresentationRequest::End {
                        session_id: session.0,
                        reason: match reason {
                            DismissReason::Requested => ClientDismissReason::Requested,
                            DismissReason::Selection => ClientDismissReason::Selection,
                            DismissReason::OutsidePress => ClientDismissReason::OutsidePress,
                            DismissReason::Timeout => ClientDismissReason::Timeout,
                            DismissReason::SourceHidden => ClientDismissReason::SourceHidden,
                            DismissReason::Replaced => ClientDismissReason::Replaced,
                        },
                    }
                }
            })
    }
}

fn parse_args() -> (u64, bool, PresentationPolicy) {
    let mut frames = 600;
    let mut require_hardware = false;
    let mut presentation_policy = PresentationPolicy::Anchored;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--frames" => {
                frames = args
                    .next()
                    .expect("--frames requires a value")
                    .parse()
                    .expect("--frames must be an integer")
            }
            "--require-hardware" => require_hardware = true,
            "--presentation" => {
                let value = args.next().expect("--presentation requires a value");
                presentation_policy = match value.as_str() {
                    "anchored" => PresentationPolicy::Anchored,
                    "in-place" => PresentationPolicy::InPlace,
                    "full-bar" => PresentationPolicy::FullBar,
                    value if value.starts_with("slot:") && value.len() > 5 => {
                        PresentationPolicy::Slot(value[5..].to_owned())
                    }
                    value if value.starts_with("region:") && value.len() > 7 => {
                        PresentationPolicy::Region(value[7..].to_owned())
                    }
                    _ => panic!("unsupported presentation policy: {value}"),
                };
            }
            "--help" | "-h" => {
                println!(
                    "usage: touchbar-ui-demo [--frames N] [--require-hardware] [--presentation anchored|in-place|slot:ID|region:ID|full-bar]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    (frames, require_hardware, presentation_policy)
}

fn main() -> Result<()> {
    let (max_frames, require_hardware, presentation_policy) = parse_args();
    if max_frames == 0 {
        bail!("--frames must be greater than zero");
    }
    let summary = run(
        ClientOptions::new("touchbar.ui-demo")
            .item_id("audio.volume")
            .compact_sizing(Sizing::new(64, 80, 80))
            .expanded_sizing(Sizing::new(220, 360, 500))
            .require_hardware(require_hardware),
        UiDemo {
            renderer: None,
            max_frames,
            interactions: InteractionState::default(),
            interaction_map: InteractionMap::default(),
            gestures: GestureArena::default(),
            gesture_map: GestureMap::default(),
            palette: {
                let mut palette = SelectionPalette::new(PALETTE, 5).gap(5.0);
                palette.set_selected(Some(2));
                palette
            },
            width: 0,
            height: 0,
            popover: false,
            popover_anchor: Rect::new(0.0, 0.0, 80.0, 60.0),
            presentations: PresentationController::default(),
            presentation_policy,
            last_contact: None,
            received_input: 0,
            theme: Theme::default(),
            ui: RetainedUi::new(UiDemo::compact_node(false)),
            scheduler: {
                let mut scheduler = FrameScheduler::default();
                scheduler.set_continuous(true);
                scheduler
            },
        },
    )?;
    println!(
        "client-summary plugin={} frames={} renderer={} ui=touchbar-ui",
        summary.plugin_id, summary.frames, summary.renderer_name
    );
    Ok(())
}
