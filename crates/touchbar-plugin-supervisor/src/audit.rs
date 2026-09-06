use std::{
    collections::VecDeque,
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};
use touchbar_package::GithubSource;
use touchbar_protocol::broker_ipc::{ActivationOrigin, BrokerErrorCode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuditFileLimits {
    pub maximum_bytes: u64,
    pub maximum_age: Duration,
    pub maximum_archives: usize,
}

impl Default for AuditFileLimits {
    fn default() -> Self {
        Self {
            maximum_bytes: 4 * 1024 * 1024,
            maximum_age: Duration::from_secs(7 * 24 * 60 * 60),
            maximum_archives: 5,
        }
    }
}

#[derive(Debug)]
pub enum AuditFileError {
    Io(io::Error),
    Encode(serde_json::Error),
    Invalid(String),
}

impl fmt::Display for AuditFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "audit file I/O failed: {error}"),
            Self::Encode(error) => write!(formatter, "audit record encoding failed: {error}"),
            Self::Invalid(message) => write!(formatter, "invalid audit file: {message}"),
        }
    }
}

impl Error for AuditFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<io::Error> for AuditFileError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for AuditFileError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encode(error)
    }
}

/// A user-owned, cross-process-safe JSON-lines audit sink.
///
/// The sink opens the active file for each append under a separate stable
/// lock, which prevents another supervisor from retaining an old inode across
/// rotation.
#[derive(Clone, Debug)]
pub struct AuditFile {
    path: PathBuf,
    lock_path: PathBuf,
    limits: AuditFileLimits,
}

impl AuditFile {
    pub fn new(path: impl AsRef<Path>, limits: AuditFileLimits) -> Result<Self, AuditFileError> {
        if limits.maximum_bytes == 0 || limits.maximum_age.is_zero() {
            return Err(AuditFileError::Invalid(
                "size and age limits must be greater than zero".into(),
            ));
        }
        if limits.maximum_archives > 32 {
            return Err(AuditFileError::Invalid(
                "audit archive count exceeds the limit of 32".into(),
            ));
        }
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| AuditFileError::Invalid("audit file requires a parent".into()))?;
        let filename = path
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| AuditFileError::Invalid("audit file requires a filename".into()))?;
        ensure_private_directory(parent)?;
        let mut lock_name = OsString::from(".");
        lock_name.push(filename);
        lock_name.push(".lock");
        let sink = Self {
            path: path.to_path_buf(),
            lock_path: parent.join(lock_name),
            limits,
        };
        let _lock = sink.lock()?;
        let file = sink.open_active()?;
        validate_private_file(&sink.path, &file.metadata()?)?;
        Ok(sink)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &AuditRecord) -> Result<(), AuditFileError> {
        let _lock = self.lock()?;
        let mut file = self.open_active()?;
        let last_sequence = self.last_durable_sequence(&mut file)?;
        let mut record = record.clone();
        record.sequence = match last_sequence {
            Some(sequence) => sequence.checked_add(1).ok_or_else(|| {
                AuditFileError::Invalid("durable audit sequence exhausted".into())
            })?,
            None => record.sequence.max(1),
        };
        let mut encoded = serde_json::to_vec(&record)?;
        encoded.push(b'\n');
        if encoded.len() as u64 > self.limits.maximum_bytes {
            return Err(AuditFileError::Invalid(
                "one audit record exceeds the file size limit".into(),
            ));
        }
        let metadata = file.metadata()?;
        validate_private_file(&self.path, &metadata)?;
        if should_rotate(&metadata, encoded.len() as u64, self.limits) {
            drop(file);
            self.rotate()?;
            file = self.open_active()?;
        }
        file.write_all(&encoded)?;
        file.sync_data()?;
        Ok(())
    }

    fn last_durable_sequence(&self, active: &mut fs::File) -> Result<Option<u64>, AuditFileError> {
        if let Some(sequence) = read_last_sequence(active, &self.path)? {
            return Ok(Some(sequence));
        }
        for index in 1..=self.limits.maximum_archives {
            let path = rotated_path(&self.path, index);
            if !validate_existing_private_file(&path)? {
                continue;
            }
            let mut archive = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&path)?;
            validate_private_file(&path, &archive.metadata()?)?;
            if let Some(sequence) = read_last_sequence(&mut archive, &path)? {
                return Ok(Some(sequence));
            }
        }
        Ok(None)
    }

    fn lock(&self) -> Result<AuditLock, AuditFileError> {
        let existed = validate_existing_private_file(&self.lock_path)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.lock_path)?;
        if !existed {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        validate_private_file(&self.lock_path, &file.metadata()?)?;
        // SAFETY: flock operates on this live descriptor and retains no pointer.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(AuditLock { file })
    }

    fn open_active(&self) -> Result<fs::File, AuditFileError> {
        let existed = validate_existing_private_file(&self.path)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.path)?;
        if !existed {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        validate_private_file(&self.path, &file.metadata()?)?;
        Ok(file)
    }

    fn rotate(&self) -> Result<(), AuditFileError> {
        if self.limits.maximum_archives == 0 {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            let oldest = rotated_path(&self.path, self.limits.maximum_archives);
            remove_valid_archive(&oldest)?;
            for index in (1..self.limits.maximum_archives).rev() {
                let source = rotated_path(&self.path, index);
                let destination = rotated_path(&self.path, index + 1);
                rename_valid_archive(&source, &destination)?;
            }
            let metadata = fs::symlink_metadata(&self.path)?;
            validate_private_file(&self.path, &metadata)?;
            fs::rename(&self.path, rotated_path(&self.path, 1))?;
        }
        let parent = self.path.parent().expect("validated audit parent");
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

struct AuditLock {
    file: fs::File,
}

impl Drop for AuditLock {
    fn drop(&mut self) {
        // SAFETY: flock operates on this live descriptor and retains no pointer.
        let _ = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN) };
    }
}

