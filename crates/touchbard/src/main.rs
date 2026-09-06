use std::{
    collections::BTreeSet,
    error::Error,
    ffi::OsStr,
    fmt,
    fs::{File, OpenOptions},
    io::{self, BufReader},
    mem,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    os::unix::fs::{FileTypeExt, OpenOptionsExt},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering, fence},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use drm::{
    ClientCapability, Device as BasicDevice, DriverCapability, VblankWaitFlags, VblankWaitTarget,
    buffer::{Buffer, DrmFourcc},
    control::{
        AtomicCommitFlags, ClipRect, Device as ControlDevice, Mode, ResourceHandle, atomic,
        connector, crtc, dumbbuffer::DumbBuffer, framebuffer, plane, property,
    },
};
use memmap2::{Mmap, MmapOptions};
use touchbar_protocol::{
    DEFAULT_REGION_WIDTH, FRAME_STREAM_ACTIVE_SLOT_OFFSET, FRAME_STREAM_FLAG_BOTTOM_UP,
    FRAME_STREAM_FLAGS_OFFSET, FRAME_STREAM_HEADER_SIZE, FRAME_STREAM_MAGIC,
    FRAME_STREAM_SEQUENCE_OFFSET, FRAME_STREAM_SLOT_COUNT,
    hardware_ipc::{
        HardwareMessage, HardwareSwapchain, SessionMessage, TouchEvent, TouchPhase,
        receive_session_message, send_hardware_message, send_hardware_swapchain,
    },
};
use touchbar_system_bar::{SystemBar, SystemBarConfig, SystemBarRenderer};

mod backlight;
mod fn_input;
mod prime_egl;
mod recovery;
mod seat_activity;
mod session_auth;
mod uinput;

use backlight::TouchBarBacklight;
use fn_input::FnInput;
use recovery::{RecoveryAction, RecoveryGesture, RecoveryMarker};
use seat_activity::SeatActivity;
use session_auth::{active_seat_uid, peer_uid};
use uinput::VirtualKeyboard;

const DEFAULT_LOGO: &str = "/usr/share/touchbar/icon.png";
const DISPLAY_CONFIRMATION: &str = "TOUCHBAR_PHYSICAL_DEMO";
const MAX_DEMO_SECONDS: u64 = 30;
const LOGO_SIZE: u32 = 48;
const DEFAULT_HARDWARE_SOCKET: &str = "/run/touchbar/hardware.sock";
const BACKLIGHT_DIM_TIMEOUT: Duration = Duration::from_secs(30);
const BACKLIGHT_OFF_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_FAILURE_EVENTS: i16 = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;

#[derive(Debug)]
struct HardwareDeviceLost {
    device: &'static str,
    events: i16,
}

impl fmt::Display for HardwareDeviceLost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} became unavailable (poll events {:#x})",
            self.device, self.events
        )
    }
}

impl Error for HardwareDeviceLost {}

fn require_hardware_fd(revents: i16, device: &'static str) -> Result<()> {
    if revents & POLL_FAILURE_EVENTS != 0 {
        return Err(HardwareDeviceLost {
            device,
            events: revents,
        }
        .into());
    }
    Ok(())
}

struct ShutdownFd {
    fd: OwnedFd,
    mask: libc::sigset_t,
}

impl ShutdownFd {
    fn install() -> Result<Self> {
        let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        if unsafe { libc::sigemptyset(&mut mask) } != 0
            || unsafe { libc::sigaddset(&mut mask, libc::SIGTERM) } != 0
            || unsafe { libc::sigaddset(&mut mask, libc::SIGINT) } != 0
        {
            return Err(io::Error::last_os_error()).context("build shutdown signal set");
        }
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result)).context("block shutdown signals");
        }
        let raw = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
            }
            return Err(error).context("create shutdown signalfd");
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            mask,
        })
    }

    fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    fn consume(&self) -> Result<bool> {
        let mut info = unsafe { std::mem::zeroed::<libc::signalfd_siginfo>() };
        let read = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                (&mut info as *mut libc::signalfd_siginfo).cast(),
                std::mem::size_of::<libc::signalfd_siginfo>(),
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(false);
            }
            return Err(error).context("read shutdown signalfd");
        }
        if read as usize != std::mem::size_of::<libc::signalfd_siginfo>() {
            bail!("shutdown signalfd returned a partial record");
        }
        Ok(matches!(
            info.ssi_signo as i32,
            libc::SIGTERM | libc::SIGINT
        ))
    }
}

impl Drop for ShutdownFd {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &self.mask, std::ptr::null_mut());
        }
    }
}

struct ServiceSocketCleanup(PathBuf);

impl Drop for ServiceSocketCleanup {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.0).is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[derive(Debug)]
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BasicDevice for Card {}
impl ControlDevice for Card {}

impl Card {
    fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(writable);
        Ok(Self(options.open(path).with_context(|| {
            format!("open DRM node {}", path.display())
        })?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Serve,
    SysfsProbe,
    KmsProbe,
    Display,
    Animate,
    Scene,
    StreamProbe,
    PrimeProbe,
    PrimeServe,
    Direct,
}

struct Args {
    action: Action,
    logo: PathBuf,
    scene: Option<PathBuf>,
    socket: Option<PathBuf>,
    duration: Duration,
}

struct Selection {
    connector: connector::Info,
    crtc: crtc::Handle,
    crtc_index: u32,
    plane: plane::Handle,
    mode: Mode,
}

struct RgbaImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

struct FrameStream {
    map: Mmap,
    width: u32,
    height: u32,
    stride: usize,
    frame_bytes: usize,
    bottom_up: bool,
}

const EV_SYN: u16 = 0;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxInputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[derive(Clone, Copy, Default)]
struct TouchSlot {
    contact_id: Option<u32>,
    x: i32,
    y: i32,
    pending: Option<TouchPhase>,
}

struct TouchInput {
    file: File,
    path: PathBuf,
    x_axis: InputAbsInfo,
    y_axis: InputAbsInfo,
    current_slot: usize,
    slots: Vec<TouchSlot>,
    started: Instant,
    display_width: u32,
    display_height: u32,
    scene_width: u32,
    scene_height: u32,
}

impl TouchInput {
    fn open(
        display_width: u32,
        display_height: u32,
        scene_width: u32,
        scene_height: u32,
    ) -> Result<Self> {
        let path = find_touchbar_input()?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("open Touch Bar input {}", path.display()))?;
        let x_axis = query_abs_axis(&file, ABS_MT_POSITION_X)?;
        let y_axis = query_abs_axis(&file, ABS_MT_POSITION_Y)?;
        let slot_axis = query_abs_axis(&file, ABS_MT_SLOT)?;
        let slot_count = usize::try_from(slot_axis.maximum - slot_axis.minimum + 1)
            .context("Touch Bar reported an invalid multitouch slot range")?;
        if slot_count == 0 || slot_count > 1024 {
            bail!("Touch Bar reported an unsupported slot count: {slot_count}");
        }
        println!(
            "touch-input=ready device={} x={}..{} y={}..{} slots={slot_count}",
            path.display(),
            x_axis.minimum,
            x_axis.maximum,
            y_axis.minimum,
            y_axis.maximum
        );
        Ok(Self {
            file,
            path,
            x_axis,
            y_axis,
            current_slot: 0,
            slots: vec![TouchSlot::default(); slot_count],
            started: Instant::now(),
            display_width,
            display_height,
            scene_width,
            scene_height,
        })
    }

    fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    fn read_events(&mut self) -> Result<Vec<TouchEvent>> {
        let mut output = Vec::new();
        let mut events = [LinuxInputEvent::default(); 64];
        loop {
            let bytes = unsafe {
                libc::read(
                    self.file.as_raw_fd(),
                    events.as_mut_ptr().cast(),
                    mem::size_of_val(&events),
                )
            };
            if bytes < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(error).context("read Touch Bar input events");
            }
            if bytes == 0 {
                bail!("Touch Bar input device {} closed", self.path.display());
            }
            let bytes = bytes as usize;
            if !bytes.is_multiple_of(mem::size_of::<LinuxInputEvent>()) {
                bail!("Touch Bar returned a partial input event");
            }
            for event in &events[..bytes / mem::size_of::<LinuxInputEvent>()] {
                self.process_event(*event, &mut output);
            }
        }
        Ok(output)
    }

    fn process_event(&mut self, event: LinuxInputEvent, output: &mut Vec<TouchEvent>) {
        match (event.kind, event.code) {
            (EV_ABS, ABS_MT_SLOT) => {
                if let Ok(slot) = usize::try_from(event.value)
                    && slot < self.slots.len()
                {
                    self.current_slot = slot;
                }
            }
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                let slot = &mut self.slots[self.current_slot];
                if event.value < 0 {
                    if slot.contact_id.is_some() {
                        slot.pending = Some(TouchPhase::Up);
                    }
                } else {
                    slot.contact_id = Some(event.value as u32);
                    slot.pending = Some(TouchPhase::Down);
                }
            }
            (EV_ABS, ABS_MT_POSITION_X) => {
                let slot = &mut self.slots[self.current_slot];
                slot.x = event.value;
                if slot.contact_id.is_some() && slot.pending.is_none() {
                    slot.pending = Some(TouchPhase::Motion);
                }
            }
            (EV_ABS, ABS_MT_POSITION_Y) => {
                let slot = &mut self.slots[self.current_slot];
                slot.y = event.value;
                if slot.contact_id.is_some() && slot.pending.is_none() {
                    slot.pending = Some(TouchPhase::Motion);
                }
            }
            (EV_SYN, SYN_REPORT) => self.flush_frame(output),
            (EV_SYN, SYN_DROPPED) => self.cancel_all(output),
            _ => {}
        }
    }

    fn flush_frame(&mut self, output: &mut Vec<TouchEvent>) {
        let time_ms = self.started.elapsed().as_millis() as u32;
        for index in 0..self.slots.len() {
            let Some(phase) = self.slots[index].pending.take() else {
                continue;
            };
            let Some(contact_id) = self.slots[index].contact_id else {
                continue;
            };
            let (x_millipixels, y_millipixels) =
                self.transform(self.slots[index].x, self.slots[index].y);
            output.push(TouchEvent {
                phase,
                contact_id,
                time_ms,
                x_millipixels,
                y_millipixels,
            });
            if phase == TouchPhase::Up {
                self.slots[index].contact_id = None;
            }
        }
    }

    fn cancel_all(&mut self, output: &mut Vec<TouchEvent>) {
        let time_ms = self.started.elapsed().as_millis() as u32;
        for index in 0..self.slots.len() {
            let Some(contact_id) = self.slots[index].contact_id.take() else {
                continue;
            };
            let (x_millipixels, y_millipixels) =
                self.transform(self.slots[index].x, self.slots[index].y);
            self.slots[index].pending = None;
            output.push(TouchEvent {
                phase: TouchPhase::Cancel,
                contact_id,
                time_ms,
                x_millipixels,
                y_millipixels,
            });
        }
    }

    fn transform(&self, x: i32, y: i32) -> (i32, i32) {
        let display_x = normalize_axis(x, self.x_axis, self.display_width);
        let display_y = normalize_axis(y, self.y_axis, self.display_height);
        let scene_left = (f64::from(self.display_width) - f64::from(self.scene_width)) / 2.0;
        let scene_top = (f64::from(self.display_height) - f64::from(self.scene_height)) / 2.0;
        (
            ((display_x - scene_left) * 1000.0).round() as i32,
            ((display_y - scene_top) * 1000.0).round() as i32,
        )
    }
}

