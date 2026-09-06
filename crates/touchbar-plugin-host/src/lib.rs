//! Resource-bounded host for sandboxed TouchBar WebAssembly Components.
//!
//! The component receives capability-free WASI plumbing for ordinary Rust
//! `std`, but no environment values, preopened filesystem paths, allowed
//! network addresses, or session services in this first vertical slice. It
//! returns a bounded, flat semantic node arena which this crate validates and
//! converts into the native, theme-aware `touchbar-ui` retained tree.

mod confinement;

pub use confinement::apply_component_confinement;

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::{BufReader, Cursor},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::FileExt,
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use touchbar_package::{
    AssetDefinition, AssetKind, BundledAsset, MAX_ASSET_BUNDLE_BYTES, MAX_TOUCHBAR_WIDTH,
    decode_asset_bundle, encode_asset_bundle,
};
use touchbar_protocol::broker_ipc::{
    ActivationContext, ActivationOrigin, BrokerErrorCode, BrokerResult, CallbackPhase,
    CapabilityState, DEFAULT_ACTIVATION_LIFETIME, HostMessage, Seqpacket, SupervisorMessage,
    WireCapabilityStatus, monotonic_micros,
};
use touchbar_ui::{
    CanvasColor, CanvasCommand, CanvasLinearGradient, CanvasPaint, Color, ColorRole,
    ContinuousValue, CrossAxisAlignment, Easing, EffectProgram, Flex, FlexItem, Icon, Image,
    ImageFit, ImageTint, MAX_EFFECT_NODES, MAX_EFFECT_PARAMETERS, MAX_EFFECT_PROGRAMS, MeterStyle,
    Motion, MotionId, MotionPlayback, MotionPolicy, Node as UiNode, Point, PressableStyle,
    ProgressValue, Rect, Representation, ResponsiveVariant, RetainedUi, ShaderEffect, Size,
    SvgAsset, SvgRasterizer, TextAlign, TextMeasurement, TextOverflow, Theme, VisualTransform,
    WidgetId,
};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::error::Context as _;
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

pub mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "plugin",
    });
}

use bindings::Plugin;
use bindings::touchbar::plugin::broker as wit_broker;
use bindings::touchbar::plugin::ui as wit;

static NEXT_SURFACE_INSTANCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorScheme {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appearance {
    pub revision: u64,
    pub scheme: ColorScheme,
    pub motion: MotionPolicy,
    pub theme: Theme,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            revision: 1,
            scheme: ColorScheme::Dark,
            motion: MotionPolicy::Full,
            theme: Theme::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HostLimits {
    pub memory_bytes: usize,
    pub table_elements: usize,
    pub instances: usize,
    pub memories: usize,
    pub tables: usize,
    pub fuel_per_call: u64,
    pub max_items: usize,
    pub max_nodes: usize,
    pub max_expanded_nodes: usize,
    pub max_tree_depth: usize,
    pub max_text_bytes: usize,
    pub max_canvas_commands: usize,
    pub max_canvas_points: usize,
    pub max_canvas_path_segments: usize,
    pub max_canvas_fill_triangles: usize,
    pub max_animations: usize,
    pub max_effect_nodes: usize,
    pub max_effect_programs: usize,
    pub activation_lifetime: Duration,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 16 * 1024 * 1024,
            table_elements: 10_000,
            instances: 16,
            memories: 4,
            tables: 4,
            fuel_per_call: 5_000_000,
            max_items: 64,
            max_nodes: 256,
            max_expanded_nodes: 512,
            max_tree_depth: 32,
            max_text_bytes: 16 * 1024,
            max_canvas_commands: 512,
            max_canvas_points: 2048,
            max_canvas_path_segments: 256,
            max_canvas_fill_triangles: 512,
            max_animations: 64,
            max_effect_nodes: MAX_EFFECT_NODES,
            max_effect_programs: MAX_EFFECT_PROGRAMS,
            activation_lifetime: DEFAULT_ACTIVATION_LIFETIME,
        }
    }
}

struct HostState {
    store_limits: StoreLimits,
    wasi: WasiCtx,
    resources: ResourceTable,
    broker: Option<BrokerClient>,
    phase: Option<CallbackPhase>,
    surface_instance: u64,
    activation: Option<ActivationContext>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.resources,
        }
    }
}

impl wit::Host for HostState {}

impl wit_broker::Host for HostState {
    fn capabilities(&mut self) -> wit_broker::CapabilitySnapshot {
        let Some(broker) = self.broker.as_ref() else {
            return wit_broker::CapabilitySnapshot {
                generation: 0,
                states: Vec::new(),
            };
        };
        wit_broker::CapabilitySnapshot {
            generation: broker.generation(),
            states: broker
                .states()
                .iter()
                .map(to_wit_capability_state)
                .collect(),
        }
    }

    fn request(
        &mut self,
        capability: String,
        operation: String,
        payload: Vec<u8>,
    ) -> std::result::Result<u64, wit_broker::ErrorCode> {
        let phase = self.phase.ok_or(wit_broker::ErrorCode::InvalidPhase)?;
        if matches!(phase, CallbackPhase::Items | CallbackPhase::Render) {
            return Err(wit_broker::ErrorCode::InvalidPhase);
        }
        self.broker
            .as_mut()
            .ok_or(wit_broker::ErrorCode::Unavailable)?
            .submit(
                phase,
                capability,
                operation,
                payload,
                self.activation.clone(),
            )
            .map_err(|_| wit_broker::ErrorCode::Internal)
    }

    fn cancel(&mut self, request_id: u64) -> std::result::Result<(), wit_broker::ErrorCode> {
        if !matches!(
            self.phase,
            Some(CallbackPhase::Input | CallbackPhase::HostEvent)
        ) {
            return Err(wit_broker::ErrorCode::InvalidPhase);
        }
        self.broker
            .as_mut()
            .ok_or(wit_broker::ErrorCode::Unavailable)?
            .cancel(request_id)
            .map(|_| ())
            .map_err(|_| wit_broker::ErrorCode::Internal)
    }

