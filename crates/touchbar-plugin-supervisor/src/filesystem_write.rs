use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result as AnyResult, bail};
use sha2::{Digest, Sha256};
use touchbar_broker_schema::{
    FilesystemMutationResult, FilesystemPath, FilesystemRename, FilesystemWriteFile,
    FilesystemWriteStream, FilesystemWriteStreamChunk, FilesystemWriteStreamCommit,
    FilesystemWriteStreamOpened, MAX_FILE_WRITE_STREAM_CHUNK_BYTES, SchemaError,
};
use touchbar_policy::{
    CapabilityId, CapabilityScope, FilesystemMountBinding, FilesystemWriteScope, WriteOperation,
};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

use crate::{
    ActivationLedger, Backend, BackendRequest, CancellationToken, OpenedResource,
    ResourceEventSink, ResourceHandle, ResourceLimits,
};

pub const FILESYSTEM_CREATE_FILE_OPERATION: &str = "create-file";
pub const FILESYSTEM_REPLACE_FILE_OPERATION: &str = "replace-file";
pub const FILESYSTEM_APPEND_FILE_OPERATION: &str = "append-file";
pub const FILESYSTEM_DELETE_FILE_OPERATION: &str = "delete-file";
pub const FILESYSTEM_RENAME_OPERATION: &str = "rename";
pub const FILESYSTEM_CREATE_DIRECTORY_OPERATION: &str = "create-directory";
pub const FILESYSTEM_CREATE_FILE_STREAM_OPERATION: &str = "create-file-stream";
pub const FILESYSTEM_REPLACE_FILE_STREAM_OPERATION: &str = "replace-file-stream";
pub const FILESYSTEM_APPEND_FILE_STREAM_OPERATION: &str = "append-file-stream";
pub const FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION: &str = "write-stream-chunk";
pub const FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION: &str = "write-stream-commit";

const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_FLAGS: u64 =
    RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH;
const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;
const INTERNAL_NAME_PREFIX: &str = ".touchbar-";
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const QUOTA_RECORD_MAGIC: &[u8; 8] = b"OTBWQ002";
const QUOTA_SLOTS: usize = 60;
const QUOTA_RECORD_BYTES: usize = 16 + QUOTA_SLOTS * 8;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

pub(crate) struct PersistentQuota {
    directory: OwnedFd,
    record_name: CString,
    lock_name: CString,
    seconds_per_slot: u64,
}

struct QuotaRecord {
    latest_tick: u64,
    slots: [u64; QUOTA_SLOTS],
}

pub struct FilesystemWriteBackend {
    quota: PersistentQuota,
    mutations: Mutex<()>,
    uploads: Arc<Mutex<BTreeMap<u64, Arc<Mutex<WriteUpload>>>>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StreamOperation {
    Create,
    Replace,
    Append,
}

struct UploadShared {
    closed: AtomicBool,
    committed: AtomicBool,
}

struct WriteUpload {
    shared: Arc<UploadShared>,
    parent: OwnedFd,
    temporary: OwnedFd,
    temporary_name: CString,
    target_name: CString,
    expected_bytes: u64,
    received_bytes: u64,
    base_bytes: u64,
    prefix_copied: bool,
    operation: StreamOperation,
    expected_target: Option<(OwnedFd, libc::stat)>,
    scope: FilesystemWriteScope,
    filesystem_mounts: BTreeMap<String, FilesystemMountBinding>,
    events: ResourceEventSink,
}

struct WriteUploadHandle {
    resource_id: u64,
    shared: Arc<UploadShared>,
    uploads: Arc<Mutex<BTreeMap<u64, Arc<Mutex<WriteUpload>>>>>,
}

impl FilesystemWriteBackend {
    pub fn new(state_directory: &Path, quota_namespace: &str) -> AnyResult<Self> {
        if quota_namespace.is_empty() {
            bail!("filesystem write quota namespace is empty");
        }
        Ok(Self {
            quota: PersistentQuota::new(
                state_directory,
                &format!("filesystem-write:{quota_namespace}"),
                60 * 60,
            )?,
            mutations: Mutex::new(()),
            uploads: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
}

impl PersistentQuota {
    pub(crate) fn new(
        state_directory: &Path,
        namespace: &str,
        window_seconds: u64,
    ) -> AnyResult<Self> {
        if namespace.is_empty() {
            bail!("durable quota namespace is empty");
        }
        if window_seconds < QUOTA_SLOTS as u64 || !window_seconds.is_multiple_of(QUOTA_SLOTS as u64)
        {
            bail!("durable quota window must divide into 60 whole-second slots");
        }
        let directory = open_private_state_directory(state_directory)?;
        let namespace = format!("{:x}", Sha256::digest(namespace.as_bytes()));
        Ok(Self {
            directory,
            record_name: CString::new(format!("rolling-quota-{namespace}.state"))
                .context("build durable quota record name")?,
            lock_name: CString::new(format!(".rolling-quota-{namespace}.lock"))
                .context("build durable quota lock name")?,
            seconds_per_slot: window_seconds / QUOTA_SLOTS as u64,
        })
    }

    pub(crate) fn reserve(&self, bytes: u64, maximum: u64) -> Result<(), BrokerErrorCode> {
        let lock = open_state_file(
            self.directory.as_raw_fd(),
            &self.lock_name,
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )?;
        validate_private_state_file(lock.as_raw_fd())?;
        // SAFETY: flock operates on this live descriptor and retains no pointer.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(BrokerErrorCode::Internal);
        }

        let current_tick = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BrokerErrorCode::Internal)?
            .as_secs()
            / self.seconds_per_slot;
        let mut record = self.read_record()?.unwrap_or(QuotaRecord {
            latest_tick: current_tick,
            slots: [0; QUOTA_SLOTS],
        });
        let effective_tick = if current_tick >= record.latest_tick {
            let elapsed = current_tick - record.latest_tick;
            if elapsed >= QUOTA_SLOTS as u64 {
                record.slots.fill(0);
            } else {
                for tick in (record.latest_tick + 1)..=current_tick {
                    record.slots[tick as usize % QUOTA_SLOTS] = 0;
                }
            }
            record.latest_tick = current_tick;
            current_tick
        } else {
            // A wall-clock rollback must not mint a fresh budget. Charge the
            // newest known slot until real time catches up.
            record.latest_tick
        };
        let used = record.slots.iter().try_fold(0_u64, |total, value| {
            total
                .checked_add(*value)
                .ok_or(BrokerErrorCode::QuotaExceeded)
        })?;
        let next = used
            .checked_add(bytes)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
        if next > maximum {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        let slot = effective_tick as usize % QUOTA_SLOTS;
        record.slots[slot] = record.slots[slot]
            .checked_add(bytes)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
        self.write_record(&record)
    }

    fn read_record(&self) -> Result<Option<QuotaRecord>, BrokerErrorCode> {
        let Some(descriptor) =
            open_existing_state_file(self.directory.as_raw_fd(), &self.record_name)?
        else {
            return Ok(None);
        };
        validate_private_state_file(descriptor.as_raw_fd())?;
        let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
        if metadata.st_size != QUOTA_RECORD_BYTES as libc::off_t {
            return Err(BrokerErrorCode::Internal);
        }
        let mut bytes = [0_u8; QUOTA_RECORD_BYTES];
        pread_all(descriptor.as_raw_fd(), &mut bytes)?;
        if &bytes[..8] != QUOTA_RECORD_MAGIC {
            return Err(BrokerErrorCode::Internal);
        }
        let latest_tick = u64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| BrokerErrorCode::Internal)?,
        );
        let mut slots = [0_u64; QUOTA_SLOTS];
        for (index, slot) in slots.iter_mut().enumerate() {
            let start = 16 + index * 8;
            *slot = u64::from_le_bytes(
                bytes[start..start + 8]
                    .try_into()
                    .map_err(|_| BrokerErrorCode::Internal)?,
            );
        }
        Ok(Some(QuotaRecord { latest_tick, slots }))
    }

    fn write_record(&self, record: &QuotaRecord) -> Result<(), BrokerErrorCode> {
        let mut bytes = [0_u8; QUOTA_RECORD_BYTES];
        bytes[..8].copy_from_slice(QUOTA_RECORD_MAGIC);
        bytes[8..16].copy_from_slice(&record.latest_tick.to_le_bytes());
        for (index, slot) in record.slots.iter().enumerate() {
            let start = 16 + index * 8;
            bytes[start..start + 8].copy_from_slice(&slot.to_le_bytes());
        }
        let temporary_name = temporary_name()?;
        let temporary = create_exclusive(self.directory.as_raw_fd(), &temporary_name, 0o600)
            .map_err(|_| BrokerErrorCode::Internal)?;
        if write_without_cancellation(temporary.as_raw_fd(), &bytes).is_err()
            || unsafe { libc::fdatasync(temporary.as_raw_fd()) } != 0
        {
            unlink(self.directory.as_raw_fd(), &temporary_name);
            return Err(BrokerErrorCode::Internal);
        }
        if rename_at(
            self.directory.as_raw_fd(),
            &temporary_name,
            self.directory.as_raw_fd(),
            &self.record_name,
            0,
        )
        .is_err()
        {
            unlink(self.directory.as_raw_fd(), &temporary_name);
            return Err(BrokerErrorCode::Internal);
        }
        sync_directory(self.directory.as_raw_fd()).map_err(|_| BrokerErrorCode::Internal)
    }
}

enum AuthorizedMutation {
    Create(FilesystemWriteFile),
    Replace(FilesystemWriteFile),
    Append(FilesystemWriteFile),
    Delete(FilesystemPath),
    Rename(FilesystemRename),
    CreateDirectory(FilesystemPath),
}

impl AuthorizedMutation {
    fn written_bytes(&self) -> u64 {
        match self {
            Self::Create(write) | Self::Replace(write) | Self::Append(write) => {
                write.bytes.len() as u64
            }
            Self::Delete(_) | Self::Rename(_) | Self::CreateDirectory(_) => 0,
        }
    }
}

impl Backend for FilesystemWriteBackend {
    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        if matches!(
            request.operation.as_str(),
            FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION | FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION
        ) {
            return self.authorize_upload_command(request);
        }
        let written_bytes = authorize_filesystem_write_mutation_request(request)?;
        let scope = filesystem_scope(request)?;
        self.reserve_quota(written_bytes, scope.maximum_total_bytes_per_hour)
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            // One supervisor serves one plugin instance. Serializing its mutations
            // prevents that plugin from racing broker-owned commit names or the
            // validate/commit sequence through separate executor workers.
            let _mutation = self
                .mutations
                .lock()
                .map_err(|_| BrokerErrorCode::Internal)?;
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            match request.operation.as_str() {
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION => {
                    self.write_upload_chunk(request, cancellation)?;
                    return Ok(BrokerResult::Success {
                        payload: Vec::new(),
                    });
                }
                FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION => {
                    let receipt = self.commit_upload(request, cancellation)?;
                    let payload = receipt.encode().map_err(schema_error)?;
                    return Ok(BrokerResult::Success { payload });
                }
                _ => {}
            }
            let mutation = decode_and_authorize(request)?;
            let receipt = match mutation {
                AuthorizedMutation::Create(write) => create_file(request, &write, cancellation),
                AuthorizedMutation::Replace(write) => replace_file(request, &write, cancellation),
                AuthorizedMutation::Append(write) => append_file(request, &write, cancellation),
                AuthorizedMutation::Delete(path) => delete_file(request, &path, cancellation),
                AuthorizedMutation::Rename(rename) => rename_file(request, &rename, cancellation),
                AuthorizedMutation::CreateDirectory(path) => {
                    create_directory(request, &path, cancellation)
                }
            }?;
            let payload = receipt.encode().map_err(schema_error)?;
            Ok(BrokerResult::Success { payload })
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

impl crate::ResourceBackend for FilesystemWriteBackend {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        (request.capability == CapabilityId::FilesystemWriteV1
            && matches!(
                request.operation.as_str(),
                FILESYSTEM_CREATE_FILE_STREAM_OPERATION
                    | FILESYSTEM_REPLACE_FILE_STREAM_OPERATION
                    | FILESYSTEM_APPEND_FILE_STREAM_OPERATION
            ))
        .then_some(ResourceLimits {
            reserved_buffered_bytes: MAX_FILE_WRITE_STREAM_CHUNK_BYTES,
            maximum_events_per_second: 2,
        })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        let stream = authorize_filesystem_write_stream_begin_request(request)?;
        let scope = filesystem_scope(request)?;
        self.reserve_quota(stream.expected_bytes, scope.maximum_total_bytes_per_hour)
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        let stream = authorize_filesystem_write_stream_begin_request(request)?;
        let scope = filesystem_scope(request)?.clone();
        let operation = match request.operation.as_str() {
            FILESYSTEM_CREATE_FILE_STREAM_OPERATION => StreamOperation::Create,
            FILESYSTEM_REPLACE_FILE_STREAM_OPERATION => StreamOperation::Replace,
            FILESYSTEM_APPEND_FILE_STREAM_OPERATION => StreamOperation::Append,
            _ => return Err(BrokerErrorCode::InvalidRequest),
        };
        let root = open_mount(request, &stream.mount)?;
        let (parent, target_name) = open_parent(root.as_raw_fd(), &stream.path)?;
        let (expected_target, base_bytes) = match operation {
            StreamOperation::Create => (None, 0),
            StreamOperation::Replace => {
                let descriptor = open_at_beneath(parent.as_raw_fd(), &target_name, libc::O_PATH)?;
                let metadata =
                    validate_regular_file(descriptor.as_raw_fd(), scope.maximum_file_bytes)?;
                (Some((descriptor, metadata)), 0)
            }
            StreamOperation::Append => {
                let descriptor = open_at_beneath(
                    parent.as_raw_fd(),
                    &target_name,
                    libc::O_RDONLY | libc::O_NONBLOCK,
                )?;
                let metadata =
                    validate_regular_file(descriptor.as_raw_fd(), scope.maximum_file_bytes)?;
                let base =
                    u64::try_from(metadata.st_size).map_err(|_| BrokerErrorCode::BackendFailed)?;
                if base
                    .checked_add(stream.expected_bytes)
                    .ok_or(BrokerErrorCode::QuotaExceeded)?
                    > scope.maximum_file_bytes
                {
                    return Err(BrokerErrorCode::QuotaExceeded);
                }
                (Some((descriptor, metadata)), base)
            }
        };
        let temporary_name = temporary_name()?;
        let temporary = create_exclusive(parent.as_raw_fd(), &temporary_name, 0o600)?;
        let shared = Arc::new(UploadShared {
            closed: AtomicBool::new(false),
            committed: AtomicBool::new(false),
        });
        let upload = Arc::new(Mutex::new(WriteUpload {
            shared: shared.clone(),
            parent,
            temporary,
            temporary_name,
            target_name,
            expected_bytes: stream.expected_bytes,
            received_bytes: 0,
            base_bytes,
            prefix_copied: operation != StreamOperation::Append,
            operation,
            expected_target,
            scope,
            filesystem_mounts: request.bindings.filesystem_mounts.clone(),
            events,
        }));
        let mut uploads = self.uploads.lock().map_err(|_| BrokerErrorCode::Internal)?;
        if uploads.contains_key(&resource_id) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        uploads.insert(resource_id, upload);
        drop(uploads);
        let response_payload = FilesystemWriteStreamOpened {
            resource_id,
            maximum_chunk_bytes: MAX_FILE_WRITE_STREAM_CHUNK_BYTES as u32,
        }
        .encode()
        .map_err(schema_error)?;
        Ok(OpenedResource {
            handle: Box::new(WriteUploadHandle {
                resource_id,
                shared,
                uploads: self.uploads.clone(),
            }),
            response_payload,
        })
    }
}

impl ResourceHandle for WriteUploadHandle {
    fn close(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        if let Ok(mut uploads) = self.uploads.lock() {
            uploads.remove(&self.resource_id);
        }
    }
}

impl Drop for WriteUpload {
    fn drop(&mut self) {
        if !self.shared.committed.load(Ordering::Acquire) {
            unlink(self.parent.as_raw_fd(), &self.temporary_name);
        }
    }
}

impl FilesystemWriteBackend {
    fn reserve_quota(&self, bytes: u64, maximum: u64) -> Result<(), BrokerErrorCode> {
        self.quota.reserve(bytes, maximum)
    }

