use std::{
    collections::{BTreeMap, VecDeque},
    io,
    os::fd::{AsFd, AsRawFd, RawFd},
};

use anyhow::{Context, Result, bail};
use memmap2::{MmapMut, MmapOptions};
use touchbar_protocol::hardware_ipc::{TouchEvent, TouchPhase};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat,
        wl_shm, wl_shm_pool, wl_surface, wl_touch,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const BUFFER_COUNT: usize = 3;
const POINTER_CONTACT_ID: u32 = u32::MAX;
const TOUCH_CONTACT_NAMESPACE: u32 = 1 << 31;
const BTN_LEFT: u32 = 0x110;
const KEY_ESC: u32 = 1;
const KEY_F: u32 = 33;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewInput {
    Touch(TouchEvent),
    FnChanged(bool),
}

#[derive(Clone, Copy, Debug)]
struct BufferData(usize);

struct PreviewState {
    width: u32,
    height: u32,
    display_scale: u32,
    compositor: Option<wl_compositor::WlCompositor>,
    surface: Option<wl_surface::WlSurface>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    pointer: Option<wl_pointer::WlPointer>,
    touch: Option<wl_touch::WlTouch>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    buffers: Vec<wl_buffer::WlBuffer>,
    busy: Vec<bool>,
    map: Option<MmapMut>,
    configured: bool,
    frame_pending: bool,
    pending_frame: Option<Vec<u8>>,
    inputs: VecDeque<PreviewInput>,
    pointer_position: Option<(f64, f64)>,
    pointer_down: bool,
    touch_positions: BTreeMap<i32, (f64, f64, u32)>,
    fn_pressed: bool,
    closed: bool,
    setup_error: Option<String>,
}

impl PreviewState {
    fn new(width: u32, height: u32, display_scale: u32) -> Self {
        Self {
            width,
            height,
            display_scale,
            compositor: None,
            surface: None,
            shm: None,
            wm_base: None,
            xdg_surface: None,
            toplevel: None,
            pointer: None,
            touch: None,
            keyboard: None,
            buffers: Vec::new(),
            busy: vec![false; BUFFER_COUNT],
            map: None,
            configured: false,
            frame_pending: false,
            pending_frame: None,
            inputs: VecDeque::new(),
            pointer_position: None,
            pointer_down: false,
            touch_positions: BTreeMap::new(),
            fn_pressed: false,
            closed: false,
            setup_error: None,
        }
    }

