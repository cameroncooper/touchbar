use std::{collections::VecDeque, env, fs, path::PathBuf, time::Instant};

use anyhow::{Context, Result, bail};
use touchbar_client::{
    AppearanceSnapshot, Application, ClientOptions, ColorScheme as ClientColorScheme,
    ContactOrigin, ContactPhase as ClientContactPhase, FrameFlow, FrameInfo, Graphics,
    MotionPolicy as ClientMotionPolicy, PresentationDismissReason, PresentationSessionEvent,
    PresentationSessionRequest, SessionPresentationLifecycle, SessionPresentationPolicy, Sizing,
    SurfaceConfig, TouchContact, run,
};
use touchbar_package::{MANIFEST_FILE_NAME, MAX_TOUCHBAR_WIDTH, PluginManifest, RuntimeSpec};
use touchbar_plugin_host::{
    ASSET_BUNDLE_FD_ENV, Appearance, BrokerClient, COMPONENT_FD_ENV, ColorScheme,
    ComponentPresentationCommand, ComponentPresentationDismissal, ComponentPresentationEndReason,
    ComponentPresentationEvent, ComponentPresentationLifecycle, ComponentPresentationPlacement,
    ComponentUpdate, HostLimits, HostedItem, InputActivation, InputEvent, InputKind,
    MANIFEST_FD_ENV, MAX_INHERITED_ASSET_BUNDLE_BYTES, MAX_INHERITED_COMPONENT_BYTES,
    MAX_INHERITED_MANIFEST_BYTES, PackageAssets, PluginHost, apply_component_confinement,
    read_supervisor_file,
};
use touchbar_protocol::broker_ipc::ActivationOrigin;
use touchbar_ui::{
    Contact, ContactPhase, InteractionMap, InteractionState, MotionPolicy, Point, Rect, RetainedUi,
    SemanticNode, UiEvent, gles,
};

mod replay;
mod replay_gpu;

const USAGE: &str = "usage:\n  touchbar-plugin-host PACKAGE [ITEM] [WIDTH] [ACTIVATE_WIDGET]\n  touchbar-plugin-host PACKAGE --replay SCENARIO.json [--screenshots DIRECTORY]\n  touchbar-plugin-host PACKAGE --live [--item ITEM] [--width WIDTH] [--frames N] [--require-hardware]";

struct OpenedPackage {
    root: PathBuf,
    manifest: PluginManifest,
    component: PathBuf,
    world: String,
    limits: HostLimits,
    host: PluginHost,
    items: Vec<HostedItem>,
    replay_broker: Option<replay::ReplayBrokerController>,
}

