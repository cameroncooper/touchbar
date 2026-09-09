use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use touchbar_package::GithubSource;

use crate::{CapabilityId, GrantRecord, grant::PersistentGrants};

const STORE_SCHEMA_VERSION: u32 = 1;
const MAX_STORE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GrantStore {
    records: BTreeMap<(GithubSource, CapabilityId), GrantRecord>,
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Parse(toml::de::Error),
    Encode(toml::ser::Error),
    Invalid(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "grant store I/O failed: {error}"),
            Self::Parse(error) => write!(formatter, "grant store parse failed: {error}"),
            Self::Encode(error) => write!(formatter, "grant store encoding failed: {error}"),
            Self::Invalid(message) => write!(formatter, "invalid grant store: {message}"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDocument {
    schema_version: u32,
    #[serde(default)]
    grants: Vec<GrantRecord>,
}

impl GrantStore {
    pub fn from_toml(source: &str) -> Result<Self, StoreError> {
        let document = toml::from_str::<StoredDocument>(source).map_err(StoreError::Parse)?;
        if document.schema_version != STORE_SCHEMA_VERSION {
            return Err(StoreError::Invalid(format!(
                "unsupported schema version {}; expected {STORE_SCHEMA_VERSION}",
                document.schema_version
            )));
        }
        let mut store = Self::default();
        for record in document.grants {
            record.validate().map_err(StoreError::Invalid)?;
            let key = (record.source.clone(), record.capability.clone());
            if store.records.insert(key, record).is_some() {
                return Err(StoreError::Invalid(
                    "duplicate source and capability decision".into(),
                ));
            }
        }
        Ok(store)
    }

    pub fn to_toml(&self) -> Result<String, StoreError> {
        let document = StoredDocument {
            schema_version: STORE_SCHEMA_VERSION,
            grants: self.records.values().cloned().collect(),
        };
        toml::to_string_pretty(&document).map_err(StoreError::Encode)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(StoreError::Invalid(format!(
                    "{} must not be a symlink",
                    path.display()
                )));
            }
            Err(error) => return Err(StoreError::Io(error)),
        };
        let metadata = file.metadata()?;
        validate_private_file(path, &metadata)?;
        if metadata.len() > MAX_STORE_BYTES {
            return Err(StoreError::Invalid(format!(
                "{} exceeds the {} byte limit",
                path.display(),
                MAX_STORE_BYTES
            )));
        }
        let mut source = String::with_capacity(metadata.len() as usize);
        file.take(MAX_STORE_BYTES + 1).read_to_string(&mut source)?;
        if source.len() as u64 > MAX_STORE_BYTES {
            return Err(StoreError::Invalid("grant store exceeds size limit".into()));
        }
        Self::from_toml(&source)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), StoreError> {
        let path = path.as_ref();
        let parent = store_parent(path)?;
        ensure_private_directory(parent)?;
        let _lock = StoreLock::acquire(parent.join(".permissions.lock"))?;
        self.save_locked(path, parent)
    }

    /// Atomically loads, updates, and saves one record under the same lock.
    /// This prevents two concurrent consent commands from losing each other's
    /// decisions between an unlocked read and a separately locked write.
    pub fn update_record(
        path: impl AsRef<Path>,
        record: GrantRecord,
    ) -> Result<Option<GrantRecord>, StoreError> {
        Ok(Self::update_records(path, [record])?
            .into_iter()
            .next()
            .expect("one record produces one result"))
    }

    /// Atomically loads, updates, and saves a complete consent decision batch.
    /// Installers use this so a multi-capability confirmation cannot persist
    /// only a prefix of the grants the user reviewed.
    pub fn update_records(
        path: impl AsRef<Path>,
        records: impl IntoIterator<Item = GrantRecord>,
    ) -> Result<Vec<Option<GrantRecord>>, StoreError> {
        let records = records.into_iter().collect::<Vec<_>>();
        for record in &records {
            record.validate().map_err(StoreError::Invalid)?;
        }
        let path = path.as_ref();
        let parent = store_parent(path)?;
        ensure_private_directory(parent)?;
        let _lock = StoreLock::acquire(parent.join(".permissions.lock"))?;
        let mut store = Self::load(path)?;
        let mut previous = Vec::with_capacity(records.len());
        for record in records {
            previous.push(store.insert(record)?);
        }
        store.save_locked(path, parent)?;
        Ok(previous)
    }

    /// Atomically removes one decision while preserving concurrent updates to
    /// every other package/capability record.
    pub fn remove_record(
        path: impl AsRef<Path>,
        source: &GithubSource,
        capability: &CapabilityId,
    ) -> Result<Option<GrantRecord>, StoreError> {
        let path = path.as_ref();
        let parent = store_parent(path)?;
        ensure_private_directory(parent)?;
        let _lock = StoreLock::acquire(parent.join(".permissions.lock"))?;
        let mut store = Self::load(path)?;
        let previous = store.remove(source, capability);
        store.save_locked(path, parent)?;
        Ok(previous)
    }

    fn save_locked(&self, path: &Path, parent: &Path) -> Result<(), StoreError> {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            validate_private_file(path, &metadata)?;
        }
        let encoded = self.to_toml()?;
        if encoded.len() as u64 > MAX_STORE_BYTES {
            return Err(StoreError::Invalid("grant store exceeds size limit".into()));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(encoded.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|error| StoreError::Io(error.error))?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn insert(&mut self, record: GrantRecord) -> Result<Option<GrantRecord>, StoreError> {
        record.validate().map_err(StoreError::Invalid)?;
        Ok(self
            .records
            .insert((record.source.clone(), record.capability.clone()), record))
    }

    pub fn remove(
        &mut self,
        source: &GithubSource,
        capability: &CapabilityId,
    ) -> Option<GrantRecord> {
        self.records.remove(&(source.clone(), capability.clone()))
    }

    pub fn get(&self, source: &GithubSource, capability: &CapabilityId) -> Option<&GrantRecord> {
        self.records.get(&(source.clone(), capability.clone()))
    }

    pub fn records(&self) -> impl Iterator<Item = &GrantRecord> {
        self.records.values()
    }
}

fn store_parent(path: &Path) -> Result<&Path, StoreError> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| StoreError::Invalid("grant store requires a parent directory".into()))
}

