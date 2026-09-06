use std::{
    env,
    ffi::CString,
    io::{Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use touchbar_broker_schema::{
    MAX_SECRET_BYTES, MAX_SECRET_CONTENT_TYPE_BYTES, SchemaError, SecretReadRequest, SecretValue,
};
use touchbar_policy::{CapabilityId, CapabilityScope, SecretBinding};
use touchbar_protocol::broker_ipc::{ActivationOrigin, BrokerErrorCode, BrokerResult};
use zbus::{
    blocking::{Connection, Proxy, connection::Builder as ConnectionBuilder, fdo::DBusProxy},
    message::{Message, Type},
    names::{BusName, WellKnownName},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};
use zeroize::Zeroizing;

use crate::{ActivationExpectation, ActivationLedger, Backend, BackendRequest, CancellationToken};

pub const SECRET_READ_OPERATION: &str = "read";
const SECRET_SERVICE_DESTINATION: &str = "org.freedesktop.secrets";
const SECRET_SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SECRET_SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const SECRET_ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const SECRET_SESSION_INTERFACE: &str = "org.freedesktop.Secret.Session";
const SECRET_METHOD_TIMEOUT: Duration = Duration::from_secs(2);
const SECRET_REPLY_MAX_BYTES: usize = 64 * 1024;
const SECRET_METADATA_REPLY_MAX_BYTES: usize = 8 * 1024;
const SECRET_HELPER_REQUEST_MAGIC: &[u8; 5] = b"TBSH\x01";
const SECRET_HELPER_ADDRESS_SPACE_BYTES: libc::rlim_t = 128 * 1024 * 1024;
const SECRET_HELPER_POLL: Duration = Duration::from_millis(5);
const SECRET_HELPER_MAX_REQUEST_BYTES: usize = 4096;
const SECRET_HELPER_MAX_RESPONSE_BYTES: usize =
    1 + 1 + 2 + MAX_SECRET_CONTENT_TYPE_BYTES + 4 + MAX_SECRET_BYTES;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
const FS_RESOLVE_UNIX: u64 = 1 << 16;
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;

#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_00b7;
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_003e;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

pub struct SecretMaterial {
    bytes: Zeroizing<Vec<u8>>,
    content_type: String,
}

impl SecretMaterial {
    pub fn new(bytes: Vec<u8>, content_type: impl Into<String>) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
            content_type: content_type.into(),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn content_type(&self) -> &str {
        &self.content_type
    }
}

/// The only boundary that can retrieve secret bytes. Tests use a fake; the
/// production implementation performs only OpenSession, Locked, GetSecret,
/// and Close against one grant-owned item object path. It never searches.
pub trait SecretTransport: Send + Sync + 'static {
    fn read(
        &self,
        binding: &SecretBinding,
        cancellation: &CancellationToken,
    ) -> Result<SecretMaterial, BrokerErrorCode>;
}

pub struct SecretReadBackend<T> {
    transport: T,
}

impl<T> SecretReadBackend<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: SecretTransport> Backend for SecretReadBackend<T> {
    fn authorize(
        &self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        authorize_secret_read_request(request, activations, now_monotonic_micros)
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let (_, binding) = decode_and_match(request)?;
            let material = self.transport.read(binding, cancellation)?;
            validate_secret_value(material.bytes.len(), &material.content_type)?;
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let payload = SecretValue {
                bytes: material.bytes.as_slice().to_vec(),
                content_type: material.content_type,
            }
            .encode()
            .map_err(schema_error)?;
            Ok(BrokerResult::Success { payload })
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

pub fn authorize_secret_read_request(
    request: &BackendRequest,
    activations: &mut ActivationLedger,
    now_monotonic_micros: u64,
) -> Result<(), BrokerErrorCode> {
    decode_and_match(request)?;
    let activation = request
        .activation
        .as_ref()
        .ok_or(BrokerErrorCode::ActivationRequired)?;
    let origin = activations.consume(
        Some(activation),
        ActivationExpectation {
            surface_instance: activation.surface_instance,
            item_id: &activation.item_id,
            widget_id: activation.widget_id,
            now_monotonic_micros,
        },
    )?;
    if origin != ActivationOrigin::Physical {
        return Err(BrokerErrorCode::ActivationRequired);
    }
    Ok(())
}

pub fn validate_secret_value(bytes: usize, content_type: &str) -> Result<(), BrokerErrorCode> {
    if bytes > MAX_SECRET_BYTES || !valid_content_type(content_type) {
        Err(BrokerErrorCode::QuotaExceeded)
    } else {
        Ok(())
    }
}

fn decode_and_match(
    request: &BackendRequest,
) -> Result<(SecretReadRequest, &SecretBinding), BrokerErrorCode> {
    if request.capability != CapabilityId::SecretReadV1
        || request.operation != SECRET_READ_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let read = SecretReadRequest::decode(&request.payload).map_err(schema_error)?;
    if !valid_kebab_id(&read.logical_name) {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let CapabilityScope::SecretRead(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    if !scope.logical_names.contains(&read.logical_name) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let binding = request
        .bindings
        .secrets
        .get(&read.logical_name)
        .ok_or(BrokerErrorCode::OutOfScope)?;
    Ok((read, binding))
}

fn valid_kebab_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_content_type(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SECRET_CONTENT_TYPE_BYTES
        && value.contains('/')
        && value
            .bytes()
            .all(|byte| matches!(byte, 0x20..=0x7e) && byte != b'\\')
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

#[derive(Default)]
pub struct ZbusSecretService {
    helper_path: Option<PathBuf>,
    session_address: Option<String>,
}

impl ZbusSecretService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a transport with an explicit trusted helper and bus address.
    ///
    /// Production resolves the helper beside `touchbar-plugin-supervisor`.
    /// Embedders and integration tests can provide their installed equivalent.
    pub fn with_helper(helper_path: impl Into<PathBuf>, session_address: Option<String>) -> Self {
        Self {
            helper_path: Some(helper_path.into()),
            session_address,
        }
    }

    fn helper_path(&self) -> Result<PathBuf, BrokerErrorCode> {
        if let Some(path) = &self.helper_path {
            return Ok(path.clone());
        }
        let executable = env::current_exe().map_err(|_| BrokerErrorCode::Unavailable)?;
        let directory = executable.parent().ok_or(BrokerErrorCode::Unavailable)?;
        Ok(directory.join("touchbar-secret-helper"))
    }
}

impl SecretTransport for ZbusSecretService {
    fn read(
        &self,
        binding: &SecretBinding,
        cancellation: &CancellationToken,
    ) -> Result<SecretMaterial, BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let SecretBinding::SecretServiceItem { object_path } = binding;
        run_secret_helper(
            &self.helper_path()?,
            self.session_address.as_deref(),
            object_path,
            cancellation,
        )
    }
}

fn run_secret_helper(
    helper_path: &Path,
    session_address: Option<&str>,
    object_path: &str,
    cancellation: &CancellationToken,
) -> Result<SecretMaterial, BrokerErrorCode> {
    let request = encode_helper_request(object_path)?;
    let parent_pid = std::process::id();
    let mut command = Command::new(helper_path);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let bus_address = session_bus_address(session_address)?;
    command.env("DBUS_SESSION_BUS_ADDRESS", bus_address);
    // SAFETY: only async-signal-safe scalar syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || harden_secret_helper(parent_pid));
    }
    let mut child = command.spawn().map_err(|_| BrokerErrorCode::Unavailable)?;
    let write_result = child
        .stdin
        .take()
        .ok_or(BrokerErrorCode::Internal)
        .and_then(|mut input| {
            input
                .write_all(&request)
                .map_err(|_| BrokerErrorCode::BackendFailed)
        });
    if let Err(error) = write_result {
        terminate_helper(&mut child);
        return Err(error);
    }
    let Some(mut output) = child.stdout.take() else {
        terminate_helper(&mut child);
        return Err(BrokerErrorCode::Internal);
    };
    if let Err(error) = set_nonblocking(output.as_raw_fd()) {
        terminate_helper(&mut child);
        return Err(error);
    }
    let mut response = Zeroizing::new(Vec::with_capacity(4096));
    let mut buffer = [0_u8; 4096];
    loop {
        match output.read(&mut buffer) {
            Ok(0) => {}
            Ok(read) => {
                if response.len().saturating_add(read) > SECRET_HELPER_MAX_RESPONSE_BYTES {
                    terminate_helper(&mut child);
                    return Err(BrokerErrorCode::QuotaExceeded);
                }
                response.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                terminate_helper(&mut child);
                return Err(BrokerErrorCode::BackendFailed);
            }
        }
        if let Some(reason) = cancellation.reason() {
            terminate_helper(&mut child);
            return Err(reason);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                set_blocking(output.as_raw_fd())?;
                let remaining = SECRET_HELPER_MAX_RESPONSE_BYTES
                    .saturating_add(1)
                    .saturating_sub(response.len());
                let mut tail = Zeroizing::new(Vec::new());
                output
                    .take(remaining as u64)
                    .read_to_end(&mut tail)
                    .map_err(|_| BrokerErrorCode::BackendFailed)?;
                if response.len().saturating_add(tail.len()) > SECRET_HELPER_MAX_RESPONSE_BYTES {
                    return Err(BrokerErrorCode::QuotaExceeded);
                }
                response.extend_from_slice(&tail);
                if !status.success() {
                    return Err(BrokerErrorCode::BackendFailed);
                }
                return decode_helper_response(&response);
            }
            Ok(None) => thread::sleep(SECRET_HELPER_POLL),
            Err(_) => {
                terminate_helper(&mut child);
                return Err(BrokerErrorCode::BackendFailed);
            }
        }
    }
}

fn session_bus_address(explicit: Option<&str>) -> Result<String, BrokerErrorCode> {
    if let Some(address) = explicit {
        return Ok(address.to_owned());
    }
    if let Ok(address) = env::var("DBUS_SESSION_BUS_ADDRESS") {
        return Ok(address);
    }
    // SAFETY: geteuid takes no arguments.
    let uid = unsafe { libc::geteuid() };
    Ok(format!("unix:path=/run/user/{uid}/bus"))
}

fn harden_secret_helper(parent_pid: u32) -> std::io::Result<()> {
    let address_space = libc::rlimit {
        rlim_cur: SECRET_HELPER_ADDRESS_SPACE_BYTES,
        rlim_max: SECRET_HELPER_ADDRESS_SPACE_BYTES,
    };
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let descriptors = libc::rlimit {
        rlim_cur: 32,
        rlim_max: 32,
    };
    // SAFETY: all arguments are scalar values or readable rlimit pairs.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &address_space) } != 0
        || unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) } != 0
        || unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &descriptors) } != 0
        || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: getppid takes no arguments.
    if unsafe { libc::getppid() } != parent_pid as libc::pid_t {
        return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
    }
    Ok(())
}