    fn authorize_upload_command(&self, request: &BackendRequest) -> Result<(), BrokerErrorCode> {
        if request.capability != CapabilityId::FilesystemWriteV1 {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let resource_id = match request.operation.as_str() {
            FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION => {
                let chunk =
                    FilesystemWriteStreamChunk::decode(&request.payload).map_err(schema_error)?;
                chunk.resource_id
            }
            FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION => {
                let commit =
                    FilesystemWriteStreamCommit::decode(&request.payload).map_err(schema_error)?;
                commit.resource_id
            }
            _ => return Err(BrokerErrorCode::InvalidRequest),
        };
        let upload = self.upload(resource_id)?;
        let upload = upload.lock().map_err(|_| BrokerErrorCode::Internal)?;
        require_open_upload(&upload)?;
        authorize_filesystem_write_stream_command(
            request,
            resource_id,
            upload.expected_bytes,
            upload.received_bytes,
            &upload.scope,
            &upload.filesystem_mounts,
        )
        .map(|_| ())
    }

    fn write_upload_chunk(
        &self,
        request: &BackendRequest,
        cancellation: &CancellationToken,
    ) -> Result<(), BrokerErrorCode> {
        let resource_id = FilesystemWriteStreamChunk::decode(&request.payload)
            .map_err(schema_error)?
            .resource_id;
        let upload = self.upload(resource_id)?;
        let mut upload = upload.lock().map_err(|_| BrokerErrorCode::Internal)?;
        require_open_upload(&upload)?;
        let FilesystemWriteStreamCommandAuthorization::Chunk { chunk, end_offset } =
            authorize_filesystem_write_stream_command(
                request,
                resource_id,
                upload.expected_bytes,
                upload.received_bytes,
                &upload.scope,
                &upload.filesystem_mounts,
            )?
        else {
            return Err(BrokerErrorCode::InvalidRequest);
        };
        prepare_stream_append(&mut upload, cancellation)?;
        let destination_offset = upload
            .base_bytes
            .checked_add(chunk.offset)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
        pwrite_all_at(
            upload.temporary.as_raw_fd(),
            &chunk.bytes,
            destination_offset,
            cancellation,
        )?;
        upload.received_bytes = end_offset;
        Ok(())
    }

    fn commit_upload(
        &self,
        request: &BackendRequest,
        cancellation: &CancellationToken,
    ) -> Result<FilesystemMutationResult, BrokerErrorCode> {
        let resource_id = FilesystemWriteStreamCommit::decode(&request.payload)
            .map_err(schema_error)?
            .resource_id;
        let upload = self.upload(resource_id)?;
        let mut upload = upload.lock().map_err(|_| BrokerErrorCode::Internal)?;
        require_open_upload(&upload)?;
        if !matches!(
            authorize_filesystem_write_stream_command(
                request,
                resource_id,
                upload.expected_bytes,
                upload.received_bytes,
                &upload.scope,
                &upload.filesystem_mounts,
            )?,
            FilesystemWriteStreamCommandAuthorization::Commit
        ) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        prepare_stream_append(&mut upload, cancellation)?;
        // SAFETY: temporary is a live regular file created by this broker.
        if unsafe { libc::fdatasync(upload.temporary.as_raw_fd()) } != 0 {
            return Err(os_error());
        }
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        if upload.shared.closed.load(Ordering::Acquire) {
            return Err(BrokerErrorCode::Cancelled);
        }
        let metadata = validate_regular_file(
            upload.temporary.as_raw_fd(),
            upload.scope.maximum_file_bytes,
        )?;
        let resulting_size = upload
            .base_bytes
            .checked_add(upload.expected_bytes)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
        if u64::try_from(metadata.st_size).ok() != Some(resulting_size) {
            return Err(BrokerErrorCode::BackendFailed);
        }

        match upload.operation {
            StreamOperation::Create => {
                rename_at(
                    upload.parent.as_raw_fd(),
                    &upload.temporary_name,
                    upload.parent.as_raw_fd(),
                    &upload.target_name,
                    RENAME_NOREPLACE,
                )?;
                let committed = metadata_at(upload.parent.as_raw_fd(), &upload.target_name)?;
                if !same_file(&metadata, &committed) {
                    let _ = rename_at(
                        upload.parent.as_raw_fd(),
                        &upload.target_name,
                        upload.parent.as_raw_fd(),
                        &upload.temporary_name,
                        RENAME_NOREPLACE,
                    );
                    return Err(BrokerErrorCode::BackendFailed);
                }
            }
            StreamOperation::Replace => {
                let (_, expected) = upload
                    .expected_target
                    .as_ref()
                    .ok_or(BrokerErrorCode::Internal)?;
                exchange_and_remove(
                    upload.parent.as_raw_fd(),
                    &upload.temporary_name,
                    &upload.target_name,
                    expected,
                    false,
                )?;
            }
            StreamOperation::Append => {
                let (_, expected) = upload
                    .expected_target
                    .as_ref()
                    .ok_or(BrokerErrorCode::Internal)?;
                exchange_and_remove(
                    upload.parent.as_raw_fd(),
                    &upload.temporary_name,
                    &upload.target_name,
                    expected,
                    true,
                )?;
            }
        }
        upload.shared.committed.store(true, Ordering::Release);
        sync_directory(upload.parent.as_raw_fd())?;
        let receipt = FilesystemMutationResult {
            bytes_written: upload.expected_bytes,
            resulting_size: Some(resulting_size),
        };
        if let Ok(payload) = receipt.encode() {
            let _ = upload.events.complete(payload);
        }
        Ok(receipt)
    }

    fn upload(&self, resource_id: u64) -> Result<Arc<Mutex<WriteUpload>>, BrokerErrorCode> {
        self.uploads
            .lock()
            .map_err(|_| BrokerErrorCode::Internal)?
            .get(&resource_id)
            .cloned()
            .ok_or(BrokerErrorCode::Unavailable)
    }
}

fn require_open_upload(upload: &WriteUpload) -> Result<(), BrokerErrorCode> {
    if upload.shared.closed.load(Ordering::Acquire)
        || upload.shared.committed.load(Ordering::Acquire)
    {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(())
}

fn prepare_stream_append(
    upload: &mut WriteUpload,
    cancellation: &CancellationToken,
) -> Result<(), BrokerErrorCode> {
    if upload.prefix_copied {
        return Ok(());
    }
    if upload.operation != StreamOperation::Append {
        return Err(BrokerErrorCode::Internal);
    }
    let (descriptor, expected) = upload
        .expected_target
        .as_ref()
        .ok_or(BrokerErrorCode::Internal)?;
    copy_exact(
        descriptor.as_raw_fd(),
        upload.temporary.as_raw_fd(),
        upload.base_bytes,
        cancellation,
    )?;
    let after_copy = descriptor_metadata(descriptor.as_raw_fd())?;
    if !same_precommit_snapshot(expected, &after_copy) {
        return Err(BrokerErrorCode::BackendFailed);
    }
    upload.prefix_copied = true;
    Ok(())
}

pub fn authorize_filesystem_write_stream_begin_request(
    request: &BackendRequest,
) -> Result<FilesystemWriteStream, BrokerErrorCode> {
    if request.capability != CapabilityId::FilesystemWriteV1 {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let scope = filesystem_scope(request)?;
    let required_operation = match request.operation.as_str() {
        FILESYSTEM_CREATE_FILE_STREAM_OPERATION => WriteOperation::Create,
        FILESYSTEM_REPLACE_FILE_STREAM_OPERATION => WriteOperation::Replace,
        FILESYSTEM_APPEND_FILE_STREAM_OPERATION => WriteOperation::Append,
        _ => return Err(BrokerErrorCode::InvalidRequest),
    };
    require_operation(scope, required_operation)?;
    let stream = FilesystemWriteStream::decode(&request.payload).map_err(schema_error)?;
    validate_mount_and_path(request, scope, &stream.mount, &stream.path)?;
    if stream.expected_bytes > scope.maximum_file_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(stream)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilesystemWriteStreamCommandAuthorization {
    Chunk {
        chunk: FilesystemWriteStreamChunk,
        end_offset: u64,
    },
    Commit,
}

pub fn authorize_filesystem_write_stream_command(
    request: &BackendRequest,
    resource_id: u64,
    expected_bytes: u64,
    received_bytes: u64,
    expected_scope: &FilesystemWriteScope,
    expected_mounts: &BTreeMap<String, FilesystemMountBinding>,
) -> Result<FilesystemWriteStreamCommandAuthorization, BrokerErrorCode> {
    if request.capability != CapabilityId::FilesystemWriteV1 {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let scope = filesystem_scope(request)?;
    if scope != expected_scope || &request.bindings.filesystem_mounts != expected_mounts {
        return Err(BrokerErrorCode::OutOfScope);
    }
    match request.operation.as_str() {
        FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION => {
            let chunk =
                FilesystemWriteStreamChunk::decode(&request.payload).map_err(schema_error)?;
            if chunk.resource_id != resource_id || chunk.offset != received_bytes {
                return Err(BrokerErrorCode::InvalidRequest);
            }
            let end_offset = chunk
                .offset
                .checked_add(chunk.bytes.len() as u64)
                .ok_or(BrokerErrorCode::QuotaExceeded)?;
            if end_offset > expected_bytes {
                return Err(BrokerErrorCode::QuotaExceeded);
            }
            Ok(FilesystemWriteStreamCommandAuthorization::Chunk { chunk, end_offset })
        }
        FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION => {
            let commit =
                FilesystemWriteStreamCommit::decode(&request.payload).map_err(schema_error)?;
            if commit.resource_id != resource_id || received_bytes != expected_bytes {
                return Err(BrokerErrorCode::InvalidRequest);
            }
            Ok(FilesystemWriteStreamCommandAuthorization::Commit)
        }
        _ => Err(BrokerErrorCode::InvalidRequest),
    }
}

pub fn authorize_filesystem_write_mutation_request(
    request: &BackendRequest,
) -> Result<u64, BrokerErrorCode> {
    decode_and_authorize(request).map(|mutation| mutation.written_bytes())
}

fn decode_and_authorize(request: &BackendRequest) -> Result<AuthorizedMutation, BrokerErrorCode> {
    if request.capability != CapabilityId::FilesystemWriteV1 {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let scope = filesystem_scope(request)?;
    let mutation = match request.operation.as_str() {
        FILESYSTEM_CREATE_FILE_OPERATION => {
            require_operation(scope, WriteOperation::Create)?;
            AuthorizedMutation::Create(decode_write(request)?)
        }
        FILESYSTEM_REPLACE_FILE_OPERATION => {
            require_operation(scope, WriteOperation::Replace)?;
            AuthorizedMutation::Replace(decode_write(request)?)
        }
        FILESYSTEM_APPEND_FILE_OPERATION => {
            require_operation(scope, WriteOperation::Append)?;
            AuthorizedMutation::Append(decode_write(request)?)
        }
        FILESYSTEM_DELETE_FILE_OPERATION => {
            require_operation(scope, WriteOperation::Delete)?;
            AuthorizedMutation::Delete(
                FilesystemPath::decode(&request.payload).map_err(schema_error)?,
            )
        }
        FILESYSTEM_RENAME_OPERATION => {
            require_operation(scope, WriteOperation::Rename)?;
            AuthorizedMutation::Rename(
                FilesystemRename::decode(&request.payload).map_err(schema_error)?,
            )
        }
        FILESYSTEM_CREATE_DIRECTORY_OPERATION => {
            require_operation(scope, WriteOperation::CreateDirectory)?;
            AuthorizedMutation::CreateDirectory(
                FilesystemPath::decode(&request.payload).map_err(schema_error)?,
            )
        }
        _ => return Err(BrokerErrorCode::InvalidRequest),
    };
    match &mutation {
        AuthorizedMutation::Create(write)
        | AuthorizedMutation::Replace(write)
        | AuthorizedMutation::Append(write) => {
            validate_mount_and_path(request, scope, &write.mount, &write.path)?;
            if write.bytes.len() as u64 > scope.maximum_file_bytes {
                return Err(BrokerErrorCode::QuotaExceeded);
            }
        }
        AuthorizedMutation::Delete(path) | AuthorizedMutation::CreateDirectory(path) => {
            validate_mount_and_path(request, scope, &path.mount, &path.path)?;
        }
        AuthorizedMutation::Rename(rename) => {
            validate_mount_and_path(request, scope, &rename.mount, &rename.source)?;
            validate_mount_and_path(request, scope, &rename.mount, &rename.destination)?;
            if rename.source == rename.destination {
                return Err(BrokerErrorCode::InvalidRequest);
            }
        }
    }
    Ok(mutation)
}

fn decode_write(request: &BackendRequest) -> Result<FilesystemWriteFile, BrokerErrorCode> {
    FilesystemWriteFile::decode(&request.payload).map_err(schema_error)
}

fn require_operation(
    scope: &FilesystemWriteScope,
    operation: WriteOperation,
) -> Result<(), BrokerErrorCode> {
    scope
        .operations
        .contains(&operation)
        .then_some(())
        .ok_or(BrokerErrorCode::OutOfScope)
}

fn validate_mount_and_path(
    request: &BackendRequest,
    scope: &FilesystemWriteScope,
    mount: &str,
    path: &str,
) -> Result<(), BrokerErrorCode> {
    if mount.is_empty()
        || !scope
            .mounts
            .iter()
            .any(|candidate| candidate.label == mount)
        || !request.bindings.filesystem_mounts.contains_key(mount)
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| match component {
            Component::Normal(name) => name
                .to_str()
                .is_none_or(|name| name.starts_with(INTERNAL_NAME_PREFIX)),
            _ => true,
        })
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(())
}

fn create_file(
    request: &BackendRequest,
    write: &FilesystemWriteFile,
    cancellation: &CancellationToken,
) -> Result<FilesystemMutationResult, BrokerErrorCode> {
    let root = open_mount(request, &write.mount)?;
    let (parent, name) = open_parent(root.as_raw_fd(), &write.path)?;
    let temporary_name = temporary_name()?;
    let temporary = create_exclusive(parent.as_raw_fd(), &temporary_name, 0o600)?;
    if let Err(error) = write_and_sync(temporary.as_raw_fd(), &write.bytes, cancellation) {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(error);
    }
    if let Some(reason) = cancellation.reason() {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(reason);
    }
    let temporary_metadata = match descriptor_metadata(temporary.as_raw_fd()) {
        Ok(metadata) => metadata,
        Err(error) => {
            unlink(parent.as_raw_fd(), &temporary_name);
            return Err(error);
        }
    };
    if let Err(error) = rename_at(
        parent.as_raw_fd(),
        &temporary_name,
        parent.as_raw_fd(),
        &name,
        RENAME_NOREPLACE,
    ) {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(error);
    }
    let committed = metadata_at(parent.as_raw_fd(), &name);
    if !committed
        .as_ref()
        .is_ok_and(|metadata| same_file(&temporary_metadata, metadata))
    {
        let rollback = rename_at(
            parent.as_raw_fd(),
            &name,
            parent.as_raw_fd(),
            &temporary_name,
            RENAME_NOREPLACE,
        );
        if rollback.is_ok() {
            unlink(parent.as_raw_fd(), &temporary_name);
        }
        return Err(BrokerErrorCode::BackendFailed);
    }
    sync_directory(parent.as_raw_fd())?;
    Ok(FilesystemMutationResult {
        bytes_written: write.bytes.len() as u64,
        resulting_size: Some(write.bytes.len() as u64),
    })
}

fn append_file(
    request: &BackendRequest,
    write: &FilesystemWriteFile,
    cancellation: &CancellationToken,
) -> Result<FilesystemMutationResult, BrokerErrorCode> {
    let scope = filesystem_scope(request)?;
    let root = open_mount(request, &write.mount)?;
    let (parent, name) = open_parent(root.as_raw_fd(), &write.path)?;
    let existing = open_at_beneath(parent.as_raw_fd(), &name, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let existing_metadata = validate_regular_file(existing.as_raw_fd(), scope.maximum_file_bytes)?;
    let initial =
        u64::try_from(existing_metadata.st_size).map_err(|_| BrokerErrorCode::BackendFailed)?;
    let resulting_size = initial
        .checked_add(write.bytes.len() as u64)
        .ok_or(BrokerErrorCode::QuotaExceeded)?;
    if resulting_size > scope.maximum_file_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }

    // Appending in place can expose a prefix on cancellation and lets a
    // concurrent size change exceed the grant. Build the complete result in a
    // private same-directory file, validate the source snapshot again, and
    // atomically exchange it with the target.
    let temporary_name = temporary_name()?;
    let temporary = create_exclusive(parent.as_raw_fd(), &temporary_name, 0o600)?;
    if let Err(error) = copy_exact(
        existing.as_raw_fd(),
        temporary.as_raw_fd(),
        initial,
        cancellation,
    )
    .and_then(|()| pwrite_all_at(temporary.as_raw_fd(), &write.bytes, initial, cancellation))
    {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(error);
    }
    // SAFETY: temporary is a live regular file.
    if unsafe { libc::fdatasync(temporary.as_raw_fd()) } != 0 {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(os_error());
    }
    let after_copy = match descriptor_metadata(existing.as_raw_fd()) {
        Ok(metadata) => metadata,
        Err(error) => {
            unlink(parent.as_raw_fd(), &temporary_name);
            return Err(error);
        }
    };
    if !same_precommit_snapshot(&existing_metadata, &after_copy) {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(BrokerErrorCode::BackendFailed);
    }
    if let Some(reason) = cancellation.reason() {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(reason);
    }
    exchange_and_remove(
        parent.as_raw_fd(),
        &temporary_name,
        &name,
        &existing_metadata,
        true,
    )?;
    sync_directory(parent.as_raw_fd())?;
    Ok(FilesystemMutationResult {
        bytes_written: write.bytes.len() as u64,
        resulting_size: Some(resulting_size),
    })
}

fn replace_file(
    request: &BackendRequest,
    write: &FilesystemWriteFile,
    cancellation: &CancellationToken,
) -> Result<FilesystemMutationResult, BrokerErrorCode> {
    let scope = filesystem_scope(request)?;
    let root = open_mount(request, &write.mount)?;
    let (parent, name) = open_parent(root.as_raw_fd(), &write.path)?;
    let existing = open_at_beneath(parent.as_raw_fd(), &name, libc::O_PATH)?;
    let existing_metadata = validate_regular_file(existing.as_raw_fd(), scope.maximum_file_bytes)?;
    let temporary_name = temporary_name()?;
    let temporary = create_exclusive(parent.as_raw_fd(), &temporary_name, 0o600)?;
    if let Err(error) = write_and_sync(temporary.as_raw_fd(), &write.bytes, cancellation) {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(error);
    }
    if let Some(reason) = cancellation.reason() {
        unlink(parent.as_raw_fd(), &temporary_name);
        return Err(reason);
    }
    exchange_and_remove(
        parent.as_raw_fd(),
        &temporary_name,
        &name,
        &existing_metadata,
        false,
    )?;
    sync_directory(parent.as_raw_fd())?;
    Ok(FilesystemMutationResult {
        bytes_written: write.bytes.len() as u64,
        resulting_size: Some(write.bytes.len() as u64),
    })
}

fn delete_file(
    request: &BackendRequest,
    path: &FilesystemPath,
    cancellation: &CancellationToken,
) -> Result<FilesystemMutationResult, BrokerErrorCode> {
    let scope = filesystem_scope(request)?;
    let root = open_mount(request, &path.mount)?;
    let (parent, name) = open_parent(root.as_raw_fd(), &path.path)?;
    let existing = open_at_beneath(parent.as_raw_fd(), &name, libc::O_PATH)?;
    let existing_metadata = validate_regular_file(existing.as_raw_fd(), scope.maximum_file_bytes)?;
    if let Some(reason) = cancellation.reason() {
        return Err(reason);
    }
    let quarantine = temporary_name()?;
    rename_at(
        parent.as_raw_fd(),
        &name,
        parent.as_raw_fd(),
        &quarantine,
        RENAME_NOREPLACE,
    )?;
    let moved = match metadata_at(parent.as_raw_fd(), &quarantine) {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = rename_at(
                parent.as_raw_fd(),
                &quarantine,
                parent.as_raw_fd(),
                &name,
                RENAME_NOREPLACE,
            );
            return Err(error);
        }
    };
    if !same_file(&existing_metadata, &moved) {
        let _ = rename_at(
            parent.as_raw_fd(),
            &quarantine,
            parent.as_raw_fd(),
            &name,
            RENAME_NOREPLACE,
        );
        return Err(BrokerErrorCode::BackendFailed);
    }
    unlink_checked(parent.as_raw_fd(), &quarantine)?;
    sync_directory(parent.as_raw_fd())?;
    Ok(FilesystemMutationResult {
        bytes_written: 0,
        resulting_size: None,
    })
}

fn rename_file(
    request: &BackendRequest,
    rename: &FilesystemRename,
    cancellation: &CancellationToken,
) -> Result<FilesystemMutationResult, BrokerErrorCode> {
    let scope = filesystem_scope(request)?;
    let root = open_mount(request, &rename.mount)?;
    let (source_parent, source_name) = open_parent(root.as_raw_fd(), &rename.source)?;
    let (destination_parent, destination_name) =
        open_parent(root.as_raw_fd(), &rename.destination)?;
    let source = open_at_beneath(source_parent.as_raw_fd(), &source_name, libc::O_PATH)?;
    let source_metadata = validate_regular_file(source.as_raw_fd(), scope.maximum_file_bytes)?;
    if let Some(reason) = cancellation.reason() {
        return Err(reason);
    }
    rename_at(
        source_parent.as_raw_fd(),
        &source_name,
        destination_parent.as_raw_fd(),
        &destination_name,
        RENAME_NOREPLACE,
    )?;
    let destination_metadata = match metadata_at(destination_parent.as_raw_fd(), &destination_name)
    {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = rename_at(
                destination_parent.as_raw_fd(),
                &destination_name,
                source_parent.as_raw_fd(),
                &source_name,
                RENAME_NOREPLACE,
            );
            return Err(error);
        }
    };
    if !same_file(&source_metadata, &destination_metadata) {
        let _ = rename_at(
            destination_parent.as_raw_fd(),
            &destination_name,
            source_parent.as_raw_fd(),
            &source_name,
            RENAME_NOREPLACE,
        );
        return Err(BrokerErrorCode::BackendFailed);
    }
    sync_directory(source_parent.as_raw_fd())?;
    if source_parent.as_raw_fd() != destination_parent.as_raw_fd() {
        sync_directory(destination_parent.as_raw_fd())?;
    }
    Ok(FilesystemMutationResult {
        bytes_written: 0,
        resulting_size: None,
    })
}

fn create_directory(
    request: &BackendRequest,
    path: &FilesystemPath,
    cancellation: &CancellationToken,
) -> Result<FilesystemMutationResult, BrokerErrorCode> {
    let root = open_mount(request, &path.mount)?;
    let (parent, name) = open_parent(root.as_raw_fd(), &path.path)?;
    if let Some(reason) = cancellation.reason() {
        return Err(reason);
    }
    // SAFETY: name is nul-terminated and parent is a pinned directory.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(os_error());
    }
    sync_directory(parent.as_raw_fd())?;
    Ok(FilesystemMutationResult {
        bytes_written: 0,
        resulting_size: None,
    })
}

fn filesystem_scope(request: &BackendRequest) -> Result<&FilesystemWriteScope, BrokerErrorCode> {
    match &request.authorized_scope {
        CapabilityScope::FilesystemWrite(scope) => Ok(scope),
        _ => Err(BrokerErrorCode::InvalidRequest),
    }
}

fn open_private_state_directory(path: &Path) -> AnyResult<OwnedFd> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        bail!("filesystem write state directory must be a normalized absolute path");
    }
    let path = CString::new(path.as_os_str().as_bytes()).context("state path contains NUL")?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: syscall arguments point to initialized storage of the declared size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor = i32::try_from(descriptor).context("state directory descriptor overflow")?;
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open filesystem write state directory");
    }
    // SAFETY: successful open returned one uniquely owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata is writable and descriptor is live.
    if unsafe { libc::fstat(descriptor.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("inspect filesystem write state directory");
    }
    // SAFETY: fstat succeeded.
    let metadata = unsafe { metadata.assume_init() };
    // SAFETY: geteuid has no preconditions.
    let effective_user = unsafe { libc::geteuid() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR
        || metadata.st_uid != effective_user
        || metadata.st_mode & 0o077 != 0
    {
        bail!("filesystem write state directory must be user-owned and private");
    }
    Ok(descriptor)
}

fn open_state_file(
    directory: RawFd,
    name: &CString,
    flags: i32,
    mode: libc::mode_t,
) -> Result<OwnedFd, BrokerErrorCode> {
    // SAFETY: name is nul-terminated and O_CREAT callers supply a mode.
    let descriptor = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    owned_descriptor(descriptor)
}

fn open_existing_state_file(
    directory: RawFd,
    name: &CString,
) -> Result<Option<OwnedFd>, BrokerErrorCode> {
    // SAFETY: name is nul-terminated and flags require no mode.
    let descriptor = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(BrokerErrorCode::Internal)
        };
    }
    // SAFETY: successful open returned one uniquely owned descriptor.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(descriptor) }))
}

