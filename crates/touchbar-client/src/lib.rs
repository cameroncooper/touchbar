//! Rust client runtime for an TouchBar plugin.
//!
//! This crate deliberately stops at rendering and lifecycle. It does not
//! broker desktop actions: a plugin remains an ordinary user process and uses
//! PipeWire, D-Bus, sockets, or other operating-system APIs directly.

use std::{
    ffi::c_void,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
    ptr,
    time::Instant,
};

use anyhow::{Context as _, Result, bail};
use glow::HasContext as _;
use khronos_egl as egl;
pub use touchbar_protocol::appearance::{AppearanceSnapshot, ColorScheme, MotionPolicy, Rgba8};
use touchbar_protocol::{
    DEFAULT_SOCKET_NAME, TOUCHBAR_PROTOCOL_VERSION,
    appearance::ColorRole,
    client::{touchbar_appearance_v1, touchbar_manager_v1, touchbar_surface_v1},
    join_input_sequence,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_compositor, wl_registry, wl_surface},
};
use wayland_egl::WlEglSurface;

const EGL_SYNC_NATIVE_FENCE_ANDROID: egl::Enum = 0x3144;
type DupNativeFenceFd = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;

/// Options used to connect one standalone plugin process to `touchbar-sessiond`.
#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub plugin_id: String,
    pub item_id: String,
    pub compact_sizing: Sizing,
    pub expanded_sizing: Option<Sizing>,
    pub surface_role: SurfaceRole,
    pub require_hardware: bool,
    /// Used only when `WAYLAND_DISPLAY` was not already supplied by a launcher.
    pub default_socket_name: String,
}

impl ClientOptions {
    pub fn new(plugin_id: impl Into<String>) -> Self {
        let plugin_id = plugin_id.into();
        Self {
            item_id: plugin_id.clone(),
            plugin_id,
            compact_sizing: Sizing::new(80, 1004, 2008),
            expanded_sizing: None,
            surface_role: SurfaceRole::Item,
            require_hardware: false,
            default_socket_name: DEFAULT_SOCKET_NAME.into(),
        }
    }

    pub fn require_hardware(mut self, required: bool) -> Self {
        self.require_hardware = required;
        self
    }

    pub fn item_id(mut self, item_id: impl Into<String>) -> Self {
        self.item_id = item_id.into();
        self
    }

    pub fn compact_sizing(mut self, sizing: Sizing) -> Self {
        self.compact_sizing = sizing;
        self
    }

    pub fn expanded_sizing(mut self, sizing: Sizing) -> Self {
        self.expanded_sizing = Some(sizing);
        self
    }

