use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use touchbar_protocol::appearance::{AppearanceSnapshot, ColorScheme, MotionPolicy, Rgba8};

use crate::power::PowerState;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnimationCadence {
    external_hz: u32,
    battery_hz: u32,
}

impl Default for AnimationCadence {
    fn default() -> Self {
        Self {
            external_hz: 60,
            battery_hz: 30,
        }
    }
}

impl AnimationCadence {
    pub fn hz(self, power: PowerState) -> u32 {
        match power {
            PowerState::Battery => self.battery_hz,
            PowerState::External | PowerState::Unknown => self.external_hz,
        }
    }

    pub fn frame_period(self, power: PowerState) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.hz(power)))
    }
}

#[derive(Clone, Copy, Default)]
struct AppearanceConfig {
    snapshot: AppearanceSnapshot,
    cadence: AnimationCadence,
}

pub struct AppearanceSource {
    path: Option<PathBuf>,
    snapshot: AppearanceSnapshot,
    cadence: AnimationCadence,
    last_contents: Option<String>,
    last_check: Instant,
}

impl AppearanceSource {
    pub fn discover() -> Self {
        let path = std::env::var_os("TOUCHBAR_THEME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                    })
                    .map(|root| root.join("touchbar/theme.toml"))
            });
        let contents = path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok());
        let config = contents
            .as_deref()
            .and_then(|contents| parse_theme_config(contents, 1))
            .unwrap_or_default();
        println!(
            "appearance-source={} generation={} scheme={:?} animation_hz={} battery_animation_hz={}",
            path.as_deref()
                .map_or_else(|| "built-in".into(), |path| path.display().to_string()),
            config.snapshot.generation,
            config.snapshot.scheme,
            config.cadence.external_hz,
            config.cadence.battery_hz,
        );
        Self {
            path,
            snapshot: config.snapshot,
            cadence: config.cadence,
            last_contents: contents,
            last_check: Instant::now(),
        }
    }

    pub fn snapshot(&self) -> AppearanceSnapshot {
        self.snapshot
    }

    pub fn cadence(&self) -> AnimationCadence {
        self.cadence
    }

    pub fn next_poll_delay(&self) -> Option<Duration> {
        self.path
            .as_ref()
            .map(|_| POLL_INTERVAL.saturating_sub(self.last_check.elapsed()))
    }

    pub fn poll(&mut self) -> Option<AppearanceSnapshot> {
        if self.last_check.elapsed() < POLL_INTERVAL {
            return None;
        }
        self.last_check = Instant::now();
        let path = self.path.as_deref()?;
        let contents = std::fs::read_to_string(path).ok()?;
        if self.last_contents.as_deref() == Some(&contents) {
            return None;
        }
        self.last_contents = Some(contents.clone());
        let generation = self.snapshot.generation.wrapping_add(1).max(1);
        let next = parse_theme_config(&contents, generation)?;
        if same_appearance(self.snapshot, next.snapshot) && self.cadence == next.cadence {
            return None;
        }
        self.snapshot = next.snapshot;
        self.cadence = next.cadence;
        println!(
            "appearance-changed generation={} scheme={:?} animation_hz={} battery_animation_hz={} source={}",
            next.snapshot.generation,
            next.snapshot.scheme,
            next.cadence.external_hz,
            next.cadence.battery_hz,
            path.display()
        );
        Some(next.snapshot)
    }
}

fn same_appearance(mut left: AppearanceSnapshot, mut right: AppearanceSnapshot) -> bool {
    left.generation = 0;
    right.generation = 0;
    left == right
}

#[cfg(test)]
fn parse_theme(contents: &str, generation: u32) -> Option<AppearanceSnapshot> {
    parse_theme_config(contents, generation).map(|config| config.snapshot)
}

fn parse_theme_config(contents: &str, generation: u32) -> Option<AppearanceConfig> {
    let values = contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((
                key.trim().to_string(),
                value.trim().trim_matches(['"', '\'']).to_string(),
            ))
        })
        .collect::<HashMap<_, _>>();
    let color = |key: &str| values.get(key).and_then(|value| parse_hex_color(value));
    let background = color("background")?;
    let accent = color("accent")?;
    let foreground = color("foreground")?;
    let muted = color("muted").unwrap_or(foreground);
    let selection = color("selection").unwrap_or(accent);
    let destructive = color("red").unwrap_or(Rgba8::rgb(230, 80, 80));
    let scheme = match values.get("mode").map(String::as_str) {
        Some("light") => ColorScheme::Light,
        _ => ColorScheme::Dark,
    };
    let motion = match values.get("motion").map(String::as_str) {
        Some("reduced") => MotionPolicy::Reduced,
        Some("disabled") => MotionPolicy::Disabled,
        _ => MotionPolicy::Full,
    };
    let defaults = AnimationCadence::default();
    let cadence = AnimationCadence {
        external_hz: parse_animation_hz(values.get("animation_hz"), defaults.external_hz)?,
        battery_hz: parse_animation_hz(values.get("battery_animation_hz"), defaults.battery_hz)?,
    };
    Some(AppearanceConfig {
        snapshot: AppearanceSnapshot {
            generation,
            scheme,
            motion,
            background,
            surface: with_alpha(selection, 180),
            surface_hover: with_alpha(muted, 210),
            surface_pressed: with_alpha(accent, 230),
            foreground,
            muted,
            accent,
            destructive,
            corner_radius_millipixels: 9_000,
        },
        cadence,
    })
}

