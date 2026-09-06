use std::{
    fs::OpenOptions,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
};

use anyhow::Result;
use input::{Libinput, LibinputInterface, event::Event};

/// Libinput's device opener is deliberately narrower than its callback
/// contract: the hardware service may observe only concrete evdev nodes.
struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        if !is_evdev_node(path) || flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(libc::EACCES);
        }
        OpenOptions::new()
            .read(true)
            .custom_flags(flags | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map(Into::into)
            .map_err(|error| error.raw_os_error().unwrap_or(libc::EIO))
    }

    fn close_restricted(&mut self, descriptor: OwnedFd) {
        drop(descriptor);
    }
}

/// Discard-only activity observer for backlight policy. Event payloads never
/// leave this module; the caller learns only whether real seat activity was
/// present in the drained batch.
pub struct SeatActivity {
    context: Libinput,
}

impl SeatActivity {
    pub fn open() -> Result<Self> {
        let mut context = Libinput::new_with_udev(Interface);
        context
            .udev_assign_seat("seat0")
            .map_err(|_| anyhow::anyhow!("assign libinput seat0"))?;
        println!("seat-activity=ready seat=seat0 payload=discarded");
        Ok(Self { context })
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.context.as_fd().as_raw_fd()
    }

    pub fn read_activity(&mut self) -> Result<bool> {
        self.context
            .dispatch()
            .map_err(|_| anyhow::anyhow!("dispatch libinput seat0"))?;
        let mut active = false;
        for event in &mut self.context {
            active |= matches!(
                event,
                Event::Keyboard(_) | Event::Pointer(_) | Event::Gesture(_) | Event::Touch(_)
            );
        }
        Ok(active)
    }
}

fn is_evdev_node(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    path.parent() == Some(Path::new("/dev/input"))
        && name.strip_prefix("event").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libinput_opener_accepts_only_concrete_evdev_nodes() {
        assert!(is_evdev_node(Path::new("/dev/input/event0")));
        assert!(is_evdev_node(Path::new("/dev/input/event123")));
        for path in [
            "/dev/input/event",
            "/dev/input/eventx",
            "/dev/input/../uinput",
            "/dev/uinput",
            "/tmp/event0",
        ] {
            assert!(!is_evdev_node(Path::new(path)), "accepted {path}");
        }
    }

    #[test]
    fn libinput_opener_rejects_write_access_before_opening() {
        let mut interface = Interface;
        assert_eq!(
            interface
                .open_restricted(Path::new("/dev/input/event0"), libc::O_WRONLY)
                .unwrap_err(),
            libc::EACCES
        );
        assert_eq!(
            interface
                .open_restricted(Path::new("/dev/input/event0"), libc::O_RDWR)
                .unwrap_err(),
            libc::EACCES
        );
    }

    #[test]
    #[ignore = "requires readable live seat0 evdev devices"]
    fn live_seat_opens_and_discards_initial_events() {
        let mut activity = SeatActivity::open().unwrap();
        assert!(activity.as_raw_fd() >= 0);
        let _ = activity.read_activity().unwrap();
    }
}