fn find_touchbar_input() -> Result<PathBuf> {
    let entries = std::fs::read_dir("/sys/class/input").context("read input device list")?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("event") {
            continue;
        }
        let device_name =
            std::fs::read_to_string(entry.path().join("device/name")).unwrap_or_default();
        if device_name.trim().ends_with("Touch Bar") {
            return Ok(Path::new("/dev/input").join(name));
        }
    }
    bail!("no Touch Bar evdev device was found")
}

fn query_abs_axis(file: &File, axis: u16) -> Result<InputAbsInfo> {
    const IOC_READ: u64 = 2;
    const IOC_DIR_SHIFT: u64 = 30;
    const IOC_SIZE_SHIFT: u64 = 16;
    const IOC_TYPE_SHIFT: u64 = 8;
    let request = (IOC_READ << IOC_DIR_SHIFT)
        | ((mem::size_of::<InputAbsInfo>() as u64) << IOC_SIZE_SHIFT)
        | (u64::from(b'E') << IOC_TYPE_SHIFT)
        | u64::from(0x40 + axis);
    let mut info = InputAbsInfo::default();
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, &mut info) };
    if result < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("query Touch Bar absolute axis {axis:#x}"));
    }
    if info.maximum <= info.minimum {
        bail!("Touch Bar absolute axis {axis:#x} has an invalid range");
    }
    Ok(info)
}

fn normalize_axis(value: i32, axis: InputAbsInfo, extent: u32) -> f64 {
    let position = f64::from(value.clamp(axis.minimum, axis.maximum) - axis.minimum);
    let range = f64::from(axis.maximum - axis.minimum);
    position / range * f64::from(extent.saturating_sub(1))
}

fn parse_args() -> Args {
    let mut action = Action::Serve;
    let mut logo = PathBuf::from(DEFAULT_LOGO);
    let mut scene = None;
    let mut socket = Some(PathBuf::from(DEFAULT_HARDWARE_SOCKET));
    let mut duration = Duration::from_secs(10);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(
                    args.next().expect("--socket requires a path"),
                ));
            }
            "--probe" => action = Action::SysfsProbe,
            "--kms-probe" => action = Action::KmsProbe,
            "--display" => action = Action::Display,
            "--animate" => action = Action::Animate,
            "--scene" => {
                action = Action::Scene;
                scene = Some(PathBuf::from(args.next().expect("--scene requires a path")));
            }
            "--stream-probe" => {
                action = Action::StreamProbe;
                scene = Some(PathBuf::from(
                    args.next().expect("--stream-probe requires a path"),
                ));
            }
            "--prime-probe" => action = Action::PrimeProbe,
            "--prime-serve" => {
                action = Action::PrimeServe;
                socket = Some(PathBuf::from(
                    args.next().expect("--prime-serve requires a path"),
                ));
            }
            "--direct" => {
                action = Action::Direct;
                socket = Some(PathBuf::from(
                    args.next().expect("--direct requires a path"),
                ));
            }
            "--logo" => logo = PathBuf::from(args.next().expect("--logo requires a path")),
            "--duration" => {
                let seconds = args
                    .next()
                    .expect("--duration requires seconds")
                    .parse::<u64>()
                    .expect("--duration must be an integer");
                duration = Duration::from_secs(seconds.min(MAX_DEMO_SECONDS));
            }
            "--help" | "-h" => {
                println!(
                    "usage: touchbard [--socket PATH] [--probe | --kms-probe | --prime-probe | --prime-serve PATH | --direct PATH | --display | --animate | --scene PATH | --stream-probe PATH] [--logo PATH] [--duration SECONDS]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    Args {
        action,
        logo,
        scene,
        socket,
        duration,
    }
}

fn find_adp_card() -> Result<PathBuf> {
    let mut cards = std::fs::read_dir("/sys/class/drm")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                return false;
            };
            name.strip_prefix("card").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
            })
        })
        .collect::<Vec<_>>();
    cards.sort();

    for card in cards {
        let driver = std::fs::canonicalize(card.join("device/driver"));
        if driver
            .ok()
            .and_then(|path| path.file_name().map(OsStr::to_owned))
            .as_deref()
            == Some(OsStr::new("adp"))
        {
            let name = card.file_name().context("ADP sysfs card has no filename")?;
            return Ok(Path::new("/dev/dri").join(name));
        }
    }
    bail!("no DRM card driven by the Apple ADP driver was found")
}

fn select_touchbar(card: &Card, force_probe: bool) -> Result<Selection> {
    let resources = card.resource_handles().context("query DRM resources")?;
    let connector = resources
        .connectors()
        .iter()
        .filter_map(|handle| card.get_connector(*handle, force_probe).ok())
        .find(|info| {
            info.state() == connector::State::Connected
                && info.interface() == connector::Interface::DSI
                && info
                    .modes()
                    .first()
                    .is_some_and(|mode| mode.size().1 / mode.size().0 >= 30)
        })
        .context("no connected portrait DSI Touch Bar connector was found")?;
    let mode = *connector
        .modes()
        .first()
        .context("Touch Bar connector has no modes")?;

    let crtc = connector
        .current_encoder()
        .and_then(|handle| card.get_encoder(handle).ok())
        .and_then(|encoder| encoder.crtc())
        .or_else(|| {
            connector.encoders().iter().find_map(|handle| {
                card.get_encoder(*handle).ok().and_then(|encoder| {
                    resources
                        .filter_crtcs(encoder.possible_crtcs())
                        .first()
                        .copied()
                })
            })
        })
        .or_else(|| resources.crtcs().first().copied())
        .context("Touch Bar has no compatible CRTC")?;

    let plane = card
        .plane_handles()
        .context("query DRM planes")?
        .into_iter()
        .find(|handle| {
            card.get_plane(*handle).ok().is_some_and(|info| {
                resources
                    .filter_crtcs(info.possible_crtcs())
                    .contains(&crtc)
            })
        })
        .context("Touch Bar has no compatible plane")?;
    let crtc_index = resources
        .crtcs()
        .iter()
        .position(|handle| *handle == crtc)
        .context("selected Touch Bar CRTC is absent from DRM resources")?
        as u32;

    Ok(Selection {
        connector,
        crtc,
        crtc_index,
        plane,
        mode,
    })
}

fn print_selection(node: &Path, selection: &Selection) {
    let (width, height) = selection.mode.size();
    println!(
        "adp-node={} connector={} connector_id={} crtc_id={} crtc_index={} plane_id={} mode={}x{} refresh_hz={}",
        node.display(),
        selection.connector,
        u32::from(selection.connector.handle()),
        u32::from(selection.crtc),
        selection.crtc_index,
        u32::from(selection.plane),
        width,
        height,
        selection.mode.vrefresh()
    );
}

fn find_prop_id<T: ResourceHandle>(
    card: &Card,
    handle: T,
    name: &'static str,
) -> Result<property::Handle> {
    let props = card.get_properties(handle)?;
    for id in props.as_props_and_values().0 {
        let info = card.get_property(*id)?;
        if info.name().to_bytes() == name.as_bytes() {
            return Ok(*id);
        }
    }
    bail!("DRM property {name} was not found")
}

fn load_png(path: &Path) -> Result<RgbaImage> {
    let file = File::open(path).with_context(|| format!("open logo {}", path.display()))?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("read PNG header")?;
    let buffer_size = reader
        .output_buffer_size()
        .context("PNG output buffer size overflow")?;
    let mut decoded = vec![0; buffer_size];
    let info = reader.next_frame(&mut decoded).context("decode PNG logo")?;
    let source = &decoded[..info.buffer_size()];
    let mut pixels = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    match info.color_type {
        png::ColorType::Rgba => pixels.extend_from_slice(source),
        png::ColorType::Rgb => {
            for pixel in source.as_chunks::<3>().0 {
                pixels.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xff]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for pixel in source.as_chunks::<2>().0 {
                pixels.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for value in source {
                pixels.extend_from_slice(&[*value, *value, *value, 0xff]);
            }
        }
        png::ColorType::Indexed => bail!("PNG palette was not expanded by the decoder"),
    }
    Ok(RgbaImage {
        width: info.width,
        height: info.height,
        pixels,
    })
}

impl FrameStream {
    fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("open frame stream {}", path.display()))?;
        // SAFETY: the producer owns file sizing and only appends complete
        // frames through the documented seqlock protocol.
        let map = unsafe { MmapOptions::new().map(&file) }.context("map frame stream")?;
        if map.len() < FRAME_STREAM_HEADER_SIZE || map[0..8] != FRAME_STREAM_MAGIC {
            bail!("frame stream has an invalid header");
        }
        let width = read_u32(&map, 8)?;
        let height = read_u32(&map, 12)?;
        let stride = read_u32(&map, 16)? as usize;
        let slots = read_u32(&map, 20)? as usize;
        let flags = read_u32(&map, FRAME_STREAM_FLAGS_OFFSET)?;
        if width == 0 || width > 2008 || height == 0 || height > 60 {
            bail!("frame stream dimensions {width}x{height} exceed the Touch Bar");
        }
        if stride < width as usize * 4 || slots != FRAME_STREAM_SLOT_COUNT {
            bail!("frame stream stride or slot count is invalid");
        }
        let frame_bytes = stride
            .checked_mul(height as usize)
            .context("frame stream size overflow")?;
        let expected = FRAME_STREAM_HEADER_SIZE
            .checked_add(
                frame_bytes
                    .checked_mul(slots)
                    .context("frame stream slot size overflow")?,
            )
            .context("frame stream mapping size overflow")?;
        if map.len() < expected {
            bail!("frame stream is truncated");
        }
        Ok(Self {
            map,
            width,
            height,
            stride,
            frame_bytes,
            bottom_up: flags & FRAME_STREAM_FLAG_BOTTOM_UP != 0,
        })
    }

    fn latest(&self, pixels: &mut Vec<u8>) -> Result<Option<u64>> {
        for _ in 0..8 {
            let before = self.sequence_atomic().load(Ordering::Acquire);
            if before == 0 {
                return Ok(None);
            }
            if before & 1 != 0 {
                thread::yield_now();
                continue;
            }
            // The active slot is protected by the sequence counter. Volatile
            // prevents the compiler from caching a value changed by the other
            // process through its shared mapping.
            let slot = unsafe {
                self.map
                    .as_ptr()
                    .add(FRAME_STREAM_ACTIVE_SLOT_OFFSET)
                    .cast::<u32>()
                    .read_volatile()
            } as usize;
            if slot >= FRAME_STREAM_SLOT_COUNT {
                bail!("frame stream selected an invalid slot");
            }
            let start = FRAME_STREAM_HEADER_SIZE + slot * self.frame_bytes;
            pixels.resize(self.frame_bytes, 0);
            pixels.copy_from_slice(&self.map[start..start + self.frame_bytes]);
            fence(Ordering::Acquire);
            let after = self.sequence_atomic().load(Ordering::Acquire);
            if before == after && after & 1 == 0 {
                return Ok(Some(after / 2));
            }
        }
        Ok(None)
    }

    fn sequence_atomic(&self) -> &AtomicU64 {
        // SAFETY: the mapping is page-aligned and the fixed offset is aligned
        // to AtomicU64. The producer initializes the same location atomically.
        unsafe {
            &*(self
                .map
                .as_ptr()
                .add(FRAME_STREAM_SEQUENCE_OFFSET)
                .cast::<AtomicU64>())
        }
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("frame stream header is truncated")?;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
}

