use std::{
    collections::BTreeSet,
    fs::{self, File},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use touchbar_package::GithubSource;
use touchbar_policy::{
    CapabilityId, CapabilityScope, ContextReadScope, Decision, GrantRecord, GrantStore, ReusePolicy,
};

fn fixture(required_permission: bool) -> (TempDir, PathBuf, String) {
    let directory = tempfile::tempdir().unwrap();
    let package = directory.path().join("package");
    let state = directory.path().join("state");
    fs::create_dir_all(package.join("component")).unwrap();
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let component = b"locked component artifact";
    fs::write(package.join("component/plugin.wasm"), component).unwrap();
    let permission = if required_permission {
        r#"
[[permission]]
capability = "context.read.v1"
required = true
reason = "Required launch test"
[permission.scope]
facts = ["application.id"]
maximum_updates_per_second = 1
"#
    } else {
        ""
    };
    fs::write(
        package.join("touchbar-plugin.toml"),
        format!(
            r#"
manifest_version = 1

[plugin]
name = "Launch Test"
version = "1.0.0"
description = "Supervisor launch fixture"
license = "MIT"
source = "github:alice/launch-test"
api = "^1.0"

[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/plugin@1.0.0"

[[items]]
id = "test"
label = "Test"
{permission}
"#
        ),
    )
    .unwrap();
    let digest = format!("sha256:{:x}", Sha256::digest(component));
    (directory, package, digest)
}

fn supervisor(package: &Path, digest: &str, host: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_touchbar-plugin-supervisor"));
    command
        .arg(package)
        .args(["--host", host.to_str().unwrap()])
        .args(["--source", "github:alice/launch-test"])
        .args(["--version", "1.0.0"])
        .args(["--digest", digest])
        .args(["--provenance", "local-development"])
        .args([
            "--state",
            package.parent().unwrap().join("state").to_str().unwrap(),
        ]);
    command
}

fn add_asset(package: &Path, bytes: &[u8]) -> String {
    fs::create_dir(package.join("assets")).unwrap();
    fs::write(package.join("assets/mark.svg"), bytes).unwrap();
    let manifest = fs::read_to_string(package.join("touchbar-plugin.toml")).unwrap();
    fs::write(
        package.join("touchbar-plugin.toml"),
        format!(
            "{manifest}\n[[asset]]\nid = \"mark\"\npath = \"assets/mark.svg\"\nkind = \"symbolic-svg\"\nwidth = 24\nheight = 24\n"
        ),
    )
    .unwrap();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn save_context_grant(path: &Path, digest: &str, decision: Decision) {
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut grants = GrantStore::default();
    grants
        .insert(GrantRecord {
            source: GithubSource::new("alice", "launch-test").unwrap(),
            capability: CapabilityId::ContextReadV1,
            approved_scope: CapabilityScope::ContextRead(ContextReadScope {
                facts: BTreeSet::from(["application.id".into()]),
                maximum_updates_per_second: 1,
            }),
            bindings: Default::default(),
            decision,
            reuse: ReusePolicy::ExactDigest,
            approved_version: semver::Version::new(1, 0, 0),
            approved_digest: digest.into(),
        })
        .unwrap();
    grants.save(path).unwrap();
}

fn wait_for_pids(path: &Path, count: usize) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let pids = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect::<Vec<_>>();
        if pids.len() >= count {
            return pids;
        }
        assert!(Instant::now() < deadline, "host was not launched in time");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_until_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: signal zero performs an existence check and sends no signal.
        let exists = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !exists {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "revoked host process remained alive"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn installer_identity_and_digest_are_verified_before_exec() {
    let (directory, package, digest) = fixture(false);
    assert!(
        supervisor(&package, &digest, Path::new("/bin/true"))
            .status()
            .unwrap()
            .success()
    );

    let wrong_digest = format!("sha256:{}", "0".repeat(64));
    let output = supervisor(&package, &wrong_digest, Path::new("/bin/true"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not match the installer lock"));

    let output = Command::new(env!("CARGO_BIN_EXE_touchbar-plugin-supervisor"))
        .arg(&package)
        .args(["--host", "/bin/true"])
        .args(["--source", "github:alice/launch-test"])
        .args(["--version", "1.0.0"])
        .args(["--digest", &digest])
        .args(["--state", directory.path().join("state").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--provenance is required"));

    let output = Command::new(env!("CARGO_BIN_EXE_touchbar-plugin-supervisor"))
        .arg(&package)
        .args(["--host", "/bin/true"])
        .args(["--source", "github:mallory/forged"])
        .args(["--version", "1.0.0"])
        .args(["--digest", &digest])
        .args(["--provenance", "local-development"])
        .args(["--state", directory.path().join("state").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("installer-owned source and version"));
}

#[test]
fn denied_required_capability_prevents_host_execution() {
    let (directory, package, digest) = fixture(true);
    let marker = directory.path().join("host-executed");
    let host = directory.path().join("fake-host.sh");
    fs::write(&host, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();

    let output = supervisor(&package, &digest, &host).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("host was not executed"));
    assert!(!marker.exists());
}

#[test]
fn audit_option_creates_private_sink_before_host_execution() {
    let (directory, package, digest) = fixture(false);
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let audit = directory.path().join("audit.jsonl");
    let status = supervisor(&package, &digest, Path::new("/bin/true"))
        .args(["--audit", audit.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::symlink_metadata(&audit).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(directory.path().join(".audit.jsonl.lock").is_file());
}

#[test]
fn component_host_is_launched_with_no_new_privileges() {
    let (directory, package, digest) = fixture(false);
    let observed = directory.path().join("no-new-privs");
    let host = directory.path().join("inspect-host.sh");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\nawk '/^NoNewPrivs:/ {{ print $2 }}' /proc/self/status > '{}'\n",
            observed.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        supervisor(&package, &digest, &host)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read_to_string(observed).unwrap().trim(), "1");
}

#[test]
fn component_host_is_moved_into_a_bounded_ephemeral_cgroup() {
    let (directory, package, digest) = fixture(false);
    let observed = directory.path().join("containment");
    let host = directory.path().join("inspect-containment.sh");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\nsed -n 's/^0:://p' /proc/self/cgroup > '{}'\ngrep '^Max address space' /proc/self/limits >> '{}'\ngrep '^Max processes' /proc/self/limits >> '{}'\n",
            observed.display(),
            observed.display(),
            observed.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        supervisor(&package, &digest, &host)
            .status()
            .unwrap()
            .success()
    );
    let lines = fs::read_to_string(&observed).unwrap();
    let mut lines = lines.lines();
    let membership = lines.next().unwrap();
    assert!(membership.contains("/touchbar-component-"));
    let address = lines.next().unwrap().split_whitespace().collect::<Vec<_>>();
    assert_eq!(&address[3..5], &["8589934592", "8589934592"]);
    let processes = lines.next().unwrap().split_whitespace().collect::<Vec<_>>();
    assert_eq!(processes[2], processes[3]);
    assert!(processes[2].parse::<u64>().unwrap() > 0);
    assert!(
        !Path::new("/sys/fs/cgroup")
            .join(membership.trim_start_matches('/'))
            .exists(),
        "component cgroup remained after a clean host exit"
    );
}

#[test]
fn host_launch_scrubs_ambient_environment_and_descriptors() {
    let (directory, package, digest) = fixture(false);
    let observed = directory.path().join("launch-boundary");
    let host = directory.path().join("inspect-boundary.sh");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\nif [ -e /proc/self/fd/20 ] || [ -n \"${{AMBIENT_SECRET+x}}\" ]; then printf unsafe > '{}'; else printf clean > '{}'; fi\n",
            observed.display(),
            observed.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
    let inherited = File::open(package.join("touchbar-plugin.toml")).unwrap();
    let inherited_fd = inherited.as_raw_fd();
    let mut command = supervisor(&package, &digest, &host);
    command.env("AMBIENT_SECRET", "must-not-cross-exec");
    // SAFETY: dup2 is async-signal-safe and duplicates a live descriptor into
    // the supervisor process immediately before exec.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(inherited_fd, 20) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    assert!(command.status().unwrap().success());
    assert_eq!(fs::read_to_string(observed).unwrap(), "clean");
}

#[test]
fn crashed_host_is_reaped_and_the_supervisor_fails_closed() {
    let (directory, package, digest) = fixture(false);
    let host = directory.path().join("crashing-host.sh");
    fs::write(&host, "#!/bin/sh\nkill -ABRT $$\n").unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
    let output = supervisor(&package, &digest, &host).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
}

#[test]
fn host_receives_sealed_verified_bytes_despite_path_replacement() {
    let (directory, package, digest) = fixture(false);
    let ready = directory.path().join("ready");
    let proceed = directory.path().join("proceed");
    let observed = directory.path().join("observed-digest");
    let writable = directory.path().join("artifact-write-state");
    let host = directory.path().join("inspect-artifact.sh");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nsha256sum /proc/self/fd/4 | awk '{{ print $1 }}' > '{}'\nif printf x >&4 2>/dev/null; then echo writable > '{}'; else echo sealed > '{}'; fi\n",
            ready.display(),
            proceed.display(),
            observed.display(),
            writable.display(),
            writable.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();

    let mut child = supervisor(&package, &digest, &host)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists());
    fs::write(
        package.join("component/replacement.wasm"),
        b"attacker replacement",
    )
    .unwrap();
    fs::rename(
        package.join("component/replacement.wasm"),
        package.join("component/plugin.wasm"),
    )
    .unwrap();
    fs::write(&proceed, b"go").unwrap();
    assert!(child.wait().unwrap().success());

    assert_eq!(
        fs::read_to_string(observed).unwrap().trim(),
        digest.strip_prefix("sha256:").unwrap()
    );
    assert_eq!(fs::read_to_string(writable).unwrap().trim(), "sealed");
}

#[test]
fn assets_require_installer_digests_and_cross_exec_only_as_a_sealed_bundle() {
    let (directory, package, digest) = fixture(false);
    let original = b"trusted-svg";
    let asset_digest = add_asset(&package, original);

    let missing = supervisor(&package, &digest, Path::new("/bin/true"))
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("exactly match"));

    let wrong = format!("mark=sha256:{}", "0".repeat(64));
    let output = supervisor(&package, &digest, Path::new("/bin/true"))
        .args(["--asset-digest", &wrong])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("installer lock"));

    let ready = directory.path().join("asset-ready");
    let proceed = directory.path().join("asset-proceed");
    let observed = directory.path().join("asset-digest");
    let writable = directory.path().join("asset-write-state");
    let host = directory.path().join("inspect-assets.sh");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\ntail -c +23 /proc/self/fd/6 | sha256sum | awk '{{ print $1 }}' > '{}'\nif printf x >&6 2>/dev/null; then echo writable > '{}'; else echo sealed > '{}'; fi\n",
            ready.display(),
            proceed.display(),
            observed.display(),
            writable.display(),
            writable.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();

    let argument = format!("mark={asset_digest}");
    let mut child = supervisor(&package, &digest, &host)
        .args(["--asset-digest", &argument])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists());
    fs::write(package.join("assets/replacement.svg"), b"attacker").unwrap();
    fs::rename(
        package.join("assets/replacement.svg"),
        package.join("assets/mark.svg"),
    )
    .unwrap();
    fs::write(&proceed, b"go").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        fs::read_to_string(observed).unwrap().trim(),
        asset_digest.strip_prefix("sha256:").unwrap()
    );
    assert_eq!(fs::read_to_string(writable).unwrap().trim(), "sealed");
}

#[test]
fn required_grant_revocation_stops_and_regrant_restarts_the_host() {
    let (directory, package, digest) = fixture(true);
    let grants = directory.path().join("permissions.toml");
    let launches = directory.path().join("launches");
    let host = directory.path().join("fake-host.sh");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> '{}'\nexec cat <&3 >/dev/null\n",
            launches.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
    save_context_grant(&grants, &digest, Decision::Allow);

    let child = supervisor(&package, &digest, &host)
        .args(["--grants", grants.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let first_pid = wait_for_pids(&launches, 1)[0];

    save_context_grant(&grants, &digest, Decision::Deny);
    wait_until_gone(first_pid);
    assert_eq!(wait_for_pids(&launches, 1).len(), 1);

    save_context_grant(&grants, &digest, Decision::Allow);
    let pids = wait_for_pids(&launches, 2);
    assert_ne!(pids[0], pids[1]);

    let killed_supervisor = child.0.id();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    wait_until_gone(pids[1]);

    // SIGKILL bypasses the supervisor's cgroup destructor. A subsequent
    // launch must safely reap the empty, dead-owner leaf before creating its
    // own containment boundary.
    assert!(
        supervisor(&package, &digest, Path::new("/bin/true"))
            .args(["--grants", grants.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    let current = fs::read_to_string("/proc/self/cgroup").unwrap();
    let relative = current
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))
        .unwrap();
    let parent = Path::new("/sys/fs/cgroup").join(relative);
    let stale_prefix = format!("touchbar-component-{killed_supervisor}-");
    assert!(
        fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(&stale_prefix)),
        "dead supervisor left an orphan component cgroup"
    );
}

#[test]
fn session_grants_override_persistent_policy_and_reload_live() {
    let (directory, package, digest) = fixture(true);
    let persistent_directory = directory.path().join("persistent");
    let session_directory = directory.path().join("runtime");
    fs::create_dir(&persistent_directory).unwrap();
    fs::create_dir(&session_directory).unwrap();
    fs::set_permissions(&persistent_directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&session_directory, fs::Permissions::from_mode(0o700)).unwrap();
    let grants = persistent_directory.join("permissions.toml");
    let session_grants = session_directory.join("session-permissions.toml");
    let launches = directory.path().join("session-launches");
    let host = directory.path().join("session-host.sh");
    fs::write(
        &host,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> '{}'\nexec cat <&3 >/dev/null\n",
            launches.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();

    save_context_grant(&grants, &digest, Decision::Deny);
    save_context_grant(&session_grants, &digest, Decision::Allow);
    let child = supervisor(&package, &digest, &host)
        .args(["--grants", grants.to_str().unwrap()])
        .args(["--session-grants", session_grants.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let first_pid = wait_for_pids(&launches, 1)[0];

    // A session denial has precedence over the persistent decision and must
    // revoke the already-running component without restarting its supervisor.
    save_context_grant(&session_grants, &digest, Decision::Deny);
    wait_until_gone(first_pid);
    assert_eq!(wait_for_pids(&launches, 1).len(), 1);

    // Clearing the session layer falls back to the persistent denial, so the
    // component must remain stopped until durable policy itself changes.
    GrantStore::default().save(&session_grants).unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(wait_for_pids(&launches, 1).len(), 1);

    save_context_grant(&grants, &digest, Decision::Allow);
    let pids = wait_for_pids(&launches, 2);
    assert_ne!(pids[0], pids[1]);

    child.0.kill().unwrap();
    child.0.wait().unwrap();
    wait_until_gone(pids[1]);
}
