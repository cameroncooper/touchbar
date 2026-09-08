use std::{
    collections::HashMap,
    ffi::CString,
    fs::File,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use touchbar_package::AppearanceProviderFields;
use touchbar_policy::FilesystemMountBinding;
use touchbar_protocol::appearance::{AppearanceSnapshot, ColorScheme, MotionPolicy, Rgba8};

use crate::power::PowerState;

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// One permission-authorized, package-declared appearance source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSource {
    pub plugin: String,
    pub id: String,
    pub label: String,
    pub mount: FilesystemMountBinding,
    pub path: PathBuf,
    pub fields: AppearanceProviderFields,
    pub maximum_file_bytes: u64,
    pub maximum_updates_per_second: u16,
}

impl ProviderSource {
    fn description(&self) -> String {
        format!("provider={}:{} ({})", self.plugin, self.id, self.label)
    }

    fn read_to_string(&self) -> Result<String> {
        read_bounded_beneath(&self.mount, &self.path, self.maximum_file_bytes)
            .with_context(|| self.description())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Source {
    BuiltIn,
    File(PathBuf),
    Provider(ProviderSource),
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
    pub fn discover(provider: Option<ProviderSource>) -> Self {
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
        let contents = read_source(&source).ok();
        let config = contents
            .as_deref()
            .and_then(|contents| parse_source_config(&source, contents, 1))
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
        (!matches!(self.source, Source::BuiltIn)).then(|| {
            self.poll_interval()
                .saturating_sub(self.last_check.elapsed())
        })
    }

    pub fn poll(&mut self) -> Option<AppearanceSnapshot> {
        if self.last_check.elapsed() < self.poll_interval() {
            return None;
        }
        self.last_check = Instant::now();
        if matches!(self.source, Source::BuiltIn) {
            return None;
        }
        let contents = read_source(&self.source).ok()?;
        if self.last_contents.as_deref() == Some(&contents) {
            return None;
        }
        let generation = self.snapshot.generation.wrapping_add(1).max(1);
        let next = parse_source_config(&self.source, &contents, generation)?;
        self.last_contents = Some(contents);
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
            source_description(&self.source)
        );
        Some(next.snapshot)
    }

    /// Replace only the automatically selected provider. Explicit environment
    /// and user theme files always retain precedence.
    pub fn set_provider(&mut self, provider: Option<ProviderSource>) -> Option<AppearanceSnapshot> {
        if self.explicit_override {
            return None;
        }
        let next_source = provider.map(Source::Provider).unwrap_or(Source::BuiltIn);
        if self.source == next_source {
            return None;
        }
        let contents = read_source(&next_source).ok();
        let generation = self.snapshot.generation.wrapping_add(1).max(1);
        let config = contents
            .as_deref()
            .and_then(|contents| parse_source_config(&next_source, contents, generation))
            .unwrap_or_else(|| AppearanceConfig {
                snapshot: AppearanceSnapshot {
                    generation,
                    ..AppearanceSnapshot::default()
                },
                cadence: AnimationCadence::default(),
            });
        self.source = next_source;
        self.last_contents = contents;
        self.last_check = Instant::now();
        let changed =
            !same_appearance(self.snapshot, config.snapshot) || self.cadence != config.cadence;
        self.snapshot = config.snapshot;
        self.cadence = config.cadence;
        println!(
            "appearance-source={} generation={} scheme={:?}",
            source_description(&self.source),
            self.snapshot.generation,
            self.snapshot.scheme
        );
        changed.then_some(self.snapshot)
    }

    fn poll_interval(&self) -> Duration {
        match &self.source {
            Source::Provider(provider) => Duration::from_nanos(
                1_000_000_000 / u64::from(provider.maximum_updates_per_second.max(1)),
            ),
            Source::File(_) => POLL_INTERVAL,
            Source::BuiltIn => POLL_INTERVAL,
        }
    }
}

fn source_description(source: &Source) -> String {
    match source {
        Source::BuiltIn => "built-in".into(),
        Source::File(path) => path.display().to_string(),
        Source::Provider(provider) => provider.description(),
    }
}

fn read_source(source: &Source) -> Result<String> {
    match source {
        Source::BuiltIn => bail!("built-in appearance has no source document"),
        Source::File(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read appearance file {}", path.display())),
        Source::Provider(provider) => provider.read_to_string(),
    }
}

fn parse_source_config(
    source: &Source,
    contents: &str,
    generation: u32,
) -> Option<AppearanceConfig> {
    match source {
        Source::Provider(provider) => parse_provider_config(contents, &provider.fields, generation),
        Source::BuiltIn | Source::File(_) => parse_theme_config(contents, generation),
    }
}

fn same_appearance(mut left: AppearanceSnapshot, mut right: AppearanceSnapshot) -> bool {
    left.generation = 0;
    right.generation = 0;
    left == right
}

fn parse_provider_config(
    contents: &str,
    fields: &AppearanceProviderFields,
    generation: u32,
) -> Option<AppearanceConfig> {
    let document = contents.parse::<toml::Table>().ok()?;
    let string = |key: &str| document.get(key)?.as_str();
    let color = |key: &str| parse_hex_color(string(key)?);
    let background = color(&fields.background)?;
    let accent = color(&fields.accent)?;
    let foreground = color(&fields.foreground)?;
    let selection = color(&fields.selection).unwrap_or(accent);
    let muted = color(&fields.muted).unwrap_or(foreground);
    let destructive = color(&fields.destructive).unwrap_or(Rgba8::rgb(230, 80, 80));
    let scheme = match string(&fields.scheme) {
        Some("dark") => ColorScheme::Dark,
        Some("light") => ColorScheme::Light,
        _ => return None,
    };
    Some(AppearanceConfig {
        snapshot: AppearanceSnapshot {
            generation,
            scheme,
            motion: MotionPolicy::Full,
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
        cadence: AnimationCadence::default(),
    })
}

fn read_bounded_beneath(
    binding: &FilesystemMountBinding,
    relative: &Path,
    maximum_bytes: u64,
) -> Result<String> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("appearance provider path is not normalized and relative");
    }
    let root = openat2(
        libc::AT_FDCWD,
        &binding.path,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    )?;
    let metadata = descriptor_metadata(root.as_raw_fd())?;
    if metadata.st_dev != binding.device || metadata.st_ino != binding.inode {
        bail!("appearance provider filesystem grant root was replaced");
    }
    let file = openat2(
        root.as_raw_fd(),
        relative,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV,
    )?;
    let metadata = descriptor_metadata(file.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG || metadata.st_nlink != 1 {
        bail!("appearance provider source must be a single-link regular file");
    }
    let maximum = usize::try_from(maximum_bytes).unwrap_or(usize::MAX);
    if metadata.st_size < 0 || metadata.st_size as u64 > maximum_bytes {
        bail!("appearance provider source exceeds its granted size limit");
    }
    let mut bytes = Vec::with_capacity((metadata.st_size as usize).min(maximum));
    // SAFETY: `file` is a uniquely owned readable descriptor returned by openat2.
    let mut file = unsafe { File::from_raw_fd(file.into_raw_fd()) };
    file.by_ref()
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        bail!("appearance provider source grew beyond its granted size limit");
    }
    String::from_utf8(bytes).context("appearance provider source is not UTF-8")
}

fn openat2(directory: i32, path: &Path, flags: i32, resolve: u64) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: every pointer references initialized storage for the duration of
    // the syscall and a successful descriptor is uniquely owned below.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor =
        i32::try_from(descriptor).map_err(|_| io::Error::other("openat2 descriptor overflow"))?;
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat2 returned one live descriptor owned by this function.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn descriptor_metadata(descriptor: i32) -> io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to writable storage and descriptor is live.
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat initialized metadata on success.
    Ok(unsafe { metadata.assume_init() })
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
            source: Source::File(path.clone()),
            explicit_override: true,
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

