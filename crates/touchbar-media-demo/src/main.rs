use std::{sync::Arc, time::Duration};

use anyhow::{Result, bail};
use touchbar_client::{
    AppearanceSnapshot, Application, ClientOptions, ContactPhase as ClientContactPhase, FrameFlow,
    FrameInfo, Graphics, PresentationDismissReason as ClientDismissReason,
    PresentationSessionEvent as ClientPresentationEvent,
    PresentationSessionRequest as ClientPresentationRequest,
    SessionPresentationLifecycle as ClientLifecycle, SessionPresentationPolicy as ClientPolicy,
    Sizing, SurfaceConfig, TouchContact, run,
};
use touchbar_ui::{
    Color, ColorRole, Contact, ContactPhase, ContinuousValue, CrossAxisAlignment, DismissReason,
    DismissalPolicy, Easing, Flex, FlexItem, Icon, Image, ImageFit, ImageTint, InteractionMap,
    InteractionState, MeterStyle, Motion, MotionId, MotionPlayback, Node, Point,
    PresentationCommand, PresentationController, PresentationLifecycle, PresentationPolicy,
    PresentationSessionId, PressableStyle, Rect, Representation, ResponsiveVariant, RetainedUi,
    SliderStyle, Symbol, SymbolCatalog, TextAlign, Theme, TinyGraphStyle, UiEvent, VisualTransform,
    WidgetId, gles,
};

const ROOT: WidgetId = WidgetId(100);
const PREVIOUS: WidgetId = WidgetId(101);
const PLAY_PAUSE: WidgetId = WidgetId(102);
const NEXT: WidgetId = WidgetId(103);
const TIMELINE: WidgetId = WidgetId(104);

#[derive(Clone, Copy, Debug)]
struct Track {
    title: &'static str,
    artist: &'static str,
    duration: f32,
    colors: [Color; 3],
}

const TRACKS: [Track; 3] = [
    Track {
        title: "Glass Skyline",
        artist: "TouchBar Sessions",
        duration: 214.0,
        colors: [
            Color::rgb(0.16, 0.72, 0.42),
            Color::rgb(0.05, 0.20, 0.16),
            Color::rgb(0.78, 0.96, 0.25),
        ],
    },
    Track {
        title: "Neon Terminal",
        artist: "Midnight Compiler",
        duration: 187.0,
        colors: [
            Color::rgb(0.50, 0.25, 0.96),
            Color::rgb(0.08, 0.04, 0.18),
            Color::rgb(0.20, 0.78, 0.96),
        ],
    },
    Track {
        title: "Touch the Horizon Across a Thousand Workspaces",
        artist: "Asahi Drive",
        duration: 246.0,
        colors: [
            Color::rgb(0.94, 0.34, 0.18),
            Color::rgb(0.22, 0.05, 0.04),
            Color::rgb(0.98, 0.78, 0.24),
        ],
    },
];

#[derive(Clone, Debug)]
struct Playback {
    track: usize,
    playing: bool,
    position: f32,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            track: 0,
            playing: true,
            position: 64.0,
        }
    }
}

impl Playback {
    fn current(&self) -> Track {
        TRACKS[self.track]
    }

    fn fraction(&self) -> f32 {
        (self.position / self.current().duration).clamp(0.0, 1.0)
    }

    fn advance(&mut self, delta: f32) {
        if !self.playing {
            return;
        }
        self.position += delta.max(0.0);
        if self.position >= self.current().duration {
            self.next();
        }
    }

    fn next(&mut self) {
        self.track = (self.track + 1) % TRACKS.len();
        self.position = 0.0;
    }

    fn previous(&mut self) {
        self.track = self.track.checked_sub(1).unwrap_or(TRACKS.len() - 1);
        self.position = 0.0;
    }
}

struct MediaDemo {
    renderer: Option<gles::Renderer>,
    max_frames: u64,
    width: u32,
    height: u32,
    theme: Theme,
    playback: Playback,
    artworks: Vec<Image>,
    media_symbol: Image,
    ui: RetainedUi,
    interactions: InteractionState,
    interaction_map: InteractionMap,
    presentations: PresentationController<()>,
    expanded: bool,
    pending_timeline_capture: Option<u32>,
    last_contact: Option<Contact>,
    last_elapsed: Option<f32>,
    demo_cycle: u32,
    track_transition_started: Duration,
    wave_phase: f32,
}

