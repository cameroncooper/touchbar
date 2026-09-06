use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

/// A Linux eventfd used to bridge blocking reader threads into the session
/// daemon's single event loop without periodic channel polling.
pub struct EventSignal {
    fd: OwnedFd,
}

impl EventSignal {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            fd: self.fd.try_clone()?,
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn notify(&self) {
        let value = 1_u64.to_ne_bytes();
        loop {
            let written =
                unsafe { libc::write(self.fd.as_raw_fd(), value.as_ptr().cast(), value.len()) };
            if written == value.len() as isize {
                return;
            }
            if written < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
            }
            // A saturated counter already guarantees that the consumer wakes.
            return;
        }
    }

    pub fn drain(&self) -> io::Result<()> {
        let mut value = 0_u64;
        loop {
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    size_of::<u64>(),
                )
            };
            if read == size_of::<u64>() as isize {
                continue;
            }
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(error);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "eventfd returned a partial counter",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_signal_coalesces_and_drains_without_blocking() {
        let signal = EventSignal::new().unwrap();
        let sender = signal.try_clone().unwrap();
        sender.notify();
        sender.notify();
        signal.drain().unwrap();
        signal.drain().unwrap();
    }
}
