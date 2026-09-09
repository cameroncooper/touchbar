use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use touchbar_broker_schema::{
    FilesystemDirectoryEntries, FilesystemEntry, FilesystemEntryKind, FilesystemFileChunk,
    FilesystemListDirectory, FilesystemReadFile, FilesystemReadStream, FilesystemStreamEvent,
    FilesystemStreamOpened, MAX_FILE_STREAM_CHUNK_BYTES, SchemaError,
};
use touchbar_policy::{
    AppearanceProvideScope, CapabilityId, CapabilityScope, FileKind, FilesystemMountBinding,
    FilesystemMountRequest, FilesystemReadScope, StandardDirectoryBinding,
};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

use crate::{
    ActivationLedger, Backend, BackendRequest, CancellationToken, OpenedResource, ResourceBackend,
    ResourceEventSink, ResourceHandle, ResourceLimits,
};

pub const FILESYSTEM_READ_FILE_OPERATION: &str = "read-file";
pub const FILESYSTEM_LIST_DIRECTORY_OPERATION: &str = "list-directory";
pub const FILESYSTEM_READ_STREAM_OPERATION: &str = "read-file-stream";

const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_FLAGS: u64 =
    RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH;
const MAXIMUM_SCANNED_DIRECTORY_ENTRIES: usize = 4096;
const FILE_STREAM_RESERVED_BYTES: usize = 256 * 1024;
const FILE_STREAM_TIMEOUT: Duration = Duration::from_secs(30);

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[derive(Default)]
pub struct FilesystemReadBackend;

impl FilesystemReadBackend {
    pub fn new() -> Self {
        Self
    }
}

enum AuthorizedOperation {
    Read(FilesystemReadFile),
    List(FilesystemListDirectory),
}

impl Backend for FilesystemReadBackend {
    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_and_authorize(request).map(|_| ())
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let operation = decode_and_authorize(request)?;
            let payload = match operation {
                AuthorizedOperation::Read(read) => {
                    read_file(request, &read, cancellation)?.encode()
                }
                AuthorizedOperation::List(list) => {
                    list_directory(request, &list, cancellation)?.encode()
                }
            }
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
            Ok(BrokerResult::Success { payload })
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

struct FilesystemStreamHandle {
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ResourceHandle for FilesystemStreamHandle {
    fn close(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.worker.take();
    }
}

impl ResourceBackend for FilesystemReadBackend {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        (request.capability == CapabilityId::FilesystemReadV1
            && request.operation == FILESYSTEM_READ_STREAM_OPERATION
            && matches!(request.authorized_scope, CapabilityScope::FilesystemRead(_)))
        .then_some(ResourceLimits {
            reserved_buffered_bytes: FILE_STREAM_RESERVED_BYTES,
            maximum_events_per_second: u16::MAX,
        })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_and_authorize_stream(request).map(|_| ())
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        let read = decode_and_authorize_stream(request)?;
        let scope = filesystem_scope(request)?;
        let root = open_root(mount_root(request, &read.mount)?)?;
        let file = open_beneath(
            root.as_raw_fd(),
            &read.path,
            libc::O_RDONLY | libc::O_NONBLOCK,
        )?;
        let metadata = validate_regular_file(file.as_raw_fd(), scope.maximum_file_bytes)?;
        let total_size =
            u64::try_from(metadata.st_size).map_err(|_| BrokerErrorCode::BackendFailed)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new(Arc::clone(&cancelled), FILE_STREAM_TIMEOUT)?;
        let worker = thread::Builder::new()
            .name(format!("touchbar-file-stream-{resource_id}"))
            .spawn(
                move || match stream_file(file, read, total_size, &cancellation, &events) {
                    Ok((end_offset, total_bytes, eof)) => {
                        let payload = FilesystemStreamEvent::Complete {
                            end_offset,
                            total_bytes,
                            eof,
                        }
                        .encode();
                        match payload {
                            Ok(payload) => {
                                let _ = events.complete(payload);
                            }
                            Err(_) => events.finish(BrokerErrorCode::Internal),
                        }
                    }
                    Err(error) => events.finish(error),
                },
            )
            .map_err(|_| BrokerErrorCode::Unavailable)?;
        let response_payload = FilesystemStreamOpened { resource_id }
            .encode()
            .map_err(|_| BrokerErrorCode::Internal)?;
        Ok(OpenedResource {
            handle: Box::new(FilesystemStreamHandle {
                cancelled,
                worker: Some(worker),
            }),
            response_payload,
        })
    }
}

fn decode_and_authorize_stream(
    request: &BackendRequest,
) -> Result<FilesystemReadStream, BrokerErrorCode> {
    if request.capability != CapabilityId::FilesystemReadV1
        || request.operation != FILESYSTEM_READ_STREAM_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let CapabilityScope::FilesystemRead(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let read = FilesystemReadStream::decode(&request.payload).map_err(schema_error)?;
    validate_mount_and_path(
        &read.mount,
        &read.path,
        false,
        scope,
        &request.bindings.filesystem_mounts,
    )?;
    if !scope.kinds.contains(&FileKind::RegularFile) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    if read.maximum_bytes > scope.maximum_file_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(read)
}

fn stream_file(
    file: OwnedFd,
    read: FilesystemReadStream,
    total_size: u64,
    cancellation: &CancellationToken,
    events: &ResourceEventSink,
) -> Result<(u64, u64, bool), BrokerErrorCode> {
    events.emit_buffered(
        FilesystemStreamEvent::Metadata {
            offset: read.offset,
            total_size,
        }
        .encode()
        .map_err(schema_error)?,
    )?;
    let mut offset = read.offset;
    let mut total_bytes = 0_u64;
    let target_bytes = read.maximum_bytes.min(total_size.saturating_sub(offset));
    let mut reached_eof = offset >= total_size;
    while total_bytes < target_bytes {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let requested =
            usize::try_from((target_bytes - total_bytes).min(MAX_FILE_STREAM_CHUNK_BYTES as u64))
                .map_err(|_| BrokerErrorCode::QuotaExceeded)?;
        let mut bytes = vec![0_u8; requested];
        let file_offset =
            libc::off_t::try_from(offset).map_err(|_| BrokerErrorCode::InvalidRequest)?;
        // SAFETY: the descriptor and writable buffer remain valid for pread.
        let count = unsafe {
            libc::pread(
                file.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                file_offset,
            )
        };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os_error());
        }
        if count == 0 {
            reached_eof = true;
            break;
        }
        bytes.truncate(count as usize);
        events.emit_buffered(
            FilesystemStreamEvent::Chunk { offset, bytes }
                .encode()
                .map_err(schema_error)?,
        )?;
        offset = offset
            .checked_add(count as u64)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
        total_bytes = total_bytes
            .checked_add(count as u64)
            .ok_or(BrokerErrorCode::QuotaExceeded)?;
        reached_eof |= offset >= total_size;
    }
    Ok((offset, total_bytes, reached_eof))
}