fn validate_private_state_file(descriptor: RawFd) -> Result<(), BrokerErrorCode> {
    let metadata = descriptor_metadata(descriptor)?;
    // SAFETY: geteuid has no preconditions.
    let effective_user = unsafe { libc::geteuid() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != effective_user
        || metadata.st_nlink != 1
        || metadata.st_mode & 0o077 != 0
    {
        return Err(BrokerErrorCode::Internal);
    }
    Ok(())
}

fn pread_all(descriptor: RawFd, bytes: &mut [u8]) -> Result<(), BrokerErrorCode> {
    let mut offset = 0;
    while offset < bytes.len() {
        // SAFETY: the remaining slice is writable and descriptor is live.
        let count = unsafe {
            libc::pread(
                descriptor,
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
                offset as libc::off_t,
            )
        };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(BrokerErrorCode::Internal);
        }
        if count == 0 {
            return Err(BrokerErrorCode::Internal);
        }
        offset += usize::try_from(count).map_err(|_| BrokerErrorCode::Internal)?;
    }
    Ok(())
}

fn write_without_cancellation(descriptor: RawFd, mut bytes: &[u8]) -> Result<(), BrokerErrorCode> {
    while !bytes.is_empty() {
        // SAFETY: descriptor is live and bytes is readable for its length.
        let count = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(BrokerErrorCode::Internal);
        }
        if count == 0 {
            return Err(BrokerErrorCode::Internal);
        }
        bytes = &bytes[usize::try_from(count).map_err(|_| BrokerErrorCode::Internal)?..];
    }
    Ok(())
}

