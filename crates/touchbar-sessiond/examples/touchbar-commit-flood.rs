use anyhow::{Context, Result, bail};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_compositor, wl_registry, wl_surface},
};

struct State;

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

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);

fn main() -> Result<()> {
    let connection = Connection::connect_to_env().context("connect to test compositor")?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let handle = queue.handle();
    let compositor = globals
        .bind::<wl_compositor::WlCompositor, _, _>(&handle, 1..=6, ())
        .context("bind wl_compositor")?;
    let surface = compositor.create_surface(&handle, ());
    for batch in 1..=40 {
        for _ in 0..16 {
            surface.commit();
        }
        if let Err(error) = queue.roundtrip(&mut State) {
            println!("commit-flood=disconnected batch={batch} error={error}");
            return Ok(());
        }
    }
    bail!("compositor accepted an abusive commit flood")
}