impl MediaDemo {
    fn compact_node(&self) -> Node {
        compact_media_node(
            self.playback.current(),
            self.artworks[self.playback.track].clone(),
            self.media_symbol.clone(),
            self.playback.fraction(),
            self.playback.playing,
            self.interactions.is_pressed(ROOT),
            self.interactions.is_pressed(PLAY_PAUSE),
            self.track_motion(),
        )
    }

    fn expanded_node(&self) -> Node {
        expanded_media_node(
            self.playback.current(),
            self.artworks[self.playback.track].clone(),
            self.playback.fraction(),
            self.playback.playing,
            self.interactions.is_pressed(PREVIOUS),
            self.interactions.is_pressed(PLAY_PAUSE),
            self.interactions.is_pressed(NEXT),
            self.track_motion(),
            self.wave_phase,
        )
    }

    fn track_motion(&self) -> Motion {
        Motion {
            id: MotionId(1),
            from: VisualTransform {
                translation: Point::new(7.0, 0.0),
                scale: 0.96,
                opacity: 0.0,
            },
            to: VisualTransform::IDENTITY,
            started: self.track_transition_started,
            duration: Duration::from_millis(260),
            easing: Easing::EaseInOut,
            playback: MotionPlayback::Once,
        }
    }

    fn rebuild_tree(&mut self) {
        let node = if self.expanded {
            self.expanded_node()
        } else {
            self.compact_node()
        };
        self.ui.replace(node);
    }

    fn present(&mut self, lifecycle: PresentationLifecycle, now: Duration) {
        let dismissal = match lifecycle {
            PresentationLifecycle::Transient { .. } => DismissalPolicy::transient(),
            PresentationLifecycle::Persistent => {
                DismissalPolicy::persistent(Some(Duration::from_secs(8)))
            }
        };
        let session =
            self.presentations
                .present(PresentationPolicy::Anchored, lifecycle, dismissal, (), now);
        println!(
            "media-presentation=request session={} lifecycle={lifecycle:?}",
            session.0
        );
    }

    fn consume_events(&mut self, events: Vec<UiEvent>) {
        let now = self
            .last_contact
            .map_or(Duration::ZERO, |contact| contact.time);
        for event in events {
            match event {
                UiEvent::Activated { id: ROOT } => {
                    self.present(PresentationLifecycle::Persistent, now);
                }
                UiEvent::LongPressed { id: ROOT, contact } => {
                    self.pending_timeline_capture = Some(contact);
                    self.present(PresentationLifecycle::Transient { contact }, now);
                }
                UiEvent::Activated { id: PLAY_PAUSE } => {
                    self.playback.playing = !self.playback.playing;
                    println!("media-action=play-pause playing={}", self.playback.playing);
                }
                UiEvent::Activated { id: PREVIOUS } => {
                    self.playback.previous();
                    self.track_transition_started = now;
                    println!(
                        "media-action=previous track={}",
                        self.playback.current().title
                    );
                }
                UiEvent::Activated { id: NEXT } => {
                    self.playback.next();
                    self.track_transition_started = now;
                    println!("media-action=next track={}", self.playback.current().title);
                }
                UiEvent::ValueChanged {
                    id: TIMELINE,
                    value,
                } => {
                    self.playback.position = self.playback.current().duration * value;
                    println!("media-action=seek value={value:.3}");
                }
                _ => {}
            }
        }
    }
}

impl Application for MediaDemo {
    fn appearance_changed(&mut self, appearance: AppearanceSnapshot) -> Result<bool> {
        self.theme = appearance.into();
        self.ui.invalidate();
        println!(
            "media-appearance generation={} scheme={:?} accent=#{:08x}",
            appearance.generation,
            appearance.scheme,
            appearance.accent.packed()
        );
        Ok(true)
    }