    fn provider(root: &Path) -> ProviderSource {
        ProviderSource {
            plugin: "github:owner/omarchy".into(),
            id: "omarchy".into(),
            label: "Omarchy".into(),
            mount: FilesystemMountBinding::from_directory(root).unwrap(),
            path: PathBuf::from("theme/colors.toml"),
            fields: AppearanceProviderFields::default(),
            maximum_file_bytes: 64 * 1024,
            maximum_updates_per_second: 4,
        }
    }

    #[test]
    fn provider_parses_only_palette_and_keeps_host_motion_policy() {
        let config =
            parse_provider_config(DARK_THEME, &AppearanceProviderFields::default(), 9).unwrap();
        assert_eq!(config.snapshot.generation, 9);
        assert_eq!(config.snapshot.scheme, ColorScheme::Dark);
        assert_eq!(config.snapshot.accent, Rgba8::rgb(0x79, 0x81, 0x86));
        assert_eq!(config.snapshot.motion, MotionPolicy::Full);
        assert_eq!(config.cadence, AnimationCadence::default());

        let malformed = DARK_THEME.replace("mode = \"dark\"", "mode = \"sepia\"");
        assert!(
            parse_provider_config(&malformed, &AppearanceProviderFields::default(), 10).is_none()
        );
    }

    #[test]
    fn provider_survives_atomic_theme_directory_replacement_and_invalid_updates() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("theme")).unwrap();
        std::fs::write(root.path().join("theme/colors.toml"), DARK_THEME).unwrap();
        let provider = provider(root.path());
        let initial = parse_provider_config(DARK_THEME, &provider.fields, 1)
            .unwrap()
            .snapshot;
        let mut source = AppearanceSource {
            source: Source::Provider(provider),
            explicit_override: false,
            snapshot: initial,
            cadence: AnimationCadence::default(),
            last_contents: Some(DARK_THEME.into()),
            last_check: Instant::now() - POLL_INTERVAL,
        };

        let next = root.path().join("next-theme");
        std::fs::create_dir(&next).unwrap();
        let malformed = DARK_THEME.replace("#798186", "not-a-color");
        std::fs::write(next.join("colors.toml"), malformed).unwrap();
        std::fs::rename(root.path().join("theme"), root.path().join("old-theme")).unwrap();
        std::fs::rename(&next, root.path().join("theme")).unwrap();
        source.last_check = Instant::now() - POLL_INTERVAL;
        assert_eq!(source.poll(), None);
        assert_eq!(source.snapshot.generation, 1);

        let changed = DARK_THEME.replace("#798186", "#112233");
        std::fs::write(root.path().join("theme/colors.toml"), changed).unwrap();
        source.last_check = Instant::now() - POLL_INTERVAL;
        let snapshot = source.poll().unwrap();
        assert_eq!(snapshot.generation, 2);
        assert_eq!(snapshot.accent, Rgba8::rgb(0x11, 0x22, 0x33));
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
