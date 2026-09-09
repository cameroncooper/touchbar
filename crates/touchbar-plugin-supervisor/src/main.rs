use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::CString,
    fs,
    io::{Read, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use semver::Version;
use sha2::{Digest, Sha256};
use touchbar_package::{
    GithubSource, MANIFEST_FILE_NAME, MAX_ASSET_FILE_BYTES, PluginManifest, RuntimeSpec,
    encode_asset_bundle,
};
use touchbar_plugin_supervisor::{
    AppearanceProviderBackend, AuditFile, AuditFileLimits, ClipboardBackend, CommandRunBackend,
    ComponentCgroup, ConnectionExit, ConnectionIdentity, ConnectionLimits, ContextReadBackend,
    DbusCallBackend, DbusSubscriptionBackend, FilesystemReadBackend, FilesystemWriteBackend,
    GrantStoreWatcher, HttpRequestBackend, LocalIpcBackend, NotificationBackend, SecretReadBackend,
    SupervisorConnection, UriOpenBackend, WaylandClipboard, ZbusDesktopPortal, ZbusSecretService,
    ZbusTransport, component_task_limit,
};
use touchbar_policy::{
    CapabilityId, CapabilityRegistry, CapabilityRequest, CapabilityScope, EffectivePolicy,
    GrantStore, PackageInstance, Provenance, RuntimeKind, SessionGrants,
    calculate_effective_policy,
};
use touchbar_protocol::broker_ipc::{Seqpacket, TransportError};

const BROKER_FD: i32 = 3;
const COMPONENT_FD: i32 = 4;
const MANIFEST_FD: i32 = 5;
const ASSET_BUNDLE_FD: i32 = 6;
const APPEARANCE_SINK_FD: i32 = 7;
const BROKER_FD_ENV: &str = "TOUCHBAR_BROKER_FD";
const COMPONENT_FD_ENV: &str = "TOUCHBAR_COMPONENT_FD";
const MANIFEST_FD_ENV: &str = "TOUCHBAR_MANIFEST_FD";
const ASSET_BUNDLE_FD_ENV: &str = "TOUCHBAR_ASSET_BUNDLE_FD";
const APPEARANCE_SINK_FD_ENV: &str = "TOUCHBAR_APPEARANCE_SINK_FD";
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const USAGE: &str = "usage: touchbar-plugin-supervisor PACKAGE --host HOST --source github:OWNER/REPO --version VERSION --digest sha256:HEX [--package-digest sha256:HEX] [--appearance-provider ID] [--asset-digest ID=sha256:HEX]... --provenance verified-release|unverified-release|local-development --state DIRECTORY [--grants FILE] [--session-grants FILE] [--audit FILE] [-- HOST_ARGUMENTS...]";

struct Arguments {
    package: PathBuf,
    host: PathBuf,
    source: GithubSource,
    version: Version,
    digest: String,
    package_digest: Option<String>,
    appearance_provider: Option<String>,
    asset_digests: Vec<String>,
    provenance: Provenance,
    state: PathBuf,
    grants: Option<PathBuf>,
    session_grants: Option<PathBuf>,
    audit: Option<PathBuf>,
    host_arguments: Vec<String>,
}

enum SessionOutcome {
    HostExited(u8),
    PolicyBlocked,
}

struct VisualBackends {
    filesystem_write: Arc<FilesystemWriteBackend>,
    notification: Arc<NotificationBackend<ZbusDesktopPortal>>,
    local_ipc: Arc<LocalIpcBackend>,
    clipboard: Arc<ClipboardBackend<WaylandClipboard>>,
}

impl VisualBackends {
    fn new(state: &Path, source: &str) -> Result<Self> {
        Ok(Self {
            filesystem_write: Arc::new(
                FilesystemWriteBackend::new(state, source)
                    .context("open persistent filesystem write quota state")?,
            ),
            notification: Arc::new(
                NotificationBackend::new(ZbusDesktopPortal::new(), state, source)
                    .context("open persistent notification rate state")?,
            ),
            local_ipc: Arc::new(
                LocalIpcBackend::new(state, source)
                    .context("open persistent local IPC quota state")?,
            ),
            clipboard: Arc::new(
                ClipboardBackend::new(WaylandClipboard::new(), state, source)
                    .context("open persistent clipboard rate state")?,
            ),
        })
    }
}

fn main() -> ExitCode {
    std::panic::set_hook(Box::new(|_| {
        eprintln!("touchbar-plugin-supervisor: internal worker panic");
    }));
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("touchbar-plugin-supervisor: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8> {
    harden_supervisor_process()?;
    let arguments = parse_arguments()?;
    validate_digest(&arguments.digest)?;
    if let Some(digest) = &arguments.package_digest {
        validate_digest(digest)?;
    }
    let root = arguments
        .package
        .canonicalize()
        .with_context(|| format!("open package {}", arguments.package.display()))?;
    let root_descriptor = open_package_root(&root)?;
    let manifest_bytes = read_beneath(
        root_descriptor.as_raw_fd(),
        std::path::Path::new(MANIFEST_FILE_NAME),
        MAX_MANIFEST_BYTES,
    )?;
    let manifest_source = std::str::from_utf8(&manifest_bytes).context("manifest is not UTF-8")?;
    let manifest = PluginManifest::from_toml(manifest_source)?;
    if manifest.plugin.source != arguments.source || manifest.plugin.version != arguments.version {
        bail!("package manifest identity does not match the installer-owned source and version");
    }
    let entrypoint = match &arguments.appearance_provider {
        Some(provider_id) => {
            &manifest
                .appearance_providers
                .iter()
                .find(|provider| provider.id == *provider_id)
                .with_context(|| {
                    format!("manifest does not declare appearance provider {provider_id}")
                })?
                .entrypoint
        }
        None => match &manifest.runtime {
            RuntimeSpec::Component { entrypoint, .. } => entrypoint,
            RuntimeSpec::Native { .. } => {
                bail!("the component supervisor cannot launch a native package")
            }
        },
    };
    let component_source = open_beneath(root_descriptor.as_raw_fd(), Path::new(entrypoint))?;
    let component_artifact = seal_verified_component(component_source, &arguments.digest)?;
    let manifest_artifact = seal_bytes("touchbar-manifest", &manifest_bytes)?;
    let asset_artifact = arguments
        .appearance_provider
        .is_none()
        .then(|| {
            seal_verified_assets(
                root_descriptor.as_raw_fd(),
                &manifest,
                &arguments.asset_digests,
            )
        })
        .transpose()?
        .flatten();
    let registry = CapabilityRegistry::default();
    let mut requests = registry
        .normalize(&manifest)
        .map_err(|errors| anyhow::anyhow!(format_policy_errors(&errors)))?;
    project_requests_for_worker(
        &mut requests,
        &manifest,
        arguments.appearance_provider.as_deref(),
    )?;
    let package = PackageInstance {
        source: arguments.source.clone(),
        version: arguments.version.clone(),
        digest: arguments
            .package_digest
            .clone()
            .unwrap_or_else(|| arguments.digest.clone()),
        provenance: arguments.provenance,
        runtime: RuntimeKind::Component,
    };
    let (visual_backends, appearance_backend) = match &arguments.appearance_provider {
        Some(provider) => (
            None,
            Some(Arc::new(AppearanceProviderBackend::new(
                provider,
                appearance_sink_from_environment()?,
            ))),
        ),
        None => {
            if env::var_os(APPEARANCE_SINK_FD_ENV).is_some() {
                bail!("appearance sink was supplied for a visual component");
            }
            (
                Some(VisualBackends::new(
                    &arguments.state,
                    &package.source.to_string(),
                )?),
                None,
            )
        }
    };
    let audit_file = arguments
        .audit
        .as_ref()
        .map(|path| AuditFile::new(path, AuditFileLimits::default()))
        .transpose()?;

    if arguments.grants.is_none() && arguments.session_grants.is_none() {
        let policy = effective_policy(
            &package,
            &requests,
            &GrantStore::default(),
            &SessionGrants::default(),
            &registry,
        );
        if policy.blocked {
            bail!("required component capabilities are not granted; host was not executed");
        }
        return match run_session(
            &arguments,
            &root,
            &package,
            policy,
            1,
            None,
            &requests,
            &registry,
            audit_file.as_ref(),
            &component_artifact,
            &manifest_artifact,
            asset_artifact.as_ref(),
            visual_backends.as_ref(),
            appearance_backend.as_ref(),
        )? {
            SessionOutcome::HostExited(code) => Ok(code),
            SessionOutcome::PolicyBlocked => {
                bail!("component policy became blocked without a grant watcher")
            }
        };
    }

    let first_path = arguments
        .grants
        .as_deref()
        .or(arguments.session_grants.as_deref())
        .expect("at least one grant store was checked above");
    let mut watcher = GrantStoreWatcher::new(first_path)
        .with_context(|| format!("watch grant store {}", first_path.display()))?;
    if let Some(session_path) = arguments.session_grants.as_deref()
        && session_path != first_path
    {
        watcher
            .add(session_path)
            .with_context(|| format!("watch session grant store {}", session_path.display()))?;
    }
    let mut instance_id = 1_u64;
    loop {
        let grants = load_optional_grants(arguments.grants.as_deref());
        let session_grants = load_optional_session_grants(arguments.session_grants.as_deref());
        let policy = effective_policy(&package, &requests, &grants, &session_grants, &registry);
        if policy.blocked {
            eprintln!(
                "touchbar-plugin-supervisor: required capabilities are blocked; waiting for grant changes"
            );
            watcher.wait_for_change(None)?;
            continue;
        }
        match run_session(
            &arguments,
            &root,
            &package,
            policy,
            instance_id,
            Some((
                &mut watcher,
                arguments.grants.as_deref(),
                arguments.session_grants.as_deref(),
            )),
            &requests,
            &registry,
            audit_file.as_ref(),
            &component_artifact,
            &manifest_artifact,
            asset_artifact.as_ref(),
            visual_backends.as_ref(),
            appearance_backend.as_ref(),
        )? {
            SessionOutcome::HostExited(code) => return Ok(code),
            SessionOutcome::PolicyBlocked => {
                instance_id = instance_id
                    .checked_add(1)
                    .context("plugin instance identifier exhausted")?;
            }
        }
    }
}

/// Each component world is a separate least-authority principal. Visual
/// workers never inherit provider authority, and an appearance worker sees
/// only its own provider ID and declared logical mounts.
fn project_requests_for_worker(
    requests: &mut Vec<CapabilityRequest>,
    manifest: &PluginManifest,
    appearance_provider: Option<&str>,
) -> Result<()> {
    let Some(provider_id) = appearance_provider else {
        requests.retain(|request| request.capability != CapabilityId::AppearanceProvideV1);
        return Ok(());
    };
    let provider = manifest
        .appearance_providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .with_context(|| format!("manifest does not declare appearance provider {provider_id}"))?;
    let mounts = provider.mounts.iter().collect::<BTreeSet<_>>();
    requests.retain_mut(|request| {
        let CapabilityScope::AppearanceProvide(scope) = &mut request.scope else {
            return false;
        };
        scope.providers.retain(|id| id == provider_id);
        scope.mounts.retain(|mount| mounts.contains(&mount.label));
        true
    });
    if requests.len() != 1 {
        bail!("appearance provider has no projected appearance.provide.v1 request");
    }
    Ok(())
}

fn harden_supervisor_process() -> Result<()> {
    // The supervisor handles brokered secret and clipboard payloads. Prevent
    // ordinary same-user process reads through proc/ptrace and prohibit core
    // files before any package or desktop-service data enters memory.
    // SAFETY: prctl takes scalar arguments and retains no pointers.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("disable supervisor dumpability");
    }
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: no_core is readable and RLIMIT_CORE accepts this pair.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) } != 0 {
        return Err(std::io::Error::last_os_error()).context("disable supervisor core dumps");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    arguments: &Arguments,
    root: &Path,
    package: &PackageInstance,
    policy: EffectivePolicy,
    instance_id: u64,
    watcher: Option<(&mut GrantStoreWatcher, Option<&Path>, Option<&Path>)>,
    requests: &[CapabilityRequest],
    registry: &CapabilityRegistry,
    audit_file: Option<&AuditFile>,
    component_artifact: &OwnedFd,
    manifest_artifact: &OwnedFd,
    asset_artifact: Option<&OwnedFd>,
    visual_backends: Option<&VisualBackends>,
    appearance_backend: Option<&Arc<AppearanceProviderBackend>>,
) -> Result<SessionOutcome> {
    let (host_channel, supervisor_channel) = Seqpacket::pair()?;
    let identity = ConnectionIdentity {
        instance_id,
        package: package.clone(),
    };
    println!(
        "supervisor: source={} version={} digest={} provenance={} instance={}",
        identity.package.source,
        identity.package.version,
        identity.package.digest,
        match identity.package.provenance {
            Provenance::VerifiedRelease => "verified-release",
            Provenance::UnverifiedRelease => "unverified-release",
            Provenance::LocalDevelopment => "local-development",
        },
        identity.instance_id
    );
    println!(
        "supervisor: policy blocked={} capabilities={}",
        policy.blocked,
        policy.grants.len()
    );

    let inherited_descriptor = duplicate_for_child(host_channel.as_raw_fd(), 10)?;
    let inherited_component = duplicate_for_child(component_artifact.as_raw_fd(), 10)?;
    let inherited_manifest = duplicate_for_child(manifest_artifact.as_raw_fd(), 10)?;
    let inherited_assets = asset_artifact
        .as_ref()
        .map(|assets| duplicate_for_child(assets.as_raw_fd(), 10))
        .transpose()?;
    let component_cgroup =
        ComponentCgroup::create(instance_id).context("create mandatory component-host cgroup")?;
    let component_tasks = component_task_limit(32)?;
    let broker_source = inherited_descriptor.as_raw_fd();
    let component_source = inherited_component.as_raw_fd();
    let manifest_source = inherited_manifest.as_raw_fd();
    let asset_source = inherited_assets.as_ref().map(AsRawFd::as_raw_fd);
    let cgroup_processes = component_cgroup.processes_fd();
    // SAFETY: getpid takes no arguments.
    let supervisor_pid = unsafe { libc::getpid() };
    let runtime_directory = env::var_os("XDG_RUNTIME_DIR");
    let wayland_display = env::var_os("WAYLAND_DISPLAY");
    let mut command = Command::new(&arguments.host);
    command
        .arg(root)
        .env_clear()
        .env(BROKER_FD_ENV, BROKER_FD.to_string())
        .env(COMPONENT_FD_ENV, COMPONENT_FD.to_string())
        .env(MANIFEST_FD_ENV, MANIFEST_FD.to_string())
        .env("MESA_SHADER_CACHE_DISABLE", "true");
    if let Some(provider) = &arguments.appearance_provider {
        command.args(["--appearance-provider-worker", provider]);
    } else {
        command.args(&arguments.host_arguments);
    }
    if asset_source.is_some() {
        command.env(ASSET_BUNDLE_FD_ENV, ASSET_BUNDLE_FD.to_string());
    }
    if let Some(runtime_directory) = runtime_directory {
        command.env("XDG_RUNTIME_DIR", runtime_directory);
    }
    if let Some(wayland_display) = wayland_display {
        command.env("WAYLAND_DISPLAY", wayland_display);
    }
    // SAFETY: the closure calls only async-signal-safe descriptor operations
    // and returns before the child execs the trusted component host.
    unsafe {
        command.pre_exec(move || {
            let task_limit = libc::rlimit {
                rlim_cur: component_tasks,
                rlim_max: component_tasks,
            };
            if libc::setrlimit(libc::RLIMIT_NPROC, &task_limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let address_space = libc::rlimit {
                rlim_cur: 8 * 1024 * 1024 * 1024,
                rlim_max: 8 * 1024 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &address_space) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != supervisor_pid {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            let current = b"0";
            let moved = libc::write(cgroup_processes, current.as_ptr().cast(), current.len());
            if moved != current.len() as isize {
                return Err(if moved < 0 {
                    std::io::Error::last_os_error()
                } else {
                    std::io::Error::from_raw_os_error(libc::EIO)
                });
            }
            for (source, target) in [
                (broker_source, BROKER_FD),
                (component_source, COMPONENT_FD),
                (manifest_source, MANIFEST_FD),
            ] {
                if libc::dup2(source, target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(source) = asset_source
                && libc::dup2(source, ASSET_BUNDLE_FD) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if asset_source.is_none() {
                libc::close(ASSET_BUNDLE_FD);
            }
            if libc::syscall(libc::SYS_close_range, 7_u32, u32::MAX, 0_u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("launch component host {}", arguments.host.display()))?;
    // Keep the leaf alive until the host has been reaped. Its Drop path issues
    // cgroup.kill before removal, covering abnormal child/thread teardown.
    let _component_cgroup = component_cgroup;
    drop((
        inherited_descriptor,
        inherited_component,
        inherited_manifest,
        inherited_assets,
    ));
    drop(host_channel);

    let mut connection = SupervisorConnection::new(
        supervisor_channel,
        identity,
        policy,
        ConnectionLimits::default(),
    )?;
    if let Some(audit_file) = audit_file {
        connection.set_audit_file(audit_file.clone());
    }
    if let Some(backend) = appearance_backend {
        connection.register_backend(CapabilityId::AppearanceProvideV1, backend.clone());
    } else {
        let visual = visual_backends.context("visual worker backends are unavailable")?;
        connection.register_backend(
            CapabilityId::DbusCallV1,
            Arc::new(DbusCallBackend::new(ZbusTransport::new())),
        );
        let context_backend = Arc::new(ContextReadBackend::with_hyprland_source());
        connection.register_backend(CapabilityId::ContextReadV1, context_backend.clone());
        connection.register_resource_backend(CapabilityId::ContextReadV1, context_backend);
        let filesystem_backend = Arc::new(FilesystemReadBackend::new());
        connection.register_backend(CapabilityId::FilesystemReadV1, filesystem_backend.clone());
        connection.register_resource_backend(CapabilityId::FilesystemReadV1, filesystem_backend);
        connection.register_backend(
            CapabilityId::FilesystemWriteV1,
            visual.filesystem_write.clone(),
        );
        connection.register_resource_backend(
            CapabilityId::FilesystemWriteV1,
            visual.filesystem_write.clone(),
        );
        let http_backend = Arc::new(HttpRequestBackend::new());
        connection.register_backend(CapabilityId::HttpRequestV1, http_backend.clone());
        connection.register_resource_backend(CapabilityId::HttpRequestV1, http_backend);
        connection.register_resource_backend(
            CapabilityId::DbusSubscribeV1,
            Arc::new(DbusSubscriptionBackend::new(ZbusTransport::new())),
        );
        connection.register_resource_backend(
            CapabilityId::CommandRunV1,
            Arc::new(CommandRunBackend::new(root.to_owned())),
        );
        connection.register_backend(
            CapabilityId::UriOpenV1,
            Arc::new(UriOpenBackend::new(ZbusDesktopPortal::new())),
        );
        connection.register_backend(
            CapabilityId::NotificationSendV1,
            visual.notification.clone(),
        );
        connection.register_backend(
            CapabilityId::SecretReadV1,
            Arc::new(SecretReadBackend::new(ZbusSecretService::new())),
        );
        connection.register_backend(CapabilityId::LocalConnectV1, visual.local_ipc.clone());
        connection
            .register_resource_backend(CapabilityId::LocalConnectV1, visual.local_ipc.clone());
        connection.register_backend(CapabilityId::ClipboardReadV1, visual.clipboard.clone());
        connection.register_backend(CapabilityId::ClipboardWriteV1, visual.clipboard.clone());
    }
    let exit = match watcher {
        Some((watcher, grants_path, session_grants_path)) => {
            let update_fd = watcher.as_raw_fd();
            let mut reload = || {
                if !watcher.drain_changes().map_err(TransportError::Io)? {
                    return Ok(None);
                }
                let grants = load_optional_grants(grants_path);
                let session_grants = load_optional_session_grants(session_grants_path);
                Ok(Some(effective_policy(
                    package,
                    requests,
                    &grants,
                    &session_grants,
                    registry,
                )))
            };
            connection.serve_with_policy_updates(update_fd, &mut reload)
        }
        None => connection
            .serve_until_disconnect()
            .map(|()| ConnectionExit::HostDisconnected),
    };
    let exit = match exit {
        Ok(exit) => exit,
        Err(TransportError::Disconnected) => ConnectionExit::HostDisconnected,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    if exit == ConnectionExit::PolicyBlocked {
        let _ = child.kill();
    }
    let status = child.wait().context("wait for component host")?;
    if exit == ConnectionExit::PolicyBlocked {
        Ok(SessionOutcome::PolicyBlocked)
    } else {
        Ok(SessionOutcome::HostExited(exit_code(status.code())))
    }
}

fn effective_policy(
    package: &PackageInstance,
    requests: &[CapabilityRequest],
    grants: &GrantStore,
    session_grants: &SessionGrants,
    registry: &CapabilityRegistry,
) -> EffectivePolicy {
    calculate_effective_policy(package, requests, grants, session_grants, registry)
}

fn load_optional_grants(path: Option<&Path>) -> GrantStore {
    path.map(load_grants_fail_closed).unwrap_or_default()
}

fn load_optional_session_grants(path: Option<&Path>) -> SessionGrants {
    let store = load_optional_grants(path);
    match SessionGrants::from_records(store.records()) {
        Ok(grants) => grants,
        Err(error) => {
            eprintln!("touchbar-plugin-supervisor: ignoring invalid session grants: {error}");
            SessionGrants::default()
        }
    }
}

fn load_grants_fail_closed(path: &Path) -> GrantStore {
    match GrantStore::load(path) {
        Ok(grants) => grants,
        Err(error) => {
            eprintln!(
                "touchbar-plugin-supervisor: ignoring invalid grant store {}: {error}",
                path.display()
            );
            GrantStore::default()
        }
    }
}

fn exit_code(code: Option<i32>) -> u8 {
    code.unwrap_or(1).clamp(0, u8::MAX as i32) as u8
}

fn parse_arguments() -> Result<Arguments> {
    let mut values = env::args().skip(1);
    let package = PathBuf::from(values.next().context(USAGE)?);
    let mut host = None;
    let mut source = None;
    let mut version = None;
    let mut digest = None;
    let mut package_digest = None;
    let mut appearance_provider = None;
    let mut asset_digests = Vec::new();
    let mut provenance = None;
    let mut state = None;
    let mut grants = None;
    let mut session_grants = None;
    let mut audit = None;
    let mut host_arguments = Vec::new();
    while let Some(argument) = values.next() {
        match argument.as_str() {
            "--host" => {
                host = Some(PathBuf::from(
                    values.next().context("--host requires PATH")?,
                ))
            }
            "--source" => {
                source = Some(
                    values
                        .next()
                        .context("--source requires VALUE")?
                        .parse::<GithubSource>()
                        .map_err(anyhow::Error::msg)?,
                )
            }
            "--version" => {
                version = Some(
                    values
                        .next()
                        .context("--version requires VALUE")?
                        .parse::<Version>()
                        .context("--version must be semantic version")?,
                )
            }
            "--digest" => digest = Some(values.next().context("--digest requires VALUE")?),
            "--package-digest" => {
                package_digest = Some(values.next().context("--package-digest requires VALUE")?)
            }
            "--appearance-provider" => {
                appearance_provider =
                    Some(values.next().context("--appearance-provider requires ID")?)
            }
            "--asset-digest" => asset_digests.push(
                values
                    .next()
                    .context("--asset-digest requires ID=sha256:HEX")?,
            ),
            "--provenance" => {
                provenance = Some(
                    match values
                        .next()
                        .context("--provenance requires VALUE")?
                        .as_str()
                    {
                        "verified-release" => Provenance::VerifiedRelease,
                        "unverified-release" => Provenance::UnverifiedRelease,
                        "local-development" => Provenance::LocalDevelopment,
                        _ => bail!(
                            "--provenance must be verified-release, unverified-release, or local-development"
                        ),
                    },
                )
            }
            "--state" => {
                state = Some(PathBuf::from(
                    values.next().context("--state requires DIRECTORY")?,
                ))
            }
            "--grants" => {
                grants = Some(PathBuf::from(
                    values.next().context("--grants requires FILE")?,
                ))
            }
            "--session-grants" => {
                session_grants = Some(PathBuf::from(
                    values.next().context("--session-grants requires FILE")?,
                ))
            }
            "--audit" => {
                audit = Some(PathBuf::from(
                    values.next().context("--audit requires FILE")?,
                ))
            }
            "--" => {
                host_arguments.extend(values);
                break;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown option {other}\n{USAGE}"),
        }
    }
    Ok(Arguments {
        package,
        host: host.context("--host is required")?,
        source: source.context("--source is required")?,
        version: version.context("--version is required")?,
        digest: digest.context("--digest is required")?,
        package_digest,
        appearance_provider,
        asset_digests,
        provenance: provenance.context("--provenance is required")?,
        state: state.context("--state is required")?,
        grants,
        session_grants,
        audit,
        host_arguments,
    })
}

fn appearance_sink_from_environment() -> Result<Seqpacket> {
    let value = env::var_os(APPEARANCE_SINK_FD_ENV)
        .context("appearance provider requires an inherited publication channel")?;
    let descriptor = value
        .to_str()
        .context("appearance sink descriptor is not UTF-8")?
        .parse::<i32>()
        .context("appearance sink descriptor is not an integer")?;
    if descriptor != APPEARANCE_SINK_FD {
        bail!("appearance sink descriptor is not in its fixed inherited slot");
    }
    // SAFETY: sessiond transfers ownership of this fixed inherited descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    Seqpacket::try_from_owned_fd(descriptor).context("validate appearance publication channel")
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn open_package_root(path: &Path) -> Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).context("package path contains NUL")?;
    // SAFETY: path is a live nul-terminated string and these flags take no mode.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    owned_fd(descriptor).context("open canonical package root")
}

fn open_beneath(root: i32, path: &Path) -> Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).context("package path contains NUL")?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: 0x01 | 0x02 | 0x04 | 0x08,
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
    let descriptor = i32::try_from(descriptor).context("openat2 descriptor overflow")?;
    owned_fd(descriptor).with_context(|| format!("open package member {}", path.to_string_lossy()))
}

fn read_beneath(root: i32, path: &Path, maximum_bytes: usize) -> Result<Vec<u8>> {
    let descriptor = open_beneath(root, path)?;
    let mut file = fs::File::from(descriptor);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(maximum_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum_bytes {
        bail!("package member exceeds its size limit");
    }
    Ok(bytes)
}

fn seal_verified_component(source: OwnedFd, expected_digest: &str) -> Result<OwnedFd> {
    let metadata = descriptor_metadata(source.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_size < 0
        || metadata.st_size as u64 > MAX_COMPONENT_BYTES
    {
        bail!("component artifact is not a bounded regular file");
    }
    let name = CString::new("touchbar-component").unwrap();
    // SAFETY: name is nul-terminated and memfd_create retains no pointer.
    let sealed =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    let sealed = owned_fd(sealed).context("create verified component memory file")?;
    let mut source = fs::File::from(source);
    let mut destination = fs::File::from(sealed);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("component artifact size overflow")?;
        if total > MAX_COMPONENT_BYTES {
            bail!("component artifact exceeds its size limit");
        }
        hasher.update(&buffer[..read]);
        destination.write_all(&buffer[..read])?;
    }
    let actual_digest = format!("sha256:{:x}", hasher.finalize());
    if actual_digest != expected_digest {
        bail!("component artifact digest does not match the installer lock");
    }
    seal_file(&destination)?;
    destination.seek(std::io::SeekFrom::Start(0))?;
    Ok(destination.into())
}

fn seal_verified_assets(
    root: i32,
    manifest: &PluginManifest,
    supplied: &[String],
) -> Result<Option<OwnedFd>> {
    let mut expected = BTreeMap::new();
    for value in supplied {
        let (id, digest) = value
            .split_once('=')
            .context("--asset-digest must be ID=sha256:HEX")?;
        validate_digest(digest)?;
        if expected.insert(id, digest).is_some() {
            bail!("duplicate --asset-digest ID `{id}`");
        }
    }
    if expected.len() != manifest.assets.len()
        || manifest
            .assets
            .iter()
            .any(|asset| !expected.contains_key(asset.id.as_str()))
    {
        bail!("asset digests do not exactly match the sealed package manifest");
    }
    if manifest.assets.is_empty() {
        return Ok(None);
    }

    let mut assets = Vec::with_capacity(manifest.assets.len());
    for asset in &manifest.assets {
        let descriptor = open_beneath(root, Path::new(&asset.path))?;
        let bytes =
            read_verified_asset(descriptor, expected[asset.id.as_str()], asset.id.as_str())?;
        assets.push((asset.id.as_str(), bytes));
    }
    let bundle = encode_asset_bundle(assets.iter().map(|(id, bytes)| (*id, bytes.as_slice())))?;
    Ok(Some(seal_bytes("touchbar-assets", &bundle)?))
}

fn read_verified_asset(source: OwnedFd, expected_digest: &str, id: &str) -> Result<Vec<u8>> {
    let metadata = descriptor_metadata(source.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_nlink != 1
        || metadata.st_size < 0
        || metadata.st_size as usize > MAX_ASSET_FILE_BYTES
    {
        bail!("asset `{id}` is not a bounded single-link regular file");
    }
    let mut file = fs::File::from(source);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_ASSET_FILE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ASSET_FILE_BYTES {
        bail!("asset `{id}` exceeds its encoded size limit");
    }
    let actual_digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    if actual_digest != expected_digest {
        bail!("asset `{id}` digest does not match the installer lock");
    }
    Ok(bytes)
}

fn seal_bytes(name: &str, bytes: &[u8]) -> Result<OwnedFd> {
    let name = CString::new(name).context("memory file name contains NUL")?;
    // SAFETY: name is nul-terminated and memfd_create retains no pointer.
    let descriptor =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    let descriptor = owned_fd(descriptor).context("create verified manifest memory file")?;
    let mut file = fs::File::from(descriptor);
    file.write_all(bytes)?;
    seal_file(&file)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(file.into())
}

fn seal_file(file: &fs::File) -> Result<()> {
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: fcntl operates on a live descriptor with an integer seal mask.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(std::io::Error::last_os_error()).context("seal verified package member");
    }
    Ok(())
}

fn descriptor_metadata(descriptor: i32) -> Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata provides sufficient writable storage and descriptor is live.
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("inspect package member");
    }
    // SAFETY: fstat succeeded and initialized metadata.
    Ok(unsafe { metadata.assume_init() })
}

fn duplicate_for_child(descriptor: i32, minimum: i32) -> Result<OwnedFd> {
    // SAFETY: fcntl duplicates a live descriptor and returns a new owned descriptor.
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, minimum) };
    owned_fd(duplicate).context("duplicate inherited child descriptor")
}

fn owned_fd(descriptor: i32) -> Result<OwnedFd> {
    if descriptor < 0 {
        Err(std::io::Error::last_os_error()).context("descriptor operation failed")
    } else {
        // SAFETY: the successful syscall returned one uniquely owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn validate_digest(digest: &str) -> Result<()> {
    let valid = digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    if !valid {
        bail!("--digest must be sha256 followed by 64 lowercase hexadecimal digits");
    }
    Ok(())
}

fn format_policy_errors(errors: &[touchbar_policy::NormalizationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker_manifest() -> PluginManifest {
        PluginManifest::from_toml(
            r#"manifest_version = 1
[plugin]
name = "Workers"
version = "1.0.0"
description = "Worker projection fixture"
license = "MIT"
source = "github:alice/workers"
api = "^1.0"
[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/plugin@1.0.0"
[[items]]
id = "main"
label = "Main"
[[appearance-provider]]
id = "first"
label = "First"
entrypoint = "component/first.wasm"
world = "touchbar:plugin/appearance-provider@1.0.0"
desktop_sessions = ["example"]
mounts = ["first-state"]
[[appearance-provider]]
id = "second"
label = "Second"
entrypoint = "component/second.wasm"
world = "touchbar:plugin/appearance-provider@1.0.0"
desktop_sessions = ["example"]
mounts = ["second-state"]
[[permission]]
capability = "appearance.provide.v1"
required = true
reason = "Publish appearance"
[permission.scope]
providers = ["first", "second"]
maximum_file_bytes = 4096
maximum_updates_per_second = 2
[[permission.scope.mounts]]
label = "first-state"
[[permission.scope.mounts]]
label = "second-state"
"#,
        )
        .unwrap()
    }

    #[test]
    fn component_worlds_receive_disjoint_role_authority() {
        let manifest = worker_manifest();
        let registry = CapabilityRegistry::default();
        let all = registry.normalize(&manifest).unwrap();

        let mut visual = all.clone();
        project_requests_for_worker(&mut visual, &manifest, None).unwrap();
        assert!(visual.is_empty());

        let mut provider = all;
        project_requests_for_worker(&mut provider, &manifest, Some("first")).unwrap();
        assert_eq!(provider.len(), 1);
        let CapabilityScope::AppearanceProvide(scope) = &provider[0].scope else {
            panic!("wrong projected capability")
        };
        assert_eq!(scope.providers, BTreeSet::from(["first".into()]));
        assert_eq!(
            scope
                .mounts
                .iter()
                .map(|mount| mount.label.as_str())
                .collect::<Vec<_>>(),
            vec!["first-state"]
        );
    }
}
