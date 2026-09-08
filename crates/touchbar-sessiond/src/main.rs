use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd, RawFd},
        unix::fs::MetadataExt,
        unix::net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use memmap2::{Mmap, MmapOptions};
use touchbar_layout::{
    BarSpec, Element, GroupElement, GroupLayout, GroupSpec, ItemId, ItemSpec, resolve,
};
use touchbar_model::{ContextValue, ProfileCompositionSnapshot};
use touchbar_package::{PresentationBarElement, PresentationGroupElement, PresentationGroupLayout};
use touchbar_profile_config::ProfileDocument;
use touchbar_protocol::{
    DEFAULT_REGION_WIDTH, DEFAULT_SOCKET_NAME, REFRESH_MILLIHZ, TOUCHBAR_HEIGHT,
    TOUCHBAR_PROTOCOL_VERSION,
    appearance::{AppearanceSnapshot, ColorRole, ColorScheme, MotionPolicy},
    hardware_ipc::{
        HardwareMessage, KeyPhase, SessionMessage, SystemKey, TouchEvent as OutputTouchEvent,
        TouchPhase as OutputTouchPhase, receive_hardware_message, receive_hardware_swapchain,
        send_session_message,
    },
    server::{
        touchbar_appearance_v1 as appearance_protocol, touchbar_manager_v1, touchbar_surface_v1,
    },
};
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource,
    backend::{ClientData, ClientId, DisconnectReason, ObjectId, protocol::ProtocolError},
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface},
};

mod appearance;
mod context;
mod frame_output;
mod gpu;
mod input;
mod live_profiles;
mod plugins;
mod power;
mod presentation;
mod preview_output;
mod profile_watch;
mod status_scene;
mod system_scene;
mod wake;

use appearance::AppearanceSource;
use context::{ContextEvent, ContextReplay, HyprlandContextSource};
use frame_output::FramePublisher;
use gpu::{CompletionFence, GpuCompositor, LayerGeometry};
use input::{Contact as GlobalContact, Phase as ContactPhase, Router as InputRouter};
use live_profiles::LiveProfiles;
use power::{PowerSource, PowerState};
use presentation::{
    Anchor, full_bar, in_place_bar, in_place_content_bar, resolve_popover, slot_bar,
    slot_content_bar,
};
use preview_output::{PreviewInput, PreviewOutput};
use profile_watch::ProfileWatcher;
use status_scene::StatusScene;
use system_scene::SystemScene;
use touchbar_system_bar::SystemLayer;
use wake::EventSignal;

const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const DRM_FORMAT_MOD_APPLE_TILED: u64 = 0x0c00_0000_0000_0001;
const DRM_FORMAT_MOD_APPLE_TILED_COMPRESSED: u64 = 0x0c00_0000_0000_0002;
const MAX_PLUGIN_SURFACES: usize = 64;
const PROFILE_RELOAD_COALESCE: Duration = Duration::from_millis(75);
const THEME_CHANGE_ACTIVITY_DURATION: Duration = Duration::from_secs(2);
const MAX_PENDING_CONFIGURES: usize = 64;
const FN_TAP_MAX_DURATION: Duration = Duration::from_millis(250);
const FN_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);
static TERMINATE_SESSION: AtomicBool = AtomicBool::new(false);
const MAX_PENDING_FRAME_CALLBACKS: usize = 8;
const CLIENT_COMMIT_RATE: u128 = 120;
const CLIENT_COMMIT_BURST: u128 = 8;
const CLIENT_COMMIT_REJECT_LIMIT: u32 = 256;
const CLIENT_COMMIT_REJECT_WINDOW: Duration = Duration::from_secs(1);
const GPU_RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const DEMO_POLL_INTERVAL: Duration = Duration::from_millis(4);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Item,
    Backdrop,
}

struct ClientTracker {
    disconnected: AtomicBool,
    commit_budget: Mutex<CommitBudget>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitAdmission {
    Accept,
    Drop,
    Disconnect,
}

struct CommitBudget {
    credit: u128,
    last_refill: Instant,
    rejected_since: Instant,
    rejected: u32,
}

impl Default for ClientTracker {
    fn default() -> Self {
        Self {
            disconnected: AtomicBool::new(false),
            commit_budget: Mutex::new(CommitBudget::new(Instant::now())),
        }
    }
}

impl ClientTracker {
    fn admit_commit(&self, now: Instant) -> CommitAdmission {
        self.commit_budget
            .lock()
            .map_or(CommitAdmission::Disconnect, |mut budget| budget.admit(now))
    }
}

impl CommitBudget {
    const TOKEN: u128 = 1_000_000_000;

    fn new(now: Instant) -> Self {
        Self {
            credit: CLIENT_COMMIT_BURST * Self::TOKEN,
            last_refill: now,
            rejected_since: now,
            rejected: 0,
        }
    }

    fn admit(&mut self, now: Instant) -> CommitAdmission {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.credit = self
            .credit
            .saturating_add(elapsed.as_nanos().saturating_mul(CLIENT_COMMIT_RATE))
            .min(CLIENT_COMMIT_BURST * Self::TOKEN);
        self.last_refill = now;

        if self.credit >= Self::TOKEN {
            self.credit -= Self::TOKEN;
            return CommitAdmission::Accept;
        }

        if now.saturating_duration_since(self.rejected_since) >= CLIENT_COMMIT_REJECT_WINDOW {
            self.rejected_since = now;
            self.rejected = 0;
        }
        self.rejected = self.rejected.saturating_add(1);
        if self.rejected >= CLIENT_COMMIT_REJECT_LIMIT {
            CommitAdmission::Disconnect
        } else {
            CommitAdmission::Drop
        }
    }
}

impl ClientData for ClientTracker {
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        self.disconnected.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
struct PoolData {
    map: Arc<Mmap>,
    size: usize,
}

#[derive(Clone)]
struct ShmBufferData {
    map: Arc<Mmap>,
    offset: usize,
    width: u32,
    height: u32,
    stride: usize,
    format: wl_shm::Format,
}

struct DmabufPlane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
    modifier: u64,
}

struct DmabufBufferData {
    width: u32,
    height: u32,
    format: u32,
    y_invert: bool,
    planes: Vec<DmabufPlane>,
}

enum BufferData {
    Shm(ShmBufferData),
    Dmabuf(DmabufBufferData),
}

#[derive(Default)]
struct DmabufParams {
    used: bool,
    planes: Vec<Option<DmabufPlane>>,
}

#[derive(Default)]
struct DmabufParamsData(Mutex<DmabufParams>);

#[derive(Clone)]
struct DmabufGlobalData {
    device: Vec<u8>,
    format_table: Arc<File>,
    table_size: u32,
}

struct TouchbarSurfaceData {
    surface: wl_surface::WlSurface,
    plugin_id: String,
}

struct SurfaceState {
    kind: SurfaceKind,
    plugin_id: String,
    item_id: String,
    layout_id: String,
    compact_spec: ItemSpec,
    expanded_spec: Option<ItemSpec>,
    role: touchbar_surface_v1::TouchbarSurfaceV1,
    layer_id: u64,
    compact_geometry: LayerGeometry,
    geometry: LayerGeometry,
    pending_configures: VecDeque<(u32, LayerGeometry)>,
    deferred_configure: Option<LayerGeometry>,
    configured: bool,
    visible: bool,
    pending_buffer: Option<wl_buffer::WlBuffer>,
    pending_acquire_fence: Option<OwnedFd>,
    pending_callbacks: Vec<wl_callback::WlCallback>,
    committed_frames: u64,
    last_presentation_session: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentationKind {
    Anchored,
    InPlace,
    Slot,
    Region,
    FullBar,
}

#[derive(Clone, Debug)]
struct ActivePresentation {
    surface: ObjectId,
    session_id: u32,
    persistent: bool,
    policy: PresentationKind,
    target: String,
    content_bar: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PluginPlaceholder {
    item: String,
    message: String,
}

impl PresentationKind {
    fn protocol(self) -> touchbar_surface_v1::PresentationPolicy {
        match self {
            Self::Anchored => touchbar_surface_v1::PresentationPolicy::Anchored,
            Self::InPlace => touchbar_surface_v1::PresentationPolicy::InPlace,
            Self::Slot => touchbar_surface_v1::PresentationPolicy::Slot,
            Self::Region => touchbar_surface_v1::PresentationPolicy::Region,
            Self::FullBar => touchbar_surface_v1::PresentationPolicy::FullBar,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Anchored => "anchored",
            Self::InPlace => "in-place",
            Self::Slot => "slot",
            Self::Region => "region",
            Self::FullBar => "full-bar",
        }
    }
}

fn valid_presentation_target(policy: PresentationKind, target: &str) -> bool {
    match policy {
        PresentationKind::Slot | PresentationKind::Region => {
            !target.is_empty()
                && target.len() <= 128
                && target
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                && target.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_' | b'.')
                })
        }
        PresentationKind::Anchored | PresentationKind::InPlace | PresentationKind::FullBar => {
            target.is_empty()
        }
    }
}

fn bar_item_ids(bar: &BarSpec) -> BTreeSet<ItemId> {
    let mut ids = BTreeSet::new();
    for element in &bar.elements {
        match element {
            Element::Item(item) => {
                ids.insert(item.id.clone());
            }
            Element::Group(group) => collect_group_item_ids(group, &mut ids),
            Element::FixedSpace { .. } | Element::FlexibleSpace { .. } => {}
        }
    }
    ids
}

fn collect_group_item_ids(group: &GroupSpec, ids: &mut BTreeSet<ItemId>) {
    for element in &group.elements {
        match element {
            GroupElement::Item(item) => {
                ids.insert(item.id.clone());
            }
            GroupElement::Group(group) => collect_group_item_ids(group, ids),
        }
    }
}

struct PendingRelease {
    buffer: wl_buffer::WlBuffer,
    fence: CompletionFence,
}

struct DirectOutput {
    stream: UnixStream,
    event_rx: Receiver<std::io::Result<HardwareMessage>>,
    event_signal: EventSignal,
    reader: Option<JoinHandle<()>>,
    available: VecDeque<usize>,
    in_flight: Vec<Option<u64>>,
    pending_touch: VecDeque<OutputTouchEvent>,
    pending_fn: Option<bool>,
    next_sequence: u64,
}

impl DirectOutput {
    fn new(stream: UnixStream, buffer_count: usize) -> Result<Self> {
        let mut reader_stream = stream
            .try_clone()
            .context("clone ADP output event socket")?;
        let (event_tx, event_rx) = mpsc::channel();
        let event_signal = EventSignal::new().context("create ADP reader event signal")?;
        let reader_signal = event_signal
            .try_clone()
            .context("clone ADP reader event signal")?;
        let reader = thread::Builder::new()
            .name("adp-buffer-release".into())
            .spawn(move || {
                loop {
                    let event = receive_hardware_message(&mut reader_stream);
                    let stop = event.is_err();
                    if event_tx.send(event).is_err() {
                        break;
                    }
                    reader_signal.notify();
                    if stop {
                        break;
                    }
                }
            })
            .context("start ADP buffer release reader")?;
        Ok(Self {
            stream,
            event_rx,
            event_signal,
            reader: Some(reader),
            available: (0..buffer_count).collect(),
            in_flight: vec![None; buffer_count],
            pending_touch: VecDeque::new(),
            pending_fn: None,
            next_sequence: 1,
        })
    }

    fn acquire(&mut self) -> Result<Option<usize>> {
        self.poll_releases()?;
        Ok(self.available.pop_front())
    }

    fn notification_fd(&self) -> RawFd {
        self.event_signal.as_raw_fd()
    }