    fn configured(&mut self, graphics: &Graphics, config: SurfaceConfig) -> Result<()> {
        if self.renderer.is_none() {
            self.renderer = Some(gles::Renderer::new(graphics.gl())?);
        }
        self.width = config.width;
        self.height = config.height;
        self.rebuild_tree();
        println!(
            "configured plugin=touchbar.media-demo region={}x{} renderer={} transport=dmabuf expanded={}",
            config.width,
            config.height,
            graphics.renderer_name(),
            self.expanded
        );
        Ok(())
    }

    fn render(&mut self, graphics: &Graphics, frame: FrameInfo) -> Result<FrameFlow> {
        let elapsed = frame.elapsed_seconds;
        let now = Duration::from_secs_f32(elapsed);
        let delta = self
            .last_elapsed
            .replace(elapsed)
            .map_or(0.0, |previous| elapsed - previous);
        let previous_track = self.playback.track;
        self.playback.advance(delta);
        if self.playback.track != previous_track {
            self.track_transition_started = now;
        }
        let demo_cycle = (elapsed / 8.0) as u32;
        if demo_cycle > self.demo_cycle {
            self.demo_cycle = demo_cycle;
            self.playback.next();
            self.track_transition_started = now;
            println!(
                "media-simulation=track-change track={}",
                self.playback.current().title
            );
        }
        self.wave_phase = elapsed;
        self.presentations.tick(now);
        self.rebuild_tree();

        let resolved = self.ui.resolve_with_measurer(
            Rect::new(
                4.0,
                4.0,
                (frame.surface.width as f32 - 8.0).max(1.0),
                (frame.surface.height as f32 - 8.0).max(1.0),
            ),
            self.theme,
            self.renderer
                .as_ref()
                .expect("renderer initialized during configure"),
        );
        self.interaction_map = resolved.interactions;
        if let Some(contact) = self.pending_timeline_capture.take()
            && let Some(target) = self.interaction_map.target(TIMELINE)
        {
            let transferred = self.interactions.transfer_capture(contact, target);
            println!("media-capture=timeline contact={contact} transferred={transferred}");
        }
        if frame.number.is_multiple_of(120) {
            let representation = resolved
                .inspector
                .representations
                .get(&ROOT)
                .copied()
                .unwrap_or(Representation::Full);
            println!(
                "media-inspector revision={} representation={representation:?} track={} progress={:.3}",
                resolved.inspector.revision,
                self.playback.current().title,
                self.playback.fraction()
            );
        }
        let timed_events = self
            .interactions
            .tick(Duration::from_secs_f32(frame.elapsed_seconds));
        self.consume_events(timed_events);
        self.renderer
            .as_ref()
            .expect("renderer initialized during configure")
            .draw_at(
                graphics.gl(),
                &resolved.scene,
                frame.surface.width,
                frame.surface.height,
                now,
            )?;

        Ok(if frame.number + 1 >= self.max_frames {
            FrameFlow::Exit
        } else {
            // The showcase keeps requesting frames for its marquee and waveform.
            FrameFlow::Animate
        })
    }

    fn visibility_changed(&mut self, visible: bool) -> Result<bool> {
        self.ui.set_visible(visible);
        Ok(visible)
    }

    fn touch(&mut self, contact: TouchContact) -> Result<bool> {
        let phase = match contact.phase {
            ClientContactPhase::Down => ContactPhase::Down,
            ClientContactPhase::Motion => ContactPhase::Motion,
            ClientContactPhase::Up => ContactPhase::Up,
            ClientContactPhase::Cancel => ContactPhase::Cancel,
        };
        let local = Contact {
            id: contact.id,
            phase,
            position: Point::new(contact.x, contact.y),
            time: contact.time,
        };
        self.last_contact = Some(local);
        if phase == ContactPhase::Down {
            self.presentations.activity(contact.time);
        }
        let events = self.interactions.handle(&self.interaction_map, local);
        self.consume_events(events);
        if phase == ContactPhase::Up {
            self.presentations.released(contact.id);
        }
        self.rebuild_tree();
        Ok(true)
    }

