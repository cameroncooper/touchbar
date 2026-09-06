use std::{
    fs,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use touchbar_plugin_supervisor::{CancellationToken, SecretTransport, ZbusSecretService};
use touchbar_policy::SecretBinding;
use zbus::{
    blocking::{Connection, connection::Builder as ConnectionBuilder},
    zvariant::{OwnedObjectPath, OwnedValue},
};

const SERVICE: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const ITEM_PATH: &str = "/org/freedesktop/secrets/collection/login/1";
const SESSION_PATH: &str = "/org/freedesktop/secrets/session/test";

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
        assert!(lines.next().is_none());
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
            thread::sleep(Duration::from_millis(1));
        }
    }
}

struct SecretService;

#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl SecretService {
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
            OwnedObjectPath::try_from(SESSION_PATH).unwrap(),
        ))
    }
}

struct SecretSession {
    closes: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.freedesktop.Secret.Session")]
impl SecretSession {
    fn close(&self) {
        self.closes.fetch_add(1, Ordering::Release);
    }
}

struct SecretItem {
    value_bytes: usize,
    delay: Duration,
    calls: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.freedesktop.Secret.Item")]
impl SecretItem {
    #[zbus(property)]
    fn locked(&self) -> bool {
        false
    }

    fn get_secret(&self, session: OwnedObjectPath) -> (OwnedObjectPath, Vec<u8>, Vec<u8>, String) {
        self.calls.fetch_add(1, Ordering::Release);
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        (
            session,
            Vec::new(),
            vec![0x5a; self.value_bytes],
            "application/octet-stream".into(),
        )
    }
}

struct MalformedSecretItem;

#[zbus::interface(name = "org.freedesktop.Secret.Item")]
impl MalformedSecretItem {
    #[zbus(property)]
    fn locked(&self) -> bool {
        false
    }

    fn get_secret(&self, _session: OwnedObjectPath) -> &'static str {
        "not-a-secret-structure"
    }
}

struct Harness {
    bus: PrivateBus,
    _service: Connection,
    calls: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
}

impl Harness {
    fn correct(value_bytes: usize, delay: Duration) -> Self {
        let bus = PrivateBus::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let service = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .serve_at(SERVICE_PATH, SecretService)
            .unwrap()
            .serve_at(
                ITEM_PATH,
                SecretItem {
                    value_bytes,
                    delay,
                    calls: calls.clone(),
                },
            )
            .unwrap()
            .serve_at(
                SESSION_PATH,
                SecretSession {
                    closes: closes.clone(),
                },
            )
            .unwrap()
            .build()
            .unwrap();
        Self {
            bus,
            _service: service,
            calls,
            closes,
        }
    }

    fn malformed() -> Self {
        let bus = PrivateBus::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let service = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .serve_at(SERVICE_PATH, SecretService)
            .unwrap()
            .serve_at(ITEM_PATH, MalformedSecretItem)
            .unwrap()
            .serve_at(
                SESSION_PATH,
                SecretSession {
                    closes: closes.clone(),
                },
            )
            .unwrap()
            .build()
            .unwrap();
        Self {
            bus,
            _service: service,
            calls,
            closes,
        }
    }