    fn initialize_surface(&mut self, qh: &QueueHandle<Self>) {
        if self.surface.is_some() {
            return;
        }
        let (Some(compositor), Some(wm_base)) = (&self.compositor, &self.wm_base) else {
            return;
        };
        let surface = compositor.create_surface(qh, ());
        surface.set_buffer_scale(self.display_scale as i32);
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());
        let logical_width = (self.width / self.display_scale) as i32;
        let logical_height = (self.height / self.display_scale) as i32;
        toplevel.set_title("TouchBar Preview".into());
        toplevel.set_app_id("io.github.cameroncooper.touchbar.preview".into());
        toplevel.set_min_size(logical_width, logical_height);
        toplevel.set_max_size(logical_width, logical_height);
        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
        if let Err(error) = self.initialize_buffers(qh) {
            self.setup_error = Some(format!("{error:#}"));
        }
    }

    fn initialize_buffers(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        if !self.buffers.is_empty() {
            return Ok(());
        }
        let Some(shm) = &self.shm else {
            return Ok(());
        };
        let frame_bytes = self.width as usize * self.height as usize * 4;
        let total_bytes = frame_bytes * BUFFER_COUNT;
        let pool_size = i32::try_from(total_bytes).context("preview buffer pool exceeds i32")?;
        let file = tempfile::tempfile().context("create preview shared-memory file")?;
        file.set_len(total_bytes as u64)
            .context("size preview shared-memory file")?;
        // SAFETY: this process owns the newly sized file and keeps its mapping
        // alive for at least as long as every wl_buffer created from it.
        let map = unsafe { MmapOptions::new().map_mut(&file) }
            .context("map preview shared-memory file")?;
        let pool = shm.create_pool(file.as_fd(), pool_size, qh, ());
        for index in 0..BUFFER_COUNT {
            self.buffers.push(pool.create_buffer(
                (index * frame_bytes) as i32,
                self.width as i32,
                self.height as i32,
                (self.width * 4) as i32,
                wl_shm::Format::Argb8888,
                qh,
                BufferData(index),
            ));
        }
        pool.destroy();
        self.map = Some(map);
        Ok(())
    }

    fn queue_frame(&mut self, scene_rgba: &[u8], qh: &QueueHandle<Self>) -> Result<()> {
        let expected = self.width as usize * self.height as usize * 4;
        if scene_rgba.len() != expected {
            bail!(
                "preview frame has {} bytes; expected {expected}",
                scene_rgba.len()
            );
        }
        self.pending_frame = Some(scene_rgba.to_vec());
        self.submit_pending(qh);
        Ok(())
    }

    fn submit_pending(&mut self, qh: &QueueHandle<Self>) {
        if !self.configured || self.frame_pending {
            return;
        }
        let Some(index) = self.busy.iter().position(|busy| !busy) else {
            return;
        };
        let Some(source) = self.pending_frame.take() else {
            return;
        };
        let frame_bytes = self.width as usize * self.height as usize * 4;
        let start = index * frame_bytes;
        let Some(map) = &mut self.map else {
            self.pending_frame = Some(source);
            return;
        };
        rgba_to_argb(
            &source,
            &mut map[start..start + frame_bytes],
            self.width,
            self.height,
        );
        let Some(surface) = &self.surface else {
            self.pending_frame = Some(source);
            return;
        };
        surface.attach(Some(&self.buffers[index]), 0, 0);
        surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
        surface.frame(qh, ());
        surface.commit();
        self.busy[index] = true;
        self.frame_pending = true;
    }

    fn queue_pointer(&mut self, phase: TouchPhase, time_ms: u32) {
        let Some((x, y)) = self.pointer_position else {
            return;
        };
        self.inputs.push_back(PreviewInput::Touch(touch_event(
            phase,
            POINTER_CONTACT_ID,
            time_ms,
            x,
            y,
            self.display_scale,
            self.width,
            self.height,
        )));
    }

    fn cancel_inputs(&mut self) {
        if self.pointer_down {
            self.queue_pointer(TouchPhase::Cancel, 0);
            self.pointer_down = false;
        }
        for (id, (x, y, time_ms)) in std::mem::take(&mut self.touch_positions) {
            if let Some(id) = preview_touch_id(id) {
                self.inputs.push_back(PreviewInput::Touch(touch_event(
                    TouchPhase::Cancel,
                    id,
                    time_ms,
                    x,
                    y,
                    self.display_scale,
                    self.width,
                    self.height,
                )));
            }
        }
        if self.fn_pressed {
            self.inputs.push_back(PreviewInput::FnChanged(false));
            self.fn_pressed = false;
        }
    }
}

pub struct PreviewOutput {
    event_queue: EventQueue<PreviewState>,
    state: PreviewState,
}

impl PreviewOutput {
    pub fn connect(width: u32, height: u32, display_scale: u32) -> Result<Self> {
        validate_display_scale(width, height, display_scale)?;
        let connection =
            Connection::connect_to_env().context("connect preview to desktop Wayland")?;
        let mut event_queue = connection.new_event_queue();
        let qh = event_queue.handle();
        connection.display().get_registry(&qh, ());
        let mut state = PreviewState::new(width, height, display_scale);
        event_queue
            .roundtrip(&mut state)
            .context("discover desktop Wayland globals")?;
        event_queue
            .roundtrip(&mut state)
            .context("configure desktop TouchBar preview")?;
        if let Some(error) = state.setup_error.take() {
            bail!("initialize desktop TouchBar preview: {error}");
        }
        if state.surface.is_none() || state.shm.is_none() || state.xdg_surface.is_none() {
            bail!("desktop compositor is missing wl_compositor, wl_shm, or xdg_wm_base");
        }
        println!(
            "preview-output=ready buffer={}x{} window={}x{} scale={} input=mouse,touch,f-key",
            width,
            height,
            width / display_scale,
            height / display_scale,
            display_scale
        );
        Ok(Self { event_queue, state })
    }

    pub fn notification_fd(&self) -> RawFd {
        self.event_queue.as_fd().as_raw_fd()
    }