enum Mode {
    Headless {
        item: Option<String>,
        width: f32,
        activate_widget: Option<u64>,
    },
    Live {
        item: Option<String>,
        width: u32,
        max_frames: Option<u64>,
        require_hardware: bool,
    },
    Replay {
        scenario: PathBuf,
        screenshots: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let (package, mode) = parse_args()?;
    let live = matches!(&mode, Mode::Live { .. });
    let replay_scenario = match &mode {
        Mode::Replay { scenario, .. } => Some(replay::Scenario::load(scenario)?),
        _ => None,
    };
    let opened = open_package(package, live, replay_scenario.as_ref())?;
    match mode {
        Mode::Headless {
            item,
            width,
            activate_widget,
        } => run_headless(opened, item, width, activate_widget),
        Mode::Live {
            item,
            width,
            max_frames,
            require_hardware,
        } => run_live(opened, item, width, max_frames, require_hardware),
        Mode::Replay { screenshots, .. } => replay::run(
            opened.host,
            &opened.items,
            replay_scenario.expect("loaded for replay mode"),
            opened.replay_broker,
            screenshots.as_deref(),
        ),
    }
}

fn parse_args() -> Result<(PathBuf, Mode)> {
    let mut arguments = env::args().skip(1);
    let package = arguments.next().context(USAGE)?;
    if package == "--help" || package == "-h" {
        println!("{USAGE}");
        std::process::exit(0);
    }
    let package = PathBuf::from(package);
    let remaining = arguments.collect::<Vec<_>>();
    if remaining.first().is_some_and(|value| value == "--replay") {
        let scenario = remaining
            .get(1)
            .context("--replay requires a scenario path")?;
        let mut screenshots = None;
        let mut index = 2;
        while index < remaining.len() {
            match remaining[index].as_str() {
                "--screenshots" => {
                    screenshots = Some(PathBuf::from(required_value(
                        &remaining,
                        index,
                        "--screenshots",
                    )?));
                    index += 2;
                }
                other => bail!("unknown replay option {other}\n{USAGE}"),
            }
        }
        return Ok((
            package,
            Mode::Replay {
                scenario: PathBuf::from(scenario),
                screenshots,
            },
        ));
    }
    if !remaining.iter().any(|argument| argument == "--live") {
        if remaining
            .iter()
            .any(|argument| argument == "--help" || argument == "-h")
        {
            println!("{USAGE}");
            std::process::exit(0);
        }
        if remaining.len() > 3 {
            bail!(USAGE);
        }
        let item = remaining.first().cloned();
        let width = remaining
            .get(1)
            .map(|value| value.parse::<f32>())
            .transpose()
            .context("WIDTH must be a number")?
            .unwrap_or(160.0);
        let activate_widget = remaining
            .get(2)
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("ACTIVATE_WIDGET must be an integer")?;
        return Ok((
            package,
            Mode::Headless {
                item,
                width,
                activate_widget,
            },
        ));
    }

    let mut item = None;
    let mut width = 160_u32;
    let mut max_frames = None;
    let mut require_hardware = false;
    let mut index = 0;
    while index < remaining.len() {
        match remaining[index].as_str() {
            "--live" => index += 1,
            "--item" => {
                item = Some(required_value(&remaining, index, "--item")?.to_owned());
                index += 2;
            }
            "--width" => {
                width = required_value(&remaining, index, "--width")?
                    .parse()
                    .context("--width must be an integer")?;
                index += 2;
            }
            "--frames" => {
                let frames = required_value(&remaining, index, "--frames")?
                    .parse::<u64>()
                    .context("--frames must be an integer")?;
                if frames == 0 {
                    bail!("--frames must be greater than zero");
                }
                max_frames = Some(frames);
                index += 2;
            }
            "--require-hardware" => {
                require_hardware = true;
                index += 1;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown live option {other}\n{USAGE}"),
        }
    }
    if !(1..=MAX_TOUCHBAR_WIDTH).contains(&width) {
        bail!("--width must be between 1 and {MAX_TOUCHBAR_WIDTH}");
    }
    Ok((
        package,
        Mode::Live {
            item,
            width,
            max_frames,
            require_hardware,
        },
    ))
}

fn required_value<'a>(arguments: &'a [String], index: usize, option: &str) -> Result<&'a str> {
    arguments
        .get(index + 1)
        .map(String::as_str)
        .with_context(|| format!("{option} requires a value"))
}

