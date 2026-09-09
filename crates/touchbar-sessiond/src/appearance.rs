use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use touchbar_broker_schema::{AppearanceColor, AppearancePublish, AppearanceScheme};
use touchbar_protocol::appearance::{AppearanceSnapshot, ColorScheme, MotionPolicy, Rgba8};

use crate::power::PowerState;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Stable identity for one permission-authorized package provider. The
/// provider's paths and parsing logic remain inside its sandboxed worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderIdentity {
    pub plugin: String,
    pub id: String,
    pub label: String,
}

impl ProviderIdentity {
    fn description(&self) -> String {
        format!("provider={}:{} ({})", self.plugin, self.id, self.label)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Source {
    BuiltIn,
    File(PathBuf),
    Provider(ProviderIdentity),
}

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
    source: Source,
    explicit_override: bool,
    snapshot: AppearanceSnapshot,
    cadence: AnimationCadence,
    last_contents: Option<String>,
    last_check: Instant,
}

impl AppearanceSource {
    pub fn discover(provider: Option<ProviderIdentity>) -> Self {
        let environment_path = std::env::var_os("TOUCHBAR_THEME").map(PathBuf::from);
        let config_path = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|root| root.join("touchbar/theme.toml"));
        let explicit_override =
            environment_path.is_some() || config_path.as_deref().is_some_and(|path| path.is_file());
        let source = environment_path
            .map(Source::File)
            .or_else(|| config_path.filter(|path| path.is_file()).map(Source::File))
            .or_else(|| provider.map(Source::Provider))
            .unwrap_or(Source::BuiltIn);
        let contents = match &source {
            Source::File(path) => std::fs::read_to_string(path).ok(),
            Source::BuiltIn | Source::Provider(_) => None,
        };
        let config = contents
            .as_deref()
            .and_then(|contents| parse_theme_config(contents, 1))
            .unwrap_or_default();
        println!(
            "appearance-source={} generation={} scheme={:?} animation_hz={} battery_animation_hz={}",
            source_description(&source),
            config.snapshot.generation,
            config.snapshot.scheme,
            config.cadence.external_hz,
            config.cadence.battery_hz,
        );
        Self {
            source,
            explicit_override,
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
        matches!(self.source, Source::File(_))
            .then(|| POLL_INTERVAL.saturating_sub(self.last_check.elapsed()))
    }

    pub fn poll(&mut self) -> Option<AppearanceSnapshot> {
        if self.last_check.elapsed() < POLL_INTERVAL {
            return None;
        }
        self.last_check = Instant::now();
        let Source::File(path) = &self.source else {
            return None;
        };
        let contents = std::fs::read_to_string(path).ok()?;
        if self.last_contents.as_deref() == Some(&contents) {
            return None;
        }
        let generation = self.snapshot.generation.wrapping_add(1).max(1);
        let next = parse_theme_config(&contents, generation)?;
        self.last_contents = Some(contents);
        if same_appearance(self.snapshot, next.snapshot) && self.cadence == next.cadence {
            return None;
        }
        self.accept(next, "file")
    }

    /// Replace only the automatically selected provider. Explicit environment
    /// and user theme files always retain precedence.
    pub fn set_provider(
        &mut self,
        provider: Option<ProviderIdentity>,
    ) -> Option<AppearanceSnapshot> {
        if self.explicit_override {
            return None;
        }
        let next_source = provider.map(Source::Provider).unwrap_or(Source::BuiltIn);
        if self.source == next_source {
            return None;
        }
        let generation = self.snapshot.generation.wrapping_add(1).max(1);
        self.source = next_source;
        self.last_contents = None;
        self.last_check = Instant::now();
        let config = AppearanceConfig {
            snapshot: AppearanceSnapshot {
                generation,
                ..AppearanceSnapshot::default()
            },
            cadence: AnimationCadence::default(),
        };
        self.accept(config, "provider-selection")
    }

    pub fn publish_provider(
        &mut self,
        identity: &ProviderIdentity,
        publication: &AppearancePublish,
    ) -> Option<AppearanceSnapshot> {
        if self.explicit_override
            || !matches!(&self.source, Source::Provider(active) if active == identity)
            || publication.provider != identity.id
        {
            return None;
        }
        let generation = self.snapshot.generation.wrapping_add(1).max(1);
        let config = config_from_publication(publication, generation);
        if same_appearance(self.snapshot, config.snapshot) {
            return None;
        }
        self.accept(config, "provider-publication")
    }

