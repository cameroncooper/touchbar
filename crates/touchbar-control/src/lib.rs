//! Bounded, versioned, same-user control protocol for `touchbar-sessiond`.

use std::{
    fs,
    io::{Read, Write},
    os::fd::{AsFd, BorrowedFd},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_millis(100);
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Ping { version: u32 },
    Reload { version: u32 },
    Status { version: u32 },
    ProfileSelect { version: u32, profile: String },
    ProfileAutomatic { version: u32 },
    HardwareYield { version: u32 },
}

impl Request {
    pub fn version(&self) -> u32 {
        match self {
            Self::Ping { version }
            | Self::Reload { version }
            | Self::Status { version }
            | Self::ProfileSelect { version, .. }
            | Self::ProfileAutomatic { version }
            | Self::HardwareYield { version } => *version,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessStatus {
    pub source: String,
    pub item: String,
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRuntimeStatus {
    pub hardware_connected: bool,
    pub hardware_yielded: bool,
    pub user_content_visible: bool,
    pub fn_pressed: bool,
    pub system_scene_visible: bool,
    pub profile: ProfileRuntimeStatus,
    pub plugin_placeholder: Option<PluginPlaceholderStatus>,
    pub power_source: PowerSourceStatus,
    pub animation_frame_rate_hz: u32,
}

/// A connection-scoped request for the normal user session to stop competing
/// for the hardware socket. Dropping this value releases the lease.
pub struct HardwareYieldLease {
    _stream: UnixStream,
}

pub fn acquire_hardware_yield(path: impl AsRef<Path>) -> Result<(Response, HardwareYieldLease)> {
    validate_control_socket_path(path.as_ref())?;
    let mut stream = UnixStream::connect(path.as_ref())
        .with_context(|| format!("connect to {}", path.as_ref().display()))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    if peer_uid(&stream)? != unsafe { libc::geteuid() } {
        bail!("control server uid is not authorized")
    }
    write_json(&mut stream, &Request::HardwareYield { version: VERSION })?;
    let response = read_json(&mut stream)?;
    Ok((response, HardwareYieldLease { _stream: stream }))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerSourceStatus {
    External,
    Battery,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPlaceholderStatus {
    pub item: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRuntimeStatus {
    pub configured: bool,
    pub ready: bool,
    pub automatic: bool,
    pub active: Option<String>,
    pub available: Vec<String>,
    pub missing_required_items: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    pub message: String,
    pub runtime: SessionRuntimeStatus,
    pub processes: Vec<ProcessStatus>,
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    uid: u32,
}

impl Server {
    pub fn bind(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        validate_control_socket_path(path)?;
        let parent = path.parent().context("control socket needs a parent")?;
        ensure_private_directory(parent)?;
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if !metadata.file_type().is_socket() {
                bail!("{} exists and is not a socket", path.display())
            }
            match UnixStream::connect(path) {
                Ok(_) => bail!(
                    "touchbar-sessiond control socket {} is already active",
                    path.display()
                ),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(path)?;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("probe {}", path.display()));
                }
            }
        }
        let listener = UnixListener::bind(path)
            .with_context(|| format!("bind control socket {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            uid: unsafe { libc::geteuid() },
        })
    }

    pub fn poll(&self) -> Result<Vec<(UnixStream, Request)>> {
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error).context("accept control client"),
            };
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            if peer_uid(&stream)? != self.uid {
                let _ = write_response(
                    &mut stream,
                    &Response {
                        version: VERSION,
                        ok: false,
                        message: "peer uid is not authorized".into(),
                        runtime: SessionRuntimeStatus {
                            hardware_connected: false,
                            hardware_yielded: false,
                            user_content_visible: false,
                            fn_pressed: false,
                            system_scene_visible: false,
                            profile: ProfileRuntimeStatus::default(),
                            plugin_placeholder: None,
                            power_source: PowerSourceStatus::Unknown,
                            animation_frame_rate_hz: 60,
                        },
                        processes: Vec::new(),
                    },
                );
                continue;
            }
            match read_request(&mut stream) {
                Ok(request) => requests.push((stream, request)),
                Err(error) => {
                    let _ = write_response(
                        &mut stream,
                        &Response {
                            version: VERSION,
                            ok: false,
                            message: error.to_string(),
                            runtime: SessionRuntimeStatus {
                                hardware_connected: false,
                                hardware_yielded: false,
                                user_content_visible: false,
                                fn_pressed: false,
                                system_scene_visible: false,
                                profile: ProfileRuntimeStatus::default(),
                                plugin_placeholder: None,
                                power_source: PowerSourceStatus::Unknown,
                                animation_frame_rate_hz: 60,
                            },
                            processes: Vec::new(),
                        },
                    );
                }
            }
        }
        Ok(requests)
    }
}

impl AsFd for Server {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn call(path: impl AsRef<Path>, request: &Request) -> Result<Response> {
    validate_control_socket_path(path.as_ref())?;
    let mut stream = UnixStream::connect(path.as_ref())
        .with_context(|| format!("connect to {}", path.as_ref().display()))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    if peer_uid(&stream)? != unsafe { libc::geteuid() } {
        bail!("control server uid is not authorized")
    }
    write_json(&mut stream, request)?;
    read_json(&mut stream)
}

pub fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    write_json(stream, response)
}

fn read_request(stream: &mut UnixStream) -> Result<Request> {
    let request: Request = read_json(stream)?;
    if request.version() != VERSION {
        bail!("unsupported control protocol version {}", request.version())
    }
    Ok(request)
}

fn write_json(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        bail!("control message exceeds size limit")
    }
    stream.write_all(&u32::try_from(bytes.len())?.to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T> {
    let mut encoded = [0_u8; 4];
    stream.read_exact(&mut encoded)?;
    let length = u32::from_le_bytes(encoded) as usize;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        bail!("invalid control message length")
    }
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).context("invalid control message")
}