fn terminate_helper(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn set_nonblocking(descriptor: libc::c_int) -> Result<(), BrokerErrorCode> {
    // SAFETY: descriptor is the owned child stdout pipe and F_GETFL has no third argument.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(BrokerErrorCode::Internal);
    }
    // SAFETY: F_SETFL updates flags on the same valid descriptor.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        return Err(BrokerErrorCode::Internal);
    }
    Ok(())
}

fn set_blocking(descriptor: libc::c_int) -> Result<(), BrokerErrorCode> {
    // SAFETY: descriptor is the owned child stdout pipe and F_GETFL has no third argument.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(BrokerErrorCode::Internal);
    }
    // SAFETY: F_SETFL updates flags on the same valid descriptor.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags & !libc::O_NONBLOCK) } != 0 {
        return Err(BrokerErrorCode::Internal);
    }
    Ok(())
}

fn encode_helper_request(object_path: &str) -> Result<Vec<u8>, BrokerErrorCode> {
    if object_path.is_empty() || object_path.len() > SECRET_HELPER_MAX_REQUEST_BYTES - 7 {
        return Err(BrokerErrorCode::OutOfScope);
    }
    zbus::zvariant::ObjectPath::try_from(object_path).map_err(|_| BrokerErrorCode::OutOfScope)?;
    let length = u16::try_from(object_path.len()).map_err(|_| BrokerErrorCode::OutOfScope)?;
    let mut request = Vec::with_capacity(7 + object_path.len());
    request.extend_from_slice(SECRET_HELPER_REQUEST_MAGIC);
    request.extend_from_slice(&length.to_be_bytes());
    request.extend_from_slice(object_path.as_bytes());
    Ok(request)
}