    pub fn pump(&mut self) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context("dispatch pending preview events")?;
        if let Some(guard) = self.event_queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("read desktop Wayland events"),
            }
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context("dispatch preview events")?;
        match self.event_queue.flush() {
            Ok(()) => Ok(()),
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == io::ErrorKind::WouldBlock =>
            {
                Ok(())
            }
            Err(error) => Err(error).context("flush desktop Wayland preview"),
        }
    }

    pub fn present(&mut self, scene_rgba: &[u8]) -> Result<()> {
        let qh = self.event_queue.handle();
        self.state.queue_frame(scene_rgba, &qh)?;
        self.event_queue
            .flush()
            .context("flush preview frame to desktop Wayland")
    }

    pub fn drain_input(&mut self) -> Vec<PreviewInput> {
        self.state.inputs.drain(..).collect()
    }

    pub fn closed(&self) -> bool {
        self.state.closed
    }
}

fn validate_display_scale(width: u32, height: u32, scale: u32) -> Result<()> {
    if !matches!(scale, 1 | 2 | 4) {
        bail!("preview display scale must be 1, 2, or 4");
    }
    if !width.is_multiple_of(scale) || !height.is_multiple_of(scale) {
        bail!("preview dimensions must be divisible by display scale {scale}");
    }
    Ok(())
}

fn logical_millipixels(value: f64, scale: u32, maximum: u32) -> i32 {
    let maximum = f64::from(maximum).max(1.0) - 0.001;
    (value.mul_add(f64::from(scale), 0.0).clamp(0.0, maximum) * 1000.0).round() as i32
}

#[allow(clippy::too_many_arguments)]
fn touch_event(
    phase: TouchPhase,
    contact_id: u32,
    time_ms: u32,
    x: f64,
    y: f64,
    scale: u32,
    width: u32,
    height: u32,
) -> TouchEvent {
    TouchEvent {
        phase,
        contact_id,
        time_ms,
        x_millipixels: logical_millipixels(x, scale, width),
        y_millipixels: logical_millipixels(y, scale, height),
    }
}

fn preview_touch_id(id: i32) -> Option<u32> {
    let id = u32::try_from(id).ok()?;
    (id < TOUCH_CONTACT_NAMESPACE - 1).then_some(TOUCH_CONTACT_NAMESPACE | id)
}

