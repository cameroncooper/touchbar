use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

pub const HOLD_DURATION: Duration = Duration::from_secs(8);
pub const MARKER_NAME: &str = "recovery-fallback";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    EnterFallback,
    ResumeSessions,
}

/// Hardware-owned state for the recovery gesture. One continuous Fn hold can
/// toggle at most once; the key must be released before the inverse action can
/// be armed.
pub struct RecoveryGesture {
    pressed_since: Option<Instant>,
    fired_for_press: bool,
    fallback_locked: bool,
}

impl RecoveryGesture {
    pub fn new(fn_pressed: bool, now: Instant) -> Self {
        Self {
            pressed_since: fn_pressed.then_some(now),
            fired_for_press: false,
            fallback_locked: false,
        }
    }

    pub fn fn_changed(&mut self, pressed: bool, now: Instant) {
        if pressed {
            self.pressed_since = Some(now);
            self.fired_for_press = false;
        } else {
            self.pressed_since = None;
            self.fired_for_press = false;
        }
    }

    pub fn poll(&mut self, now: Instant) -> Option<RecoveryAction> {
        let pressed_since = self.pressed_since?;
        if self.fired_for_press || now.saturating_duration_since(pressed_since) < HOLD_DURATION {
            return None;
        }
        self.fired_for_press = true;
        self.fallback_locked = !self.fallback_locked;
        Some(if self.fallback_locked {
            RecoveryAction::EnterFallback
        } else {
            RecoveryAction::ResumeSessions
        })
    }

    pub fn sessions_allowed(&self) -> bool {
        !self.fallback_locked
    }
}

/// Root-owned presence marker used only for status reporting. Session clients
/// cannot create or replace it because the hardware runtime directory is
/// validated before this object is constructed.
pub struct RecoveryMarker {
    path: PathBuf,
    owner: u32,
}

impl RecoveryMarker {
    pub fn initialize(socket: &Path) -> Result<Self> {
        let parent = socket.parent().context("hardware socket needs a parent")?;
        let marker = Self {
            path: parent.join(MARKER_NAME),
            // SAFETY: geteuid has no preconditions and retains no pointers.
            owner: unsafe { libc::geteuid() },
        };
        marker.clear_stale()?;
        Ok(marker)
    }

    pub fn set_locked(&self, locked: bool) -> Result<()> {
        if !locked {
            return self.remove_owned();
        }
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => bail!("recovery marker {} already exists", self.path.display()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect recovery marker {}", self.path.display()));
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.path)
            .with_context(|| format!("create recovery marker {}", self.path.display()))?;
        file.write_all(b"fallback-locked\n")
            .context("write recovery marker")?;
        Ok(())
    }

    fn clear_stale(&self) -> Result<()> {
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(metadata) if metadata.file_type().is_file() && metadata.uid() == self.owner => {
                fs::remove_file(&self.path).with_context(|| {
                    format!("remove stale recovery marker {}", self.path.display())
                })
            }
            Ok(_) => bail!(
                "refusing unexpected recovery marker {}",
                self.path.display()
            ),
            Err(error) => Err(error)
                .with_context(|| format!("inspect recovery marker {}", self.path.display())),
        }
    }

    fn remove_owned(&self) -> Result<()> {
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(metadata) if metadata.file_type().is_file() && metadata.uid() == self.owner => {
                fs::remove_file(&self.path)
                    .with_context(|| format!("remove recovery marker {}", self.path.display()))
            }
            Ok(_) => bail!(
                "refusing unexpected recovery marker {}",
                self.path.display()
            ),
            Err(error) => Err(error)
                .with_context(|| format!("inspect recovery marker {}", self.path.display())),
        }
    }
}

impl Drop for RecoveryMarker {
    fn drop(&mut self) {
        let _ = self.remove_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_hold_toggles_once_and_release_rearms() {
        let start = Instant::now();
        let mut gesture = RecoveryGesture::new(false, start);
        gesture.fn_changed(true, start);
        assert_eq!(gesture.poll(start + HOLD_DURATION / 2), None);
        assert_eq!(
            gesture.poll(start + HOLD_DURATION),
            Some(RecoveryAction::EnterFallback)
        );
        assert_eq!(gesture.poll(start + HOLD_DURATION * 2), None);
        assert!(!gesture.sessions_allowed());

        gesture.fn_changed(false, start + HOLD_DURATION * 2);
        gesture.fn_changed(true, start + HOLD_DURATION * 3);
        assert_eq!(
            gesture.poll(start + HOLD_DURATION * 4),
            Some(RecoveryAction::ResumeSessions)
        );
        assert!(gesture.sessions_allowed());
    }

    #[test]
    fn interrupted_hold_never_triggers() {
        let start = Instant::now();
        let mut gesture = RecoveryGesture::new(true, start);
        gesture.fn_changed(false, start + HOLD_DURATION - Duration::from_millis(1));
        assert_eq!(gesture.poll(start + HOLD_DURATION * 2), None);
        assert!(gesture.sessions_allowed());
    }

    #[test]
    fn marker_lifecycle_is_owned_and_ephemeral() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("hardware.sock");
        let marker_path = directory.path().join(MARKER_NAME);
        fs::write(&marker_path, b"stale\n").unwrap();

        let marker = RecoveryMarker::initialize(&socket).unwrap();
        assert!(!marker_path.exists());
        marker.set_locked(true).unwrap();
        let metadata = fs::symlink_metadata(&marker_path).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(
            metadata.uid(),
            fs::metadata(directory.path()).unwrap().uid()
        );
        marker.set_locked(false).unwrap();
        assert!(!marker_path.exists());

        marker.set_locked(true).unwrap();
        drop(marker);
        assert!(!marker_path.exists());
    }

    #[test]
    fn marker_initialization_rejects_symlinks_and_directories() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("hardware.sock");
        let marker_path = directory.path().join(MARKER_NAME);
        let target = directory.path().join("target");
        fs::write(&target, b"unchanged\n").unwrap();
        std::os::unix::fs::symlink(&target, &marker_path).unwrap();
        assert!(RecoveryMarker::initialize(&socket).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged\n");

        fs::remove_file(&marker_path).unwrap();
        fs::create_dir(&marker_path).unwrap();
        assert!(RecoveryMarker::initialize(&socket).is_err());
    }
}