    /// Render a non-interactive full-canvas layer behind normal item surfaces.
    pub fn backdrop(mut self) -> Self {
        self.surface_role = SurfaceRole::Backdrop;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceRole {
    Item,
    Backdrop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sizing {
    pub minimum: u32,
    pub preferred: u32,
    pub maximum: u32,
}

impl Sizing {
    pub const fn new(minimum: u32, preferred: u32, maximum: u32) -> Self {
        Self {
            minimum,
            preferred,
            maximum,
        }
    }

    fn is_valid(self) -> bool {
        self.minimum > 0 && self.minimum <= self.preferred && self.preferred <= self.maximum
    }
}

/// Geometry and pacing selected by the compositor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

/// Timing information for a render callback.
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub number: u64,
    pub elapsed_seconds: f32,
    pub surface: SurfaceConfig,
}

/// What the runtime should do after submitting the frame just rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameFlow {
    /// Ask the compositor for another frame callback.
    Animate,
    /// Submit this frame and wait for input, visibility, or configuration.
    Wait,
    /// Submit this final frame and terminate normally.
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContactPhase {
    Down,
    Motion,
    Up,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContactOrigin {
    Physical,
    Synthetic,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchContact {
    pub id: u32,
    pub phase: ContactPhase,
    pub x: f32,
    pub y: f32,
    /// Monotonic time in the client runtime's clock domain, suitable for UI
    /// gesture recognizers and frame-timed animation.
    pub time: std::time::Duration,
    /// Original timestamp supplied by the compositor.
    pub compositor_time: std::time::Duration,
    /// Nonzero compositor-issued sequence shared by every event in one
    /// captured gesture.
    pub input_sequence: u64,
    /// Whether this contact came from the actual Touch Bar or a developer
    /// simulator. Synthetic contacts can drive UI but never grant authority.
    pub origin: ContactOrigin,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopoverAnchor {
    pub x: f32,
    pub width: f32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPresentationPolicy {
    Anchored,
    InPlace,
    Slot(String),
    Region(String),
    FullBar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPresentationLifecycle {
    Transient { contact_id: u32 },
    Persistent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationDismissReason {
    Requested,
    Selection,
    OutsidePress,
    Timeout,
    SourceHidden,
    Replaced,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentationSessionRequest {
    Begin {
        session_id: u32,
        policy: SessionPresentationPolicy,
        lifecycle: SessionPresentationLifecycle,
    },
    End {
        session_id: u32,
        reason: PresentationDismissReason,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum PresentationSessionEvent {
    Anchor {
        session_id: u32,
        anchor: PopoverAnchor,
    },
    Started {
        session_id: u32,
        policy: SessionPresentationPolicy,
        lifecycle: SessionPresentationLifecycle,
    },
    Ended {
        session_id: u32,
        reason: PresentationDismissReason,
    },
}

/// Application callbacks run on the plugin process's Wayland/GL thread.
pub trait Application {
    /// Called after the EGL context is current and whenever geometry changes.
    fn configured(&mut self, _graphics: &Graphics, _config: SurfaceConfig) -> Result<()> {
        Ok(())
    }

    /// Receive one complete compositor-selected semantic appearance snapshot.
    /// Returning true requests a redraw. Raw GLES applications may ignore it.
    fn appearance_changed(&mut self, _appearance: AppearanceSnapshot) -> Result<bool> {
        Ok(false)
    }

    /// Draw one complete frame using [`Graphics::gl`]. The runtime performs
    /// `eglSwapBuffers`, so application code never owns the Wayland buffers.
    fn render(&mut self, graphics: &Graphics, frame: FrameInfo) -> Result<FrameFlow>;

    /// Called when compositor policy changes surface visibility. Returning
    /// true asks for a fresh frame when the surface becomes visible.
    fn visibility_changed(&mut self, _visible: bool) -> Result<bool> {
        Ok(false)
    }

    /// Deliver one compositor-captured contact in current surface-local
    /// coordinates. Return true when the visual state became dirty.
    fn touch(&mut self, _contact: TouchContact) -> Result<bool> {
        Ok(false)
    }

    /// Optional descriptor for an application-owned event source. The
    /// descriptor must remain valid until the next application callback.
    fn external_event_fd(&self) -> Option<RawFd> {
        None
    }

    /// Called on the application thread when `external_event_fd` is readable.
    /// Returning true requests a redraw under normal visibility/frame pacing.
    fn external_event(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Receive confirmations for identified presentation
    /// sessions. Anchor always precedes Started for anchored presentations.
    fn presentation_session_changed(&mut self, _event: PresentationSessionEvent) -> Result<()> {
        Ok(())
    }

    /// Return queued session commands. The runtime drains a bounded batch
    /// after callbacks, preserving replace/dismiss ordering.
    fn take_presentation_session_request(&mut self) -> Option<PresentationSessionRequest> {
        None
    }
}

/// A current GLES3 context targeting the plugin's compositor-assigned surface.
pub struct Graphics {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    surface: egl::Surface,
    context: egl::Context,
    gl: glow::Context,
    dup_native_fence_fd: Option<DupNativeFenceFd>,
    window: WlEglSurface,
    width: u32,
    height: u32,
    renderer_name: String,
}

impl Graphics {
    fn new(
        connection: &Connection,
        wayland_surface: &wl_surface::WlSurface,
        width: u32,
        height: u32,
        require_hardware: bool,
    ) -> Result<Self> {
        if !wayland_egl::is_available() {
            bail!("libwayland-egl is unavailable");
        }
        let window = WlEglSurface::new(wayland_surface.id(), width as i32, height as i32)
            .context("create Wayland EGL window")?;

        // SAFETY: libEGL is loaded through its stable ABI.
        let egl = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required().context("load libEGL.so.1")?
        };
        egl.bind_api(egl::OPENGL_ES_API)
            .context("select OpenGL ES API")?;

        // SAFETY: the connection outlives this graphics object and its display.
        let display =
            unsafe { egl.get_display(connection.backend().display_ptr().cast::<c_void>()) }
                .context("obtain Wayland EGL display")?;
        egl.initialize(display).context("initialize Wayland EGL")?;
        let extensions = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .context("query Wayland EGL extensions")?
            .to_string_lossy();
        let dup_native_fence_fd = if extensions
            .split_whitespace()
            .any(|extension| extension == "EGL_ANDROID_native_fence_sync")
        {
            let function = egl
                .get_proc_address("eglDupNativeFenceFDANDROID")
                .context("EGL_ANDROID_native_fence_sync has no fence export function")?;
            // SAFETY: EGL returned this address for the exact extension entry point.
            Some(unsafe {
                std::mem::transmute::<unsafe extern "system" fn(), DupNativeFenceFd>(function)
            })
        } else {
            None
        };

        let attributes = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attributes)
            .context("choose Wayland EGL config")?
            .context("no GLES3 Wayland EGL config")?;
        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .context("create GLES3 context")?;
        // SAFETY: `window` belongs to this display and outlives the EGL surface.
        let surface = unsafe {
            egl.create_window_surface(display, config, window.ptr() as *mut c_void, None)
        }
        .context("create Wayland EGL surface")?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .context("make plugin GLES context current")?;
        egl.swap_interval(display, 0)
            .context("disable implicit EGL swap pacing")?;

        // SAFETY: EGL owns the function pointers for the current context.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|function| function as *const () as *const c_void)
                    .unwrap_or(ptr::null())
            })
        };
        // SAFETY: this context is current on the runtime thread.
        let renderer_name = unsafe { gl.get_parameter_string(glow::RENDERER) };
        let lowercase = renderer_name.to_ascii_lowercase();
        if require_hardware && (lowercase.contains("llvmpipe") || lowercase.contains("softpipe")) {
            bail!("software renderer selected: {renderer_name}");
        }

        Ok(Self {
            egl,
            display,
            surface,
            context,
            gl,
            dup_native_fence_fd,
            window,
            width,
            height,
            renderer_name,
        })
    }

    pub fn gl(&self) -> &glow::Context {
        &self.gl
    }

    pub fn renderer_name(&self) -> &str {
        &self.renderer_name
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.window.resize(width as i32, height as i32, 0, 0);
        self.width = width;
        self.height = height;
    }

    fn present(&self) -> Result<()> {
        self.egl
            .swap_buffers(self.display, self.surface)
            .context("submit plugin DMA-BUF")
    }

    fn export_acquire_fence(&self) -> Result<Option<OwnedFd>> {
        let Some(dup_native_fence_fd) = self.dup_native_fence_fd else {
            return Ok(None);
        };
        // EGL_SYNC_NATIVE_FENCE_ANDROID captures all prior commands in this
        // current GLES context. Flushing publishes the fence to the kernel;
        // exporting duplicates the sync_file descriptor for Wayland transfer.
        let sync = unsafe {
            self.egl.create_sync(
                self.display,
                EGL_SYNC_NATIVE_FENCE_ANDROID,
                &[egl::ATTRIB_NONE],
            )
        }
        .context("create plugin DMA-BUF acquire fence")?;
        unsafe {
            self.gl.flush();
        }
        let raw_fd = unsafe {
            dup_native_fence_fd(
                self.display.as_ptr().cast::<c_void>(),
                sync.as_ptr().cast::<c_void>(),
            )
        };
        let destroy = unsafe { self.egl.destroy_sync(self.display, sync) };
        if raw_fd < 0 {
            destroy.context("destroy failed plugin acquire fence")?;
            bail!("export plugin DMA-BUF acquire fence failed");
        }
        // SAFETY: eglDupNativeFenceFDANDROID returned a new owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        destroy.context("destroy exported plugin acquire fence")?;
        Ok(Some(fd))
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        let _ = self.window.get_size();
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunSummary {
    pub plugin_id: String,
    pub frames: u64,
    pub renderer_name: String,
}

struct Runtime<A> {
    application: A,
    compositor: Option<wl_compositor::WlCompositor>,
    manager: Option<touchbar_manager_v1::TouchbarManagerV1>,
    appearance: Option<touchbar_appearance_v1::TouchbarAppearanceV1>,
    pending_appearance: Option<(AppearanceSnapshot, u8)>,
    surface: Option<wl_surface::WlSurface>,
    role: Option<touchbar_surface_v1::TouchbarSurfaceV1>,
    graphics: Option<Graphics>,
    config: Option<SurfaceConfig>,
    options: ClientOptions,
    started: Instant,
    frames: u64,
    frame_pending: bool,
    visible: bool,
    redraw_pending: bool,
    last_flow: FrameFlow,
    running: bool,
    error: Option<anyhow::Error>,
}

impl<A: Application + 'static> Runtime<A> {
    fn new(options: ClientOptions, application: A) -> Self {
        Self {
            application,
            compositor: None,
            manager: None,
            appearance: None,
            pending_appearance: None,
            surface: None,
            role: None,
            graphics: None,
            config: None,
            options,
            started: Instant::now(),
            frames: 0,
            frame_pending: false,
            visible: true,
            redraw_pending: false,
            last_flow: FrameFlow::Wait,
            running: true,
            error: None,
        }
    }

    fn maybe_create_surface(&mut self, qh: &QueueHandle<Self>) {
        if self.surface.is_some() {
            return;
        }
        let (Some(compositor), Some(manager)) = (self.compositor.clone(), self.manager.clone())
        else {
            return;
        };
        let surface = compositor.create_surface(qh, ());
        self.appearance = Some(manager.get_appearance(qh, ()));
        let compact = self.options.compact_sizing;
        let expanded = self.options.expanded_sizing.unwrap_or(Sizing::new(0, 0, 0));
        let role = if self.options.surface_role == SurfaceRole::Backdrop {
            manager.get_backdrop_surface(&surface, self.options.plugin_id.clone(), qh, ())
        } else {
            manager.get_item_surface(
                &surface,
                self.options.plugin_id.clone(),
                self.options.item_id.clone(),
                compact.minimum,
                compact.preferred,
                compact.maximum,
                expanded.minimum,
                expanded.preferred,
                expanded.maximum,
                qh,
                (),
            )
        };
        self.surface = Some(surface);
        self.role = Some(role);
    }

    fn configure(
        &mut self,
        connection: &Connection,
        role: &touchbar_surface_v1::TouchbarSurfaceV1,
        serial: u32,
        config: SurfaceConfig,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        if config.width == 0 || config.height == 0 {
            bail!("compositor configured an empty region");
        }
        role.ack_configure(serial);
        // Wayland EGL may submit its wl_surface commit through libwayland
        // immediately. Flush the role acknowledgement first so the compositor
        // always observes configure ordering across the Rust/libwayland bridge.
        connection
            .flush()
            .context("flush configure acknowledgement")?;
        let surface = self.surface.as_ref().context("surface is unavailable")?;
        match self.graphics.as_mut() {
            Some(graphics) => graphics.resize(config.width, config.height),
            None => {
                self.graphics = Some(Graphics::new(
                    connection,
                    surface,
                    config.width,
                    config.height,
                    self.options.require_hardware,
                )?);
            }
        }
        self.config = Some(config);
        self.application
            .configured(self.graphics.as_ref().unwrap(), config)?;
        self.redraw_pending = true;
        if self.visible {
            self.render_and_commit(qh)?;
        }
        Ok(())
    }

    fn render_and_commit(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let surface = self.surface.clone().context("surface is unavailable")?;
        let frame = FrameInfo {
            number: self.frames,
            elapsed_seconds: self.started.elapsed().as_secs_f32(),
            surface: self.config.context("surface is not configured")?,
        };
        let flow = self.application.render(
            self.graphics.as_ref().context("graphics is unavailable")?,
            frame,
        )?;
        self.redraw_pending = false;
        self.last_flow = flow;
        self.send_presentation_request();
        if flow == FrameFlow::Animate && !self.frame_pending {
            surface.frame(qh, ());
            self.frame_pending = true;
        }
        let graphics = self.graphics.as_ref().context("graphics is unavailable")?;
        if let Some(fence) = graphics.export_acquire_fence()? {
            self.role
                .as_ref()
                .context("surface role is unavailable")?
                .set_acquire_fence(fence.as_fd());
        }
        graphics.present()?;
        self.frames += 1;
        if flow == FrameFlow::Exit {
            self.running = false;
        }
        Ok(())
    }

    fn send_presentation_request(&mut self) {
        let Some(role) = &self.role else {
            return;
        };
        for _ in 0..32 {
            let Some(request) = self.application.take_presentation_session_request() else {
                break;
            };
            match request {
                PresentationSessionRequest::Begin {
                    session_id,
                    policy,
                    lifecycle,
                } => {
                    let (policy, target) = encode_session_policy(&policy);
                    let (lifecycle, contact_id) = encode_session_lifecycle(lifecycle);
                    role.begin_presentation(session_id, policy, lifecycle, contact_id, target);
                }
                PresentationSessionRequest::End { session_id, reason } => {
                    role.end_presentation(session_id, encode_dismiss_reason(reason));
                }
            }
        }
    }

    fn touch(&mut self, contact: TouchContact, qh: &QueueHandle<Self>) -> Result<()> {
        let redraw = self.application.touch(contact)?;
        self.send_presentation_request();
        self.redraw_pending |= redraw;
        if self.redraw_pending && self.visible && !self.frame_pending && self.graphics.is_some() {
            self.render_and_commit(qh)?;
        }
        Ok(())
    }

    fn external_event(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let redraw = self.application.external_event()?;
        self.send_presentation_request();
        self.redraw_pending |= redraw;
        if self.redraw_pending && self.visible && !self.frame_pending && self.graphics.is_some() {
            self.render_and_commit(qh)?;
        }
        Ok(())
    }

    fn fail(&mut self, error: anyhow::Error) {
        self.error = Some(error);
        self.running = false;
    }
}

fn encode_session_policy(
    policy: &SessionPresentationPolicy,
) -> (touchbar_surface_v1::PresentationPolicy, String) {
    match policy {
        SessionPresentationPolicy::Anchored => (
            touchbar_surface_v1::PresentationPolicy::Anchored,
            String::new(),
        ),
        SessionPresentationPolicy::InPlace => (
            touchbar_surface_v1::PresentationPolicy::InPlace,
            String::new(),
        ),
        SessionPresentationPolicy::Slot(target) => (
            touchbar_surface_v1::PresentationPolicy::Slot,
            target.clone(),
        ),
        SessionPresentationPolicy::Region(target) => (
            touchbar_surface_v1::PresentationPolicy::Region,
            target.clone(),
        ),
        SessionPresentationPolicy::FullBar => (
            touchbar_surface_v1::PresentationPolicy::FullBar,
            String::new(),
        ),
    }
}

fn encode_session_lifecycle(
    lifecycle: SessionPresentationLifecycle,
) -> (touchbar_surface_v1::PresentationLifecycle, u32) {
    match lifecycle {
        SessionPresentationLifecycle::Transient { contact_id } => (
            touchbar_surface_v1::PresentationLifecycle::Transient,
            contact_id,
        ),
        SessionPresentationLifecycle::Persistent => {
            (touchbar_surface_v1::PresentationLifecycle::Persistent, 0)
        }
    }
}

fn encode_dismiss_reason(reason: PresentationDismissReason) -> touchbar_surface_v1::DismissReason {
    match reason {
        PresentationDismissReason::Requested => touchbar_surface_v1::DismissReason::Requested,
        PresentationDismissReason::Selection => touchbar_surface_v1::DismissReason::Selection,
        PresentationDismissReason::OutsidePress => touchbar_surface_v1::DismissReason::OutsidePress,
        PresentationDismissReason::Timeout => touchbar_surface_v1::DismissReason::Timeout,
        PresentationDismissReason::SourceHidden => touchbar_surface_v1::DismissReason::SourceHidden,
        PresentationDismissReason::Replaced => touchbar_surface_v1::DismissReason::Replaced,
        PresentationDismissReason::Rejected => touchbar_surface_v1::DismissReason::Rejected,
    }
}

fn decode_session_policy<E: std::fmt::Display>(
    policy: std::result::Result<touchbar_surface_v1::PresentationPolicy, E>,
    target: String,
) -> Result<SessionPresentationPolicy> {
    Ok(
        match policy.map_err(|value| anyhow::anyhow!("unknown presentation policy {value}"))? {
            touchbar_surface_v1::PresentationPolicy::Anchored => {
                SessionPresentationPolicy::Anchored
            }
            touchbar_surface_v1::PresentationPolicy::InPlace => SessionPresentationPolicy::InPlace,
            touchbar_surface_v1::PresentationPolicy::Slot => {
                SessionPresentationPolicy::Slot(target)
            }
            touchbar_surface_v1::PresentationPolicy::Region => {
                SessionPresentationPolicy::Region(target)
            }
            touchbar_surface_v1::PresentationPolicy::FullBar => SessionPresentationPolicy::FullBar,
            _ => bail!("unsupported presentation policy"),
        },
    )
}

fn decode_session_lifecycle<E: std::fmt::Display>(
    lifecycle: std::result::Result<touchbar_surface_v1::PresentationLifecycle, E>,
    contact_id: u32,
) -> Result<SessionPresentationLifecycle> {
    Ok(
        match lifecycle
            .map_err(|value| anyhow::anyhow!("unknown presentation lifecycle {value}"))?
        {
            touchbar_surface_v1::PresentationLifecycle::Transient => {
                SessionPresentationLifecycle::Transient { contact_id }
            }
            touchbar_surface_v1::PresentationLifecycle::Persistent => {
                SessionPresentationLifecycle::Persistent
            }
            _ => bail!("unsupported presentation lifecycle"),
        },
    )
}

fn decode_dismiss_reason<E: std::fmt::Display>(
    reason: std::result::Result<touchbar_surface_v1::DismissReason, E>,
) -> Result<PresentationDismissReason> {
    Ok(
        match reason.map_err(|value| anyhow::anyhow!("unknown dismiss reason {value}"))? {
            touchbar_surface_v1::DismissReason::Requested => PresentationDismissReason::Requested,
            touchbar_surface_v1::DismissReason::Selection => PresentationDismissReason::Selection,
            touchbar_surface_v1::DismissReason::OutsidePress => {
                PresentationDismissReason::OutsidePress
            }
            touchbar_surface_v1::DismissReason::Timeout => PresentationDismissReason::Timeout,
            touchbar_surface_v1::DismissReason::SourceHidden => {
                PresentationDismissReason::SourceHidden
            }
            touchbar_surface_v1::DismissReason::Replaced => PresentationDismissReason::Replaced,
            touchbar_surface_v1::DismissReason::Rejected => PresentationDismissReason::Rejected,
            _ => bail!("unsupported presentation dismiss reason"),
        },
    )
}

fn decode_contact_origin<E: std::fmt::Display>(
    origin: std::result::Result<touchbar_surface_v1::InputOrigin, E>,
) -> Result<ContactOrigin> {
    match origin.map_err(|value| anyhow::anyhow!("unknown Touch Bar input origin {value}"))? {
        touchbar_surface_v1::InputOrigin::Physical => Ok(ContactOrigin::Physical),
        touchbar_surface_v1::InputOrigin::Synthetic => Ok(ContactOrigin::Synthetic),
        _ => bail!("unsupported Touch Bar input origin"),
    }
}

impl<A: Application + 'static> Dispatch<wl_registry::WlRegistry, ()> for Runtime<A> {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "touchbar_manager_v1" => {
                    state.manager =
                        Some(registry.bind(name, version.min(TOUCHBAR_PROTOCOL_VERSION), qh, ()))
                }
                _ => {}
            }
            state.maybe_create_surface(qh);
        }
    }
}

delegate_noop!(@<A: Application + 'static> Runtime<A>: ignore wl_compositor::WlCompositor);
delegate_noop!(@<A: Application + 'static> Runtime<A>: ignore wl_surface::WlSurface);
delegate_noop!(@<A: Application + 'static> Runtime<A>: ignore touchbar_manager_v1::TouchbarManagerV1);

impl<A: Application + 'static> Dispatch<touchbar_appearance_v1::TouchbarAppearanceV1, ()>
    for Runtime<A>
{
    fn event(
        state: &mut Self,
        _resource: &touchbar_appearance_v1::TouchbarAppearanceV1,
        event: touchbar_appearance_v1::Event,
        _data: &(),
        _connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let result = (|| -> Result<()> {
            match event {
                touchbar_appearance_v1::Event::Begin {
                    generation,
                    scheme,
                    motion,
                    corner_radius_millipixels,
                } => {
                    let scheme = match scheme.into_result() {
                        Ok(touchbar_appearance_v1::Scheme::Dark) => ColorScheme::Dark,
                        Ok(touchbar_appearance_v1::Scheme::Light) => ColorScheme::Light,
                        Ok(_) | Err(_) => {
                            return Err(anyhow::anyhow!("unknown appearance color scheme"));
                        }
                    };
                    let motion = match motion.into_result() {
                        Ok(touchbar_appearance_v1::MotionPolicy::Full) => MotionPolicy::Full,
                        Ok(touchbar_appearance_v1::MotionPolicy::Reduced) => MotionPolicy::Reduced,
                        Ok(touchbar_appearance_v1::MotionPolicy::Disabled) => {
                            MotionPolicy::Disabled
                        }
                        Ok(_) | Err(_) => {
                            return Err(anyhow::anyhow!("unknown appearance motion policy"));
                        }
                    };
                    state.pending_appearance = Some((
                        AppearanceSnapshot {
                            generation,
                            scheme,
                            motion,
                            corner_radius_millipixels,
                            ..AppearanceSnapshot::default()
                        },
                        0,
                    ));
                    Ok(())
                }
                touchbar_appearance_v1::Event::Color {
                    generation,
                    role,
                    rgba,
                } => {
                    let (snapshot, seen) = state
                        .pending_appearance
                        .as_mut()
                        .context("appearance color arrived before begin")?;
                    if snapshot.generation != generation {
                        bail!("appearance color generation does not match begin");
                    }
                    let role = match role
                        .into_result()
                        .context("appearance contains an unknown color role")?
                    {
                        touchbar_appearance_v1::ColorRole::Background => ColorRole::Background,
                        touchbar_appearance_v1::ColorRole::Surface => ColorRole::Surface,
                        touchbar_appearance_v1::ColorRole::SurfaceHover => ColorRole::SurfaceHover,
                        touchbar_appearance_v1::ColorRole::SurfacePressed => {
                            ColorRole::SurfacePressed
                        }
                        touchbar_appearance_v1::ColorRole::Foreground => ColorRole::Foreground,
                        touchbar_appearance_v1::ColorRole::Muted => ColorRole::Muted,
                        touchbar_appearance_v1::ColorRole::Accent => ColorRole::Accent,
                        touchbar_appearance_v1::ColorRole::Destructive => ColorRole::Destructive,
                        _ => return Err(anyhow::anyhow!("unknown appearance color role")),
                    };
                    snapshot.set_color(role, Rgba8::from_packed(rgba));
                    *seen |= 1 << role as u32;
                    Ok(())
                }
                touchbar_appearance_v1::Event::Done { generation } => {
                    let (snapshot, seen) = state
                        .pending_appearance
                        .take()
                        .context("appearance done arrived before begin")?;
                    if snapshot.generation != generation || seen != u8::MAX {
                        bail!("appearance snapshot is incomplete");
                    }
                    let redraw = state.application.appearance_changed(snapshot)?;
                    state.redraw_pending |= redraw;
                    if state.redraw_pending
                        && state.visible
                        && !state.frame_pending
                        && state.graphics.is_some()
                    {
                        state.render_and_commit(qh)?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        })();
        if let Err(error) = result {
            state.fail(error);
        }
    }
}

impl<A: Application + 'static> Dispatch<touchbar_surface_v1::TouchbarSurfaceV1, ()> for Runtime<A> {
    fn event(
        state: &mut Self,
        role: &touchbar_surface_v1::TouchbarSurfaceV1,
        event: touchbar_surface_v1::Event,
        _data: &(),
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            touchbar_surface_v1::Event::Configure {
                serial,
                width,
                height,
                refresh_millihz,
            } => {
                let config = SurfaceConfig {
                    width,
                    height,
                    refresh_millihz,
                };
                if let Err(error) = state.configure(connection, role, serial, config, qh) {
                    state.fail(error);
                }
            }
            touchbar_surface_v1::Event::Visibility { visible } => {
                let visible = visible != 0;
                state.visible = visible;
                match state.application.visibility_changed(visible) {
                    Ok(redraw) => {
                        state.redraw_pending |= redraw;
                        if visible && state.last_flow == FrameFlow::Animate {
                            state.redraw_pending = true;
                        }
                        if state.redraw_pending
                            && !state.frame_pending
                            && state.graphics.is_some()
                            && let Err(error) = state.render_and_commit(qh)
                        {
                            state.fail(error);
                        }
                    }
                    Err(error) => state.fail(error),
                }
            }
            touchbar_surface_v1::Event::TouchDown {
                time,
                contact_id,
                x,
                y,
                sequence_hi,
                sequence_lo,
                origin,
            } => {
                let origin = match decode_contact_origin(origin.into_result()) {
                    Ok(origin) => origin,
                    Err(error) => return state.fail(error),
                };
                let contact = TouchContact {
                    id: contact_id,
                    phase: ContactPhase::Down,
                    x: x as f32,
                    y: y as f32,
                    time: state.started.elapsed(),
                    compositor_time: std::time::Duration::from_millis(u64::from(time)),
                    input_sequence: join_input_sequence(sequence_hi, sequence_lo)
                        .expect("the compositor emitted a zero input sequence"),
                    origin,
                };
                if let Err(error) = state.touch(contact, qh) {
                    state.fail(error);
                }
            }
            touchbar_surface_v1::Event::TouchMotion {
                time,
                contact_id,
                x,
                y,
                sequence_hi,
                sequence_lo,
                origin,
            } => {
                let origin = match decode_contact_origin(origin.into_result()) {
                    Ok(origin) => origin,
                    Err(error) => return state.fail(error),
                };
                let contact = TouchContact {
                    id: contact_id,
                    phase: ContactPhase::Motion,
                    x: x as f32,
                    y: y as f32,
                    time: state.started.elapsed(),
                    compositor_time: std::time::Duration::from_millis(u64::from(time)),
                    input_sequence: join_input_sequence(sequence_hi, sequence_lo)
                        .expect("the compositor emitted a zero input sequence"),
                    origin,
                };
                if let Err(error) = state.touch(contact, qh) {
                    state.fail(error);
                }
            }
            touchbar_surface_v1::Event::TouchUp {
                time,
                contact_id,
                x,
                y,
                sequence_hi,
                sequence_lo,
                origin,
            } => {
                let origin = match decode_contact_origin(origin.into_result()) {
                    Ok(origin) => origin,
                    Err(error) => return state.fail(error),
                };
                let contact = TouchContact {
                    id: contact_id,
                    phase: ContactPhase::Up,
                    x: x as f32,
                    y: y as f32,
                    time: state.started.elapsed(),
                    compositor_time: std::time::Duration::from_millis(u64::from(time)),
                    input_sequence: join_input_sequence(sequence_hi, sequence_lo)
                        .expect("the compositor emitted a zero input sequence"),
                    origin,
                };
                if let Err(error) = state.touch(contact, qh) {
                    state.fail(error);
                }
            }
            touchbar_surface_v1::Event::TouchCancel {
                time,
                contact_id,
                sequence_hi,
                sequence_lo,
                origin,
            } => {
                let origin = match decode_contact_origin(origin.into_result()) {
                    Ok(origin) => origin,
                    Err(error) => return state.fail(error),
                };
                let contact = TouchContact {
                    id: contact_id,
                    phase: ContactPhase::Cancel,
                    x: 0.0,
                    y: 0.0,
                    time: state.started.elapsed(),
                    compositor_time: std::time::Duration::from_millis(u64::from(time)),
                    input_sequence: join_input_sequence(sequence_hi, sequence_lo)
                        .expect("the compositor emitted a zero input sequence"),
                    origin,
                };
                if let Err(error) = state.touch(contact, qh) {
                    state.fail(error);
                }
            }
            touchbar_surface_v1::Event::PresentationAnchor {
                session_id,
                x,
                width,
            } => {
                if let Err(error) = state.application.presentation_session_changed(
                    PresentationSessionEvent::Anchor {
                        session_id,
                        anchor: PopoverAnchor {
                            x: x as f32,
                            width: width as f32,
                        },
                    },
                ) {
                    state.fail(error);
                }
            }
            touchbar_surface_v1::Event::PresentationStarted {
                session_id,
                policy,
                lifecycle,
                contact_id,
                target,
            } => {
                let result =
                    decode_session_policy(policy.into_result(), target).and_then(|policy| {
                        decode_session_lifecycle(lifecycle.into_result(), contact_id).map(
                            |lifecycle| PresentationSessionEvent::Started {
                                session_id,
                                policy,
                                lifecycle,
                            },
                        )
                    });
                match result.and_then(|event| state.application.presentation_session_changed(event))
                {
                    Ok(()) => {}
                    Err(error) => state.fail(error),
                }
            }
            touchbar_surface_v1::Event::PresentationEnded { session_id, reason } => {
                let result = decode_dismiss_reason(reason.into_result()).and_then(|reason| {
                    state.application.presentation_session_changed(
                        PresentationSessionEvent::Ended { session_id, reason },
                    )
                });
                if let Err(error) = result {
                    state.fail(error);
                }
            }
            _ => {}
        }
    }
}

impl<A: Application + 'static> Dispatch<wl_callback::WlCallback, ()> for Runtime<A> {
    fn event(
        state: &mut Self,
        _callback: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_pending = false;
            if state.visible
                && (state.last_flow == FrameFlow::Animate || state.redraw_pending)
                && let Err(error) = state.render_and_commit(qh)
            {
                state.fail(error);
            }
        }
    }
}

/// Connect to `touchbar-sessiond` and run an application until it requests exit or
/// the compositor disconnects.
pub fn run<A: Application + 'static>(options: ClientOptions, application: A) -> Result<RunSummary> {
    if options.surface_role == SurfaceRole::Item && !options.compact_sizing.is_valid() {
        bail!("compact sizing must satisfy 0 < minimum <= preferred <= maximum");
    }
    if options.surface_role == SurfaceRole::Item
        && options
            .expanded_sizing
            .is_some_and(|sizing| !sizing.is_valid())
    {
        bail!("expanded sizing must satisfy 0 < minimum <= preferred <= maximum");
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        // SAFETY: the SDK is single-threaded and does this before constructing
        // the application event loop or starting any worker threads.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &options.default_socket_name);
        }
    }
    let connection = Connection::connect_to_env().context("connect to touchbar-sessiond")?;
    let mut event_queue = connection.new_event_queue();
    let qh = event_queue.handle();
    connection.display().get_registry(&qh, ());

    let mut runtime = Runtime::new(options, application);
    while runtime.running {
        let Some(external_fd) = runtime.application.external_event_fd() else {
            event_queue
                .blocking_dispatch(&mut runtime)
                .context("dispatch Wayland event")?;
            connection.flush().context("flush Wayland requests")?;
            continue;
        };

        event_queue
            .dispatch_pending(&mut runtime)
            .context("dispatch pending Wayland events")?;
        // Event handlers may enqueue binds, surface-role requests, commits,
        // and frame callbacks. Flush after dispatch so those requests reach
        // the compositor before this thread waits on Wayland and broker FDs.
        event_queue.flush().context("flush Wayland requests")?;
        if !runtime.running {
            continue;
        }
        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };
        let mut descriptors = [
            libc::pollfd {
                fd: read_guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: external_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: descriptors is valid writable storage for two pollfd values.
            let result =
                unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
            if result >= 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("poll Wayland and application event sources");
            }
        }
        if descriptors[0].revents != 0 {
            read_guard.read().context("read Wayland events")?;
        } else {
            drop(read_guard);
        }
        if descriptors[1].revents != 0 {
            runtime
                .external_event(&qh)
                .context("dispatch application event source")?;
        }
    }
    if let Some(error) = runtime.error {
        return Err(error);
    }
    let renderer_name = runtime
        .graphics
        .as_ref()
        .map(|graphics| graphics.renderer_name.clone())
        .unwrap_or_default();
    Ok(RunSummary {
        plugin_id: runtime.options.plugin_id,
        frames: runtime.frames,
        renderer_name,
    })
}