fn open_package(
    package: PathBuf,
    live: bool,
    replay_scenario: Option<&replay::Scenario>,
) -> Result<OpenedPackage> {
    let root = package
        .canonicalize()
        .with_context(|| format!("open package {}", package.display()))?;
    if !root.is_dir() {
        bail!("package path must be a directory");
    }
    let broker = BrokerClient::from_environment()?;
    let limits = HostLimits::default();
    let supervised = broker.is_some();
    if supervised && replay_scenario.is_some() {
        bail!("replay cannot use an inherited production broker");
    }
    let inherited_manifest = read_supervisor_file(MANIFEST_FD_ENV, MAX_INHERITED_MANIFEST_BYTES)?;
    let inherited_component =
        read_supervisor_file(COMPONENT_FD_ENV, MAX_INHERITED_COMPONENT_BYTES)?;
    let inherited_assets =
        read_supervisor_file(ASSET_BUNDLE_FD_ENV, MAX_INHERITED_ASSET_BUNDLE_BYTES)?;
    let (manifest, component, mut host, replay_broker) = match (
        supervised,
        inherited_manifest,
        inherited_component,
        inherited_assets,
    ) {
        (true, Some(manifest), Some(component), assets) => {
            apply_component_confinement(live)?;
            let manifest = PluginManifest::from_toml(
                std::str::from_utf8(&manifest).context("verified manifest is not UTF-8")?,
            )?;
            let assets = match (manifest.assets.is_empty(), assets) {
                (true, None) => PackageAssets::default(),
                (false, Some(bundle)) => PackageAssets::decode(&manifest.assets, &bundle)?,
                (true, Some(_)) => bail!("supervisor supplied an unsolicited asset bundle"),
                (false, None) => {
                    bail!("supervised host requires its declared sealed asset bundle")
                }
            };
            let display = match &manifest.runtime {
                RuntimeSpec::Component { entrypoint, .. } => root.join(entrypoint),
                RuntimeSpec::Native { .. } => {
                    bail!("touchbar-plugin-host requires a component package")
                }
            };
            let mut host = PluginHost::from_bytes_with_broker(&component, limits, broker)?;
            host.set_assets(assets);
            (manifest, display, host, None)
        }
        (true, _, _, _) => {
            bail!("supervised host requires sealed component and manifest descriptors")
        }
        (false, None, None, None) => {
            let manifest_path = root.join(MANIFEST_FILE_NAME);
            let manifest = PluginManifest::from_toml(
                &fs::read_to_string(&manifest_path)
                    .with_context(|| format!("read {}", manifest_path.display()))?,
            )?;
            let entrypoint = match &manifest.runtime {
                RuntimeSpec::Component { entrypoint, .. } => entrypoint.clone(),
                RuntimeSpec::Native { .. } => {
                    bail!("touchbar-plugin-host requires a component package")
                }
            };
            let component = root.join(&entrypoint).canonicalize().with_context(|| {
                format!(
                    "resolve component entrypoint {}",
                    root.join(&entrypoint).display()
                )
            })?;
            if !component.starts_with(&root) {
                bail!("component entrypoint resolves outside its package");
            }
            let assets = PackageAssets::from_directory(&root, &manifest.assets)?;
            let (broker, replay_broker) = replay_scenario
                .map(|scenario| scenario.replay_broker(&manifest))
                .transpose()?
                .flatten()
                .map_or((None, None), |(broker, controller)| {
                    (Some(broker), Some(controller))
                });
            let mut host = PluginHost::from_file_with_broker(&component, limits, broker)?;
            host.set_assets(assets);
            (manifest, component, host, replay_broker)
        }
        (false, _, _, _) => bail!("sealed package descriptors require supervisor transport"),
    };
    let world = match &manifest.runtime {
        RuntimeSpec::Component { world, .. } => world.clone(),
        RuntimeSpec::Native { .. } => bail!("touchbar-plugin-host requires a component package"),
    };
    let items = host.items()?;
    let declared = manifest
        .items
        .iter()
        .map(|item| (&item.id, &item.label))
        .collect::<Vec<_>>();
    let exported = items
        .iter()
        .map(|item| (&item.id, &item.label))
        .collect::<Vec<_>>();
    if exported != declared {
        bail!("component-exported items do not exactly match the package manifest");
    }

    Ok(OpenedPackage {
        root,
        manifest,
        component,
        world,
        limits,
        host,
        items,
        replay_broker,
    })
}

fn select_item(items: &[HostedItem], requested: Option<String>) -> Result<String> {
    let item = requested
        .or_else(|| items.first().map(|item| item.id.clone()))
        .context("component exported no items")?;
    if !items.iter().any(|candidate| candidate.id == item) {
        bail!("component does not export item {item}");
    }
    Ok(item)
}

fn print_package(opened: &OpenedPackage) {
    println!(
        "package: {} {}",
        opened.manifest.plugin.name, opened.manifest.plugin.version
    );
    println!("source: {}", opened.manifest.plugin.source);
    println!("world: {}", opened.world);
    println!("sandboxed component: {}", opened.component.display());
    println!("package root: {}", opened.root.display());
    match opened.host.broker_generation() {
        Some(generation) => {
            println!("broker: supervised generation={generation}");
            for state in opened
                .host
                .broker_states()
                .expect("broker states exist with a generation")
            {
                println!(
                    "capability: {} required={} status={:?}",
                    state.capability, state.required, state.status
                );
            }
        }
        None => {
            println!("broker: unavailable (manual launch; default deny)");
            for permission in &opened.manifest.permissions {
                println!(
                    "capability: {} required={} status=Unavailable",
                    permission.capability, permission.required
                );
            }
        }
    }
    println!(
        "limits: {} MiB memory, {} fuel/call, {} UI nodes",
        opened.limits.memory_bytes / (1024 * 1024),
        opened.limits.fuel_per_call,
        opened.limits.max_nodes
    );
    for item in &opened.items {
        println!("item: {} ({})", item.id, item.label);
    }
}