fn draw_centered_logo(
    target: &mut [u8],
    pitch: usize,
    physical_width: u32,
    physical_height: u32,
    logo: &RgbaImage,
    logical_x_offset: i32,
) -> Result<()> {
    let logical_width = physical_height;
    let logical_height = physical_width;
    if physical_width < LOGO_SIZE || logical_height < LOGO_SIZE {
        bail!("Touch Bar mode is too small for the logo");
    }
    target.fill(0);
    let left = i64::from((logical_width - LOGO_SIZE) / 2) + i64::from(logical_x_offset);
    let top = (logical_height - LOGO_SIZE) / 2;
    if left < 0 || left + i64::from(LOGO_SIZE) > i64::from(logical_width) {
        bail!("animated logo exceeds the logical Touch Bar bounds");
    }
    let left = left as u32;

    for y in 0..LOGO_SIZE {
        let source_y = y * logo.height / LOGO_SIZE;
        for x in 0..LOGO_SIZE {
            let source_x = x * logo.width / LOGO_SIZE;
            let source = ((source_y * logo.width + source_x) * 4) as usize;
            let alpha = u32::from(logo.pixels[source + 3]);
            if alpha == 0 {
                continue;
            }
            let red = u32::from(logo.pixels[source]) * alpha / 255;
            let green = u32::from(logo.pixels[source + 1]) * alpha / 255;
            let blue = u32::from(logo.pixels[source + 2]) * alpha / 255;

            // Match tiny-dfr's Cairo transform: translate by the 60-pixel
            // height, then rotate the 2008x60 logical scene by +90 degrees.
            let logical_x = left + x;
            let logical_y = top + y;
            let physical_x = physical_width - 1 - logical_y;
            let physical_y = logical_x;
            let destination = physical_y as usize * pitch + physical_x as usize * 4;
            if destination + 4 > target.len() {
                bail!("rotated logo exceeds the DRM dumb buffer");
            }
            target[destination] = blue as u8;
            target[destination + 1] = green as u8;
            target[destination + 2] = red as u8;
            target[destination + 3] = 0xff;
        }
    }
    Ok(())
}

fn draw_rgba_scene(
    target: &mut [u8],
    pitch: usize,
    physical_width: u32,
    physical_height: u32,
    stream: &FrameStream,
    pixels: &[u8],
) -> Result<()> {
    if pixels.len() != stream.frame_bytes {
        bail!("scene pixels do not match the frame stream");
    }
    draw_logical_rgba(
        target,
        pitch,
        physical_width,
        physical_height,
        stream.width,
        stream.height,
        stream.stride,
        stream.bottom_up,
        pixels,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_logical_rgba(
    target: &mut [u8],
    pitch: usize,
    physical_width: u32,
    physical_height: u32,
    logical_width: u32,
    logical_height: u32,
    source_stride: usize,
    bottom_up: bool,
    pixels: &[u8],
) -> Result<()> {
    let required = source_stride
        .checked_mul(logical_height as usize)
        .context("logical scene dimensions overflow")?;
    if source_stride < logical_width as usize * 4 || pixels.len() < required {
        bail!("logical scene pixel buffer is too small");
    }
    if logical_width > physical_height || logical_height > physical_width {
        bail!("logical scene exceeds the Touch Bar mode");
    }
    target.fill(0);
    let logical_left = (physical_height - logical_width) / 2;
    let logical_top = (physical_width - logical_height) / 2;
    for y in 0..logical_height {
        let source_y = if bottom_up { logical_height - 1 - y } else { y };
        for x in 0..logical_width {
            let source = source_y as usize * source_stride + x as usize * 4;
            let alpha = u32::from(pixels[source + 3]);
            let logical_x = logical_left + x;
            let logical_y = logical_top + y;
            let physical_x = physical_width - 1 - logical_y;
            let physical_y = logical_x;
            let destination = physical_y as usize * pitch + physical_x as usize * 4;
            if destination + 4 > target.len() {
                bail!("rotated scene exceeds the DRM dumb buffer");
            }
            target[destination] = (u32::from(pixels[source + 2]) * alpha / 255) as u8;
            target[destination + 1] = (u32::from(pixels[source + 1]) * alpha / 255) as u8;
            target[destination + 2] = (u32::from(pixels[source]) * alpha / 255) as u8;
            target[destination + 3] = 0xff;
        }
    }
    Ok(())
}

fn atomic_modeset(
    card: &Card,
    selection: &Selection,
    framebuffer: framebuffer::Handle,
) -> Result<()> {
    let mut request = atomic::AtomicModeReq::new();
    request.add_property(
        selection.connector.handle(),
        find_prop_id(card, selection.connector.handle(), "CRTC_ID")?,
        property::Value::CRTC(Some(selection.crtc)),
    );
    let mode_blob = card.create_property_blob(&selection.mode)?;
    request.add_property(
        selection.crtc,
        find_prop_id(card, selection.crtc, "MODE_ID")?,
        mode_blob,
    );
    request.add_property(
        selection.crtc,
        find_prop_id(card, selection.crtc, "ACTIVE")?,
        property::Value::Boolean(true),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "FB_ID")?,
        property::Value::Framebuffer(Some(framebuffer)),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "CRTC_ID")?,
        property::Value::CRTC(Some(selection.crtc)),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "SRC_X")?,
        property::Value::UnsignedRange(0),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "SRC_Y")?,
        property::Value::UnsignedRange(0),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "SRC_W")?,
        property::Value::UnsignedRange(u64::from(selection.mode.size().0) << 16),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "SRC_H")?,
        property::Value::UnsignedRange(u64::from(selection.mode.size().1) << 16),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "CRTC_X")?,
        property::Value::SignedRange(0),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "CRTC_Y")?,
        property::Value::SignedRange(0),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "CRTC_W")?,
        property::Value::UnsignedRange(u64::from(selection.mode.size().0)),
    );
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "CRTC_H")?,
        property::Value::UnsignedRange(u64::from(selection.mode.size().1)),
    );
    card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request)
        .context("commit Touch Bar atomic modeset")
}

fn atomic_flip(card: &Card, selection: &Selection, framebuffer: framebuffer::Handle) -> Result<()> {
    let mut request = atomic::AtomicModeReq::new();
    request.add_property(
        selection.plane,
        find_prop_id(card, selection.plane, "FB_ID")?,
        property::Value::Framebuffer(Some(framebuffer)),
    );
    card.atomic_commit(AtomicCommitFlags::empty(), request)
        .context("commit Touch Bar atomic framebuffer flip")
}

fn draw_scanout(
    card: &Card,
    dumb: &mut DumbBuffer,
    display_width: u16,
    display_height: u16,
    logo: &RgbaImage,
    logical_x_offset: i32,
) -> Result<()> {
    let pitch = dumb.pitch() as usize;
    let mut mapping = card
        .map_dumb_buffer(dumb)
        .context("map Touch Bar dumb buffer")?;
    draw_centered_logo(
        mapping.as_mut(),
        pitch,
        u32::from(display_width),
        u32::from(display_height),
        logo,
        logical_x_offset,
    )
}

fn display_logo(node: &Path, args: &Args, animate: bool) -> Result<()> {
    if std::env::var(DISPLAY_CONFIRMATION).as_deref() != Ok("1") {
        bail!(
            "refusing physical modeset without {DISPLAY_CONFIRMATION}=1; use scripts/run-m3-logo.sh"
        );
    }
    let logo = load_png(&args.logo)?;
    let card = Card::open(node, true)?;
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(ClientCapability::Atomic, true)?;
    card.acquire_master_lock()
        .context("acquire ADP DRM master; tiny-dfr must be stopped first")?;
    let selection = select_touchbar(&card, true)?;
    print_selection(node, &selection);
    let (display_width, display_height) = selection.mode.size();

    // tiny-dfr uses a 64-pixel-wide allocation for the 60-pixel-wide mode.
    let mut dumb = card
        .create_dumb_buffer((64, u32::from(display_height)), DrmFourcc::Xrgb8888, 32)
        .context("allocate Touch Bar dumb buffer")?;
    let pitch = dumb.pitch() as usize;
    {
        let mut mapping = card
            .map_dumb_buffer(&mut dumb)
            .context("map Touch Bar dumb buffer")?;
        draw_centered_logo(
            mapping.as_mut(),
            pitch,
            u32::from(display_width),
            u32::from(display_height),
            &logo,
            0,
        )?;
    }
    let framebuffer = card
        .add_framebuffer(&dumb, 24, 32)
        .context("create Touch Bar framebuffer")?;
    atomic_modeset(&card, &selection, framebuffer)?;
    card.dirty_framebuffer(
        framebuffer,
        &[ClipRect::new(0, 0, display_width, display_height)],
    )
    .context("mark Touch Bar framebuffer dirty")?;

    if animate {
        let mut updates = 0_u64;
        let mut first_vblank = None;
        let mut last_vblank = None;
        let mut first_vblank_frame = None;
        let mut last_vblank_frame = None;

        println!(
            "physical-animation active logo={} duration_seconds={}",
            args.logo.display(),
            args.duration.as_secs()
        );
        // This ADP driver revision can retire only one framebuffer-changing
        // atomic commit every two refreshes. For the mapped-copy compatibility
        // path, retain one scanout allocation and update it immediately after
        // each vblank. The centered logo is far enough down the physical scan
        // that this small CPU write completes before scanout reaches it.
        card.wait_vblank(
            VblankWaitTarget::Relative(1),
            VblankWaitFlags::empty(),
            selection.crtc_index,
            0,
        )
        .context("wait for initial Touch Bar modeset")?;
        let started = Instant::now();
        while started.elapsed() < args.duration {
            let vblank = card
                .wait_vblank(
                    VblankWaitTarget::Relative(1),
                    VblankWaitFlags::empty(),
                    selection.crtc_index,
                    0,
                )
                .context("pace Touch Bar update to vblank")?;
            if let Some(timestamp) = vblank.time() {
                first_vblank.get_or_insert(timestamp);
                last_vblank = Some(timestamp);
            }
            first_vblank_frame.get_or_insert(vblank.frame());
            last_vblank_frame = Some(vblank.frame());
            let phase = updates as f64 * std::f64::consts::TAU / 180.0;
            let offset = (phase.sin() * 180.0).round() as i32;
            draw_scanout(
                &card,
                &mut dumb,
                display_width,
                display_height,
                &logo,
                offset,
            )?;
            card.dirty_framebuffer(
                framebuffer,
                &[ClipRect::new(0, 0, display_width, display_height)],
            )
            .context("mark animated Touch Bar framebuffer dirty")?;
            updates += 1;
        }

        let span = first_vblank
            .zip(last_vblank)
            .and_then(|(first, last)| last.checked_sub(first))
            .unwrap_or_default();
        let fps = if updates > 1 && !span.is_zero() {
            (updates - 1) as f64 / span.as_secs_f64()
        } else {
            0.0
        };
        let sequence_delta = first_vblank_frame
            .zip(last_vblank_frame)
            .map_or(0, |(first, last)| last.wrapping_sub(first));
        println!(
            "scanout-summary updates={updates} span_ms={} fps={fps:.2} first_frame={} last_frame={} sequence_delta={sequence_delta}",
            span.as_millis(),
            first_vblank_frame.unwrap_or_default(),
            last_vblank_frame.unwrap_or_default()
        );
    } else {
        println!(
            "physical-demo active logo={} duration_seconds={}",
            args.logo.display(),
            args.duration.as_secs()
        );
        thread::sleep(args.duration);
    }
    println!("physical-demo complete; handing control back to tiny-dfr");

    // The supervising script restarts tiny-dfr immediately after this process
    // closes its DRM master. Framebuffer cleanup is best-effort because it may
    // still be attached until the replacement master modesets.
    let _ = card.destroy_framebuffer(framebuffer);
    let _ = card.destroy_dumb_buffer(dumb);
    let _ = card.release_master_lock();
    Ok(())
}