    fn accept(&mut self, config: AppearanceConfig, reason: &str) -> Option<AppearanceSnapshot> {
        let changed =
            !same_appearance(self.snapshot, config.snapshot) || self.cadence != config.cadence;
        self.snapshot = config.snapshot;
        self.cadence = config.cadence;
        println!(
            "appearance-changed generation={} scheme={:?} source={} reason={reason}",
            self.snapshot.generation,
            self.snapshot.scheme,
            source_description(&self.source),
        );
        changed.then_some(self.snapshot)
    }
}

fn source_description(source: &Source) -> String {
    match source {
        Source::BuiltIn => "built-in".into(),
        Source::File(path) => path.display().to_string(),
        Source::Provider(provider) => provider.description(),
    }
}

fn config_from_publication(publication: &AppearancePublish, generation: u32) -> AppearanceConfig {
    let background = rgba(publication.background);
    let foreground = rgba(publication.foreground);
    let accent = rgba(publication.accent);
    let selection = rgba(publication.selection);
    let muted = rgba(publication.muted);
    AppearanceConfig {
        snapshot: AppearanceSnapshot {
            generation,
            scheme: match publication.scheme {
                AppearanceScheme::Dark => ColorScheme::Dark,
                AppearanceScheme::Light => ColorScheme::Light,
            },
            motion: MotionPolicy::Full,
            background,
            surface: with_alpha(selection, 180),
            surface_hover: with_alpha(muted, 210),
            surface_pressed: with_alpha(accent, 230),
            foreground,
            muted,
            accent,
            destructive: rgba(publication.destructive),
            corner_radius_millipixels: 9_000,
        },
        cadence: AnimationCadence::default(),
    }
}

fn rgba(color: AppearanceColor) -> Rgba8 {
    Rgba8::rgb(color.red, color.green, color.blue)
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
    }

    #[test]
    fn accepts_typed_provider_palette_for_only_the_active_provider() {
        let provider = ProviderIdentity {
            plugin: "github:owner/omarchy".into(),
            id: "omarchy".into(),
            label: "Omarchy".into(),
        };
        let publication = AppearancePublish {
            provider: "omarchy".into(),
            scheme: AppearanceScheme::Dark,
            background: AppearanceColor {
                red: 1,
                green: 2,
                blue: 3,
            },
            foreground: AppearanceColor {
                red: 4,
                green: 5,
                blue: 6,
            },
            accent: AppearanceColor {
                red: 7,
                green: 8,
                blue: 9,
            },
            selection: AppearanceColor {
                red: 10,
                green: 11,
                blue: 12,
            },
            muted: AppearanceColor {
                red: 13,
                green: 14,
                blue: 15,
            },
            destructive: AppearanceColor {
                red: 16,
                green: 17,
                blue: 18,
            },
        };
        let mut source = AppearanceSource {
            source: Source::Provider(provider.clone()),
            explicit_override: false,
            snapshot: AppearanceSnapshot::default(),
            cadence: AnimationCadence::default(),
            last_contents: None,
            last_check: Instant::now(),
        };
        let snapshot = source.publish_provider(&provider, &publication).unwrap();
        assert_eq!(snapshot.background, Rgba8::rgb(1, 2, 3));
        assert_eq!(snapshot.accent, Rgba8::rgb(7, 8, 9));
        let other = ProviderIdentity {
            id: "other".into(),
            ..provider
        };
        assert_eq!(source.publish_provider(&other, &publication), None);
    }

    #[test]
    fn cadence_is_battery_aware_and_theme_configurable() {
        let defaults = parse_theme_config(DARK_THEME, 1).unwrap().cadence;
        assert_eq!(defaults.hz(PowerState::External), 60);
        assert_eq!(defaults.hz(PowerState::Battery), 30);
        let configured = parse_theme_config(
            &format!("{DARK_THEME}\nanimation_hz = 48\nbattery_animation_hz = 24\n"),
            2,
        )
        .unwrap()
        .cadence;
        assert_eq!(configured.hz(PowerState::External), 48);
        assert_eq!(configured.hz(PowerState::Battery), 24);
    }
}
