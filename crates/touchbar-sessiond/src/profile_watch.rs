//! Event-driven watching for the user profile document.

use std::{
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

pub struct ProfileWatcher {
    target: PathBuf,
    watched_directory: PathBuf,
    fd: OwnedFd,
}

impl ProfileWatcher {
    pub fn new(target: &Path) -> Result<Self> {
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            std::env::current_dir()?.join(target)
        };
        target
            .file_name()
            .context("profile watch target needs a file name")?;
        let watched_directory = nearest_existing_directory(
            target
                .parent()
                .context("profile watch target needs a parent")?,
        )?;
        let fd = watch_directory(&watched_directory)?;
        Ok(Self {
            target,
            watched_directory,
            fd,
        })
    }

    pub fn notification_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drains all queued filesystem events. Any event in the narrowest
    /// existing ancestor is sufficient: the profile loader performs strict
    /// validation, and rearming descends as newly created directories appear.
    pub fn drain(&mut self) -> Result<bool> {
        let mut changed = false;
        let mut bytes = [0_u8; 4096];
        loop {
            let count =
                unsafe { libc::read(self.fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if count > 0 {
                changed = true;
                continue;
            }
            if count == 0 {
                bail!("profile filesystem watch closed unexpectedly");
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("read profile filesystem events");
            }
        }
        if changed {
            self.rearm_to_nearest_parent()?;
        }
        Ok(changed)
    }

    fn rearm_to_nearest_parent(&mut self) -> Result<()> {
        let directory = nearest_existing_directory(
            self.target
                .parent()
                .context("profile watch target needs a parent")?,
        )?;
        if directory != self.watched_directory {
            self.fd = watch_directory(&directory)?;
            self.watched_directory = directory;
        }
        Ok(())
    }
}

fn nearest_existing_directory(path: &Path) -> Result<PathBuf> {
    let mut candidate = path;
    loop {
        match fs::metadata(candidate) {
            Ok(metadata) if metadata.is_dir() => return Ok(candidate.to_path_buf()),
            Ok(_) => bail!("{} is not a directory", candidate.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                candidate = candidate.parent().with_context(|| {
                    format!("no existing parent for profile path {}", path.display())
                })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect profile parent {}", candidate.display()));
            }
        }
    }
}

fn watch_directory(path: &Path) -> Result<OwnedFd> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create profile filesystem watch");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .context("profile parent path contains a NUL byte")?;
    let descriptor = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), WATCH_MASK) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("watch profile parent directory");
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, time::Duration};

    fn wait_readable(fd: RawFd) {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, Duration::from_secs(1).as_millis() as i32) };
        assert_eq!(ready, 1);
    }

    #[test]
    fn descends_into_new_parents_and_observes_atomic_saves() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("touchbar");
        let target = directory.join("profiles.toml");
        let mut watcher = ProfileWatcher::new(&target).unwrap();

        fs::create_dir(&directory).unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
        assert_eq!(watcher.watched_directory, directory);

        let temporary = directory.join("profiles.toml.new");
        let mut file = fs::File::create(&temporary).unwrap();
        file.write_all(b"version = 1\n").unwrap();
        file.sync_all().unwrap();
        fs::rename(temporary, target).unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
    }
}