    fn presentation_session_changed(&mut self, event: ClientPresentationEvent) -> Result<()> {
        match event {
            ClientPresentationEvent::Anchor { session_id, anchor } => {
                self.presentations.set_anchor(
                    PresentationSessionId(session_id),
                    Rect::new(anchor.x, 0.0, anchor.width, self.height as f32),
                );
            }
            ClientPresentationEvent::Started { session_id, .. } => {
                if self.presentations.active().map(|session| session.id())
                    == Some(PresentationSessionId(session_id))
                {
                    self.expanded = true;
                    println!("media-presentation=started session={session_id}");
                }
            }
            ClientPresentationEvent::Ended { session_id, reason } => {
                if self
                    .presentations
                    .compositor_dismissed(PresentationSessionId(session_id))
                {
                    self.expanded = false;
                    self.pending_timeline_capture = None;
                    println!("media-presentation=ended session={session_id} reason={reason:?}");
                }
            }
        }
        self.rebuild_tree();
        Ok(())
    }

    fn take_presentation_session_request(&mut self) -> Option<ClientPresentationRequest> {
        self.presentations
            .take_command()
            .map(map_presentation_command)
    }
}

#[allow(clippy::too_many_arguments)]
fn compact_media_node(
    track: Track,
    artwork: Image,
    media_symbol: Image,
    progress: f32,
    playing: bool,
    root_pressed: bool,
    play_pressed: bool,
    motion: Motion,
) -> Node {
    let wrap = |content: Node| Node::Pressable {
        id: ROOT,
        label: format!("Now playing: {} by {}", track.title, track.artist),
        pressed: root_pressed,
        hold: Some(Duration::from_millis(350)),
        selected: None,
        style: PressableStyle {
            padding: 4.0,
            ..PressableStyle::control()
        },
        child: Box::new(content),
    };
    let minimal = wrap(Node::motion(
        motion,
        Node::Layer(vec![
            Node::Image {
                image: media_symbol,
                opacity: 1.0,
                fit: ImageFit::Contain,
                tint: ImageTint::Mask(ColorRole::Foreground),
                label: "Media".into(),
            },
            Node::positioned(
                Rect::new(4.0, 42.0, 64.0, 3.0),
                Node::meter(
                    "Track position",
                    ContinuousValue::unit(progress),
                    None,
                    MeterStyle::default(),
                ),
            ),
        ]),
    ));
    let compact = wrap(Node::motion(
        motion,
        Node::row_aligned(
            4.0,
            0.0,
            CrossAxisAlignment::Center,
            vec![
                FlexItem::new(
                    Flex::fixed(36.0).priority(-1),
                    Node::Image {
                        image: artwork.clone(),
                        opacity: 1.0,
                        fit: ImageFit::Cover,
                        tint: ImageTint::None,
                        label: format!("Artwork for {}", track.title),
                    },
                ),
                FlexItem::new(
                    Flex::flexible(25.0, 80.0, 260.0).grow(1.0).required(),
                    info_column(track, progress),
                ),
                FlexItem::new(
                    Flex::fixed(36.0).required(),
                    transport_button(
                        PLAY_PAUSE,
                        if playing { Icon::Pause } else { Icon::Play },
                        if playing { "Pause" } else { "Play" },
                        play_pressed,
                        true,
                    ),
                ),
            ],
        ),
    ));
    let full = wrap(Node::motion(
        motion,
        Node::row_aligned(
            5.0,
            0.0,
            CrossAxisAlignment::Center,
            vec![
                FlexItem::new(
                    Flex::fixed(44.0).priority(-1),
                    Node::Image {
                        image: artwork,
                        opacity: 1.0,
                        fit: ImageFit::Cover,
                        tint: ImageTint::None,
                        label: format!("Artwork for {}", track.title),
                    },
                ),
                FlexItem::new(
                    Flex::flexible(60.0, 180.0, 300.0).grow(1.0).required(),
                    info_column(track, progress),
                ),
                FlexItem::new(
                    Flex::fixed(40.0).required(),
                    transport_button(
                        PLAY_PAUSE,
                        if playing { Icon::Pause } else { Icon::Play },
                        if playing { "Pause" } else { "Play" },
                        play_pressed,
                        true,
                    ),
                ),
            ],
        ),
    ));
    Node::responsive(
        ROOT,
        vec![
            ResponsiveVariant::new(Representation::Minimal, 0.0, minimal),
            ResponsiveVariant::new(Representation::Compact, 112.0, compact),
            ResponsiveVariant::new(Representation::Full, 280.0, full),
        ],
    )
}

