use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_POWER_SUPPLIES: usize = 64;
const MAX_ATTRIBUTE_BYTES: u64 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerState {
    External,
    Battery,
    Unknown,
}

impl PowerState {
    pub fn label(self) -> &'static str {
        match self {
            Self::External => "external",
            Self::Battery => "battery",
            Self::Unknown => "unknown",
        }
    }
}

pub struct PowerSource {
    root: PathBuf,
    state: PowerState,
    last_check: Instant,
}

impl PowerSource {
    pub fn discover() -> Self {
        let root = std::env::var_os("TOUCHBAR_POWER_SUPPLY_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/sys/class/power_supply"));
        let state = inspect(&root);
        println!("power-source={} state={}", root.display(), state.label());
        Self {
            root,
            state,
            last_check: Instant::now(),
        }
    }

    pub fn state(&self) -> PowerState {
        self.state
    }

    pub fn next_poll_delay(&self) -> Duration {
        POLL_INTERVAL.saturating_sub(self.last_check.elapsed())
    }

    pub fn poll(&mut self) -> Option<PowerState> {
        if self.last_check.elapsed() < POLL_INTERVAL {
            return None;
        }
        self.last_check = Instant::now();
        let next = inspect(&self.root);
        if next == self.state {
            return None;
        }
        self.state = next;
        println!("power-state-changed state={}", next.label());
        Some(next)
    }
}

fn inspect(root: &Path) -> PowerState {
    let Ok(entries) = fs::read_dir(root) else {
        return PowerState::Unknown;
    };
    let mut battery = false;
    let mut external = false;
    for entry in entries.flatten().take(MAX_POWER_SUPPLIES) {
        let path = entry.path();
        let Some(kind) = read_attribute(&path.join("type")) else {
            continue;
        };
        if kind == "Battery" {
            battery = true;
        } else if is_external_supply(&kind)
            && read_attribute(&path.join("online")).as_deref() == Some("1")
        {
            external = true;
        }
    }
    if external {
        PowerState::External
    } else if battery {
        PowerState::Battery
    } else {
        PowerState::Unknown
    }
}

fn read_attribute(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut value = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_ATTRIBUTE_BYTES + 1)
        .read_to_string(&mut value)
        .ok()?;
    (value.len() as u64 <= MAX_ATTRIBUTE_BYTES).then(|| value.trim().to_owned())
}

fn is_external_supply(kind: &str) -> bool {
    matches!(
        kind,
        "UPS"
            | "Mains"
            | "USB"
            | "USB_DCP"
            | "USB_CDP"
            | "USB_ACA"
            | "USB_C"
            | "USB_PD"
            | "USB_PD_DRP"
            | "BrickID"
            | "Wireless"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(root: &Path, name: &str, kind: &str, online: Option<&str>) {
        let path = root.join(name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("type"), kind).unwrap();
        if let Some(online) = online {
            fs::write(path.join("online"), online).unwrap();
        }
    }

    #[test]
    fn external_power_wins_over_a_present_battery() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "battery", "Battery", None);
        supply(root.path(), "adapter", "Mains", Some("1\n"));
        assert_eq!(inspect(root.path()), PowerState::External);
    }

    #[test]
    fn present_battery_without_online_adapter_is_on_battery() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "battery", "Battery", None);
        supply(root.path(), "adapter", "USB_PD", Some("0"));
        assert_eq!(inspect(root.path()), PowerState::Battery);
    }

    #[test]
    fn missing_malformed_and_batteryless_sources_are_unknown() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "adapter", "Mains", Some("invalid"));
        assert_eq!(inspect(root.path()), PowerState::Unknown);
        assert_eq!(inspect(&root.path().join("missing")), PowerState::Unknown);
    }

    #[test]
    fn polling_observes_a_live_external_to_battery_transition() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "battery", "Battery", None);
        supply(root.path(), "adapter", "Mains", Some("1"));
        let mut source = PowerSource {
            root: root.path().to_owned(),
            state: PowerState::External,
            last_check: Instant::now() - POLL_INTERVAL,
        };
        fs::write(root.path().join("adapter/online"), "0\n").unwrap();
        assert_eq!(source.poll(), Some(PowerState::Battery));
        assert_eq!(source.state(), PowerState::Battery);
        assert_eq!(source.poll(), None);
    }
}