/// The scene framebuffer is top-down: the compositor's blit chain maps
/// framebuffer row 0 to texel row 0 at every hop, so the scene inherits the
/// client buffer's row order rather than GL's bottom-up convention. Only the
/// channel order changes here. (The ADP output blit's transpose-and-flip is the
/// panel's +90-degree rotation, not an origin correction, so it is unrelated.)
fn rgba_to_argb(source: &[u8], target: &mut [u8], width: u32, height: u32) {
    let stride = width as usize * 4;
    debug_assert_eq!(source.len(), stride * height as usize);
    debug_assert_eq!(target.len(), source.len());
    for y in 0..height as usize {
        let source_row = &source[y * stride..(y + 1) * stride];
        let target_row = &mut target[y * stride..(y + 1) * stride];
        let (source_pixels, []) = source_row.as_chunks::<4>() else {
            unreachable!("RGBA rows are four-byte aligned")
        };
        let (target_pixels, []) = target_row.as_chunks_mut::<4>() else {
            unreachable!("ARGB rows are four-byte aligned")
        };
        for (rgba, bgra) in source_pixels.iter().zip(target_pixels) {
            bgra.copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for PreviewState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_shm" => {
                state.shm = Some(registry.bind(name, 1, qh, ()));
                if let Err(error) = state.initialize_buffers(qh) {
                    state.setup_error = Some(format!("{error:#}"));
                }
            }
            "wl_seat" => {
                registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
            }
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, 1, qh, ())),
            _ => return,
        }
        state.initialize_surface(qh);
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for PreviewState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for PreviewState {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
            state.submit_pending(qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for PreviewState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.cancel_inputs();
            state.closed = true;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for PreviewState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &BufferData,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.busy[data.0] = false;
            state.submit_pending(qh);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for PreviewState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_pending = false;
            state.submit_pending(qh);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for PreviewState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Touch) && state.touch.is_none() {
                state.touch = Some(seat.get_touch(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for PreviewState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_position = Some((surface_x, surface_y));
                if state.pointer_down && matches!(event, wl_pointer::Event::Motion { .. }) {
                    let wl_pointer::Event::Motion { time, .. } = event else {
                        unreachable!()
                    };
                    state.queue_pointer(TouchPhase::Motion, time);
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if !state.pointer_down {
                    state.pointer_position = None;
                }
            }
            wl_pointer::Event::Button {
                time,
                button: BTN_LEFT,
                state: WEnum::Value(button_state),
                ..
            } => match button_state {
                wl_pointer::ButtonState::Pressed if !state.pointer_down => {
                    state.pointer_down = true;
                    state.queue_pointer(TouchPhase::Down, time);
                }
                wl_pointer::ButtonState::Released if state.pointer_down => {
                    state.queue_pointer(TouchPhase::Up, time);
                    state.pointer_down = false;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for PreviewState {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_touch::Event::Down { time, id, x, y, .. } => {
                let Some(contact_id) = preview_touch_id(id) else {
                    return;
                };
                state.touch_positions.insert(id, (x, y, time));
                state.inputs.push_back(PreviewInput::Touch(touch_event(
                    TouchPhase::Down,
                    contact_id,
                    time,
                    x,
                    y,
                    state.display_scale,
                    state.width,
                    state.height,
                )));
            }
            wl_touch::Event::Motion { time, id, x, y } => {
                let Some(contact_id) = preview_touch_id(id) else {
                    return;
                };
                let Some(position) = state.touch_positions.get_mut(&id) else {
                    return;
                };
                *position = (x, y, time);
                state.inputs.push_back(PreviewInput::Touch(touch_event(
                    TouchPhase::Motion,
                    contact_id,
                    time,
                    x,
                    y,
                    state.display_scale,
                    state.width,
                    state.height,
                )));
            }
            wl_touch::Event::Up { time, id, .. } => {
                let Some(contact_id) = preview_touch_id(id) else {
                    return;
                };
                let Some((x, y, _)) = state.touch_positions.remove(&id) else {
                    return;
                };
                state.inputs.push_back(PreviewInput::Touch(touch_event(
                    TouchPhase::Up,
                    contact_id,
                    time,
                    x,
                    y,
                    state.display_scale,
                    state.width,
                    state.height,
                )));
            }
            wl_touch::Event::Cancel => state.cancel_inputs(),
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for PreviewState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(key_state),
            ..
        } = event
        {
            let pressed = key_state == wl_keyboard::KeyState::Pressed;
            if key == KEY_ESC && pressed {
                state.cancel_inputs();
                state.closed = true;
            } else if key == KEY_F && state.fn_pressed != pressed {
                state.fn_pressed = pressed;
                state.inputs.push_back(PreviewInput::FnChanged(pressed));
            }
        }
    }
}

delegate_noop!(PreviewState: ignore wl_compositor::WlCompositor);
delegate_noop!(PreviewState: ignore wl_surface::WlSurface);
delegate_noop!(PreviewState: ignore wl_shm::WlShm);
delegate_noop!(PreviewState: ignore wl_shm_pool::WlShmPool);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_top_down_rgba_to_wayland_argb_without_reordering_rows() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, // top row
            9, 10, 11, 12, 13, 14, 15, 16, // bottom row
        ];
        let mut target = [0; 16];
        rgba_to_argb(&source, &mut target, 2, 2);
        assert_eq!(
            target,
            [3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );
    }

    #[test]
    fn display_coordinates_map_back_to_touchbar_pixels() {
        let event = touch_event(TouchPhase::Down, 4, 20, 500.25, 12.5, 2, 2008, 60);
        assert_eq!(event.x_millipixels, 1_000_500);
        assert_eq!(event.y_millipixels, 25_000);
    }

    #[test]
    fn coordinates_are_clamped_to_the_touchbar() {
        assert_eq!(logical_millipixels(-2.0, 2, 2008), 0);
        assert_eq!(logical_millipixels(3000.0, 2, 2008), 2_007_999);
    }

    #[test]
    fn only_exact_divisible_display_scales_are_accepted() {
        for scale in [1, 2, 4] {
            validate_display_scale(2008, 60, scale).unwrap();
        }
        for scale in [0, 3, 5] {
            assert!(validate_display_scale(2008, 60, scale).is_err());
        }
    }

    #[test]
    fn native_touch_ids_use_a_namespace_disjoint_from_hardware_contacts() {
        assert_eq!(preview_touch_id(0), Some(1 << 31));
        assert_eq!(preview_touch_id(42), Some((1 << 31) | 42));
        assert_eq!(preview_touch_id(-1), None);
        assert_ne!(preview_touch_id(i32::MAX), Some(POINTER_CONTACT_ID));
    }
}
