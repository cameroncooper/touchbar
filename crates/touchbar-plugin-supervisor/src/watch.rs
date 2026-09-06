use std::{
    ffi::CString,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
    time::Duration,
};

const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

/// Watches a grant store by filename in its parent directory.
///
/// Watching the directory, rather than the file inode, is required because
/// [`touchbar_policy::GrantStore::save`] atomically replaces the file.
pub struct GrantStoreWatcher {
    descriptor: OwnedFd,
    targets: Vec<WatchTarget>,
}

struct WatchTarget {
    watch_descriptor: i32,
    directory: PathBuf,
    filename: Vec<u8>,
}

impl GrantStoreWatcher {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        // SAFETY: inotify_init1 has no pointer arguments and returns a uniquely
        // owned descriptor on success.
        let descriptor = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful inotify_init1 returned a uniquely owned descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let mut watcher = Self {
            descriptor,
            targets: Vec::new(),
        };
        watcher.add(path)?;
        Ok(watcher)
    }

    pub fn add(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "grant path has no parent"))?
            .to_path_buf();
        let filename = path
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "grant path has no filename")
            })?
            .as_bytes()
            .to_vec();
        if self
            .targets
            .iter()
            .any(|target| target.directory == directory && target.filename == filename)
        {
            return Ok(());
        }
        let encoded_directory = CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "grant path contains NUL"))?;
        // SAFETY: encoded_directory is NUL terminated, the descriptor is live,
        // and inotify copies the path during this call.
        let watch_descriptor = unsafe {
            libc::inotify_add_watch(
                self.descriptor.as_raw_fd(),
                encoded_directory.as_ptr(),
                WATCH_MASK,
            )
        };
        if watch_descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        self.targets.push(WatchTarget {
            watch_descriptor,
            directory,
            filename,
        });
        Ok(())
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }

    pub fn directory(&self) -> &Path {
        &self.targets[0].directory
    }

    /// Drains all queued inotify records and reports whether the watched file
    /// may have changed. Unrelated directory traffic is ignored.
    pub fn drain_changes(&mut self) -> io::Result<bool> {
        let mut buffer = [0_u8; 8192];
        let mut changed = false;
        loop {
            // SAFETY: buffer is writable for its full length and descriptor is
            // a live nonblocking inotify descriptor.
            let read = unsafe {
                libc::read(
                    self.descriptor.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(changed);
                }
                return Err(error);
            }
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "grant-store watcher closed",
                ));
            }
            let read = read as usize;
            let mut offset = 0;
            while offset < read {
                let header_size = std::mem::size_of::<libc::inotify_event>();
                if read - offset < header_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated grant-store watch event",
                    ));
                }
                // SAFETY: the bounds check above makes the header readable;
                // read_unaligned handles the byte buffer's alignment.
                let event = unsafe {
                    std::ptr::read_unaligned(
                        buffer.as_ptr().add(offset).cast::<libc::inotify_event>(),
                    )
                };
                let record_size = header_size.checked_add(event.len as usize).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "oversized grant watch event")
                })?;
                if record_size > read - offset {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated grant-store watch filename",
                    ));
                }
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    changed = true;
                }
                if event.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0
                    && self
                        .targets
                        .iter()
                        .any(|target| target.watch_descriptor == event.wd)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "grant-store directory watch was invalidated",
                    ));
                }
                let name = &buffer[offset + header_size..offset + record_size];
                let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
                if self.targets.iter().any(|target| {
                    target.watch_descriptor == event.wd && name == target.filename.as_slice()
                }) && event.mask
                    & (libc::IN_CLOSE_WRITE
                        | libc::IN_MOVED_TO
                        | libc::IN_CREATE
                        | libc::IN_DELETE
                        | libc::IN_ATTRIB)
                    != 0
                {
                    changed = true;
                }
                offset += record_size;
            }
        }
    }

    pub fn wait_for_change(&mut self, timeout: Option<Duration>) -> io::Result<bool> {
        loop {
            if self.drain_changes()? {
                return Ok(true);
            }
            let timeout = timeout.map_or(-1, duration_to_poll_timeout);
            let mut descriptor = libc::pollfd {
                fd: self.descriptor.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor is writable for one pollfd and remains live.
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(false);
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "grant-store watcher poll failed",
                ));
            }
        }
    }
}

fn duration_to_poll_timeout(duration: Duration) -> i32 {
    if duration.is_zero() {
        return 0;
    }
    duration.as_millis().max(1).min(i32::MAX as u128) as i32
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::*;

    #[test]
    fn observes_atomic_replacement_and_ignores_unrelated_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("permissions.toml");
        fs::write(&target, "old").unwrap();
        let mut watcher = GrantStoreWatcher::new(&target).unwrap();

        fs::write(directory.path().join("unrelated"), "noise").unwrap();
        assert!(!watcher.drain_changes().unwrap());

        let replacement = directory.path().join("replacement");
        fs::write(&replacement, "new").unwrap();
        fs::rename(&replacement, &target).unwrap();
        assert!(
            watcher
                .wait_for_change(Some(Duration::from_secs(1)))
                .unwrap()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "new");
    }

    #[test]
    fn one_descriptor_observes_persistent_and_session_stores_in_different_directories() {
        let persistent = tempfile::tempdir().unwrap();
        let session = tempfile::tempdir().unwrap();
        let persistent_path = persistent.path().join("permissions.toml");
        let session_path = session.path().join("session-permissions.toml");
        fs::write(&persistent_path, "old").unwrap();
        fs::write(&session_path, "old").unwrap();
        let mut watcher = GrantStoreWatcher::new(&persistent_path).unwrap();
        watcher.add(&session_path).unwrap();

        fs::write(&session_path, "new").unwrap();
        assert!(
            watcher
                .wait_for_change(Some(Duration::from_secs(1)))
                .unwrap()
        );
        fs::write(&persistent_path, "new").unwrap();
        assert!(
            watcher
                .wait_for_change(Some(Duration::from_secs(1)))
                .unwrap()
        );
    }
}