fn run_headless(
    mut opened: OpenedPackage,
    requested_item: Option<String>,
    width: f32,
    activate_widget: Option<u64>,
) -> Result<()> {
    print_package(&opened);
    let item = select_item(&opened.items, requested_item)?;
    let viewport = Rect::new(0.0, 0.0, width, 60.0);
    let appearance = Appearance::default();
    let mut ui = opened.host.render(&item, viewport, appearance)?;
    let resolved = ui.resolve(viewport, appearance.theme);
    println!("resolved {item} at {width}x60:");
    print_semantics(&resolved.inspector.semantics, 0);
    println!("primitives: {}", resolved.scene.primitives.len());

    if let Some(widget_id) = activate_widget {
        let rerender = opened.host.handle_event(&InputEvent {
            item_id: item.clone(),
            widget_id,
            kind: InputKind::Activated,
            value: None,
            contact_id: None,
            activation: None,
        })?;
        println!("activation {widget_id}: rerender={}", rerender.rerender);
        if rerender.rerender {
            let mut ui = opened.host.render(&item, viewport, appearance)?;
            let resolved = ui.resolve(viewport, appearance.theme);
            print_semantics(&resolved.inspector.semantics, 0);
        }
    }
    Ok(())
}

fn run_live(
    opened: OpenedPackage,
    requested_item: Option<String>,
    width: u32,
    max_frames: Option<u64>,
    require_hardware: bool,
) -> Result<()> {
    print_package(&opened);
    let item = select_item(&opened.items, requested_item)?;
    let plugin_id = opened.manifest.plugin.source.to_string();
    let expanded_sizing = manifest_presentation_sizing(&opened.manifest, &item);
    let mut options = ClientOptions::new(&plugin_id)
        .item_id(&item)
        .compact_sizing(Sizing::new(width, width, width))
        .require_hardware(require_hardware);
    if let Some(sizing) = expanded_sizing {
        options = options.expanded_sizing(sizing);
    }
    let summary = run(
        options,
        ComponentApplication {
            host: opened.host,
            item,
            renderer: None,
            ui: None,
            interactions: InteractionState::default(),
            interaction_map: InteractionMap::default(),
            appearance: appearance_from_snapshot(AppearanceSnapshot::default()),
            width: 0,
            height: 0,
            max_frames,
            started: Instant::now(),
            next_presentation_session: 1,
            active_presentation_session: None,
            presentation_requests: VecDeque::new(),
        },
    )?;
    println!(
        "client-summary plugin={} frames={} renderer={} runtime=component",
        summary.plugin_id, summary.frames, summary.renderer_name
    );
    Ok(())
}

fn manifest_presentation_sizing(manifest: &PluginManifest, item: &str) -> Option<Sizing> {
    let item = manifest
        .items
        .iter()
        .find(|candidate| candidate.id == item)?;
    let ids = [
        item.expanded_bar.as_deref(),
        item.press_and_hold_bar.as_deref(),
    ];
    let bars = ids
        .into_iter()
        .flatten()
        .filter_map(|id| manifest.bars.iter().find(|bar| bar.id == id))
        .collect::<Vec<_>>();
    Some(Sizing::new(
        bars.iter().map(|bar| bar.minimum_width).min()?,
        bars.iter().map(|bar| bar.preferred_width).max()?,
        bars.iter().map(|bar| bar.maximum_width).max()?,
    ))
}

struct ComponentApplication {
    host: PluginHost,
    item: String,
    renderer: Option<gles::Renderer>,
    ui: Option<RetainedUi>,
    interactions: InteractionState,
    interaction_map: InteractionMap,
    appearance: Appearance,
    width: u32,
    height: u32,
    max_frames: Option<u64>,
    started: Instant,
    next_presentation_session: u32,
    active_presentation_session: Option<u32>,
    presentation_requests: VecDeque<PresentationSessionRequest>,
}