    fn transport(&self) -> ZbusSecretService {
        ZbusSecretService::with_helper(
            env!("CARGO_BIN_EXE_touchbar-secret-helper"),
            Some(self.bus.address.clone()),
        )
    }
}

fn binding() -> SecretBinding {
    SecretBinding::SecretServiceItem {
        object_path: ITEM_PATH.into(),
    }
}

fn token(cancelled: Arc<AtomicBool>, timeout: Duration) -> CancellationToken {
    CancellationToken::new(cancelled, timeout).unwrap()
}

#[test]
fn isolated_helper_returns_only_a_bounded_valid_secret() {
    let harness = Harness::correct(32, Duration::ZERO);
    let material = harness
        .transport()
        .read(
            &binding(),
            &token(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(material.bytes(), &[0x5a; 32]);
    assert_eq!(material.content_type(), "application/octet-stream");
    wait_for_count(&harness.closes, 1);
}

#[test]
fn isolated_helper_rejects_oversized_and_malformed_replies() {
    let oversized = Harness::correct(2 * 1024 * 1024, Duration::ZERO);
    assert!(matches!(
        oversized.transport().read(
            &binding(),
            &token(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)),
        ),
        Err(touchbar_protocol::broker_ipc::BrokerErrorCode::QuotaExceeded)
    ));
    wait_for_count(&oversized.closes, 1);

    let malformed = Harness::malformed();
    assert!(matches!(
        malformed.transport().read(
            &binding(),
            &token(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)),
        ),
        Err(touchbar_protocol::broker_ipc::BrokerErrorCode::BackendFailed)
    ));
    wait_for_count(&malformed.closes, 1);
}

#[test]
fn cancellation_kills_and_reaps_a_stalled_helper() {
    let harness = Harness::correct(32, Duration::from_millis(500));
    let baseline_children = child_pids();
    let cancelled = Arc::new(AtomicBool::new(false));
    let canceller = {
        let cancelled = cancelled.clone();
        let calls = harness.calls.clone();
        thread::spawn(move || {
            wait_for_count(&calls, 1);
            let deadline = Instant::now() + Duration::from_secs(1);
            let helper_pid = loop {
                if let Some(pid) = child_pids()
                    .into_iter()
                    .find(|pid| !baseline_children.contains(pid))
                {
                    break pid;
                }
                assert!(
                    Instant::now() < deadline,
                    "helper process was not observable"
                );
                thread::sleep(Duration::from_millis(1));
            };
            let limits = fs::read_to_string(format!("/proc/{helper_pid}/limits")).unwrap();
            let status = fs::read_to_string(format!("/proc/{helper_pid}/status")).unwrap();
            let memory_error = fs::File::open(format!("/proc/{helper_pid}/mem"))
                .unwrap_err()
                .kind();
            cancelled.store(true, Ordering::Release);
            (limits, status, memory_error)
        })
    };
    let started = Instant::now();
    let result = harness
        .transport()
        .read(&binding(), &token(cancelled, Duration::from_secs(2)));
    let (limits, status, memory_error) = canceller.join().unwrap();
    let address_space = limits
        .lines()
        .find(|line| line.starts_with("Max address space"))
        .unwrap();
    assert!(address_space.contains("134217728"), "{address_space}");
    assert!(status.lines().any(|line| line == "NoNewPrivs:\t1"));
    assert!(status.lines().any(|line| line == "Seccomp:\t2"));
    assert_eq!(memory_error, std::io::ErrorKind::PermissionDenied);
    assert!(matches!(
        result,
        Err(touchbar_protocol::broker_ipc::BrokerErrorCode::Cancelled)
    ));
    assert!(started.elapsed() < Duration::from_millis(300));
}

#[test]
#[ignore = "release-only hostile Secret Service isolation campaign"]
fn security_campaign_secret_helper_is_bounded_and_reclaimable() {
    let normal = Harness::correct(48 * 1024, Duration::ZERO);
    let oversized = Harness::correct(2 * 1024 * 1024, Duration::ZERO);
    let stalled = Harness::correct(32, Duration::from_millis(100));
    let baseline_threads = process_thread_count();
    let baseline_children = process_children();
    for _ in 0..16 {
        let material = normal
            .transport()
            .read(
                &binding(),
                &token(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)),
            )
            .unwrap();
        assert_eq!(material.bytes().len(), 48 * 1024);
    }

    for _ in 0..8 {
        assert!(matches!(
            oversized.transport().read(
                &binding(),
                &token(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)),
            ),
            Err(touchbar_protocol::broker_ipc::BrokerErrorCode::QuotaExceeded)
        ));
    }

    for expected_call in 1..=8 {
        let cancelled = Arc::new(AtomicBool::new(false));
        let canceller = {
            let cancelled = cancelled.clone();
            let calls = stalled.calls.clone();
            thread::spawn(move || {
                wait_for_count(&calls, expected_call);
                cancelled.store(true, Ordering::Release);
            })
        };
        assert!(matches!(
            stalled
                .transport()
                .read(&binding(), &token(cancelled, Duration::from_secs(2))),
            Err(touchbar_protocol::broker_ipc::BrokerErrorCode::Cancelled)
        ));
        canceller.join().unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while (process_thread_count() > baseline_threads + 2 || process_children() != baseline_children)
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(process_thread_count() <= baseline_threads + 2);
    assert_eq!(process_children(), baseline_children);
}

fn wait_for_count(counter: &AtomicUsize, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while counter.load(Ordering::Acquire) < expected && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(counter.load(Ordering::Acquire) >= expected);
}

fn process_thread_count() -> usize {
    fs::read_dir("/proc/self/task").unwrap().count()
}

fn process_children() -> String {
    child_pids()
        .into_iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn child_pids() -> Vec<u32> {
    let mut children = Vec::<u32>::new();
    for task in fs::read_dir("/proc/self/task").unwrap() {
        let path = task.unwrap().path().join("children");
        let Ok(value) = fs::read_to_string(path) else {
            continue;
        };
        children.extend(
            value
                .split_ascii_whitespace()
                .map(|pid| pid.parse::<u32>().unwrap()),
        );
    }
    children.sort_unstable();
    children.dedup();
    children
}