fn info_column(track: Track, progress: f32) -> Node {
    Node::column_aligned(
        1.0,
        0.0,
        CrossAxisAlignment::Stretch,
        vec![
            FlexItem::new(
                Flex::content(8.0, 17.0),
                Node::Label {
                    text: track.title.into(),
                    size: 11.0,
                    color: ColorRole::Foreground,
                    align: TextAlign::Leading,
                    overflow: touchbar_ui::TextOverflow::Marquee {
                        speed: 24.0,
                        gap: 28.0,
                    },
                    measurement: touchbar_ui::TextMeasurement::Content,
                },
            ),
            FlexItem::new(
                Flex::content(7.0, 13.0).priority(-1),
                Node::Label {
                    text: track.artist.into(),
                    size: 9.0,
                    color: ColorRole::Muted,
                    align: TextAlign::Leading,
                    overflow: touchbar_ui::TextOverflow::Ellipsis,
                    measurement: touchbar_ui::TextMeasurement::Content,
                },
            ),
            FlexItem::new(
                Flex::fixed(4.0),
                Node::meter(
                    "Track position",
                    ContinuousValue::unit(progress),
                    None,
                    MeterStyle::default(),
                ),
            ),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
fn expanded_media_node(
    track: Track,
    artwork: Image,
    progress: f32,
    playing: bool,
    previous_pressed: bool,
    play_pressed: bool,
    next_pressed: bool,
    motion: Motion,
    wave_phase: f32,
) -> Node {
    Node::motion(
        motion,
        Node::row_aligned(
            4.0,
            0.0,
            CrossAxisAlignment::Center,
            vec![
                FlexItem::new(
                    Flex::fixed(48.0),
                    Node::Image {
                        image: artwork,
                        opacity: 1.0,
                        fit: ImageFit::Cover,
                        tint: ImageTint::None,
                        label: format!("Artwork for {}", track.title),
                    },
                ),
                FlexItem::new(
                    Flex::flexible(80.0, 210.0, 400.0).grow(1.0).required(),
                    Node::column_aligned(
                        1.0,
                        0.0,
                        CrossAxisAlignment::Stretch,
                        vec![
                            FlexItem::new(
                                Flex::fixed(14.0),
                                Node::Label {
                                    text: format!("{} — {}", track.title, track.artist),
                                    size: 11.0,
                                    color: ColorRole::Foreground,
                                    align: TextAlign::Leading,
                                    overflow: touchbar_ui::TextOverflow::Marquee {
                                        speed: 24.0,
                                        gap: 28.0,
                                    },
                                    measurement: touchbar_ui::TextMeasurement::Content,
                                },
                            ),
                            FlexItem::new(Flex::fixed(26.0), timeline_node(track, progress)),
                            FlexItem::new(
                                Flex::fixed(10.0),
                                Node::tiny_graph(
                                    "Audio waveform",
                                    waveform_samples(wave_phase),
                                    0.0,
                                    1.0,
                                    TinyGraphStyle::default(),
                                ),
                            ),
                        ],
                    ),
                ),
                FlexItem::new(
                    Flex::fixed(38.0),
                    transport_button(
                        PREVIOUS,
                        Icon::ChevronLeft,
                        "Previous",
                        previous_pressed,
                        false,
                    ),
                ),
                FlexItem::new(
                    Flex::fixed(42.0),
                    transport_button(
                        PLAY_PAUSE,
                        if playing { Icon::Pause } else { Icon::Play },
                        if playing { "Pause" } else { "Play" },
                        play_pressed,
                        true,
                    ),
                ),
                FlexItem::new(
                    Flex::fixed(38.0),
                    transport_button(NEXT, Icon::ChevronRight, "Next", next_pressed, false),
                ),
            ],
        ),
    )
}

fn timeline_node(track: Track, progress: f32) -> Node {
    let elapsed = track.duration * progress;
    Node::row_aligned(
        3.0,
        0.0,
        CrossAxisAlignment::Center,
        vec![
            FlexItem::new(
                Flex::fixed(31.0),
                Node::stable_label(format_time(elapsed), "00:00", 8.0),
            ),
            FlexItem::new(
                Flex::flexible(24.0, 120.0, 400.0).grow(1.0).required(),
                Node::styled_slider(
                    TIMELINE,
                    "Media timeline",
                    ContinuousValue::unit(progress),
                    SliderStyle {
                        track_height: 5.0,
                        thumb_diameter: 14.0,
                        tick_count: 9,
                        ..SliderStyle::default()
                    },
                ),
            ),
            FlexItem::new(
                Flex::fixed(31.0),
                Node::stable_label(format_time(track.duration), "00:00", 8.0),
            ),
        ],
    )
}

fn format_time(seconds: f32) -> String {
    let seconds = seconds.max(0.0) as u32;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn waveform_samples(phase: f32) -> Arc<[f32]> {
    (0..36)
        .map(|index| ((index as f32 * 0.71 + phase * 3.2).sin() * 0.5 + 0.5).max(0.12))
        .collect::<Vec<_>>()
        .into()
}

#[cfg(test)]
fn settled_motion() -> Motion {
    Motion {
        id: MotionId(1),
        from: VisualTransform::IDENTITY,
        to: VisualTransform::IDENTITY,
        started: Duration::ZERO,
        duration: Duration::ZERO,
        easing: Easing::Linear,
        playback: MotionPlayback::Once,
    }
}

fn transport_button(id: WidgetId, icon: Icon, label: &str, pressed: bool, accented: bool) -> Node {
    Node::Pressable {
        id,
        label: label.into(),
        pressed,
        hold: None,
        selected: None,
        style: if accented {
            PressableStyle::accent()
        } else {
            PressableStyle::control()
        },
        child: Box::new(Node::Icon {
            icon,
            color: ColorRole::Foreground,
            label: label.into(),
        }),
    }
}

fn artwork(track: Track) -> Image {
    let size = 48_u32;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let diagonal = (x + y) as f32 / (size * 2 - 2) as f32;
            let stripe = ((x / 8 + y / 8) % 2) as usize;
            let base = track.colors[stripe];
            let color = base.mix(track.colors[2], diagonal * 0.45);
            pixels.extend_from_slice(&[
                (color.red * 255.0) as u8,
                (color.green * 255.0) as u8,
                (color.blue * 255.0) as u8,
                255,
            ]);
        }
    }
    let revision = TRACKS
        .iter()
        .position(|candidate| candidate.title == track.title)
        .unwrap_or_default() as u64
        + 1;
    Image::rgba8(500, revision, size, size, pixels).expect("generated artwork is valid RGBA")
}

fn map_presentation_command(command: PresentationCommand) -> ClientPresentationRequest {
    match command {
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
                PresentationLifecycle::Transient { contact } => ClientLifecycle::Transient {
                    contact_id: contact,
                },
                PresentationLifecycle::Persistent => ClientLifecycle::Persistent,
            },
        },
        PresentationCommand::Dismiss { session, reason } => ClientPresentationRequest::End {
            session_id: session.0,
            reason: match reason {
                DismissReason::Requested => ClientDismissReason::Requested,
                DismissReason::Selection => ClientDismissReason::Selection,
                DismissReason::OutsidePress => ClientDismissReason::OutsidePress,
                DismissReason::Timeout => ClientDismissReason::Timeout,
                DismissReason::SourceHidden => ClientDismissReason::SourceHidden,
                DismissReason::Replaced => ClientDismissReason::Replaced,
            },
        },
    }
}