fn decode_helper_response(response: &[u8]) -> Result<SecretMaterial, BrokerErrorCode> {
    let (&status, response) = response
        .split_first()
        .ok_or(BrokerErrorCode::BackendFailed)?;
    if status == 1 {
        if response.len() != 1 {
            return Err(BrokerErrorCode::BackendFailed);
        }
        return Err(decode_helper_error(response[0]));
    }
    if status != 0 || response.len() < 6 {
        return Err(BrokerErrorCode::BackendFailed);
    }
    let content_type_length = usize::from(u16::from_be_bytes([response[0], response[1]]));
    if content_type_length > MAX_SECRET_CONTENT_TYPE_BYTES
        || response.len() < 6 + content_type_length
    {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    let content_type = std::str::from_utf8(&response[2..2 + content_type_length])
        .map_err(|_| BrokerErrorCode::BackendFailed)?
        .to_owned();
    let value_offset = 2 + content_type_length;
    let value_length = usize::try_from(u32::from_be_bytes(
        response[value_offset..value_offset + 4]
            .try_into()
            .map_err(|_| BrokerErrorCode::BackendFailed)?,
    ))
    .map_err(|_| BrokerErrorCode::QuotaExceeded)?;
    if value_length > MAX_SECRET_BYTES || response.len() != value_offset + 4 + value_length {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    validate_secret_value(value_length, &content_type)?;
    Ok(SecretMaterial::new(
        response[value_offset + 4..].to_vec(),
        content_type,
    ))
}

fn decode_helper_error(code: u8) -> BrokerErrorCode {
    match code {
        1 => BrokerErrorCode::Unavailable,
        2 => BrokerErrorCode::Denied,
        3 => BrokerErrorCode::OutOfScope,
        4 => BrokerErrorCode::QuotaExceeded,
        5 => BrokerErrorCode::Timeout,
        _ => BrokerErrorCode::BackendFailed,
    }
}

fn encode_helper_error(error: BrokerErrorCode) -> [u8; 2] {
    let code = match error {
        BrokerErrorCode::Unavailable => 1,
        BrokerErrorCode::Denied => 2,
        BrokerErrorCode::OutOfScope => 3,
        BrokerErrorCode::QuotaExceeded => 4,
        BrokerErrorCode::Timeout => 5,
        _ => 255,
    };
    [1, code]
}

/// Runs the internal bounded Secret Service transport helper on stdin/stdout.
/// This entry point is used only by the sibling `touchbar-secret-helper` binary.
pub fn run_secret_transport_helper() -> Result<(), BrokerErrorCode> {
    let result = (|| {
        // Linux resets dumpability during exec, so restore the nondumpable
        // state in the helper before any desktop-service bytes are received.
        // SAFETY: prctl takes scalar arguments for this operation.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err(BrokerErrorCode::Unavailable);
        }
        let mut request = Vec::new();
        std::io::stdin()
            .take((SECRET_HELPER_MAX_REQUEST_BYTES + 1) as u64)
            .read_to_end(&mut request)
            .map_err(|_| BrokerErrorCode::InvalidRequest)?;
        if request.len() < 7 || request.len() > SECRET_HELPER_MAX_REQUEST_BYTES {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        if &request[..5] != SECRET_HELPER_REQUEST_MAGIC {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let length = usize::from(u16::from_be_bytes([request[5], request[6]]));
        if request.len() != 7 + length {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        let object_path =
            std::str::from_utf8(&request[7..]).map_err(|_| BrokerErrorCode::InvalidRequest)?;
        let bus_address =
            env::var("DBUS_SESSION_BUS_ADDRESS").map_err(|_| BrokerErrorCode::Unavailable)?;
        apply_secret_helper_landlock(&bus_address)?;
        apply_secret_helper_seccomp()?;
        read_secret_over_dbus(object_path, Some(&bus_address))
    })();
    let mut stdout = std::io::stdout().lock();
    match result {
        Ok(material) => {
            validate_secret_value(material.bytes.len(), &material.content_type)?;
            let content_type_length = u16::try_from(material.content_type.len())
                .map_err(|_| BrokerErrorCode::QuotaExceeded)?;
            let value_length =
                u32::try_from(material.bytes.len()).map_err(|_| BrokerErrorCode::QuotaExceeded)?;
            stdout
                .write_all(&[0])
                .and_then(|_| stdout.write_all(&content_type_length.to_be_bytes()))
                .and_then(|_| stdout.write_all(material.content_type.as_bytes()))
                .and_then(|_| stdout.write_all(&value_length.to_be_bytes()))
                .and_then(|_| stdout.write_all(&material.bytes))
                .map_err(|_| BrokerErrorCode::BackendFailed)?;
        }
        Err(error) => stdout
            .write_all(&encode_helper_error(error))
            .map_err(|_| BrokerErrorCode::BackendFailed)?,
    }
    stdout.flush().map_err(|_| BrokerErrorCode::BackendFailed)
}

fn apply_secret_helper_landlock(bus_address: &str) -> Result<(), BrokerErrorCode> {
    let socket = session_bus_socket(bus_address)?;
    let metadata = std::fs::symlink_metadata(&socket).map_err(|_| BrokerErrorCode::Unavailable)?;
    // SAFETY: geteuid takes no arguments.
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(BrokerErrorCode::Unavailable);
    }

    // SAFETY: a null attribute and VERSION flag query the supported ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < 9 {
        return Err(BrokerErrorCode::Unavailable);
    }
    let attributes = RulesetAttr {
        handled_access_fs: FS_EXECUTE
            | FS_WRITE_FILE
            | FS_READ_FILE
            | FS_READ_DIR
            | FS_REMOVE_DIR
            | FS_REMOVE_FILE
            | FS_MAKE_CHAR
            | FS_MAKE_DIR
            | FS_MAKE_REG
            | FS_MAKE_SOCK
            | FS_MAKE_FIFO
            | FS_MAKE_BLOCK
            | FS_MAKE_SYM
            | FS_REFER
            | FS_TRUNCATE
            | FS_IOCTL_DEV
            | FS_RESOLVE_UNIX,
        handled_access_net: NET_BIND_TCP | NET_CONNECT_TCP,
        scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
    };
    // SAFETY: attributes is initialized for its complete declared size.
    let ruleset = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attributes,
            mem::size_of::<RulesetAttr>(),
            0,
        )
    };
    if ruleset < 0 {
        return Err(BrokerErrorCode::Unavailable);
    }
    // SAFETY: the syscall returned one uniquely owned descriptor.
    let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset as RawFd) };
    add_secret_helper_socket_rule(ruleset.as_raw_fd(), &socket)?;
    // SAFETY: no_new_privs is inherited from pre-exec, the ruleset descriptor
    // is valid, and flags zero is the supported restrict operation.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) } != 0 {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(())
}