impl ComponentApplication {
    fn viewport(&self) -> Option<Rect> {
        (self.width > 0 && self.height > 0)
            .then(|| Rect::new(0.0, 0.0, self.width as f32, self.height as f32))
    }

    fn rebuild(&mut self) -> Result<bool> {
        let Some(viewport) = self.viewport() else {
            return Ok(false);
        };
        self.ui = Some(self.host.render_at(
            &self.item,
            viewport,
            self.appearance,
            self.started.elapsed(),
        )?);
        Ok(true)
    }

    fn send_ui_events(
        &mut self,
        events: Vec<UiEvent>,
        activation: Option<InputActivation>,
        contact_id: u32,
    ) -> Result<bool> {
        let mut rerender = false;
        for event in events {
            let (widget_id, kind, value, event_contact_id) = match event {
                UiEvent::Pressed { id } => (id.0, InputKind::Pressed, None, contact_id),
                UiEvent::Activated { id } => (id.0, InputKind::Activated, None, contact_id),
                UiEvent::LongPressed { id, contact } => {
                    (id.0, InputKind::LongPressed, None, contact)
                }
                UiEvent::ValueChanged { id, value } => {
                    (id.0, InputKind::ValueChanged, Some(value), contact_id)
                }
                UiEvent::Released { id } => (id.0, InputKind::Released, None, contact_id),
                UiEvent::Cancelled { id } => (id.0, InputKind::Cancelled, None, contact_id),
            };
            let changed = self.host.handle_event(&InputEvent {
                item_id: self.item.clone(),
                widget_id,
                kind,
                value,
                contact_id: Some(event_contact_id),
                activation: (kind == InputKind::Activated)
                    .then_some(activation)
                    .flatten(),
            })?;
            println!(
                "component-input item={} widget={} kind={kind:?} rerender={}",
                self.item, widget_id, changed.rerender
            );
            rerender |= changed.rerender;
            self.apply_component_presentation(changed, event_contact_id)?;
        }
        if rerender {
            self.rebuild()?;
        }
        Ok(rerender)
    }

    fn apply_component_presentation(
        &mut self,
        update: ComponentUpdate,
        contact_id: u32,
    ) -> Result<()> {
        let Some(command) = update.presentation else {
            return Ok(());
        };
        match command {
            ComponentPresentationCommand::Begin {
                placement,
                lifecycle,
            } => {
                if self.active_presentation_session.is_some() {
                    bail!("component already has an active presentation");
                }
                let session_id = self.next_presentation_session.max(1);
                self.next_presentation_session = session_id.wrapping_add(1).max(1);
                let policy = match placement {
                    ComponentPresentationPlacement::Anchored => SessionPresentationPolicy::Anchored,
                    ComponentPresentationPlacement::InPlace => SessionPresentationPolicy::InPlace,
                    ComponentPresentationPlacement::Slot(target) => {
                        SessionPresentationPolicy::Slot(target)
                    }
                    ComponentPresentationPlacement::Region(target) => {
                        SessionPresentationPolicy::Region(target)
                    }
                    ComponentPresentationPlacement::FullBar => SessionPresentationPolicy::FullBar,
                };
                let lifecycle = match lifecycle {
                    ComponentPresentationLifecycle::Persistent => {
                        SessionPresentationLifecycle::Persistent
                    }
                    ComponentPresentationLifecycle::Transient => {
                        if contact_id == 0 {
                            bail!("transient component presentation requires a physical contact");
                        }
                        SessionPresentationLifecycle::Transient { contact_id }
                    }
                };
                self.active_presentation_session = Some(session_id);
                self.presentation_requests
                    .push_back(PresentationSessionRequest::Begin {
                        session_id,
                        policy,
                        lifecycle,
                    });
            }
            ComponentPresentationCommand::End(reason) => {
                let session_id = self
                    .active_presentation_session
                    .context("component has no active presentation to end")?;
                let reason = match reason {
                    ComponentPresentationDismissal::Requested => {
                        PresentationDismissReason::Requested
                    }
                    ComponentPresentationDismissal::Selection => {
                        PresentationDismissReason::Selection
                    }
                    ComponentPresentationDismissal::Timeout => PresentationDismissReason::Timeout,
                };
                self.presentation_requests
                    .push_back(PresentationSessionRequest::End { session_id, reason });
            }
        }
        Ok(())
    }
}