    fn submit(&mut self, index: usize) -> Result<u64> {
        let slot = self
            .in_flight
            .get_mut(index)
            .context("submitted ADP buffer index is out of range")?;
        if slot.is_some() {
            bail!("ADP buffer {index} was submitted while still in flight");
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        send_session_message(
            &mut self.stream,
            SessionMessage::FrameReady {
                index: index as u16,
                sequence,
            },
        )
        .context("send ADP buffer-ready event")?;
        *slot = Some(sequence);
        Ok(sequence)
    }

    fn poll_releases(&mut self) -> Result<()> {
        self.event_signal
            .drain()
            .context("drain ADP reader event signal")?;
        loop {
            let event = match self.event_rx.try_recv() {
                Ok(event) => event.context("ADP presenter event stream failed")?,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => bail!("ADP presenter disconnected"),
            };
            let (index, sequence) = match event {
                HardwareMessage::BufferReleased { index, sequence } => (index, sequence),
                HardwareMessage::Touch(touch) => {
                    self.pending_touch.push_back(touch);
                    continue;
                }
                HardwareMessage::FnChanged { pressed } => {
                    self.pending_fn = Some(pressed);
                    continue;
                }
            };
            let index = usize::from(index);
            let slot = self
                .in_flight
                .get_mut(index)
                .context("released ADP buffer index is out of range")?;
            if *slot != Some(sequence) {
                bail!(
                    "stale ADP release for buffer {index}: sequence {sequence}, expected {:?}",
                    *slot
                );
            }
            *slot = None;
            self.available.push_back(index);
        }
    }

    fn drain_touch(&mut self) -> Result<Vec<OutputTouchEvent>> {
        self.poll_releases()?;
        Ok(self.pending_touch.drain(..).collect())
    }

    fn take_fn_changed(&mut self) -> Result<Option<bool>> {
        self.poll_releases()?;
        Ok(self.pending_fn.take())
    }

    fn emit_key(&mut self, key: SystemKey, phase: KeyPhase) -> Result<()> {
        send_session_message(&mut self.stream, SessionMessage::Key { key, phase })
            .context("send system key to hardware daemon")
    }
}

impl Drop for DirectOutput {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

enum AcceptedBuffer {
    Dmabuf {
        completion: CompletionFence,
        explicit: bool,
    },
    Shm,
}

#[derive(Default)]
struct FnLayerGesture {
    pressed_at: Option<Instant>,
    media_armed_until: Option<Instant>,
    media_active: bool,
}

impl FnLayerGesture {
    fn transition(&mut self, pressed: bool, now: Instant) -> Option<SystemLayer> {
        if pressed {
            self.media_active = self
                .media_armed_until
                .take()
                .is_some_and(|deadline| now <= deadline);
            self.pressed_at = Some(now);
            return Some(if self.media_active {
                SystemLayer::Media
            } else {
                SystemLayer::Function
            });
        }

        let was_quick_tap = self
            .pressed_at
            .take()
            .is_some_and(|started| now.saturating_duration_since(started) <= FN_TAP_MAX_DURATION);
        self.media_armed_until =
            (!self.media_active && was_quick_tap).then_some(now + FN_DOUBLE_TAP_WINDOW);
        self.media_active = false;
        None
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct State {
    started: Instant,
    clients: Vec<Arc<ClientTracker>>,
    surfaces: HashMap<ObjectId, SurfaceState>,
    appearances: HashMap<ObjectId, appearance_protocol::TouchbarAppearanceV1>,
    appearance_source: AppearanceSource,
    power_source: PowerSource,
    live_profiles: Option<LiveProfiles>,
    user_profile_document: Option<ProfileDocument>,
    packaged_profiles: plugins::PackagedProfileCatalog,
    profile_layouts: u64,
    context_changes: u64,
    theme_change_activity_until: Option<Instant>,
    next_layer_id: u64,
    next_configure_serial: u32,
    input: InputRouter<ObjectId>,
    next_input_sequence: u64,
    input_sequences: HashMap<u32, u64>,
    input_origins: HashMap<u32, touchbar_surface_v1::InputOrigin>,
    fn_pressed: bool,
    fn_gesture: FnLayerGesture,
    active_presentation: Option<ActivePresentation>,
    presentation_catalog: plugins::PresentationCatalog,
    backdrop_surface: Option<ObjectId>,
    input_events: u64,
    presentation_changes: u64,
    max_surfaces: usize,
    committed_frames: u64,
    presented_frames: u64,
    changed_frames: u64,
    invalid_frames: u64,
    rate_limited_commits: u64,
    rate_limited_disconnects: u64,
    dropped_frame_callbacks: u64,
    last_checksum: Option<u64>,
    first_frame_at: Option<Instant>,
    last_frame_at: Option<Instant>,
    scene_dirty: bool,
    gpu: GpuCompositor,
    frame_publisher: Option<FramePublisher>,
    preview_output: Option<PreviewOutput>,
    direct_output: Option<DirectOutput>,
    hardware_socket: Option<PathBuf>,
    hardware_yielded: bool,
    next_hardware_reconnect: Instant,
    hardware_reconnect_failures: u64,
    system_scene: Option<SystemScene>,
    system_scene_visible: bool,
    status_scene: StatusScene,
    plugin_placeholder: Option<PluginPlaceholder>,
    placeholder_contacts: BTreeSet<u32>,
    profile_path: Option<PathBuf>,
    pending_releases: Vec<PendingRelease>,
    peak_pending_releases: usize,
    completed_releases: u64,
    dmabuf_frames: u64,
    explicit_sync_frames: u64,
    implicit_sync_frames: u64,
    shm_frames: u64,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    fn new(
        mut gpu: GpuCompositor,
        frame_publisher: Option<FramePublisher>,
        preview_output: Option<PreviewOutput>,
        direct_output: Option<DirectOutput>,
        hardware_socket: Option<PathBuf>,
        live_profiles: Option<LiveProfiles>,
        user_profile_document: Option<ProfileDocument>,
        profile_path: Option<PathBuf>,
        system_bar: bool,
    ) -> Result<Self> {
        let appearance_source = AppearanceSource::discover();
        let power_source = PowerSource::discover();
        gpu.set_background_color(Self::gpu_color(appearance_source.snapshot().background));
        // The canvas follows the attached panel; a presenter that already
        // installed a swapchain has resized the scene by now.
        let canvas_width = gpu.canvas_width();
        let mut system_scene = system_bar.then(|| SystemScene::new(canvas_width, TOUCHBAR_HEIGHT));
        if let Some(scene) = &mut system_scene {
            let pixels = scene
                .render(appearance_source.snapshot(), canvas_width, TOUCHBAR_HEIGHT)
                .to_vec();
            gpu.update_rgba_layer(
                system_scene::LAYER_ID,
                LayerGeometry {
                    x: 0,
                    width: canvas_width,
                    height: TOUCHBAR_HEIGHT,
                    opacity: 1.0,
                    z_index: u32::MAX - 1,
                },
                &pixels,
            )?;
        }
        Ok(Self {
            started: Instant::now(),
            clients: Vec::new(),
            surfaces: HashMap::new(),
            appearances: HashMap::new(),
            appearance_source,
            power_source,
            live_profiles,
            user_profile_document,
            packaged_profiles: plugins::PackagedProfileCatalog::default(),
            profile_layouts: 0,
            context_changes: 0,
            theme_change_activity_until: None,
            next_layer_id: 1,
            next_configure_serial: 1,
            input: InputRouter::default(),
            next_input_sequence: 1,
            input_sequences: HashMap::new(),
            input_origins: HashMap::new(),
            fn_pressed: false,
            fn_gesture: FnLayerGesture::default(),
            active_presentation: None,
            presentation_catalog: plugins::PresentationCatalog::default(),
            backdrop_surface: None,
            input_events: 0,
            presentation_changes: 0,
            max_surfaces: 0,
            committed_frames: 0,
            presented_frames: 0,
            changed_frames: 0,
            invalid_frames: 0,
            rate_limited_commits: 0,
            rate_limited_disconnects: 0,
            dropped_frame_callbacks: 0,
            last_checksum: None,
            first_frame_at: None,
            last_frame_at: None,
            scene_dirty: system_bar,
            gpu,
            frame_publisher,
            preview_output,
            direct_output,
            hardware_socket,
            hardware_yielded: false,
            next_hardware_reconnect: Instant::now(),
            hardware_reconnect_failures: 0,
            system_scene,
            system_scene_visible: system_bar,
            status_scene: StatusScene::new(TOUCHBAR_HEIGHT),
            plugin_placeholder: None,
            placeholder_contacts: BTreeSet::new(),
            profile_path,
            pending_releases: Vec::new(),
            peak_pending_releases: 0,
            completed_releases: 0,
            dmabuf_frames: 0,
            explicit_sync_frames: 0,
            implicit_sync_frames: 0,
            shm_frames: 0,
        })
    }

    fn publish_appearance(
        resource: &appearance_protocol::TouchbarAppearanceV1,
        snapshot: AppearanceSnapshot,
    ) {
        let scheme = match snapshot.scheme {
            ColorScheme::Dark => appearance_protocol::Scheme::Dark,
            ColorScheme::Light => appearance_protocol::Scheme::Light,
        };
        let motion = match snapshot.motion {
            MotionPolicy::Full => appearance_protocol::MotionPolicy::Full,
            MotionPolicy::Reduced => appearance_protocol::MotionPolicy::Reduced,
            MotionPolicy::Disabled => appearance_protocol::MotionPolicy::Disabled,
        };
        resource.begin(
            snapshot.generation,
            scheme,
            motion,
            snapshot.corner_radius_millipixels,
        );
        for role in ColorRole::ALL {
            let protocol_role = match role {
                ColorRole::Background => appearance_protocol::ColorRole::Background,
                ColorRole::Surface => appearance_protocol::ColorRole::Surface,
                ColorRole::SurfaceHover => appearance_protocol::ColorRole::SurfaceHover,
                ColorRole::SurfacePressed => appearance_protocol::ColorRole::SurfacePressed,
                ColorRole::Foreground => appearance_protocol::ColorRole::Foreground,
                ColorRole::Muted => appearance_protocol::ColorRole::Muted,
                ColorRole::Accent => appearance_protocol::ColorRole::Accent,
                ColorRole::Destructive => appearance_protocol::ColorRole::Destructive,
            };
            resource.color(
                snapshot.generation,
                protocol_role,
                snapshot.color(role).packed(),
            );
        }
        resource.done(snapshot.generation);
    }

    fn set_presentation_catalog(&mut self, catalog: plugins::PresentationCatalog) {
        println!("presentation-catalog bars={}", catalog.bar_count());
        self.presentation_catalog = catalog;
        if self.active_presentation.is_some() {
            self.refresh_active_presentation();
        }
    }

    fn set_packaged_profile_catalog(
        &mut self,
        catalog: plugins::PackagedProfileCatalog,
    ) -> Result<()> {
        println!(
            "package-profile-catalog profiles={}",
            catalog.profile_count()
        );
        // The built-in profile demo is a self-contained diagnostic fixture.
        // Normal automatic mode always retains a discoverable profile path.
        if self.profile_path.is_none() {
            return Ok(());
        }
        let document = catalog.merge(self.user_profile_document.as_ref())?;
        self.replace_effective_profiles(document)?;
        self.packaged_profiles = catalog;
        Ok(())
    }

    fn poll_appearance(&mut self) -> Result<bool> {
        let Some(snapshot) = self.appearance_source.poll() else {
            return Ok(false);
        };
        self.appearances.retain(|_, resource| resource.is_alive());
        self.appearances
            .values()
            .for_each(|resource| Self::publish_appearance(resource, snapshot));
        self.gpu
            .set_background_color(Self::gpu_color(snapshot.background));
        self.refresh_system_scene()?;
        self.refresh_plugin_placeholder_scene()?;
        self.scene_dirty = true;
        self.theme_change_activity_until = Some(Instant::now() + THEME_CHANGE_ACTIVITY_DURATION);
        self.handle_context_event(ContextEvent {
            key: "activity.theme-change".into(),
            value: ContextValue::Boolean(true),
        })?;
        Ok(true)
    }

    fn poll_transient_context(&mut self) -> Result<()> {
        if self
            .theme_change_activity_until
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.theme_change_activity_until = None;
            self.handle_context_event(ContextEvent {
                key: "activity.theme-change".into(),
                value: ContextValue::Boolean(false),
            })?;
        }
        Ok(())
    }

    fn next_transient_context_delay(&self) -> Option<Duration> {
        self.theme_change_activity_until
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn poll_power(&mut self) -> bool {
        self.power_source.poll().is_some()
    }

    fn animation_frame_rate_hz(&self) -> u32 {
        self.appearance_source
            .cadence()
            .hz(self.power_source.state())
    }

    fn animation_frame_period(&self) -> Duration {
        self.appearance_source
            .cadence()
            .frame_period(self.power_source.state())
    }

    fn power_source_status(&self) -> touchbar_control::PowerSourceStatus {
        match self.power_source.state() {
            PowerState::External => touchbar_control::PowerSourceStatus::External,
            PowerState::Battery => touchbar_control::PowerSourceStatus::Battery,
            PowerState::Unknown => touchbar_control::PowerSourceStatus::Unknown,
        }
    }

    fn gpu_color(color: touchbar_protocol::appearance::Rgba8) -> [f32; 4] {
        let alpha = f32::from(color.alpha) / 255.0;
        [
            f32::from(color.red) / 255.0 * alpha,
            f32::from(color.green) / 255.0 * alpha,
            f32::from(color.blue) / 255.0 * alpha,
            alpha,
        ]
    }

    fn presentation_content_bar(
        &self,
        presentation: &ActivePresentation,
    ) -> Result<Option<BarSpec>> {
        let Some(expected_bar) = presentation.content_bar.as_deref() else {
            return Ok(None);
        };
        let source = self
            .surfaces
            .get(&presentation.surface)
            .context("presentation source is unavailable")?;
        let declared = self
            .presentation_catalog
            .bar_for_item(&source.plugin_id, &source.item_id, presentation.persistent)
            .filter(|bar| bar.id == expected_bar)
            .context("presentation bar is no longer enabled")?;

        let mut elements = Vec::with_capacity(declared.elements.len());
        let mut pending_spaces = Vec::new();
        let mut included_items = BTreeSet::new();
        for element in &declared.elements {
            match element {
                PresentationBarElement::Item {
                    item,
                    minimum_width,
                    preferred_width,
                    maximum_width,
                } => {
                    if !self
                        .presentation_catalog
                        .item_enabled(&source.plugin_id, item)
                    {
                        continue;
                    }
                    let surface = self
                        .surfaces
                        .values()
                        .find(|candidate| {
                            candidate.kind == SurfaceKind::Item
                                && candidate.plugin_id == source.plugin_id
                                && candidate.item_id == *item
                        })
                        .with_context(|| {
                            format!(
                                "presentation item {}:{} is enabled but not connected",
                                source.plugin_id, item
                            )
                        })?;
                    if !included_items.is_empty() {
                        elements.append(&mut pending_spaces);
                    } else {
                        pending_spaces.clear();
                    }
                    included_items.insert(surface.layout_id.clone());
                    elements.push(Element::Item(ItemSpec::new(
                        surface.layout_id.as_str(),
                        *minimum_width,
                        *preferred_width,
                        *maximum_width,
                    )));
                }
                PresentationBarElement::FixedSpace { width } => {
                    pending_spaces.push(Element::FixedSpace { width: *width });
                }
                PresentationBarElement::FlexibleSpace { minimum, weight } => {
                    pending_spaces.push(Element::FlexibleSpace {
                        minimum: *minimum,
                        weight: *weight,
                    });
                }
                PresentationBarElement::Group {
                    id,
                    layout,
                    spacing,
                    visibility_priority,
                    compression_priority,
                    elements: group_elements,
                } => {
                    let had_items = !included_items.is_empty();
                    let Some(group) = self.presentation_group_spec(
                        source,
                        id,
                        *layout,
                        *spacing,
                        *visibility_priority,
                        *compression_priority,
                        group_elements,
                        &mut included_items,
                    )?
                    else {
                        continue;
                    };
                    if had_items {
                        elements.append(&mut pending_spaces);
                    } else {
                        pending_spaces.clear();
                    }
                    elements.push(Element::Group(group));
                }
            }
        }
        if included_items.is_empty() {
            bail!("presentation bar has no enabled items");
        }
        let principal_item = declared.principal_item.as_ref().and_then(|principal| {
            self.surfaces
                .values()
                .find(|candidate| {
                    candidate.kind == SurfaceKind::Item
                        && candidate.plugin_id == source.plugin_id
                        && candidate.item_id == *principal
                        && included_items.contains(&candidate.layout_id)
                })
                .map(|surface| surface.compact_spec.id.clone())
        });
        Ok(Some(BarSpec {
            elements,
            principal_item,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn presentation_group_spec(
        &self,
        source: &SurfaceState,
        id: &str,
        layout: PresentationGroupLayout,
        spacing: u32,
        visibility_priority: i32,
        compression_priority: i32,
        declared_elements: &[PresentationGroupElement],
        included_items: &mut BTreeSet<String>,
    ) -> Result<Option<GroupSpec>> {
        let mut elements = Vec::with_capacity(declared_elements.len());
        for element in declared_elements {
            match element {
                PresentationGroupElement::Item {
                    item,
                    minimum_width,
                    preferred_width,
                    maximum_width,
                } => {
                    if !self
                        .presentation_catalog
                        .item_enabled(&source.plugin_id, item)
                    {
                        continue;
                    }
                    let surface = self
                        .surfaces
                        .values()
                        .find(|candidate| {
                            candidate.kind == SurfaceKind::Item
                                && candidate.plugin_id == source.plugin_id
                                && candidate.item_id == *item
                        })
                        .with_context(|| {
                            format!(
                                "presentation item {}:{} is enabled but not connected",
                                source.plugin_id, item
                            )
                        })?;
                    included_items.insert(surface.layout_id.clone());
                    elements.push(GroupElement::Item(ItemSpec::new(
                        surface.layout_id.as_str(),
                        *minimum_width,
                        *preferred_width,
                        *maximum_width,
                    )));
                }
                PresentationGroupElement::Group {
                    id,
                    layout,
                    spacing,
                    visibility_priority,
                    compression_priority,
                    elements: child_elements,
                } => {
                    if let Some(group) = self.presentation_group_spec(
                        source,
                        id,
                        *layout,
                        *spacing,
                        *visibility_priority,
                        *compression_priority,
                        child_elements,
                        included_items,
                    )? {
                        elements.push(GroupElement::Group(group));
                    }
                }
            }
        }
        if elements.is_empty() {
            return Ok(None);
        }
        let layout = match layout {
            PresentationGroupLayout::Natural => GroupLayout::Natural,
            PresentationGroupLayout::EqualWidth => GroupLayout::EqualWidth,
        };
        Ok(Some(
            GroupSpec::new(format!("{}:{id}", source.plugin_id), elements)
                .layout(layout)
                .spacing(spacing)
                .visibility_priority(visibility_priority)
                .compression_priority(compression_priority),
        ))
    }

    fn presentation_content_sizing(
        &self,
        presentation: &ActivePresentation,
        fallback: ItemSpec,
    ) -> Result<ItemSpec> {
        let Some(expected_bar) = presentation.content_bar.as_deref() else {
            return Ok(fallback);
        };
        let source = self
            .surfaces
            .get(&presentation.surface)
            .context("presentation source is unavailable")?;
        let bar = self
            .presentation_catalog
            .bar_for_item(&source.plugin_id, &source.item_id, presentation.persistent)
            .filter(|bar| bar.id == expected_bar)
            .context("presentation bar is no longer enabled")?;
        Ok(ItemSpec::new(
            source.layout_id.as_str(),
            bar.minimum_width,
            bar.preferred_width,
            bar.maximum_width,
        ))
    }

    fn active_presentation_items(&self) -> Vec<ObjectId> {
        let Some(presentation) = &self.active_presentation else {
            return Vec::new();
        };
        let mut ids = vec![presentation.surface.clone()];
        let Ok(Some(bar)) = self.presentation_content_bar(presentation) else {
            return ids;
        };
        let layout_ids = bar_item_ids(&bar);
        for (id, _) in self
            .surfaces
            .iter()
            .filter(|(_, surface)| layout_ids.contains(&surface.compact_spec.id))
        {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    fn input_targets(&self) -> Vec<input::Target<ObjectId>> {
        let presentation_items = self.active_presentation_items();
        self.surfaces
            .iter()
            .filter(|(id, surface)| {
                surface.kind == SurfaceKind::Item
                    && self
                        .active_presentation
                        .as_ref()
                        .is_none_or(|_| presentation_items.contains(*id))
            })
            .map(|(id, surface)| input::Target {
                key: id.clone(),
                x: f64::from(surface.geometry.x),
                y: 0.0,
                width: f64::from(surface.geometry.width),
                height: f64::from(surface.geometry.height),
                layer: (u64::from(surface.geometry.z_index) << 32) | surface.layer_id,
                visible: surface.visible,
            })
            .collect()
    }

    fn dismisses_presentation_on_selection(&self, target: &ObjectId) -> bool {
        let Some(presentation) = self.active_presentation.as_ref() else {
            return false;
        };
        if !presentation.persistent {
            return true;
        }
        if presentation.surface == *target {
            return false;
        }
        let Some(source) = self.surfaces.get(&presentation.surface) else {
            return false;
        };
        self.presentation_catalog
            .bar_for_item(&source.plugin_id, &source.item_id, true)
            .is_some_and(|bar| bar.dismiss_on_selection)
    }

    fn dispatch_touch(
        &mut self,
        contact: GlobalContact,
        origin: touchbar_surface_v1::InputOrigin,
    ) -> bool {
        let targets = self.input_targets();
        let transient_presentation = self
            .active_presentation
            .as_ref()
            .is_some_and(|presentation| !presentation.persistent);
        if transient_presentation
            && matches!(contact.phase, ContactPhase::Motion | ContactPhase::Up)
            && let Some(current) = self.input.captured_target(contact.id).cloned()
            && let Some(target) = self.input.hit_target(contact.x, contact.y, &targets)
            && target != current
            && let Some(sequence) = self.input_sequences.get(&contact.id).copied()
            && let Some(origin) = self.input_origins.get(&contact.id).copied()
        {
            self.emit_surface_touch(
                &current,
                ContactPhase::Cancel,
                contact.time_ms,
                contact.id,
                0.0,
                0.0,
                sequence,
                origin,
            );
            self.input.retarget(contact.id, target.clone());
            if let Some(geometry) = self.surfaces.get(&target).map(|surface| surface.geometry) {
                self.emit_surface_touch(
                    &target,
                    ContactPhase::Down,
                    contact.time_ms,
                    contact.id,
                    contact.x - f64::from(geometry.x),
                    contact.y,
                    sequence,
                    origin,
                );
            }
        }
        let Some(routed) = self.input.route(contact, &targets) else {
            if contact.phase == ContactPhase::Down
                && self
                    .active_presentation
                    .as_ref()
                    .is_some_and(|presentation| presentation.persistent)
            {
                self.restore_compact_with_reason(touchbar_surface_v1::DismissReason::OutsidePress);
                return true;
            }
            return false;
        };
        let input_sequence = match routed.phase {
            ContactPhase::Down => {
                let sequence = self.next_input_sequence;
                let Some(next) = sequence.checked_add(1) else {
                    eprintln!("touch input sequence exhausted");
                    return false;
                };
                self.next_input_sequence = next;
                self.input_sequences.insert(routed.id, sequence);
                self.input_origins.insert(routed.id, origin);
                sequence
            }
            ContactPhase::Motion => {
                if self.input_origins.get(&routed.id) != Some(&origin) {
                    return false;
                }
                let Some(sequence) = self.input_sequences.get(&routed.id).copied() else {
                    return false;
                };
                sequence
            }
            ContactPhase::Up | ContactPhase::Cancel => {
                if self.input_origins.remove(&routed.id) != Some(origin) {
                    return false;
                }
                let Some(sequence) = self.input_sequences.remove(&routed.id) else {
                    return false;
                };
                sequence
            }
        };
        let dismiss_on_selection = routed.phase == ContactPhase::Up
            && self.dismisses_presentation_on_selection(&routed.target);
        self.emit_surface_touch(
            &routed.target,
            routed.phase,
            routed.time_ms,
            routed.id,
            routed.local_x,
            routed.local_y,
            input_sequence,
            origin,
        );
        let committed = match routed.phase {
            ContactPhase::Down => {
                if let Some(profiles) = &mut self.live_profiles {
                    profiles.capture_started(routed.id);
                }
                None
            }
            ContactPhase::Up | ContactPhase::Cancel => match self.live_profiles.as_mut() {
                Some(profiles) => match profiles.capture_ended(routed.id) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        eprintln!("commit deferred profile layout failed: {error:#}");
                        None
                    }
                },
                None => None,
            },
            ContactPhase::Motion => None,
        };
        self.input_events += 1;
        if let Some(snapshot) = committed
            && let Err(error) = self.apply_profile_snapshot(&snapshot)
        {
            eprintln!("deferred profile layout failed: {error:#}");
        }
        if dismiss_on_selection {
            self.restore_compact_with_reason(touchbar_surface_v1::DismissReason::Selection);
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_surface_touch(
        &self,
        target: &ObjectId,
        phase: ContactPhase,
        time_ms: u32,
        contact_id: u32,
        local_x: f64,
        local_y: f64,
        input_sequence: u64,
        origin: touchbar_surface_v1::InputOrigin,
    ) {
        let Some(surface) = self.surfaces.get(target) else {
            return;
        };
        let (sequence_hi, sequence_lo) = touchbar_protocol::split_input_sequence(input_sequence)
            .expect("touchbar-sessiond only allocates nonzero input sequences");
        match phase {
            ContactPhase::Down => surface.role.touch_down(
                time_ms,
                contact_id,
                local_x,
                local_y,
                sequence_hi,
                sequence_lo,
                origin,
            ),
            ContactPhase::Motion => surface.role.touch_motion(
                time_ms,
                contact_id,
                local_x,
                local_y,
                sequence_hi,
                sequence_lo,
                origin,
            ),
            ContactPhase::Up => surface.role.touch_up(
                time_ms,
                contact_id,
                local_x,
                local_y,
                sequence_hi,
                sequence_lo,
                origin,
            ),
            ContactPhase::Cancel => {
                surface
                    .role
                    .touch_cancel(time_ms, contact_id, sequence_hi, sequence_lo, origin)
            }
        }
        println!(
            "touch plugin={} item={} phase={phase:?} contact={contact_id} local={local_x:.1},{local_y:.1}",
            surface.plugin_id, surface.item_id
        );
    }

    fn poll_direct_input(&mut self) {
        let (events, fn_changed) = match self.direct_output.as_mut() {
            Some(output) => match (output.drain_touch(), output.take_fn_changed()) {
                (Ok(events), Ok(fn_changed)) => (events, fn_changed),
                (Err(error), _) | (_, Err(error)) => {
                    self.disconnect_hardware(&error);
                    return;
                }
            },
            None => return,
        };
        if let Some(pressed) = fn_changed {
            self.handle_fn_changed(pressed, "hardware");
        }
        for event in events {
            self.handle_output_touch(event, touchbar_surface_v1::InputOrigin::Physical);
        }
    }

    fn poll_preview_input(&mut self) -> Result<()> {
        let inputs = match self.preview_output.as_mut() {
            Some(output) => {
                output.pump()?;
                output.drain_input()
            }
            None => return Ok(()),
        };
        for input in inputs {
            match input {
                PreviewInput::Touch(event) => {
                    self.handle_output_touch(event, touchbar_surface_v1::InputOrigin::Synthetic);
                }
                PreviewInput::FnChanged(pressed) => self.handle_fn_changed(pressed, "preview"),
            }
        }
        Ok(())
    }

    fn preview_closed(&self) -> bool {
        self.preview_output
            .as_ref()
            .is_some_and(PreviewOutput::closed)
    }

    fn handle_fn_changed(&mut self, pressed: bool, source: &str) {
        if self.fn_pressed == pressed {
            return;
        }
        let layer = self.fn_gesture.transition(pressed, Instant::now());
        if pressed {
            self.cancel_plugin_contacts();
        }
        self.fn_pressed = pressed;
        println!(
            "{source}-fn={} layer={}",
            if pressed { "pressed" } else { "released" },
            layer.map_or("profile", |layer| match layer {
                SystemLayer::Media => "media",
                SystemLayer::Function => "function",
            })
        );
        if let Some(scene) = &mut self.system_scene {
            let (_, transitions) = scene.set_fn_override(layer);
            self.send_system_transitions(transitions);
            if let Err(error) = self.sync_system_scene_visibility() {
                eprintln!("change system Fn visibility failed: {error:#}");
            } else if self.system_scene_visible
                && let Err(error) = self.refresh_system_scene()
            {
                eprintln!("render system Fn layer failed: {error:#}");
            }
        }
        if let Err(error) = self.refresh_plugin_placeholder_scene() {
            eprintln!("render plugin placeholder after Fn change failed: {error:#}");
        }
    }

    fn handle_output_touch(
        &mut self,
        event: OutputTouchEvent,
        origin: touchbar_surface_v1::InputOrigin,
    ) {
        let placeholder_left = self.gpu.canvas_width().saturating_sub(status_scene::WIDTH);
        let placeholder_captured = self.placeholder_contacts.contains(&event.contact_id);
        let placeholder_hit = self.plugin_placeholder.is_some()
            && !self.fn_pressed
            && event.x_millipixels >= placeholder_left.saturating_mul(1_000) as i32;
        if placeholder_captured || placeholder_hit {
            match event.phase {
                OutputTouchPhase::Down => {
                    self.placeholder_contacts.insert(event.contact_id);
                }
                OutputTouchPhase::Up | OutputTouchPhase::Cancel => {
                    self.placeholder_contacts.remove(&event.contact_id);
                }
                OutputTouchPhase::Motion => {}
            }
            return;
        }
        if self.system_scene_visible
            && let Some(scene) = &mut self.system_scene
        {
            let transitions = scene.handle_touch(event);
            if !transitions.is_empty() {
                if origin == touchbar_surface_v1::InputOrigin::Physical {
                    self.send_system_transitions(transitions);
                }
                if let Err(error) = self.refresh_system_scene() {
                    eprintln!("render system key state failed: {error:#}");
                }
            }
            return;
        }
        let phase = match event.phase {
            OutputTouchPhase::Down => ContactPhase::Down,
            OutputTouchPhase::Motion => ContactPhase::Motion,
            OutputTouchPhase::Up => ContactPhase::Up,
            OutputTouchPhase::Cancel => ContactPhase::Cancel,
        };
        self.dispatch_touch(
            GlobalContact {
                id: event.contact_id,
                phase,
                x: f64::from(event.x_millipixels) / 1000.0,
                y: f64::from(event.y_millipixels) / 1000.0,
                time_ms: event.time_ms,
            },
            origin,
        );
    }

    fn disconnect_hardware(&mut self, error: &anyhow::Error) {
        eprintln!("hardware-output=disconnected error={error:#}");
        self.cancel_plugin_contacts();
        if let Some(scene) = &mut self.system_scene {
            scene.cancel_all();
            scene.set_fn_pressed(false);
        }
        self.fn_gesture.reset();
        self.fn_pressed = false;
        self.direct_output = None;
        self.gpu.remove_output_swapchain();
        self.next_hardware_reconnect = Instant::now() + Duration::from_secs(1);
        self.scene_dirty = true;
    }

    fn yield_hardware(&mut self) -> Result<()> {
        if self.hardware_socket.is_none() {
            bail!("this session has no hardware connection to yield");
        }
        self.cancel_plugin_contacts();
        self.release_system_keys();
        if let Some(scene) = &mut self.system_scene {
            scene.set_fn_pressed(false);
        }
        self.fn_gesture.reset();
        self.fn_pressed = false;
        self.placeholder_contacts.clear();
        self.direct_output = None;
        self.gpu.remove_output_swapchain();
        self.hardware_yielded = true;
        self.hardware_reconnect_failures = 0;
        self.scene_dirty = true;
        println!("hardware-output=yielded");
        Ok(())
    }

    fn resume_hardware(&mut self) {
        if !self.hardware_yielded {
            return;
        }
        self.hardware_yielded = false;
        self.next_hardware_reconnect = Instant::now();
        self.hardware_reconnect_failures = 0;
        println!("hardware-output=yield-released");
    }

    fn poll_hardware_reconnect(&mut self) {
        if self.hardware_yielded
            || self.direct_output.is_some()
            || Instant::now() < self.next_hardware_reconnect
        {
            return;
        }
        let Some(path) = self.hardware_socket.clone() else {
            return;
        };
        match connect_hardware_output(&mut self.gpu, &path) {
            Ok(output) => {
                self.direct_output = Some(output);
                self.hardware_reconnect_failures = 0;
                self.scene_dirty = true;
                println!("hardware-output=reconnected socket={}", path.display());
            }
            Err(error) => {
                self.hardware_reconnect_failures += 1;
                if self.hardware_reconnect_failures == 1
                    || self.hardware_reconnect_failures.is_multiple_of(10)
                {
                    eprintln!(
                        "hardware-output=reconnect-pending attempts={} error={error:#}",
                        self.hardware_reconnect_failures
                    );
                }
                self.next_hardware_reconnect = Instant::now() + Duration::from_secs(1);
            }
        }
    }

    fn send_system_transitions(&mut self, transitions: Vec<touchbar_system_bar::KeyTransition>) {
        let mut failure = None;
        for transition in transitions {
            let Some(output) = &mut self.direct_output else {
                return;
            };
            if let Err(error) = output.emit_key(transition.key, transition.phase) {
                failure = Some(error);
                break;
            }
        }
        if let Some(error) = failure {
            self.disconnect_hardware(&error);
        }
    }

    fn refresh_system_scene(&mut self) -> Result<()> {
        if !self.system_scene_visible {
            return Ok(());
        }
        let canvas_width = self.gpu.canvas_width();
        let Some(scene) = &mut self.system_scene else {
            return Ok(());
        };
        let pixels = scene
            .render(
                self.appearance_source.snapshot(),
                canvas_width,
                TOUCHBAR_HEIGHT,
            )
            .to_vec();
        self.gpu.update_rgba_layer(
            system_scene::LAYER_ID,
            LayerGeometry {
                x: 0,
                width: canvas_width,
                height: TOUCHBAR_HEIGHT,
                opacity: 1.0,
                z_index: u32::MAX - 1,
            },
            &pixels,
        )?;
        self.scene_dirty = true;
        Ok(())
    }

    fn set_plugin_placeholder(&mut self, next: Option<PluginPlaceholder>) -> Result<()> {
        if self.plugin_placeholder == next {
            return Ok(());
        }
        self.plugin_placeholder = next;
        self.placeholder_contacts.clear();
        self.refresh_plugin_placeholder_scene()?;
        println!(
            "plugin-placeholder={}",
            self.plugin_placeholder
                .as_ref()
                .map_or("hidden", |_| "visible")
        );
        Ok(())
    }

    fn refresh_plugin_placeholder_scene(&mut self) -> Result<()> {
        let Some(placeholder) = self
            .plugin_placeholder
            .as_ref()
            .filter(|_| !self.fn_pressed)
        else {
            self.gpu.remove_layer(status_scene::LAYER_ID);
            self.scene_dirty = true;
            return Ok(());
        };
        let pixels = self
            .status_scene
            .render(
                &placeholder.item,
                &placeholder.message,
                self.appearance_source.snapshot(),
                TOUCHBAR_HEIGHT,
            )
            .to_vec();
        let status_left = self.gpu.canvas_width().saturating_sub(status_scene::WIDTH);
        self.gpu.update_rgba_layer(
            status_scene::LAYER_ID,
            LayerGeometry {
                x: status_left,
                width: status_scene::WIDTH,
                height: TOUCHBAR_HEIGHT,
                opacity: 1.0,
                z_index: u32::MAX,
            },
            &pixels,
        )?;
        self.scene_dirty = true;
        Ok(())
    }

    fn has_visible_user_content(&self) -> bool {
        self.surfaces
            .values()
            .any(|surface| surface.visible && surface.committed_frames > 0)
    }

    fn needs_frame_tick(&self) -> bool {
        self.scene_dirty
            || self
                .surfaces
                .values()
                .any(|surface| !surface.pending_callbacks.is_empty())
    }

    fn next_hardware_reconnect_delay(&self) -> Option<Duration> {
        (!self.hardware_yielded && self.direct_output.is_none() && self.hardware_socket.is_some())
            .then(|| {
                self.next_hardware_reconnect
                    .saturating_duration_since(Instant::now())
            })
    }

    fn sync_system_scene_visibility(&mut self) -> Result<()> {
        if self.system_scene.is_none() {
            return Ok(());
        }
        let visible = self.fn_pressed || !self.has_visible_user_content();
        if visible == self.system_scene_visible {
            return Ok(());
        }
        self.system_scene_visible = visible;
        if visible {
            self.refresh_system_scene()?;
            println!(
                "system-scene=visible reason={}",
                if self.fn_pressed {
                    "fn"
                } else {
                    "empty-profile"
                }
            );
        } else {
            self.release_system_keys();
            self.gpu.remove_layer(system_scene::LAYER_ID);
            self.scene_dirty = true;
            println!("system-scene=hidden reason=user-content");
        }
        Ok(())
    }

    fn cancel_plugin_contacts(&mut self) {
        let time_ms = self.started.elapsed().as_millis() as u32;
        let cancelled = self.input.cancel_all();
        let mut deferred = None;
        for (contact, target) in cancelled {
            let Some(sequence) = self.input_sequences.remove(&contact) else {
                continue;
            };
            let Some(origin) = self.input_origins.remove(&contact) else {
                continue;
            };
            if let Some(surface) = self.surfaces.get(&target) {
                let (high, low) = touchbar_protocol::split_input_sequence(sequence)
                    .expect("captured contacts always have a nonzero input sequence");
                surface
                    .role
                    .touch_cancel(time_ms, contact, high, low, origin);
            }
            if let Some(profiles) = &mut self.live_profiles {
                match profiles.capture_ended(contact) {
                    Ok(snapshot) => deferred = snapshot.or(deferred),
                    Err(error) => {
                        eprintln!("commit profile after Fn cancellation failed: {error:#}")
                    }
                }
            }
        }
        if let Some(snapshot) = deferred
            && let Err(error) = self.apply_profile_snapshot(&snapshot)
        {
            eprintln!("apply profile after Fn cancellation failed: {error:#}");
        }
    }

    fn release_system_keys(&mut self) {
        let transitions = self
            .system_scene
            .as_mut()
            .map(SystemScene::cancel_all)
            .unwrap_or_default();
        self.send_system_transitions(transitions);
    }

    fn remove_surface(&mut self, id: &ObjectId) {
        let was_presentation = self.active_presentation_items().contains(id);
        let was_backdrop = self.backdrop_surface.as_ref() == Some(id);
        let cancelled = self.input.cancel_target(id);
        let mut deferred = None;
        for contact in cancelled {
            self.input_sequences.remove(&contact);
            self.input_origins.remove(&contact);
            if let Some(profiles) = &mut self.live_profiles {
                match profiles.capture_ended(contact) {
                    Ok(snapshot) => deferred = snapshot.or(deferred),
                    Err(error) => {
                        eprintln!("commit profile after surface removal failed: {error:#}")
                    }
                }
            }
        }
        if let Some(mut surface) = self.surfaces.remove(id) {
            if let Some(buffer) = surface.pending_buffer.take() {
                buffer.release();
            }
            self.gpu.remove_layer(surface.layer_id);
            self.scene_dirty = true;
        }
        if was_presentation {
            self.restore_compact_with_reason(touchbar_surface_v1::DismissReason::SourceHidden);
        }
        if was_backdrop {
            self.backdrop_surface = None;
        }
        if let Some(snapshot) = deferred
            && let Err(error) = self.apply_profile_snapshot(&snapshot)
        {
            eprintln!("profile layout after surface removal failed: {error:#}");
        }
        if let Err(error) = self.try_initialize_live_profiles() {
            eprintln!("profile reconciliation after surface removal failed: {error:#}");
        }
        if let Err(error) = self.relayout_compact() {
            eprintln!("compact relayout after surface removal failed: {error:#}");
        }
    }

    fn next_serial(&mut self) -> u32 {
        let serial = self.next_configure_serial;
        self.next_configure_serial = self.next_configure_serial.wrapping_add(1).max(1);
        serial
    }

    fn configure_surface(&mut self, id: &ObjectId, geometry: LayerGeometry) {
        let Some(surface) = self.surfaces.get(id) else {
            return;
        };
        if surface
            .deferred_configure
            .or_else(|| {
                surface
                    .pending_configures
                    .back()
                    .map(|(_, geometry)| *geometry)
            })
            .is_some_and(|pending| pending == geometry)
            || (surface.configured
                && surface.pending_configures.is_empty()
                && surface.deferred_configure.is_none()
                && surface.geometry == geometry)
        {
            return;
        }
        if surface.pending_configures.len() >= MAX_PENDING_CONFIGURES {
            if let Some(surface) = self.surfaces.get_mut(id) {
                surface.deferred_configure = Some(geometry);
            }
            return;
        }
        let serial = self.next_serial();
        let Some(surface) = self.surfaces.get_mut(id) else {
            return;
        };
        surface.pending_configures.push_back((serial, geometry));
        surface
            .role
            .configure(serial, geometry.width, geometry.height, REFRESH_MILLIHZ);
    }

    fn relayout_compact(&mut self) -> Result<()> {
        let bar = self.compact_bar();
        self.apply_compact_bar(&bar)
    }

    fn compact_bar(&self) -> BarSpec {
        let mut ordered = self
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.kind == SurfaceKind::Item)
            .map(|(id, surface)| (surface.layer_id, id.clone(), surface.compact_spec.clone()))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(layer, _, _)| *layer);
        match &self.live_profiles {
            Some(profiles) => profiles
                .current()
                .map(|snapshot| snapshot.composition.bar.clone())
                .unwrap_or_else(|| BarSpec {
                    elements: Vec::new(),
                    principal_item: None,
                }),
            None => BarSpec {
                elements: ordered
                    .iter()
                    .map(|(_, _, item)| Element::Item(item.clone()))
                    .collect(),
                principal_item: None,
            },
        }
    }

    fn apply_compact_bar(&mut self, bar: &BarSpec) -> Result<()> {
        self.apply_bar(bar, true)
    }

    fn apply_bar(&mut self, bar: &BarSpec, update_compact: bool) -> Result<()> {
        self.apply_bar_with_required(bar, update_compact, &BTreeSet::new())
    }

    fn apply_bar_with_required(
        &mut self,
        bar: &BarSpec,
        update_compact: bool,
        required_items: &BTreeSet<ItemId>,
    ) -> Result<()> {
        let layout = resolve(bar, self.gpu.canvas_width(), required_items)?;
        let placements = layout
            .placements
            .iter()
            .map(|placement| {
                (
                    placement.id.as_str().to_string(),
                    placement.x,
                    placement.width,
                )
            })
            .collect::<Vec<_>>();
        let presentation_source_hidden = update_compact
            && self
                .active_presentation
                .as_ref()
                .and_then(|presentation| self.surfaces.get(&presentation.surface))
                .is_some_and(|surface| {
                    !placements
                        .iter()
                        .any(|(item, _, _)| item == &surface.layout_id)
                });
        if presentation_source_hidden {
            self.restore_compact_with_reason(touchbar_surface_v1::DismissReason::SourceHidden);
        }
        let ids = self
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.kind == SurfaceKind::Item)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            let placement = self.surfaces.get(&id).and_then(|surface| {
                placements
                    .iter()
                    .find(|(item, _, _)| item == &surface.layout_id)
                    .map(|(_, x, width)| (*x, *width))
            });
            let Some((x, width)) = placement else {
                if let Some(surface) = self.surfaces.get_mut(&id) {
                    let changed = surface.visible;
                    surface.visible = false;
                    if changed {
                        surface.role.visibility(0);
                        println!(
                            "visibility plugin={} item={} visible=0",
                            surface.plugin_id, surface.item_id
                        );
                    }
                    self.gpu.remove_layer(surface.layer_id);
                    self.scene_dirty = true;
                }
                continue;
            };
            let geometry = LayerGeometry {
                x,
                width,
                height: TOUCHBAR_HEIGHT,
                opacity: 1.0,
                z_index: 100,
            };
            if let Some(surface) = self.surfaces.get_mut(&id) {
                let changed = !surface.visible;
                if update_compact {
                    surface.compact_geometry = geometry;
                }
                surface.visible = true;
                if changed {
                    surface.role.visibility(1);
                    println!(
                        "visibility plugin={} item={} visible=1",
                        surface.plugin_id, surface.item_id
                    );
                }
            }
            let is_active_presentation = self
                .active_presentation
                .as_ref()
                .is_some_and(|presentation| presentation.surface == id);
            if !update_compact || !is_active_presentation {
                self.configure_surface(&id, geometry);
            }
        }
        if update_compact && self.active_presentation.is_some() {
            self.refresh_active_presentation();
        }
        println!(
            "compact-layout placements={} hidden={}",
            layout.placements.len(),
            layout.hidden_items.len()
        );
        Ok(())
    }

    fn apply_overlay_bar(&mut self, bar: &BarSpec, x: u32, width: u32) -> Result<()> {
        let end = x
            .checked_add(width)
            .context("presentation region overflow")?;
        if end > self.gpu.canvas_width() {
            bail!("presentation region exceeds the Touch Bar");
        }
        let layout = resolve(bar, width, &bar_item_ids(bar))?;
        for placement in &layout.placements {
            let id = self
                .surfaces
                .iter()
                .find(|(_, surface)| surface.layout_id == placement.id.as_str())
                .map(|(id, _)| id.clone())
                .with_context(|| {
                    format!(
                        "presentation surface `{}` disconnected",
                        placement.id.as_str()
                    )
                })?;
            if let Some(surface) = self.surfaces.get_mut(&id) {
                let changed = !surface.visible;
                surface.visible = true;
                if changed {
                    surface.role.visibility(1);
                    println!(
                        "visibility plugin={} item={} visible=1",
                        surface.plugin_id, surface.item_id
                    );
                }
            }
            self.configure_surface(
                &id,
                LayerGeometry {
                    x: x + placement.x,
                    width: placement.width,
                    height: TOUCHBAR_HEIGHT,
                    opacity: 1.0,
                    z_index: 1000,
                },
            );
            println!(
                "presentation-layout item={} x={} width={} overlay=true",
                placement.id.as_str(),
                x + placement.x,
                placement.width
            );
        }
        Ok(())
    }

    fn refresh_active_presentation(&mut self) {
        let Some(presentation) = self.active_presentation.clone() else {
            return;
        };
        if let Err(error) = self.apply_active_presentation(&presentation) {
            eprintln!(
                "presentation={} refresh-failed error={error:#}",
                presentation.policy.label()
            );
            self.restore_compact_with_reason(touchbar_surface_v1::DismissReason::SourceHidden);
        }
    }

    fn apply_active_presentation(&mut self, presentation: &ActivePresentation) -> Result<()> {
        let (compact, expanded, layout_id, role) = self
            .surfaces
            .get(&presentation.surface)
            .and_then(|surface| {
                Some((
                    surface.compact_geometry,
                    surface.expanded_spec.clone()?,
                    surface.layout_id.clone(),
                    surface.role.clone(),
                ))
            })
            .context("presentation source or expanded sizing is unavailable")?;
        let content = self.presentation_content_bar(presentation)?;
        let container_sizing = self.presentation_content_sizing(presentation, expanded.clone())?;
        match presentation.policy {
            PresentationKind::Anchored => {
                let placement = resolve_popover(
                    Anchor {
                        x: compact.x,
                        width: compact.width,
                    },
                    container_sizing,
                    self.gpu.canvas_width(),
                )?;
                role.presentation_anchor(
                    presentation.session_id,
                    f64::from(compact.x - placement.x),
                    compact.width,
                );
                if let Some(content) = &content {
                    self.apply_overlay_bar(content, placement.x, placement.width)?;
                } else {
                    self.configure_surface(
                        &presentation.surface,
                        LayerGeometry {
                            x: placement.x,
                            width: placement.width,
                            height: TOUCHBAR_HEIGHT,
                            opacity: 1.0,
                            z_index: 1000,
                        },
                    );
                }
            }
            PresentationKind::InPlace => {
                if let Some(content) = content {
                    let required = bar_item_ids(&content);
                    let bar = in_place_content_bar(&self.compact_bar(), &layout_id, content)?;
                    self.apply_bar_with_required(&bar, false, &required)?;
                } else {
                    let bar = in_place_bar(&self.compact_bar(), &layout_id, expanded)?;
                    self.apply_bar(&bar, false)?;
                }
            }
            PresentationKind::Slot => {
                let snapshot = self
                    .live_profiles
                    .as_ref()
                    .and_then(LiveProfiles::current)
                    .context("slot presentation requires an active configured profile")?;
                if let Some(content) = content {
                    let required = bar_item_ids(&content);
                    let bar = slot_content_bar(
                        &snapshot.composition.bar,
                        &snapshot.composition.slots,
                        &presentation.target,
                        content,
                    )?;
                    self.apply_bar_with_required(&bar, false, &required)?;
                } else {
                    let bar = slot_bar(
                        &snapshot.composition.bar,
                        &snapshot.composition.slots,
                        &presentation.target,
                        &layout_id,
                        expanded,
                    )?;
                    self.apply_bar(&bar, false)?;
                }
            }
            PresentationKind::Region => {
                let canvas_width = self.gpu.canvas_width();
                let (x, width) = self
                    .live_profiles
                    .as_ref()
                    .and_then(|profiles| profiles.region(&presentation.target, canvas_width))
                    .with_context(|| {
                        format!("unknown presentation region `{}`", presentation.target)
                    })?;
                if let Some(content) = &content {
                    self.apply_overlay_bar(content, x, width)?;
                } else {
                    self.configure_surface(
                        &presentation.surface,
                        LayerGeometry {
                            x,
                            width,
                            height: TOUCHBAR_HEIGHT,
                            opacity: 1.0,
                            z_index: 1000,
                        },
                    );
                }
            }
            PresentationKind::FullBar => {
                if let Some(content) = content {
                    let required = bar_item_ids(&content);
                    self.apply_bar_with_required(&content, false, &required)?;
                } else {
                    self.apply_bar(&full_bar(&layout_id, self.gpu.canvas_width()), false)?;
                }
            }
        }
        println!(
            "presentation={} session={} target={} refreshed=true",
            presentation.policy.label(),
            presentation.session_id,
            if presentation.target.is_empty() {
                "-"
            } else {
                &presentation.target
            }
        );
        Ok(())
    }

    fn try_initialize_live_profiles(&mut self) -> Result<()> {
        let connected = self.connected_item_specs();
        let snapshot = self
            .live_profiles
            .as_mut()
            .map(|profiles| profiles.try_initialize(&connected))
            .transpose()?
            .flatten();
        if let Some(snapshot) = snapshot {
            println!("profile-runtime=ready");
            self.apply_profile_snapshot(&snapshot)?;
        } else if let Some(profiles) = &self.live_profiles
            && !profiles.ready()
        {
            println!(
                "profile-runtime=waiting missing={}",
                profiles.missing_required_items().join(",")
            );
        }
        Ok(())
    }

    fn connected_item_specs(&self) -> BTreeMap<String, ItemSpec> {
        self.surfaces
            .values()
            .filter(|surface| surface.kind == SurfaceKind::Item)
            .map(|surface| (surface.layout_id.clone(), surface.compact_spec.clone()))
            .collect()
    }

    fn profile_status(&self) -> touchbar_control::ProfileRuntimeStatus {
        match &self.live_profiles {
            Some(profiles) => touchbar_control::ProfileRuntimeStatus {
                configured: true,
                ready: profiles.ready(),
                automatic: profiles.manual_profile().is_none(),
                active: profiles.active_profile().map(str::to_owned),
                available: profiles.profile_ids(),
                missing_required_items: profiles.missing_required_items(),
            },
            None => touchbar_control::ProfileRuntimeStatus {
                configured: false,
                ready: true,
                automatic: true,
                active: None,
                available: Vec::new(),
                missing_required_items: Vec::new(),
            },
        }
    }

    fn reload_profiles(&mut self) -> Result<()> {
        let Some(path) = self.profile_path.clone() else {
            return Ok(());
        };
        if !path.exists() {
            let document = self.packaged_profiles.merge(None)?;
            self.replace_effective_profiles(document)?;
            self.user_profile_document = None;
            println!(
                "profile-source=automatic-all-items config={}",
                path.display()
            );
            return Ok(());
        }
        let document = touchbar_profile_config::load(&path)?;
        let effective = self.packaged_profiles.merge(Some(&document))?;
        self.replace_effective_profiles(effective)?;
        self.user_profile_document = Some(document);
        println!("profile-source={} reloaded=true", path.display());
        Ok(())
    }

    fn replace_effective_profiles(&mut self, document: Option<ProfileDocument>) -> Result<()> {
        let connected = self.connected_item_specs();
        let snapshot = match document {
            Some(document) => {
                if let Some(profiles) = &mut self.live_profiles {
                    profiles.replace_document(document, &connected)?
                } else {
                    let mut profiles = LiveProfiles::configured(document);
                    let snapshot = profiles.try_initialize(&connected)?;
                    self.live_profiles = Some(profiles);
                    snapshot
                }
            }
            None => {
                self.live_profiles = None;
                None
            }
        };
        if let Some(snapshot) = snapshot {
            self.apply_profile_snapshot(&snapshot)?;
        } else {
            self.relayout_compact()?;
        }
        Ok(())
    }

    fn select_profile(&mut self, profile: String) -> Result<&'static str> {
        let profiles = self
            .live_profiles
            .as_mut()
            .context("no user profile configuration is loaded")?;
        let snapshot = profiles.select_profile(profile)?;
        let ready = profiles.ready();
        if let Some(snapshot) = snapshot {
            self.apply_profile_snapshot(&snapshot)?;
            Ok("profile selected")
        } else if ready {
            Ok("profile selection deferred until the active gesture ends")
        } else {
            Ok("profile selected; waiting for required items")
        }
    }

    fn use_automatic_profile(&mut self) -> Result<&'static str> {
        let profiles = self
            .live_profiles
            .as_mut()
            .context("no user profile configuration is loaded")?;
        let snapshot = profiles.use_automatic_profile()?;
        let ready = profiles.ready();
        if let Some(snapshot) = snapshot {
            self.apply_profile_snapshot(&snapshot)?;
            Ok("automatic profile selection active")
        } else if ready {
            Ok("automatic profile selection active; change deferred until the active gesture ends")
        } else {
            Ok("automatic profile selection active; waiting for required items")
        }
    }

    fn handle_context_event(&mut self, event: ContextEvent) -> Result<()> {
        self.context_changes += 1;
        println!("context-event key={} value={:?}", event.key, event.value);
        let snapshot = self
            .live_profiles
            .as_mut()
            .map(|profiles| profiles.set_fact(event.key, event.value))
            .transpose()?
            .flatten();
        if let Some(snapshot) = snapshot {
            self.apply_profile_snapshot(&snapshot)?;
        } else if self.live_profiles.as_ref().is_some_and(LiveProfiles::ready) {
            println!("profile-layout=deferred captured-contact-active");
        }
        Ok(())
    }

    fn apply_profile_snapshot(&mut self, snapshot: &ProfileCompositionSnapshot) -> Result<()> {
        let items = bar_item_ids(&snapshot.composition.bar)
            .into_iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "profile-layout generation={} context_generation={} profile={} items={} retained={} entering={} leaving={}",
            snapshot.generation,
            snapshot.context_generation,
            snapshot.composition.profile,
            items,
            snapshot.delta.retained_items.len(),
            snapshot.delta.entering_items.len(),
            snapshot.delta.leaving_items.len()
        );
        self.profile_layouts += 1;
        self.apply_compact_bar(&snapshot.composition.bar)
    }

    fn profiles_ready(&self) -> bool {
        self.live_profiles.as_ref().is_some_and(LiveProfiles::ready)
    }

    fn present(
        &mut self,
        id: &ObjectId,
        session_id: u32,
        contact_id: u32,
        persistent: bool,
        policy: PresentationKind,
        target: String,
    ) {
        if !persistent && !self.input.is_captured_by(contact_id, id) {
            eprintln!("ignored presentation without matching captured contact {contact_id}");
            self.reject_presentation(id, session_id);
            return;
        }
        if (persistent && contact_id != 0) || (!persistent && contact_id == 0) {
            eprintln!("ignored presentation with inconsistent lifecycle");
            self.reject_presentation(id, session_id);
            return;
        }
        if self.active_presentation.is_some() {
            eprintln!("ignored presentation begin while another session is active");
            self.reject_presentation(id, session_id);
            return;
        }
        let Some(surface) = self
            .surfaces
            .get(id)
            .filter(|surface| surface.visible && surface.expanded_spec.is_some())
        else {
            eprintln!("ignored presentation for hidden item or item without expanded sizing");
            self.reject_presentation(id, session_id);
            return;
        };
        let role = surface.role.clone();
        let content_bar = self
            .presentation_catalog
            .bar_for_item(&surface.plugin_id, &surface.item_id, persistent)
            .map(|bar| bar.id.clone());
        let presentation = ActivePresentation {
            surface: id.clone(),
            session_id,
            persistent,
            policy,
            target: target.clone(),
            content_bar,
        };
        self.active_presentation = Some(presentation.clone());
        if let Err(error) = self.apply_active_presentation(&presentation) {
            self.active_presentation = None;
            eprintln!("ignored {} presentation: {error:#}", policy.label());
            if let Err(error) = self.relayout_compact() {
                eprintln!("restore after rejected presentation failed: {error:#}");
            }
            self.reject_presentation(id, session_id);
            return;
        }
        role.presentation_started(
            session_id,
            policy.protocol(),
            if persistent {
                touchbar_surface_v1::PresentationLifecycle::Persistent
            } else {
                touchbar_surface_v1::PresentationLifecycle::Transient
            },
            contact_id,
            target,
        );
        self.presentation_changes += 1;
        println!(
            "presentation={} mode={} contact={contact_id} session={session_id} content={}",
            policy.label(),
            if persistent {
                "persistent"
            } else {
                "transient"
            },
            self.active_presentation
                .as_ref()
                .and_then(|presentation| presentation.content_bar.as_deref())
                .unwrap_or("source")
        );
    }

    fn restore_compact_with_reason(&mut self, reason: touchbar_surface_v1::DismissReason) {
        let Some(presentation) = self.active_presentation.take() else {
            return;
        };
        if let Some(surface) = self.surfaces.get(&presentation.surface) {
            surface
                .role
                .presentation_ended(presentation.session_id, reason);
        }
        if let Err(error) = self.relayout_compact() {
            eprintln!("restore compact layout after presentation failed: {error:#}");
        }
        self.presentation_changes += 1;
        println!(
            "presentation=compact previous={} session={} reason={reason:?}",
            presentation.policy.label(),
            presentation.session_id
        );
    }

    fn reject_presentation(&self, id: &ObjectId, session_id: u32) {
        if session_id != 0
            && let Some(surface) = self.surfaces.get(id)
        {
            surface
                .role
                .presentation_ended(session_id, touchbar_surface_v1::DismissReason::Rejected);
        }
    }

    fn accept_surface_commit(&mut self, surface_id: &ObjectId) {
        let Some(surface) = self.surfaces.get_mut(surface_id) else {
            return;
        };
        let buffer = surface.pending_buffer.take();
        let acquire_fence = surface.pending_acquire_fence.take();
        let layer_id = surface.layer_id;
        let geometry = surface.geometry;
        let configured = surface.configured;
        let visible = surface.visible;
        let plugin_id = surface.plugin_id.clone();
        let role = surface.role.clone();
        let Some(buffer) = buffer else {
            if acquire_fence.is_some() {
                self.invalid_frames += 1;
                role.post_error(
                    touchbar_surface_v1::Error::AcquireFenceWithoutBuffer,
                    "acquire fence committed without a buffer",
                );
            }
            return;
        };

        if !visible {
            // A buffer committed before the visibility event may still be in
            // flight through the Wayland/EGL bridge. Retire it without
            // treating a normal presentation transition as a plugin error.
            buffer.release();
            return;
        }

        let result = if !configured {
            Err(anyhow::anyhow!(
                "surface committed before configure acknowledgement"
            ))
        } else if let Some(data) = buffer.data::<BufferData>() {
            match data {
                BufferData::Shm(data) => {
                    if acquire_fence.is_some() {
                        self.invalid_frames += 1;
                        role.post_error(
                            touchbar_surface_v1::Error::AcquireFenceUnsupportedBuffer,
                            "acquire fences require a DMA-BUF",
                        );
                        buffer.release();
                        return;
                    } else {
                        read_shm_rgba(data, geometry).and_then(|pixels| {
                            self.gpu
                                .update_rgba_layer(layer_id, geometry, &pixels)
                                .map(|_| AcceptedBuffer::Shm)
                        })
                    }
                }
                BufferData::Dmabuf(data) => {
                    let explicit = acquire_fence.is_some();
                    if let Some(fence) = acquire_fence
                        && let Err(error) = self.gpu.wait_on_acquire_fence(fence)
                    {
                        self.invalid_frames += 1;
                        role.post_error(
                            touchbar_surface_v1::Error::InvalidAcquireFence,
                            format!("invalid acquire fence: {error:#}"),
                        );
                        buffer.release();
                        return;
                    }
                    self.gpu
                        .update_dmabuf_layer(layer_id, geometry, data)
                        .map(|completion| AcceptedBuffer::Dmabuf {
                            completion,
                            explicit,
                        })
                }
            }
        } else {
            Err(anyhow::anyhow!("wl_buffer has no recognized backing data"))
        };

        match result {
            Ok(accepted) => {
                self.committed_frames += 1;
                match accepted {
                    AcceptedBuffer::Dmabuf {
                        completion: fence,
                        explicit,
                    } => {
                        self.dmabuf_frames += 1;
                        if explicit {
                            self.explicit_sync_frames += 1;
                        } else {
                            self.implicit_sync_frames += 1;
                        }
                        self.pending_releases.push(PendingRelease { buffer, fence });
                        self.peak_pending_releases =
                            self.peak_pending_releases.max(self.pending_releases.len());
                    }
                    AcceptedBuffer::Shm => {
                        self.shm_frames += 1;
                        buffer.release();
                    }
                }
                if let Some(surface) = self.surfaces.get_mut(surface_id) {
                    surface.committed_frames += 1;
                }
                self.scene_dirty = true;
            }
            Err(error) => {
                self.invalid_frames += 1;
                eprintln!("rejected plugin={plugin_id} buffer: {error:#}");
                buffer.release();
            }
        }
    }

    fn drop_surface_commit(&mut self, surface_id: &ObjectId) {
        if let Some(surface) = self.surfaces.get_mut(surface_id) {
            surface.pending_acquire_fence = None;
            if let Some(buffer) = surface.pending_buffer.take() {
                buffer.release();
            }
        }
    }

    fn poll_buffer_releases(&mut self) {
        let mut index = 0;
        while index < self.pending_releases.len() {
            match self
                .gpu
                .completion_signaled(&self.pending_releases[index].fence)
            {
                Ok(true) => {
                    let release = self.pending_releases.swap_remove(index);
                    self.gpu.destroy_completion(release.fence);
                    release.buffer.release();
                    self.completed_releases += 1;
                }
                Ok(false) => index += 1,
                Err(error) => {
                    eprintln!("discarding failed GPU completion fence: {error:#}");
                    let release = self.pending_releases.swap_remove(index);
                    self.gpu.destroy_completion(release.fence);
                    release.buffer.release();
                    self.completed_releases += 1;
                }
            }
        }
    }

    fn finish_buffer_releases(&mut self) {
        self.gpu.finish();
        for release in self.pending_releases.drain(..) {
            self.gpu.destroy_completion(release.fence);
            release.buffer.release();
            self.completed_releases += 1;
        }
    }

    fn present_scene(&mut self) -> Result<()> {
        if !self.scene_dirty {
            return Ok(());
        }

        let had_direct_output = self.direct_output.is_some();
        let output_index = if let Some(output) = &mut self.direct_output {
            match output.acquire() {
                Ok(index) => index,
                Err(error) => {
                    self.disconnect_hardware(&error);
                    None
                }
            }
        } else {
            None
        };
        if had_direct_output && self.direct_output.is_some() && output_index.is_none() {
            // All buffers are owned by the presenter. Keep only the newest
            // retained plugin layers and retry when a release arrives.
            return Ok(());
        }

        self.gpu.compose_scene_gpu();
        let needs_readback = self.direct_output.is_none()
            || self.frame_publisher.is_some()
            || self.preview_output.is_some();
        let checksum = needs_readback.then(|| self.gpu.read_scene_checksum());
        if let Some(publisher) = &mut self.frame_publisher {
            publisher.publish(self.gpu.scene_pixels());
        }
        if let Some(preview) = &mut self.preview_output {
            preview.present(self.gpu.scene_pixels())?;
        }
        if let Some(index) = output_index {
            self.gpu.render_scene_to_output(index)?;
            self.direct_output
                .as_mut()
                .context("ADP output disappeared during rendering")?
                .submit(index)?;
        }
        let now = Instant::now();
        self.first_frame_at.get_or_insert(now);
        self.last_frame_at = Some(now);
        self.presented_frames += 1;
        if checksum.is_none() || self.last_checksum != checksum {
            self.changed_frames += 1;
        }
        self.last_checksum = checksum;
        self.scene_dirty = false;

        if self.presented_frames == 1 || self.presented_frames.is_multiple_of(60) {
            if let Some(checksum) = checksum {
                println!(
                    "scene={} commits={} changed={} checksum={checksum:016x}",
                    self.presented_frames, self.committed_frames, self.changed_frames
                );
            } else {
                println!(
                    "scene={} commits={} changed={} output=adp-direct",
                    self.presented_frames, self.committed_frames, self.changed_frames
                );
            }
        }
        Ok(())
    }

    fn send_frame_callbacks(&mut self) {
        let callback_time = self.started.elapsed().as_millis() as u32;
        for surface in self.surfaces.values_mut().filter(|surface| surface.visible) {
            for callback in surface.pending_callbacks.drain(..) {
                callback.done(callback_time);
            }
        }
    }

    fn clients_finished(&self, expected_clients: usize) -> bool {
        self.committed_frames > 0
            && expected_clients > 0
            && self.clients.len() >= expected_clients
            && self
                .clients
                .iter()
                .all(|client| client.disconnected.load(Ordering::Acquire))
    }

    fn print_summary(&self) {
        let frame_span = self
            .first_frame_at
            .zip(self.last_frame_at)
            .map(|(first, last)| last.saturating_duration_since(first))
            .unwrap_or_default();
        let measured_fps = if self.presented_frames > 1 && !frame_span.is_zero() {
            (self.presented_frames - 1) as f64 / frame_span.as_secs_f64()
        } else {
            0.0
        };
        println!(
            "summary frames={} changed={} invalid={} dmabuf={} shm={} explicit_sync={} implicit_sync={} presented={} max_surfaces={} releases={} peak_pending={} frame_span_ms={} fps={:.2} process_elapsed_ms={} input_events={} presentation_changes={} profile_layouts={} context_changes={} rate_limited={} abusive_disconnects={} dropped_callbacks={}",
            self.committed_frames,
            self.changed_frames,
            self.invalid_frames,
            self.dmabuf_frames,
            self.shm_frames,
            self.explicit_sync_frames,
            self.implicit_sync_frames,
            self.presented_frames,
            self.max_surfaces,
            self.completed_releases,
            self.peak_pending_releases,
            frame_span.as_millis(),
            measured_fps,
            self.started.elapsed().as_millis(),
            self.input_events,
            self.presentation_changes,
            self.profile_layouts,
            self.context_changes,
            self.rate_limited_commits,
            self.rate_limited_disconnects,
            self.dropped_frame_callbacks
        );
    }
}

fn read_shm_rgba(data: &ShmBufferData, geometry: LayerGeometry) -> Result<Vec<u8>> {
    if data.width != geometry.width
        || data.height != geometry.height
        || data.stride < data.width as usize * 4
        || !matches!(
            data.format,
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
        )
    {
        bail!("buffer does not match the configured region");
    }

    let row_bytes = data.width as usize * 4;
    let required = data
        .offset
        .checked_add(
            data.stride
                .checked_mul(data.height.saturating_sub(1) as usize)
                .context("buffer size overflow")?,
        )
        .and_then(|value| value.checked_add(row_bytes))
        .context("buffer size overflow")?;
    if required > data.map.len() {
        bail!("buffer exceeds its shared-memory pool");
    }

    let mut pixels = vec![0; row_bytes * data.height as usize];
    for source_y in 0..data.height as usize {
        let source_row = data.offset + source_y * data.stride;
        let destination_row = (data.height as usize - 1 - source_y) * row_bytes;
        for x in 0..data.width as usize {
            let source = source_row + x * 4;
            let destination = destination_row + x * 4;
            pixels[destination] = data.map[source + 2];
            pixels[destination + 1] = data.map[source + 1];
            pixels[destination + 2] = data.map[source];
            pixels[destination + 3] = if data.format == wl_shm::Format::Xrgb8888 {
                0xff
            } else {
                data.map[source + 3]
            };
        }
    }
    Ok(pixels)
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                data_init.init(id, ());
            }
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn request(
        state: &mut Self,
        client: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        _data: &(),
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => {
                if let Some(surface_state) = state.surfaces.get_mut(&surface.id()) {
                    let replaced = std::mem::replace(&mut surface_state.pending_buffer, buffer);
                    if let Some(replaced) = replaced {
                        replaced.release();
                    }
                }
            }
            wl_surface::Request::Frame { callback } => {
                if let Some(surface_state) = state.surfaces.get_mut(&surface.id()) {
                    let callback = data_init.init(callback, ());
                    if surface_state.pending_callbacks.len() < MAX_PENDING_FRAME_CALLBACKS {
                        surface_state.pending_callbacks.push(callback);
                    } else {
                        state.dropped_frame_callbacks += 1;
                        callback.done(state.started.elapsed().as_millis() as u32);
                    }
                } else {
                    data_init.init(callback, ());
                }
            }
            wl_surface::Request::Commit => {
                let admission = client
                    .get_data::<ClientTracker>()
                    .map_or(CommitAdmission::Disconnect, |tracker| {
                        tracker.admit_commit(Instant::now())
                    });
                match admission {
                    CommitAdmission::Accept => state.accept_surface_commit(&surface.id()),
                    CommitAdmission::Drop => {
                        state.rate_limited_commits += 1;
                        if state.rate_limited_commits == 1 {
                            eprintln!("client-rate-limited=commit-flood action=drop");
                        }
                        state.drop_surface_commit(&surface.id());
                    }
                    CommitAdmission::Disconnect => {
                        state.rate_limited_commits += 1;
                        state.rate_limited_disconnects += 1;
                        state.drop_surface_commit(&surface.id());
                        eprintln!(
                            "client-disconnected=commit-flood rejected={CLIENT_COMMIT_REJECT_LIMIT} object={}",
                            surface.id().protocol_id()
                        );
                        client.kill(
                            handle,
                            ProtocolError {
                                code: 0,
                                object_id: surface.id().protocol_id(),
                                object_interface: "wl_surface".into(),
                                message: "client exceeded the Touch Bar surface commit budget"
                                    .into(),
                            },
                        );
                    }
                }
            }
            wl_surface::Request::Destroy => {
                state.remove_surface(&surface.id());
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, surface: &wl_surface::WlSurface, _data: &()) {
        state.remove_surface(&surface.id());
    }
}

impl Dispatch<wl_region::WlRegion, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_region::WlRegion,
        _request: wl_region::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_callback::WlCallback,
        _request: wl_callback::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_shm::WlShm>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wl_shm::WlShm,
        request: wl_shm::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            match map_pool(fd, size) {
                Ok(pool) => {
                    data_init.init(id, pool);
                }
                Err(error) => {
                    resource.post_error(wl_shm::Error::InvalidFd, error.to_string());
                }
            }
        }
    }
}

fn map_pool(fd: OwnedFd, size: i32) -> Result<PoolData> {
    if size <= 0 {
        bail!("shared-memory pool size must be positive");
    }
    let file = File::from(fd);
    // SAFETY: the owned file descriptor remains valid for map creation.
    let map = unsafe { MmapOptions::new().len(size as usize).map(&file) }
        .context("failed to map shared-memory pool")?;
    Ok(PoolData {
        map: Arc::new(map),
        size: size as usize,
    })
}

impl Dispatch<wl_shm_pool::WlShmPool, PoolData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        pool: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &PoolData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                let Ok(format) = format.into_result() else {
                    pool.post_error(wl_shm::Error::InvalidFormat, "unknown SHM format");
                    return;
                };
                if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
                    pool.post_error(wl_shm::Error::InvalidStride, "invalid buffer geometry");
                    return;
                }
                let end = offset as usize
                    + stride as usize * height.saturating_sub(1) as usize
                    + width as usize * 4;
                if end > data.size {
                    pool.post_error(wl_shm::Error::InvalidFd, "buffer exceeds SHM pool");
                    return;
                }
                data_init.init(
                    id,
                    BufferData::Shm(ShmBufferData {
                        map: data.map.clone(),
                        offset: offset as usize,
                        width: width as u32,
                        height: height as u32,
                        stride: stride as usize,
                        format,
                    }),
                );
            }
            wl_shm_pool::Request::Resize { size } if size as usize > data.size => {
                pool.post_error(
                    wl_shm::Error::InvalidFd,
                    "growing SHM pools is not implemented in milestone 1",
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, DmabufGlobalData> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
        global_data: &DmabufGlobalData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let dmabuf = data_init.init(resource, global_data.clone());
        if dmabuf.version() < 4 {
            for format in [DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888] {
                dmabuf.format(format);
                dmabuf.modifier(
                    format,
                    (DRM_FORMAT_MOD_INVALID >> 32) as u32,
                    DRM_FORMAT_MOD_INVALID as u32,
                );
                dmabuf.modifier(format, 0, DRM_FORMAT_MOD_LINEAR as u32);
            }
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, DmabufGlobalData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        request: zwp_linux_dmabuf_v1::Request,
        data: &DmabufGlobalData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_dmabuf_v1::Request::CreateParams { params_id } => {
                data_init.init(params_id, DmabufParamsData::default());
            }
            zwp_linux_dmabuf_v1::Request::GetDefaultFeedback { id }
            | zwp_linux_dmabuf_v1::Request::GetSurfaceFeedback { id, .. } => {
                let feedback = data_init.init(id, ());
                send_dmabuf_feedback(&feedback, data);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        _request: zwp_linux_dmabuf_feedback_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

fn send_dmabuf_feedback(
    feedback: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
    data: &DmabufGlobalData,
) {
    feedback.format_table(data.format_table.as_fd(), data.table_size);
    feedback.main_device(data.device.clone());
    feedback.tranche_target_device(data.device.clone());
    feedback.tranche_flags(zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty());
    feedback.tranche_formats(vec![0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0]);
    feedback.tranche_done();
    feedback.done();
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, DmabufParamsData> for State {
    fn request(
        _state: &mut Self,
        client: &Client,
        params_resource: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        request: zwp_linux_buffer_params_v1::Request,
        data: &DmabufParamsData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_buffer_params_v1::Request::Add {
                fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                let mut params = data.0.lock().unwrap();
                if params.used {
                    params_resource.post_error(
                        zwp_linux_buffer_params_v1::Error::AlreadyUsed,
                        "DMA-BUF parameters were already used",
                    );
                    return;
                }
                if plane_idx > 3 {
                    params_resource.post_error(
                        zwp_linux_buffer_params_v1::Error::PlaneIdx,
                        "DMA-BUF plane index exceeds protocol maximum",
                    );
                    return;
                }
                if params.planes.len() <= plane_idx as usize {
                    params.planes.resize_with(plane_idx as usize + 1, || None);
                }
                if params.planes[plane_idx as usize].is_some() {
                    params_resource.post_error(
                        zwp_linux_buffer_params_v1::Error::PlaneSet,
                        "DMA-BUF plane was supplied twice",
                    );
                    return;
                }
                params.planes[plane_idx as usize] = Some(DmabufPlane {
                    fd,
                    offset,
                    stride,
                    modifier: (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
                });
            }
            zwp_linux_buffer_params_v1::Request::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                flags,
            } => match take_dmabuf(data, width, height, format, flags) {
                Ok(buffer_data) => {
                    data_init.init(buffer_id, BufferData::Dmabuf(buffer_data));
                }
                Err(error) => params_resource.post_error(
                    zwp_linux_buffer_params_v1::Error::InvalidWlBuffer,
                    error.to_string(),
                ),
            },
            zwp_linux_buffer_params_v1::Request::Create {
                width,
                height,
                format,
                flags,
            } => match take_dmabuf(data, width, height, format, flags) {
                Ok(buffer_data) => match client
                    .create_resource::<wl_buffer::WlBuffer, BufferData, State>(
                        handle,
                        1,
                        BufferData::Dmabuf(buffer_data),
                    ) {
                    Ok(buffer) => params_resource.created(&buffer),
                    Err(_) => params_resource.failed(),
                },
                Err(_) => params_resource.failed(),
            },
            _ => {}
        }
    }
}

fn take_dmabuf(
    data: &DmabufParamsData,
    width: i32,
    height: i32,
    format: u32,
    flags: wayland_server::WEnum<zwp_linux_buffer_params_v1::Flags>,
) -> Result<DmabufBufferData> {
    if width <= 0 || height <= 0 {
        bail!("DMA-BUF dimensions must be positive");
    }
    if !matches!(format, DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888) {
        bail!("unsupported DMA-BUF format {format:#010x}");
    }
    let flags = flags
        .into_result()
        .map_err(|raw| anyhow::anyhow!("unknown DMA-BUF flags {raw:?}"))?;
    if flags.intersects(
        zwp_linux_buffer_params_v1::Flags::Interlaced
            | zwp_linux_buffer_params_v1::Flags::BottomFirst,
    ) {
        bail!("interlaced DMA-BUFs are unsupported");
    }
    let mut params = data.0.lock().unwrap();
    if params.used {
        bail!("DMA-BUF parameters were already used");
    }
    params.used = true;
    if params.planes.is_empty() || params.planes.iter().any(Option::is_none) {
        bail!("DMA-BUF plane set is incomplete");
    }
    let planes = std::mem::take(&mut params.planes)
        .into_iter()
        .map(Option::unwrap)
        .collect();
    Ok(DmabufBufferData {
        width: width as u32,
        height: height as u32,
        format,
        y_invert: flags.contains(zwp_linux_buffer_params_v1::Flags::YInvert),
        planes,
    })
}

impl GlobalDispatch<touchbar_manager_v1::TouchbarManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<touchbar_manager_v1::TouchbarManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

fn valid_item_spec(spec: &ItemSpec) -> bool {
    !spec.id.as_str().is_empty()
        && spec.min_width > 0
        && spec.min_width <= spec.preferred_width
        && spec.preferred_width <= spec.max_width
}

struct SurfaceRegistration {
    plugin_id: String,
    compact_spec: ItemSpec,
    expanded_spec: Option<ItemSpec>,
}

fn register_surface(
    state: &mut State,
    manager: &touchbar_manager_v1::TouchbarManagerV1,
    id: New<touchbar_surface_v1::TouchbarSurfaceV1>,
    surface: wl_surface::WlSurface,
    registration: SurfaceRegistration,
    data_init: &mut DataInit<'_, State>,
) {
    let SurfaceRegistration {
        plugin_id,
        mut compact_spec,
        expanded_spec,
    } = registration;
    if state.surfaces.len() >= MAX_PLUGIN_SURFACES {
        manager.post_error(0_u32, "all Touch Bar test surfaces are assigned");
        return;
    }
    if plugin_id.trim().is_empty() || !valid_item_spec(&compact_spec) {
        manager.post_error(0_u32, "plugin, item, or compact sizing is invalid");
        return;
    }
    if expanded_spec
        .as_ref()
        .is_some_and(|spec| !valid_item_spec(spec))
    {
        manager.post_error(0_u32, "popover sizing is invalid");
        return;
    }
    if state.surfaces.values().any(|candidate| {
        candidate.kind == SurfaceKind::Item
            && candidate.plugin_id == plugin_id
            && candidate.item_id == compact_spec.id.as_str()
    }) {
        manager.post_error(0_u32, "plugin item identity is already connected");
        return;
    }

    let item_id = compact_spec.id.as_str().to_string();
    let layout_id = match touchbar_profile_config::qualified_item_id(&plugin_id, &item_id) {
        Ok(id) => id,
        Err(error) => {
            manager.post_error(
                0_u32,
                format!("plugin or item identity is invalid: {error}"),
            );
            return;
        }
    };
    compact_spec.id = ItemId::new(layout_id.clone());
    let placeholder = LayerGeometry {
        x: 0,
        width: compact_spec.preferred_width,
        height: TOUCHBAR_HEIGHT,
        opacity: 1.0,
        z_index: 100,
    };
    let layer_id = state.next_layer_id;
    state.next_layer_id += 1;
    let role = data_init.init(
        id,
        TouchbarSurfaceData {
            surface: surface.clone(),
            plugin_id: plugin_id.clone(),
        },
    );
    state.surfaces.insert(
        surface.id(),
        SurfaceState {
            kind: SurfaceKind::Item,
            plugin_id: plugin_id.clone(),
            item_id: item_id.clone(),
            layout_id,
            compact_spec,
            expanded_spec,
            role,
            layer_id,
            compact_geometry: placeholder,
            geometry: placeholder,
            pending_configures: VecDeque::new(),
            deferred_configure: None,
            configured: false,
            visible: true,
            pending_buffer: None,
            pending_acquire_fence: None,
            pending_callbacks: Vec::new(),
            committed_frames: 0,
            last_presentation_session: 0,
        },
    );
    state.max_surfaces = state.max_surfaces.max(state.surfaces.len());
    if let Err(error) = state.try_initialize_live_profiles() {
        manager.post_error(0_u32, format!("profile initialization failed: {error}"));
        return;
    }
    if let Err(error) = state.relayout_compact() {
        manager.post_error(0_u32, format!("compact layout failed: {error}"));
        return;
    }
    if let Some(surface) = state.surfaces.get(&surface.id()) {
        let geometry = surface
            .pending_configures
            .back()
            .map_or(surface.geometry, |(_, geometry)| *geometry);
        println!(
            "assigned plugin={plugin_id} item={item_id} x={} region={}x{} layer={layer_id}",
            geometry.x, geometry.width, geometry.height
        );
    }
}

fn register_backdrop(
    state: &mut State,
    manager: &touchbar_manager_v1::TouchbarManagerV1,
    id: New<touchbar_surface_v1::TouchbarSurfaceV1>,
    surface: wl_surface::WlSurface,
    plugin_id: String,
    data_init: &mut DataInit<'_, State>,
) {
    if state.surfaces.len() >= MAX_PLUGIN_SURFACES {
        manager.post_error(0_u32, "the Touch Bar surface limit was reached");
        return;
    }
    if state.backdrop_surface.is_some() {
        manager.post_error(0_u32, "a backdrop surface is already connected");
        return;
    }
    if plugin_id.trim().is_empty() {
        manager.post_error(0_u32, "backdrop plugin ID is invalid");
        return;
    }
    let canvas_width = state.gpu.canvas_width();
    let geometry = LayerGeometry {
        x: 0,
        width: canvas_width,
        height: TOUCHBAR_HEIGHT,
        opacity: 1.0,
        z_index: 0,
    };
    let layer_id = state.next_layer_id;
    state.next_layer_id += 1;
    let role = data_init.init(
        id,
        TouchbarSurfaceData {
            surface: surface.clone(),
            plugin_id: plugin_id.clone(),
        },
    );
    let surface_id = surface.id();
    state.surfaces.insert(
        surface_id.clone(),
        SurfaceState {
            kind: SurfaceKind::Backdrop,
            plugin_id: plugin_id.clone(),
            item_id: format!("backdrop:{plugin_id}"),
            layout_id: format!("backdrop:{plugin_id}"),
            compact_spec: ItemSpec::new("backdrop", canvas_width, canvas_width, canvas_width),
            expanded_spec: None,
            role,
            layer_id,
            compact_geometry: geometry,
            geometry,
            pending_configures: VecDeque::new(),
            deferred_configure: None,
            configured: false,
            visible: true,
            pending_buffer: None,
            pending_acquire_fence: None,
            pending_callbacks: Vec::new(),
            committed_frames: 0,
            last_presentation_session: 0,
        },
    );
    state.backdrop_surface = Some(surface_id.clone());
    state.max_surfaces = state.max_surfaces.max(state.surfaces.len());
    if let Some(backdrop) = state.surfaces.get(&surface_id) {
        backdrop.role.visibility(1);
    }
    state.configure_surface(&surface_id, geometry);
    println!(
        "assigned plugin={plugin_id} role=backdrop x=0 region={}x{} layer={layer_id}",
        geometry.width, geometry.height
    );
}

impl Dispatch<touchbar_manager_v1::TouchbarManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &touchbar_manager_v1::TouchbarManagerV1,
        request: touchbar_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            touchbar_manager_v1::Request::GetItemSurface {
                id,
                surface,
                plugin_id,
                item_id,
                compact_min_width,
                compact_preferred_width,
                compact_max_width,
                expanded_min_width,
                expanded_preferred_width,
                expanded_max_width,
            } => {
                let compact_spec = ItemSpec::new(
                    item_id.clone(),
                    compact_min_width,
                    compact_preferred_width,
                    compact_max_width,
                );
                let expanded_spec = (expanded_min_width != 0
                    || expanded_preferred_width != 0
                    || expanded_max_width != 0)
                    .then(|| {
                        ItemSpec::new(
                            format!("{item_id}.expanded"),
                            expanded_min_width,
                            expanded_preferred_width,
                            expanded_max_width,
                        )
                    });
                register_surface(
                    state,
                    manager,
                    id,
                    surface,
                    SurfaceRegistration {
                        plugin_id,
                        compact_spec,
                        expanded_spec,
                    },
                    data_init,
                );
            }
            touchbar_manager_v1::Request::GetAppearance { id } => {
                let resource = data_init.init(id, ());
                State::publish_appearance(&resource, state.appearance_source.snapshot());
                state.appearances.insert(resource.id(), resource);
            }
            touchbar_manager_v1::Request::GetBackdropSurface {
                id,
                surface,
                plugin_id,
            } => register_backdrop(state, manager, id, surface, plugin_id, data_init),
            touchbar_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<appearance_protocol::TouchbarAppearanceV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &appearance_protocol::TouchbarAppearanceV1,
        request: appearance_protocol::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let appearance_protocol::Request::Destroy = request {
            state.appearances.remove(&resource.id());
        }
    }
}

impl Dispatch<touchbar_surface_v1::TouchbarSurfaceV1, TouchbarSurfaceData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        role: &touchbar_surface_v1::TouchbarSurfaceV1,
        request: touchbar_surface_v1::Request,
        data: &TouchbarSurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            touchbar_surface_v1::Request::AckConfigure { serial } => {
                let accepted = state
                    .surfaces
                    .get_mut(&data.surface.id())
                    .and_then(|surface| {
                        let position = surface
                            .pending_configures
                            .iter()
                            .position(|(pending_serial, _)| serial == *pending_serial)?;
                        let geometry = surface.pending_configures[position].1;
                        surface.pending_configures.drain(..=position);
                        surface.geometry = geometry;
                        surface.configured = true;
                        let deferred = (surface.pending_configures.len() < MAX_PENDING_CONFIGURES)
                            .then(|| surface.deferred_configure.take())
                            .flatten();
                        Some((surface.layer_id, deferred))
                    });
                if let Some((layer_id, deferred)) = accepted {
                    state.gpu.remove_layer(layer_id);
                    println!(
                        "configured plugin={} surface={:?} serial={serial}",
                        data.plugin_id,
                        data.surface.id()
                    );
                    if let Some(geometry) = deferred {
                        state.configure_surface(&data.surface.id(), geometry);
                    }
                } else {
                    role.post_error(0_u32, "configure acknowledgement has a stale serial");
                }
            }
            touchbar_surface_v1::Request::SetAcquireFence { fd } => {
                let Some(surface) = state.surfaces.get_mut(&data.surface.id()) else {
                    role.post_error(0_u32, "managed surface no longer exists");
                    return;
                };
                if surface.pending_acquire_fence.is_some() {
                    state.invalid_frames += 1;
                    role.post_error(
                        touchbar_surface_v1::Error::DuplicateAcquireFence,
                        "multiple acquire fences supplied for one commit",
                    );
                    return;
                }
                surface.pending_acquire_fence = Some(fd);
            }
            touchbar_surface_v1::Request::BeginPresentation {
                session_id,
                policy,
                lifecycle,
                contact_id,
                target,
            } => {
                if state.active_presentation.as_ref().is_some_and(|active| {
                    active.surface == data.surface.id() && active.session_id == session_id
                }) {
                    return;
                }
                let fresh_session =
                    state
                        .surfaces
                        .get_mut(&data.surface.id())
                        .is_some_and(|surface| {
                            if session_id == 0 || session_id <= surface.last_presentation_session {
                                false
                            } else {
                                surface.last_presentation_session = session_id;
                                true
                            }
                        });
                if !fresh_session {
                    eprintln!("ignored stale presentation begin session={session_id}");
                    state.reject_presentation(&data.surface.id(), session_id);
                    return;
                }
                let policy = match policy.into_result() {
                    Ok(touchbar_surface_v1::PresentationPolicy::Anchored) => {
                        PresentationKind::Anchored
                    }
                    Ok(touchbar_surface_v1::PresentationPolicy::InPlace) => {
                        PresentationKind::InPlace
                    }
                    Ok(touchbar_surface_v1::PresentationPolicy::Slot) => PresentationKind::Slot,
                    Ok(touchbar_surface_v1::PresentationPolicy::Region) => PresentationKind::Region,
                    Ok(touchbar_surface_v1::PresentationPolicy::FullBar) => {
                        PresentationKind::FullBar
                    }
                    Ok(_) | Err(_) => {
                        state.reject_presentation(&data.surface.id(), session_id);
                        return;
                    }
                };
                let lifecycle = lifecycle.into_result();
                if !valid_presentation_target(policy, &target) {
                    state.reject_presentation(&data.surface.id(), session_id);
                    return;
                }
                let persistent = match lifecycle {
                    Ok(touchbar_surface_v1::PresentationLifecycle::Transient) => false,
                    Ok(touchbar_surface_v1::PresentationLifecycle::Persistent) => true,
                    Ok(_) | Err(_) => {
                        state.reject_presentation(&data.surface.id(), session_id);
                        return;
                    }
                };
                state.present(
                    &data.surface.id(),
                    session_id,
                    contact_id,
                    persistent,
                    policy,
                    target,
                );
            }
            touchbar_surface_v1::Request::EndPresentation { session_id, reason } => {
                if !state.active_presentation.as_ref().is_some_and(|active| {
                    active.surface == data.surface.id() && active.session_id == session_id
                }) {
                    eprintln!("ignored stale presentation end session={session_id}");
                    return;
                }
                match reason.into_result() {
                    Ok(reason) => state.restore_compact_with_reason(reason),
                    Err(_) => role.post_error(0_u32, "unknown presentation dismiss reason"),
                }
            }
            touchbar_surface_v1::Request::Destroy => {
                if let Some(surface) = state.surfaces.get_mut(&data.surface.id()) {
                    surface.configured = false;
                    surface.pending_acquire_fence = None;
                }
            }
            _ => {}
        }
    }
}