fn peer_uid(stream: &UnixStream) -> Result<u32> {
    use std::os::fd::AsRawFd;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("read control peer credentials");
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        bail!("invalid peer credentials")
    }
    Ok(credentials.uid)
}

fn validate_control_socket_path(path: &Path) -> Result<()> {
    let length = path.as_os_str().as_bytes().len();
    if length > UNIX_SOCKET_PATH_MAX_BYTES {
        bail!(
            "control socket path is {length} bytes, but Linux Unix socket paths may be at most {UNIX_SOCKET_PATH_MAX_BYTES} bytes: {}; choose a shorter TOUCHBAR_HOME or --control-socket path",
            path.display()
        );
    }
    if path.as_os_str().as_bytes().contains(&0) {
        bail!("control socket path contains a null byte")
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let metadata = fs::symlink_metadata(path)?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir()
        || std::os::unix::fs::MetadataExt::uid(&metadata) != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!("{} must be a private user-owned directory", path.display())
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};

    #[test]
    fn same_user_round_trip_is_versioned() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("control.sock");
        let server = Server::bind(&path).unwrap();
        let client_path = path.clone();
        let client =
            thread::spawn(move || call(client_path, &Request::Ping { version: VERSION }).unwrap());
        loop {
            let requests = server.poll().unwrap();
            if let Some((mut stream, request)) = requests.into_iter().next() {
                assert_eq!(request, Request::Ping { version: VERSION });
                write_response(
                    &mut stream,
                    &Response {
                        version: VERSION,
                        ok: true,
                        message: "pong".into(),
                        runtime: SessionRuntimeStatus {
                            hardware_connected: false,
                            hardware_yielded: false,
                            user_content_visible: false,
                            fn_pressed: false,
                            system_scene_visible: false,
                            profile: ProfileRuntimeStatus::default(),
                            plugin_placeholder: None,
                            power_source: PowerSourceStatus::Unknown,
                            animation_frame_rate_hz: 60,
                        },
                        processes: Vec::new(),
                    },
                )
                .unwrap();
                break;
            }
            thread::yield_now();
        }
        assert_eq!(client.join().unwrap().message, "pong");
    }

    #[test]
    fn hardware_yield_lives_until_the_client_drops_its_connection() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("control.sock");
        let server = Server::bind(&path).unwrap();
        let client_path = path.clone();
        let (acquired_tx, acquired_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let client = thread::spawn(move || {
            let (response, lease) = acquire_hardware_yield(client_path).unwrap();
            assert!(response.ok);
            acquired_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(lease);
        });
        let mut lease_stream = loop {
            if let Some((mut stream, request)) = server.poll().unwrap().into_iter().next() {
                assert_eq!(request, Request::HardwareYield { version: VERSION });
                write_response(
                    &mut stream,
                    &Response {
                        version: VERSION,
                        ok: true,
                        message: "hardware yielded".into(),
                        runtime: SessionRuntimeStatus {
                            hardware_connected: false,
                            hardware_yielded: true,
                            user_content_visible: false,
                            fn_pressed: false,
                            system_scene_visible: false,
                            profile: ProfileRuntimeStatus::default(),
                            plugin_placeholder: None,
                            power_source: PowerSourceStatus::Unknown,
                            animation_frame_rate_hz: 60,
                        },
                        processes: Vec::new(),
                    },
                )
                .unwrap();
                stream.set_nonblocking(true).unwrap();
                break stream;
            }
            thread::yield_now();
        };
        acquired_rx.recv().unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(
            lease_stream.read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        release_tx.send(()).unwrap();
        client.join().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match lease_stream.read(&mut byte) {
                Ok(0) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline);
                    thread::yield_now();
                }
                other => panic!("unexpected lease read result: {other:?}"),
            }
        }
    }

    #[test]
    fn fresh_v1_response_requires_explicit_runtime_state() {
        let missing_runtime = br#"{
            "version": 1,
            "ok": true,
            "message": "running",
            "processes": []
        }"#;
        assert!(serde_json::from_slice::<Response>(missing_runtime).is_err());

        let response = Response {
            version: VERSION,
            ok: true,
            message: "running".into(),
            runtime: SessionRuntimeStatus {
                hardware_connected: true,
                hardware_yielded: false,
                user_content_visible: true,
                fn_pressed: false,
                system_scene_visible: true,
                profile: ProfileRuntimeStatus::default(),
                plugin_placeholder: None,
                power_source: PowerSourceStatus::External,
                animation_frame_rate_hz: 60,
            },
            processes: Vec::new(),
        };
        assert_eq!(
            serde_json::from_slice::<Response>(&serde_json::to_vec(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn existing_public_parent_is_rejected_without_chmod() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("public");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Server::bind(parent.join("control.sock")).is_err());
        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn symlink_parent_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(Server::bind(link.join("control.sock")).is_err());
    }

    #[test]
    fn oversized_socket_path_has_an_actionable_error_before_bind() {
        let path = PathBuf::from(format!("/tmp/{}/control.sock", "deep".repeat(30)));
        let error = match Server::bind(&path) {
            Ok(_) => panic!("oversized socket path unexpectedly bound"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("107 bytes"));
        assert!(error.contains("shorter TOUCHBAR_HOME"));
    }
}
