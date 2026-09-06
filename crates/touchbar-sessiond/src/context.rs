//! Context events from deterministic replay or Hyprland's event socket.

use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use touchbar_model::ContextValue;

use crate::wake::EventSignal;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextEvent {
    pub key: String,
    pub value: ContextValue,
}

impl ContextEvent {
    fn text(key: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: ContextValue::Text(value.into()),
        }
    }

    fn application(value: &str) -> Self {
        Self::text("application.id", value.trim().to_ascii_lowercase())
    }
}

pub struct ContextReplay {
    started: Option<Instant>,
    events: VecDeque<(Duration, ContextEvent)>,
}

impl ContextReplay {
    pub fn demo() -> Self {
        Self {
            started: None,
            events: [
                (
                    Duration::from_millis(150),
                    ContextEvent::text("application.id", "firefox"),
                ),
                (
                    Duration::from_millis(1_100),
                    ContextEvent::text("application.id", "terminal"),
                ),
                (
                    Duration::from_millis(1_600),
                    ContextEvent::text("application.id", "firefox"),
                ),
            ]
            .into(),
        }
    }

    pub fn poll(&mut self) -> Vec<ContextEvent> {
        let started = *self.started.get_or_insert_with(Instant::now);
        let elapsed = started.elapsed();
        let mut ready = Vec::new();
        while self
            .events
            .front()
            .is_some_and(|(scheduled, _)| *scheduled <= elapsed)
        {
            ready.push(self.events.pop_front().unwrap().1);
        }
        ready
    }

    pub fn finished(&self) -> bool {
        self.events.is_empty()
    }
}

pub struct HyprlandContextSource {
    receiver: Receiver<ContextEvent>,
    signal: EventSignal,
}

impl HyprlandContextSource {
    pub fn connect() -> Result<(Self, Vec<ContextEvent>)> {
        let socket = hyprland_event_socket()?;
        let stream = UnixStream::connect(&socket)
            .with_context(|| format!("connect Hyprland event socket {}", socket.display()))?;
        // Open the event stream before querying the snapshot so a focus change
        // cannot disappear between discovery and subscription. Seed the
        // reader's fact cache from that snapshot; title-only `activewindow`
        // traffic must not wake the compositor when the application class did
        // not change.
        let initial = initial_application().into_iter().collect::<Vec<_>>();
        let mut last_values = initial
            .iter()
            .map(|event| (event.key.clone(), event.value.clone()))
            .collect::<BTreeMap<_, _>>();
        let (sender, receiver) = mpsc::channel();
        let signal = EventSignal::new().context("create Hyprland event signal")?;
        let reader_signal = signal.try_clone().context("clone Hyprland event signal")?;
        thread::Builder::new()
            .name("hyprland-touchbar-context".into())
            .spawn(move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if let Some(event) = parse_hyprland_event(&line)
                        && retain_changed_fact(&mut last_values, &event)
                    {
                        if sender.send(event).is_err() {
                            break;
                        }
                        reader_signal.notify();
                    }
                }
                // Also wake the event loop so channel disconnection is
                // observed without a background polling interval.
                reader_signal.notify();
            })
            .context("start Hyprland context reader")?;
        Ok((Self { receiver, signal }, initial))
    }

    pub fn notification_fd(&self) -> std::os::fd::RawFd {
        self.signal.as_raw_fd()
    }

    pub fn poll(&self) -> Result<Vec<ContextEvent>> {
        self.signal.drain().context("drain Hyprland event signal")?;
        let mut events = Vec::new();
        loop {
            match self.receiver.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => return Ok(events),
                Err(TryRecvError::Disconnected) => {
                    bail!("Hyprland context event stream disconnected")
                }
            }
        }
    }
}

fn retain_changed_fact(
    last_values: &mut BTreeMap<String, ContextValue>,
    event: &ContextEvent,
) -> bool {
    if last_values.get(&event.key) == Some(&event.value) {
        return false;
    }
    last_values.insert(event.key.clone(), event.value.clone());
    true
}

fn hyprland_event_socket() -> Result<PathBuf> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
        .context("HYPRLAND_INSTANCE_SIGNATURE is not set")?;
    let mut candidates = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(
            PathBuf::from(runtime)
                .join("hypr")
                .join(&signature)
                .join(".socket2.sock"),
        );
    }
    candidates.push(
        PathBuf::from("/tmp/hypr")
            .join(signature)
            .join(".socket2.sock"),
    );
    candidates
        .into_iter()
        .find(|path| path.exists())
        .context("Hyprland event socket was not found")
}

fn initial_application() -> Option<ContextEvent> {
    let output = Command::new("hyprctl").arg("activewindow").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("class:")
            .map(str::trim)
            .filter(|class| !class.is_empty())
            .map(ContextEvent::application)
    })
}

fn parse_hyprland_event(line: &str) -> Option<ContextEvent> {
    let (name, payload) = line.split_once(">>")?;
    match name {
        "activewindow" => Some(ContextEvent::application(
            payload.split_once(',').map_or(payload, |(class, _)| class),
        )),
        "workspace" | "workspacev2" => Some(ContextEvent::text(
            "workspace.id",
            payload.rsplit_once(',').map_or(payload, |(_, name)| name),
        )),
        "focusedmon" => payload
            .split_once(',')
            .map(|(_, workspace)| ContextEvent::text("workspace.id", workspace)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_application_and_workspace_events() {
        assert_eq!(
            parse_hyprland_event("activewindow>>Firefox,Documentation"),
            Some(ContextEvent::application("firefox"))
        );
        assert_eq!(
            parse_hyprland_event("workspacev2>>3,coding"),
            Some(ContextEvent::text("workspace.id", "coding"))
        );
        assert_eq!(
            parse_hyprland_event("focusedmon>>eDP-1,web"),
            Some(ContextEvent::text("workspace.id", "web"))
        );
        assert_eq!(parse_hyprland_event("openwindow>>abc,1,class,title"), None);
    }

    #[test]
    fn unchanged_hyprland_facts_do_not_wake_the_compositor() {
        let mut last = BTreeMap::new();
        let foot = ContextEvent::application("foot");
        assert!(retain_changed_fact(&mut last, &foot));
        assert!(!retain_changed_fact(&mut last, &foot));

        let firefox = ContextEvent::application("firefox");
        assert!(retain_changed_fact(&mut last, &firefox));
        assert!(!retain_changed_fact(&mut last, &firefox));

        let workspace = ContextEvent::text("workspace.id", "2");
        assert!(retain_changed_fact(&mut last, &workspace));
        assert!(!retain_changed_fact(&mut last, &workspace));
    }
}