fn prime_probe(node: &Path) -> Result<()> {
    let card = Card::open(node, true)?;
    let capabilities = card
        .get_driver_capability(DriverCapability::Prime)
        .context("query ADP PRIME capabilities")?;
    let mut dumb = card
        .create_dumb_buffer((64, 2048), DrmFourcc::Xrgb8888, 32)
        .context("allocate unused ADP PRIME probe buffer")?;
    let prime = card
        .buffer_to_prime_fd(dumb.handle(), drm::CLOEXEC | drm::RDWR)
        .context("export ADP dumb buffer as PRIME DMA-BUF")?;
    let imported = card
        .prime_fd_to_buffer(prime.as_fd())
        .context("round-trip ADP PRIME DMA-BUF")?;
    let renderer = prime_egl::render_test(prime.as_fd(), dumb.size(), dumb.pitch())
        .context("import ADP PRIME DMA-BUF into AGX")?;
    let first_pixel = {
        let mapping = card
            .map_dumb_buffer(&mut dumb)
            .context("map GPU-written ADP probe buffer")?;
        <[u8; 4]>::try_from(&mapping[0..4]).expect("four-byte pixel")
    };
    println!(
        "prime-probe=ok capabilities={capabilities:#x} format={:?} size={}x{} pitch={} same_handle={} egl_renderer={renderer:?} first_pixel={first_pixel:02x?}",
        dumb.format(),
        dumb.size().0,
        dumb.size().1,
        dumb.pitch(),
        imported == dumb.handle()
    );
    if imported != dumb.handle() {
        card.close_buffer(imported)
            .context("close round-trip PRIME handle")?;
    }
    card.destroy_dumb_buffer(dumb)
        .context("destroy ADP PRIME probe buffer")?;
    Ok(())
}