fn session_bus_socket(address: &str) -> Result<PathBuf, BrokerErrorCode> {
    let address = address
        .strip_prefix("unix:path=")
        .ok_or(BrokerErrorCode::Unavailable)?;
    let mut fields = address.split(',');
    let path = fields.next().ok_or(BrokerErrorCode::Unavailable)?;
    for field in fields {
        let guid = field
            .strip_prefix("guid=")
            .ok_or(BrokerErrorCode::Unavailable)?;
        if guid.len() != 32 || !guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(BrokerErrorCode::Unavailable);
        }
    }
    if path.is_empty() || path.bytes().any(|byte| matches!(byte, b';' | b'%')) {
        return Err(BrokerErrorCode::Unavailable);
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(path)
}

fn add_secret_helper_socket_rule(ruleset: RawFd, socket: &Path) -> Result<(), BrokerErrorCode> {
    let socket = CString::new(socket.as_os_str().as_encoded_bytes())
        .map_err(|_| BrokerErrorCode::Unavailable)?;
    // SAFETY: socket is nul-terminated and O_PATH does not access content.
    let descriptor = unsafe { libc::open(socket.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(BrokerErrorCode::Unavailable);
    }
    // SAFETY: open returned one uniquely owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let attributes = PathBeneathAttr {
        allowed_access: FS_RESOLVE_UNIX,
        parent_fd: descriptor.as_raw_fd(),
    };
    // SAFETY: both descriptors and the packed rule attributes are valid.
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            &attributes,
            0,
        )
    } != 0
    {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(())
}