fn open_mount(request: &BackendRequest, mount: &str) -> Result<OwnedFd, BrokerErrorCode> {
    let binding = request
        .bindings
        .filesystem_mounts
        .get(mount)
        .ok_or(BrokerErrorCode::OutOfScope)?;
    crate::filesystem::open_bound_mount(binding)
}

fn open_parent(root: RawFd, path: &str) -> Result<(OwnedFd, CString), BrokerErrorCode> {
    let path = Path::new(path);
    let name = path
        .file_name()
        .ok_or(BrokerErrorCode::InvalidRequest)
        .and_then(c_string)?;
    let parent = path.parent().ok_or(BrokerErrorCode::InvalidRequest)?;
    let descriptor = if parent.as_os_str().is_empty() {
        // SAFETY: F_DUPFD_CLOEXEC duplicates the live mount descriptor.
        let descriptor = unsafe { libc::fcntl(root, libc::F_DUPFD_CLOEXEC, 3) };
        owned_descriptor(descriptor)?
    } else {
        open_beneath(
            root,
            parent.to_str().ok_or(BrokerErrorCode::InvalidRequest)?,
            libc::O_PATH | libc::O_DIRECTORY,
        )?
    };
    Ok((descriptor, name))
}

fn open_beneath(root: RawFd, path: &str, flags: i32) -> Result<OwnedFd, BrokerErrorCode> {
    let path = CString::new(path).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    open_at_beneath(root, &path, flags)
}

fn open_at_beneath(root: RawFd, path: &CString, flags: i32) -> Result<OwnedFd, BrokerErrorCode> {
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: RESOLVE_FLAGS,
    };
    // SAFETY: syscall arguments point to initialized storage of the declared size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor = i32::try_from(descriptor).map_err(|_| os_error())?;
    owned_descriptor(descriptor)
}

fn create_exclusive(
    parent: RawFd,
    name: &CString,
    mode: libc::mode_t,
) -> Result<OwnedFd, BrokerErrorCode> {
    // SAFETY: name is nul-terminated and O_CREAT supplies the required mode.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    owned_descriptor(descriptor)
}

fn temporary_name() -> Result<CString, BrokerErrorCode> {
    let mut random = [0_u8; 16];
    let mut filled = 0;
    while filled < random.len() {
        // SAFETY: the remaining slice is writable for the supplied length.
        let count = unsafe {
            libc::syscall(
                libc::SYS_getrandom,
                random[filled..].as_mut_ptr(),
                random.len() - filled,
                0,
            )
        };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(BrokerErrorCode::BackendFailed);
        }
        if count == 0 {
            return Err(BrokerErrorCode::BackendFailed);
        }
        filled += usize::try_from(count).map_err(|_| BrokerErrorCode::BackendFailed)?;
    }
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    CString::new(format!("{INTERNAL_NAME_PREFIX}{random}.tmp"))
        .map_err(|_| BrokerErrorCode::Internal)
}

fn copy_exact(
    source: RawFd,
    destination: RawFd,
    length: u64,
    cancellation: &CancellationToken,
) -> Result<(), BrokerErrorCode> {
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut offset = 0_u64;
    while offset < length {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let remaining = usize::try_from((length - offset).min(COPY_BUFFER_BYTES as u64))
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        let file_offset =
            libc::off_t::try_from(offset).map_err(|_| BrokerErrorCode::BackendFailed)?;
        // SAFETY: source is live and buffer has room for remaining bytes.
        let count =
            unsafe { libc::pread(source, buffer.as_mut_ptr().cast(), remaining, file_offset) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os_error());
        }
        if count == 0 {
            return Err(BrokerErrorCode::BackendFailed);
        }
        let count = usize::try_from(count).map_err(|_| BrokerErrorCode::BackendFailed)?;
        pwrite_all_at(destination, &buffer[..count], offset, cancellation)?;
        offset = offset
            .checked_add(count as u64)
            .ok_or(BrokerErrorCode::BackendFailed)?;
    }
    Ok(())
}

fn write_and_sync(
    descriptor: RawFd,
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<(), BrokerErrorCode> {
    write_all(descriptor, bytes, cancellation)?;
    // SAFETY: descriptor is a live regular file.
    if unsafe { libc::fdatasync(descriptor) } != 0 {
        return Err(os_error());
    }
    Ok(())
}

fn write_all(
    descriptor: RawFd,
    mut bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<(), BrokerErrorCode> {
    while !bytes.is_empty() {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        // SAFETY: descriptor is live and bytes is readable for its length.
        let count = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os_error());
        }
        if count == 0 {
            return Err(BrokerErrorCode::BackendFailed);
        }
        bytes = &bytes[count as usize..];
    }
    Ok(())
}

fn pwrite_all_at(
    descriptor: RawFd,
    mut bytes: &[u8],
    mut offset: u64,
    cancellation: &CancellationToken,
) -> Result<(), BrokerErrorCode> {
    while !bytes.is_empty() {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let file_offset =
            libc::off_t::try_from(offset).map_err(|_| BrokerErrorCode::QuotaExceeded)?;
        // SAFETY: descriptor is live and bytes is readable for its length.
        let count =
            unsafe { libc::pwrite(descriptor, bytes.as_ptr().cast(), bytes.len(), file_offset) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os_error());
        }
        if count == 0 {
            return Err(BrokerErrorCode::BackendFailed);
        }
        let count = usize::try_from(count).map_err(|_| BrokerErrorCode::BackendFailed)?;
        bytes = &bytes[count..];
        offset = offset
            .checked_add(count as u64)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
    }
    Ok(())
}

fn exchange_and_remove(
    parent: RawFd,
    temporary_name: &CString,
    target_name: &CString,
    expected_target: &libc::stat,
    require_unchanged_snapshot: bool,
) -> Result<(), BrokerErrorCode> {
    if let Err(error) = rename_at(parent, temporary_name, parent, target_name, RENAME_EXCHANGE) {
        unlink(parent, temporary_name);
        return Err(error);
    }
    let swapped = metadata_at(parent, temporary_name);
    let valid = swapped.as_ref().is_ok_and(|metadata| {
        if require_unchanged_snapshot {
            same_snapshot(expected_target, metadata)
        } else {
            same_file(expected_target, metadata)
        }
    });
    if !valid {
        // With per-plugin serialization and the reserved random namespace, a
        // rollback cannot race another sandbox request. A hostile native
        // same-user process remains outside this sandbox's threat boundary.
        let rollback = rename_at(parent, temporary_name, parent, target_name, RENAME_EXCHANGE);
        return match (swapped, rollback) {
            (Err(error), Ok(())) => Err(error),
            _ => Err(BrokerErrorCode::BackendFailed),
        };
    }
    unlink_checked(parent, temporary_name)
}

fn rename_at(
    source_parent: RawFd,
    source: &CString,
    destination_parent: RawFd,
    destination: &CString,
    flags: u32,
) -> Result<(), BrokerErrorCode> {
    // SAFETY: both names are nul-terminated and both parents are pinned directories.
    if unsafe {
        libc::renameat2(
            source_parent,
            source.as_ptr(),
            destination_parent,
            destination.as_ptr(),
            flags,
        )
    } != 0
    {
        return Err(os_error());
    }
    Ok(())
}