fn prime_serve(node: &Path, socket: &Path) -> Result<()> {
    const BUFFER_COUNT: usize = 2;
    const ALLOCATION_WIDTH: u32 = 64;
    const PHYSICAL_WIDTH: u32 = 60;
    const PHYSICAL_HEIGHT: u32 = 2008;

    let card = Card::open(node, true)?;
    let mut dumb_buffers = Vec::with_capacity(BUFFER_COUNT);
    let mut prime_fds = Vec::with_capacity(BUFFER_COUNT);
    for _ in 0..BUFFER_COUNT {
        let dumb = card
            .create_dumb_buffer((ALLOCATION_WIDTH, PHYSICAL_HEIGHT), DrmFourcc::Xrgb8888, 32)
            .context("allocate ADP IPC probe buffer")?;
        let prime = card
            .buffer_to_prime_fd(dumb.handle(), drm::CLOEXEC | drm::RDWR)
            .context("export ADP IPC probe buffer")?;
        dumb_buffers.push(dumb);
        prime_fds.push(prime);
    }
    let pitch = dumb_buffers[0].pitch();
    if dumb_buffers.iter().any(|buffer| buffer.pitch() != pitch) {
        bail!("ADP returned inconsistent swapchain pitches");
    }
    let info = HardwareSwapchain {
        logical_width: PHYSICAL_HEIGHT,
        logical_height: PHYSICAL_WIDTH,
        physical_width: PHYSICAL_WIDTH,
        physical_height: PHYSICAL_HEIGHT,
        format: u32::from_le_bytes(*b"XR24"),
        pitch,
        buffer_size: u64::from(pitch) * u64::from(PHYSICAL_HEIGHT),
        buffer_count: BUFFER_COUNT as u16,
    };

    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect compositor socket {}", socket.display()))?;
    let borrowed = prime_fds.iter().map(AsFd::as_fd).collect::<Vec<_>>();
    send_hardware_swapchain(&stream, info, &borrowed).context("send ADP swapchain descriptors")?;

    let logical_left = (PHYSICAL_HEIGHT - DEFAULT_REGION_WIDTH) as usize / 2;
    let expected_bgr = [0x80, 0x40, 0x20];
    let mut centered_pixel = [0_u8; 4];
    for (index, dumb) in dumb_buffers.iter_mut().enumerate() {
        let expected_event = SessionMessage::FrameReady {
            index: index as u16,
            sequence: index as u64 + 1,
        };
        let event = receive_session_message(&mut stream)
            .context("receive compositor buffer-ready event")?;
        if event != expected_event {
            bail!("unexpected buffer-ready event {event:?}; expected {expected_event:?}");
        }
        let mapping = card
            .map_dumb_buffer(dumb)
            .context("map compositor-written ADP IPC probe buffer")?;
        let offset = logical_left * pitch as usize;
        let pixel = <[u8; 4]>::try_from(&mapping[offset..offset + 4]).expect("four-byte pixel");
        centered_pixel = pixel;
        if pixel[..3] != expected_bgr {
            bail!(
                "ADP buffer {index} has unexpected centered BGR {pixel:02x?}; expected {expected_bgr:02x?}"
            );
        }
        send_hardware_message(
            &mut stream,
            HardwareMessage::BufferReleased {
                index: index as u16,
                sequence: index as u64 + 1,
            },
        )
        .context("send presenter buffer-release event")?;
    }
    println!(
        "prime-ipc-probe=ok buffers={BUFFER_COUNT} size={ALLOCATION_WIDTH}x{PHYSICAL_HEIGHT} pitch={pitch} centered_pixel={centered_pixel:02x?} state=unchanged"
    );

    drop(prime_fds);
    for dumb in dumb_buffers {
        card.destroy_dumb_buffer(dumb)
            .context("destroy ADP IPC probe buffer")?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceScanout {
    Fallback,
    Session { index: usize, sequence: u64 },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum BacklightState {
    #[default]
    Active,
    Dimmed,
    Off,
}

/// Long-lived production hardware ownership. Diagnostic actions intentionally
/// use their shorter, independently guarded setup paths below.
struct ServiceHardware {
    card: Card,
    selection: Selection,
    display_width: u16,
    display_height: u16,
    fallback_dumb: DumbBuffer,
    fallback_framebuffer: framebuffer::Handle,
    _session_dumbs: Vec<DumbBuffer>,
    session_framebuffers: Vec<framebuffer::Handle>,
    session_prime_fds: Vec<OwnedFd>,
    swapchain: HardwareSwapchain,
    scanout: ServiceScanout,
    touch_input: TouchInput,
    fn_input: FnInput,
    seat_activity: SeatActivity,
    keyboard: VirtualKeyboard,
    backlight: TouchBarBacklight,
    fallback_bar: SystemBar,
    fallback_renderer: SystemBarRenderer,
    last_activity: Instant,
    backlight_state: BacklightState,
    wake_contacts: WakeContacts,
    recovery_gesture: RecoveryGesture,
    recovery_marker: RecoveryMarker,
}

#[derive(Default)]
struct WakeContacts {
    contacts: BTreeSet<u32>,
}

impl WakeContacts {
    /// A contact that wakes a dark panel is never delivered as UI input. Its
    /// complete lifetime stays suppressed so a held waking finger cannot turn
    /// into a drag or release action after the pixels become visible.
    fn consume(&mut self, event: TouchEvent, woke_panel: bool) -> bool {
        if woke_panel {
            if matches!(event.phase, TouchPhase::Up | TouchPhase::Cancel) {
                self.contacts.remove(&event.contact_id);
            } else {
                self.contacts.insert(event.contact_id);
            }
            return true;
        }
        if !self.contacts.contains(&event.contact_id) {
            return false;
        }
        if matches!(event.phase, TouchPhase::Up | TouchPhase::Cancel) {
            self.contacts.remove(&event.contact_id);
        }
        true
    }
}

impl ServiceHardware {
    const BUFFER_COUNT: usize = 3;
    const ALLOCATION_WIDTH: u32 = 64;

    fn open(node: &Path, recovery_marker: RecoveryMarker) -> Result<Self> {
        let card = Card::open(node, true)?;
        card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
        card.set_client_capability(ClientCapability::Atomic, true)?;
        card.acquire_master_lock()
            .context("acquire persistent ADP DRM master; tiny-dfr must be stopped first")?;
        let selection = select_touchbar(&card, true)?;
        print_selection(node, &selection);
        let (display_width, display_height) = selection.mode.size();
        let logical_width = u32::from(display_height);
        let logical_height = u32::from(display_width);

        let mut fallback_dumb = card
            .create_dumb_buffer(
                (Self::ALLOCATION_WIDTH, u32::from(display_height)),
                DrmFourcc::Xrgb8888,
                32,
            )
            .context("allocate persistent fallback buffer")?;
        let fallback_framebuffer = card
            .add_framebuffer(&fallback_dumb, 24, 32)
            .context("create persistent fallback framebuffer")?;

        let mut session_dumbs = Vec::with_capacity(Self::BUFFER_COUNT);
        let mut session_framebuffers = Vec::with_capacity(Self::BUFFER_COUNT);
        let mut session_prime_fds = Vec::with_capacity(Self::BUFFER_COUNT);
        for _ in 0..Self::BUFFER_COUNT {
            let dumb = card
                .create_dumb_buffer(
                    (Self::ALLOCATION_WIDTH, u32::from(display_height)),
                    DrmFourcc::Xrgb8888,
                    32,
                )
                .context("allocate persistent session buffer")?;
            let framebuffer = card
                .add_framebuffer(&dumb, 24, 32)
                .context("create persistent session framebuffer")?;
            let prime = card
                .buffer_to_prime_fd(dumb.handle(), drm::CLOEXEC | drm::RDWR)
                .context("export persistent session buffer")?;
            session_dumbs.push(dumb);
            session_framebuffers.push(framebuffer);
            session_prime_fds.push(prime);
        }
        let pitch = session_dumbs[0].pitch();
        if fallback_dumb.pitch() != pitch
            || session_dumbs.iter().any(|buffer| buffer.pitch() != pitch)
        {
            bail!("ADP returned inconsistent persistent-output pitches");
        }
        let swapchain = HardwareSwapchain {
            logical_width,
            logical_height,
            physical_width: u32::from(display_width),
            physical_height: u32::from(display_height),
            format: u32::from_le_bytes(*b"XR24"),
            pitch,
            buffer_size: u64::from(pitch) * u64::from(display_height),
            buffer_count: Self::BUFFER_COUNT as u16,
        };

        let touch_input =
            TouchInput::open(logical_width, logical_height, logical_width, logical_height)?;
        let fn_input = FnInput::open()?;
        let recovery_gesture = RecoveryGesture::new(fn_input.pressed(), Instant::now());
        let mut seat_activity = SeatActivity::open()?;
        // Initial device discovery is not user activity.
        let _ = seat_activity.read_activity()?;
        let keyboard = VirtualKeyboard::open()?;
        println!("system-keys=ready device=/dev/uinput");
        let mut backlight = TouchBarBacklight::open()?;
        backlight.set_active()?;
        println!(
            "touchbar-backlight=ready device={} brightness={}",
            backlight.path().display(),
            backlight.active_value()
        );

        let mut fallback_bar = SystemBar::new(
            SystemBarConfig::default(),
            logical_width as f32,
            logical_height as f32,
        );
        fallback_bar.set_fn_pressed(fn_input.pressed());
        let mut fallback_renderer = SystemBarRenderer::new(logical_width, logical_height);
        {
            let mut output = DrmInitialFallbackOutput {
                card: &card,
                selection: &selection,
                dumb: &mut fallback_dumb,
                framebuffer: fallback_framebuffer,
                display_width,
                display_height,
                bar: &fallback_bar,
                renderer: &mut fallback_renderer,
            };
            activate_initial_fallback(&mut output)?;
        }
        println!("hardware-fallback=presented initial=true");
        println!(
            "hardware-fallback=active layer={}",
            if fn_input.pressed() {
                "function"
            } else {
                "media"
            }
        );

        Ok(Self {
            card,
            selection,
            display_width,
            display_height,
            fallback_dumb,
            fallback_framebuffer,
            _session_dumbs: session_dumbs,
            session_framebuffers,
            session_prime_fds,
            swapchain,
            scanout: ServiceScanout::Fallback,
            touch_input,
            fn_input,
            seat_activity,
            keyboard,
            backlight,
            fallback_bar,
            fallback_renderer,
            last_activity: Instant::now(),
            backlight_state: BacklightState::Active,
            wake_contacts: WakeContacts::default(),
            recovery_gesture,
            recovery_marker,
        })
    }

    fn wake(&mut self) -> Result<bool> {
        self.last_activity = Instant::now();
        let woke_dark_panel = self.backlight_state == BacklightState::Off;
        if self.backlight_state != BacklightState::Active {
            self.backlight.set_active()?;
            self.backlight_state = BacklightState::Active;
            println!("touchbar-backlight=active");
        }
        Ok(woke_dark_panel)
    }

    fn update_idle_backlight(&mut self) -> Result<()> {
        let elapsed = self.last_activity.elapsed();
        if elapsed >= BACKLIGHT_OFF_TIMEOUT && self.backlight_state != BacklightState::Off {
            self.backlight.set_idle()?;
            self.backlight_state = BacklightState::Off;
            println!("touchbar-backlight=off");
        } else if elapsed >= BACKLIGHT_DIM_TIMEOUT && self.backlight_state == BacklightState::Active
        {
            self.backlight.set_dimmed()?;
            self.backlight_state = BacklightState::Dimmed;
            println!("touchbar-backlight=dimmed");
        }
        Ok(())
    }

    fn poll_recovery_gesture(&mut self) -> Result<Option<RecoveryAction>> {
        let Some(action) = self.recovery_gesture.poll(Instant::now()) else {
            return Ok(None);
        };
        self.keyboard.release_all()?;
        let locked = action == RecoveryAction::EnterFallback;
        if let Err(error) = self.recovery_marker.set_locked(locked) {
            // Status publication is secondary to the hardware-owned recovery
            // action. Never keep a broken user scene active because its marker
            // could not be updated.
            eprintln!("hardware-recovery=marker-failed error={error:#}");
        }
        println!(
            "hardware-recovery={} gesture=fn-hold duration_ms={}",
            if locked {
                "fallback-locked"
            } else {
                "sessions-resumed"
            },
            recovery::HOLD_DURATION.as_millis()
        );
        Ok(Some(action))
    }

    fn emit_fallback_transitions(
        &mut self,
        transitions: impl IntoIterator<Item = touchbar_system_bar::KeyTransition>,
    ) -> Result<()> {
        for transition in transitions {
            self.keyboard.emit(transition.key, transition.phase)?;
        }
        Ok(())
    }

    fn refresh_fallback(&mut self) -> Result<()> {
        render_system_fallback(
            &self.card,
            &mut self.fallback_dumb,
            self.fallback_framebuffer,
            self.display_width,
            self.display_height,
            &self.fallback_bar,
            &mut self.fallback_renderer,
        )
    }

    fn show_fallback(&mut self) -> Result<()> {
        let releases = self.fallback_bar.cancel_all();
        self.emit_fallback_transitions(releases)?;
        let transitions = self.fallback_bar.set_fn_pressed(self.fn_input.pressed());
        self.emit_fallback_transitions(transitions)?;
        self.keyboard.release_all()?;
        self.refresh_fallback()?;
        if self.scanout != ServiceScanout::Fallback {
            atomic_flip(&self.card, &self.selection, self.fallback_framebuffer)?;
        }
        self.scanout = ServiceScanout::Fallback;
        println!(
            "hardware-fallback=active layer={}",
            if self.fn_input.pressed() {
                "function"
            } else {
                "media"
            }
        );
        Ok(())
    }

    fn wait_for_session(
        &mut self,
        listener: &UnixListener,
        shutdown: &ShutdownFd,
    ) -> Result<Option<UnixStream>> {
        loop {
            let mut poll_fds = [
                libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.touch_input.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.fn_input.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.seat_activity.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: shutdown.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, 500) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("poll persistent fallback");
            }

            if poll_fds[4].revents & libc::POLLIN != 0 && shutdown.consume()? {
                return Ok(None);
            }
            require_hardware_fd(poll_fds[1].revents, "Touch Bar input")?;
            require_hardware_fd(poll_fds[2].revents, "Fn input")?;
            require_hardware_fd(poll_fds[3].revents, "seat activity input")?;

            let mut redraw = false;
            if poll_fds[1].revents & libc::POLLIN != 0 {
                let events = self.touch_input.read_events()?;
                let woke_panel = if events.is_empty() {
                    false
                } else {
                    self.wake()?
                };
                for event in events {
                    if self.wake_contacts.consume(event, woke_panel) {
                        continue;
                    }
                    let transitions = self.fallback_bar.handle_touch(event);
                    self.emit_fallback_transitions(transitions)?;
                    redraw = true;
                }
            }
            if poll_fds[2].revents & libc::POLLIN != 0 {
                let _ = self.wake()?;
                if let Some(pressed) = self.fn_input.read_changed()? {
                    self.recovery_gesture.fn_changed(pressed, Instant::now());
                    let transitions = self.fallback_bar.set_fn_pressed(pressed);
                    self.emit_fallback_transitions(transitions)?;
                    println!(
                        "hardware-fn={} fallback-layer={}",
                        if pressed { "pressed" } else { "released" },
                        if pressed { "function" } else { "media" }
                    );
                    redraw = true;
                }
            }
            if poll_fds[3].revents & libc::POLLIN != 0 && self.seat_activity.read_activity()? {
                let _ = self.wake()?;
            }
            let _ = self.poll_recovery_gesture()?;
            if redraw {
                self.refresh_fallback()?;
            }
            self.update_idle_backlight()?;

            if poll_fds[0].revents & libc::POLLIN == 0 {
                continue;
            }
            let (stream, _) = listener.accept().context("accept user Touch Bar session")?;
            if !self.recovery_gesture.sessions_allowed() {
                let peer = peer_uid(&stream)
                    .map(|uid| uid.to_string())
                    .unwrap_or_else(|_| "unknown".to_owned());
                eprintln!(
                    "hardware-session=rejected peer_uid={peer} reason=recovery-fallback-locked"
                );
                continue;
            }
            let actual_uid = peer_uid(&stream)?;
            match active_seat_uid("seat0") {
                Ok(expected_uid) if actual_uid == expected_uid => {
                    let releases = self.fallback_bar.cancel_all();
                    self.emit_fallback_transitions(releases)?;
                    self.keyboard.release_all()?;
                    self.refresh_fallback()?;
                    println!("hardware-session=accepted uid={actual_uid}");
                    return Ok(Some(stream));
                }
                Ok(expected_uid) => eprintln!(
                    "hardware-session=rejected peer_uid={actual_uid} active_uid={expected_uid}"
                ),
                Err(error) => eprintln!(
                    "hardware-session=rejected peer_uid={actual_uid} active_seat_error={error:#}"
                ),
            }
        }
    }

    fn run_session(&mut self, stream: UnixStream, shutdown: &ShutdownFd) -> Result<bool> {
        let result = self.run_session_inner(stream, shutdown);
        if let Err(error) = self.keyboard.release_all() {
            eprintln!("system-keys=release-failed error={error:#}");
        }
        if result
            .as_ref()
            .is_err_and(|error| error.downcast_ref::<HardwareDeviceLost>().is_some())
        {
            return result;
        }
        if let Err(error) = self.show_fallback() {
            eprintln!("hardware-fallback=restore-failed error={error:#}");
            if result.is_ok() {
                return Err(error);
            }
        }
        result
    }

    fn run_session_inner(&mut self, mut stream: UnixStream, shutdown: &ShutdownFd) -> Result<bool> {
        let borrowed = self
            .session_prime_fds
            .iter()
            .map(AsFd::as_fd)
            .collect::<Vec<_>>();
        send_hardware_swapchain(&stream, self.swapchain, &borrowed)
            .context("lend persistent hardware swapchain")?;
        send_hardware_message(
            &mut stream,
            HardwareMessage::FnChanged {
                pressed: self.fn_input.pressed(),
            },
        )
        .context("send initial Fn state")?;
        println!(
            "hardware-session waiting buffers={} size={}x{} pitch={}",
            Self::BUFFER_COUNT,
            Self::ALLOCATION_WIDTH,
            self.display_height,
            self.swapchain.pitch
        );

        let waiting_started = Instant::now();
        let mut display_started = None;
        let mut last_sequence = 0_u64;
        loop {
            if display_started.is_none() && waiting_started.elapsed() >= Duration::from_secs(5) {
                bail!("session compositor did not submit a buffer within five seconds");
            }
            let mut poll_fds = [
                libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.touch_input.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.fn_input.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.seat_activity.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: shutdown.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, 50) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("poll persistent user session");
            }
            if poll_fds[4].revents & libc::POLLIN != 0 && shutdown.consume()? {
                return Ok(false);
            }
            require_hardware_fd(poll_fds[1].revents, "Touch Bar input")?;
            require_hardware_fd(poll_fds[2].revents, "Fn input")?;
            require_hardware_fd(poll_fds[3].revents, "seat activity input")?;
            if poll_fds[1].revents & libc::POLLIN != 0 {
                let touches = self.touch_input.read_events()?;
                let woke_panel = if touches.is_empty() {
                    false
                } else {
                    self.wake()?
                };
                for touch in touches {
                    if self.wake_contacts.consume(touch, woke_panel) {
                        continue;
                    }
                    send_hardware_message(&mut stream, HardwareMessage::Touch(touch))
                        .context("forward Touch Bar input event")?;
                }
            }
            if poll_fds[2].revents & libc::POLLIN != 0 {
                let _ = self.wake()?;
                if let Some(pressed) = self.fn_input.read_changed()? {
                    self.recovery_gesture.fn_changed(pressed, Instant::now());
                    send_hardware_message(&mut stream, HardwareMessage::FnChanged { pressed })
                        .context("forward Fn state")?;
                    println!(
                        "hardware-fn={} destination=session",
                        if pressed { "pressed" } else { "released" }
                    );
                }
            }
            if poll_fds[3].revents & libc::POLLIN != 0 && self.seat_activity.read_activity()? {
                let _ = self.wake()?;
            }
            if self.poll_recovery_gesture()? == Some(RecoveryAction::EnterFallback) {
                return Ok(true);
            }
            self.update_idle_backlight()?;

            if poll_fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                bail!("user session closed the hardware socket");
            }
            if poll_fds[0].revents & libc::POLLIN == 0 {
                continue;
            }
            let message = receive_session_message(&mut stream)
                .context("receive persistent session message")?;
            let (index, sequence) = match message {
                SessionMessage::Key { key, phase } => {
                    self.keyboard.emit(key, phase)?;
                    continue;
                }
                SessionMessage::FrameReady { index, sequence } => (usize::from(index), sequence),
            };
            let framebuffer = *self
                .session_framebuffers
                .get(index)
                .context("session submitted an out-of-range hardware buffer")?;
            if matches!(self.scanout, ServiceScanout::Session { index: active, .. } if active == index)
            {
                bail!("session resubmitted the buffer currently being scanned out");
            }
            if sequence <= last_sequence {
                bail!("session submitted a stale hardware sequence");
            }
            // A newly attached compositor may arrive after the fallback has
            // gone idle. Its first complete frame is a visible lifecycle
            // transition, so present it with the panel awake. Subsequent
            // animation frames deliberately do not count as user activity.
            if display_started.is_none() {
                let _ = self.wake()?;
            }
            self.card
                .dirty_framebuffer(
                    framebuffer,
                    &[ClipRect::new(0, 0, self.display_width, self.display_height)],
                )
                .context("mark GPU-rendered persistent framebuffer dirty")?;
            atomic_flip(&self.card, &self.selection, framebuffer)?;
            if let ServiceScanout::Session {
                index: released_index,
                sequence: released_sequence,
            } = self.scanout
            {
                send_hardware_message(
                    &mut stream,
                    HardwareMessage::BufferReleased {
                        index: released_index as u16,
                        sequence: released_sequence,
                    },
                )
                .context("release retired persistent scanout buffer")?;
            }
            self.scanout = ServiceScanout::Session { index, sequence };
            last_sequence = sequence;
            if display_started.is_none() {
                display_started = Some(Instant::now());
                println!("hardware-session active first_sequence={sequence}");
            }
        }
    }

    fn prepare_shutdown(&mut self) -> Result<()> {
        let releases = self.fallback_bar.cancel_all();
        self.emit_fallback_transitions(releases)?;
        self.keyboard.release_all()?;
        self.backlight.set_idle()?;
        println!("hardware-service=stopping keys=released backlight=idle");
        Ok(())
    }
}