fn decode_and_authorize(request: &BackendRequest) -> Result<AuthorizedOperation, BrokerErrorCode> {
    if request.capability != CapabilityId::FilesystemReadV1 {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let CapabilityScope::FilesystemRead(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    match request.operation.as_str() {
        FILESYSTEM_READ_FILE_OPERATION => {
            let read = FilesystemReadFile::decode(&request.payload).map_err(schema_error)?;
            validate_mount_and_path(
                &read.mount,
                &read.path,
                false,
                scope,
                &request.bindings.filesystem_mounts,
            )?;
            if !scope.kinds.contains(&FileKind::RegularFile) {
                return Err(BrokerErrorCode::OutOfScope);
            }
            Ok(AuthorizedOperation::Read(read))
        }
        FILESYSTEM_LIST_DIRECTORY_OPERATION => {
            let list = FilesystemListDirectory::decode(&request.payload).map_err(schema_error)?;
            validate_mount_and_path(
                &list.mount,
                &list.path,
                true,
                scope,
                &request.bindings.filesystem_mounts,
            )?;
            if !scope.enumerate || !scope.kinds.contains(&FileKind::Directory) {
                return Err(BrokerErrorCode::OutOfScope);
            }
            Ok(AuthorizedOperation::List(list))
        }
        _ => Err(BrokerErrorCode::InvalidRequest),
    }
}

fn validate_mount_and_path(
    mount: &str,
    path: &str,
    allow_empty: bool,
    scope: &FilesystemReadScope,
    mounts: &BTreeMap<String, FilesystemMountBinding>,
) -> Result<(), BrokerErrorCode> {
    validate_requested_mount_and_path(mount, path, allow_empty, &scope.mounts, mounts)
}

fn validate_requested_mount_and_path(
    mount: &str,
    path: &str,
    allow_empty: bool,
    requested_mounts: &std::collections::BTreeSet<FilesystemMountRequest>,
    mounts: &BTreeMap<String, FilesystemMountBinding>,
) -> Result<(), BrokerErrorCode> {
    if mount.is_empty()
        || !requested_mounts
            .iter()
            .any(|requested| requested.label == mount)
        || !mounts.contains_key(mount)
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    if path.is_empty() {
        return allow_empty
            .then_some(())
            .ok_or(BrokerErrorCode::InvalidRequest);
    }
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(())
}

fn read_file(
    request: &BackendRequest,
    read: &FilesystemReadFile,
    cancellation: &CancellationToken,
) -> Result<FilesystemFileChunk, BrokerErrorCode> {
    let scope = filesystem_scope(request)?;
    read_file_with_limit(request, read, cancellation, scope.maximum_file_bytes)
}

fn read_file_with_limit(
    request: &BackendRequest,
    read: &FilesystemReadFile,
    cancellation: &CancellationToken,
    maximum_file_bytes: u64,
) -> Result<FilesystemFileChunk, BrokerErrorCode> {
    let root = open_root(mount_root(request, &read.mount)?)?;
    let file = open_beneath(
        root.as_raw_fd(),
        &read.path,
        libc::O_RDONLY | libc::O_NONBLOCK,
    )?;
    let metadata = validate_regular_file(file.as_raw_fd(), maximum_file_bytes)?;
    let total_size = u64::try_from(metadata.st_size).map_err(|_| BrokerErrorCode::BackendFailed)?;
    if let Some(reason) = cancellation.reason() {
        return Err(reason);
    }
    let requested =
        usize::try_from(read.maximum_bytes).map_err(|_| BrokerErrorCode::QuotaExceeded)?;
    let mut bytes = vec![0; requested];
    let offset = libc::off_t::try_from(read.offset).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    // SAFETY: the descriptor and writable buffer are valid for the duration of pread.
    let read_count = unsafe {
        libc::pread(
            file.as_raw_fd(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            offset,
        )
    };
    if read_count < 0 {
        return Err(os_error());
    }
    bytes.truncate(read_count as usize);
    if let Some(reason) = cancellation.reason() {
        return Err(reason);
    }
    let end = read.offset.saturating_add(bytes.len() as u64);
    Ok(FilesystemFileChunk {
        offset: read.offset,
        total_size,
        eof: end >= total_size,
        bytes,
    })
}

pub(crate) fn authorize_appearance_read(
    request: &BackendRequest,
) -> Result<FilesystemReadFile, BrokerErrorCode> {
    if request.capability != CapabilityId::AppearanceProvideV1
        || request.operation != FILESYSTEM_READ_FILE_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let CapabilityScope::AppearanceProvide(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let read = FilesystemReadFile::decode(&request.payload).map_err(schema_error)?;
    validate_requested_mount_and_path(
        &read.mount,
        &read.path,
        false,
        &scope.mounts,
        &request.bindings.filesystem_mounts,
    )?;
    if read.maximum_bytes > scope.maximum_file_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(read)
}

pub(crate) fn execute_appearance_read(
    request: &BackendRequest,
    read: &FilesystemReadFile,
    cancellation: &CancellationToken,
    scope: &AppearanceProvideScope,
) -> Result<FilesystemFileChunk, BrokerErrorCode> {
    read_file_with_limit(request, read, cancellation, scope.maximum_file_bytes)
}

fn list_directory(
    request: &BackendRequest,
    list: &FilesystemListDirectory,
    cancellation: &CancellationToken,
) -> Result<FilesystemDirectoryEntries, BrokerErrorCode> {
    let scope = filesystem_scope(request)?;
    let root = open_root(mount_root(request, &list.mount)?)?;
    let root_metadata = descriptor_metadata(root.as_raw_fd())?;
    let relative = if list.path.is_empty() {
        "."
    } else {
        &list.path
    };
    let directory = open_beneath(
        root.as_raw_fd(),
        relative,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
    )?;
    let directory_device = descriptor_metadata(directory.as_raw_fd())?.st_dev;
    if directory_device != root_metadata.st_dev {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let raw = directory.into_raw_fd();
    // SAFETY: raw is a valid owned directory descriptor and ownership transfers to DIR.
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        // SAFETY: fdopendir failed and did not take ownership.
        unsafe { libc::close(raw) };
        return Err(os_error());
    }
    let directory_fd = unsafe { libc::dirfd(stream) };
    let mut entries = Vec::new();
    let mut scanned = 0usize;
    let limit = usize::from(list.maximum_entries);
    let result = loop {
        if let Some(reason) = cancellation.reason() {
            break Err(reason);
        }
        // SAFETY: stream remains live until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break Ok(false);
        }
        scanned += 1;
        if scanned > MAXIMUM_SCANNED_DIRECTORY_ENTRIES {
            break Ok(true);
        }
        // SAFETY: readdir returned a live dirent with a nul-terminated name.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let Ok(name_text) = name.to_str() else {
            continue;
        };
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: all pointers are valid and fstatat initializes metadata on success.
        if unsafe {
            libc::fstatat(
                directory_fd,
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            continue;
        }
        // SAFETY: fstatat succeeded.
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_dev != directory_device {
            continue;
        }
        let kind = match metadata.st_mode & libc::S_IFMT {
            libc::S_IFREG => FilesystemEntryKind::RegularFile,
            libc::S_IFDIR => FilesystemEntryKind::Directory,
            _ => continue,
        };
        let kind_allowed = match kind {
            FilesystemEntryKind::RegularFile => scope.kinds.contains(&FileKind::RegularFile),
            FilesystemEntryKind::Directory => scope.kinds.contains(&FileKind::Directory),
        };
        if !kind_allowed {
            continue;
        }
        if entries.len() == limit {
            break Ok(true);
        }
        entries.push(FilesystemEntry {
            name: name_text.to_owned(),
            kind,
            size: u64::try_from(metadata.st_size).unwrap_or(0),
        });
    };
    // SAFETY: stream is a live DIR and closed exactly once.
    unsafe { libc::closedir(stream) };
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    result.map(|truncated| FilesystemDirectoryEntries { entries, truncated })
}

fn filesystem_scope(request: &BackendRequest) -> Result<&FilesystemReadScope, BrokerErrorCode> {
    match &request.authorized_scope {
        CapabilityScope::FilesystemRead(scope) => Ok(scope),
        _ => Err(BrokerErrorCode::InvalidRequest),
    }
}

fn mount_root<'a>(
    request: &'a BackendRequest,
    mount: &str,
) -> Result<&'a FilesystemMountBinding, BrokerErrorCode> {
    request
        .bindings
        .filesystem_mounts
        .get(mount)
        .ok_or(BrokerErrorCode::OutOfScope)
}

fn open_root(binding: &FilesystemMountBinding) -> Result<OwnedFd, BrokerErrorCode> {
    open_bound_mount(binding)
}

pub(crate) fn open_bound_mount(
    binding: &FilesystemMountBinding,
) -> Result<OwnedFd, BrokerErrorCode> {
    let path = match binding {
        FilesystemMountBinding::Path { path } => path.clone(),
        FilesystemMountBinding::StandardDirectory {
            directory,
            relative,
        } => {
            let root = standard_directory_root(*directory)?;
            if *directory == StandardDirectoryBinding::XdgRuntime {
                validate_xdg_runtime_directory(&root)?;
            }
            root.join(relative)
        }
    };
    if !normalized_absolute_path(&path) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let path = c_string(path.as_os_str())?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        // A selected root may itself be a mount point, so NO_XDEV applies only
        // to plugin paths below it. Every root component must still be a real
        // directory rather than a symlink or procfs magic link.
        resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: all syscall arguments point to initialized storage of the declared size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor = i32::try_from(descriptor).map_err(|_| os_error())?;
    let descriptor = owned_descriptor(descriptor)?;
    let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(descriptor)
}

fn standard_directory_root(
    directory: StandardDirectoryBinding,
) -> Result<PathBuf, BrokerErrorCode> {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let path = match directory {
        StandardDirectoryBinding::Home => home(),
        StandardDirectoryBinding::XdgConfig => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|path| path.join(".config"))),
        StandardDirectoryBinding::XdgData => std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|path| path.join(".local/share"))),
        StandardDirectoryBinding::XdgState => std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|path| path.join(".local/state"))),
        StandardDirectoryBinding::XdgCache => std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|path| path.join(".cache"))),
        StandardDirectoryBinding::XdgRuntime => {
            std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        }
    }
    .ok_or(BrokerErrorCode::OutOfScope)?;
    normalized_absolute_path(&path)
        .then_some(path)
        .ok_or(BrokerErrorCode::OutOfScope)
}