fn metadata_at(parent: RawFd, name: &CString) -> Result<libc::stat, BrokerErrorCode> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata is writable and name/parent identify one non-followed entry.
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(os_error());
    }
    // SAFETY: fstatat succeeded.
    Ok(unsafe { metadata.assume_init() })
}

fn descriptor_metadata(descriptor: RawFd) -> Result<libc::stat, BrokerErrorCode> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata is writable and descriptor is live.
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(os_error());
    }
    // SAFETY: fstat succeeded.
    Ok(unsafe { metadata.assume_init() })
}

fn validate_regular_file(
    descriptor: RawFd,
    maximum_file_bytes: u64,
) -> Result<libc::stat, BrokerErrorCode> {
    let metadata = descriptor_metadata(descriptor)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG || metadata.st_nlink != 1 {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let size = u64::try_from(metadata.st_size).map_err(|_| BrokerErrorCode::BackendFailed)?;
    if size > maximum_file_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(metadata)
}

fn same_file(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && right.st_mode & libc::S_IFMT == libc::S_IFREG
        && right.st_nlink == 1
}

fn same_snapshot(left: &libc::stat, right: &libc::stat) -> bool {
    same_file(left, right)
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
}

fn same_precommit_snapshot(left: &libc::stat, right: &libc::stat) -> bool {
    same_snapshot(left, right)
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn unlink_checked(parent: RawFd, name: &CString) -> Result<(), BrokerErrorCode> {
    // SAFETY: name is nul-terminated and unlinkat without flags never follows it.
    if unsafe { libc::unlinkat(parent, name.as_ptr(), 0) } != 0 {
        return Err(os_error());
    }
    Ok(())
}

fn unlink(parent: RawFd, name: &CString) {
    // SAFETY: cleanup never follows the named entry.
    unsafe {
        libc::unlinkat(parent, name.as_ptr(), 0);
    }
}

fn sync_directory(descriptor: RawFd) -> Result<(), BrokerErrorCode> {
    // O_PATH cannot be fsynced. Reopen the pinned directory through procfs;
    // the descriptor target, not an attacker-controlled path, is used.
    let path = CString::new(format!("/proc/self/fd/{descriptor}"))
        .map_err(|_| BrokerErrorCode::Internal)?;
    // SAFETY: path is nul-terminated and flags require no mode.
    let sync_descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    let sync_descriptor = owned_descriptor(sync_descriptor)?;
    // SAFETY: sync_descriptor is a live directory descriptor.
    if unsafe { libc::fsync(sync_descriptor.as_raw_fd()) } != 0 {
        return Err(os_error());
    }
    Ok(())
}

fn owned_descriptor(descriptor: i32) -> Result<OwnedFd, BrokerErrorCode> {
    if descriptor < 0 {
        Err(os_error())
    } else {
        // SAFETY: successful open returned one uniquely owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn c_string(value: &OsStr) -> Result<CString, BrokerErrorCode> {
    CString::new(value.as_bytes()).map_err(|_| BrokerErrorCode::InvalidRequest)
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

fn os_error() -> BrokerErrorCode {
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EXDEV)
        | Some(libc::ELOOP)
        | Some(libc::ENOENT)
        | Some(libc::ENOTDIR)
        | Some(libc::EACCES)
        | Some(libc::EPERM)
        | Some(libc::EEXIST)
        | Some(libc::EISDIR)
        | Some(libc::ENOTEMPTY) => BrokerErrorCode::OutOfScope,
        Some(libc::ENOSPC) | Some(libc::EDQUOT) | Some(libc::EFBIG) => {
            BrokerErrorCode::QuotaExceeded
        }
        Some(libc::ENOSYS) => BrokerErrorCode::Unsupported,
        _ => BrokerErrorCode::BackendFailed,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::CString,
        fs,
        os::unix::{
            ffi::OsStrExt,
            fs::{PermissionsExt, symlink},
        },
        path::Path,
        sync::{Arc, Barrier, atomic::AtomicBool},
        thread,
        time::{Duration, Instant},
    };

    use semver::Version;
    use tempfile::tempdir;
    use touchbar_broker_schema::{
        FilesystemMutationResult, FilesystemPath, FilesystemRename, FilesystemWriteFile,
        FilesystemWriteStream, FilesystemWriteStreamChunk, FilesystemWriteStreamCommit,
        FilesystemWriteStreamOpened, MAX_FILE_WRITE_STREAM_CHUNK_BYTES,
    };
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        CapabilityId, CapabilityScope, FilesystemMountBinding, FilesystemMountRequest,
        FilesystemWriteScope, PackageInstance, Provenance, RuntimeKind, WriteOperation,
    };
    use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

    use crate::{
        ActivationLedger, AsyncBrokerRuntime, Backend, BackendRequest, CancellationToken,
        ConnectionIdentity, ExecutorLimits, HostEvent, HostEventQueue, LifecycleLimits,
        ResourceManager,
    };

    use super::*;

    fn scope(operations: impl IntoIterator<Item = WriteOperation>) -> FilesystemWriteScope {
        FilesystemWriteScope {
            mounts: BTreeSet::from([FilesystemMountRequest {
                label: "documents".into(),
                suggested_location: None,
            }]),
            operations: operations.into_iter().collect(),
            maximum_file_bytes: 1024,
            maximum_total_bytes_per_hour: 4096,
        }
    }

    fn request(
        root: &Path,
        id: u64,
        operation: &str,
        payload: Vec<u8>,
        scope: FilesystemWriteScope,
    ) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 1,
                package: PackageInstance {
                    source: GithubSource::new("alice", "write-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: id,
            capability: CapabilityId::FilesystemWriteV1,
            authorized_scope: CapabilityScope::FilesystemWrite(scope),
            bindings: touchbar_policy::GrantBindings {
                filesystem_mounts: BTreeMap::from([(
                    "documents".into(),
                    FilesystemMountBinding::from_directory(root).unwrap(),
                )]),
                ..Default::default()
            },
            activation: None,
            operation: operation.into(),
            payload,
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)).unwrap()
    }

    fn backend(state_parent: &Path) -> FilesystemWriteBackend {
        let state = state_parent.join("quota-state");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        FilesystemWriteBackend::new(&state, "github:alice/write-test").unwrap()
    }

    fn run(backend: &FilesystemWriteBackend, request: &BackendRequest) -> BrokerResult {
        backend
            .authorize(request, &mut ActivationLedger::new(8), 0)
            .unwrap();
        backend.execute(request, &token())
    }

    fn receipt(result: BrokerResult) -> FilesystemMutationResult {
        let BrokerResult::Success { payload } = result else {
            panic!("mutation failed: {result:?}");
        };
        FilesystemMutationResult::decode(&payload).unwrap()
    }

    fn stream_command(
        opened_from: &BackendRequest,
        request_id: u64,
        operation: &str,
        payload: Vec<u8>,
    ) -> BackendRequest {
        let mut request = opened_from.clone();
        request.request_id = request_id;
        request.operation = operation.into();
        request.payload = payload;
        request
    }

    fn runtime_with_write_backend(
        backend: Arc<FilesystemWriteBackend>,
        limits: LifecycleLimits,
    ) -> AsyncBrokerRuntime {
        let mut runtime = AsyncBrokerRuntime::new(ExecutorLimits::default(), limits, 64, 128)
            .expect("create broker runtime");
        runtime.register(CapabilityId::FilesystemWriteV1, backend.clone());
        runtime.register_resource(CapabilityId::FilesystemWriteV1, backend);
        runtime
    }

    fn execute_in_runtime(
        runtime: &mut AsyncBrokerRuntime,
        request: BackendRequest,
    ) -> BrokerResult {
        let request_id = request.request_id;
        runtime
            .submit(
                request,
                Duration::from_secs(5),
                u64::MAX,
                &mut ActivationLedger::new(8),
                0,
            )
            .expect("submit filesystem operation");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            runtime.pump_completions(1).expect("pump completions");
            while let Some(event) = runtime.pop_event() {
                if let HostEvent::Completion {
                    request_id: completed,
                    result,
                } = event
                    && completed == request_id
                {
                    return result;
                }
            }
            assert!(
                Instant::now() < deadline,
                "filesystem operation {request_id} did not complete"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn execute_batch_in_runtime(
        runtime: &mut AsyncBrokerRuntime,
        requests: Vec<BackendRequest>,
    ) -> BTreeMap<u64, BrokerResult> {
        let expected = requests
            .iter()
            .map(|request| request.request_id)
            .collect::<BTreeSet<_>>();
        for request in requests {
            runtime
                .submit(
                    request,
                    Duration::from_secs(5),
                    u64::MAX,
                    &mut ActivationLedger::new(8),
                    0,
                )
                .expect("submit filesystem operation");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut completions = BTreeMap::new();
        while completions.len() < expected.len() {
            runtime.pump_completions(1).expect("pump completions");
            while let Some(event) = runtime.pop_event() {
                if let HostEvent::Completion { request_id, result } = event {
                    assert!(expected.contains(&request_id));
                    assert!(completions.insert(request_id, result).is_none());
                }
            }
            assert!(
                Instant::now() < deadline,
                "only received {}/{} filesystem completions",
                completions.len(),
                expected.len()
            );
            thread::sleep(Duration::from_millis(1));
        }
        completions
    }

    fn broker_temporary_count(directory: &Path) -> usize {
        fs::read_dir(directory)
            .unwrap()
            .filter(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(INTERNAL_NAME_PREFIX)
                })
            })
            .count()
    }

    #[test]
    fn exact_operations_create_append_replace_rename_and_delete() {
        let root = tempdir().unwrap();
        let backend = backend(root.path());
        let all = scope([
            WriteOperation::Create,
            WriteOperation::Append,
            WriteOperation::Replace,
            WriteOperation::Rename,
            WriteOperation::Delete,
            WriteOperation::CreateDirectory,
        ]);

        let directory = FilesystemPath {
            mount: "documents".into(),
            path: "notes".into(),
        };
        assert_eq!(
            receipt(run(
                &backend,
                &request(
                    root.path(),
                    1,
                    FILESYSTEM_CREATE_DIRECTORY_OPERATION,
                    directory.encode().unwrap(),
                    all.clone(),
                ),
            )),
            FilesystemMutationResult {
                bytes_written: 0,
                resulting_size: None,
            }
        );

        let mut write = FilesystemWriteFile {
            mount: "documents".into(),
            path: "notes/a.txt".into(),
            bytes: b"hello".to_vec(),
        };
        assert_eq!(
            receipt(run(
                &backend,
                &request(
                    root.path(),
                    2,
                    FILESYSTEM_CREATE_FILE_OPERATION,
                    write.encode().unwrap(),
                    all.clone(),
                ),
            )),
            FilesystemMutationResult {
                bytes_written: 5,
                resulting_size: Some(5),
            }
        );
        write.bytes = b" world".to_vec();
        assert_eq!(
            receipt(run(
                &backend,
                &request(
                    root.path(),
                    3,
                    FILESYSTEM_APPEND_FILE_OPERATION,
                    write.encode().unwrap(),
                    all.clone(),
                ),
            ))
            .resulting_size,
            Some(11)
        );
        assert_eq!(
            fs::read(root.path().join("notes/a.txt")).unwrap(),
            b"hello world"
        );

        write.bytes = b"replacement".to_vec();
        receipt(run(
            &backend,
            &request(
                root.path(),
                4,
                FILESYSTEM_REPLACE_FILE_OPERATION,
                write.encode().unwrap(),
                all.clone(),
            ),
        ));
        assert_eq!(
            fs::read(root.path().join("notes/a.txt")).unwrap(),
            b"replacement"
        );
        assert!(
            fs::read_dir(root.path().join("notes"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".touchbar-"))
        );

        let rename = FilesystemRename {
            mount: "documents".into(),
            source: "notes/a.txt".into(),
            destination: "notes/b.txt".into(),
        };
        receipt(run(
            &backend,
            &request(
                root.path(),
                5,
                FILESYSTEM_RENAME_OPERATION,
                rename.encode().unwrap(),
                all.clone(),
            ),
        ));
        assert!(!root.path().join("notes/a.txt").exists());
        assert_eq!(
            fs::read(root.path().join("notes/b.txt")).unwrap(),
            b"replacement"
        );

        let delete = FilesystemPath {
            mount: "documents".into(),
            path: "notes/b.txt".into(),
        };
        receipt(run(
            &backend,
            &request(
                root.path(),
                6,
                FILESYSTEM_DELETE_FILE_OPERATION,
                delete.encode().unwrap(),
                all,
            ),
        ));
        assert!(!root.path().join("notes/b.txt").exists());
    }

    #[test]
    fn operation_path_mount_and_byte_authority_are_exact() {
        let root = tempdir().unwrap();
        let backend = backend(root.path());
        let create_only = scope([WriteOperation::Create]);
        let write = FilesystemWriteFile {
            mount: "documents".into(),
            path: "file".into(),
            bytes: b"data".to_vec(),
        };
        let replace = request(
            root.path(),
            1,
            FILESYSTEM_REPLACE_FILE_OPERATION,
            write.encode().unwrap(),
            create_only.clone(),
        );
        assert_eq!(
            backend.authorize(&replace, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::OutOfScope)
        );

        for path in ["", "/etc/passwd", "../escape", "a/../escape", "./file"] {
            let write = FilesystemWriteFile {
                mount: "documents".into(),
                path: path.into(),
                bytes: Vec::new(),
            };
            let request = request(
                root.path(),
                2,
                FILESYSTEM_CREATE_FILE_OPERATION,
                write.encode().unwrap(),
                create_only.clone(),
            );
            assert_eq!(
                backend.authorize(&request, &mut ActivationLedger::new(8), 0),
                Err(BrokerErrorCode::OutOfScope),
                "{path}"
            );
        }

        let wrong_mount = FilesystemWriteFile {
            mount: "other".into(),
            path: "file".into(),
            bytes: Vec::new(),
        };
        let wrong_mount_request = request(
            root.path(),
            3,
            FILESYSTEM_CREATE_FILE_OPERATION,
            wrong_mount.encode().unwrap(),
            create_only.clone(),
        );
        assert_eq!(
            backend.authorize(&wrong_mount_request, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::OutOfScope)
        );

        let mut tiny = create_only;
        tiny.maximum_file_bytes = 3;
        let request = request(
            root.path(),
            4,
            FILESYSTEM_CREATE_FILE_OPERATION,
            write.encode().unwrap(),
            tiny,
        );
        assert_eq!(
            backend.authorize(&request, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );
    }

    #[test]
    fn existing_targets_symlinks_hardlinks_and_special_files_fail_closed() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("existing"), b"safe").unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        symlink(outside.join("secret"), root.join("link")).unwrap();
        fs::hard_link(outside.join("secret"), root.join("hardlink")).unwrap();
        let fifo = root.join("fifo");
        let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is a valid nul-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let backend = backend(parent.path());
        let all = scope([
            WriteOperation::Create,
            WriteOperation::Append,
            WriteOperation::Replace,
            WriteOperation::Delete,
            WriteOperation::Rename,
        ]);
        let create_existing = FilesystemWriteFile {
            mount: "documents".into(),
            path: "existing".into(),
            bytes: b"bad".to_vec(),
        };
        assert_eq!(
            run(
                &backend,
                &request(
                    &root,
                    1,
                    FILESYSTEM_CREATE_FILE_OPERATION,
                    create_existing.encode().unwrap(),
                    all.clone(),
                )
            ),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(fs::read(root.join("existing")).unwrap(), b"safe");

        for (id, path) in [(2, "link"), (3, "hardlink"), (4, "fifo")] {
            let append = FilesystemWriteFile {
                mount: "documents".into(),
                path: path.into(),
                bytes: b"bad".to_vec(),
            };
            assert!(matches!(
                run(
                    &backend,
                    &request(
                        &root,
                        id,
                        FILESYSTEM_APPEND_FILE_OPERATION,
                        append.encode().unwrap(),
                        all.clone(),
                    )
                ),
                BrokerResult::Error(_)
            ));
        }
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"secret");

        let delete_directory = FilesystemPath {
            mount: "documents".into(),
            path: "../outside".into(),
        };
        let invalid = request(
            &root,
            5,
            FILESYSTEM_DELETE_FILE_OPERATION,
            delete_directory.encode().unwrap(),
            all,
        );
        assert_eq!(
            backend.authorize(&invalid, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::OutOfScope)
        );
    }

    #[test]
    fn rename_never_overwrites_and_append_obeys_resulting_size() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("source"), b"source").unwrap();
        fs::write(root.path().join("destination"), b"destination").unwrap();
        let backend = backend(root.path());
        let permissions = scope([WriteOperation::Rename, WriteOperation::Append]);
        let rename = FilesystemRename {
            mount: "documents".into(),
            source: "source".into(),
            destination: "destination".into(),
        };
        assert_eq!(
            run(
                &backend,
                &request(
                    root.path(),
                    1,
                    FILESYSTEM_RENAME_OPERATION,
                    rename.encode().unwrap(),
                    permissions.clone(),
                )
            ),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("destination")).unwrap(),
            b"destination"
        );

        let mut small = permissions;
        small.maximum_file_bytes = 8;
        let append = FilesystemWriteFile {
            mount: "documents".into(),
            path: "source".into(),
            bytes: b"more".to_vec(),
        };
        assert_eq!(
            run(
                &backend,
                &request(
                    root.path(),
                    2,
                    FILESYSTEM_APPEND_FILE_OPERATION,
                    append.encode().unwrap(),
                    small,
                )
            ),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );
        assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
    }

    #[test]
    fn hourly_quota_is_reserved_before_io_and_cancellation_creates_nothing() {
        let root = tempdir().unwrap();
        let backend = backend(root.path());
        let mut permissions = scope([WriteOperation::Create]);
        permissions.maximum_total_bytes_per_hour = 5;
        let first = FilesystemWriteFile {
            mount: "documents".into(),
            path: "first".into(),
            bytes: vec![1; 4],
        };
        let first = request(
            root.path(),
            1,
            FILESYSTEM_CREATE_FILE_OPERATION,
            first.encode().unwrap(),
            permissions.clone(),
        );
        assert!(
            backend
                .authorize(&first, &mut ActivationLedger::new(8), 0)
                .is_ok()
        );
        let second = FilesystemWriteFile {
            mount: "documents".into(),
            path: "second".into(),
            bytes: vec![2; 2],
        };
        let second = request(
            root.path(),
            2,
            FILESYSTEM_CREATE_FILE_OPERATION,
            second.encode().unwrap(),
            permissions,
        );
        assert_eq!(
            backend.authorize(&second, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );

        let cancelled = CancellationToken::new(
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            backend.execute(&first, &cancelled),
            BrokerResult::Error(BrokerErrorCode::Cancelled)
        );
        assert!(!root.path().join("first").exists());
    }

    #[test]
    fn pinned_parent_prevents_directory_replacement_escape() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        let original = root.join("inside");
        let moved = parent.path().join("moved");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&original).unwrap();
        fs::create_dir(&outside).unwrap();
        let root_fd = super::open_mount(
            &request(
                &root,
                1,
                FILESYSTEM_CREATE_FILE_OPERATION,
                FilesystemWriteFile {
                    mount: "documents".into(),
                    path: "inside/new".into(),
                    bytes: Vec::new(),
                }
                .encode()
                .unwrap(),
                scope([WriteOperation::Create]),
            ),
            "documents",
        )
        .unwrap();
        let (pinned_parent, name) = super::open_parent(root_fd.as_raw_fd(), "inside/new").unwrap();
        fs::rename(&original, &moved).unwrap();
        symlink(&outside, &original).unwrap();
        let file = super::create_exclusive(pinned_parent.as_raw_fd(), &name, 0o600).unwrap();
        super::write_and_sync(file.as_raw_fd(), b"safe", &token()).unwrap();
        assert_eq!(fs::read(moved.join("new")).unwrap(), b"safe");
        assert!(!outside.join("new").exists());
    }

    #[test]
    fn replaced_mount_identity_cannot_redirect_a_write() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        let old_root = parent.path().join("old-root");
        fs::create_dir(&root).unwrap();
        let backend = backend(parent.path());
        let write = FilesystemWriteFile {
            mount: "documents".into(),
            path: "created".into(),
            bytes: b"must stay contained".to_vec(),
        };
        let request = request(
            &root,
            1,
            FILESYSTEM_CREATE_FILE_OPERATION,
            write.encode().unwrap(),
            scope([WriteOperation::Create]),
        );
        fs::rename(&root, &old_root).unwrap();
        fs::create_dir(&root).unwrap();

        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
        assert!(!root.join("created").exists());
        assert!(!old_root.join("created").exists());
    }

    #[test]
    fn broker_temporary_namespace_is_never_requestable() {
        let root = tempdir().unwrap();
        let backend = backend(root.path());
        for (id, path) in [
            (1, ".touchbar-forged.tmp"),
            (2, "nested/.touchbar-forged.tmp"),
            (3, ".touchbar-directory/file"),
        ] {
            let write = FilesystemWriteFile {
                mount: "documents".into(),
                path: path.into(),
                bytes: b"forged".to_vec(),
            };
            let request = request(
                root.path(),
                id,
                FILESYSTEM_CREATE_FILE_OPERATION,
                write.encode().unwrap(),
                scope([WriteOperation::Create]),
            );
            assert_eq!(
                backend.authorize(&request, &mut ActivationLedger::new(8), 0),
                Err(BrokerErrorCode::OutOfScope),
                "{path}"
            );
        }
    }

    #[test]
    fn concurrent_plugin_appends_are_serialized_and_atomic() {
        const APPENDS: usize = 32;

        let root = tempdir().unwrap();
        fs::write(root.path().join("journal"), b"seed").unwrap();
        let backend = Arc::new(backend(root.path()));
        let barrier = Arc::new(Barrier::new(APPENDS));
        let mut handles = Vec::new();
        for id in 0..APPENDS {
            let root = root.path().to_owned();
            let backend = backend.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                let write = FilesystemWriteFile {
                    mount: "documents".into(),
                    path: "journal".into(),
                    bytes: b"x".to_vec(),
                };
                let request = request(
                    &root,
                    id as u64 + 1,
                    FILESYSTEM_APPEND_FILE_OPERATION,
                    write.encode().unwrap(),
                    scope([WriteOperation::Append]),
                );
                backend
                    .authorize(&request, &mut ActivationLedger::new(8), 0)
                    .unwrap();
                barrier.wait();
                let cancellation = CancellationToken::new(
                    Arc::new(AtomicBool::new(false)),
                    Duration::from_secs(10),
                )
                .unwrap();
                backend.execute(&request, &cancellation)
            }));
        }
        for handle in handles {
            assert!(matches!(
                handle.join().unwrap(),
                BrokerResult::Success { .. }
            ));
        }
        assert_eq!(
            fs::read(root.path().join("journal")).unwrap(),
            [b"seed".as_slice(), vec![b'x'; APPENDS].as_slice()].concat()
        );
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(INTERNAL_NAME_PREFIX)
        }));
    }

    #[test]
    fn quota_survives_restart_and_is_bound_to_plugin_source() {
        let root = tempdir().unwrap();
        let state = root.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let mut permissions = scope([WriteOperation::Create]);
        permissions.maximum_total_bytes_per_hour = 5;

        let first = FilesystemWriteFile {
            mount: "documents".into(),
            path: "first".into(),
            bytes: vec![1; 4],
        };
        let first = request(
            root.path(),
            1,
            FILESYSTEM_CREATE_FILE_OPERATION,
            first.encode().unwrap(),
            permissions.clone(),
        );
        let backend = FilesystemWriteBackend::new(&state, "github:alice/write-test").unwrap();
        backend
            .authorize(&first, &mut ActivationLedger::new(8), 0)
            .unwrap();
        drop(backend);

        let second = FilesystemWriteFile {
            mount: "documents".into(),
            path: "second".into(),
            bytes: vec![2; 2],
        };
        let second = request(
            root.path(),
            2,
            FILESYSTEM_CREATE_FILE_OPERATION,
            second.encode().unwrap(),
            permissions,
        );
        let restarted = FilesystemWriteBackend::new(&state, "github:alice/write-test").unwrap();
        assert_eq!(
            restarted.authorize(&second, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );

        let other_plugin =
            FilesystemWriteBackend::new(&state, "github:bob/independent-test").unwrap();
        assert!(
            other_plugin
                .authorize(&second, &mut ActivationLedger::new(8), 0)
                .is_ok()
        );
    }

    #[test]
    fn quota_state_is_private_and_corruption_fails_closed() {
        let root = tempdir().unwrap();
        let state = root.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FilesystemWriteBackend::new(&state, "github:alice/write-test").is_err());
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();

        let backend = FilesystemWriteBackend::new(&state, "github:alice/write-test").unwrap();
        let write = FilesystemWriteFile {
            mount: "documents".into(),
            path: "file".into(),
            bytes: b"x".to_vec(),
        };
        let request = request(
            root.path(),
            1,
            FILESYSTEM_CREATE_FILE_OPERATION,
            write.encode().unwrap(),
            scope([WriteOperation::Create]),
        );
        backend
            .authorize(&request, &mut ActivationLedger::new(8), 0)
            .unwrap();
        let record_name = backend.quota.record_name.to_str().unwrap();
        fs::write(state.join(record_name), b"truncated").unwrap();
        assert_eq!(
            backend.authorize(&request, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::Internal)
        );

        let link = root.path().join("state-link");
        symlink(&state, &link).unwrap();
        assert!(FilesystemWriteBackend::new(&link, "github:alice/write-test").is_err());
    }

    #[test]
    fn large_create_stream_is_ordered_atomic_and_abortable() {
        let root = tempdir().unwrap();
        let backend = Arc::new(backend(root.path()));
        let mut permissions = scope([WriteOperation::Create]);
        permissions.maximum_file_bytes = 256 * 1024;
        permissions.maximum_total_bytes_per_hour = 512 * 1024;
        let contents = (0..96 * 1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let begin = request(
            root.path(),
            1,
            FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
            FilesystemWriteStream {
                mount: "documents".into(),
                path: "large.bin".into(),
                expected_bytes: contents.len() as u64,
            }
            .encode()
            .unwrap(),
            permissions.clone(),
        );
        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::FilesystemWriteV1, backend.clone());
        let limits = crate::ResourceBackend::limits(backend.as_ref(), &begin).unwrap();
        let opened = resources
            .open(9, &begin, limits, &mut ActivationLedger::new(8), 0)
            .unwrap();
        assert_eq!(
            FilesystemWriteStreamOpened::decode(&opened).unwrap(),
            FilesystemWriteStreamOpened {
                resource_id: 9,
                maximum_chunk_bytes: MAX_FILE_WRITE_STREAM_CHUNK_BYTES as u32,
            }
        );
        assert!(!root.path().join("large.bin").exists());

        let out_of_order = request(
            root.path(),
            2,
            FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
            FilesystemWriteStreamChunk {
                resource_id: 9,
                offset: 1,
                bytes: vec![1],
            }
            .encode()
            .unwrap(),
            permissions.clone(),
        );
        assert_eq!(
            Backend::authorize(&*backend, &out_of_order, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::InvalidRequest)
        );
        let early_commit = request(
            root.path(),
            3,
            FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
            FilesystemWriteStreamCommit { resource_id: 9 }
                .encode()
                .unwrap(),
            permissions.clone(),
        );
        assert_eq!(
            Backend::authorize(&*backend, &early_commit, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::InvalidRequest)
        );

        for (index, chunk) in contents
            .chunks(MAX_FILE_WRITE_STREAM_CHUNK_BYTES)
            .enumerate()
        {
            let offset = index * MAX_FILE_WRITE_STREAM_CHUNK_BYTES;
            let chunk_request = request(
                root.path(),
                4 + index as u64,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id: 9,
                    offset: offset as u64,
                    bytes: chunk.to_vec(),
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            assert!(matches!(
                run(&backend, &chunk_request),
                BrokerResult::Success { .. }
            ));
            assert!(!root.path().join("large.bin").exists());
        }
        let commit = request(
            root.path(),
            100,
            FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
            FilesystemWriteStreamCommit { resource_id: 9 }
                .encode()
                .unwrap(),
            permissions.clone(),
        );
        assert_eq!(
            receipt(run(&backend, &commit)).bytes_written,
            contents.len() as u64
        );
        assert_eq!(fs::read(root.path().join("large.bin")).unwrap(), contents);
        let mut events = HostEventQueue::new(4, 8);
        assert_eq!(resources.pump(&mut events, 1), vec![9]);
        assert!(matches!(
            events.pop(),
            Some(HostEvent::ResourceEvent {
                resource_id: 9,
                result: BrokerResult::Success { .. },
                ..
            })
        ));

        let abort = request(
            root.path(),
            101,
            FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
            FilesystemWriteStream {
                mount: "documents".into(),
                path: "aborted.bin".into(),
                expected_bytes: 10,
            }
            .encode()
            .unwrap(),
            permissions,
        );
        let limits = crate::ResourceBackend::limits(backend.as_ref(), &abort).unwrap();
        resources
            .open(10, &abort, limits, &mut ActivationLedger::new(8), 0)
            .unwrap();
        assert!(resources.close(10));
        assert!(!root.path().join("aborted.bin").exists());
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(INTERNAL_NAME_PREFIX)
        }));
    }

    #[test]
    fn streamed_replace_rechecks_pinned_target_and_current_grant() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("target"), b"original").unwrap();
        let backend = Arc::new(backend(root.path()));
        let mut permissions = scope([WriteOperation::Replace]);
        permissions.maximum_file_bytes = 128 * 1024;
        permissions.maximum_total_bytes_per_hour = 256 * 1024;
        let contents = vec![7; 64 * 1024];
        let begin = request(
            root.path(),
            1,
            FILESYSTEM_REPLACE_FILE_STREAM_OPERATION,
            FilesystemWriteStream {
                mount: "documents".into(),
                path: "target".into(),
                expected_bytes: contents.len() as u64,
            }
            .encode()
            .unwrap(),
            permissions.clone(),
        );
        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::FilesystemWriteV1, backend.clone());
        let limits = crate::ResourceBackend::limits(backend.as_ref(), &begin).unwrap();
        resources
            .open(11, &begin, limits, &mut ActivationLedger::new(8), 0)
            .unwrap();
        for (index, chunk) in contents
            .chunks(MAX_FILE_WRITE_STREAM_CHUNK_BYTES)
            .enumerate()
        {
            let offset = index * MAX_FILE_WRITE_STREAM_CHUNK_BYTES;
            let chunk_request = request(
                root.path(),
                2 + index as u64,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id: 11,
                    offset: offset as u64,
                    bytes: chunk.to_vec(),
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            assert!(matches!(
                run(&backend, &chunk_request),
                BrokerResult::Success { .. }
            ));
        }

        let mut narrowed = permissions.clone();
        narrowed.maximum_file_bytes -= 1;
        let wrong_authority = request(
            root.path(),
            90,
            FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
            FilesystemWriteStreamCommit { resource_id: 11 }
                .encode()
                .unwrap(),
            narrowed,
        );
        assert_eq!(
            Backend::authorize(
                &*backend,
                &wrong_authority,
                &mut ActivationLedger::new(8),
                0,
            ),
            Err(BrokerErrorCode::OutOfScope)
        );

        fs::rename(root.path().join("target"), root.path().join("old-target")).unwrap();
        fs::write(root.path().join("target"), b"intruder").unwrap();
        let commit = request(
            root.path(),
            91,
            FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
            FilesystemWriteStreamCommit { resource_id: 11 }
                .encode()
                .unwrap(),
            permissions,
        );
        assert_eq!(
            run(&backend, &commit),
            BrokerResult::Error(BrokerErrorCode::BackendFailed)
        );
        assert_eq!(fs::read(root.path().join("target")).unwrap(), b"intruder");
        assert!(resources.close(11));
    }

    #[test]
    fn streamed_append_is_atomic_and_rejects_source_changes() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("target"), b"base:").unwrap();
        let backend = Arc::new(backend(root.path()));
        let mut permissions = scope([WriteOperation::Append]);
        permissions.maximum_file_bytes = 128 * 1024;
        permissions.maximum_total_bytes_per_hour = 256 * 1024;
        let appended = vec![b'x'; 32 * 1024];
        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::FilesystemWriteV1, backend.clone());

        let begin = request(
            root.path(),
            1,
            FILESYSTEM_APPEND_FILE_STREAM_OPERATION,
            FilesystemWriteStream {
                mount: "documents".into(),
                path: "target".into(),
                expected_bytes: appended.len() as u64,
            }
            .encode()
            .unwrap(),
            permissions.clone(),
        );
        let limits = crate::ResourceBackend::limits(backend.as_ref(), &begin).unwrap();
        resources
            .open(12, &begin, limits, &mut ActivationLedger::new(8), 0)
            .unwrap();
        for (index, chunk) in appended
            .chunks(MAX_FILE_WRITE_STREAM_CHUNK_BYTES)
            .enumerate()
        {
            let offset = index * MAX_FILE_WRITE_STREAM_CHUNK_BYTES;
            let chunk_request = request(
                root.path(),
                2 + index as u64,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id: 12,
                    offset: offset as u64,
                    bytes: chunk.to_vec(),
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            assert!(matches!(
                run(&backend, &chunk_request),
                BrokerResult::Success { .. }
            ));
            assert_eq!(fs::read(root.path().join("target")).unwrap(), b"base:");
        }
        let commit = request(
            root.path(),
            50,
            FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
            FilesystemWriteStreamCommit { resource_id: 12 }
                .encode()
                .unwrap(),
            permissions.clone(),
        );
        assert_eq!(
            receipt(run(&backend, &commit)).resulting_size,
            Some(5 + appended.len() as u64)
        );
        assert_eq!(
            fs::read(root.path().join("target")).unwrap(),
            [b"base:".as_slice(), appended.as_slice()].concat()
        );
        let mut events = HostEventQueue::new(4, 8);
        assert_eq!(resources.pump(&mut events, 1), vec![12]);

        let begin = request(
            root.path(),
            51,
            FILESYSTEM_APPEND_FILE_STREAM_OPERATION,
            FilesystemWriteStream {
                mount: "documents".into(),
                path: "target".into(),
                expected_bytes: 1,
            }
            .encode()
            .unwrap(),
            permissions.clone(),
        );
        let limits = crate::ResourceBackend::limits(backend.as_ref(), &begin).unwrap();
        resources
            .open(13, &begin, limits, &mut ActivationLedger::new(8), 0)
            .unwrap();
        let chunk = request(
            root.path(),
            52,
            FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
            FilesystemWriteStreamChunk {
                resource_id: 13,
                offset: 0,
                bytes: b"z".to_vec(),
            }
            .encode()
            .unwrap(),
            permissions.clone(),
        );
        assert!(matches!(
            run(&backend, &chunk),
            BrokerResult::Success { .. }
        ));
        fs::write(root.path().join("target"), b"changed externally").unwrap();
        let commit = request(
            root.path(),
            53,
            FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
            FilesystemWriteStreamCommit { resource_id: 13 }
                .encode()
                .unwrap(),
            permissions,
        );
        assert_eq!(
            run(&backend, &commit),
            BrokerResult::Error(BrokerErrorCode::BackendFailed)
        );
        assert_eq!(
            fs::read(root.path().join("target")).unwrap(),
            b"changed externally"
        );
        assert!(resources.close(13));
    }

    #[test]
    #[ignore = "release-gate external-mutator campaign"]
    fn security_campaign_external_mutators_remain_contained() {
        const ROUNDS: u64 = 32;

        let sandbox = tempdir().unwrap();
        let root = sandbox.path().join("root");
        let outside = sandbox.path().join("outside");
        let state = sandbox.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(outside.join("sentinel"), b"outside").unwrap();

        let backend = Arc::new(
            FilesystemWriteBackend::new(&state, "github:alice/write-mutator-campaign").unwrap(),
        );
        let mut runtime = runtime_with_write_backend(backend, LifecycleLimits::default());
        let mut permissions = scope([
            WriteOperation::Create,
            WriteOperation::Replace,
            WriteOperation::Append,
        ]);
        permissions.maximum_file_bytes = 64 * 1024;
        permissions.maximum_total_bytes_per_hour = 4 * 1024 * 1024;
        let mut request_id = 1_u64;

        // Replacing a path component after open cannot redirect a stream: the
        // upload remains attached to the already-pinned directory descriptor.
        fs::create_dir(root.join("inside")).unwrap();
        for round in 0..ROUNDS {
            let begin = request(
                &root,
                request_id,
                FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
                FilesystemWriteStream {
                    mount: "documents".into(),
                    path: "inside/result".into(),
                    expected_bytes: 8,
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            request_id += 1;
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            let parked = sandbox.path().join(format!("parked-parent-{round}"));
            fs::rename(root.join("inside"), &parked).unwrap();
            symlink(&outside, root.join("inside")).unwrap();

            let chunk = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id,
                    offset: 0,
                    bytes: round.to_le_bytes().to_vec(),
                }
                .encode()
                .unwrap(),
            );
            request_id += 1;
            assert!(matches!(
                execute_in_runtime(&mut runtime, chunk),
                BrokerResult::Success { .. }
            ));
            let commit = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
                FilesystemWriteStreamCommit { resource_id }
                    .encode()
                    .unwrap(),
            );
            request_id += 1;
            assert!(matches!(
                execute_in_runtime(&mut runtime, commit),
                BrokerResult::Success { .. }
            ));
            assert_eq!(
                fs::read(parked.join("result")).unwrap(),
                round.to_le_bytes()
            );
            assert!(!outside.join("result").exists());
            assert_eq!(runtime.pump_resource_events(1), 1);
            while runtime.pop_event().is_some() {}

            fs::remove_file(root.join("inside")).unwrap();
            fs::remove_file(parked.join("result")).unwrap();
            fs::rename(&parked, root.join("inside")).unwrap();
            assert_eq!(broker_temporary_count(&root.join("inside")), 0);
        }

        // Replacing the grant's pathname likewise cannot redirect an upload
        // opened against the original device/inode-bound root.
        for round in 0..ROUNDS {
            let begin = request(
                &root,
                request_id,
                FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
                FilesystemWriteStream {
                    mount: "documents".into(),
                    path: "root-result".into(),
                    expected_bytes: 8,
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            request_id += 1;
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            let parked = sandbox.path().join(format!("parked-root-{round}"));
            fs::rename(&root, &parked).unwrap();
            fs::create_dir(&root).unwrap();

            let chunk = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id,
                    offset: 0,
                    bytes: round.to_le_bytes().to_vec(),
                }
                .encode()
                .unwrap(),
            );
            request_id += 1;
            assert!(matches!(
                execute_in_runtime(&mut runtime, chunk),
                BrokerResult::Success { .. }
            ));
            let commit = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
                FilesystemWriteStreamCommit { resource_id }
                    .encode()
                    .unwrap(),
            );
            request_id += 1;
            assert!(matches!(
                execute_in_runtime(&mut runtime, commit),
                BrokerResult::Success { .. }
            ));
            assert_eq!(
                fs::read(parked.join("root-result")).unwrap(),
                round.to_le_bytes()
            );
            assert!(!root.join("root-result").exists());
            assert_eq!(runtime.pump_resource_events(1), 1);
            while runtime.pop_event().is_some() {}

            fs::remove_dir(&root).unwrap();
            fs::remove_file(parked.join("root-result")).unwrap();
            fs::rename(&parked, &root).unwrap();
            assert_eq!(broker_temporary_count(&root), 0);
        }

        // Replacing or modifying the target after it was pinned must fail the
        // commit and preserve the external writer's current entry.
        for round in 0..ROUNDS {
            let target = root.join("target");
            let displaced = root.join("displaced");
            fs::write(&target, format!("original-{round}")).unwrap();
            let begin = request(
                &root,
                request_id,
                FILESYSTEM_REPLACE_FILE_STREAM_OPERATION,
                FilesystemWriteStream {
                    mount: "documents".into(),
                    path: "target".into(),
                    expected_bytes: 8,
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            request_id += 1;
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            let chunk = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id,
                    offset: 0,
                    bytes: round.to_le_bytes().to_vec(),
                }
                .encode()
                .unwrap(),
            );
            request_id += 1;
            assert!(matches!(
                execute_in_runtime(&mut runtime, chunk),
                BrokerResult::Success { .. }
            ));
            fs::rename(&target, &displaced).unwrap();
            fs::write(&target, format!("intruder-{round}")).unwrap();
            let commit = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
                FilesystemWriteStreamCommit { resource_id }
                    .encode()
                    .unwrap(),
            );
            request_id += 1;
            assert_eq!(
                execute_in_runtime(&mut runtime, commit),
                BrokerResult::Error(BrokerErrorCode::BackendFailed)
            );
            assert_eq!(
                fs::read(&target).unwrap(),
                format!("intruder-{round}").as_bytes()
            );
            assert_eq!(
                fs::read(&displaced).unwrap(),
                format!("original-{round}").as_bytes()
            );
            assert!(runtime.close_resource(resource_id));
            fs::remove_file(&target).unwrap();
            fs::remove_file(&displaced).unwrap();
            assert_eq!(broker_temporary_count(&root), 0);

            fs::write(&target, format!("base-{round}")).unwrap();
            let begin = request(
                &root,
                request_id,
                FILESYSTEM_APPEND_FILE_STREAM_OPERATION,
                FilesystemWriteStream {
                    mount: "documents".into(),
                    path: "target".into(),
                    expected_bytes: 1,
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            request_id += 1;
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            let chunk = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id,
                    offset: 0,
                    bytes: b"!".to_vec(),
                }
                .encode()
                .unwrap(),
            );
            request_id += 1;
            assert!(matches!(
                execute_in_runtime(&mut runtime, chunk),
                BrokerResult::Success { .. }
            ));
            fs::write(&target, format!("externally-changed-{round}")).unwrap();
            let commit = stream_command(
                &begin,
                request_id,
                FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
                FilesystemWriteStreamCommit { resource_id }
                    .encode()
                    .unwrap(),
            );
            request_id += 1;
            assert_eq!(
                execute_in_runtime(&mut runtime, commit),
                BrokerResult::Error(BrokerErrorCode::BackendFailed)
            );
            assert_eq!(
                fs::read(&target).unwrap(),
                format!("externally-changed-{round}").as_bytes()
            );
            assert!(runtime.close_resource(resource_id));
            fs::remove_file(&target).unwrap();
            assert_eq!(broker_temporary_count(&root), 0);
        }

        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
        assert_eq!(runtime.lifecycle().resource_count(), 0);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 0);
    }

    #[test]
    #[ignore = "release-gate streamed-write resource-pressure campaign"]
    fn security_campaign_stream_pressure_is_atomic_and_reclaimable() {
        const CAPACITY: usize = 16;

        let sandbox = tempdir().unwrap();
        let root = sandbox.path().join("root");
        let state = sandbox.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let backend = Arc::new(
            FilesystemWriteBackend::new(&state, "github:alice/write-pressure-campaign").unwrap(),
        );
        let mut runtime = runtime_with_write_backend(backend.clone(), LifecycleLimits::default());
        let mut permissions = scope([WriteOperation::Create]);
        permissions.maximum_file_bytes = 64 * 1024;
        permissions.maximum_total_bytes_per_hour = 4 * 1024 * 1024;

        let mut streams = Vec::new();
        for index in 0..CAPACITY {
            let begin = request(
                &root,
                index as u64 + 1,
                FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
                FilesystemWriteStream {
                    mount: "documents".into(),
                    path: format!("item-{index}"),
                    expected_bytes: 1,
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            streams.push((resource_id, begin));
        }
        let reserved = CAPACITY * MAX_FILE_WRITE_STREAM_CHUNK_BYTES;
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY);
        assert_eq!(runtime.lifecycle().buffered_bytes(), reserved);
        assert_eq!(broker_temporary_count(&root), CAPACITY);

        let overflow = request(
            &root,
            100,
            FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
            FilesystemWriteStream {
                mount: "documents".into(),
                path: "overflow".into(),
                expected_bytes: 1,
            }
            .encode()
            .unwrap(),
            permissions.clone(),
        );
        assert_eq!(
            runtime.open_resource(&overflow, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY);
        assert_eq!(runtime.lifecycle().buffered_bytes(), reserved);
        assert_eq!(broker_temporary_count(&root), CAPACITY);

        for (resource_id, _) in streams.drain(..CAPACITY / 2) {
            assert!(runtime.close_resource(resource_id));
        }
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY / 2);
        assert_eq!(broker_temporary_count(&root), CAPACITY / 2);

        let mut last_resource_id = streams.last().unwrap().0;
        for index in CAPACITY..CAPACITY + CAPACITY / 2 {
            let begin = request(
                &root,
                index as u64 + 1,
                FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
                FilesystemWriteStream {
                    mount: "documents".into(),
                    path: format!("item-{index}"),
                    expected_bytes: 1,
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            assert!(resource_id > last_resource_id);
            last_resource_id = resource_id;
            streams.push((resource_id, begin));
        }
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY);
        assert_eq!(broker_temporary_count(&root), CAPACITY);

        let chunk_requests = streams
            .iter()
            .enumerate()
            .map(|(index, (resource_id, begin))| {
                stream_command(
                    begin,
                    1_000 + index as u64,
                    FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                    FilesystemWriteStreamChunk {
                        resource_id: *resource_id,
                        offset: 0,
                        bytes: vec![index as u8],
                    }
                    .encode()
                    .unwrap(),
                )
            })
            .collect();
        let chunks = execute_batch_in_runtime(&mut runtime, chunk_requests);
        assert!(
            chunks
                .values()
                .all(|result| matches!(result, BrokerResult::Success { .. }))
        );
        assert!(streams.iter().all(|(_, begin)| {
            !root
                .join(FilesystemWriteStream::decode(&begin.payload).unwrap().path)
                .exists()
        }));

        let commit_count = CAPACITY / 2;
        let commit_requests = streams
            .iter()
            .take(commit_count)
            .enumerate()
            .map(|(index, (resource_id, begin))| {
                stream_command(
                    begin,
                    2_000 + index as u64,
                    FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
                    FilesystemWriteStreamCommit {
                        resource_id: *resource_id,
                    }
                    .encode()
                    .unwrap(),
                )
            })
            .collect();
        let commits = execute_batch_in_runtime(&mut runtime, commit_requests);
        assert!(
            commits
                .values()
                .all(|result| matches!(result, BrokerResult::Success { .. }))
        );
        assert_eq!(runtime.pump_resource_events(2), commit_count);
        while runtime.pop_event().is_some() {}
        assert_eq!(
            runtime.lifecycle().resource_count(),
            CAPACITY - commit_count
        );
        assert_eq!(
            runtime.lifecycle().buffered_bytes(),
            (CAPACITY - commit_count) * MAX_FILE_WRITE_STREAM_CHUNK_BYTES
        );

        for (_, begin) in streams.iter().take(commit_count) {
            let stream = FilesystemWriteStream::decode(&begin.payload).unwrap();
            assert!(root.join(stream.path).exists());
        }
        for (_, begin) in streams.iter().skip(commit_count) {
            let stream = FilesystemWriteStream::decode(&begin.payload).unwrap();
            assert!(!root.join(stream.path).exists());
        }

        let effect = runtime.revoke(&CapabilityId::FilesystemWriteV1, 3);
        assert_eq!(effect.closed_resources.len(), CAPACITY - commit_count);
        assert_eq!(
            effect.released_buffered_bytes,
            (CAPACITY - commit_count) * MAX_FILE_WRITE_STREAM_CHUNK_BYTES
        );
        assert_eq!(runtime.lifecycle().resource_count(), 0);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 0);
        assert_eq!(broker_temporary_count(&root), 0);

        // A cancelled command cannot advance the stream, and repeated close
        // cycles reclaim both the broker slot and its private staging inode.
        for index in 0..64_u64 {
            let begin = request(
                &root,
                3_000 + index,
                FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
                FilesystemWriteStream {
                    mount: "documents".into(),
                    path: format!("cancelled-{index}"),
                    expected_bytes: 1,
                }
                .encode()
                .unwrap(),
                permissions.clone(),
            );
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            assert!(resource_id > last_resource_id);
            last_resource_id = resource_id;
            let chunk = stream_command(
                &begin,
                4_000 + index,
                FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
                FilesystemWriteStreamChunk {
                    resource_id,
                    offset: 0,
                    bytes: b"x".to_vec(),
                }
                .encode()
                .unwrap(),
            );
            Backend::authorize(&*backend, &chunk, &mut ActivationLedger::new(8), 0).unwrap();
            let cancelled =
                CancellationToken::new(Arc::new(AtomicBool::new(true)), Duration::from_secs(1))
                    .unwrap();
            assert_eq!(
                backend.execute(&chunk, &cancelled),
                BrokerResult::Error(BrokerErrorCode::Cancelled)
            );
            assert!(runtime.close_resource(resource_id));
            assert!(!root.join(format!("cancelled-{index}")).exists());
            assert_eq!(broker_temporary_count(&root), 0);
        }

        let begin = request(
            &root,
            5_000,
            FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
            FilesystemWriteStream {
                mount: "documents".into(),
                path: "teardown".into(),
                expected_bytes: 1,
            }
            .encode()
            .unwrap(),
            permissions,
        );
        runtime
            .open_resource(&begin, &mut ActivationLedger::new(8), 0)
            .unwrap();
        assert_eq!(broker_temporary_count(&root), 1);
        drop(runtime);
        assert_eq!(broker_temporary_count(&root), 0);
        assert!(!root.join("teardown").exists());
    }
}