fn should_rotate(metadata: &fs::Metadata, append_bytes: u64, limits: AuditFileLimits) -> bool {
    let too_large = metadata
        .len()
        .checked_add(append_bytes)
        .is_none_or(|total| total > limits.maximum_bytes);
    let too_old = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= limits.maximum_age);
    metadata.len() > 0 && (too_large || too_old)
}

fn read_last_sequence(file: &mut fs::File, path: &Path) -> Result<Option<u64>, AuditFileError> {
    file.seek(SeekFrom::Start(0))?;
    let mut previous = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.is_empty() {
            return Err(AuditFileError::Invalid(format!(
                "{} contains an empty audit record",
                path.display()
            )));
        }
        let record = serde_json::from_str::<AuditRecord>(&line)?;
        if previous.is_some_and(|sequence| record.sequence <= sequence) {
            return Err(AuditFileError::Invalid(format!(
                "{} has a non-monotonic audit sequence",
                path.display()
            )));
        }
        previous = Some(record.sequence);
    }
    Ok(previous)
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut filename = path
        .file_name()
        .expect("validated audit filename")
        .to_os_string();
    filename.push(format!(".{index}"));
    path.with_file_name(filename)
}

fn remove_valid_archive(path: &Path) -> Result<(), AuditFileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_file(path, &metadata)?;
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn rename_valid_archive(source: &Path, destination: &Path) -> Result<(), AuditFileError> {
    match fs::symlink_metadata(source) {
        Ok(metadata) => {
            validate_private_file(source, &metadata)?;
            fs::rename(source, destination)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), AuditFileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| AuditFileError::Invalid("audit directory has no parent".into()))?;
            if !parent.is_dir() {
                return Err(AuditFileError::Invalid(format!(
                    "audit parent {} is not a directory",
                    parent.display()
                )));
            }
            fs::DirBuilder::new().mode(0o700).create(path)?;
            validate_private_directory(path, &fs::symlink_metadata(path)?)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_private_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), AuditFileError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AuditFileError::Invalid(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    validate_owner_and_mode(path, metadata, 0o077)
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata) -> Result<(), AuditFileError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
        return Err(AuditFileError::Invalid(format!(
            "{} must be an unlinked regular file",
            path.display()
        )));
    }
    validate_owner_and_mode(path, metadata, 0o177)
}