fn create_dmabuf_global_data() -> Result<DmabufGlobalData> {
    let render_node = match std::env::var_os("TOUCHBAR_RENDER_NODE") {
        Some(path) => PathBuf::from(path),
        None => {
            let mut nodes = std::fs::read_dir("/dev/dri")
                .context("read /dev/dri for DMA-BUF feedback")?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("renderD"))
                })
                .collect::<Vec<_>>();
            nodes.sort();
            nodes
                .into_iter()
                .next()
                .context("no DRM render node is available")?
        }
    };
    let device = std::fs::metadata(&render_node)
        .with_context(|| format!("stat DRM render node {}", render_node.display()))?
        .rdev()
        .to_ne_bytes()
        .to_vec();

    let entries = [
        (DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_APPLE_TILED_COMPRESSED),
        (DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_APPLE_TILED),
        (DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_LINEAR),
        (DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_APPLE_TILED_COMPRESSED),
        (DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_APPLE_TILED),
        (DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR),
    ];
    let mut table = tempfile::tempfile().context("create DMA-BUF format table")?;
    for (format, modifier) in entries {
        table.write_all(&format.to_ne_bytes())?;
        table.write_all(&0_u32.to_ne_bytes())?;
        table.write_all(&modifier.to_ne_bytes())?;
    }
    table.flush()?;
    let table_size = table.metadata()?.len() as u32;
    println!(
        "dmabuf-device={} formats={}",
        render_node.display(),
        entries.len()
    );
    Ok(DmabufGlobalData {
        device,
        format_table: Arc::new(table),
        table_size,
    })
}