impl PersistentGrants for GrantStore {
    fn grant(&self, source: &GithubSource, capability: &CapabilityId) -> Option<&GrantRecord> {
        self.get(source, capability)
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| StoreError::Invalid("grant store directory has no parent".into()))?;
            if !parent.is_dir() {
                return Err(StoreError::Invalid(format!(
                    "parent {} is not a directory",
                    parent.display()
                )));
            }
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                // Another consent writer may have created the same directory
                // after our lstat. Validate that object below before use.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(StoreError::Io(error)),
            }
            let metadata = fs::symlink_metadata(path)?;
            validate_private_directory(path, &metadata)
        }
        Err(error) => Err(StoreError::Io(error)),
    }
}

fn validate_private_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), StoreError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::Invalid(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    validate_owner_and_mode(path, metadata, 0o077)
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata) -> Result<(), StoreError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::Invalid(format!(
            "{} must be a regular file, not a symlink",
            path.display()
        )));
    }
    validate_owner_and_mode(path, metadata, 0o177)
}

fn validate_owner_and_mode(
    path: &Path,
    metadata: &fs::Metadata,
    forbidden_mode: u32,
) -> Result<(), StoreError> {
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(StoreError::Invalid(format!(
            "{} is not owned by the current user",
            path.display()
        )));
    }
    if metadata.mode() & forbidden_mode != 0 {
        return Err(StoreError::Invalid(format!(
            "{} permissions are too broad ({:o})",
            path.display(),
            metadata.mode() & 0o777
        )));
    }
    Ok(())
}

struct StoreLock {
    file: fs::File,
}

impl StoreLock {
    fn acquire(path: PathBuf) -> Result<Self, StoreError> {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        validate_private_file(Path::new("grant store lock"), &file.metadata()?)?;
        // SAFETY: flock operates on this live file descriptor and retains no pointer.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(StoreError::Io(io::Error::last_os_error()));
        }
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // SAFETY: the file descriptor remains live until after Drop returns.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}