fn parse_args() -> (u64, Option<u32>, bool) {
    let mut frames = 600;
    let mut width = None;
    let mut require_hardware = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--frames" => {
                frames = args
                    .next()
                    .expect("--frames requires a value")
                    .parse()
                    .expect("--frames must be an integer");
            }
            "--width" => {
                let parsed = args
                    .next()
                    .expect("--width requires a value")
                    .parse()
                    .expect("--width must be an integer");
                width = Some(parsed);
            }
            "--require-hardware" => require_hardware = true,
            "--help" | "-h" => {
                println!(
                    "usage: touchbar-media-demo [--frames N] [--width N] [--require-hardware]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    (frames, width, require_hardware)
}

fn main() -> Result<()> {
    let (max_frames, fixed_width, require_hardware) = parse_args();
    if max_frames == 0 {
        bail!("--frames must be greater than zero");
    }
    if fixed_width.is_some_and(|width| width == 0 || width > 2008) {
        bail!("--width must be between 1 and 2008");
    }
    let sizing = fixed_width.map_or(Sizing::new(80, 160, 420), |width| {
        Sizing::new(width, width, width)
    });
    let artworks = TRACKS.iter().copied().map(artwork).collect();
    let media_symbol = SymbolCatalog::default().image(Symbol::Graph, 32)?;
    let summary = run(
        ClientOptions::new("touchbar.media-demo")
            .item_id("media.now-playing")
            .compact_sizing(sizing)
            .expanded_sizing(Sizing::new(320, 420, 700))
            .require_hardware(require_hardware),
        MediaDemo {
            renderer: None,
            max_frames,
            width: 0,
            height: 0,
            theme: Theme::default(),
            playback: Playback::default(),
            artworks,
            media_symbol,
            ui: RetainedUi::new(Node::Empty),
            interactions: InteractionState::default(),
            interaction_map: InteractionMap::default(),
            presentations: PresentationController::default(),
            expanded: false,
            pending_timeline_capture: None,
            last_contact: None,
            last_elapsed: None,
            demo_cycle: 0,
            track_transition_started: Duration::ZERO,
            wave_phase: 0.0,
        },
    )?;
    println!(
        "media-summary plugin={} frames={} renderer={}",
        summary.plugin_id, summary.frames, summary.renderer_name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_media_selects_minimal_compact_and_full_representations() {
        let track = TRACKS[0];
        for (width, expected) in [
            (80.0, Representation::Minimal),
            (160.0, Representation::Compact),
            (420.0, Representation::Full),
        ] {
            let mut ui = RetainedUi::new(compact_media_node(
                track,
                artwork(track),
                SymbolCatalog::default().image(Symbol::Graph, 32).unwrap(),
                0.25,
                true,
                false,
                false,
                settled_motion(),
            ));
            let resolved = ui.resolve(Rect::new(0.0, 0.0, width, 60.0), Theme::default());
            assert_eq!(
                resolved.inspector.representations.get(&ROOT),
                Some(&expected)
            );
            assert!(resolved.interactions.target(ROOT).is_some());
        }
    }

    #[test]
    fn expanded_media_exposes_stable_transport_and_timeline_targets() {
        let mut ui = RetainedUi::new(expanded_media_node(
            TRACKS[0],
            artwork(TRACKS[0]),
            0.5,
            true,
            false,
            false,
            false,
            settled_motion(),
            0.0,
        ));
        let resolved = ui.resolve(Rect::new(0.0, 0.0, 420.0, 60.0), Theme::default());
        assert!(resolved.interactions.target(PREVIOUS).is_some());
        assert!(resolved.interactions.target(PLAY_PAUSE).is_some());
        assert!(resolved.interactions.target(NEXT).is_some());
        assert!(resolved.interactions.target(TIMELINE).is_some());
    }

    #[test]
    fn playback_wraps_tracks_and_seek_fraction_is_clamped() {
        let mut playback = Playback {
            track: TRACKS.len() - 1,
            ..Playback::default()
        };
        playback.next();
        assert_eq!(playback.track, 0);
        playback.position = playback.current().duration * 2.0;
        assert_eq!(playback.fraction(), 1.0);
        playback.previous();
        assert_eq!(playback.track, TRACKS.len() - 1);
    }
}