fn display_direct(node: &Path, socket: &Path, duration: Duration) -> Result<()> {
    if std::env::var(DISPLAY_CONFIRMATION).as_deref() != Ok("1") {
        bail!("refusing physical modeset without {DISPLAY_CONFIRMATION}=1; use the M3 runner");
    }
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connect compositor output socket {}", socket.display()))?;
    run_hardware_session(node, stream, Some(duration))
}

fn render_system_fallback(
    card: &Card,
    dumb: &mut DumbBuffer,
    framebuffer: framebuffer::Handle,
    display_width: u16,
    display_height: u16,
    bar: &SystemBar,
    renderer: &mut SystemBarRenderer,
) -> Result<()> {
    draw_system_fallback(card, dumb, display_width, display_height, bar, renderer)?;
    mark_full_framebuffer_dirty(card, framebuffer, display_width, display_height)
}

fn draw_system_fallback(
    card: &Card,
    dumb: &mut DumbBuffer,
    display_width: u16,
    display_height: u16,
    bar: &SystemBar,
    renderer: &mut SystemBarRenderer,
) -> Result<()> {
    let logical_width = u32::from(display_height);
    let logical_height = u32::from(display_width);
    let pixels = renderer.render_platform(&bar.buttons(), logical_width, logical_height);
    let pitch = dumb.pitch() as usize;
    let mut mapping = card
        .map_dumb_buffer(dumb)
        .context("map hardware fallback buffer")?;
    draw_logical_rgba(
        mapping.as_mut(),
        pitch,
        u32::from(display_width),
        u32::from(display_height),
        logical_width,
        logical_height,
        logical_width as usize * 4,
        false,
        pixels,
    )
}

fn mark_full_framebuffer_dirty(
    card: &Card,
    framebuffer: framebuffer::Handle,
    display_width: u16,
    display_height: u16,
) -> Result<()> {
    card.dirty_framebuffer(
        framebuffer,
        &[ClipRect::new(0, 0, display_width, display_height)],
    )
    .context("mark hardware fallback framebuffer dirty")
}

trait InitialFallbackOutput {
    fn render(&mut self) -> Result<()>;
    fn modeset(&mut self) -> Result<()>;
    fn flush_after_modeset(&mut self) -> Result<()>;
}

fn activate_initial_fallback(output: &mut impl InitialFallbackOutput) -> Result<()> {
    // Dumb-buffer writes are not guaranteed to reach a newly enabled ADP
    // scanout when DIRTYFB precedes the modeset. Fn interaction used to appear
    // to wake a blank bar because its redraw issued DIRTYFB after the modeset.
    // Keep this ordering explicit and regression-testable.
    output
        .render()
        .context("render initial hardware fallback")?;
    output
        .modeset()
        .context("activate hardware fallback scanout")?;
    output
        .flush_after_modeset()
        .context("flush initial hardware fallback after modeset")
}

struct DrmInitialFallbackOutput<'a> {
    card: &'a Card,
    selection: &'a Selection,
    dumb: &'a mut DumbBuffer,
    framebuffer: framebuffer::Handle,
    display_width: u16,
    display_height: u16,
    bar: &'a SystemBar,
    renderer: &'a mut SystemBarRenderer,
}

impl InitialFallbackOutput for DrmInitialFallbackOutput<'_> {
    fn render(&mut self) -> Result<()> {
        draw_system_fallback(
            self.card,
            self.dumb,
            self.display_width,
            self.display_height,
            self.bar,
            self.renderer,
        )
    }

    fn modeset(&mut self) -> Result<()> {
        atomic_modeset(self.card, self.selection, self.framebuffer)
    }

    fn flush_after_modeset(&mut self) -> Result<()> {
        mark_full_framebuffer_dirty(
            self.card,
            self.framebuffer,
            self.display_width,
            self.display_height,
        )
    }
}

fn serve(node: &Path, socket: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    if unsafe { libc::geteuid() } != 0 {
        bail!("touchbard must run as a system service with effective uid 0");
    }
    let shutdown = ShutdownFd::install()?;
    let parent = socket.parent().context("hardware socket needs a parent")?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create hardware runtime directory {}", parent.display()))?;
    }
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    if !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != 0
        || parent_metadata.mode() & 0o022 != 0
    {
        bail!(
            "hardware runtime directory {} must be a root-owned non-symlink directory without group/world write access",
            parent.display()
        );
    }
    if let Ok(metadata) = std::fs::symlink_metadata(socket) {
        if !metadata.file_type().is_socket() {
            bail!("hardware socket path {} is not a socket", socket.display());
        }
        match UnixStream::connect(socket) {
            Ok(_) => bail!(
                "another touchbard is already listening on {}",
                socket.display()
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(socket)?;
            }
            Err(error) => return Err(error).context("probe existing hardware socket"),
        }
    }

    let recovery_marker = RecoveryMarker::initialize(socket)?;
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind hardware socket {}", socket.display()))?;
    let _socket_cleanup = ServiceSocketCleanup(socket.to_path_buf());
    // Any local process may attempt a connection, but SO_PEERCRED plus logind's
    // active seat owner is the authorization boundary below. This lets the
    // daemon start before a graphical login and survive user switching.
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))?;
    println!("hardware-service=ready socket={}", socket.display());
    let mut hardware = ServiceHardware::open(node, recovery_marker)?;

    loop {
        let Some(stream) = hardware.wait_for_session(&listener, &shutdown)? else {
            break;
        };
        match hardware.run_session(stream, &shutdown) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) if error.downcast_ref::<HardwareDeviceLost>().is_some() => {
                return Err(error).context("hardware device disappeared");
            }
            Err(error) => eprintln!("hardware-session=ended error={error:#}"),
        }
    }
    hardware.prepare_shutdown()?;
    println!("hardware-service=stopped");
    Ok(())
}

