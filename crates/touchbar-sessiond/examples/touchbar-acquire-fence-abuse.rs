use std::{
    ffi::CString,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
};

use anyhow::{Context, Result, bail};
use touchbar_protocol::client::{touchbar_manager_v1, touchbar_surface_v1};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 60;
const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");

#[derive(Default)]
struct State {
    configured: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<touchbar_surface_v1::TouchbarSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        role: &touchbar_surface_v1::TouchbarSurfaceV1,
        event: touchbar_surface_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let touchbar_surface_v1::Event::Configure { serial, .. } = event {
            role.ack_configure(serial);
            state.configured = true;
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore touchbar_manager_v1::TouchbarManagerV1);
delegate_noop!(State: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);

fn event_fd() -> Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create invalid test fence");
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fake_dmabuf(size: usize) -> Result<OwnedFd> {
    let name = CString::new("touchbar-invalid-dmabuf").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create fake DMA-BUF storage");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_fd().as_raw_fd(), size as libc::off_t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("size fake DMA-BUF storage");
    }
    Ok(fd)
}

fn main() -> Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "duplicate".into());
    let connection = Connection::connect_to_env().context("connect to test compositor")?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let handle = queue.handle();
    let compositor = globals.bind::<wl_compositor::WlCompositor, _, _>(&handle, 1..=6, ())?;
    let manager =
        globals.bind::<touchbar_manager_v1::TouchbarManagerV1, _, _>(&handle, 1..=1, ())?;
    let surface = compositor.create_surface(&handle, ());
    let role = manager.get_item_surface(
        &surface,
        "touchbar.acquire-fence-abuse".into(),
        "probe".into(),
        WIDTH,
        WIDTH,
        WIDTH,
        0,
        0,
        0,
        &handle,
        (),
    );
    let mut state = State::default();
    for _ in 0..4 {
        queue.roundtrip(&mut state)?;
        if state.configured {
            break;
        }
    }
    if !state.configured {
        bail!("managed surface was not configured");
    }

    match mode.as_str() {
        "duplicate" => {
            let first = event_fd()?;
            let second = event_fd()?;
            role.set_acquire_fence(first.as_fd());
            role.set_acquire_fence(second.as_fd());
        }
        "no-buffer" => {
            let fence = event_fd()?;
            role.set_acquire_fence(fence.as_fd());
            surface.commit();
        }
        "shm" => {
            let shm = globals.bind::<wl_shm::WlShm, _, _>(&handle, 1..=1, ())?;
            let stride = WIDTH * 4;
            let storage = fake_dmabuf((stride * HEIGHT) as usize)?;
            let pool = shm.create_pool(storage.as_fd(), (stride * HEIGHT) as i32, &handle, ());
            let buffer = pool.create_buffer(
                0,
                WIDTH as i32,
                HEIGHT as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                &handle,
                (),
            );
            surface.attach(Some(&buffer), 0, 0);
            let fence = event_fd()?;
            role.set_acquire_fence(fence.as_fd());
            surface.commit();
        }
        "invalid" => {
            let dmabuf =
                globals.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(&handle, 1..=4, ())?;
            let params = dmabuf.create_params(&handle, ());
            let storage = fake_dmabuf((WIDTH * HEIGHT * 4) as usize)?;
            params.add(storage.as_fd(), 0, 0, WIDTH * 4, 0, 0);
            let buffer = params.create_immed(
                WIDTH as i32,
                HEIGHT as i32,
                DRM_FORMAT_ARGB8888,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &handle,
                (),
            );
            surface.attach(Some(&buffer), 0, 0);
            let fence = event_fd()?;
            role.set_acquire_fence(fence.as_fd());
            surface.commit();
        }
        _ => bail!("mode must be duplicate, no-buffer, shm, or invalid"),
    }
    connection.flush()?;
    // Do not send a wl_display.sync after the malformed request. A protocol
    // error is terminal, and leaving that extra request unread while the
    // server closes the client can make Linux report ECONNRESET before the
    // already-queued protocol error. Block only for the compositor's response.
    match queue.blocking_dispatch(&mut state) {
        Ok(_) => bail!("compositor accepted malformed acquire-fence traffic"),
        Err(error) => {
            println!("acquire-fence-abuse=disconnected mode={mode} error={error}");
            Ok(())
        }
    }
}