fn apply_secret_helper_seccomp() -> Result<(), BrokerErrorCode> {
    let mut filter = vec![
        seccomp_statement(BPF_LD_W_ABS, 4),
        seccomp_jump(BPF_JMP_JEQ_K, AUDIT_ARCH_NATIVE, 1, 0),
        seccomp_statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
        seccomp_statement(BPF_LD_W_ABS, 0),
    ];
    add_secret_sensitive_prctl_rule(&mut filter);
    add_secret_errno_rule(&mut filter, libc::SYS_clone3, libc::ENOSYS);
    add_secret_thread_clone_rule(&mut filter);
    for syscall in secret_blocked_syscalls() {
        filter.push(seccomp_jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
        filter.push(seccomp_statement(
            BPF_RET_K,
            SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ));
    }
    add_secret_unix_socket_rule(&mut filter, libc::SYS_socket);
    add_secret_unix_socket_rule(&mut filter, libc::SYS_socketpair);
    filter.push(seccomp_statement(BPF_RET_K, SECCOMP_RET_ALLOW));
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(|_| BrokerErrorCode::Internal)?,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: program references the live initialized filter for this call.
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &program as *const libc::sock_fprog,
        )
    } != 0
    {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(())
}

fn secret_blocked_syscalls() -> Vec<libc::c_long> {
    let syscalls = vec![
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
        libc::SYS_pidfd_open,
        libc::SYS_pidfd_getfd,
        libc::SYS_pidfd_send_signal,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_process_madvise,
        libc::SYS_kcmp,
        libc::SYS_prlimit64,
        libc::SYS_setpriority,
        libc::SYS_sched_setaffinity,
        libc::SYS_sched_setscheduler,
        libc::SYS_sched_setparam,
        libc::SYS_ioprio_set,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_fanotify_init,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_register,
        libc::SYS_io_uring_enter,
        libc::SYS_personality,
        libc::SYS_acct,
    ];
    #[cfg(target_arch = "x86_64")]
    let syscalls = {
        let mut syscalls = syscalls;
        syscalls.extend([libc::SYS_fork, libc::SYS_vfork]);
        syscalls
    };
    syscalls
}

fn add_secret_errno_rule(filter: &mut Vec<libc::sock_filter>, syscall: libc::c_long, errno: i32) {
    filter.push(seccomp_jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
    filter.push(seccomp_statement(
        BPF_RET_K,
        SECCOMP_RET_ERRNO | errno as u32,
    ));
}

fn add_secret_thread_clone_rule(filter: &mut Vec<libc::sock_filter>) {
    const BPF_ALU_AND_K: u16 = 0x54;
    const SECCOMP_ARG_ZERO_LOW: u32 = 16;
    let required = (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as u32;
    filter.push(seccomp_jump(BPF_JMP_JEQ_K, libc::SYS_clone as u32, 0, 4));
    filter.push(seccomp_statement(BPF_LD_W_ABS, SECCOMP_ARG_ZERO_LOW));
    filter.push(seccomp_statement(BPF_ALU_AND_K, required));
    filter.push(seccomp_jump(BPF_JMP_JEQ_K, required, 1, 0));
    filter.push(seccomp_statement(
        BPF_RET_K,
        SECCOMP_RET_ERRNO | libc::EPERM as u32,
    ));
}

fn add_secret_sensitive_prctl_rule(filter: &mut Vec<libc::sock_filter>) {
    const SECCOMP_ARG_ZERO_LOW: u32 = 16;
    filter.push(seccomp_jump(BPF_JMP_JEQ_K, libc::SYS_prctl as u32, 0, 5));
    filter.push(seccomp_statement(BPF_LD_W_ABS, SECCOMP_ARG_ZERO_LOW));
    filter.push(seccomp_jump(
        BPF_JMP_JEQ_K,
        libc::PR_SET_DUMPABLE as u32,
        2,
        0,
    ));
    filter.push(seccomp_jump(
        BPF_JMP_JEQ_K,
        libc::PR_SET_PTRACER as u32,
        1,
        0,
    ));
    filter.push(seccomp_jump(
        BPF_JMP_JEQ_K,
        libc::PR_SET_PDEATHSIG as u32,
        0,
        1,
    ));
    filter.push(seccomp_statement(
        BPF_RET_K,
        SECCOMP_RET_ERRNO | libc::EPERM as u32,
    ));
}

fn add_secret_unix_socket_rule(filter: &mut Vec<libc::sock_filter>, syscall: libc::c_long) {
    filter.push(seccomp_jump(BPF_JMP_JEQ_K, syscall as u32, 0, 3));
    filter.push(seccomp_statement(BPF_LD_W_ABS, 16));
    filter.push(seccomp_jump(BPF_JMP_JEQ_K, libc::AF_UNIX as u32, 1, 0));
    filter.push(seccomp_statement(
        BPF_RET_K,
        SECCOMP_RET_ERRNO | libc::EAFNOSUPPORT as u32,
    ));
}

const fn seccomp_statement(code: u16, value: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k: value,
    }
}

const fn seccomp_jump(code: u16, value: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt,
        jf,
        k: value,
    }
}

