use std::{
    fs::{File, OpenOptions},
    io, mem,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use input_linux_sys::{input_event, timeval};

const EV_KEY: u16 = 1;
const KEY_FN: u16 = 0x1d0;
const KEY_CAPABILITY_BYTES: usize = 96;

/// Reads only the physical Fn state. Other keyboard events never leave this
/// module or cross the privileged hardware boundary.
pub struct FnInput {
    file: File,
    path: PathBuf,
    pressed: bool,
}

impl FnInput {
    pub fn open() -> Result<Self> {
        let path = find_fn_input()?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("open Fn input {}", path.display()))?;
        let pressed = query_key_state(&file, KEY_FN)?;
        println!("fn-input=ready device={} pressed={pressed}", path.display());
        Ok(Self {
            file,
            path,
            pressed,
        })
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    pub fn pressed(&self) -> bool {
        self.pressed
    }

    /// Returns only actual state transitions, coalescing an arbitrary number
    /// of unrelated keyboard events read in the same batch.
    pub fn read_changed(&mut self) -> Result<Option<bool>> {
        let mut changed = None;
        let mut events = [zero_event(); 64];
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
                return Err(error)
                    .with_context(|| format!("read Fn input {}", self.path.display()));
            }
            if bytes == 0 {
                bail!("Fn input {} closed", self.path.display());
            }
            let bytes = bytes as usize;
            if !bytes.is_multiple_of(mem::size_of::<input_event>()) {
                bail!("Fn input returned a partial event");
            }
            for event in &events[..bytes / mem::size_of::<input_event>()] {
                let Some(pressed) = fn_state(event.type_, event.code, event.value) else {
                    continue;
                };
                if self.pressed != pressed {
                    self.pressed = pressed;
                    changed = Some(pressed);
                }
            }
        }
        Ok(changed)
    }
}

fn find_fn_input() -> Result<PathBuf> {
    let mut events = std::fs::read_dir("/sys/class/input")?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let suffix = name.strip_prefix("event")?;
            (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| (entry.path(), name.to_owned()))
        })
        .collect::<Vec<_>>();
    events.sort_by(|left, right| left.1.cmp(&right.1));

    for (sysfs, event) in events {
        let path = Path::new("/dev/input").join(event);
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        else {
            continue;
        };
        if supports_key(&file, KEY_FN)? {
            let name = std::fs::read_to_string(sysfs.join("device/name")).unwrap_or_default();
            if name.trim() == "TouchBar System Keys" {
                continue;
            }
            return Ok(path);
        }
    }
    bail!("no evdev keyboard advertising KEY_FN was found")
}

fn supports_key(file: &File, key: u16) -> Result<bool> {
    let mut bits = [0_u8; KEY_CAPABILITY_BYTES];
    let request = ioctl_read_request(0x20 + u64::from(EV_KEY), bits.len());
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, bits.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("query evdev key capabilities");
    }
    let byte = usize::from(key / 8);
    Ok(byte < bits.len() && bits[byte] & (1 << (key % 8)) != 0)
}

fn query_key_state(file: &File, key: u16) -> Result<bool> {
    let mut bits = [0_u8; KEY_CAPABILITY_BYTES];
    let request = ioctl_read_request(0x18, bits.len());
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, bits.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("query evdev key state");
    }
    let byte = usize::from(key / 8);
    Ok(byte < bits.len() && bits[byte] & (1 << (key % 8)) != 0)
}

const fn ioctl_read_request(number: u64, size: usize) -> u64 {
    const IOC_READ: u64 = 2;
    const IOC_DIR_SHIFT: u64 = 30;
    const IOC_SIZE_SHIFT: u64 = 16;
    const IOC_TYPE_SHIFT: u64 = 8;
    (IOC_READ << IOC_DIR_SHIFT)
        | ((size as u64) << IOC_SIZE_SHIFT)
        | ((b'E' as u64) << IOC_TYPE_SHIFT)
        | number
}

const fn fn_state(kind: u16, code: u16, value: i32) -> Option<bool> {
    match (kind, code, value) {
        (EV_KEY, KEY_FN, 0) => Some(false),
        (EV_KEY, KEY_FN, 1) => Some(true),
        _ => None,
    }
}

const fn zero_event() -> input_event {
    input_event {
        time: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: 0,
        code: 0,
        value: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fn_press_and_release_are_observable() {
        assert_eq!(fn_state(EV_KEY, KEY_FN, 1), Some(true));
        assert_eq!(fn_state(EV_KEY, KEY_FN, 0), Some(false));
        assert_eq!(fn_state(EV_KEY, KEY_FN, 2), None);
        assert_eq!(fn_state(EV_KEY, 30, 1), None);
        assert_eq!(fn_state(0, KEY_FN, 1), None);
    }

    #[test]
    fn key_capability_buffer_covers_fn() {
        assert!(usize::from(KEY_FN / 8) < KEY_CAPABILITY_BYTES);
    }
}