fn parse_animation_hz(value: Option<&String>, default: u32) -> Option<u32> {
    value
        .map(|value| {
            value
                .parse::<u32>()
                .ok()
                .filter(|value| (1..=60).contains(value))
        })
        .unwrap_or(Some(default))
}

fn with_alpha(color: Rgba8, alpha: u8) -> Rgba8 {
    Rgba8 { alpha, ..color }
}

fn parse_hex_color(value: &str) -> Option<Rgba8> {
    let value = value.strip_prefix('#')?;
    if value.len() != 6 {
        return None;
    }
    Some(Rgba8::rgb(
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK_THEME: &str = r##"
        mode = "dark"
        accent = "#798186"
        selection = "#343d41"
        muted = "#4b4e55"
        background = "#101315"
        foreground = "#cacccc"
        red = "#de6145"
    "##;

    #[test]
    fn parses_semantic_palette_with_translucent_surfaces() {
        let snapshot = parse_theme(DARK_THEME, 7).unwrap();
        assert_eq!(snapshot.generation, 7);
        assert_eq!(snapshot.background, Rgba8::rgb(0x10, 0x13, 0x15));
        assert_eq!(snapshot.surface.packed(), 0x343d_41b4);
        assert_eq!(snapshot.accent, Rgba8::rgb(0x79, 0x81, 0x86));
        assert_eq!(snapshot.destructive, Rgba8::rgb(0xde, 0x61, 0x45));
        assert_eq!(snapshot.motion, MotionPolicy::Full);

        let reduced = parse_theme(&format!("{DARK_THEME}\nmotion = \"reduced\"\n"), 8).unwrap();
        assert_eq!(reduced.motion, MotionPolicy::Reduced);
    }

    #[test]
    fn rejects_incomplete_palettes() {
        assert!(parse_theme("background = \"#000000\"", 1).is_none());
    }

    #[test]
    fn polling_publishes_a_new_generation_only_after_a_real_change() {
        let path = std::env::temp_dir().join(format!(
            "touchbar-appearance-test-{}-{}.toml",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        std::fs::write(&path, DARK_THEME).unwrap();
        let initial = parse_theme(DARK_THEME, 4).unwrap();
        let mut source = AppearanceSource {
            path: Some(path.clone()),
            snapshot: initial,
            cadence: AnimationCadence::default(),
            last_contents: Some(DARK_THEME.into()),
            last_check: Instant::now() - POLL_INTERVAL,
        };
        assert_eq!(source.poll(), None);

        let changed = DARK_THEME.replace("#798186", "#112233");
        std::fs::write(&path, changed).unwrap();
        source.last_check = Instant::now() - POLL_INTERVAL;
        let snapshot = source.poll().unwrap();
        assert_eq!(snapshot.generation, 5);
        assert_eq!(snapshot.accent, Rgba8::rgb(0x11, 0x22, 0x33));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cadence_is_battery_aware_and_theme_configurable() {
        let defaults = parse_theme_config(DARK_THEME, 1).unwrap().cadence;
        assert_eq!(defaults.hz(PowerState::External), 60);
        assert_eq!(defaults.hz(PowerState::Unknown), 60);
        assert_eq!(defaults.hz(PowerState::Battery), 30);

        let configured = parse_theme_config(
            &format!("{DARK_THEME}\nanimation_hz = 48\nbattery_animation_hz = 24\n"),
            2,
        )
        .unwrap()
        .cadence;
        assert_eq!(configured.hz(PowerState::External), 48);
        assert_eq!(configured.hz(PowerState::Battery), 24);
        assert!(parse_theme_config(&format!("{DARK_THEME}\nanimation_hz = 0\n"), 3).is_none());
        assert!(
            parse_theme_config(&format!("{DARK_THEME}\nbattery_animation_hz = 61\n"), 3).is_none()
        );
    }
}
