use std::{
    collections::HashMap,
    io::{self, Write},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use zbus::{blocking::connection::Builder as ConnectionBuilder, zvariant::Value};

const SERVICE: &str = "org.mpris.MediaPlayer2.playerctld";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

struct FakePlayer {
    playing: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    #[zbus(property)]
    fn playback_status(&self) -> &'static str {
        let status = status(self.playing.load(Ordering::Relaxed));
        report(&format!("get PlaybackStatus={status}"));
        status
    }

    fn play_pause(&self) {
        let playing = !self.playing.fetch_xor(true, Ordering::Relaxed);
        report(&format!("PlayPause status={}", status(playing)));
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fake-mpris: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> zbus::Result<()> {
    let playing = Arc::new(AtomicBool::new(false));
    let connection = ConnectionBuilder::session()?
        .name(SERVICE)?
        .serve_at(
            PATH,
            FakePlayer {
                playing: Arc::clone(&playing),
            },
        )?
        .build()?;
    println!("fake-mpris: ready service={SERVICE}");
    io::stdout().flush()?;

    let mut reported = playing.load(Ordering::Relaxed);
    let mut next_automatic_change = Instant::now() + Duration::from_millis(1_500);
    loop {
        let now = Instant::now();
        if now >= next_automatic_change {
            playing.fetch_xor(true, Ordering::Relaxed);
            next_automatic_change = now + Duration::from_millis(1_500);
        }

        let current = playing.load(Ordering::Relaxed);
        if current != reported {
            emit_status(&connection, current)?;
            reported = current;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn emit_status(connection: &zbus::blocking::Connection, playing: bool) -> zbus::Result<()> {
    let mut properties = HashMap::new();
    properties.insert("PlaybackStatus", Value::from(status(playing)));
    connection.emit_signal(
        Option::<&str>::None,
        PATH,
        PROPERTIES_INTERFACE,
        "PropertiesChanged",
        &(PLAYER_INTERFACE, properties, Vec::<String>::new()),
    )
}

fn status(playing: bool) -> &'static str {
    if playing { "Playing" } else { "Paused" }
}

fn report(message: &str) {
    println!("fake-mpris: {message}");
    let _ = io::stdout().flush();
}