    fn close(&mut self, resource_id: u64) -> std::result::Result<(), wit_broker::ErrorCode> {
        if !matches!(
            self.phase,
            Some(CallbackPhase::Input | CallbackPhase::HostEvent)
        ) {
            return Err(wit_broker::ErrorCode::InvalidPhase);
        }
        self.broker
            .as_mut()
            .ok_or(wit_broker::ErrorCode::Unavailable)?
            .close(resource_id)
            .map(|_| ())
            .map_err(|_| wit_broker::ErrorCode::Internal)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostedItem {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputKind {
    Pressed,
    Activated,
    LongPressed,
    ValueChanged,
    Released,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputActivation {
    pub origin: ActivationOrigin,
    pub input_sequence: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputEvent {
    pub item_id: String,
    pub widget_id: u64,
    pub kind: InputKind,
    pub value: Option<f32>,
    /// Physical contact associated with this semantic event. `None` is used
    /// only by headless tooling and non-touch host callbacks.
    pub contact_id: Option<u32>,
    /// Trusted host metadata. This field is never represented in guest WIT.
    pub activation: Option<InputActivation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComponentPresentationPlacement {
    Anchored,
    InPlace,
    Slot(String),
    Region(String),
    FullBar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentPresentationLifecycle {
    Persistent,
    Transient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentPresentationDismissal {
    Requested,
    Selection,
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComponentPresentationCommand {
    Begin {
        placement: ComponentPresentationPlacement,
        lifecycle: ComponentPresentationLifecycle,
    },
    End(ComponentPresentationDismissal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentUpdate {
    pub rerender: bool,
    pub presentation: Option<ComponentPresentationCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ComponentPresentationEvent {
    Anchor { x: f32, width: f32 },
    Started,
    Ended(ComponentPresentationEndReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentPresentationEndReason {
    Requested,
    Selection,
    OutsidePress,
    Timeout,
    SourceHidden,
    Replaced,
    Rejected,
}

pub const BROKER_FD_ENV: &str = "TOUCHBAR_BROKER_FD";
pub const COMPONENT_FD_ENV: &str = "TOUCHBAR_COMPONENT_FD";
pub const MANIFEST_FD_ENV: &str = "TOUCHBAR_MANIFEST_FD";
pub const ASSET_BUNDLE_FD_ENV: &str = "TOUCHBAR_ASSET_BUNDLE_FD";
pub const BROKER_FD: RawFd = 3;
pub const COMPONENT_FD: RawFd = 4;
pub const MANIFEST_FD: RawFd = 5;
pub const ASSET_BUNDLE_FD: RawFd = 6;
pub const MAX_INHERITED_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_INHERITED_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_INHERITED_ASSET_BUNDLE_BYTES: u64 = MAX_ASSET_BUNDLE_BYTES as u64;
const REQUIRED_FILE_SEALS: i32 =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

/// Host-side endpoint for the supervisor-owned broker connection.
///
/// The endpoint and its capability snapshot are never placed in WASI. Future
/// typed WIT imports call through this object from trusted host code.
pub struct BrokerClient {
    channel: Seqpacket,
    next_request_id: u64,
    suppressed_responses: BTreeSet<u64>,
    generation: u64,
    states: Vec<CapabilityState>,
    dropped_events: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrokerEvent {
    Completion {
        request_id: u64,
        result: BrokerResult,
    },
    ResourceEvent {
        resource_id: u64,
        sequence: u64,
        result: BrokerResult,
    },
    CapabilityChanged {
        generation: u64,
        state: CapabilityState,
    },
    Overflow {
        generation: u64,
        dropped_events: u64,
    },
    Shutdown {
        generation: u64,
        reason: BrokerErrorCode,
    },
}

impl BrokerClient {
    pub fn from_environment() -> Result<Option<Self>> {
        let Some(value) = env::var_os(BROKER_FD_ENV) else {
            return Ok(None);
        };
        let descriptor = value
            .to_str()
            .context("broker descriptor environment value is not UTF-8")?
            .parse::<i32>()
            .context("broker descriptor environment value is not an integer")?;
        if descriptor != BROKER_FD {
            bail!("broker descriptor is not in its fixed inherited slot");
        }
        // SAFETY: the supervisor transfers ownership of this inherited
        // descriptor to the child process through BROKER_FD_ENV.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let channel = Seqpacket::try_from_owned_fd(descriptor)
            .context("validate inherited private broker channel")?;
        Self::connect(channel).map(Some)
    }

    pub fn connect(channel: Seqpacket) -> Result<Self> {
        channel
            .send_host(&HostMessage::GetCapabilities { request_id: 1 })
            .context("request initial broker capability snapshot")?;
        let SupervisorMessage::Capabilities {
            request_id: 1,
            generation,
            states,
        } = channel
            .recv_supervisor()
            .context("receive initial broker capability snapshot")?
        else {
            bail!("supervisor did not begin with the requested capability snapshot");
        };
        Ok(Self {
            channel,
            next_request_id: 2,
            suppressed_responses: BTreeSet::new(),
            generation,
            states,
            dropped_events: 0,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn states(&self) -> &[CapabilityState] {
        &self.states
    }

    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    pub fn receive_event(&mut self) -> Result<BrokerEvent> {
        loop {
            let message = self
                .channel
                .recv_supervisor()
                .context("receive broker event")?;
            if let Some(event) = self.decode_event(message)? {
                return Ok(event);
            }
        }
    }

    pub fn try_receive_event(&mut self) -> Result<Option<BrokerEvent>> {
        loop {
            let Some(message) = self
                .channel
                .try_recv_supervisor()
                .context("receive broker event")?
            else {
                return Ok(None);
            };
            if let Some(event) = self.decode_event(message)? {
                return Ok(Some(event));
            }
        }
    }

    fn decode_event(&mut self, message: SupervisorMessage) -> Result<Option<BrokerEvent>> {
        match message {
            SupervisorMessage::Response { request_id, .. }
                if self.suppressed_responses.remove(&request_id) =>
            {
                Ok(None)
            }
            SupervisorMessage::Response { request_id, result } => {
                Ok(Some(BrokerEvent::Completion { request_id, result }))
            }
            SupervisorMessage::ResourceEvent {
                resource_id,
                sequence,
                result,
            } => Ok(Some(BrokerEvent::ResourceEvent {
                resource_id,
                sequence,
                result,
            })),
            SupervisorMessage::CapabilityChanged { generation, state } => {
                self.generation = generation;
                if let Some(existing) = self
                    .states
                    .iter_mut()
                    .find(|existing| existing.capability == state.capability)
                {
                    *existing = state.clone();
                } else {
                    self.states.push(state.clone());
                }
                Ok(Some(BrokerEvent::CapabilityChanged { generation, state }))
            }
            SupervisorMessage::Overflow {
                generation,
                dropped_events,
            } => {
                self.generation = generation;
                self.dropped_events = self.dropped_events.saturating_add(dropped_events);
                Ok(Some(BrokerEvent::Overflow {
                    generation,
                    dropped_events,
                }))
            }
            SupervisorMessage::Shutdown { generation, reason } => {
                self.generation = generation;
                Ok(Some(BrokerEvent::Shutdown { generation, reason }))
            }
            SupervisorMessage::Capabilities { .. } => {
                bail!("received a broker response where an event was required")
            }
        }
    }

    pub fn submit(
        &mut self,
        phase: CallbackPhase,
        capability: impl Into<String>,
        operation: impl Into<String>,
        payload: Vec<u8>,
        activation: Option<ActivationContext>,
    ) -> Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("broker request identifier exhausted")?;
        self.channel
            .send_host(&HostMessage::Request {
                request_id,
                phase,
                capability: capability.into(),
                operation: operation.into(),
                payload,
                activation,
            })
            .context("send broker request")?;
        Ok(request_id)
    }

    pub fn cancel(&mut self, target_request_id: u64) -> Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("broker request identifier exhausted")?;
        self.channel
            .send_host(&HostMessage::Cancel {
                request_id,
                target_request_id,
            })
            .context("send broker cancellation")?;
        self.suppressed_responses.insert(request_id);
        Ok(request_id)
    }

    pub fn close(&mut self, resource_id: u64) -> Result<u64> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("broker request identifier exhausted")?;
        self.channel
            .send_host(&HostMessage::Close {
                request_id,
                resource_id,
            })
            .context("send broker resource close")?;
        self.suppressed_responses.insert(request_id);
        Ok(request_id)
    }
}

/// Consumes and reads one supervisor-created sealed memory file. A normal
/// pathname-backed descriptor or writable memfd is rejected so environment
/// variables cannot substitute unverified package bytes.
pub fn read_supervisor_file(environment: &str, maximum_bytes: u64) -> Result<Option<Vec<u8>>> {
    let Some(value) = env::var_os(environment) else {
        return Ok(None);
    };
    let descriptor = value
        .to_str()
        .context("inherited descriptor environment value is not UTF-8")?
        .parse::<i32>()
        .context("inherited descriptor environment value is not an integer")?;
    let expected = match environment {
        COMPONENT_FD_ENV => COMPONENT_FD,
        MANIFEST_FD_ENV => MANIFEST_FD,
        ASSET_BUNDLE_FD_ENV => ASSET_BUNDLE_FD,
        _ => bail!("unknown inherited package descriptor variable"),
    };
    if descriptor != expected {
        bail!("package descriptor is not in its fixed inherited slot");
    }
    // SAFETY: the supervisor transfers ownership of this descriptor to the
    // component host and names it through a fixed environment variable.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata has sufficient writable storage and descriptor is live.
    if unsafe { libc::fstat(descriptor.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(anyhow!(
            "inspect inherited package descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fstat succeeded.
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG || metadata.st_size < 0 {
        bail!("inherited package descriptor is not a regular file");
    }
    let length = metadata.st_size as u64;
    if length > maximum_bytes {
        bail!("inherited package descriptor exceeds its size limit");
    }
    // SAFETY: F_GET_SEALS queries integer state on a live descriptor.
    let seals = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & REQUIRED_FILE_SEALS != REQUIRED_FILE_SEALS {
        bail!("inherited package descriptor is not supervisor-sealed");
    }
    let length = usize::try_from(length).context("inherited package size overflow")?;
    let file = std::fs::File::from(descriptor);
    let mut bytes = vec![0; length];
    let mut offset = 0usize;
    while offset < bytes.len() {
        let read = file
            .read_at(&mut bytes[offset..], offset as u64)
            .context("read inherited package descriptor")?;
        if read == 0 {
            bail!("inherited package descriptor was truncated");
        }
        offset += read;
    }
    Ok(Some(bytes))
}

pub struct PluginHost {
    store: Store<HostState>,
    bindings: Plugin,
    limits: HostLimits,
    animations: AnimationRegistry,
    effects: EffectRegistry,
    guest_render_calls: u64,
    assets: PackageAssets,
}

#[derive(Clone, Debug, Default)]
pub struct PackageAssets {
    images: BTreeMap<String, Image>,
}

impl PackageAssets {
    pub fn decode(definitions: &[AssetDefinition], bundle: &[u8]) -> Result<Self> {
        let bundled = decode_asset_bundle(bundle)?;
        decode_asset_payloads(definitions, &bundled)
    }

    pub fn from_directory(root: &Path, definitions: &[AssetDefinition]) -> Result<Self> {
        let mut payloads = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let joined = root.join(&definition.path);
            let canonical = joined
                .canonicalize()
                .with_context(|| format!("resolve asset {}", definition.id))?;
            if !canonical.starts_with(root) {
                bail!("asset {} resolves outside its package", definition.id);
            }
            if canonical != joined {
                bail!("asset {} path contains a symlink", definition.id);
            }
            let metadata = std::fs::symlink_metadata(&joined)
                .with_context(|| format!("inspect asset {}", definition.id))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if !metadata.is_file() || metadata.nlink() != 1 {
                    bail!("asset {} must be a single-link regular file", definition.id);
                }
            }
            #[cfg(not(unix))]
            if !metadata.is_file() {
                bail!("asset {} must be a regular file", definition.id);
            }
            let bytes = std::fs::read(&canonical)
                .with_context(|| format!("read asset {}", definition.id))?;
            payloads.push((definition.id.as_str(), bytes));
        }
        let bundle =
            encode_asset_bundle(payloads.iter().map(|(id, bytes)| (*id, bytes.as_slice())))?;
        Self::decode(definitions, &bundle)
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    fn get(&self, id: &str) -> Option<&Image> {
        self.images.get(id)
    }
}

fn decode_asset_payloads(
    definitions: &[AssetDefinition],
    payloads: &[BundledAsset],
) -> Result<PackageAssets> {
    if definitions.len() != payloads.len() {
        bail!("asset bundle does not exactly match the sealed manifest");
    }
    let mut images = BTreeMap::new();
    let mut total_pixels = 0_u64;
    let mut rasterizer = SvgRasterizer::new(definitions.len().max(1));
    for (index, (definition, payload)) in definitions.iter().zip(payloads).enumerate() {
        if definition.id != payload.id {
            bail!("asset bundle order and IDs do not exactly match the sealed manifest");
        }
        let pixels = u64::from(definition.width) * u64::from(definition.height);
        total_pixels = total_pixels
            .checked_add(pixels)
            .context("decoded asset pixel budget overflow")?;
        if total_pixels > touchbar_package::MAX_ASSET_PIXELS {
            bail!("decoded assets exceed the package pixel budget");
        }
        let image_id = 0x4153_5345_5400_0000_u64
            .checked_add(index as u64)
            .context("asset image identifier overflow")?;
        let image = match definition.kind {
            AssetKind::Png => decode_png(definition, &payload.bytes, image_id)?,
            AssetKind::SymbolicSvg => {
                validate_symbolic_svg(&payload.bytes)?;
                let source = std::str::from_utf8(&payload.bytes)
                    .context("symbolic SVG asset is not UTF-8")?;
                let asset = SvgAsset::new(image_id, 1, source)?;
                rasterizer.rasterize(&asset, definition.width, definition.height)?
            }
        };
        if images.insert(definition.id.clone(), image).is_some() {
            bail!("asset bundle contains a duplicate manifest ID");
        }
    }
    Ok(PackageAssets { images })
}

fn decode_png(definition: &AssetDefinition, bytes: &[u8], image_id: u64) -> Result<Image> {
    let limits = png::Limits {
        bytes: (definition.width as usize)
            .saturating_mul(definition.height as usize)
            .saturating_mul(8)
            .saturating_add(64 * 1024),
    };
    let mut decoder = png::Decoder::new_with_limits(BufReader::new(Cursor::new(bytes)), limits);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("read PNG asset header")?;
    let header = reader.info();
    if header.width != definition.width || header.height != definition.height {
        bail!(
            "PNG asset `{}` dimensions are {}x{}, expected {}x{}",
            definition.id,
            header.width,
            header.height,
            definition.width,
            definition.height
        );
    }
    let maximum_output = definition.width as usize * definition.height as usize * 4;
    let output_size = reader
        .output_buffer_size()
        .context("PNG asset output size overflow")?;
    if output_size > maximum_output {
        bail!("PNG asset output exceeds its declared decoded size");
    }
    let mut decoded = vec![0; output_size];
    let info = reader
        .next_frame(&mut decoded)
        .context("decode PNG asset")?;
    if info.width != definition.width || info.height != definition.height {
        bail!("PNG asset frame dimensions changed during decode");
    }
    let source = &decoded[..info.buffer_size()];
    let mut rgba = Vec::with_capacity(maximum_output);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(source),
        png::ColorType::Rgb => {
            for pixel in source.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xff]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for pixel in source.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for value in source {
                rgba.extend_from_slice(&[*value, *value, *value, 0xff]);
            }
        }
        png::ColorType::Indexed => bail!("PNG palette was not expanded by the decoder"),
    }
    Image::rgba8(image_id, 1, definition.width, definition.height, rgba)
}

fn validate_symbolic_svg(bytes: &[u8]) -> Result<()> {
    let source = std::str::from_utf8(bytes).context("symbolic SVG asset is not UTF-8")?;
    let lower = source.to_ascii_lowercase();
    for forbidden in [
        "<image",
        "<script",
        "<foreignobject",
        "href=",
        "url(",
        "@import",
        "<!doctype",
        "<!entity",
    ] {
        if lower.contains(forbidden) {
            bail!("symbolic SVG contains forbidden external or active content");
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct AnimationSignature {
    from: VisualTransform,
    to: VisualTransform,
    duration: Duration,
    easing: Easing,
    playback: MotionPlayback,
}

#[derive(Clone, Copy, Debug)]
struct AnimationState {
    signature: AnimationSignature,
    started: Duration,
}

#[derive(Clone, Debug, Default)]
struct AnimationRegistry {
    entries: BTreeMap<(String, u64), AnimationState>,
}

#[derive(Clone, Debug, PartialEq)]
struct EffectSignature {
    source: String,
    period: Option<Duration>,
}

#[derive(Clone, Debug)]
struct EffectState {
    signature: EffectSignature,
    started: Duration,
}

#[derive(Clone, Debug, Default)]
struct EffectRegistry {
    programs: BTreeMap<String, Arc<EffectProgram>>,
    entries: BTreeMap<(String, u64), EffectState>,
}

impl EffectRegistry {
    fn bind(
        &mut self,
        item_id: &str,
        effect_id: u64,
        source: &str,
        period: Option<Duration>,
        now: Duration,
        max_programs: usize,
    ) -> Result<(Arc<EffectProgram>, Duration)> {
        let signature = EffectSignature {
            source: source.to_owned(),
            period,
        };
        let key = (item_id.to_owned(), effect_id);
        let started = self
            .entries
            .get(&key)
            .filter(|state| state.signature == signature)
            .map_or(now, |state| state.started);
        let program = if let Some(program) = self.programs.get(source) {
            program.clone()
        } else {
            if self.programs.len() >= max_programs {
                bail!("component exceeds its lifetime shader-effect program budget");
            }
            let program = Arc::new(EffectProgram::compile(source)?);
            self.programs.insert(source.to_owned(), program.clone());
            program
        };
        self.entries.insert(key, EffectState { signature, started });
        Ok((program, started))
    }

    fn retain_item(&mut self, item_id: &str, present: &BTreeSet<u64>) {
        self.entries
            .retain(|(item, effect), _| item != item_id || present.contains(effect));
    }
}

impl AnimationRegistry {
    fn bind(
        &mut self,
        item_id: &str,
        animation_id: u64,
        signature: AnimationSignature,
        now: Duration,
    ) -> Duration {
        let key = (item_id.to_owned(), animation_id);
        if let Some(state) = self.entries.get(&key)
            && state.signature == signature
        {
            return state.started;
        }
        self.entries.insert(
            key,
            AnimationState {
                signature,
                started: now,
            },
        );
        now
    }

    fn retain_item(&mut self, item_id: &str, present: &BTreeSet<u64>) {
        self.entries
            .retain(|(item, animation), _| item != item_id || present.contains(animation));
    }
}

impl PluginHost {
    pub fn from_file(path: impl AsRef<Path>, limits: HostLimits) -> Result<Self> {
        Self::from_file_with_broker(path, limits, None)
    }

    pub fn from_file_with_broker(
        path: impl AsRef<Path>,
        limits: HostLimits,
        broker: Option<BrokerClient>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let (engine, mut store) = create_store(limits, broker)?;
        let component = Component::from_file(&engine, path)
            .with_context(|| format!("compile component {}", path.display()))?;
        let linker = create_linker(&engine)?;
        let bindings = Plugin::instantiate(&mut store, &component, &linker)
            .context("instantiate component without ambient capabilities")?;
        Ok(Self {
            store,
            bindings,
            limits,
            animations: AnimationRegistry::default(),
            effects: EffectRegistry::default(),
            guest_render_calls: 0,
            assets: PackageAssets::default(),
        })
    }

    pub fn from_bytes(bytes: &[u8], limits: HostLimits) -> Result<Self> {
        Self::from_bytes_with_broker(bytes, limits, None)
    }

    pub fn from_bytes_with_broker(
        bytes: &[u8],
        limits: HostLimits,
        broker: Option<BrokerClient>,
    ) -> Result<Self> {
        let (engine, mut store) = create_store(limits, broker)?;
        let component = Component::new(&engine, bytes).context("compile component")?;
        let linker = create_linker(&engine)?;
        let bindings = Plugin::instantiate(&mut store, &component, &linker)
            .context("instantiate component without ambient capabilities")?;
        Ok(Self {
            store,
            bindings,
            limits,
            animations: AnimationRegistry::default(),
            effects: EffectRegistry::default(),
            guest_render_calls: 0,
            assets: PackageAssets::default(),
        })
    }

    pub fn set_assets(&mut self, assets: PackageAssets) {
        self.assets = assets;
    }

    pub fn items(&mut self) -> Result<Vec<HostedItem>> {
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::Items);
        let result = self
            .bindings
            .call_items(&mut self.store)
            .context("component items call failed");
        self.store.data_mut().phase = None;
        let items = result?;
        validate_items(items, self.limits)
    }

    pub fn render(
        &mut self,
        item_id: &str,
        viewport: Rect,
        appearance: Appearance,
    ) -> Result<RetainedUi> {
        self.render_at(item_id, viewport, appearance, Duration::ZERO)
    }

    pub fn render_at(
        &mut self,
        item_id: &str,
        viewport: Rect,
        appearance: Appearance,
        now: Duration,
    ) -> Result<RetainedUi> {
        validate_viewport(viewport)?;
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::Render);
        let request = wit::RenderRequest {
            item_id: item_id.into(),
            viewport: wit::Viewport {
                width: viewport.width,
                height: viewport.height,
                scale: 1.0,
            },
            theme: to_wit_theme(appearance),
        };
        self.guest_render_calls = self.guest_render_calls.saturating_add(1);
        let result = self
            .bindings
            .call_render(&mut self.store, &request)
            .context("component render call failed");
        self.store.data_mut().phase = None;
        let view = result?.map_err(|message| anyhow!("component rejected render: {message}"))?;
        let mut animations = self.animations.clone();
        let mut effects = self.effects.clone();
        let root = ViewDecoder::new_with_assets(
            &view.nodes,
            self.limits,
            item_id,
            now,
            &mut animations,
            &mut effects,
            &self.assets,
        )
        .decode(view.root)?;
        self.animations = animations;
        self.effects = effects;
        Ok(RetainedUi::new(root))
    }

    pub fn guest_render_calls(&self) -> u64 {
        self.guest_render_calls
    }

    pub fn handle_event(&mut self, event: &InputEvent) -> Result<ComponentUpdate> {
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::Input);
        let activation = activation_for_input(
            self.store.data().surface_instance,
            event,
            self.limits.activation_lifetime,
        )?;
        self.store.data_mut().activation = activation;
        let event = wit::InputEvent {
            item_id: event.item_id.clone(),
            widget_id: event.widget_id,
            kind: match event.kind {
                InputKind::Pressed => wit::InputKind::Pressed,
                InputKind::Activated => wit::InputKind::Activated,
                InputKind::LongPressed => wit::InputKind::LongPressed,
                InputKind::ValueChanged => wit::InputKind::ValueChanged,
                InputKind::Released => wit::InputKind::Released,
                InputKind::Cancelled => wit::InputKind::Cancelled,
            },
            value: event.value,
            contact_id: event.contact_id,
        };
        let result = self
            .bindings
            .call_handle_event(&mut self.store, &event)
            .context("component event call failed");
        self.store.data_mut().phase = None;
        self.store.data_mut().activation = None;
        let update = result?.map_err(|message| anyhow!("component rejected event: {message}"))?;
        from_wit_update(update)
    }

    pub fn handle_presentation_event(
        &mut self,
        event: ComponentPresentationEvent,
    ) -> Result<ComponentUpdate> {
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::HostEvent);
        let event = match event {
            ComponentPresentationEvent::Anchor { x, width } => {
                wit::PresentationEvent::Anchor(wit::PresentationAnchor { x, width })
            }
            ComponentPresentationEvent::Started => wit::PresentationEvent::Started,
            ComponentPresentationEvent::Ended(reason) => {
                wit::PresentationEvent::Ended(match reason {
                    ComponentPresentationEndReason::Requested => {
                        wit::PresentationEndReason::Requested
                    }
                    ComponentPresentationEndReason::Selection => {
                        wit::PresentationEndReason::Selection
                    }
                    ComponentPresentationEndReason::OutsidePress => {
                        wit::PresentationEndReason::OutsidePress
                    }
                    ComponentPresentationEndReason::Timeout => wit::PresentationEndReason::Timeout,
                    ComponentPresentationEndReason::SourceHidden => {
                        wit::PresentationEndReason::SourceHidden
                    }
                    ComponentPresentationEndReason::Replaced => {
                        wit::PresentationEndReason::Replaced
                    }
                    ComponentPresentationEndReason::Rejected => {
                        wit::PresentationEndReason::Rejected
                    }
                })
            }
        };
        let result = self
            .bindings
            .call_handle_presentation_event(&mut self.store, event)
            .context("component presentation-event call failed");
        self.store.data_mut().phase = None;
        let update = result?
            .map_err(|message| anyhow!("component rejected presentation event: {message}"))?;
        let update = from_wit_update(update)?;
        if update.presentation.is_some() {
            bail!("component requested a presentation from a presentation callback");
        }
        Ok(update)
    }

    pub fn broker_generation(&self) -> Option<u64> {
        self.store
            .data()
            .broker
            .as_ref()
            .map(BrokerClient::generation)
    }

    pub fn broker_states(&self) -> Option<&[CapabilityState]> {
        self.store.data().broker.as_ref().map(BrokerClient::states)
    }

    /// Descriptor used only by the trusted native event loop. It is never
    /// inserted into WASI or exposed as a guest resource.
    pub fn broker_event_fd(&self) -> Option<RawFd> {
        self.store
            .data()
            .broker
            .as_ref()
            .map(|broker| broker.channel.as_raw_fd())
    }

    /// Delivers every broker event currently queued to an event-aware guest.
    /// A bounded batch preserves fairness with Wayland input and rendering.
    pub fn dispatch_broker_events(&mut self) -> Result<bool> {
        let mut rerender = false;
        for _ in 0..64 {
            let event = {
                let state = self.store.data_mut();
                let Some(broker) = state.broker.as_mut() else {
                    bail!("component broker is unavailable");
                };
                broker.try_receive_event()?
            };
            let Some(event) = event else {
                break;
            };
            self.refuel()?;
            self.store.data_mut().phase = Some(CallbackPhase::HostEvent);
            let event = to_wit_broker_event(event);
            let result = self
                .bindings
                .call_handle_host_event(&mut self.store, &event)
                .context("component host-event call failed");
            self.store.data_mut().phase = None;
            let update =
                result?.map_err(|message| anyhow!("component rejected host event: {message}"))?;
            if update.presentation.is_some() {
                bail!("component requested a presentation outside a physical input callback");
            }
            rerender |= update.rerender;
        }
        Ok(rerender)
    }

    fn refuel(&mut self) -> Result<()> {
        Ok(self
            .store
            .set_fuel(self.limits.fuel_per_call)
            .context("reset component instruction budget")?)
    }
}

fn from_wit_update(update: wit::Update) -> Result<ComponentUpdate> {
    let presentation = update
        .presentation
        .map(|command| -> Result<ComponentPresentationCommand> {
            match command {
                wit::PresentationCommand::Begin(begin) => {
                    let target = begin.target.unwrap_or_default();
                    let placement = match begin.placement {
                        wit::PresentationPlacement::Anchored => {
                            require_empty_presentation_target(&target, "anchored")?;
                            ComponentPresentationPlacement::Anchored
                        }
                        wit::PresentationPlacement::InPlace => {
                            require_empty_presentation_target(&target, "in-place")?;
                            ComponentPresentationPlacement::InPlace
                        }
                        wit::PresentationPlacement::Slot => {
                            require_presentation_target(target, "slot")?
                        }
                        wit::PresentationPlacement::Region => {
                            let target = require_nonempty_presentation_target(target, "region")?;
                            ComponentPresentationPlacement::Region(target)
                        }
                        wit::PresentationPlacement::FullBar => {
                            require_empty_presentation_target(&target, "full-bar")?;
                            ComponentPresentationPlacement::FullBar
                        }
                    };
                    Ok(ComponentPresentationCommand::Begin {
                        placement,
                        lifecycle: match begin.lifecycle {
                            wit::PresentationLifecycle::Persistent => {
                                ComponentPresentationLifecycle::Persistent
                            }
                            wit::PresentationLifecycle::Transient => {
                                ComponentPresentationLifecycle::Transient
                            }
                        },
                    })
                }
                wit::PresentationCommand::End(reason) => {
                    Ok(ComponentPresentationCommand::End(match reason {
                        wit::PresentationDismissal::Requested => {
                            ComponentPresentationDismissal::Requested
                        }
                        wit::PresentationDismissal::Selection => {
                            ComponentPresentationDismissal::Selection
                        }
                        wit::PresentationDismissal::Timeout => {
                            ComponentPresentationDismissal::Timeout
                        }
                    }))
                }
            }
        })
        .transpose()?;
    Ok(ComponentUpdate {
        rerender: update.rerender,
        presentation,
    })
}

fn require_empty_presentation_target(target: &str, placement: &str) -> Result<()> {
    if !target.is_empty() {
        bail!("{placement} presentation must not specify a target");
    }
    Ok(())
}

fn require_nonempty_presentation_target(target: String, placement: &str) -> Result<String> {
    if target.is_empty() {
        bail!("{placement} presentation requires a target");
    }
    Ok(target)
}

fn require_presentation_target(
    target: String,
    placement: &str,
) -> Result<ComponentPresentationPlacement> {
    require_nonempty_presentation_target(target, placement)
        .map(ComponentPresentationPlacement::Slot)
}

fn activation_for_input(
    surface_instance: u64,
    event: &InputEvent,
    lifetime: Duration,
) -> Result<Option<ActivationContext>> {
    Ok(match (event.kind, event.activation) {
        (
            InputKind::Activated,
            Some(
                activation @ InputActivation {
                    origin: ActivationOrigin::Physical | ActivationOrigin::TrustedControl,
                    ..
                },
            ),
        ) => {
            let now = monotonic_micros()?;
            let lifetime = u64::try_from(lifetime.as_micros()).unwrap_or(u64::MAX);
            Some(ActivationContext {
                origin: activation.origin,
                surface_instance,
                item_id: event.item_id.clone(),
                widget_id: event.widget_id,
                input_sequence: activation.input_sequence,
                deadline_monotonic_micros: now.saturating_add(lifetime),
            })
        }
        _ => None,
    })
}

fn create_store(
    limits: HostLimits,
    broker: Option<BrokerClient>,
) -> Result<(Engine, Store<HostState>)> {
    let mut config = Config::new();
    // The workspace deliberately omits Wasmtime's `parallel-compilation`
    // feature so compilation remains compatible with the post-confinement
    // ban on clone/fork.
    config.wasm_component_model(true).consume_fuel(true);
    let engine = Engine::new(&config).context("create Wasmtime engine")?;
    let store_limits = StoreLimitsBuilder::new()
        .memory_size(limits.memory_bytes)
        .table_elements(limits.table_elements)
        .instances(limits.instances)
        .memories(limits.memories)
        .tables(limits.tables)
        .build();
    // The default context has closed stdio, no args, no environment, no
    // preopened filesystem paths, and denies all socket addresses. Clocks,
    // random, and poll provide ordinary Rust `std` plumbing without ambient
    // user-session access.
    let wasi = WasiCtx::builder().build();
    let surface_instance = NEXT_SURFACE_INSTANCE.fetch_add(1, Ordering::Relaxed);
    if surface_instance == 0 {
        bail!("component surface instance identifier exhausted");
    }
    let mut store = Store::new(
        &engine,
        HostState {
            store_limits,
            wasi,
            resources: ResourceTable::new(),
            broker,
            phase: None,
            surface_instance,
            activation: None,
        },
    );
    store.limiter(|state| &mut state.store_limits);
    store
        .set_fuel(limits.fuel_per_call)
        .context("set initial component instruction budget")?;
    Ok((engine, store))
}

fn create_linker(engine: &Engine) -> Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
        .context("link policy-brokered component imports")?;
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .context("link capability-free WASI runtime plumbing")?;
    Ok(linker)
}

fn to_wit_capability_state(state: &CapabilityState) -> wit_broker::CapabilityState {
    wit_broker::CapabilityState {
        capability: state.capability.clone(),
        required: state.required,
        status: match state.status {
            WireCapabilityStatus::Granted => wit_broker::CapabilityStatus::Granted,
            WireCapabilityStatus::Denied => wit_broker::CapabilityStatus::Denied,
            WireCapabilityStatus::NeedsConsent => wit_broker::CapabilityStatus::NeedsConsent,
            WireCapabilityStatus::Unsupported => wit_broker::CapabilityStatus::Unsupported,
            WireCapabilityStatus::DisclosureOnly => wit_broker::CapabilityStatus::DisclosureOnly,
        },
    }
}

fn to_wit_error(error: BrokerErrorCode) -> wit_broker::ErrorCode {
    match error {
        BrokerErrorCode::Unavailable => wit_broker::ErrorCode::Unavailable,
        BrokerErrorCode::Denied => wit_broker::ErrorCode::Denied,
        BrokerErrorCode::OutOfScope => wit_broker::ErrorCode::OutOfScope,
        BrokerErrorCode::InvalidRequest => wit_broker::ErrorCode::InvalidRequest,
        BrokerErrorCode::InvalidPhase => wit_broker::ErrorCode::InvalidPhase,
        BrokerErrorCode::ActivationRequired => wit_broker::ErrorCode::ActivationRequired,
        BrokerErrorCode::QuotaExceeded => wit_broker::ErrorCode::QuotaExceeded,
        BrokerErrorCode::RateLimited => wit_broker::ErrorCode::RateLimited,
        BrokerErrorCode::Timeout => wit_broker::ErrorCode::Timeout,
        BrokerErrorCode::Cancelled => wit_broker::ErrorCode::Cancelled,
        BrokerErrorCode::Unsupported => wit_broker::ErrorCode::Unsupported,
        BrokerErrorCode::BackendFailed => wit_broker::ErrorCode::BackendFailed,
        BrokerErrorCode::Internal => wit_broker::ErrorCode::Internal,
    }
}

fn to_wit_result(result: BrokerResult) -> wit_broker::OperationResult {
    match result {
        BrokerResult::Success { payload } => wit_broker::OperationResult::Success(payload),
        BrokerResult::Error(error) => wit_broker::OperationResult::Error(to_wit_error(error)),
    }
}

fn to_wit_broker_event(event: BrokerEvent) -> wit_broker::HostEvent {
    match event {
        BrokerEvent::Completion { request_id, result } => {
            wit_broker::HostEvent::Completion((request_id, to_wit_result(result)))
        }
        BrokerEvent::ResourceEvent {
            resource_id,
            sequence,
            result,
        } => wit_broker::HostEvent::ResourceEvent((resource_id, sequence, to_wit_result(result))),
        BrokerEvent::CapabilityChanged { generation, state } => {
            wit_broker::HostEvent::CapabilityChanged((generation, to_wit_capability_state(&state)))
        }
        BrokerEvent::Overflow {
            generation,
            dropped_events,
        } => wit_broker::HostEvent::Overflow((generation, dropped_events)),
        BrokerEvent::Shutdown { generation, reason } => {
            wit_broker::HostEvent::Shutdown((generation, to_wit_error(reason)))
        }
    }
}

fn validate_items(items: Vec<wit::Item>, limits: HostLimits) -> Result<Vec<HostedItem>> {
    if items.len() > limits.max_items {
        bail!(
            "component returned {} items; limit is {}",
            items.len(),
            limits.max_items
        );
    }
    let mut ids = BTreeSet::new();
    let mut text_bytes = 0usize;
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        validate_id("item id", &item.id)?;
        if !ids.insert(item.id.clone()) {
            bail!("component returned duplicate item id {}", item.id);
        }
        if item.label.trim().is_empty() {
            bail!("item {} has an empty label", item.id);
        }
        text_bytes = text_bytes
            .checked_add(item.id.len() + item.label.len())
            .ok_or_else(|| anyhow!("item text budget overflow"))?;
        if text_bytes > limits.max_text_bytes {
            bail!("component item metadata exceeds text budget");
        }
        result.push(HostedItem {
            id: item.id,
            label: item.label,
        });
    }
    Ok(result)
}

fn validate_id(label: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.as_bytes()[0].is_ascii_lowercase()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        bail!("{label} must be a lowercase kebab-case identifier");
    }
    Ok(())
}

fn validate_viewport(viewport: Rect) -> Result<()> {
    if viewport.x != 0.0
        || viewport.y != 0.0
        || !viewport.width.is_finite()
        || !viewport.height.is_finite()
        || viewport.width <= 0.0
        || viewport.height <= 0.0
        || viewport.width > MAX_TOUCHBAR_WIDTH as f32
        || viewport.height > 60.0
    {
        bail!("component viewport must be a local rectangle within {MAX_TOUCHBAR_WIDTH}x60");
    }
    Ok(())
}

fn to_wit_theme(appearance: Appearance) -> wit::Theme {
    let theme = appearance.theme;
    wit::Theme {
        revision: appearance.revision,
        scheme: match appearance.scheme {
            ColorScheme::Dark => wit::ColorScheme::Dark,
            ColorScheme::Light => wit::ColorScheme::Light,
        },
        motion: match appearance.motion {
            MotionPolicy::Full => wit::MotionPolicy::Full,
            MotionPolicy::Reduced => wit::MotionPolicy::Reduced,
            MotionPolicy::Disabled => wit::MotionPolicy::Disabled,
        },
        background: to_wit_color(theme.background),
        control: to_wit_color(theme.control),
        control_pressed: to_wit_color(theme.control_pressed),
        accent: to_wit_color(theme.accent),
        track: to_wit_color(theme.track),
        foreground: to_wit_color(theme.foreground),
        muted: to_wit_color(theme.muted),
        destructive: to_wit_color(theme.destructive),
        corner_radius: theme.corner_radius,
    }
}

fn to_wit_color(color: Color) -> wit::Rgba {
    wit::Rgba {
        red: color.red,
        green: color.green,
        blue: color.blue,
        alpha: color.alpha,
    }
}

struct ViewDecoder<'a> {
    nodes: &'a [wit::Node],
    limits: HostLimits,
    item_id: &'a str,
    now: Duration,
    animations: &'a mut AnimationRegistry,
    effects: &'a mut EffectRegistry,
    animation_signatures: BTreeMap<u64, AnimationSignature>,
    effect_signatures: BTreeMap<u64, EffectSignature>,
    visiting: BTreeSet<u32>,
    expanded_nodes: usize,
    text_bytes: usize,
    canvas_commands: usize,
    canvas_points: usize,
    canvas_path_segments: usize,
    canvas_fill_triangles: usize,
    effect_nodes: usize,
    assets: Option<&'a PackageAssets>,
}

impl<'a> ViewDecoder<'a> {
    fn new(
        nodes: &'a [wit::Node],
        limits: HostLimits,
        item_id: &'a str,
        now: Duration,
        animations: &'a mut AnimationRegistry,
        effects: &'a mut EffectRegistry,
    ) -> Self {
        Self {
            nodes,
            limits,
            item_id,
            now,
            animations,
            effects,
            animation_signatures: BTreeMap::new(),
            effect_signatures: BTreeMap::new(),
            visiting: BTreeSet::new(),
            expanded_nodes: 0,
            text_bytes: 0,
            canvas_commands: 0,
            canvas_points: 0,
            canvas_path_segments: 0,
            canvas_fill_triangles: 0,
            effect_nodes: 0,
            assets: None,
        }
    }

    fn new_with_assets(
        nodes: &'a [wit::Node],
        limits: HostLimits,
        item_id: &'a str,
        now: Duration,
        animations: &'a mut AnimationRegistry,
        effects: &'a mut EffectRegistry,
        assets: &'a PackageAssets,
    ) -> Self {
        let mut decoder = Self::new(nodes, limits, item_id, now, animations, effects);
        decoder.assets = Some(assets);
        decoder
    }

    fn decode(mut self, root: u32) -> Result<UiNode> {
        if self.nodes.is_empty() {
            bail!("component returned an empty node arena");
        }
        if self.nodes.len() > self.limits.max_nodes {
            bail!(
                "component returned {} nodes; limit is {}",
                self.nodes.len(),
                self.limits.max_nodes
            );
        }
        let root = self.node(root, 0)?;
        let animations = self.animation_signatures.keys().copied().collect();
        self.animations.retain_item(self.item_id, &animations);
        let effects = self.effect_signatures.keys().copied().collect();
        self.effects.retain_item(self.item_id, &effects);
        Ok(root)
    }

    fn node(&mut self, index: u32, depth: usize) -> Result<UiNode> {
        if depth > self.limits.max_tree_depth {
            bail!("component view exceeds maximum tree depth");
        }
        self.expanded_nodes += 1;
        if self.expanded_nodes > self.limits.max_expanded_nodes {
            bail!("component view exceeds expanded node budget");
        }
        let node = self
            .nodes
            .get(index as usize)
            .ok_or_else(|| anyhow!("component view references missing node {index}"))?;
        if !self.visiting.insert(index) {
            bail!("component view contains a cycle at node {index}");
        }

        let decoded = match node {
            wit::Node::Empty => UiNode::Empty,
            wit::Node::Layer(children) => UiNode::Layer(
                children
                    .iter()
                    .map(|child| self.node(*child, depth + 1))
                    .collect::<Result<Vec<_>>>()?,
            ),
            wit::Node::Label(label) => {
                self.text(&label.text)?;
                finite_range("label size", label.size, 1.0, 64.0)?;
                UiNode::Label {
                    text: label.text.clone(),
                    size: label.size,
                    color: color_role(label.color),
                    align: text_align(label.align),
                    overflow: TextOverflow::Ellipsis,
                    measurement: TextMeasurement::Content,
                }
            }
            wit::Node::Icon(icon_node) => {
                self.text(&icon_node.label)?;
                UiNode::Icon {
                    icon: icon(icon_node.icon),
                    color: color_role(icon_node.color),
                    label: icon_node.label.clone(),
                }
            }
            wit::Node::Image(image) => {
                self.text(&image.asset_id)?;
                self.text(&image.label)?;
                validate_id("asset ID", &image.asset_id)?;
                finite_range("image opacity", image.opacity, 0.0, 1.0)?;
                let asset = self
                    .assets
                    .and_then(|assets| assets.get(&image.asset_id))
                    .with_context(|| {
                        format!("component references unknown asset `{}`", image.asset_id)
                    })?
                    .clone();
                UiNode::Image {
                    image: asset,
                    opacity: image.opacity,
                    fit: match image.fit {
                        wit::ImageFit::Contain => ImageFit::Contain,
                        wit::ImageFit::Cover => ImageFit::Cover,
                        wit::ImageFit::Stretch => ImageFit::Stretch,
                    },
                    tint: match image.tint {
                        wit::ImageTint::None => ImageTint::None,
                        wit::ImageTint::Multiply(role) => ImageTint::Multiply(color_role(role)),
                        wit::ImageTint::Mask(role) => ImageTint::Mask(color_role(role)),
                    },
                    label: image.label.clone(),
                }
            }
            wit::Node::Button(button) => {
                self.text(&button.label)?;
                if button.selected {
                    UiNode::Toggle {
                        id: WidgetId(button.widget_id),
                        label: button.label.clone(),
                        icon: button.icon.map(icon),
                        selected: true,
                        pressed: button.pressed,
                    }
                } else {
                    UiNode::button(
                        WidgetId(button.widget_id),
                        button.label.clone(),
                        button.icon.map(icon),
                        button.pressed,
                    )
                }
            }
            wit::Node::Row(container) => {
                let (gap, padding, alignment, children) = self.container(container, depth)?;
                UiNode::row_aligned(gap, padding, alignment, children)
            }
            wit::Node::Column(container) => {
                let (gap, padding, alignment, children) = self.container(container, depth)?;
                UiNode::column_aligned(gap, padding, alignment, children)
            }
            wit::Node::Panel(panel) => {
                finite_range("panel radius", panel.radius, 0.0, 60.0)?;
                finite_range("panel padding", panel.padding, 0.0, 60.0)?;
                UiNode::Panel {
                    radius: panel.radius,
                    color: color_role(panel.color),
                    padding: panel.padding,
                    child: Box::new(self.node(panel.child, depth + 1)?),
                }
            }
            wit::Node::Pressable(pressable) => {
                self.text(&pressable.label)?;
                finite_range("pressable padding", pressable.padding, 0.0, 60.0)?;
                if let Some(radius) = pressable.corner_radius {
                    finite_range("pressable corner radius", radius, 0.0, 60.0)?;
                }
                if let Some(hold_ms) = pressable.hold_ms
                    && !(100..=10_000).contains(&hold_ms)
                {
                    bail!("pressable hold must be between 100 and 10000 milliseconds");
                }
                UiNode::Pressable {
                    id: WidgetId(pressable.widget_id),
                    label: pressable.label.clone(),
                    pressed: pressable.pressed,
                    hold: pressable
                        .hold_ms
                        .map(|value| Duration::from_millis(u64::from(value))),
                    selected: pressable.selected,
                    style: PressableStyle {
                        background: pressable.background.map(color_role),
                        pressed_background: pressable.pressed_background.map(color_role),
                        corner_radius: pressable.corner_radius,
                        padding: pressable.padding,
                        minimum_size: Size::new(0.0, 0.0),
                        content_foreground: pressable.content_foreground.map(color_role),
                    },
                    child: Box::new(self.node(pressable.child, depth + 1)?),
                }
            }
            wit::Node::Slider(slider) => {
                self.text(&slider.label)?;
                finite_range("slider value", slider.value, 0.0, 1.0)?;
                UiNode::slider(
                    WidgetId(slider.widget_id),
                    slider.label.clone(),
                    slider.value,
                )
            }
            wit::Node::Meter(meter) => {
                self.text(&meter.label)?;
                finite_range("meter value", meter.value, 0.0, 1.0)?;
                if let Some(peak) = meter.peak {
                    finite_range("meter peak", peak, 0.0, 1.0)?;
                }
                UiNode::meter(
                    meter.label.clone(),
                    ContinuousValue::unit(meter.value),
                    meter.peak,
                    MeterStyle::default(),
                )
            }
            wit::Node::Progress(progress) => {
                self.text(&progress.label)?;
                finite_range("progress value", progress.value, 0.0, 1.0)?;
                UiNode::Progress {
                    label: progress.label.clone(),
                    value: ProgressValue::Determinate(progress.value),
                    track: color_role(progress.track),
                    fill: color_role(progress.fill),
                }
            }
            wit::Node::Opacity(opacity) => {
                finite_range("opacity", opacity.opacity, 0.0, 1.0)?;
                UiNode::Opacity {
                    opacity: opacity.opacity,
                    child: Box::new(self.node(opacity.child, depth + 1)?),
                }
            }
            wit::Node::Motion(motion) => {
                if motion.animation_id == 0 {
                    bail!("animation ID must be nonzero");
                }
                if !(1..=60_000).contains(&motion.duration_ms) {
                    bail!("animation duration must be between 1 and 60000 milliseconds");
                }
                let signature = AnimationSignature {
                    from: visual_transform("animation start", &motion.start_transform)?,
                    to: visual_transform("animation end", &motion.end_transform)?,
                    duration: Duration::from_millis(u64::from(motion.duration_ms)),
                    easing: match motion.easing {
                        wit::Easing::Linear => Easing::Linear,
                        wit::Easing::EaseInOut => Easing::EaseInOut,
                    },
                    playback: match motion.playback {
                        wit::AnimationPlayback::Once => MotionPlayback::Once,
                        wit::AnimationPlayback::Loop => MotionPlayback::Loop,
                        wit::AnimationPlayback::Alternate => MotionPlayback::Alternate,
                    },
                };
                if let Some(existing) = self.animation_signatures.get(&motion.animation_id)
                    && *existing != signature
                {
                    bail!("animation ID has conflicting descriptions in one view");
                }
                self.animation_signatures
                    .insert(motion.animation_id, signature);
                if self.animation_signatures.len() > self.limits.max_animations {
                    bail!("component view exceeds animation budget");
                }
                let started =
                    self.animations
                        .bind(self.item_id, motion.animation_id, signature, self.now);
                UiNode::motion(
                    Motion {
                        id: MotionId(motion.animation_id),
                        from: signature.from,
                        to: signature.to,
                        started,
                        duration: signature.duration,
                        easing: signature.easing,
                        playback: signature.playback,
                    },
                    self.node(motion.child, depth + 1)?,
                )
            }
            wit::Node::Canvas(canvas) => self.canvas(canvas)?,
            wit::Node::ShaderEffect(effect) => self.shader_effect(effect)?,
            wit::Node::Responsive(responsive) => {
                let mut variants = Vec::with_capacity(responsive.variants.len());
                for variant in &responsive.variants {
                    finite_range(
                        "responsive minimum width",
                        variant.minimum_width,
                        0.0,
                        MAX_TOUCHBAR_WIDTH as f32,
                    )?;
                    variants.push(ResponsiveVariant::new(
                        representation(variant.representation),
                        variant.minimum_width,
                        self.node(variant.node, depth + 1)?,
                    ));
                }
                UiNode::responsive(WidgetId(responsive.widget_id), variants)
            }
        };
        self.visiting.remove(&index);
        Ok(decoded)
    }

    fn container(
        &mut self,
        container: &wit::ContainerNode,
        depth: usize,
    ) -> Result<(f32, f32, CrossAxisAlignment, Vec<FlexItem>)> {
        finite_range(
            "container gap",
            container.gap,
            0.0,
            MAX_TOUCHBAR_WIDTH as f32,
        )?;
        finite_range("container padding", container.padding, 0.0, 502.0)?;
        let mut children = Vec::with_capacity(container.children.len());
        for child in &container.children {
            validate_flex(&child.layout)?;
            children.push(FlexItem::new(
                Flex {
                    minimum: child.layout.minimum,
                    basis: child.layout.basis,
                    maximum: child.layout.maximum,
                    grow: child.layout.grow,
                    shrink: child.layout.shrink,
                    visibility_priority: child.layout.visibility_priority,
                    required: child.layout.required,
                    intrinsic: child.layout.intrinsic,
                },
                self.node(child.node, depth + 1)?,
            ));
        }
        Ok((
            container.gap,
            container.padding,
            alignment(container.align),
            children,
        ))
    }

    fn text(&mut self, text: &str) -> Result<()> {
        self.text_bytes = self
            .text_bytes
            .checked_add(text.len())
            .ok_or_else(|| anyhow!("component text budget overflow"))?;
        if self.text_bytes > self.limits.max_text_bytes {
            bail!("component view exceeds text budget");
        }
        Ok(())
    }

    fn canvas(&mut self, canvas: &wit::CanvasNode) -> Result<UiNode> {
        self.text(&canvas.label)?;
        finite_range("canvas viewbox width", canvas.viewbox_width, 1.0, 4096.0)?;
        finite_range("canvas viewbox height", canvas.viewbox_height, 1.0, 4096.0)?;
        self.canvas_commands = self
            .canvas_commands
            .checked_add(canvas.commands.len())
            .ok_or_else(|| anyhow!("component canvas command budget overflow"))?;
        if self.canvas_commands > self.limits.max_canvas_commands {
            bail!("component view exceeds canvas command budget");
        }
        let commands = canvas
            .commands
            .iter()
            .map(|command| self.canvas_command(command))
            .collect::<Result<Vec<_>>>()?;
        Ok(UiNode::canvas(
            canvas.label.clone(),
            Size::new(canvas.viewbox_width, canvas.viewbox_height),
            commands,
        ))
    }

    fn shader_effect(&mut self, effect: &wit::ShaderEffectNode) -> Result<UiNode> {
        if effect.effect_id == 0 {
            bail!("shader-effect ID must be nonzero");
        }
        self.text(&effect.label)?;
        finite_range("shader-effect opacity", effect.opacity, 0.0, 1.0)?;
        finite_range(
            "shader-effect preferred width",
            effect.preferred_width,
            1.0,
            4096.0,
        )?;
        finite_range(
            "shader-effect preferred height",
            effect.preferred_height,
            1.0,
            4096.0,
        )?;
        if effect.parameters.len() > MAX_EFFECT_PARAMETERS {
            bail!("shader effect has more than eight scalar parameters");
        }
        let mut parameters = [0.0; MAX_EFFECT_PARAMETERS];
        for (destination, value) in parameters.iter_mut().zip(&effect.parameters) {
            finite_range("shader-effect parameter", *value, -1_000_000.0, 1_000_000.0)?;
            *destination = *value;
        }
        let period = effect
            .animation_period_ms
            .map(|milliseconds| {
                if !(100..=60_000).contains(&milliseconds) {
                    bail!("shader-effect period must be between 100 and 60000 milliseconds");
                }
                Ok(Duration::from_millis(u64::from(milliseconds)))
            })
            .transpose()?;
        self.effect_nodes += 1;
        if self.effect_nodes > self.limits.max_effect_nodes {
            bail!("component view exceeds shader-effect node budget");
        }
        let signature = EffectSignature {
            source: effect.source.clone(),
            period,
        };
        if let Some(existing) = self.effect_signatures.get(&effect.effect_id)
            && *existing != signature
        {
            bail!("shader-effect ID has conflicting descriptions in one view");
        }
        self.effect_signatures.insert(effect.effect_id, signature);
        let (program, started) = self.effects.bind(
            self.item_id,
            effect.effect_id,
            &effect.source,
            period,
            self.now,
            self.limits.max_effect_programs,
        )?;
        Ok(UiNode::shader_effect(
            effect.label.clone(),
            Size::new(effect.preferred_width, effect.preferred_height),
            ShaderEffect {
                program,
                parameters,
                opacity: effect.opacity,
                started,
                period,
            },
        ))
    }

    fn canvas_command(&mut self, command: &wit::CanvasCommand) -> Result<CanvasCommand> {
        Ok(match command {
            wit::CanvasCommand::FillRect(value) => {
                canvas_rect(value.x, value.y, value.width, value.height)?;
                finite_range("canvas corner radius", value.radius, 0.0, 2048.0)?;
                CanvasCommand::FillRect {
                    rect: Rect::new(value.x, value.y, value.width, value.height),
                    radius: value.radius,
                    paint: canvas_paint(&value.paint)?,
                }
            }
            wit::CanvasCommand::Line(value) => CanvasCommand::Line {
                start: canvas_point(&value.start)?,
                end: canvas_point(&value.end)?,
                width: canvas_stroke_width(value.width)?,
                paint: canvas_paint(&value.paint)?,
            },
            wit::CanvasCommand::Polyline(value) => {
                if !(2..=256).contains(&value.points.len()) {
                    bail!("canvas polyline must contain between 2 and 256 points");
                }
                self.canvas_points = self
                    .canvas_points
                    .checked_add(value.points.len())
                    .ok_or_else(|| anyhow!("component canvas point budget overflow"))?;
                if self.canvas_points > self.limits.max_canvas_points {
                    bail!("component view exceeds canvas point budget");
                }
                CanvasCommand::Polyline {
                    points: value
                        .points
                        .iter()
                        .map(canvas_point)
                        .collect::<Result<Vec<_>>>()?
                        .into(),
                    width: canvas_stroke_width(value.width)?,
                    paint: canvas_paint(&value.paint)?,
                }
            }
            wit::CanvasCommand::FillCircle(value) => {
                let center = canvas_point(&value.center)?;
                finite_range("canvas circle radius", value.radius, 0.0, 2048.0)?;
                CanvasCommand::FillCircle {
                    center,
                    radius: value.radius,
                    paint: canvas_paint(&value.paint)?,
                }
            }
            wit::CanvasCommand::Text(value) => {
                canvas_rect(value.x, value.y, value.width, value.height)?;
                finite_range("canvas text size", value.size, 1.0, 256.0)?;
                self.text(&value.text)?;
                CanvasCommand::Text {
                    rect: Rect::new(value.x, value.y, value.width, value.height),
                    text: value.text.clone(),
                    size: value.size,
                    paint: canvas_paint(&value.color)?,
                    align: text_align(value.align),
                }
            }
            wit::CanvasCommand::FillLinearGradientRect(value) => {
                canvas_rect(value.x, value.y, value.width, value.height)?;
                finite_range("canvas gradient corner radius", value.radius, 0.0, 2048.0)?;
                let start = canvas_point(&value.gradient.start)?;
                let end = canvas_point(&value.gradient.end)?;
                if start == end {
                    bail!("canvas linear gradient must have a nonzero axis");
                }
                CanvasCommand::FillLinearGradientRect {
                    rect: Rect::new(value.x, value.y, value.width, value.height),
                    radius: value.radius,
                    gradient: CanvasLinearGradient {
                        start,
                        end,
                        start_color: canvas_paint(&value.gradient.start_color)?,
                        end_color: canvas_paint(&value.gradient.end_color)?,
                    },
                }
            }
            wit::CanvasCommand::StrokePath(value) => self.canvas_path(value)?,
            wit::CanvasCommand::FillPath(value) => self.canvas_fill_path(value)?,
        })
    }

    fn canvas_path(&mut self, path: &wit::CanvasPath) -> Result<CanvasCommand> {
        if !(2..=128).contains(&path.segments.len()) {
            bail!("canvas path must contain between 2 and 128 segments");
        }
        self.canvas_path_segments = self
            .canvas_path_segments
            .checked_add(path.segments.len())
            .ok_or_else(|| anyhow!("component canvas path segment budget overflow"))?;
        if self.canvas_path_segments > self.limits.max_canvas_path_segments {
            bail!("component view exceeds canvas path segment budget");
        }

        let mut subpaths = Vec::<std::sync::Arc<[Point]>>::new();
        let mut points = Vec::<Point>::new();
        let mut current = None;
        let mut start = None;
        let flush = |points: &mut Vec<Point>, subpaths: &mut Vec<std::sync::Arc<[Point]>>| {
            if points.len() >= 2 {
                subpaths.push(std::mem::take(points).into());
            } else {
                points.clear();
            }
        };

        for segment in &path.segments {
            match segment {
                wit::CanvasPathSegment::MoveTo(value) => {
                    flush(&mut points, &mut subpaths);
                    let value = canvas_point(value)?;
                    points.push(value);
                    current = Some(value);
                    start = Some(value);
                }
                wit::CanvasPathSegment::LineTo(value) => {
                    if current.is_none() {
                        bail!("canvas path must begin each subpath with move-to");
                    }
                    let value = canvas_point(value)?;
                    points.push(value);
                    current = Some(value);
                }
                wit::CanvasPathSegment::QuadraticTo(value) => {
                    let origin =
                        current.ok_or_else(|| anyhow!("canvas path must begin with move-to"))?;
                    let control = canvas_point(&value.control)?;
                    let endpoint = canvas_point(&value.endpoint)?;
                    for step in 1..=8 {
                        let amount = step as f32 / 8.0;
                        let inverse = 1.0 - amount;
                        points.push(Point::new(
                            inverse * inverse * origin.x
                                + 2.0 * inverse * amount * control.x
                                + amount * amount * endpoint.x,
                            inverse * inverse * origin.y
                                + 2.0 * inverse * amount * control.y
                                + amount * amount * endpoint.y,
                        ));
                    }
                    current = Some(endpoint);
                }
                wit::CanvasPathSegment::CubicTo(value) => {
                    let origin =
                        current.ok_or_else(|| anyhow!("canvas path must begin with move-to"))?;
                    let control_one = canvas_point(&value.control_one)?;
                    let control_two = canvas_point(&value.control_two)?;
                    let endpoint = canvas_point(&value.endpoint)?;
                    for step in 1..=12 {
                        let amount = step as f32 / 12.0;
                        let inverse = 1.0 - amount;
                        points.push(Point::new(
                            inverse.powi(3) * origin.x
                                + 3.0 * inverse * inverse * amount * control_one.x
                                + 3.0 * inverse * amount * amount * control_two.x
                                + amount.powi(3) * endpoint.x,
                            inverse.powi(3) * origin.y
                                + 3.0 * inverse * inverse * amount * control_one.y
                                + 3.0 * inverse * amount * amount * control_two.y
                                + amount.powi(3) * endpoint.y,
                        ));
                    }
                    current = Some(endpoint);
                }
                wit::CanvasPathSegment::Close => {
                    let first = start
                        .ok_or_else(|| anyhow!("canvas path close requires an open subpath"))?;
                    if current != Some(first) {
                        points.push(first);
                    }
                    flush(&mut points, &mut subpaths);
                    current = None;
                    start = None;
                }
            }
        }
        flush(&mut points, &mut subpaths);
        if subpaths.is_empty() {
            bail!("canvas path contains no drawable subpath");
        }
        let flattened_points = subpaths.iter().map(|path| path.len()).sum::<usize>();
        self.canvas_points = self
            .canvas_points
            .checked_add(flattened_points)
            .ok_or_else(|| anyhow!("component canvas point budget overflow"))?;
        if self.canvas_points > self.limits.max_canvas_points {
            bail!("component view exceeds canvas point budget");
        }
        Ok(CanvasCommand::StrokePath {
            subpaths: subpaths.into(),
            width: canvas_stroke_width(path.width)?,
            paint: canvas_paint(&path.paint)?,
        })
    }

    fn canvas_fill_path(&mut self, path: &wit::CanvasFillPath) -> Result<CanvasCommand> {
        if !(4..=128).contains(&path.segments.len()) {
            bail!("canvas fill path must contain between 4 and 128 segments");
        }
        self.canvas_path_segments = self
            .canvas_path_segments
            .checked_add(path.segments.len())
            .ok_or_else(|| anyhow!("component canvas path segment budget overflow"))?;
        if self.canvas_path_segments > self.limits.max_canvas_path_segments {
            bail!("component view exceeds canvas path segment budget");
        }

        let mut points = Vec::new();
        let mut current = None;
        let mut start = None;
        let mut closed = false;
        for segment in &path.segments {
            if closed {
                bail!("canvas fill path must contain exactly one closed subpath");
            }
            match segment {
                wit::CanvasPathSegment::MoveTo(value) => {
                    if current.is_some() || !points.is_empty() {
                        bail!("canvas fill path must contain exactly one subpath");
                    }
                    let value = canvas_point(value)?;
                    points.push(value);
                    current = Some(value);
                    start = Some(value);
                }
                wit::CanvasPathSegment::LineTo(value) => {
                    if current.is_none() {
                        bail!("canvas fill path must begin with move-to");
                    }
                    let value = canvas_point(value)?;
                    points.push(value);
                    current = Some(value);
                }
                wit::CanvasPathSegment::QuadraticTo(value) => {
                    let origin = current
                        .ok_or_else(|| anyhow!("canvas fill path must begin with move-to"))?;
                    let control = canvas_point(&value.control)?;
                    let endpoint = canvas_point(&value.endpoint)?;
                    for step in 1..=8 {
                        let amount = step as f32 / 8.0;
                        let inverse = 1.0 - amount;
                        points.push(Point::new(
                            inverse * inverse * origin.x
                                + 2.0 * inverse * amount * control.x
                                + amount * amount * endpoint.x,
                            inverse * inverse * origin.y
                                + 2.0 * inverse * amount * control.y
                                + amount * amount * endpoint.y,
                        ));
                    }
                    current = Some(endpoint);
                }
                wit::CanvasPathSegment::CubicTo(value) => {
                    let origin = current
                        .ok_or_else(|| anyhow!("canvas fill path must begin with move-to"))?;
                    let control_one = canvas_point(&value.control_one)?;
                    let control_two = canvas_point(&value.control_two)?;
                    let endpoint = canvas_point(&value.endpoint)?;
                    for step in 1..=12 {
                        let amount = step as f32 / 12.0;
                        let inverse = 1.0 - amount;
                        points.push(Point::new(
                            inverse.powi(3) * origin.x
                                + 3.0 * inverse * inverse * amount * control_one.x
                                + 3.0 * inverse * amount * amount * control_two.x
                                + amount.powi(3) * endpoint.x,
                            inverse.powi(3) * origin.y
                                + 3.0 * inverse * inverse * amount * control_one.y
                                + 3.0 * inverse * amount * amount * control_two.y
                                + amount.powi(3) * endpoint.y,
                        ));
                    }
                    current = Some(endpoint);
                }
                wit::CanvasPathSegment::Close => {
                    if current.is_none() || start.is_none() {
                        bail!("canvas fill path close requires an open subpath");
                    }
                    closed = true;
                    current = None;
                }
            }
        }
        if !closed {
            bail!("canvas fill path must end with close");
        }
        if points.last() == points.first() {
            points.pop();
        }
        simplify_collinear_polygon(&mut points);
        if !(3..=258).contains(&points.len()) {
            bail!("canvas fill path must flatten to between 3 and 258 vertices");
        }
        self.canvas_points = self
            .canvas_points
            .checked_add(points.len())
            .ok_or_else(|| anyhow!("component canvas point budget overflow"))?;
        if self.canvas_points > self.limits.max_canvas_points {
            bail!("component view exceeds canvas point budget");
        }
        validate_simple_polygon(&points)?;
        let triangles = triangulate_simple_polygon(&points)?;
        let triangle_count = triangles.len() / 3;
        if triangle_count > 256 {
            bail!("canvas fill path exceeds its 256-triangle limit");
        }
        self.canvas_fill_triangles = self
            .canvas_fill_triangles
            .checked_add(triangle_count)
            .ok_or_else(|| anyhow!("component canvas fill triangle budget overflow"))?;
        if self.canvas_fill_triangles > self.limits.max_canvas_fill_triangles {
            bail!("component view exceeds canvas fill triangle budget");
        }
        Ok(CanvasCommand::FillPath {
            triangles: triangles.into(),
            paint: canvas_paint(&path.paint)?,
        })
    }
}

fn polygon_cross(a: Point, b: Point, c: Point) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn polygon_area(points: &[Point]) -> f32 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(left, right)| left.x * right.y - right.x * left.y)
        .sum::<f32>()
        * 0.5
}

fn simplify_collinear_polygon(points: &mut Vec<Point>) {
    loop {
        if points.len() <= 3 {
            break;
        }
        let mut removed = false;
        for index in 0..points.len() {
            let previous = points[(index + points.len() - 1) % points.len()];
            let current = points[index];
            let next = points[(index + 1) % points.len()];
            if current == previous
                || current == next
                || polygon_cross(previous, current, next) == 0.0
            {
                points.remove(index);
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }
}

fn validate_simple_polygon(points: &[Point]) -> Result<()> {
    for left in 0..points.len() {
        for right in left + 1..points.len() {
            if points[left] == points[right] {
                bail!("canvas fill path repeats a vertex");
            }
        }
    }
    for left in 0..points.len() {
        let left_next = (left + 1) % points.len();
        for right in left + 1..points.len() {
            let right_next = (right + 1) % points.len();
            if left == right || left_next == right || right_next == left {
                continue;
            }
            if segments_intersect(
                points[left],
                points[left_next],
                points[right],
                points[right_next],
            ) {
                bail!("canvas fill path self-intersects");
            }
        }
    }
    if polygon_area(points).abs() < 0.0001 {
        bail!("canvas fill path has zero area");
    }
    Ok(())
}

fn segments_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let ab_c = polygon_cross(a, b, c);
    let ab_d = polygon_cross(a, b, d);
    let cd_a = polygon_cross(c, d, a);
    let cd_b = polygon_cross(c, d, b);
    (ab_c == 0.0 && point_on_segment(c, a, b))
        || (ab_d == 0.0 && point_on_segment(d, a, b))
        || (cd_a == 0.0 && point_on_segment(a, c, d))
        || (cd_b == 0.0 && point_on_segment(b, c, d))
        || ((ab_c > 0.0) != (ab_d > 0.0) && (cd_a > 0.0) != (cd_b > 0.0))
}

fn point_on_segment(point: Point, start: Point, end: Point) -> bool {
    point.x >= start.x.min(end.x)
        && point.x <= start.x.max(end.x)
        && point.y >= start.y.min(end.y)
        && point.y <= start.y.max(end.y)
}

fn triangulate_simple_polygon(points: &[Point]) -> Result<Vec<Point>> {
    let counter_clockwise = polygon_area(points) > 0.0;
    let mut indices = (0..points.len()).collect::<Vec<_>>();
    let mut triangles = Vec::with_capacity((points.len() - 2) * 3);
    while indices.len() > 3 {
        let mut ear = None;
        for position in 0..indices.len() {
            let previous = indices[(position + indices.len() - 1) % indices.len()];
            let current = indices[position];
            let next = indices[(position + 1) % indices.len()];
            let cross = polygon_cross(points[previous], points[current], points[next]);
            if (counter_clockwise && cross <= 0.0) || (!counter_clockwise && cross >= 0.0) {
                continue;
            }
            if indices.iter().copied().any(|candidate| {
                candidate != previous
                    && candidate != current
                    && candidate != next
                    && point_in_triangle(
                        points[candidate],
                        points[previous],
                        points[current],
                        points[next],
                        counter_clockwise,
                    )
            }) {
                continue;
            }
            ear = Some((position, previous, current, next));
            break;
        }
        let Some((position, previous, current, next)) = ear else {
            bail!("canvas fill path could not be triangulated");
        };
        triangles.extend_from_slice(&[points[previous], points[current], points[next]]);
        indices.remove(position);
    }
    triangles.extend_from_slice(&[points[indices[0]], points[indices[1]], points[indices[2]]]);
    Ok(triangles)
}

fn point_in_triangle(point: Point, a: Point, b: Point, c: Point, counter_clockwise: bool) -> bool {
    let signs = [
        polygon_cross(a, b, point),
        polygon_cross(b, c, point),
        polygon_cross(c, a, point),
    ];
    if counter_clockwise {
        signs.into_iter().all(|value| value >= 0.0)
    } else {
        signs.into_iter().all(|value| value <= 0.0)
    }
}

fn visual_transform(label: &str, transform: &wit::VisualTransform) -> Result<VisualTransform> {
    finite_range(
        &format!("{label} translation x"),
        transform.translation_x,
        -4096.0,
        4096.0,
    )?;
    finite_range(
        &format!("{label} translation y"),
        transform.translation_y,
        -4096.0,
        4096.0,
    )?;
    finite_range(&format!("{label} scale"), transform.scale, 0.0, 8.0)?;
    finite_range(&format!("{label} opacity"), transform.opacity, 0.0, 1.0)?;
    Ok(VisualTransform {
        translation: Point::new(transform.translation_x, transform.translation_y),
        scale: transform.scale,
        opacity: transform.opacity,
    })
}

fn canvas_coordinate(label: &str, value: f32) -> Result<f32> {
    finite_range(label, value, -4096.0, 8192.0)?;
    Ok(value)
}

fn canvas_point(point: &wit::CanvasPoint) -> Result<Point> {
    Ok(Point::new(
        canvas_coordinate("canvas point x", point.x)?,
        canvas_coordinate("canvas point y", point.y)?,
    ))
}

fn canvas_rect(x: f32, y: f32, width: f32, height: f32) -> Result<()> {
    canvas_coordinate("canvas rect x", x)?;
    canvas_coordinate("canvas rect y", y)?;
    finite_range("canvas rect width", width, 0.0, 4096.0)?;
    finite_range("canvas rect height", height, 0.0, 4096.0)?;
    Ok(())
}

fn canvas_stroke_width(width: f32) -> Result<f32> {
    finite_range("canvas stroke width", width, 0.25, 256.0)?;
    Ok(width)
}

fn canvas_paint(paint: &wit::CanvasPaint) -> Result<CanvasPaint> {
    finite_range("canvas paint opacity", paint.opacity, 0.0, 1.0)?;
    let color = match &paint.color {
        wit::CanvasColor::Role(role) => CanvasColor::Role(color_role(*role)),
        wit::CanvasColor::Rgba(color) => {
            for (label, value) in [
                ("red", color.red),
                ("green", color.green),
                ("blue", color.blue),
                ("alpha", color.alpha),
            ] {
                finite_range(&format!("canvas RGBA {label}"), value, 0.0, 1.0)?;
            }
            CanvasColor::Rgba(Color::rgba(color.red, color.green, color.blue, color.alpha))
        }
    };
    Ok(CanvasPaint {
        color,
        opacity: paint.opacity,
    })
}

fn validate_flex(flex: &wit::Flex) -> Result<()> {
    finite_range("flex minimum", flex.minimum, 0.0, MAX_TOUCHBAR_WIDTH as f32)?;
    finite_range("flex basis", flex.basis, 0.0, MAX_TOUCHBAR_WIDTH as f32)?;
    finite_range("flex maximum", flex.maximum, 0.0, MAX_TOUCHBAR_WIDTH as f32)?;
    finite_range("flex grow", flex.grow, 0.0, 1000.0)?;
    finite_range("flex shrink", flex.shrink, 0.0, 1000.0)?;
    if flex.minimum > flex.basis || flex.basis > flex.maximum {
        bail!("flex values must satisfy minimum <= basis <= maximum");
    }
    Ok(())
}

fn finite_range(label: &str, value: f32, minimum: f32, maximum: f32) -> Result<()> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        bail!("{label} must be finite and between {minimum} and {maximum}");
    }
    Ok(())
}

fn color_role(role: wit::ColorRole) -> ColorRole {
    match role {
        wit::ColorRole::Background => ColorRole::Background,
        wit::ColorRole::Control => ColorRole::Control,
        wit::ColorRole::ControlPressed => ColorRole::ControlPressed,
        wit::ColorRole::Track => ColorRole::Track,
        wit::ColorRole::Foreground => ColorRole::Foreground,
        wit::ColorRole::Muted => ColorRole::Muted,
        wit::ColorRole::Accent => ColorRole::Accent,
        wit::ColorRole::Destructive => ColorRole::Destructive,
        wit::ColorRole::OnAccent => ColorRole::OnAccent,
        wit::ColorRole::OnDestructive => ColorRole::OnDestructive,
    }
}

fn text_align(align: wit::TextAlign) -> TextAlign {
    match align {
        wit::TextAlign::Leading => TextAlign::Leading,
        wit::TextAlign::Center => TextAlign::Center,
        wit::TextAlign::Trailing => TextAlign::Trailing,
    }
}

fn icon(value: wit::Icon) -> Icon {
    match value {
        wit::Icon::Volume => Icon::Volume,
        wit::Icon::Muted => Icon::Muted,
        wit::Icon::Play => Icon::Play,
        wit::Icon::Pause => Icon::Pause,
        wit::Icon::Check => Icon::Check,
        wit::Icon::ChevronLeft => Icon::ChevronLeft,
        wit::Icon::ChevronRight => Icon::ChevronRight,
    }
}

fn alignment(value: wit::CrossAxisAlignment) -> CrossAxisAlignment {
    match value {
        wit::CrossAxisAlignment::Start => CrossAxisAlignment::Start,
        wit::CrossAxisAlignment::Center => CrossAxisAlignment::Center,
        wit::CrossAxisAlignment::End => CrossAxisAlignment::End,
        wit::CrossAxisAlignment::Stretch => CrossAxisAlignment::Stretch,
    }
}

fn representation(value: wit::Representation) -> Representation {
    match value {
        wit::Representation::Minimal => Representation::Minimal,
        wit::Representation::Compact => Representation::Compact,
        wit::Representation::Full => Representation::Full,
    }
}

#[cfg(test)]
mod tests {
    use touchbar_protocol::broker_ipc::{BrokerErrorCode, WireCapabilityStatus};

    use super::*;

    fn label(text: &str) -> wit::Node {
        wit::Node::Label(wit::LabelNode {
            text: text.into(),
            size: 12.0,
            color: wit::ColorRole::Foreground,
            align: wit::TextAlign::Center,
        })
    }

    fn semantic_canvas_paint(role: wit::ColorRole) -> wit::CanvasPaint {
        wit::CanvasPaint {
            color: wit::CanvasColor::Role(role),
            opacity: 1.0,
        }
    }

    fn decode_view(nodes: &[wit::Node], limits: HostLimits) -> Result<UiNode> {
        let mut animations = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        ViewDecoder::new(
            nodes,
            limits,
            "test",
            Duration::ZERO,
            &mut animations,
            &mut effects,
        )
        .decode(0)
    }

    fn decode_view_with_assets(nodes: &[wit::Node], assets: &PackageAssets) -> Result<UiNode> {
        let mut animations = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        ViewDecoder::new_with_assets(
            nodes,
            HostLimits::default(),
            "test",
            Duration::ZERO,
            &mut animations,
            &mut effects,
            assets,
        )
        .decode(0)
    }

    fn asset_definition(kind: AssetKind, width: u32, height: u32) -> AssetDefinition {
        AssetDefinition {
            id: "mark".into(),
            path: match kind {
                AssetKind::Png => "assets/mark.png",
                AssetKind::SymbolicSvg => "assets/mark.svg",
            }
            .into(),
            kind,
            width,
            height,
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&vec![0xff; width as usize * height as usize * 4])
                .unwrap();
        }
        bytes
    }

    #[test]
    fn sealed_assets_decode_and_image_nodes_keep_live_semantic_tint() {
        let definition = asset_definition(AssetKind::SymbolicSvg, 24, 24);
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M2 2h20v20H2z"/></svg>"#;
        let bundle = encode_asset_bundle([("mark", svg.as_slice())]).unwrap();
        let assets = PackageAssets::decode(std::slice::from_ref(&definition), &bundle).unwrap();
        assert_eq!(assets.len(), 1);
        let nodes = [wit::Node::Image(wit::AssetImageNode {
            asset_id: "mark".into(),
            label: "TouchBar mark".into(),
            opacity: 0.8,
            fit: wit::ImageFit::Contain,
            tint: wit::ImageTint::Mask(wit::ColorRole::Accent),
        })];
        let UiNode::Image {
            image,
            opacity,
            fit,
            tint,
            label,
        } = decode_view_with_assets(&nodes, &assets).unwrap()
        else {
            panic!("expected an image node")
        };
        assert_eq!((image.width, image.height), (24, 24));
        assert_eq!(opacity, 0.8);
        assert_eq!(fit, ImageFit::Contain);
        assert_eq!(tint, ImageTint::Mask(ColorRole::Accent));
        assert_eq!(label, "TouchBar mark");
    }

    #[test]
    fn asset_decode_rejects_dimension_spoofing_active_svg_and_unknown_ids() {
        let png = png(2, 2);
        let bundle = encode_asset_bundle([("mark", png.as_slice())]).unwrap();
        assert!(
            PackageAssets::decode(&[asset_definition(AssetKind::Png, 3, 2)], &bundle)
                .unwrap_err()
                .to_string()
                .contains("dimensions are 2x2, expected 3x2")
        );

        let active =
            br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="file:///etc/passwd"/></svg>"#;
        let bundle = encode_asset_bundle([("mark", active.as_slice())]).unwrap();
        assert!(
            PackageAssets::decode(&[asset_definition(AssetKind::SymbolicSvg, 24, 24)], &bundle)
                .unwrap_err()
                .to_string()
                .contains("forbidden external or active content")
        );

        let node = [wit::Node::Image(wit::AssetImageNode {
            asset_id: "missing".into(),
            label: "Missing".into(),
            opacity: 1.0,
            fit: wit::ImageFit::Cover,
            tint: wit::ImageTint::None,
        })];
        assert!(
            decode_view_with_assets(&node, &PackageAssets::default())
                .unwrap_err()
                .to_string()
                .contains("unknown asset")
        );
    }

    fn wit_transform(x: f32, y: f32, scale: f32, opacity: f32) -> wit::VisualTransform {
        wit::VisualTransform {
            translation_x: x,
            translation_y: y,
            scale,
            opacity,
        }
    }

    fn motion_node(id: u64, end_scale: f32, child: u32) -> wit::Node {
        wit::Node::Motion(wit::MotionNode {
            animation_id: id,
            start_transform: wit_transform(-4.0, 0.0, 0.8, 0.0),
            end_transform: wit_transform(0.0, 0.0, end_scale, 1.0),
            duration_ms: 800,
            easing: wit::Easing::EaseInOut,
            playback: wit::AnimationPlayback::Alternate,
            child,
        })
    }

    fn effect_node(id: u64, source: &str, period: Option<u32>) -> wit::Node {
        wit::Node::ShaderEffect(wit::ShaderEffectNode {
            effect_id: id,
            label: "Theme wave".into(),
            source: source.into(),
            parameters: vec![0.25, 0.75],
            opacity: 0.8,
            preferred_width: 160.0,
            preferred_height: 60.0,
            animation_period_ms: period,
        })
    }

    #[test]
    fn flat_component_arena_becomes_responsive_theme_aware_ui() {
        let nodes = vec![
            label("full"),
            label("small"),
            wit::Node::Responsive(wit::ResponsiveNode {
                widget_id: 9,
                variants: vec![
                    wit::ResponsiveVariant {
                        representation: wit::Representation::Minimal,
                        minimum_width: 0.0,
                        node: 1,
                    },
                    wit::ResponsiveVariant {
                        representation: wit::Representation::Full,
                        minimum_width: 100.0,
                        node: 0,
                    },
                ],
            }),
        ];
        let mut animations = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        let root = ViewDecoder::new(
            &nodes,
            HostLimits::default(),
            "test",
            Duration::ZERO,
            &mut animations,
            &mut effects,
        )
        .decode(2)
        .unwrap();
        let mut ui = RetainedUi::new(root);
        let compact = ui.resolve(Rect::new(0.0, 0.0, 80.0, 60.0), Theme::default());
        assert_eq!(
            compact.inspector.representations.get(&WidgetId(9)),
            Some(&Representation::Minimal)
        );
        let full = ui.resolve(Rect::new(0.0, 0.0, 160.0, 60.0), Theme::default());
        assert_eq!(
            full.inspector.representations.get(&WidgetId(9)),
            Some(&Representation::Full)
        );
    }

    #[test]
    fn rejects_cycles_and_unbounded_text() {
        let cycle = [wit::Node::Responsive(wit::ResponsiveNode {
            widget_id: 1,
            variants: vec![wit::ResponsiveVariant {
                representation: wit::Representation::Full,
                minimum_width: 0.0,
                node: 0,
            }],
        })];
        assert!(
            decode_view(&cycle, HostLimits::default())
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );

        let limits = HostLimits {
            max_text_bytes: 3,
            ..HostLimits::default()
        };
        assert!(decode_view(&[label("long")], limits).is_err());
    }

    #[test]
    fn invalid_floats_cannot_reach_taffy_or_the_gpu_renderer() {
        let nodes = [wit::Node::Label(wit::LabelNode {
            text: "bad".into(),
            size: f32::NAN,
            color: wit::ColorRole::Foreground,
            align: wit::TextAlign::Center,
        })];
        assert!(decode_view(&nodes, HostLimits::default()).is_err());
    }

    #[test]
    fn canvas_commands_become_bounded_theme_aware_native_primitives() {
        let canvas = wit::Node::Canvas(wit::CanvasNode {
            label: "Signal history".into(),
            viewbox_width: 100.0,
            viewbox_height: 50.0,
            commands: vec![
                wit::CanvasCommand::FillRect(wit::CanvasRect {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 50.0,
                    radius: 4.0,
                    paint: semantic_canvas_paint(wit::ColorRole::Track),
                }),
                wit::CanvasCommand::Polyline(wit::CanvasPolyline {
                    points: vec![
                        wit::CanvasPoint { x: 0.0, y: 40.0 },
                        wit::CanvasPoint { x: 50.0, y: 10.0 },
                        wit::CanvasPoint { x: 100.0, y: 30.0 },
                    ],
                    width: 2.0,
                    paint: semantic_canvas_paint(wit::ColorRole::Accent),
                }),
                wit::CanvasCommand::FillLinearGradientRect(wit::CanvasGradientRect {
                    x: 0.0,
                    y: 44.0,
                    width: 100.0,
                    height: 6.0,
                    radius: 3.0,
                    gradient: wit::CanvasLinearGradient {
                        start: wit::CanvasPoint { x: 0.0, y: 47.0 },
                        end: wit::CanvasPoint { x: 100.0, y: 47.0 },
                        start_color: semantic_canvas_paint(wit::ColorRole::Accent),
                        end_color: semantic_canvas_paint(wit::ColorRole::Muted),
                    },
                }),
                wit::CanvasCommand::StrokePath(wit::CanvasPath {
                    segments: vec![
                        wit::CanvasPathSegment::MoveTo(wit::CanvasPoint { x: 0.0, y: 25.0 }),
                        wit::CanvasPathSegment::QuadraticTo(wit::CanvasQuadraticSegment {
                            control: wit::CanvasPoint { x: 25.0, y: 0.0 },
                            endpoint: wit::CanvasPoint { x: 50.0, y: 25.0 },
                        }),
                        wit::CanvasPathSegment::CubicTo(wit::CanvasCubicSegment {
                            control_one: wit::CanvasPoint { x: 60.0, y: 40.0 },
                            control_two: wit::CanvasPoint { x: 80.0, y: 4.0 },
                            endpoint: wit::CanvasPoint { x: 100.0, y: 25.0 },
                        }),
                    ],
                    width: 1.5,
                    paint: semantic_canvas_paint(wit::ColorRole::Foreground),
                }),
                wit::CanvasCommand::Text(wit::CanvasText {
                    x: 4.0,
                    y: 4.0,
                    width: 40.0,
                    height: 12.0,
                    text: "42%".into(),
                    size: 10.0,
                    color: semantic_canvas_paint(wit::ColorRole::Foreground),
                    align: wit::TextAlign::Leading,
                }),
            ],
        });
        let decoded = decode_view(&[canvas], HostLimits::default()).unwrap();
        let UiNode::Canvas {
            label,
            viewbox,
            commands,
        } = decoded
        else {
            panic!("canvas did not decode to a native Canvas node");
        };
        assert_eq!(label, "Signal history");
        assert_eq!(viewbox, Size::new(100.0, 50.0));
        assert_eq!(commands.len(), 5);
        assert!(matches!(
            &commands[1],
            CanvasCommand::Polyline { points, paint, .. }
                if points.len() == 3
                    && paint.color == CanvasColor::Role(ColorRole::Accent)
        ));
        assert!(matches!(
            &commands[2],
            CanvasCommand::FillLinearGradientRect { gradient, .. }
                if gradient.start_color.color == CanvasColor::Role(ColorRole::Accent)
                    && gradient.end_color.color == CanvasColor::Role(ColorRole::Muted)
        ));
        assert!(matches!(
            &commands[3],
            CanvasCommand::StrokePath { subpaths, .. }
                if subpaths.len() == 1 && subpaths[0].len() == 21
        ));
    }

    #[test]
    fn canvas_rejects_nonfinite_color_and_budget_abuse() {
        let paint = semantic_canvas_paint(wit::ColorRole::Accent);
        let invalid_point = [wit::Node::Canvas(wit::CanvasNode {
            label: "bad".into(),
            viewbox_width: 100.0,
            viewbox_height: 50.0,
            commands: vec![wit::CanvasCommand::Line(wit::CanvasLine {
                start: wit::CanvasPoint {
                    x: f32::NAN,
                    y: 0.0,
                },
                end: wit::CanvasPoint { x: 1.0, y: 1.0 },
                width: 1.0,
                paint,
            })],
        })];
        assert!(decode_view(&invalid_point, HostLimits::default()).is_err());

        let invalid_rgba = [wit::Node::Canvas(wit::CanvasNode {
            label: "bad color".into(),
            viewbox_width: 100.0,
            viewbox_height: 50.0,
            commands: vec![wit::CanvasCommand::FillCircle(wit::CanvasCircle {
                center: wit::CanvasPoint { x: 5.0, y: 5.0 },
                radius: 2.0,
                paint: wit::CanvasPaint {
                    color: wit::CanvasColor::Rgba(wit::Rgba {
                        red: 2.0,
                        green: 0.0,
                        blue: 0.0,
                        alpha: 1.0,
                    }),
                    opacity: 1.0,
                },
            })],
        })];
        assert!(decode_view(&invalid_rgba, HostLimits::default()).is_err());

        let too_many_points = [wit::Node::Canvas(wit::CanvasNode {
            label: "too many".into(),
            viewbox_width: 100.0,
            viewbox_height: 50.0,
            commands: vec![wit::CanvasCommand::Polyline(wit::CanvasPolyline {
                points: vec![
                    wit::CanvasPoint { x: 0.0, y: 0.0 },
                    wit::CanvasPoint { x: 1.0, y: 1.0 },
                    wit::CanvasPoint { x: 2.0, y: 2.0 },
                ],
                width: 1.0,
                paint,
            })],
        })];
        let limits = HostLimits {
            max_canvas_points: 2,
            ..HostLimits::default()
        };
        assert!(
            decode_view(&too_many_points, limits)
                .unwrap_err()
                .to_string()
                .contains("point budget")
        );

        let command_limits = HostLimits {
            max_canvas_commands: 0,
            ..HostLimits::default()
        };
        assert!(
            decode_view(&too_many_points, command_limits)
                .unwrap_err()
                .to_string()
                .contains("command budget")
        );
    }

    #[test]
    fn gradients_and_paths_reject_degenerate_grammar_and_expansion_abuse() {
        let canvas = |command| {
            [wit::Node::Canvas(wit::CanvasNode {
                label: "advanced".into(),
                viewbox_width: 100.0,
                viewbox_height: 50.0,
                commands: vec![command],
            })]
        };
        let degenerate = canvas(wit::CanvasCommand::FillLinearGradientRect(
            wit::CanvasGradientRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 50.0,
                radius: 0.0,
                gradient: wit::CanvasLinearGradient {
                    start: wit::CanvasPoint { x: 5.0, y: 5.0 },
                    end: wit::CanvasPoint { x: 5.0, y: 5.0 },
                    start_color: semantic_canvas_paint(wit::ColorRole::Accent),
                    end_color: semantic_canvas_paint(wit::ColorRole::Muted),
                },
            },
        ));
        assert!(
            decode_view(&degenerate, HostLimits::default())
                .unwrap_err()
                .to_string()
                .contains("nonzero axis")
        );

        let missing_move = canvas(wit::CanvasCommand::StrokePath(wit::CanvasPath {
            segments: vec![
                wit::CanvasPathSegment::LineTo(wit::CanvasPoint { x: 1.0, y: 1.0 }),
                wit::CanvasPathSegment::LineTo(wit::CanvasPoint { x: 2.0, y: 2.0 }),
            ],
            width: 1.0,
            paint: semantic_canvas_paint(wit::ColorRole::Foreground),
        }));
        assert!(
            decode_view(&missing_move, HostLimits::default())
                .unwrap_err()
                .to_string()
                .contains("move-to")
        );

        let curve = canvas(wit::CanvasCommand::StrokePath(wit::CanvasPath {
            segments: vec![
                wit::CanvasPathSegment::MoveTo(wit::CanvasPoint { x: 0.0, y: 0.0 }),
                wit::CanvasPathSegment::CubicTo(wit::CanvasCubicSegment {
                    control_one: wit::CanvasPoint { x: 10.0, y: 20.0 },
                    control_two: wit::CanvasPoint { x: 20.0, y: 10.0 },
                    endpoint: wit::CanvasPoint { x: 30.0, y: 30.0 },
                }),
            ],
            width: 1.0,
            paint: semantic_canvas_paint(wit::ColorRole::Foreground),
        }));
        let limits = HostLimits {
            max_canvas_path_segments: 1,
            ..HostLimits::default()
        };
        assert!(
            decode_view(&curve, limits)
                .unwrap_err()
                .to_string()
                .contains("path segment budget")
        );
        let limits = HostLimits {
            max_canvas_points: 12,
            ..HostLimits::default()
        };
        assert!(
            decode_view(&curve, limits)
                .unwrap_err()
                .to_string()
                .contains("point budget")
        );
    }

    #[test]
    fn filled_paths_are_triangulated_and_reject_open_intersecting_or_excessive_shapes() {
        let canvas = |segments, limits| {
            decode_view(
                &[wit::Node::Canvas(wit::CanvasNode {
                    label: "filled".into(),
                    viewbox_width: 100.0,
                    viewbox_height: 50.0,
                    commands: vec![wit::CanvasCommand::FillPath(wit::CanvasFillPath {
                        segments,
                        paint: semantic_canvas_paint(wit::ColorRole::Accent),
                    })],
                })],
                limits,
            )
        };
        let point = |x, y| wit::CanvasPoint { x, y };
        let valid = vec![
            wit::CanvasPathSegment::MoveTo(point(0.0, 0.0)),
            wit::CanvasPathSegment::LineTo(point(30.0, 0.0)),
            wit::CanvasPathSegment::LineTo(point(15.0, 12.0)),
            wit::CanvasPathSegment::LineTo(point(30.0, 30.0)),
            wit::CanvasPathSegment::LineTo(point(0.0, 30.0)),
            wit::CanvasPathSegment::Close,
        ];
        let UiNode::Canvas { commands, .. } = canvas(valid.clone(), HostLimits::default()).unwrap()
        else {
            panic!("expected canvas")
        };
        let CanvasCommand::FillPath { triangles, paint } = &commands[0] else {
            panic!("expected filled path")
        };
        assert_eq!(triangles.len(), 9);
        assert_eq!(*paint, CanvasPaint::role(ColorRole::Accent));

        let mut open = valid.clone();
        open.pop();
        assert!(canvas(open, HostLimits::default()).is_err());

        let crossing = vec![
            wit::CanvasPathSegment::MoveTo(point(0.0, 0.0)),
            wit::CanvasPathSegment::LineTo(point(30.0, 30.0)),
            wit::CanvasPathSegment::LineTo(point(0.0, 30.0)),
            wit::CanvasPathSegment::LineTo(point(30.0, 0.0)),
            wit::CanvasPathSegment::Close,
        ];
        assert!(
            canvas(crossing, HostLimits::default())
                .unwrap_err()
                .to_string()
                .contains("self-intersects")
        );

        let limits = HostLimits {
            max_canvas_fill_triangles: 1,
            ..HostLimits::default()
        };
        assert!(
            canvas(valid, limits)
                .unwrap_err()
                .to_string()
                .contains("fill triangle budget")
        );
    }

    #[test]
    fn animations_are_bounded_and_keep_host_time_by_stable_identity() {
        let nodes = [label("pulse"), motion_node(7, 1.0, 0)];
        let mut animations = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        let decode = |registry: &mut AnimationRegistry,
                      effects: &mut EffectRegistry,
                      nodes: &[wit::Node],
                      now| {
            ViewDecoder::new(
                nodes,
                HostLimits::default(),
                "media",
                now,
                registry,
                effects,
            )
            .decode(1)
        };
        let first = decode(
            &mut animations,
            &mut effects,
            &nodes,
            Duration::from_secs(2),
        )
        .unwrap();
        let UiNode::Motion { motion: first, .. } = first else {
            panic!("motion node did not decode");
        };
        assert_eq!(first.started, Duration::from_secs(2));
        assert_eq!(first.playback, MotionPlayback::Alternate);

        let retained = decode(
            &mut animations,
            &mut effects,
            &nodes,
            Duration::from_secs(9),
        )
        .unwrap();
        let UiNode::Motion {
            motion: retained, ..
        } = retained
        else {
            panic!("motion node did not decode");
        };
        assert_eq!(retained.started, first.started);

        let changed = [label("pulse"), motion_node(7, 1.2, 0)];
        let restarted = decode(
            &mut animations,
            &mut effects,
            &changed,
            Duration::from_secs(10),
        )
        .unwrap();
        let UiNode::Motion {
            motion: restarted, ..
        } = restarted
        else {
            panic!("motion node did not decode");
        };
        assert_eq!(restarted.started, Duration::from_secs(10));

        let conflicting = [
            label("pulse"),
            motion_node(7, 1.0, 0),
            motion_node(7, 1.2, 0),
            wit::Node::Layer(vec![1, 2]),
        ];
        let mut registry = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        assert!(
            ViewDecoder::new(
                &conflicting,
                HostLimits::default(),
                "media",
                Duration::ZERO,
                &mut registry,
                &mut effects,
            )
            .decode(3)
            .unwrap_err()
            .to_string()
            .contains("conflicting")
        );

        let invalid = [label("pulse"), motion_node(0, 1.0, 0)];
        let mut registry = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        assert!(
            ViewDecoder::new(
                &invalid,
                HostLimits::default(),
                "media",
                Duration::ZERO,
                &mut registry,
                &mut effects,
            )
            .decode(1)
            .is_err()
        );
        let invalid = [
            label("pulse"),
            wit::Node::Motion(wit::MotionNode {
                start_transform: wit_transform(0.0, 0.0, f32::INFINITY, 1.0),
                ..match motion_node(8, 1.0, 0) {
                    wit::Node::Motion(motion) => motion,
                    _ => unreachable!(),
                }
            }),
        ];
        let mut registry = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        assert!(
            ViewDecoder::new(
                &invalid,
                HostLimits::default(),
                "media",
                Duration::ZERO,
                &mut registry,
                &mut effects,
            )
            .decode(1)
            .is_err()
        );
        let limits = HostLimits {
            max_animations: 0,
            ..HostLimits::default()
        };
        let mut registry = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        assert!(
            ViewDecoder::new(
                &nodes,
                limits,
                "media",
                Duration::ZERO,
                &mut registry,
                &mut effects,
            )
            .decode(1)
            .unwrap_err()
            .to_string()
            .contains("animation budget")
        );
    }

    #[test]
    fn shader_effects_are_bounded_and_keep_host_time_by_stable_identity() {
        const FIRST: &str = "let amount = 0.5 + 0.5 * sin(uv.x * 8.0 - time); let color = mix(background, accent, amount * params0.x);";
        const SECOND: &str = "let amount = smoothstep(0.0, 1.0, uv.x); let color = mix(control, destructive, amount);";
        let nodes = [effect_node(17, FIRST, Some(2_000))];
        let mut animations = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        let mut decode = |registry: &mut EffectRegistry, nodes: &[wit::Node], now| {
            ViewDecoder::new(
                nodes,
                HostLimits::default(),
                "media",
                now,
                &mut animations,
                registry,
            )
            .decode(0)
        };
        let first = decode(&mut effects, &nodes, Duration::from_secs(2)).unwrap();
        let UiNode::ShaderEffect { effect: first, .. } = first else {
            panic!("shader effect did not decode");
        };
        assert_eq!(first.started, Duration::from_secs(2));
        assert_eq!(first.period, Some(Duration::from_secs(2)));
        assert_eq!(&first.parameters[..2], &[0.25, 0.75]);

        let retained = decode(&mut effects, &nodes, Duration::from_secs(9)).unwrap();
        let UiNode::ShaderEffect {
            effect: retained, ..
        } = retained
        else {
            panic!("shader effect did not decode");
        };
        assert_eq!(retained.started, first.started);
        assert!(Arc::ptr_eq(&retained.program, &first.program));

        let changed = [effect_node(17, SECOND, Some(2_000))];
        let restarted = decode(&mut effects, &changed, Duration::from_secs(10)).unwrap();
        let UiNode::ShaderEffect {
            effect: restarted, ..
        } = restarted
        else {
            panic!("shader effect did not decode");
        };
        assert_eq!(restarted.started, Duration::from_secs(10));

        let invalid = [effect_node(0, FIRST, Some(2_000))];
        assert!(decode(&mut effects, &invalid, Duration::ZERO).is_err());
        let invalid = [effect_node(18, FIRST, Some(99))];
        assert!(decode(&mut effects, &invalid, Duration::ZERO).is_err());
        let mut invalid = match effect_node(18, FIRST, None) {
            wit::Node::ShaderEffect(effect) => effect,
            _ => unreachable!(),
        };
        invalid.parameters = vec![0.0; MAX_EFFECT_PARAMETERS + 1];
        assert!(
            decode(
                &mut effects,
                &[wit::Node::ShaderEffect(invalid)],
                Duration::ZERO,
            )
            .is_err()
        );

        let conflicting = [
            effect_node(21, FIRST, None),
            effect_node(21, SECOND, None),
            wit::Node::Layer(vec![0, 1]),
        ];
        let mut conflict_animations = AnimationRegistry::default();
        assert!(
            ViewDecoder::new(
                &conflicting,
                HostLimits::default(),
                "media",
                Duration::ZERO,
                &mut conflict_animations,
                &mut effects,
            )
            .decode(2)
            .unwrap_err()
            .to_string()
            .contains("conflicting descriptions")
        );

        let limits = HostLimits {
            max_effect_nodes: 0,
            ..HostLimits::default()
        };
        let mut animations = AnimationRegistry::default();
        let mut no_effects = EffectRegistry::default();
        assert!(
            ViewDecoder::new(
                &nodes,
                limits,
                "media",
                Duration::ZERO,
                &mut animations,
                &mut no_effects,
            )
            .decode(0)
            .unwrap_err()
            .to_string()
            .contains("node budget")
        );

        let limits = HostLimits {
            max_effect_programs: 1,
            ..HostLimits::default()
        };
        let mut animations = AnimationRegistry::default();
        let mut effects = EffectRegistry::default();
        ViewDecoder::new(
            &[effect_node(1, FIRST, None)],
            limits,
            "media",
            Duration::ZERO,
            &mut animations,
            &mut effects,
        )
        .decode(0)
        .unwrap();
        assert!(
            ViewDecoder::new(
                &[effect_node(1, SECOND, None)],
                limits,
                "media",
                Duration::from_secs(1),
                &mut animations,
                &mut effects,
            )
            .decode(0)
            .unwrap_err()
            .to_string()
            .contains("lifetime shader-effect program budget")
        );
    }

    #[test]
    fn broker_client_submits_asynchronously_and_preserves_event_order() {
        let (host, supervisor) = Seqpacket::pair().unwrap();
        let server = std::thread::spawn(move || {
            assert!(matches!(
                supervisor.recv_host().unwrap(),
                HostMessage::GetCapabilities { request_id: 1 }
            ));
            supervisor
                .send_supervisor(&SupervisorMessage::Capabilities {
                    request_id: 1,
                    generation: 1,
                    states: vec![CapabilityState {
                        capability: "context.read.v1".into(),
                        required: false,
                        status: WireCapabilityStatus::NeedsConsent,
                    }],
                })
                .unwrap();
            assert!(matches!(
                supervisor.recv_host().unwrap(),
                HostMessage::Request { request_id: 2, .. }
            ));
            supervisor
                .send_supervisor(&SupervisorMessage::CapabilityChanged {
                    generation: 2,
                    state: CapabilityState {
                        capability: "context.read.v1".into(),
                        required: false,
                        status: WireCapabilityStatus::Granted,
                    },
                })
                .unwrap();
            supervisor
                .send_supervisor(&SupervisorMessage::Overflow {
                    generation: 2,
                    dropped_events: 3,
                })
                .unwrap();
            supervisor
                .send_supervisor(&SupervisorMessage::ResourceEvent {
                    resource_id: 7,
                    sequence: 4,
                    result: BrokerResult::Success {
                        payload: vec![1, 2],
                    },
                })
                .unwrap();
            supervisor
                .send_supervisor(&SupervisorMessage::Response {
                    request_id: 2,
                    result: BrokerResult::Error(BrokerErrorCode::Unavailable),
                })
                .unwrap();
        });
        let mut client = BrokerClient::connect(host).unwrap();
        assert!(client.try_receive_event().unwrap().is_none());
        assert_eq!(
            client
                .submit(
                    CallbackPhase::Input,
                    "context.read.v1",
                    "read",
                    Vec::new(),
                    None,
                )
                .unwrap(),
            2
        );
        assert!(matches!(
            client.receive_event().unwrap(),
            BrokerEvent::CapabilityChanged { generation: 2, .. }
        ));
        assert_eq!(
            client.receive_event().unwrap(),
            BrokerEvent::Overflow {
                generation: 2,
                dropped_events: 3,
            }
        );
        assert_eq!(
            client.receive_event().unwrap(),
            BrokerEvent::ResourceEvent {
                resource_id: 7,
                sequence: 4,
                result: BrokerResult::Success {
                    payload: vec![1, 2]
                },
            }
        );
        assert_eq!(
            client.receive_event().unwrap(),
            BrokerEvent::Completion {
                request_id: 2,
                result: BrokerResult::Error(BrokerErrorCode::Unavailable),
            }
        );
        assert_eq!(client.generation(), 2);
        assert_eq!(client.dropped_events(), 3);
        assert_eq!(client.states()[0].status, WireCapabilityStatus::Granted);
        server.join().unwrap();
    }

    #[test]
    fn pure_guest_phases_cannot_submit_broker_work() {
        let (_, mut store) = create_store(HostLimits::default(), None).unwrap();
        assert_eq!(
            wit_broker::Host::request(
                store.data_mut(),
                "context.read.v1".into(),
                "read".into(),
                Vec::new(),
            ),
            Err(wit_broker::ErrorCode::InvalidPhase)
        );
        store.data_mut().phase = Some(CallbackPhase::Render);
        assert_eq!(
            wit_broker::Host::request(
                store.data_mut(),
                "context.read.v1".into(),
                "read".into(),
                Vec::new(),
            ),
            Err(wit_broker::ErrorCode::InvalidPhase)
        );
        store.data_mut().phase = Some(CallbackPhase::Input);
        assert_eq!(
            wit_broker::Host::request(
                store.data_mut(),
                "context.read.v1".into(),
                "read".into(),
                Vec::new(),
            ),
            Err(wit_broker::ErrorCode::Unavailable)
        );
    }

    #[test]
    fn only_trusted_activation_callbacks_receive_short_lived_authority() {
        let event = InputEvent {
            item_id: "media".into(),
            widget_id: 9,
            kind: InputKind::Activated,
            value: None,
            contact_id: Some(7),
            activation: Some(InputActivation {
                origin: ActivationOrigin::Physical,
                input_sequence: 12,
            }),
        };
        let before = monotonic_micros().unwrap();
        let activation = activation_for_input(4, &event, Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let after = monotonic_micros().unwrap();
        assert_eq!(activation.surface_instance, 4);
        assert_eq!(activation.item_id, "media");
        assert_eq!(activation.widget_id, 9);
        assert_eq!(activation.input_sequence, 12);
        assert!(activation.deadline_monotonic_micros >= before + 2_000_000);
        assert!(activation.deadline_monotonic_micros <= after + 2_000_000);

        let mut pressed = event.clone();
        pressed.kind = InputKind::Pressed;
        assert!(
            activation_for_input(4, &pressed, Duration::from_secs(2))
                .unwrap()
                .is_none()
        );
        let mut synthetic = event;
        synthetic.activation = Some(InputActivation {
            origin: ActivationOrigin::Synthetic,
            input_sequence: 12,
        });
        assert!(
            activation_for_input(4, &synthetic, Duration::from_secs(2))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cancellation_acknowledgements_are_not_guest_completion_events() {
        let (host, supervisor) = Seqpacket::pair().unwrap();
        let server = std::thread::spawn(move || {
            assert!(matches!(
                supervisor.recv_host().unwrap(),
                HostMessage::GetCapabilities { request_id: 1 }
            ));
            supervisor
                .send_supervisor(&SupervisorMessage::Capabilities {
                    request_id: 1,
                    generation: 1,
                    states: Vec::new(),
                })
                .unwrap();
            assert!(matches!(
                supervisor.recv_host().unwrap(),
                HostMessage::Cancel {
                    request_id: 2,
                    target_request_id: 41,
                }
            ));
            supervisor
                .send_supervisor(&SupervisorMessage::Response {
                    request_id: 2,
                    result: BrokerResult::Success {
                        payload: Vec::new(),
                    },
                })
                .unwrap();
            supervisor
                .send_supervisor(&SupervisorMessage::Overflow {
                    generation: 1,
                    dropped_events: 1,
                })
                .unwrap();
        });
        let mut client = BrokerClient::connect(host).unwrap();
        assert_eq!(client.cancel(41).unwrap(), 2);
        assert_eq!(
            client.receive_event().unwrap(),
            BrokerEvent::Overflow {
                generation: 1,
                dropped_events: 1,
            }
        );
        server.join().unwrap();
    }

    #[test]
    fn trusted_host_state_attaches_activation_to_the_submitted_packet() {
        let (host, supervisor) = Seqpacket::pair().unwrap();
        let expected = ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 9,
            item_id: "media".into(),
            widget_id: 4,
            input_sequence: 27,
            deadline_monotonic_micros: monotonic_micros().unwrap() + 1_000_000,
        };
        let expected_on_server = expected.clone();
        let server = std::thread::spawn(move || {
            assert!(matches!(
                supervisor.recv_host().unwrap(),
                HostMessage::GetCapabilities { request_id: 1 }
            ));
            supervisor
                .send_supervisor(&SupervisorMessage::Capabilities {
                    request_id: 1,
                    generation: 1,
                    states: Vec::new(),
                })
                .unwrap();
            let HostMessage::Request {
                request_id: 2,
                phase: CallbackPhase::Input,
                activation,
                ..
            } = supervisor.recv_host().unwrap()
            else {
                panic!("expected an input request");
            };
            assert_eq!(activation, Some(expected_on_server));
        });
        let broker = BrokerClient::connect(host).unwrap();
        let (_, mut store) = create_store(HostLimits::default(), Some(broker)).unwrap();
        store.data_mut().phase = Some(CallbackPhase::Input);
        store.data_mut().activation = Some(expected);
        assert_eq!(
            wit_broker::Host::request(
                store.data_mut(),
                "context.read.v1".into(),
                "read".into(),
                Vec::new(),
            ),
            Ok(2)
        );
        server.join().unwrap();
    }
}