fn validate_existing_private_file(path: &Path) -> Result<bool, AuditFileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_file(path, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn validate_owner_and_mode(
    path: &Path,
    metadata: &fs::Metadata,
    forbidden_mode: u32,
) -> Result<(), AuditFileError> {
    // SAFETY: geteuid has no preconditions and retains no pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(AuditFileError::Invalid(format!(
            "{} is not owned by the current user",
            path.display()
        )));
    }
    if metadata.mode() & forbidden_mode != 0 {
        return Err(AuditFileError::Invalid(format!(
            "{} permissions are too broad ({:o})",
            path.display(),
            metadata.mode() & 0o777
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditDecision {
    Allowed,
    Denied,
    OutOfScope,
    Unavailable,
    PolicyBlocked,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditActivation {
    None,
    Physical,
    TrustedControl,
    SyntheticRejected,
}

impl From<Option<ActivationOrigin>> for AuditActivation {
    fn from(origin: Option<ActivationOrigin>) -> Self {
        match origin {
            None => Self::None,
            Some(ActivationOrigin::Physical) => Self::Physical,
            Some(ActivationOrigin::TrustedControl) => Self::TrustedControl,
            Some(ActivationOrigin::Synthetic) => Self::SyntheticRejected,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditResult {
    Success,
    Unavailable,
    Denied,
    OutOfScope,
    InvalidRequest,
    InvalidPhase,
    ActivationRequired,
    QuotaExceeded,
    RateLimited,
    Timeout,
    Cancelled,
    Unsupported,
    BackendFailed,
    Internal,
}

impl From<Option<BrokerErrorCode>> for AuditResult {
    fn from(error: Option<BrokerErrorCode>) -> Self {
        match error {
            None => Self::Success,
            Some(BrokerErrorCode::Unavailable) => Self::Unavailable,
            Some(BrokerErrorCode::Denied) => Self::Denied,
            Some(BrokerErrorCode::OutOfScope) => Self::OutOfScope,
            Some(BrokerErrorCode::InvalidRequest) => Self::InvalidRequest,
            Some(BrokerErrorCode::InvalidPhase) => Self::InvalidPhase,
            Some(BrokerErrorCode::ActivationRequired) => Self::ActivationRequired,
            Some(BrokerErrorCode::QuotaExceeded) => Self::QuotaExceeded,
            Some(BrokerErrorCode::RateLimited) => Self::RateLimited,
            Some(BrokerErrorCode::Timeout) => Self::Timeout,
            Some(BrokerErrorCode::Cancelled) => Self::Cancelled,
            Some(BrokerErrorCode::Unsupported) => Self::Unsupported,
            Some(BrokerErrorCode::BackendFailed) => Self::BackendFailed,
            Some(BrokerErrorCode::Internal) => Self::Internal,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    pub sequence: u64,
    pub wall_clock_unix_millis: u64,
    pub source: GithubSource,
    pub version: String,
    pub artifact_digest: String,
    pub instance_id: u64,
    pub capability: String,
    pub operation: String,
    pub activation: AuditActivation,
    pub decision: AuditDecision,
    pub result: AuditResult,
    pub duration_micros: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub resource_count: u32,
}

/// Inputs intentionally contain no payload, path, URL, D-Bus arguments, or
/// backend diagnostic text, making sensitive-data redaction structural.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditInput {
    pub wall_clock_unix_millis: u64,
    pub capability: String,
    pub operation: String,
    pub activation: Option<ActivationOrigin>,
    pub decision: AuditDecision,
    pub error: Option<BrokerErrorCode>,
    pub duration_micros: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub resource_count: u32,
}

pub struct AuditLog {
    source: GithubSource,
    version: String,
    artifact_digest: String,
    instance_id: u64,
    maximum_records: usize,
    next_sequence: u64,
    records: VecDeque<AuditRecord>,
}

impl AuditLog {
    pub fn new(
        source: GithubSource,
        version: impl Into<String>,
        artifact_digest: impl Into<String>,
        instance_id: u64,
        maximum_records: usize,
    ) -> Self {
        Self {
            source,
            version: version.into(),
            artifact_digest: artifact_digest.into(),
            instance_id,
            maximum_records,
            next_sequence: 1,
            records: VecDeque::new(),
        }
    }

    pub fn push(&mut self, input: AuditInput) -> &AuditRecord {
        if self.maximum_records == 0 {
            self.maximum_records = 1;
        }
        if self.records.len() == self.maximum_records {
            self.records.pop_front();
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.records.push_back(AuditRecord {
            sequence,
            wall_clock_unix_millis: input.wall_clock_unix_millis,
            source: self.source.clone(),
            version: self.version.clone(),
            artifact_digest: self.artifact_digest.clone(),
            instance_id: self.instance_id,
            capability: input.capability,
            operation: input.operation,
            activation: input.activation.into(),
            decision: input.decision,
            result: input.error.into(),
            duration_micros: input.duration_micros,
            request_bytes: input.request_bytes,
            response_bytes: input.response_bytes,
            resource_count: input.resource_count,
        });
        self.records.back().expect("record was just inserted")
    }

    pub fn records(&self) -> impl Iterator<Item = &AuditRecord> {
        self.records.iter()
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.records)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::Arc,
        thread,
        time::Duration,
    };

    use super::*;

    fn log(maximum: usize) -> AuditLog {
        AuditLog::new(
            GithubSource::new("alice", "media").unwrap(),
            "1.0.0",
            format!("sha256:{}", "a".repeat(64)),
            7,
            maximum,
        )
    }

    fn input(operation: &str) -> AuditInput {
        AuditInput {
            wall_clock_unix_millis: 10,
            capability: "secret.read.v1".into(),
            operation: operation.into(),
            activation: Some(ActivationOrigin::Physical),
            decision: AuditDecision::Denied,
            error: Some(BrokerErrorCode::Denied),
            duration_micros: 3,
            request_bytes: 400,
            response_bytes: 0,
            resource_count: 0,
        }
    }

    #[test]
    fn audit_is_bounded_and_contains_no_payload_field() {
        let mut log = log(2);
        log.push(input("first"));
        log.push(input("second"));
        log.push(input("third"));
        let records = log.records().collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].sequence, 2);
        let encoded = log.to_json().unwrap();
        assert!(!encoded.contains("payload"));
        assert!(!encoded.contains("super-secret-value"));
    }

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn record() -> AuditRecord {
        let mut log = log(1);
        log.push(input("read")).clone()
    }

    #[test]
    fn durable_log_is_private_json_lines_and_structurally_redacted() {
        let directory = private_directory();
        let path = directory.path().join("audit.jsonl");
        let sink = AuditFile::new(&path, AuditFileLimits::default()).unwrap();
        sink.append(&record()).unwrap();

        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        let encoded = fs::read_to_string(path).unwrap();
        assert_eq!(encoded.lines().count(), 1);
        let decoded: AuditRecord = serde_json::from_str(encoded.trim()).unwrap();
        assert_eq!(decoded.operation, "read");
        assert!(!encoded.contains("payload"));
        assert!(!encoded.contains("super-secret-value"));
    }

    #[test]
    fn size_and_age_rotation_are_bounded() {
        let directory = private_directory();
        let path = directory.path().join("audit.jsonl");
        let record = record();
        let line_bytes = serde_json::to_vec(&record).unwrap().len() as u64 + 1;
        let sink = AuditFile::new(
            &path,
            AuditFileLimits {
                maximum_bytes: line_bytes,
                maximum_age: Duration::from_secs(60),
                maximum_archives: 2,
            },
        )
        .unwrap();
        sink.append(&record).unwrap();
        sink.append(&record).unwrap();
        sink.append(&record).unwrap();
        assert!(rotated_path(&path, 1).is_file());
        assert!(rotated_path(&path, 2).is_file());
        assert!(!rotated_path(&path, 3).exists());
        let oldest: AuditRecord =
            serde_json::from_str(fs::read_to_string(rotated_path(&path, 2)).unwrap().trim())
                .unwrap();
        let middle: AuditRecord =
            serde_json::from_str(fs::read_to_string(rotated_path(&path, 1)).unwrap().trim())
                .unwrap();
        let newest: AuditRecord =
            serde_json::from_str(fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(
            [oldest.sequence, middle.sequence, newest.sequence],
            [1, 2, 3]
        );

        let age_path = directory.path().join("age.jsonl");
        let age_sink = AuditFile::new(
            &age_path,
            AuditFileLimits {
                maximum_bytes: 1024 * 1024,
                maximum_age: Duration::from_millis(1),
                maximum_archives: 1,
            },
        )
        .unwrap();
        age_sink.append(&record).unwrap();
        thread::sleep(Duration::from_millis(5));
        age_sink.append(&record).unwrap();
        assert!(rotated_path(&age_path, 1).is_file());
    }

    #[test]
    fn concurrent_appenders_produce_complete_records() {
        let directory = private_directory();
        let path = directory.path().join("audit.jsonl");
        let sink = Arc::new(
            AuditFile::new(
                &path,
                AuditFileLimits {
                    maximum_bytes: 1024 * 1024,
                    maximum_age: Duration::from_secs(60),
                    maximum_archives: 1,
                },
            )
            .unwrap(),
        );
        let mut threads = Vec::new();
        for _ in 0..4 {
            let sink = Arc::clone(&sink);
            threads.push(thread::spawn(move || {
                for _ in 0..10 {
                    sink.append(&record()).unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let encoded = fs::read_to_string(path).unwrap();
        let records = encoded
            .lines()
            .map(serde_json::from_str::<AuditRecord>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(records.len(), 40);
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            (1..=40).collect::<Vec<_>>()
        );
    }

    #[test]
    fn broad_directories_files_and_symlinks_are_rejected() {
        let broad = tempfile::tempdir().unwrap();
        fs::set_permissions(broad.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            AuditFile::new(broad.path().join("audit.jsonl"), AuditFileLimits::default()).is_err()
        );

        let directory = private_directory();
        let broad_file = directory.path().join("broad.jsonl");
        fs::write(&broad_file, "").unwrap();
        fs::set_permissions(&broad_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(AuditFile::new(&broad_file, AuditFileLimits::default()).is_err());

        let target = directory.path().join("target");
        let link = directory.path().join("link.jsonl");
        fs::write(&target, "").unwrap();
        symlink(&target, &link).unwrap();
        assert!(AuditFile::new(&link, AuditFileLimits::default()).is_err());
    }
}