fn run_hardware_session(
    node: &Path,
    mut stream: UnixStream,
    duration: Option<Duration>,
) -> Result<()> {
    const BUFFER_COUNT: usize = 3;
    const ALLOCATION_WIDTH: u32 = 64;

    let card = Card::open(node, true)?;
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(ClientCapability::Atomic, true)?;
    card.acquire_master_lock()
        .context("acquire ADP DRM master; tiny-dfr must be stopped first")?;
    let selection = select_touchbar(&card, true)?;
    print_selection(node, &selection);
    let (display_width, display_height) = selection.mode.size();

    let mut dumb_buffers = Vec::with_capacity(BUFFER_COUNT);
    let mut framebuffers = Vec::with_capacity(BUFFER_COUNT);
    let mut prime_fds = Vec::with_capacity(BUFFER_COUNT);
    for _ in 0..BUFFER_COUNT {
        let dumb = card
            .create_dumb_buffer(
                (ALLOCATION_WIDTH, u32::from(display_height)),
                DrmFourcc::Xrgb8888,
                32,
            )
            .context("allocate direct Touch Bar buffer")?;
        let framebuffer = card
            .add_framebuffer(&dumb, 24, 32)
            .context("create direct Touch Bar framebuffer")?;
        let prime = card
            .buffer_to_prime_fd(dumb.handle(), drm::CLOEXEC | drm::RDWR)
            .context("export direct Touch Bar buffer")?;
        dumb_buffers.push(dumb);
        framebuffers.push(framebuffer);
        prime_fds.push(prime);
    }
    let pitch = dumb_buffers[0].pitch();
    if dumb_buffers.iter().any(|buffer| buffer.pitch() != pitch) {
        bail!("ADP returned inconsistent direct-output pitches");
    }
    let info = HardwareSwapchain {
        logical_width: u32::from(display_height),
        logical_height: u32::from(display_width),
        physical_width: u32::from(display_width),
        physical_height: u32::from(display_height),
        format: u32::from_le_bytes(*b"XR24"),
        pitch,
        buffer_size: u64::from(pitch) * u64::from(display_height),
        buffer_count: BUFFER_COUNT as u16,
    };
    let borrowed = prime_fds.iter().map(AsFd::as_fd).collect::<Vec<_>>();
    send_hardware_swapchain(&stream, info, &borrowed).context("lend direct ADP swapchain")?;
    let mut touch_input = match TouchInput::open(
        info.logical_width,
        info.logical_height,
        DEFAULT_REGION_WIDTH,
        info.logical_height,
    ) {
        Ok(input) => Some(input),
        Err(error) => {
            eprintln!("touch-input=unavailable error={error:#}");
            None
        }
    };
    let mut virtual_keyboard = match VirtualKeyboard::open() {
        Ok(keyboard) => {
            println!("system-keys=ready device=/dev/uinput");
            Some(keyboard)
        }
        Err(error) => {
            eprintln!("system-keys=unavailable error={error:#}");
            None
        }
    };
    let mut fn_input = match FnInput::open() {
        Ok(input) => Some(input),
        Err(error) => {
            eprintln!("fn-input=unavailable error={error:#}");
            None
        }
    };
    if let Some(input) = &fn_input {
        send_hardware_message(
            &mut stream,
            HardwareMessage::FnChanged {
                pressed: input.pressed(),
            },
        )
        .context("send initial Fn state")?;
    }
    let mut backlight = match TouchBarBacklight::open() {
        Ok(mut backlight) => {
            backlight.set_active()?;
            Some(backlight)
        }
        Err(error) => {
            eprintln!("touchbar-backlight=unavailable error={error:#}");
            None
        }
    };

    println!(
        "hardware-session waiting buffers={BUFFER_COUNT} size={}x{} pitch={pitch} duration={}",
        ALLOCATION_WIDTH,
        display_height,
        duration.map_or_else(
            || "persistent".into(),
            |duration| format!("{}s", duration.as_secs())
        )
    );
    let waiting_started = Instant::now();
    let mut display_started = None;
    let mut current: Option<(usize, u64)> = None;
    let mut last_sequence = 0_u64;
    let mut updates = 0_u64;
    let mut last_activity = Instant::now();
    let mut backlight_state = BacklightState::Active;
    let mut wake_contacts = WakeContacts::default();
    loop {
        if display_started.is_some_and(|started: Instant| {
            duration.is_some_and(|duration| started.elapsed() >= duration)
        }) {
            break;
        }
        if display_started.is_none() && waiting_started.elapsed() >= Duration::from_secs(5) {
            bail!("compositor did not submit a direct buffer within five seconds");
        }
        let mut poll_fds = [
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: touch_input.as_ref().map_or(-1, TouchInput::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: fn_input.as_ref().map_or(-1, FnInput::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let poll_result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, 50) };
        if poll_result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("poll ADP output and Touch Bar input");
        }
        if poll_fds[1].revents & libc::POLLIN != 0 {
            let touches = match touch_input
                .as_mut()
                .expect("polled Touch Bar input")
                .read_events()
            {
                Ok(events) => events,
                Err(error) => {
                    eprintln!("touch-input=stopped error={error:#}");
                    touch_input = None;
                    Vec::new()
                }
            };
            let woke_panel = !touches.is_empty() && backlight_state == BacklightState::Off;
            if !touches.is_empty() {
                last_activity = Instant::now();
                if backlight_state != BacklightState::Active {
                    if let Some(backlight) = backlight.as_mut() {
                        backlight.set_active()?;
                    }
                    backlight_state = BacklightState::Active;
                }
            }
            for touch in touches {
                if wake_contacts.consume(touch, woke_panel) {
                    continue;
                }
                send_hardware_message(&mut stream, HardwareMessage::Touch(touch))
                    .context("forward Touch Bar input event")?;
            }
        }
        if poll_fds[2].revents & libc::POLLIN != 0 {
            last_activity = Instant::now();
            if backlight_state != BacklightState::Active {
                if let Some(backlight) = backlight.as_mut() {
                    backlight.set_active()?;
                }
                backlight_state = BacklightState::Active;
            }
            match fn_input.as_mut().expect("polled Fn input").read_changed() {
                Ok(Some(pressed)) => {
                    send_hardware_message(&mut stream, HardwareMessage::FnChanged { pressed })
                        .context("forward Fn state")?
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("fn-input=stopped error={error:#}");
                    fn_input = None;
                }
            }
        }
        let idle_elapsed = last_activity.elapsed();
        if idle_elapsed >= BACKLIGHT_OFF_TIMEOUT && backlight_state != BacklightState::Off {
            if let Some(backlight) = backlight.as_mut() {
                backlight.set_idle()?;
                println!("touchbar-backlight=off");
            }
            backlight_state = BacklightState::Off;
        } else if idle_elapsed >= BACKLIGHT_DIM_TIMEOUT && backlight_state == BacklightState::Active
        {
            if let Some(backlight) = backlight.as_mut() {
                backlight.set_dimmed()?;
                println!("touchbar-backlight=dimmed");
            }
            backlight_state = BacklightState::Dimmed;
        }
        if poll_fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            bail!("compositor closed the ADP output socket");
        }
        if poll_fds[0].revents & libc::POLLIN == 0 {
            continue;
        }
        let event = receive_session_message(&mut stream)
            .context("receive direct ADP buffer-ready event")?;
        let (index, sequence) = match event {
            SessionMessage::FrameReady { index, sequence } => (index, sequence),
            SessionMessage::Key { key, phase } => {
                let keyboard = virtual_keyboard
                    .as_mut()
                    .context("session requested a system key but uinput is unavailable")?;
                keyboard.emit(key, phase)?;
                continue;
            }
        };
        let index = usize::from(index);
        let framebuffer = *framebuffers
            .get(index)
            .context("compositor submitted an out-of-range ADP buffer")?;
        if current.is_some_and(|(current_index, _)| current_index == index) {
            bail!("compositor resubmitted the buffer currently being scanned out");
        }
        if sequence <= last_sequence {
            bail!("compositor submitted a stale direct-output sequence");
        }

        card.dirty_framebuffer(
            framebuffer,
            &[ClipRect::new(0, 0, display_width, display_height)],
        )
        .context("mark GPU-rendered Touch Bar framebuffer dirty")?;
        if current.is_none() {
            atomic_modeset(&card, &selection, framebuffer)?;
            display_started = Some(Instant::now());
            println!("hardware-session active first_sequence={sequence}");
        } else {
            atomic_flip(&card, &selection, framebuffer)?;
        }
        if let Some((released_index, released_sequence)) = current {
            send_hardware_message(
                &mut stream,
                HardwareMessage::BufferReleased {
                    index: released_index as u16,
                    sequence: released_sequence,
                },
            )
            .context("release retired ADP scanout buffer")?;
        }
        current = Some((index, sequence));
        last_sequence = sequence;
        updates += 1;
    }

    let elapsed = display_started.map_or(Duration::ZERO, |started| started.elapsed());
    let fps = if updates > 1 && !elapsed.is_zero() {
        (updates - 1) as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    println!(
        "direct-summary updates={updates} last_sequence={last_sequence} elapsed_ms={} fps={fps:.2}",
        elapsed.as_millis()
    );
    println!("hardware-session complete");

    drop(stream);
    drop(prime_fds);
    for framebuffer in framebuffers {
        let _ = card.destroy_framebuffer(framebuffer);
    }
    for dumb in dumb_buffers {
        let _ = card.destroy_dumb_buffer(dumb);
    }
    let _ = card.release_master_lock();
    Ok(())
}