impl Application for ComponentApplication {
    fn appearance_changed(&mut self, snapshot: AppearanceSnapshot) -> Result<bool> {
        self.appearance = appearance_from_snapshot(snapshot);
        if let Some(renderer) = &self.renderer {
            renderer.set_motion_policy(self.appearance.motion);
        }
        let rebuilt = self.rebuild()?;
        println!(
            "component-appearance item={} generation={} scheme={:?} motion={:?} accent=#{:08x}",
            self.item,
            snapshot.generation,
            snapshot.scheme,
            snapshot.motion,
            snapshot.accent.packed()
        );
        Ok(rebuilt)
    }

    fn configured(&mut self, graphics: &Graphics, config: SurfaceConfig) -> Result<()> {
        if self.renderer.is_none() {
            self.renderer = Some(gles::Renderer::new(graphics.gl())?);
        }
        self.renderer
            .as_ref()
            .expect("renderer initialized above")
            .set_motion_policy(self.appearance.motion);
        self.width = config.width;
        self.height = config.height;
        self.rebuild()?;
        println!(
            "configured plugin=component item={} region={}x{} renderer={} transport=dmabuf runtime=component",
            self.item,
            config.width,
            config.height,
            graphics.renderer_name()
        );
        Ok(())
    }

    fn render(&mut self, graphics: &Graphics, frame: FrameInfo) -> Result<FrameFlow> {
        if self.ui.is_none() {
            self.rebuild()?;
        }
        let now = self.started.elapsed();
        let hold_events = self.interactions.tick(now);
        if !hold_events.is_empty() {
            self.send_ui_events(hold_events, None, 0)?;
        }
        let viewport = Rect::new(
            0.0,
            0.0,
            frame.surface.width as f32,
            frame.surface.height as f32,
        );
        let renderer = self
            .renderer
            .as_ref()
            .expect("renderer initialized during configure");
        let resolved = self
            .ui
            .as_mut()
            .expect("component UI built during configure")
            .resolve_with_measurer(viewport, self.appearance.theme, renderer);
        self.interaction_map = resolved.interactions;
        let animating = resolved
            .scene
            .has_active_motion(now, self.appearance.motion);
        renderer.draw_at(
            graphics.gl(),
            &resolved.scene,
            frame.surface.width,
            frame.surface.height,
            now,
        )?;
        if frame.number.is_multiple_of(120) {
            println!(
                "component-frame item={} number={} revision={} primitives={}",
                self.item,
                frame.number,
                resolved.inspector.revision,
                resolved.scene.primitives.len()
            );
        }
        if self
            .max_frames
            .is_some_and(|maximum| frame.number + 1 >= maximum)
        {
            println!(
                "component-animation-summary frames={} guest-renders={}",
                frame.number + 1,
                self.host.guest_render_calls()
            );
            Ok(FrameFlow::Exit)
        } else if animating || self.interactions.has_captures() {
            Ok(FrameFlow::Animate)
        } else {
            Ok(FrameFlow::Wait)
        }
    }

    fn visibility_changed(&mut self, visible: bool) -> Result<bool> {
        let Some(ui) = self.ui.as_mut() else {
            return Ok(false);
        };
        ui.set_visible(visible);
        println!("component-visibility item={} visible={visible}", self.item);
        Ok(ui.needs_render())
    }

    fn touch(&mut self, contact: TouchContact) -> Result<bool> {
        let phase = match contact.phase {
            ClientContactPhase::Down => ContactPhase::Down,
            ClientContactPhase::Motion => ContactPhase::Motion,
            ClientContactPhase::Up => ContactPhase::Up,
            ClientContactPhase::Cancel => ContactPhase::Cancel,
        };
        let events = self.interactions.handle(
            &self.interaction_map,
            Contact {
                id: contact.id,
                phase,
                position: Point::new(contact.x, contact.y),
                time: contact.time,
            },
        );
        self.send_ui_events(
            events,
            Some(InputActivation {
                origin: activation_origin(contact.origin),
                input_sequence: contact.input_sequence,
            }),
            contact.id,
        )
    }