fn read_secret_over_dbus(
    object_path: &str,
    session_address: Option<&str>,
) -> Result<SecretMaterial, BrokerErrorCode> {
    let builder = match session_address {
        Some(address) => ConnectionBuilder::address(address),
        None => ConnectionBuilder::session(),
    }
    .map_err(|_| BrokerErrorCode::Unavailable)?;
    let connection = builder
        .method_timeout(SECRET_METHOD_TIMEOUT)
        .max_queued(1)
        .build()
        .map_err(|_| BrokerErrorCode::Unavailable)?;
    let owner = activate_and_resolve_secret_service(&connection)?;
    let open_session = connection
        .call_method(
            Some(owner.as_str()),
            SECRET_SERVICE_PATH,
            Some(SECRET_SERVICE_INTERFACE),
            "OpenSession",
            &("plain", Value::new("")),
        )
        .map_err(map_secret_call_error)?;
    validate_secret_reply(
        &open_session,
        &connection,
        &owner,
        "vo",
        SECRET_METADATA_REPLY_MAX_BYTES,
    )?;
    let (output, session): (OwnedValue, OwnedObjectPath) = open_session
        .body()
        .deserialize()
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    if <&str>::try_from(&output) != Ok("") {
        return Err(BrokerErrorCode::BackendFailed);
    }
    let result = read_item(&connection, &owner, object_path, &session);
    close_session(&connection, &owner, &session);
    result
}

fn read_item(
    connection: &Connection,
    owner: &str,
    object_path: &str,
    session: &OwnedObjectPath,
) -> Result<SecretMaterial, BrokerErrorCode> {
    zbus::zvariant::ObjectPath::try_from(object_path).map_err(|_| BrokerErrorCode::OutOfScope)?;
    let locked_reply = connection
        .call_method(
            Some(owner),
            object_path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(SECRET_ITEM_INTERFACE, "Locked"),
        )
        .map_err(map_secret_call_error)?;
    validate_secret_reply(
        &locked_reply,
        connection,
        owner,
        "v",
        SECRET_METADATA_REPLY_MAX_BYTES,
    )?;
    let locked = bool::try_from(
        locked_reply
            .body()
            .deserialize::<OwnedValue>()
            .map_err(|_| BrokerErrorCode::BackendFailed)?,
    )
    .map_err(|_| BrokerErrorCode::BackendFailed)?;
    if locked {
        return Err(BrokerErrorCode::Denied);
    }
    let secret_reply = connection
        .call_method(
            Some(owner),
            object_path,
            Some(SECRET_ITEM_INTERFACE),
            "GetSecret",
            session,
        )
        .map_err(map_secret_call_error)?;
    validate_secret_reply(
        &secret_reply,
        connection,
        owner,
        "oayays",
        SECRET_REPLY_MAX_BYTES,
    )?;
    let (returned_session, parameters, value, content_type): (
        OwnedObjectPath,
        Vec<u8>,
        Vec<u8>,
        String,
    ) = secret_reply
        .body()
        .deserialize()
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    if returned_session != *session || !parameters.is_empty() {
        return Err(BrokerErrorCode::BackendFailed);
    }
    validate_secret_value(value.len(), &content_type)?;
    Ok(SecretMaterial::new(value, content_type))
}

fn close_session(connection: &Connection, owner: &str, session: &OwnedObjectPath) {
    if let Ok(proxy) = Proxy::new(
        connection,
        owner,
        session.as_str(),
        SECRET_SESSION_INTERFACE,
    ) {
        let _ = proxy.call_noreply("Close", &());
    }
}

fn activate_and_resolve_secret_service(connection: &Connection) -> Result<String, BrokerErrorCode> {
    let proxy = DBusProxy::new(connection).map_err(|_| BrokerErrorCode::Unavailable)?;
    let well_known = WellKnownName::try_from(SECRET_SERVICE_DESTINATION)
        .map_err(|_| BrokerErrorCode::Internal)?;
    proxy
        .start_service_by_name(well_known.clone(), 0)
        .map_err(map_secret_fdo_error)?;
    proxy
        .get_name_owner(BusName::WellKnown(well_known))
        .map(|owner| owner.to_string())
        .map_err(map_secret_fdo_error)
}

fn validate_secret_reply(
    message: &Message,
    connection: &Connection,
    owner: &str,
    signature: &str,
    maximum_body_bytes: usize,
) -> Result<(), BrokerErrorCode> {
    let header = message.header();
    let destination = connection
        .unique_name()
        .ok_or(BrokerErrorCode::BackendFailed)?;
    if message.message_type() != Type::MethodReturn
        || header.sender().map(|name| name.as_str()) != Some(owner)
        || header.destination().map(|name| name.as_str()) != Some(destination.as_str())
        || message.body().len() > maximum_body_bytes
        || header.unix_fds().unwrap_or(0) != 0
    {
        return Err(
            if message.body().len() > maximum_body_bytes || header.unix_fds().unwrap_or(0) != 0 {
                BrokerErrorCode::QuotaExceeded
            } else {
                BrokerErrorCode::BackendFailed
            },
        );
    }
    let actual = message.body().signature().to_string();
    let actual = actual
        .strip_prefix('(')
        .and_then(|actual| actual.strip_suffix(')'))
        .unwrap_or(&actual);
    (actual == signature)
        .then_some(())
        .ok_or(BrokerErrorCode::BackendFailed)
}

fn map_secret_call_error(error: zbus::Error) -> BrokerErrorCode {
    match error {
        zbus::Error::InputOutput(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            BrokerErrorCode::Timeout
        }
        _ => BrokerErrorCode::BackendFailed,
    }
}