fn display_scene(node: &Path, args: &Args) -> Result<()> {
    if std::env::var(DISPLAY_CONFIRMATION).as_deref() != Ok("1") {
        bail!("refusing physical modeset without {DISPLAY_CONFIRMATION}=1; use the M3 runner");
    }
    let scene_path = args.scene.as_deref().context("--scene requires a path")?;
    let stream = FrameStream::open(scene_path)?;
    let mut pixels = Vec::new();
    let first_frame_deadline = Instant::now() + Duration::from_secs(2);
    let first_sequence = loop {
        if let Some(sequence) = stream.latest(&mut pixels)? {
            break sequence;
        }
        if Instant::now() >= first_frame_deadline {
            bail!("frame stream did not publish a frame within two seconds");
        }
        thread::sleep(Duration::from_millis(5));
    };

    let card = Card::open(node, true)?;
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(ClientCapability::Atomic, true)?;
    card.acquire_master_lock()
        .context("acquire ADP DRM master; tiny-dfr must be stopped first")?;
    let selection = select_touchbar(&card, true)?;
    print_selection(node, &selection);
    let (display_width, display_height) = selection.mode.size();
    let mut dumb = card
        .create_dumb_buffer((64, u32::from(display_height)), DrmFourcc::Xrgb8888, 32)
        .context("allocate Touch Bar scene buffer")?;
    let pitch = dumb.pitch() as usize;
    {
        let mut mapping = card
            .map_dumb_buffer(&mut dumb)
            .context("map Touch Bar scene buffer")?;
        draw_rgba_scene(
            mapping.as_mut(),
            pitch,
            u32::from(display_width),
            u32::from(display_height),
            &stream,
            &pixels,
        )?;
    }
    let framebuffer = card
        .add_framebuffer(&dumb, 24, 32)
        .context("create Touch Bar scene framebuffer")?;
    atomic_modeset(&card, &selection, framebuffer)?;
    card.dirty_framebuffer(
        framebuffer,
        &[ClipRect::new(0, 0, display_width, display_height)],
    )
    .context("mark Touch Bar scene framebuffer dirty")?;

    println!(
        "physical-scene active source={} source_size={}x{} duration_seconds={}",
        scene_path.display(),
        stream.width,
        stream.height,
        args.duration.as_secs()
    );
    let started = Instant::now();
    let mut last_sequence = first_sequence;
    let mut physical_updates = 1_u64;
    let mut dropped_source_frames = 0_u64;
    while started.elapsed() < args.duration {
        card.wait_vblank(
            VblankWaitTarget::Relative(1),
            VblankWaitFlags::empty(),
            selection.crtc_index,
            0,
        )
        .context("pace physical scene to Touch Bar vblank")?;
        let Some(sequence) = stream.latest(&mut pixels)? else {
            continue;
        };
        if sequence == last_sequence {
            continue;
        }
        dropped_source_frames += sequence.saturating_sub(last_sequence).saturating_sub(1);
        last_sequence = sequence;
        {
            let mut mapping = card
                .map_dumb_buffer(&mut dumb)
                .context("map Touch Bar scene buffer")?;
            draw_rgba_scene(
                mapping.as_mut(),
                pitch,
                u32::from(display_width),
                u32::from(display_height),
                &stream,
                &pixels,
            )?;
        }
        card.dirty_framebuffer(
            framebuffer,
            &[ClipRect::new(0, 0, display_width, display_height)],
        )
        .context("mark updated Touch Bar scene dirty")?;
        physical_updates += 1;
    }
    println!(
        "scene-summary physical_updates={physical_updates} last_source_sequence={last_sequence} dropped_source_frames={dropped_source_frames}"
    );
    println!("physical-scene complete; handing control back to tiny-dfr");

    let _ = card.destroy_framebuffer(framebuffer);
    let _ = card.destroy_dumb_buffer(dumb);
    let _ = card.release_master_lock();
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args();
    if args.action == Action::StreamProbe {
        let path = args
            .scene
            .as_deref()
            .context("--stream-probe requires a path")?;
        let stream = FrameStream::open(path)?;
        let mut pixels = Vec::new();
        let sequence = stream
            .latest(&mut pixels)?
            .context("frame stream has not published a frame")?;
        let checksum = pixels.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
        });
        println!(
            "stream-probe=ok size={}x{} sequence={sequence} checksum={checksum:016x}",
            stream.width, stream.height
        );
        return Ok(());
    }
    let node = find_adp_card()?;
    println!("discovered-adp-node={}", node.display());
    match args.action {
        Action::Serve => serve(
            &node,
            args.socket
                .as_deref()
                .context("hardware service requires --socket PATH")?,
        ),
        Action::SysfsProbe => {
            println!("probe=ok driver=adp state=unchanged");
            Ok(())
        }
        Action::KmsProbe => {
            let card = Card::open(&node, false)?;
            // Plane enumeration is hidden unless universal planes are enabled
            // for this DRM file descriptor. These client capabilities do not
            // alter connector, CRTC, or framebuffer state.
            card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
            card.set_client_capability(ClientCapability::Atomic, true)?;
            let selection = select_touchbar(&card, false)?;
            print_selection(&node, &selection);
            println!("kms-probe=ok state=unchanged");
            Ok(())
        }
        Action::PrimeProbe => prime_probe(&node),
        Action::PrimeServe => prime_serve(
            &node,
            args.socket
                .as_deref()
                .context("--prime-serve requires a path")?,
        ),
        Action::Direct => display_direct(
            &node,
            args.socket.as_deref().context("--direct requires a path")?,
            args.duration,
        ),
        Action::Display => display_logo(&node, &args, false),
        Action::Animate => display_logo(&node, &args, true),
        Action::Scene => display_scene(&node, &args),
        Action::StreamProbe => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordedInitialFallback {
        calls: Vec<&'static str>,
    }

    impl InitialFallbackOutput for RecordedInitialFallback {
        fn render(&mut self) -> Result<()> {
            self.calls.push("render");
            Ok(())
        }

        fn modeset(&mut self) -> Result<()> {
            self.calls.push("modeset");
            Ok(())
        }

        fn flush_after_modeset(&mut self) -> Result<()> {
            self.calls.push("flush");
            Ok(())
        }
    }

    fn test_touch_input() -> TouchInput {
        TouchInput {
            file: File::open("/dev/null").unwrap(),
            path: PathBuf::from("/dev/null"),
            x_axis: InputAbsInfo {
                minimum: 0,
                maximum: 23_044,
                ..InputAbsInfo::default()
            },
            y_axis: InputAbsInfo {
                minimum: 0,
                maximum: 639,
                ..InputAbsInfo::default()
            },
            current_slot: 0,
            slots: vec![TouchSlot::default(); 4],
            started: Instant::now(),
            display_width: 2008,
            display_height: 60,
            scene_width: 1004,
            scene_height: 60,
        }
    }

    #[test]
    fn initial_fallback_is_flushed_only_after_the_modeset() {
        let mut output = RecordedInitialFallback::default();

        activate_initial_fallback(&mut output).unwrap();

        assert_eq!(output.calls, ["render", "modeset", "flush"]);
    }

    fn input_event(kind: u16, code: u16, value: i32) -> LinuxInputEvent {
        LinuxInputEvent {
            kind,
            code,
            value,
            ..LinuxInputEvent::default()
        }
    }

    fn touch(contact_id: u32, phase: TouchPhase) -> TouchEvent {
        TouchEvent {
            phase,
            contact_id,
            time_ms: 0,
            x_millipixels: 10_000,
            y_millipixels: 10_000,
        }
    }

    #[test]
    fn waking_contact_is_consumed_through_its_entire_lifetime() {
        let mut contacts = WakeContacts::default();

        assert!(contacts.consume(touch(7, TouchPhase::Down), true));
        assert!(contacts.consume(touch(7, TouchPhase::Motion), false));
        assert!(contacts.consume(touch(7, TouchPhase::Up), false));
        assert!(!contacts.consume(touch(7, TouchPhase::Down), false));
    }

    #[test]
    fn every_contact_in_a_waking_batch_is_consumed() {
        let mut contacts = WakeContacts::default();

        assert!(contacts.consume(touch(7, TouchPhase::Down), true));
        assert!(contacts.consume(touch(8, TouchPhase::Down), true));
        assert!(contacts.consume(touch(7, TouchPhase::Up), true));
        assert!(contacts.consume(touch(8, TouchPhase::Up), true));
        assert!(!contacts.consume(touch(7, TouchPhase::Down), false));
        assert!(!contacts.consume(touch(8, TouchPhase::Down), false));
    }

    #[test]
    fn terminal_event_can_wake_without_poisoning_the_next_contact() {
        let mut contacts = WakeContacts::default();

        assert!(contacts.consume(touch(7, TouchPhase::Up), true));
        assert!(!contacts.consume(touch(7, TouchPhase::Down), false));
    }

    #[test]
    fn maps_the_full_sensor_onto_the_centered_scene() {
        let input = test_touch_input();
        assert_eq!(input.transform(0, 0), (-502_000, 0));
        assert_eq!(input.transform(23_044, 639), (1_505_000, 59_000));
    }

    #[test]
    fn decodes_type_b_contact_lifecycle_on_syn_report() {
        let mut input = test_touch_input();
        let mut output = Vec::new();
        input.process_event(input_event(EV_ABS, ABS_MT_TRACKING_ID, 17), &mut output);
        input.process_event(input_event(EV_ABS, ABS_MT_POSITION_X, 6_322), &mut output);
        input.process_event(input_event(EV_ABS, ABS_MT_POSITION_Y, 320), &mut output);
        input.process_event(input_event(EV_SYN, SYN_REPORT, 0), &mut output);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].phase, TouchPhase::Down);
        assert_eq!(output[0].contact_id, 17);

        input.process_event(input_event(EV_ABS, ABS_MT_POSITION_X, 9_768), &mut output);
        input.process_event(input_event(EV_SYN, SYN_REPORT, 0), &mut output);
        assert_eq!(output[1].phase, TouchPhase::Motion);

        input.process_event(input_event(EV_ABS, ABS_MT_TRACKING_ID, -1), &mut output);
        input.process_event(input_event(EV_SYN, SYN_REPORT, 0), &mut output);
        assert_eq!(output[2].phase, TouchPhase::Up);
        assert!(input.slots[0].contact_id.is_none());
    }

    #[test]
    fn centers_and_rotates_logo_like_tiny_dfr() {
        let pitch = 64 * 4;
        let mut target = vec![0; pitch * 2008];
        let logo = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        };

        draw_centered_logo(&mut target, pitch, 60, 2008, &logo, 0).unwrap();

        let top_right = 980 * pitch + 53 * 4;
        let bottom_left = 1027 * pitch + 6 * 4;
        assert_eq!(&target[top_right..top_right + 4], &[0, 0, 255, 255]);
        assert_eq!(&target[bottom_left..bottom_left + 4], &[0, 0, 255, 255]);
        assert_eq!(
            target
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] == 255)
                .count(),
            (LOGO_SIZE * LOGO_SIZE) as usize
        );
    }

    #[test]
    fn transparent_logo_leaves_black_scanout() {
        let pitch = 64 * 4;
        let mut target = vec![0xff; pitch * 2008];
        let logo = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![255, 255, 255, 0],
        };

        draw_centered_logo(&mut target, pitch, 60, 2008, &logo, 0).unwrap();

        assert!(target.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn centers_rotates_and_flips_bottom_up_scene() {
        let pitch = 64 * 4;
        let mut target = vec![0; pitch * 2008];
        let map = MmapOptions::new()
            .len(FRAME_STREAM_HEADER_SIZE)
            .map_anon()
            .unwrap()
            .make_read_only()
            .unwrap();
        let stream = FrameStream {
            map,
            width: 2,
            height: 2,
            stride: 8,
            frame_bytes: 16,
            bottom_up: true,
        };
        // Bottom row first, then top row, as returned by glReadPixels.
        let pixels = [
            0, 255, 0, 255, 255, 255, 255, 255, 255, 0, 0, 255, 0, 0, 255, 255,
        ];

        draw_rgba_scene(&mut target, pitch, 60, 2008, &stream, &pixels).unwrap();

        let top_left = 1003 * pitch + 30 * 4;
        let top_right = 1004 * pitch + 30 * 4;
        let bottom_left = 1003 * pitch + 29 * 4;
        assert_eq!(&target[top_left..top_left + 4], &[0, 0, 255, 255]);
        assert_eq!(&target[top_right..top_right + 4], &[255, 0, 0, 255]);
        assert_eq!(&target[bottom_left..bottom_left + 4], &[0, 255, 0, 255]);
    }

    #[test]
    fn hardware_poll_failures_are_distinct_from_normal_readiness() {
        assert!(require_hardware_fd(0, "test input").is_ok());
        assert!(require_hardware_fd(libc::POLLIN, "test input").is_ok());

        let error = require_hardware_fd(libc::POLLHUP, "test input").unwrap_err();
        let lost = error.downcast_ref::<HardwareDeviceLost>().unwrap();
        assert_eq!(lost.device, "test input");
        assert_eq!(lost.events, libc::POLLHUP);
    }
}
