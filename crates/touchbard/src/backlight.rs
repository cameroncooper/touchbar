use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const ACTIVE_LEVEL_255: u32 = 128;
const DIMMED_LEVEL_255: u32 = 1;
const IDLE_LEVEL_255: u32 = 0;
const KNOWN_NAMES: &[&str] = &[
    "appletb_backlight",
    "228200000.display-pipe.0",
    "228600000.dsi.0",
];
const KNOWN_DRIVERS: &[&str] = &["hid-appletb-bl", "panel-summit"];

/// Narrow sysfs owner for the Touch Bar OLED backlight. This module never
/// accepts a path from a session client or plugin.
pub struct TouchBarBacklight {
    path: PathBuf,
    brightness: File,
    active_value: u32,
    dimmed_value: u32,
    idle_value: u32,
    current_value: Option<u32>,
}

impl TouchBarBacklight {
    pub fn open() -> Result<Self> {
        let root = Path::new("/sys/class/backlight");
        let mut entries = std::fs::read_dir(root)
            .context("read backlight devices")?
            .flatten()
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !KNOWN_NAMES.contains(&name) {
                continue;
            }
            let driver = std::fs::canonicalize(entry.path().join("device/driver"))
                .ok()
                .and_then(|path| path.file_name().map(OsStr::to_owned));
            if !driver
                .as_deref()
                .and_then(OsStr::to_str)
                .is_some_and(|driver| KNOWN_DRIVERS.contains(&driver))
            {
                continue;
            }
            let max = read_u32(&entry.path().join("max_brightness"))?;
            if max == 0 {
                bail!("Touch Bar backlight reported zero maximum brightness");
            }
            let active_value = ((u64::from(ACTIVE_LEVEL_255) * u64::from(max) + 127) / 255)
                .clamp(1, u64::from(max)) as u32;
            let dimmed_value = ((u64::from(DIMMED_LEVEL_255) * u64::from(max) + 127) / 255)
                .clamp(1, u64::from(max)) as u32;
            let idle_value = u64::from(IDLE_LEVEL_255) * u64::from(max) / 255;
            let brightness_path = entry.path().join("brightness");
            let brightness = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(&brightness_path)
                .with_context(|| format!("open {}", brightness_path.display()))?;
            return Ok(Self {
                path: entry.path(),
                brightness,
                active_value,
                dimmed_value,
                idle_value: idle_value as u32,
                current_value: None,
            });
        }
        bail!("no supported Touch Bar backlight device was found")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn active_value(&self) -> u32 {
        self.active_value
    }

    pub fn set_active(&mut self) -> Result<()> {
        self.set(self.active_value)
    }

    pub fn set_dimmed(&mut self) -> Result<()> {
        self.set(self.dimmed_value)
    }

    pub fn set_idle(&mut self) -> Result<()> {
        self.set(self.idle_value)
    }

    fn set(&mut self, value: u32) -> Result<()> {
        if self.current_value == Some(value) {
            return Ok(());
        }
        // panel-summit's sysfs handler requires the value and newline in one
        // write. `write!` may split formatting into multiple writes, causing
        // the second fragment to fail with EINVAL on real hardware.
        let encoded = format!("{value}\n");
        self.brightness
            .write_all(encoded.as_bytes())
            .context("set Touch Bar active brightness")?;
        self.brightness
            .flush()
            .context("flush Touch Bar brightness")?;
        self.current_value = Some(value);
        Ok(())
    }
}

impl Drop for TouchBarBacklight {
    fn drop(&mut self) {
        // Error exits (including device removal during suspend) do not pass
        // through the orderly service shutdown path. Leave the OLED dark when
        // the sysfs node is still writable; device removal simply makes this a
        // harmless best-effort operation.
        let _ = self.set_idle();
    }
}

fn read_u32(path: &Path) -> Result<u32> {
    std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .parse()
        .with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_is_limited_to_compiled_in_kernel_devices() {
        assert!(KNOWN_NAMES.iter().all(|name| !name.contains('/')));
        assert!(KNOWN_DRIVERS.iter().all(|name| !name.contains('/')));
    }

    #[test]
    fn default_active_level_is_visible_and_bounded() {
        assert!((1..=255).contains(&ACTIVE_LEVEL_255));
        assert!((1..ACTIVE_LEVEL_255).contains(&DIMMED_LEVEL_255));
    }
}