fn validate_xdg_runtime_directory(path: &Path) -> Result<(), BrokerErrorCode> {
    let path = c_string(path.as_os_str())?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: all syscall arguments point to initialized storage of the declared size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor = owned_descriptor(i32::try_from(descriptor).map_err(|_| os_error())?)?;
    let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
    // SAFETY: geteuid has no preconditions and retains no pointers.
    let current_user = unsafe { libc::geteuid() };
    if metadata.st_uid != current_user || metadata.st_mode & 0o077 != 0 {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(())
}

fn normalized_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.components().enumerate().all(|(index, component)| {
            (index == 0 && matches!(component, Component::RootDir))
                || (index > 0 && matches!(component, Component::Normal(_)))
        })
}

fn open_beneath(root: RawFd, path: &str, flags: i32) -> Result<OwnedFd, BrokerErrorCode> {
    let path = CString::new(path).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: RESOLVE_FLAGS,
    };
    // SAFETY: all syscall arguments point to initialized storage of the declared size.
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

fn descriptor_metadata(descriptor: RawFd) -> Result<libc::stat, BrokerErrorCode> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to enough writable storage and descriptor is live.
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
    if (metadata.st_mode & libc::S_IFMT) != libc::S_IFREG || metadata.st_nlink != 1 {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let total_size = u64::try_from(metadata.st_size).map_err(|_| BrokerErrorCode::BackendFailed)?;
    if total_size > maximum_file_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(metadata)
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
        Some(libc::EXDEV) | Some(libc::ELOOP) | Some(libc::ENOENT) | Some(libc::ENOTDIR)
        | Some(libc::EACCES) | Some(libc::EPERM) => BrokerErrorCode::OutOfScope,
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
        os::unix::{ffi::OsStrExt, fs::symlink},
        path::Path,
        sync::{Arc, atomic::AtomicBool},
        thread,
        time::{Duration, Instant},
    };

    use semver::Version;
    use tempfile::tempdir;
    use touchbar_broker_schema::{
        FilesystemDirectoryEntries, FilesystemFileChunk, FilesystemListDirectory,
        FilesystemReadFile, FilesystemReadStream, FilesystemStreamEvent, FilesystemStreamOpened,
        MAX_FILE_STREAM_CHUNK_BYTES,
    };
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        CapabilityId, CapabilityScope, FileKind, FilesystemMountBinding, FilesystemMountRequest,
        FilesystemReadScope, PackageInstance, Provenance, RuntimeKind,
    };
    use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

    use crate::{
        ActivationLedger, AsyncBrokerRuntime, Backend, BackendRequest, CancellationToken,
        ConnectionIdentity, ExecutorLimits, HostEvent, HostEventQueue, LifecycleLimits,
        ResourceBackend, ResourceManager,
    };

    use super::{
        FILE_STREAM_RESERVED_BYTES, FILESYSTEM_READ_FILE_OPERATION,
        FILESYSTEM_READ_STREAM_OPERATION, FilesystemReadBackend,
    };

    fn request(
        root: &Path,
        operation: &str,
        payload: Vec<u8>,
        maximum_file_bytes: u64,
    ) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 1,
                package: PackageInstance {
                    source: GithubSource::new("alice", "filesystem-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::FilesystemReadV1,
            authorized_scope: CapabilityScope::FilesystemRead(FilesystemReadScope {
                mounts: BTreeSet::from([FilesystemMountRequest {
                    label: "gallery".into(),
                    suggested_location: None,
                }]),
                kinds: BTreeSet::from([FileKind::RegularFile, FileKind::Directory]),
                maximum_file_bytes,
                enumerate: true,
            }),
            bindings: touchbar_policy::GrantBindings {
                filesystem_mounts: BTreeMap::from([(
                    "gallery".into(),
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
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)).unwrap()
    }

    fn execute(request: &BackendRequest) -> BrokerResult {
        FilesystemReadBackend::new().execute(request, &token())
    }

    fn runtime_with_read_backend(
        backend: Arc<FilesystemReadBackend>,
        edge_capacity: usize,
    ) -> AsyncBrokerRuntime {
        let mut runtime = AsyncBrokerRuntime::new(
            ExecutorLimits::default(),
            LifecycleLimits::default(),
            64,
            edge_capacity,
        )
        .expect("create broker runtime");
        runtime.register_resource(CapabilityId::FilesystemReadV1, backend);
        runtime
    }

    fn drain_stream(
        runtime: &mut AsyncBrokerRuntime,
        resource_id: u64,
    ) -> Vec<FilesystemStreamEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut decoded = Vec::new();
        loop {
            runtime.pump_resource_events(1);
            while let Some(event) = runtime.pop_event() {
                match event {
                    HostEvent::ResourceEvent {
                        resource_id: actual,
                        result: BrokerResult::Success { payload },
                        ..
                    } => {
                        assert_eq!(actual, resource_id);
                        decoded.push(FilesystemStreamEvent::decode(&payload).unwrap());
                    }
                    other => panic!("unexpected stream event: {other:?}"),
                }
            }
            if runtime.lifecycle().resource_count() == 0 {
                return decoded;
            }
            assert!(Instant::now() < deadline, "file stream did not terminate");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn decoded_stream_bytes(events: &[FilesystemStreamEvent]) -> Vec<u8> {
        assert!(matches!(
            events.first(),
            Some(FilesystemStreamEvent::Metadata { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(FilesystemStreamEvent::Complete { .. })
        ));
        let mut expected_offset = match events.first().unwrap() {
            FilesystemStreamEvent::Metadata { offset, .. } => *offset,
            _ => unreachable!(),
        };
        let mut bytes = Vec::new();
        for event in events {
            if let FilesystemStreamEvent::Chunk {
                offset,
                bytes: chunk,
            } = event
            {
                assert_eq!(*offset, expected_offset);
                assert!(chunk.len() <= MAX_FILE_STREAM_CHUNK_BYTES);
                expected_offset += chunk.len() as u64;
                bytes.extend_from_slice(chunk);
            }
        }
        bytes
    }

    fn process_thread_count() -> usize {
        fs::read_dir("/proc/self/task").unwrap().count()
    }

    #[test]
    fn reads_chunks_and_lists_only_safe_entry_kinds() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("image.png"), b"abcdef").unwrap();
        fs::create_dir(root.path().join("album")).unwrap();
        symlink("/etc/passwd", root.path().join("escape")).unwrap();
        let fifo = root.path().join("pipe");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_name is a valid nul-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);

        let read = FilesystemReadFile {
            mount: "gallery".into(),
            path: "image.png".into(),
            offset: 2,
            maximum_bytes: 3,
        };
        let BrokerResult::Success { payload } = execute(&request(
            root.path(),
            "read-file",
            read.encode().unwrap(),
            64,
        )) else {
            panic!("read should succeed");
        };
        assert_eq!(
            FilesystemFileChunk::decode(&payload).unwrap(),
            FilesystemFileChunk {
                offset: 2,
                total_size: 6,
                eof: false,
                bytes: b"cde".to_vec(),
            }
        );

        let list = FilesystemListDirectory {
            mount: "gallery".into(),
            path: String::new(),
            maximum_entries: 10,
        };
        let BrokerResult::Success { payload } = execute(&request(
            root.path(),
            "list-directory",
            list.encode().unwrap(),
            64,
        )) else {
            panic!("list should succeed");
        };
        let names = FilesystemDirectoryEntries::decode(&payload)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["album", "image.png"]);
    }

    #[test]
    fn traversal_symlinks_root_symlinks_and_oversized_files_fail_closed() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("large"), [0; 8]).unwrap();
        symlink("/etc/passwd", root.join("escape")).unwrap();
        fs::write(parent.path().join("outside-secret"), b"secret").unwrap();
        fs::hard_link(parent.path().join("outside-secret"), root.join("hardlink")).unwrap();

        for path in ["../outside", "/etc/passwd", "escape", "hardlink"] {
            let read = FilesystemReadFile {
                mount: "gallery".into(),
                path: path.into(),
                offset: 0,
                maximum_bytes: 8,
            };
            assert_eq!(
                execute(&request(&root, "read-file", read.encode().unwrap(), 64)),
                BrokerResult::Error(BrokerErrorCode::OutOfScope),
                "{path}"
            );
        }

        let large = FilesystemReadFile {
            mount: "gallery".into(),
            path: "large".into(),
            offset: 0,
            maximum_bytes: 8,
        };
        assert_eq!(
            execute(&request(&root, "read-file", large.encode().unwrap(), 4)),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );

        let linked_root = parent.path().join("linked-root");
        symlink(&root, &linked_root).unwrap();
        let mut linked_request = request(&root, "read-file", large.encode().unwrap(), 64);
        *linked_request
            .bindings
            .filesystem_mounts
            .get_mut("gallery")
            .unwrap() = FilesystemMountBinding::Path { path: linked_root };
        assert_eq!(
            execute(&linked_request),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
    }

    #[test]
    fn mount_binding_rejects_intermediate_symlinks_and_nested_mounts_but_follows_its_path() {
        let parent = tempdir().unwrap();
        let real_parent = parent.path().join("real");
        let root = real_parent.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("value"), b"authorized").unwrap();

        let read = FilesystemReadFile {
            mount: "gallery".into(),
            path: "value".into(),
            offset: 0,
            maximum_bytes: 64,
        };
        let mut through_intermediate_symlink = request(
            &root,
            FILESYSTEM_READ_FILE_OPERATION,
            read.encode().unwrap(),
            64,
        );
        let alias = parent.path().join("alias");
        symlink(&real_parent, &alias).unwrap();
        *through_intermediate_symlink
            .bindings
            .filesystem_mounts
            .get_mut("gallery")
            .unwrap() = FilesystemMountBinding::Path {
            path: alias.join("root"),
        };
        assert_eq!(
            execute(&through_intermediate_symlink),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );

        let replaced = request(
            &root,
            FILESYSTEM_READ_FILE_OPERATION,
            read.encode().unwrap(),
            64,
        );
        fs::rename(&root, real_parent.join("old-root")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("value"), b"attacker replacement").unwrap();
        assert!(matches!(execute(&replaced), BrokerResult::Success { .. }));

        let proc_read = FilesystemReadFile {
            mount: "gallery".into(),
            path: "proc/version".into(),
            offset: 0,
            maximum_bytes: 64,
        };
        assert_eq!(
            execute(&request(
                Path::new("/"),
                FILESYSTEM_READ_FILE_OPERATION,
                proc_read.encode().unwrap(),
                64,
            )),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
    }

    #[test]
    fn stream_uses_one_pinned_descriptor_and_lossless_bounded_chunks() {
        let root = tempdir().unwrap();
        let original = vec![0x31; MAX_FILE_STREAM_CHUNK_BYTES + 29];
        fs::write(root.path().join("media.bin"), &original).unwrap();
        let read = FilesystemReadStream {
            mount: "gallery".into(),
            path: "media.bin".into(),
            offset: 0,
            maximum_bytes: original.len() as u64,
        };
        let request = request(
            root.path(),
            FILESYSTEM_READ_STREAM_OPERATION,
            read.encode().unwrap(),
            64 * 1024,
        );
        let backend = Arc::new(FilesystemReadBackend::new());
        let mut manager = ResourceManager::new(1).unwrap();
        manager.register(CapabilityId::FilesystemReadV1, backend.clone());
        let opened = manager
            .open(
                7,
                &request,
                backend.limits(&request).unwrap(),
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();
        assert_eq!(
            FilesystemStreamOpened::decode(&opened).unwrap().resource_id,
            7
        );

        // Replacement happens after open. The worker must keep reading the
        // already-authorized inode, not resolve this path a second time.
        fs::rename(root.path().join("media.bin"), root.path().join("old.bin")).unwrap();
        fs::write(root.path().join("media.bin"), b"attacker replacement").unwrap();

        let mut queue = HostEventQueue::new(4, 16);
        let mut decoded = Vec::new();
        for _ in 0..10_000 {
            let finished = manager.pump(&mut queue, 4);
            while let Some(event) = queue.pop() {
                match event {
                    HostEvent::ResourceEvent {
                        resource_id: 7,
                        result: BrokerResult::Success { payload },
                        ..
                    } => decoded.push(FilesystemStreamEvent::decode(&payload).unwrap()),
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            if !finished.is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(manager.resource_count(), 0);
        assert!(matches!(
            decoded.first(),
            Some(FilesystemStreamEvent::Metadata { total_size, .. })
                if *total_size == original.len() as u64
        ));
        let bytes = decoded
            .iter()
            .filter_map(|event| match event {
                FilesystemStreamEvent::Chunk { bytes, .. } => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(bytes, original);
        assert_eq!(
            decoded.last(),
            Some(&FilesystemStreamEvent::Complete {
                end_offset: original.len() as u64,
                total_bytes: original.len() as u64,
                eof: true,
            })
        );
    }

    #[test]
    fn stream_request_cannot_exceed_granted_file_budget() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("small"), b"ok").unwrap();
        let read = FilesystemReadStream {
            mount: "gallery".into(),
            path: "small".into(),
            offset: 0,
            maximum_bytes: 65,
        };
        let request = request(
            root.path(),
            FILESYSTEM_READ_STREAM_OPERATION,
            read.encode().unwrap(),
            64,
        );
        let backend = Arc::new(FilesystemReadBackend::new());
        let mut manager = ResourceManager::new(2).unwrap();
        manager.register(CapabilityId::FilesystemReadV1, backend.clone());
        assert_eq!(
            manager.open(
                8,
                &request,
                backend.limits(&request).unwrap(),
                &mut ActivationLedger::new(1),
                0,
            ),
            Err(BrokerErrorCode::QuotaExceeded)
        );
    }

    #[test]
    #[ignore = "release-gate external-mutator campaign"]
    fn security_campaign_external_mutators_cannot_redirect_streams() {
        const ROUNDS: u8 = 32;

        let sandbox = tempdir().unwrap();
        let root = sandbox.path().join("root");
        let outside = sandbox.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(root.join("inside")).unwrap();
        fs::write(outside.join("value"), b"outside sentinel").unwrap();
        let backend = Arc::new(FilesystemReadBackend::new());
        let mut runtime = runtime_with_read_backend(backend, 16);

        for round in 0..ROUNDS {
            let original = vec![round; MAX_FILE_STREAM_CHUNK_BYTES * 2 + 73];
            let value = root.join("inside/value");

            // Replacing the final pathname after open cannot make the worker
            // follow the new entry: it reads only the already-pinned inode.
            fs::write(&value, &original).unwrap();
            let read = FilesystemReadStream {
                mount: "gallery".into(),
                path: "inside/value".into(),
                offset: 0,
                maximum_bytes: original.len() as u64,
            };
            let begin = request(
                &root,
                FILESYSTEM_READ_STREAM_OPERATION,
                read.encode().unwrap(),
                64 * 1024,
            );
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            let displaced = root.join("inside/displaced");
            fs::rename(&value, &displaced).unwrap();
            fs::write(&value, b"replacement entry").unwrap();
            assert_eq!(
                decoded_stream_bytes(&drain_stream(&mut runtime, resource_id)),
                original
            );
            assert_eq!(fs::read(&value).unwrap(), b"replacement entry");
            fs::remove_file(&value).unwrap();
            fs::remove_file(&displaced).unwrap();

            // Replacing an intermediate component cannot redirect the pinned
            // file descriptor into an outside directory.
            fs::write(&value, &original).unwrap();
            let begin = request(
                &root,
                FILESYSTEM_READ_STREAM_OPERATION,
                read.encode().unwrap(),
                64 * 1024,
            );
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            let parked_parent = sandbox.path().join(format!("parked-parent-{round}"));
            fs::rename(root.join("inside"), &parked_parent).unwrap();
            symlink(&outside, root.join("inside")).unwrap();
            assert_eq!(
                decoded_stream_bytes(&drain_stream(&mut runtime, resource_id)),
                original
            );
            assert_eq!(
                fs::read(outside.join("value")).unwrap(),
                b"outside sentinel"
            );
            fs::remove_file(root.join("inside")).unwrap();
            fs::remove_file(parked_parent.join("value")).unwrap();
            fs::rename(&parked_parent, root.join("inside")).unwrap();

            // Replacing the grant-root pathname also leaves this already-open
            // stream attached to the descriptor it acquired at open time.
            // A later operation will resolve the logical grant path again.
            fs::write(&value, &original).unwrap();
            let begin = request(
                &root,
                FILESYSTEM_READ_STREAM_OPERATION,
                read.encode().unwrap(),
                64 * 1024,
            );
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            let parked_root = sandbox.path().join(format!("parked-root-{round}"));
            fs::rename(&root, &parked_root).unwrap();
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("inside")).unwrap();
            fs::write(root.join("inside/value"), b"replacement root").unwrap();
            assert_eq!(
                decoded_stream_bytes(&drain_stream(&mut runtime, resource_id)),
                original
            );
            assert_eq!(
                fs::read(root.join("inside/value")).unwrap(),
                b"replacement root"
            );
            fs::remove_file(root.join("inside/value")).unwrap();
            fs::remove_dir(root.join("inside")).unwrap();
            fs::remove_dir(&root).unwrap();
            fs::remove_file(parked_root.join("inside/value")).unwrap();
            fs::rename(&parked_root, &root).unwrap();
        }

        assert_eq!(runtime.lifecycle().resource_count(), 0);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 0);
        assert_eq!(
            fs::read(outside.join("value")).unwrap(),
            b"outside sentinel"
        );
    }

    #[test]
    #[ignore = "release-gate streamed-read resource-pressure campaign"]
    fn security_campaign_stream_pressure_releases_backpressure_and_threads() {
        const CAPACITY: usize = 16;

        let sandbox = tempdir().unwrap();
        let root = sandbox.path();
        let large = vec![0x5a; 512 * 1024];
        fs::write(root.join("large"), &large).unwrap();
        fs::write(root.join("small"), b"reusable").unwrap();
        let baseline_threads = process_thread_count();
        let backend = Arc::new(FilesystemReadBackend::new());
        let mut runtime = runtime_with_read_backend(backend, 4);
        let read = FilesystemReadStream {
            mount: "gallery".into(),
            path: "large".into(),
            offset: 0,
            maximum_bytes: large.len() as u64,
        };
        let begin = request(
            root,
            FILESYSTEM_READ_STREAM_OPERATION,
            read.encode().unwrap(),
            1024 * 1024,
        );

        let mut resource_ids = Vec::new();
        for _ in 0..CAPACITY {
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            resource_ids.push(resource_id);
        }
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY);
        assert_eq!(
            runtime.lifecycle().buffered_bytes(),
            CAPACITY * FILE_STREAM_RESERVED_BYTES
        );
        assert_eq!(
            runtime.open_resource(&begin, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY);
        assert_eq!(
            runtime.lifecycle().buffered_bytes(),
            CAPACITY * FILE_STREAM_RESERVED_BYTES
        );

        let effect = runtime.revoke(&CapabilityId::FilesystemReadV1, 2);
        assert_eq!(effect.closed_resources, resource_ids);
        assert_eq!(
            effect.released_buffered_bytes,
            CAPACITY * FILE_STREAM_RESERVED_BYTES
        );
        assert_eq!(runtime.lifecycle().resource_count(), 0);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 0);
        runtime.pump_resource_events(2);
        while runtime.pop_event().is_some() {}

        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while process_thread_count() > baseline_threads + ExecutorLimits::default().workers {
            assert!(
                Instant::now() < wait_deadline,
                "revoked file-stream workers did not exit"
            );
            thread::sleep(Duration::from_millis(1));
        }

        let mut last_resource_id = *resource_ids.last().unwrap();
        for _ in 0..64 {
            let (resource_id, _) = runtime
                .open_resource(&begin, &mut ActivationLedger::new(8), 0)
                .unwrap();
            assert!(resource_id > last_resource_id);
            last_resource_id = resource_id;
            assert!(runtime.close_resource(resource_id));
            assert_eq!(runtime.lifecycle().resource_count(), 0);
            assert_eq!(runtime.lifecycle().buffered_bytes(), 0);
        }
        runtime.pump_resource_events(3);
        while runtime.pop_event().is_some() {}

        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while process_thread_count() > baseline_threads + ExecutorLimits::default().workers {
            assert!(
                Instant::now() < wait_deadline,
                "closed file-stream workers did not exit"
            );
            thread::sleep(Duration::from_millis(1));
        }

        let read = FilesystemReadStream {
            mount: "gallery".into(),
            path: "small".into(),
            offset: 0,
            maximum_bytes: 8,
        };
        let begin = request(
            root,
            FILESYSTEM_READ_STREAM_OPERATION,
            read.encode().unwrap(),
            1024 * 1024,
        );
        let (resource_id, _) = runtime
            .open_resource(&begin, &mut ActivationLedger::new(8), 0)
            .unwrap();
        assert!(resource_id > last_resource_id);
        assert_eq!(
            decoded_stream_bytes(&drain_stream(&mut runtime, resource_id)),
            b"reusable"
        );
        assert_eq!(runtime.lifecycle().resource_count(), 0);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 0);

        drop(runtime);
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while process_thread_count() > baseline_threads {
            assert!(
                Instant::now() < wait_deadline,
                "broker runtime left file-stream workers behind"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
}