fn map_secret_fdo_error(error: zbus::fdo::Error) -> BrokerErrorCode {
    match error {
        zbus::fdo::Error::ZBus(zbus::Error::InputOutput(error))
            if error.kind() == std::io::ErrorKind::TimedOut =>
        {
            BrokerErrorCode::Timeout
        }
        zbus::fdo::Error::NameHasNoOwner(_) | zbus::fdo::Error::ServiceUnknown(_) => {
            BrokerErrorCode::Unavailable
        }
        _ => BrokerErrorCode::BackendFailed,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        net::TcpListener,
        os::unix::net::{UnixListener, UnixStream},
        process::Command,
        sync::{Arc, Mutex, atomic::AtomicBool},
        time::Instant,
    };

    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        GrantBindings, PackageInstance, Provenance, RuntimeKind, SecretReadScope,
    };
    use touchbar_protocol::broker_ipc::{ActivationContext, ActivationOrigin};

    use super::*;
    use crate::ConnectionIdentity;

    struct PrivateBus {
        address: String,
        pid: libc::pid_t,
        _directory: tempfile::TempDir,
    }

    impl PrivateBus {
        fn start() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let address = format!("unix:path={}", directory.path().join("bus").display());
            let output = Command::new("dbus-daemon")
                .args([
                    "--session",
                    "--fork",
                    "--nopidfile",
                    &format!("--address={address}"),
                    "--print-address=1",
                    "--print-pid=1",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            let output = String::from_utf8(output.stdout).unwrap();
            let mut lines = output.lines();
            let address = lines.next().unwrap().to_owned();
            let pid = lines.next().unwrap().parse().unwrap();
            Self {
                address,
                pid,
                _directory: directory,
            }
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            // SAFETY: this PID came from the private daemon started above.
            let _ = unsafe { libc::kill(self.pid, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                // SAFETY: signal zero only checks this exact PID.
                if unsafe { libc::kill(self.pid, 0) } != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    struct PrivateSecretService;

    #[zbus::interface(name = "org.freedesktop.Secret.Service")]
    impl PrivateSecretService {
        fn open_session(
            &self,
            algorithm: &str,
            _input: OwnedValue,
        ) -> zbus::fdo::Result<(OwnedValue, OwnedObjectPath)> {
            if algorithm != "plain" {
                return Err(zbus::fdo::Error::NotSupported("plain only".into()));
            }
            Ok((
                OwnedValue::from(zbus::zvariant::Str::from("")),
                OwnedObjectPath::try_from("/org/freedesktop/secrets/session/test").unwrap(),
            ))
        }
    }

    struct PrivateSecretItem;

    #[zbus::interface(name = "org.freedesktop.Secret.Item")]
    impl PrivateSecretItem {
        #[zbus(property)]
        fn locked(&self) -> bool {
            false
        }

        fn get_secret(
            &self,
            session: OwnedObjectPath,
        ) -> (OwnedObjectPath, Vec<u8>, Vec<u8>, String) {
            (
                session,
                Vec::new(),
                b"private-bus-secret".to_vec(),
                "text/plain".into(),
            )
        }
    }

    struct PrivateSecretSession;

    #[zbus::interface(name = "org.freedesktop.Secret.Session")]
    impl PrivateSecretSession {
        fn close(&self) {}
    }

    #[derive(Clone, Default)]
    struct FakeSecrets {
        reads: Arc<Mutex<Vec<SecretBinding>>>,
        oversized: bool,
    }

    impl SecretTransport for FakeSecrets {
        fn read(
            &self,
            binding: &SecretBinding,
            _cancellation: &CancellationToken,
        ) -> Result<SecretMaterial, BrokerErrorCode> {
            self.reads.lock().unwrap().push(binding.clone());
            Ok(if self.oversized {
                SecretMaterial::new(vec![7; MAX_SECRET_BYTES + 1], "application/octet-stream")
            } else {
                SecretMaterial::new(b"super-secret".to_vec(), "text/plain; charset=utf8")
            })
        }
    }

    fn binding(path: &str) -> SecretBinding {
        SecretBinding::SecretServiceItem {
            object_path: path.into(),
        }
    }

    fn request(name: &str) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 3,
                package: PackageInstance {
                    source: GithubSource::new("alice", "secret-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::SecretReadV1,
            authorized_scope: CapabilityScope::SecretRead(SecretReadScope {
                logical_names: BTreeSet::from(["github-token".into()]),
            }),
            bindings: GrantBindings {
                secrets: BTreeMap::from([(
                    "github-token".into(),
                    binding("/org/freedesktop/secrets/collection/login/1"),
                )]),
                ..Default::default()
            },
            activation: None,
            operation: SECRET_READ_OPERATION.into(),
            payload: SecretReadRequest {
                logical_name: name.into(),
            }
            .encode()
            .unwrap(),
        }
    }

    fn activation(sequence: u64) -> ActivationContext {
        ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 4,
            item_id: "secrets".into(),
            widget_id: 9,
            input_sequence: sequence,
            deadline_monotonic_micros: 200,
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn exact_logical_name_and_single_use_activation_reach_only_bound_item() {
        let transport = FakeSecrets::default();
        let reads = transport.reads.clone();
        let backend = SecretReadBackend::new(transport);
        let mut request = request("github-token");
        let mut activations = ActivationLedger::new(8);
        assert_eq!(
            backend.authorize(&request, &mut activations, 100),
            Err(BrokerErrorCode::ActivationRequired)
        );
        request.activation = Some(activation(1));
        backend.authorize(&request, &mut activations, 100).unwrap();
        assert_eq!(
            backend.authorize(&request, &mut activations, 100),
            Err(BrokerErrorCode::ActivationRequired)
        );
        let mut trusted = request.clone();
        trusted.activation = Some(ActivationContext {
            origin: ActivationOrigin::TrustedControl,
            ..activation(2)
        });
        assert_eq!(
            backend.authorize(&trusted, &mut ActivationLedger::new(8), 100),
            Err(BrokerErrorCode::ActivationRequired)
        );
        let BrokerResult::Success { payload } = backend.execute(&request, &token()) else {
            panic!("secret read failed");
        };
        assert_eq!(
            SecretValue::decode(&payload).unwrap(),
            SecretValue {
                bytes: b"super-secret".to_vec(),
                content_type: "text/plain; charset=utf8".into(),
            }
        );
        assert_eq!(
            &*reads.lock().unwrap(),
            &[binding("/org/freedesktop/secrets/collection/login/1")]
        );
    }

    #[test]
    fn out_of_scope_invalid_and_rebound_names_never_reach_transport() {
        let transport = FakeSecrets::default();
        let reads = transport.reads.clone();
        let backend = SecretReadBackend::new(transport);
        for name in ["weather-key", "../github-token", "GitHub-Token", ""] {
            let mut request = request(name);
            request.activation = Some(activation(2));
            assert!(
                backend
                    .authorize(&request, &mut ActivationLedger::new(8), 100)
                    .is_err()
            );
        }
        let mut rebound = request("github-token");
        rebound.bindings.secrets.clear();
        rebound.activation = Some(activation(3));
        assert_eq!(
            backend.authorize(&rebound, &mut ActivationLedger::new(8), 100),
            Err(BrokerErrorCode::OutOfScope)
        );
        assert!(reads.lock().unwrap().is_empty());
    }

    #[test]
    fn oversized_secret_is_rejected_without_returning_any_prefix() {
        let backend = SecretReadBackend::new(FakeSecrets {
            oversized: true,
            ..Default::default()
        });
        let request = request("github-token");
        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );
    }

    #[test]
    fn private_bus_exercises_exact_production_secret_service_calls() {
        let bus = PrivateBus::start();
        let service = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name(SECRET_SERVICE_DESTINATION)
            .unwrap()
            .serve_at(SECRET_SERVICE_PATH, PrivateSecretService)
            .unwrap()
            .serve_at(
                "/org/freedesktop/secrets/collection/login/1",
                PrivateSecretItem,
            )
            .unwrap()
            .serve_at(
                "/org/freedesktop/secrets/session/test",
                PrivateSecretSession,
            )
            .unwrap()
            .build()
            .unwrap();
        let material = read_secret_over_dbus(
            "/org/freedesktop/secrets/collection/login/1",
            Some(&bus.address),
        )
        .unwrap();
        assert_eq!(material.bytes.as_slice(), b"private-bus-secret");
        assert_eq!(material.content_type, "text/plain");
        drop(service);
    }

    #[test]
    fn secret_helper_landlock_allows_only_the_pinned_bus_socket() {
        const CHILD: &str = "TOUCHBAR_SECRET_LANDLOCK_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let address = std::env::var("TOUCHBAR_SECRET_TEST_BUS").unwrap();
            let forbidden = std::env::var_os("TOUCHBAR_SECRET_FORBIDDEN").unwrap();
            let other_socket = std::env::var_os("TOUCHBAR_SECRET_OTHER_SOCKET").unwrap();
            // SAFETY: prctl takes scalar arguments for no_new_privs.
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
                0
            );
            apply_secret_helper_landlock(&address).unwrap();
            apply_secret_helper_seccomp().unwrap();

            assert_eq!(
                fs::read(forbidden).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                UnixStream::connect(other_socket).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert!(UnixStream::connect(session_bus_socket(&address).unwrap()).is_ok());
            assert_eq!(
                TcpListener::bind("127.0.0.1:0").unwrap_err().raw_os_error(),
                Some(libc::EAFNOSUPPORT)
            );
            assert_eq!(
                Command::new("/usr/bin/true").status().unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            // SAFETY: signal zero performs no delivery; Landlock rejects it
            // because the parent is outside the helper's scoped signal domain.
            assert_eq!(unsafe { libc::kill(libc::getppid(), 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            return;
        }

        let bus = PrivateBus::start();
        let directory = tempfile::tempdir().unwrap();
        let forbidden = directory.path().join("forbidden");
        fs::write(&forbidden, b"secret").unwrap();
        let other_socket = directory.path().join("other.sock");
        let _listener = UnixListener::bind(&other_socket).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "secret::tests::secret_helper_landlock_allows_only_the_pinned_bus_socket",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TOUCHBAR_SECRET_TEST_BUS", &bus.address)
            .env("TOUCHBAR_SECRET_FORBIDDEN", &forbidden)
            .env("TOUCHBAR_SECRET_OTHER_SOCKET", &other_socket)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