    fn presentation_session_changed(&mut self, event: PresentationSessionEvent) -> Result<()> {
        println!("component-presentation item={} event={event:?}", self.item);
        let host_event = match event {
            PresentationSessionEvent::Anchor { session_id, anchor } => {
                if self.active_presentation_session != Some(session_id) {
                    return Ok(());
                }
                ComponentPresentationEvent::Anchor {
                    x: anchor.x,
                    width: anchor.width,
                }
            }
            PresentationSessionEvent::Started { session_id, .. } => {
                if self.active_presentation_session != Some(session_id) {
                    return Ok(());
                }
                ComponentPresentationEvent::Started
            }
            PresentationSessionEvent::Ended { session_id, reason } => {
                if self.active_presentation_session != Some(session_id) {
                    return Ok(());
                }
                self.active_presentation_session = None;
                ComponentPresentationEvent::Ended(match reason {
                    PresentationDismissReason::Requested => {
                        ComponentPresentationEndReason::Requested
                    }
                    PresentationDismissReason::Selection => {
                        ComponentPresentationEndReason::Selection
                    }
                    PresentationDismissReason::OutsidePress => {
                        ComponentPresentationEndReason::OutsidePress
                    }
                    PresentationDismissReason::Timeout => ComponentPresentationEndReason::Timeout,
                    PresentationDismissReason::SourceHidden => {
                        ComponentPresentationEndReason::SourceHidden
                    }
                    PresentationDismissReason::Replaced => ComponentPresentationEndReason::Replaced,
                    PresentationDismissReason::Rejected => ComponentPresentationEndReason::Rejected,
                })
            }
        };
        let update = self.host.handle_presentation_event(host_event)?;
        if update.rerender {
            self.rebuild()?;
        }
        Ok(())
    }

    fn take_presentation_session_request(&mut self) -> Option<PresentationSessionRequest> {
        self.presentation_requests.pop_front()
    }

    fn external_event_fd(&self) -> Option<std::os::fd::RawFd> {
        self.host.broker_event_fd()
    }

    fn external_event(&mut self) -> Result<bool> {
        let rerender = self.host.dispatch_broker_events()?;
        if rerender {
            self.rebuild()?;
        }
        Ok(rerender)
    }
}

fn activation_origin(origin: ContactOrigin) -> ActivationOrigin {
    match origin {
        ContactOrigin::Physical => ActivationOrigin::Physical,
        ContactOrigin::Synthetic => ActivationOrigin::Synthetic,
    }
}

fn appearance_from_snapshot(snapshot: AppearanceSnapshot) -> Appearance {
    Appearance {
        revision: u64::from(snapshot.generation),
        scheme: match snapshot.scheme {
            ClientColorScheme::Dark => ColorScheme::Dark,
            ClientColorScheme::Light => ColorScheme::Light,
        },
        motion: match snapshot.motion {
            ClientMotionPolicy::Full => MotionPolicy::Full,
            ClientMotionPolicy::Reduced => MotionPolicy::Reduced,
            ClientMotionPolicy::Disabled => MotionPolicy::Disabled,
        },
        theme: snapshot.into(),
    }
}

fn print_semantics(node: &SemanticNode, depth: usize) {
    println!(
        "{}{:?} {:?} [{:.0},{:.0} {:.0}x{:.0}]",
        "  ".repeat(depth),
        node.role,
        node.label,
        node.bounds.x,
        node.bounds.y,
        node.bounds.width,
        node.bounds.height,
    );
    for child in &node.children {
        print_semantics(child, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_contacts_remain_synthetic_at_the_broker_boundary() {
        assert_eq!(
            activation_origin(ContactOrigin::Synthetic),
            ActivationOrigin::Synthetic
        );
        assert_eq!(
            activation_origin(ContactOrigin::Physical),
            ActivationOrigin::Physical
        );
    }
}