struct Args {
    socket: String,
    exit_after_clients: usize,
    frame_output: Option<PathBuf>,
    preview_scale: Option<u32>,
    swapchain_probe: Option<PathBuf>,
    hardware_socket: Option<PathBuf>,
    hardware_listen: Option<PathBuf>,
    demo_touch: Option<DemoTouchKind>,
    profile_demo: bool,
    profiles: Option<PathBuf>,
    demo_focus: bool,
    hyprland_context: bool,
    plugins: bool,
    control_socket: Option<PathBuf>,
    plugin_host: PathBuf,
    plugin_supervisor: PathBuf,
    trusted_plugins: bool,
    system_bar: bool,
}

#[derive(Clone, Copy)]
enum DemoTouchKind {
    HoldSlide,
    TapThenTap,
    Nested,
}

struct DemoTouch {
    kind: DemoTouchKind,
    armed_at: Option<Instant>,
    step: u8,
}

impl DemoTouch {
    fn new(kind: DemoTouchKind) -> Self {
        Self {
            kind,
            armed_at: None,
            step: 0,
        }
    }

    fn pump(&mut self, state: &mut State) {
        if state.committed_frames == 0 || state.surfaces.is_empty() {
            return;
        }
        let armed_at = *self.armed_at.get_or_insert_with(Instant::now);
        let elapsed = armed_at.elapsed();
        let (at, contact_id, phase, x) = match (self.kind, self.step) {
            (DemoTouchKind::HoldSlide, 0) => {
                (Duration::from_millis(50), 1, ContactPhase::Down, 30.0)
            }
            (DemoTouchKind::HoldSlide, 1) => {
                (Duration::from_millis(500), 1, ContactPhase::Motion, 250.0)
            }
            (DemoTouchKind::HoldSlide, 2) => {
                (Duration::from_millis(700), 1, ContactPhase::Motion, 330.0)
            }
            (DemoTouchKind::HoldSlide, 3) => {
                (Duration::from_millis(900), 1, ContactPhase::Up, 330.0)
            }
            (DemoTouchKind::HoldSlide, 4) => {
                (Duration::from_millis(1100), 2, ContactPhase::Down, 700.0)
            }
            (DemoTouchKind::HoldSlide, 5) => {
                (Duration::from_millis(1200), 2, ContactPhase::Cancel, 700.0)
            }
            (DemoTouchKind::TapThenTap, 0) => {
                (Duration::from_millis(50), 1, ContactPhase::Down, 30.0)
            }
            (DemoTouchKind::TapThenTap, 1) => {
                (Duration::from_millis(140), 1, ContactPhase::Up, 30.0)
            }
            (DemoTouchKind::TapThenTap, 2) => {
                (Duration::from_millis(500), 2, ContactPhase::Down, 250.0)
            }
            (DemoTouchKind::TapThenTap, 3) => {
                (Duration::from_millis(600), 2, ContactPhase::Up, 250.0)
            }
            (DemoTouchKind::Nested, 0) => (Duration::from_millis(50), 1, ContactPhase::Down, 30.0),
            (DemoTouchKind::Nested, 1) => (Duration::from_millis(140), 1, ContactPhase::Up, 30.0),
            (DemoTouchKind::Nested, 2) => {
                (Duration::from_millis(400), 2, ContactPhase::Down, 330.0)
            }
            (DemoTouchKind::Nested, 3) => (Duration::from_millis(480), 2, ContactPhase::Up, 330.0),
            (DemoTouchKind::Nested, 4) => {
                (Duration::from_millis(700), 3, ContactPhase::Down, 100.0)
            }
            (DemoTouchKind::Nested, 5) => (Duration::from_millis(780), 3, ContactPhase::Up, 100.0),
            (DemoTouchKind::Nested, 6) => {
                (Duration::from_millis(1000), 4, ContactPhase::Down, 250.0)
            }
            (DemoTouchKind::Nested, 7) => (Duration::from_millis(1080), 4, ContactPhase::Up, 250.0),
            _ => return,
        };
        if elapsed < at {
            return;
        }
        if state.dispatch_touch(
            GlobalContact {
                id: contact_id,
                phase,
                x,
                y: 30.0,
                time_ms: state.started.elapsed().as_millis() as u32,
            },
            touchbar_surface_v1::InputOrigin::Synthetic,
        ) {
            self.step += 1;
        }
    }
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn parse_args() -> Args {
    let mut socket = DEFAULT_SOCKET_NAME.to_string();
    let mut exit_after_clients = 0;
    let mut frame_output = None;
    let mut preview_scale = None;
    let mut swapchain_probe = None;
    let mut hardware_socket = None;
    let mut hardware_listen = None;
    let mut demo_touch = None;
    let mut profile_demo = false;
    let mut profiles = None;
    let mut demo_focus = false;
    let mut hyprland_context = false;
    let mut plugins = true;
    let mut control_socket = None;
    let mut plugin_host = sibling_binary("touchbar-plugin-host");
    let mut plugin_supervisor = sibling_binary("touchbar-plugin-supervisor");
    let mut trusted_plugins = false;
    let mut system_bar = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().expect("--socket requires a name"),
            "--frame-output" => {
                frame_output = Some(PathBuf::from(
                    args.next().expect("--frame-output requires a path"),
                ));
            }
            "--preview" => preview_scale = Some(2),
            "--preview-scale" => {
                preview_scale = Some(
                    args.next()
                        .expect("--preview-scale requires 1, 2, or 4")
                        .parse()
                        .expect("--preview-scale must be an integer"),
                );
            }
            "--swapchain-probe" => {
                swapchain_probe = Some(PathBuf::from(
                    args.next().expect("--swapchain-probe requires a path"),
                ));
            }
            "--hardware-socket" => {
                hardware_socket = Some(PathBuf::from(
                    args.next().expect("--hardware-socket requires a path"),
                ));
            }
            "--hardware-listen" => {
                hardware_listen = Some(PathBuf::from(
                    args.next().expect("--hardware-listen requires a path"),
                ));
            }
            "--demo-touch" => demo_touch = Some(DemoTouchKind::HoldSlide),
            "--demo-tap" => demo_touch = Some(DemoTouchKind::TapThenTap),
            "--demo-nested" => demo_touch = Some(DemoTouchKind::Nested),
            "--profile-demo" => profile_demo = true,
            "--profiles" => {
                profiles = Some(PathBuf::from(
                    args.next().expect("--profiles requires a path"),
                ));
            }
            "--demo-focus" => {
                demo_focus = true;
                profile_demo = true;
            }
            "--hyprland-context" => {
                hyprland_context = true;
            }
            "--no-plugins" => plugins = false,
            "--system-bar" => system_bar = true,
            "--control-socket" => {
                control_socket = Some(PathBuf::from(
                    args.next().expect("--control-socket requires a path"),
                ));
            }
            "--plugin-host" => {
                plugin_host = PathBuf::from(args.next().expect("--plugin-host requires a path"));
            }
            "--plugin-supervisor" => {
                plugin_supervisor =
                    PathBuf::from(args.next().expect("--plugin-supervisor requires a path"));
            }
            "--trusted-plugins" => trusted_plugins = true,
            "--exit-after-client" => exit_after_clients = 1,
            "--exit-after-clients" => {
                exit_after_clients = args
                    .next()
                    .expect("--exit-after-clients requires a count")
                    .parse()
                    .expect("--exit-after-clients must be an integer");
            }
            "--help" | "-h" => {
                println!(
                    "usage: touchbar-sessiond [--socket NAME] [--frame-output PATH] [--preview | --preview-scale 1|2|4] [--hardware-socket PATH | --hardware-listen PATH | --swapchain-probe PATH] [--system-bar] [--profiles FILE | --profile-demo] [--demo-touch | --demo-tap | --demo-nested] [--demo-focus | --hyprland-context] [--no-plugins] [--control-socket PATH] [--plugin-host PATH --plugin-supervisor PATH] [--trusted-plugins] [--exit-after-client | --exit-after-clients N]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    Args {
        socket,
        exit_after_clients,
        frame_output,
        preview_scale,
        swapchain_probe,
        hardware_socket,
        hardware_listen,
        demo_touch,
        profile_demo,
        profiles,
        demo_focus,
        hyprland_context,
        plugins,
        control_socket,
        plugin_host,
        plugin_supervisor,
        trusted_plugins,
        system_bar,
    }
}

fn sibling_binary(name: &str) -> PathBuf {
    std::env::current_exe()
        .map(|path| path.with_file_name(name))
        .unwrap_or_else(|_| PathBuf::from(name))
}

fn run_swapchain_probe(gpu: &mut GpuCompositor, path: &PathBuf) -> Result<()> {
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind ADP swapchain socket {}", path.display()))?;
    let _socket_cleanup = SocketCleanup(path.clone());
    println!("swapchain-probe=ready socket={}", path.display());
    let (mut stream, _) = listener
        .accept()
        .context("accept privileged ADP presenter")?;
    let (info, buffers) =
        receive_hardware_swapchain(&stream).context("receive ADP swapchain descriptors")?;
    gpu.install_output_swapchain(info, buffers)?;

    // A solid, opaque test layer makes both the centered active region and
    // XRGB channel order deterministic for the non-modesetting producer.
    let pixel = [0x20, 0x40, 0x80, 0xff];
    let pixels = pixel
        .into_iter()
        .cycle()
        .take((gpu.canvas_width() * TOUCHBAR_HEIGHT * 4) as usize)
        .collect::<Vec<_>>();
    gpu.update_rgba_layer(
        1,
        LayerGeometry {
            x: 0,
            width: gpu.canvas_width(),
            height: TOUCHBAR_HEIGHT,
            opacity: 1.0,
            z_index: 0,
        },
        &pixels,
    )?;
    let checksum = gpu.compose_scene();
    for index in 0..gpu.output_buffer_count() {
        gpu.render_scene_to_output(index)?;
        send_session_message(
            &mut stream,
            SessionMessage::FrameReady {
                index: index as u16,
                sequence: index as u64 + 1,
            },
        )
        .context("send rendered ADP buffer event")?;
    }
    for index in 0..gpu.output_buffer_count() {
        let expected = HardwareMessage::BufferReleased {
            index: index as u16,
            sequence: index as u64 + 1,
        };
        let event =
            receive_hardware_message(&mut stream).context("receive ADP buffer release event")?;
        if event != expected {
            bail!("unexpected ADP release event {event:?}; expected {expected:?}");
        }
    }
    println!(
        "swapchain-probe=ok buffers={} scene={}x{} checksum={checksum:016x}",
        gpu.output_buffer_count(),
        gpu.canvas_width(),
        TOUCHBAR_HEIGHT
    );
    Ok(())
}

fn connect_hardware_output(gpu: &mut GpuCompositor, path: &PathBuf) -> Result<DirectOutput> {
    let stream = UnixStream::connect(path)
        .with_context(|| format!("connect to touchbard at {}", path.display()))?;
    install_hardware_output(gpu, stream)
}

fn accept_diagnostic_hardware(gpu: &mut GpuCompositor, path: &PathBuf) -> Result<DirectOutput> {
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind diagnostic hardware socket {}", path.display()))?;
    let socket_cleanup = SocketCleanup(path.clone());
    println!("hardware-listen=waiting socket={}", path.display());
    let (stream, _) = listener
        .accept()
        .context("accept diagnostic hardware presenter")?;
    let output = install_hardware_output(gpu, stream)?;
    drop(listener);
    drop(socket_cleanup);
    Ok(output)
}

fn install_hardware_output(gpu: &mut GpuCompositor, stream: UnixStream) -> Result<DirectOutput> {
    let (info, buffers) =
        receive_hardware_swapchain(&stream).context("receive hardware swapchain descriptors")?;
    gpu.install_output_swapchain(info, buffers)?;
    println!(
        "hardware-output=ready buffers={} logical={}x{} physical={}x{} pitch={}",
        info.buffer_count,
        info.logical_width,
        info.logical_height,
        info.physical_width,
        info.physical_height,
        info.pitch
    );
    DirectOutput::new(stream, usize::from(info.buffer_count))
}

fn initialize_session_permissions(path: &Path) -> Result<()> {
    // The runtime file is only an IPC mechanism for the lifetime of this
    // compositor. Never let decisions survive a sessiond restart.
    touchbar_policy::GrantStore::default()
        .save(path)
        .context("initialize session-only plugin permissions")
}

fn load_live_profiles(
    args: &Args,
) -> Result<(
    Option<LiveProfiles>,
    Option<ProfileDocument>,
    Option<PathBuf>,
)> {
    if args.profile_demo && args.profiles.is_some() {
        bail!("--profiles and --profile-demo are mutually exclusive");
    }
    if args.profile_demo {
        println!("profile-source=built-in-demo");
        return Ok((Some(LiveProfiles::demo()), None, None));
    }

    let explicit = args.profiles.is_some();
    let path = args
        .profiles
        .clone()
        .map(Ok)
        .unwrap_or_else(touchbar_profile_config::discover_path)?;
    if !path.exists() {
        if explicit {
            bail!("profile configuration {} does not exist", path.display());
        }
        println!(
            "profile-source=automatic-all-items config={}",
            path.display()
        );
        return Ok((None, None, Some(path)));
    }
    let document = touchbar_profile_config::load(&path)?;
    println!(
        "profile-source={} profiles={} items={}",
        path.display(),
        document.profile_ids().len(),
        document.item_ids().len()
    );
    Ok((
        Some(LiveProfiles::configured(document.clone())),
        Some(document),
        Some(path),
    ))
}

fn choose_wait_delay(delays: impl IntoIterator<Item = Option<Duration>>) -> Option<Duration> {
    delays.into_iter().flatten().min()
}

fn wait_for_work(fds: impl IntoIterator<Item = RawFd>, delay: Option<Duration>) -> io::Result<()> {
    let mut poll_fds = fds
        .into_iter()
        .filter(|fd| *fd >= 0)
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect::<Vec<_>>();
    let timeout = delay.map(|delay| libc::timespec {
        tv_sec: delay.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        tv_nsec: delay.subsec_nanos().into(),
    });
    loop {
        let result = unsafe {
            libc::ppoll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as _,
                timeout
                    .as_ref()
                    .map_or(std::ptr::null(), std::ptr::from_ref),
                std::ptr::null(),
            )
        };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted && TERMINATE_SESSION.load(Ordering::Relaxed) {
            return Ok(());
        }
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

extern "C" fn terminate_session(_: libc::c_int) {
    TERMINATE_SESSION.store(true, Ordering::Relaxed);
}

fn install_termination_signal_handlers() -> Result<()> {
    TERMINATE_SESSION.store(false, Ordering::Relaxed);
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the signal handler performs only one lock-free atomic store.
        let previous =
            unsafe { libc::signal(signal, terminate_session as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error()).context("install session signal handler");
        }
    }
    Ok(())
}

fn control_lease_released(stream: &UnixStream) -> io::Result<bool> {
    let mut byte = [0_u8; 1];
    // SAFETY: `byte` is writable for the supplied one-byte length, the socket
    // descriptor remains borrowed for the call, and MSG_PEEK consumes no data.
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            byte.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    if result > 0 {
        // A lease is deliberately one-way after its response. Treat any extra
        // client bytes as release instead of leaving an unbounded input queue.
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

fn advance_frame_deadline(next_frame: &mut Instant, now: Instant, frame_period: Duration) {
    if *next_frame > now {
        return;
    }
    let elapsed_periods = now.duration_since(*next_frame).as_nanos() / frame_period.as_nanos();
    let periods = elapsed_periods.saturating_add(1);
    if let Ok(periods) = u32::try_from(periods) {
        *next_frame += frame_period * periods;
    } else {
        *next_frame = now + frame_period;
    }
}

fn main() -> Result<()> {
    let args = parse_args();
    install_termination_signal_handlers()?;
    let plugin_paths = touchbar_plugin_store::StorePaths::discover()?;
    let (live_profiles, user_profile_document, profile_path) = load_live_profiles(&args)?;
    let mut profile_watcher =
        profile_path
            .as_deref()
            .and_then(|path| match ProfileWatcher::new(path) {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    eprintln!("profile-watch=disabled error={error:#}");
                    None
                }
            });
    let mut profile_reload_deadline = None;
    if args.plugins {
        // Session decisions deliberately expire with this compositor session.
        // Reinitialize the private runtime store before any supervisor starts.
        initialize_session_permissions(&plugin_paths.session_grants)?;
    }
    let mut gpu = GpuCompositor::new(DEFAULT_REGION_WIDTH, TOUCHBAR_HEIGHT)
        .context("initialize GPU compositor")?;
    println!("compositor-renderer={}", gpu.renderer_name());
    if let Some(path) = &args.swapchain_probe {
        return run_swapchain_probe(&mut gpu, path);
    }
    if args.hardware_socket.is_some() && args.hardware_listen.is_some() {
        bail!("--hardware-socket and --hardware-listen are mutually exclusive");
    }
    if args.preview_scale.is_some()
        && (args.hardware_socket.is_some()
            || args.hardware_listen.is_some()
            || args.swapchain_probe.is_some())
    {
        bail!("desktop preview and physical hardware output are mutually exclusive");
    }
    let direct_output = match (&args.hardware_socket, &args.hardware_listen) {
        (Some(path), None) => match connect_hardware_output(&mut gpu, path) {
            Ok(output) => Some(output),
            Err(error) => {
                eprintln!("hardware-output=reconnect-pending attempts=0 error={error:#}");
                None
            }
        },
        (None, Some(path)) => Some(accept_diagnostic_hardware(&mut gpu, path)?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!(),
    };
    let frame_publisher = args
        .frame_output
        .as_deref()
        .map(|path| FramePublisher::new(path, gpu.canvas_width(), TOUCHBAR_HEIGHT))
        .transpose()?;
    let preview_output = args
        .preview_scale
        .map(|scale| PreviewOutput::connect(gpu.canvas_width(), TOUCHBAR_HEIGHT, scale))
        .transpose()?;
    let dmabuf_global = create_dmabuf_global_data()?;
    let mut display = Display::<State>::new().context("create Wayland display")?;
    let mut handle = display.handle();
    handle.create_global::<State, wl_compositor::WlCompositor, _>(6, ());
    handle.create_global::<State, wl_shm::WlShm, _>(1, ());
    handle.create_global::<State, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _>(4, dmabuf_global);
    handle.create_global::<State, touchbar_manager_v1::TouchbarManagerV1, _>(
        TOUCHBAR_PROTOCOL_VERSION,
        (),
    );

    let listener = ListeningSocket::bind(&args.socket)
        .with_context(|| format!("bind Wayland socket {}", args.socket))?;
    let control_path = args
        .control_socket
        .clone()
        .unwrap_or_else(|| plugin_paths.control.clone());
    let control = (args.plugins || args.control_socket.is_some())
        .then(|| touchbar_control::Server::bind(&control_path))
        .transpose()?;
    let mut plugin_manager = args
        .plugins
        .then(|| {
            plugins::PluginManager::new(
                plugin_paths,
                args.plugin_supervisor.clone(),
                args.plugin_host.clone(),
                args.socket.clone(),
                args.trusted_plugins,
            )
        })
        .transpose()?;
    let mut state = State::new(
        gpu,
        frame_publisher,
        preview_output,
        direct_output,
        args.hardware_socket.clone(),
        live_profiles,
        user_profile_document,
        profile_path,
        args.system_bar,
    )?;
    if let Some(manager) = &plugin_manager {
        state.set_presentation_catalog(manager.presentation_catalog().clone());
        state.set_packaged_profile_catalog(manager.profile_catalog().clone())?;
    }
    let mut frame_period = state.animation_frame_period();
    let mut next_frame = Instant::now() + frame_period;
    let mut demo_touch = args.demo_touch.map(DemoTouch::new);
    let mut demo_focus = args.demo_focus.then(ContextReplay::demo);
    let mut next_hyprland_reconnect = Instant::now();
    let mut hyprland_reconnect_failures = 0_u64;
    let (mut hyprland_context, initial_context) = if args.hyprland_context {
        match HyprlandContextSource::connect() {
            Ok((source, initial)) => {
                println!("context-source=hyprland ready=true");
                (Some(source), initial)
            }
            Err(error) => {
                eprintln!("context-source=hyprland pending=true error={error:#}");
                hyprland_reconnect_failures = 1;
                next_hyprland_reconnect = Instant::now() + Duration::from_secs(5);
                (None, Vec::new())
            }
        }
    } else {
        (None, Vec::new())
    };
    for event in initial_context {
        state.handle_context_event(event)?;
    }

    println!(
        "ready socket={} region={}x{} refresh_millihz={REFRESH_MILLIHZ}",
        args.socket,
        state.gpu.canvas_width(),
        TOUCHBAR_HEIGHT
    );

    let mut hardware_yield_lease: Option<UnixStream> = None;

    while !TERMINATE_SESSION.load(Ordering::Relaxed) {
        if let Some(stream) = &hardware_yield_lease
            && control_lease_released(stream).context("poll hardware-yield lease")?
        {
            hardware_yield_lease = None;
            state.resume_hardware();
        }
        if let Some(control) = &control {
            for (mut stream, request) in control.poll()? {
                let hardware_yield =
                    matches!(&request, touchbar_control::Request::HardwareYield { .. });
                let result = match request {
                    touchbar_control::Request::Ping { .. } => Ok("pong"),
                    touchbar_control::Request::Status { .. } => Ok("running"),
                    touchbar_control::Request::Reload { .. } => (|| -> Result<_> {
                        if let Some(manager) = plugin_manager.as_mut() {
                            manager.reload().context("reload plugins")?;
                            state.set_presentation_catalog(manager.presentation_catalog().clone());
                            state
                                .set_packaged_profile_catalog(manager.profile_catalog().clone())?;
                        }
                        state.reload_profiles().context("reload profiles")?;
                        Ok("runtime configuration reloaded")
                    })(),
                    touchbar_control::Request::ProfileSelect { profile, .. } => {
                        state.select_profile(profile)
                    }
                    touchbar_control::Request::ProfileAutomatic { .. } => {
                        state.use_automatic_profile()
                    }
                    touchbar_control::Request::HardwareYield { .. } => (|| -> Result<_> {
                        if hardware_yield_lease.is_some() {
                            bail!("hardware is already yielded to another developer session")
                        }
                        state.yield_hardware()?;
                        Ok("hardware yielded while this control connection remains open")
                    })(),
                };
                let (ok, message) = match result {
                    Ok(message) => (true, message.to_owned()),
                    Err(error) => (false, format!("command failed: {error:#}")),
                };
                let processes = plugin_manager
                    .as_mut()
                    .map(plugins::PluginManager::status)
                    .unwrap_or_default();
                let runtime = touchbar_control::SessionRuntimeStatus {
                    hardware_connected: state.direct_output.is_some(),
                    hardware_yielded: state.hardware_yielded,
                    user_content_visible: state.has_visible_user_content(),
                    fn_pressed: state.fn_pressed,
                    system_scene_visible: state.system_scene_visible,
                    profile: state.profile_status(),
                    plugin_placeholder: state.plugin_placeholder.as_ref().map(|placeholder| {
                        touchbar_control::PluginPlaceholderStatus {
                            item: placeholder.item.clone(),
                            message: placeholder.message.clone(),
                        }
                    }),
                    power_source: state.power_source_status(),
                    animation_frame_rate_hz: state.animation_frame_rate_hz(),
                };
                touchbar_control::write_response(
                    &mut stream,
                    &touchbar_control::Response {
                        version: touchbar_control::VERSION,
                        ok,
                        message,
                        runtime,
                        processes,
                    },
                )?;
                if hardware_yield && ok {
                    stream
                        .set_nonblocking(true)
                        .context("make hardware-yield lease nonblocking")?;
                    hardware_yield_lease = Some(stream);
                }
            }
        }
        if let Some(manager) = &mut plugin_manager {
            manager.poll();
        }
        while let Some(stream) = listener.accept().context("accept Wayland client")? {
            let tracker = Arc::new(ClientTracker::default());
            handle
                .insert_client(stream, tracker.clone())
                .context("insert Wayland client")?;
            state.clients.push(tracker);
        }

        match display.dispatch_clients(&mut state) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                eprintln!("wayland-client=disconnected error={error}");
            }
            Err(error) => return Err(error).context("dispatch Wayland requests"),
        }

        let missing_item = state
            .live_profiles
            .as_ref()
            .and_then(|profiles| profiles.missing_required_items().into_iter().next());
        let placeholder = missing_item.map(|item| {
            let reason = plugin_manager
                .as_ref()
                .and_then(|manager| manager.unavailable_reason(&item))
                .unwrap_or(plugins::UnavailableReason::Unavailable);
            PluginPlaceholder {
                item,
                message: reason.message().to_owned(),
            }
        });
        state.set_plugin_placeholder(placeholder)?;

        state.poll_direct_input();
        state.poll_preview_input()?;
        if state.preview_closed() {
            break;
        }
        state.poll_hardware_reconnect();
        let pacing_changed = state.poll_appearance()? | state.poll_power();
        state.poll_transient_context()?;
        if pacing_changed {
            frame_period = state.animation_frame_period();
            next_frame = Instant::now() + frame_period;
            println!(
                "animation-cadence={}Hz power={}",
                state.animation_frame_rate_hz(),
                state.power_source.state().label()
            );
        }
        if let Some(watcher) = &mut profile_watcher {
            match watcher.drain() {
                Ok(true) => {
                    profile_reload_deadline = Some(Instant::now() + PROFILE_RELOAD_COALESCE)
                }
                Ok(false) => {}
                Err(error) => {
                    eprintln!("profile-watch=disabled error={error:#}");
                    profile_watcher = None;
                    profile_reload_deadline = None;
                }
            }
        }
        if profile_reload_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            profile_reload_deadline = None;
            match state.reload_profiles() {
                Ok(()) => println!("profile-watch=reloaded"),
                Err(error) => eprintln!("profile-watch=reload-rejected error={error:#}"),
            }
        }

        if state.profiles_ready()
            && let Some(replay) = &mut demo_focus
        {
            for event in replay.poll() {
                state.handle_context_event(event)?;
            }
            if replay.finished() {
                demo_focus = None;
            }
        }
        if let Some(source) = &hyprland_context {
            match source.poll() {
                Ok(events) => {
                    for event in events {
                        state.handle_context_event(event)?;
                    }
                }
                Err(error) => {
                    eprintln!("Hyprland context source stopped: {error:#}");
                    hyprland_context = None;
                    next_hyprland_reconnect = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        if args.hyprland_context
            && hyprland_context.is_none()
            && Instant::now() >= next_hyprland_reconnect
        {
            match HyprlandContextSource::connect() {
                Ok((source, initial)) => {
                    println!("context-source=hyprland reconnected=true");
                    hyprland_context = Some(source);
                    hyprland_reconnect_failures = 0;
                    for event in initial {
                        state.handle_context_event(event)?;
                    }
                }
                Err(error) => {
                    hyprland_reconnect_failures += 1;
                    if hyprland_reconnect_failures == 1
                        || hyprland_reconnect_failures.is_multiple_of(12)
                    {
                        eprintln!(
                            "context-source=hyprland pending=true attempts={} error={error:#}",
                            hyprland_reconnect_failures
                        );
                    }
                    next_hyprland_reconnect = Instant::now() + Duration::from_secs(5);
                }
            }
        }

        if let Some(demo_touch) = &mut demo_touch {
            demo_touch.pump(&mut state);
        }

        state.sync_system_scene_visibility()?;

        let now = Instant::now();
        if state.needs_frame_tick() && now >= next_frame {
            state.present_scene()?;
            state.poll_buffer_releases();
            state.send_frame_callbacks();
            advance_frame_deadline(&mut next_frame, now, frame_period);
        }
        state.poll_buffer_releases();

        display.flush_clients().context("flush Wayland clients")?;

        if state.clients_finished(args.exit_after_clients) {
            state.present_scene()?;
            state.finish_buffer_releases();
            display
                .flush_clients()
                .context("flush final buffer releases")?;
            break;
        }

        let frame_delay = state
            .needs_frame_tick()
            .then(|| next_frame.saturating_duration_since(Instant::now()));
        let release_delay =
            (!state.pending_releases.is_empty()).then_some(GPU_RELEASE_POLL_INTERVAL);
        let demo_delay =
            (demo_touch.is_some() || demo_focus.is_some()).then_some(DEMO_POLL_INTERVAL);
        let hyprland_delay = (args.hyprland_context && hyprland_context.is_none())
            .then(|| next_hyprland_reconnect.saturating_duration_since(Instant::now()));
        let profile_reload_delay = profile_reload_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let wait_delay = choose_wait_delay([
            frame_delay,
            release_delay,
            demo_delay,
            state.appearance_source.next_poll_delay(),
            Some(state.power_source.next_poll_delay()),
            state.next_hardware_reconnect_delay(),
            state.next_transient_context_delay(),
            hyprland_delay,
            profile_reload_delay,
            plugin_manager
                .as_ref()
                .and_then(plugins::PluginManager::next_poll_delay),
        ]);
        let mut wait_fds = vec![display.as_fd().as_raw_fd(), listener.as_raw_fd()];
        if let Some(control) = &control {
            wait_fds.push(control.as_fd().as_raw_fd());
        }
        if let Some(lease) = &hardware_yield_lease {
            wait_fds.push(lease.as_raw_fd());
        }
        if let Some(output) = &state.direct_output {
            wait_fds.push(output.notification_fd());
        }
        if let Some(output) = &state.preview_output {
            wait_fds.push(output.notification_fd());
        }
        if let Some(source) = &hyprland_context {
            wait_fds.push(source.notification_fd());
        }
        if let Some(watcher) = &profile_watcher {
            wait_fds.push(watcher.notification_fd());
        }
        wait_for_work(wait_fds, wait_delay).context("wait for session work")?;
    }

    state.release_system_keys();
    state.print_summary();
    Ok(())
}

#[cfg(test)]
mod session_permission_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn fn_double_tap_and_hold_selects_media_until_release() {
        let started = Instant::now();
        let mut gesture = FnLayerGesture::default();
        assert_eq!(
            gesture.transition(true, started),
            Some(SystemLayer::Function)
        );
        assert_eq!(
            gesture.transition(false, started + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            gesture.transition(true, started + Duration::from_millis(300)),
            Some(SystemLayer::Media)
        );
        assert_eq!(
            gesture.transition(false, started + Duration::from_millis(900)),
            None
        );
        assert_eq!(
            gesture.transition(true, started + Duration::from_millis(950)),
            Some(SystemLayer::Function)
        );
    }

    #[test]
    fn long_or_expired_fn_taps_do_not_arm_media() {
        let started = Instant::now();
        let mut gesture = FnLayerGesture::default();
        gesture.transition(true, started);
        gesture.transition(
            false,
            started + FN_TAP_MAX_DURATION + Duration::from_millis(1),
        );
        assert_eq!(
            gesture.transition(true, started + Duration::from_millis(300)),
            Some(SystemLayer::Function)
        );

        gesture.reset();
        gesture.transition(true, started);
        gesture.transition(false, started + Duration::from_millis(50));
        assert_eq!(
            gesture.transition(
                true,
                started
                    + Duration::from_millis(50)
                    + FN_DOUBLE_TAP_WINDOW
                    + Duration::from_millis(1),
            ),
            Some(SystemLayer::Function)
        );
    }

    #[test]
    fn client_commit_budget_allows_60_hz_rendering_indefinitely() {
        let started = Instant::now();
        let mut budget = CommitBudget::new(started);
        for frame in 0..3_600 {
            let now = started + Duration::from_nanos(16_666_667 * frame);
            assert_eq!(budget.admit(now), CommitAdmission::Accept);
        }
    }

    #[test]
    fn hardware_yield_lease_detects_drop_and_rejects_extra_bytes() {
        let (server, mut client) = UnixStream::pair().unwrap();
        assert!(!control_lease_released(&server).unwrap());
        client.write_all(b"x").unwrap();
        assert!(control_lease_released(&server).unwrap());

        let (server, client) = UnixStream::pair().unwrap();
        assert!(!control_lease_released(&server).unwrap());
        drop(client);
        assert!(control_lease_released(&server).unwrap());
    }

    #[test]
    fn client_commit_budget_bounds_bursts_before_buffer_import() {
        let now = Instant::now();
        let mut budget = CommitBudget::new(now);
        for _ in 0..CLIENT_COMMIT_BURST {
            assert_eq!(budget.admit(now), CommitAdmission::Accept);
        }
        assert_eq!(budget.admit(now), CommitAdmission::Drop);
        assert_eq!(
            budget.admit(now + Duration::from_millis(9)),
            CommitAdmission::Accept
        );
    }

    #[test]
    fn sustained_commit_flood_disconnects_the_client() {
        let now = Instant::now();
        let mut budget = CommitBudget::new(now);
        for _ in 0..CLIENT_COMMIT_BURST {
            assert_eq!(budget.admit(now), CommitAdmission::Accept);
        }
        for _ in 1..CLIENT_COMMIT_REJECT_LIMIT {
            assert_eq!(budget.admit(now), CommitAdmission::Drop);
        }
        assert_eq!(budget.admit(now), CommitAdmission::Disconnect);
    }

    #[test]
    fn wait_delay_uses_only_the_earliest_real_deadline() {
        assert_eq!(
            choose_wait_delay([
                None,
                Some(Duration::from_millis(250)),
                Some(Duration::from_millis(17)),
            ]),
            Some(Duration::from_millis(17))
        );
        assert_eq!(choose_wait_delay([None, None]), None);
    }

    #[test]
    fn frame_deadline_skips_idle_history_without_drift() {
        let started = Instant::now();
        let period = Duration::from_millis(10);
        let mut next = started + period;
        advance_frame_deadline(&mut next, started + Duration::from_millis(35), period);
        assert_eq!(next, started + Duration::from_millis(40));
        advance_frame_deadline(&mut next, started + Duration::from_millis(35), period);
        assert_eq!(next, started + Duration::from_millis(40));
    }

    #[test]
    fn startup_replaces_stale_session_permission_state() {
        let temporary = tempfile::tempdir().unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = temporary.path().join("session-permissions.toml");
        fs::write(&path, "stale or corrupt authority").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        initialize_session_permissions(&path).unwrap();

        let store = touchbar_policy::GrantStore::load(&path).unwrap();
        assert_eq!(store.records().count(), 0);
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn presentation_targets_are_policy_specific_and_path_free() {
        assert!(valid_presentation_target(PresentationKind::Slot, "content"));
        assert!(valid_presentation_target(
            PresentationKind::Region,
            "center-wide"
        ));
        for target in [
            "",
            "../content",
            "/absolute",
            "Content",
            "two words",
            ".hidden",
        ] {
            assert!(!valid_presentation_target(PresentationKind::Slot, target));
        }
        assert!(valid_presentation_target(PresentationKind::FullBar, ""));
        assert!(!valid_presentation_target(
            PresentationKind::FullBar,
            "content"
        ));
        assert!(!valid_presentation_target(
            PresentationKind::Anchored,
            "content"
        ));
    }
}
