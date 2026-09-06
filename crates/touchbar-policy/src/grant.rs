use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use touchbar_package::GithubSource;

use crate::capability::{
    CapabilityId, CapabilityRegistry, CapabilityRequest, CapabilityScope, CapabilityStatus,
    RuntimeKind, TrustSummary, summarize_trust, validate_capability_scope,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provenance {
    VerifiedRelease,
    UnverifiedRelease,
    LocalDevelopment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReusePolicy {
    VerifiedSameSource,
    ExactDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageInstance {
    pub source: GithubSource,
    pub version: Version,
    pub digest: String,
    pub provenance: Provenance,
    pub runtime: RuntimeKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SecretBinding {
    /// An installer/user-selected Secret Service item. The package only knows
    /// the logical name from its manifest; it never supplies this object path.
    SecretServiceItem { object_path: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LocalEndpointBinding {
    /// A pathname Unix stream socket. The eventual broker additionally
    /// requires the connected peer credentials to match the supervisor user.
    UnixStream { path: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ClipboardBinding {
    /// The exact desktop compositor socket selected by the installer or user.
    /// The package cannot supply or override this path.
    WaylandDataControl { socket: PathBuf },
}

/// One installer-selected directory, bound to the exact filesystem object
/// observed when consent was recorded. The supervisor resolves `path` without
/// following symlinks and requires this device/inode pair on every use.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemMountBinding {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

impl FilesystemMountBinding {
    pub fn from_directory(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().canonicalize()?;
        let encoded = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mount path contains NUL"))?;
        let how = OpenHow {
            flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: 0,
            resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
        };
        // SAFETY: all syscall arguments point to initialized storage of the
        // declared size. A successful descriptor is uniquely owned below.
        let descriptor = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                libc::AT_FDCWD,
                encoded.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        let descriptor =
            i32::try_from(descriptor).map_err(|_| io::Error::other("mount descriptor overflow"))?;
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat2 succeeded and returned one uniquely owned descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: metadata is writable and descriptor remains live.
        if unsafe { libc::fstat(descriptor.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstat succeeded.
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "filesystem mount binding is not a directory",
            ));
        }
        Ok(Self {
            path,
            device: metadata.st_dev,
            inode: metadata.st_ino,
        })
    }
}

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBindings {
    /// Installer/user-owned bindings for logical filesystem mount labels.
    /// Package manifests never contain these host paths.
    pub filesystem_mounts: BTreeMap<String, FilesystemMountBinding>,
    /// Installer/user-owned bindings from manifest-local secret names to exact
    /// desktop secret items. Secret values never enter the grant store.
    pub secrets: BTreeMap<String, SecretBinding>,
    /// Installer/user-owned bindings from manifest-local endpoint labels to
    /// exact host sockets. Manifest suggestions never become authority.
    pub local_endpoints: BTreeMap<String, LocalEndpointBinding>,
    /// Installer/user-owned authority for the desktop clipboard. Clipboard
    /// grants never resolve the ambient WAYLAND_DISPLAY from the package.
    pub clipboard: Option<ClipboardBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRecord {
    pub source: GithubSource,
    pub capability: CapabilityId,
    pub approved_scope: CapabilityScope,
    pub bindings: GrantBindings,
    pub decision: Decision,
    pub reuse: ReusePolicy,
    pub approved_version: Version,
    pub approved_digest: String,
}

impl GrantRecord {
    pub fn validate(&self) -> Result<(), String> {
        if !self.approved_scope.matches_capability(&self.capability) {
            return Err("approved scope does not match capability".into());
        }
        validate_capability_scope(&self.capability, &self.approved_scope).map_err(|errors| {
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })?;
        validate_digest(&self.approved_digest)?;
        self.validate_bindings()?;
        if self.reuse == ReusePolicy::VerifiedSameSource
            && matches!(self.capability, CapabilityId::Unknown(_))
        {
            return Err("unknown capabilities cannot use verified-source grant reuse".into());
        }
        Ok(())
    }

    fn validate_bindings(&self) -> Result<(), String> {
        if self.decision == Decision::Deny {
            return if self.bindings == GrantBindings::default() {
                Ok(())
            } else {
                Err("denied grants cannot retain authority bindings".into())
            };
        }

        let expected_filesystem = match &self.approved_scope {
            CapabilityScope::FilesystemRead(scope) => scope
                .mounts
                .iter()
                .map(|mount| mount.label.as_str())
                .collect::<BTreeSet<_>>(),
            CapabilityScope::FilesystemWrite(scope) => scope
                .mounts
                .iter()
                .map(|mount| mount.label.as_str())
                .collect::<BTreeSet<_>>(),
            CapabilityScope::CommandRun(scope) => scope
                .commands
                .iter()
                .flat_map(|command| &command.arguments)
                .filter_map(|argument| match argument {
                    crate::CommandArgument::ApprovedFile { mount, .. } => Some(mount.as_str()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>(),
            _ => BTreeSet::new(),
        };
        let actual_filesystem = self
            .bindings
            .filesystem_mounts
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if actual_filesystem != expected_filesystem {
            return Err(
                "filesystem grant bindings must exactly match approved mount labels".into(),
            );
        }
        for binding in self.bindings.filesystem_mounts.values() {
            if !normalized_absolute_path(&binding.path)
                || binding.path.as_os_str().as_encoded_bytes().contains(&0)
            {
                return Err("filesystem grant roots must be normalized absolute paths".into());
            }
        }

        let expected_secrets = match &self.approved_scope {
            CapabilityScope::SecretRead(scope) => scope
                .logical_names
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            _ => BTreeSet::new(),
        };
        let actual_secrets = self
            .bindings
            .secrets
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if actual_secrets != expected_secrets {
            return Err("secret grant bindings must exactly match approved logical names".into());
        }
        for binding in self.bindings.secrets.values() {
            match binding {
                SecretBinding::SecretServiceItem { object_path }
                    if object_path.len() <= 1024 && valid_dbus_object_path(object_path) => {}
                SecretBinding::SecretServiceItem { .. } => {
                    return Err("secret item bindings must be valid D-Bus object paths".into());
                }
            }
        }

        let expected_endpoints = match &self.approved_scope {
            CapabilityScope::LocalConnect(scope) => scope
                .endpoints
                .iter()
                .map(|endpoint| endpoint.label.as_str())
                .collect::<BTreeSet<_>>(),
            _ => BTreeSet::new(),
        };
        let actual_endpoints = self
            .bindings
            .local_endpoints
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if actual_endpoints != expected_endpoints {
            return Err("local endpoint bindings must exactly match approved labels".into());
        }
        for binding in self.bindings.local_endpoints.values() {
            match binding {
                LocalEndpointBinding::UnixStream { path }
                    if normalized_absolute_path(path)
                        && !path.as_os_str().as_encoded_bytes().contains(&0)
                        && path.as_os_str().as_encoded_bytes().len() <= 107 => {}
                LocalEndpointBinding::UnixStream { .. } => {
                    return Err("local endpoint bindings must use normalized absolute paths".into());
                }
            }
        }

        let expects_clipboard = matches!(
            self.capability,
            CapabilityId::ClipboardReadV1 | CapabilityId::ClipboardWriteV1
        );
        if self.bindings.clipboard.is_some() != expects_clipboard {
            return Err(
                "clipboard grant bindings must exist exactly for clipboard capabilities".into(),
            );
        }
        if let Some(ClipboardBinding::WaylandDataControl { socket }) = &self.bindings.clipboard
            && (!normalized_absolute_path(socket)
                || socket.as_os_str().as_encoded_bytes().contains(&0)
                || socket.as_os_str().as_encoded_bytes().len() > 107)
        {
            return Err("clipboard socket bindings must be normalized absolute paths".into());
        }
        Ok(())
    }
}

fn normalized_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn valid_dbus_object_path(value: &str) -> bool {
    if value == "/" {
        return true;
    }
    value.starts_with('/')
        && !value.ends_with('/')
        && value[1..].split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionGrants {
    records: BTreeMap<(GithubSource, CapabilityId), GrantRecord>,
}

impl SessionGrants {
    pub fn insert(&mut self, record: GrantRecord) -> Result<Option<GrantRecord>, String> {
        record.validate()?;
        Ok(self
            .records
            .insert((record.source.clone(), record.capability.clone()), record))
    }

    pub fn get(&self, source: &GithubSource, capability: &CapabilityId) -> Option<&GrantRecord> {
        self.records.get(&(source.clone(), capability.clone()))
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    pub fn from_records<'a>(
        records: impl IntoIterator<Item = &'a GrantRecord>,
    ) -> Result<Self, String> {
        let mut session = Self::default();
        for record in records {
            session.insert(record.clone())?;
        }
        Ok(session)
    }

    pub fn records(&self) -> impl Iterator<Item = &GrantRecord> {
        self.records.values()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveGrant {
    pub request: CapabilityRequest,
    pub status: CapabilityStatus,
    pub approved_scope: Option<CapabilityScope>,
    pub bindings: GrantBindings,
    pub from_session: bool,
}

impl EffectiveGrant {
    pub fn allows(&self) -> bool {
        self.status == CapabilityStatus::Granted
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivePolicy {
    pub grants: Vec<EffectiveGrant>,
    pub blocked: bool,
    pub trust: TrustSummary,
}

pub trait PersistentGrants {
    fn grant(&self, source: &GithubSource, capability: &CapabilityId) -> Option<&GrantRecord>;
}

pub fn calculate_effective_policy(
    package: &PackageInstance,
    requests: &[CapabilityRequest],
    persistent: &impl PersistentGrants,
    session: &SessionGrants,
    registry: &CapabilityRegistry,
) -> EffectivePolicy {
    let mut effective = Vec::with_capacity(requests.len());
    for request in requests {
        let (record, from_session) = session
            .get(&package.source, &request.capability)
            .map(|record| (Some(record), true))
            .unwrap_or_else(|| {
                (
                    persistent.grant(&package.source, &request.capability),
                    false,
                )
            });
        let status = if package.runtime == RuntimeKind::Native {
            CapabilityStatus::DisclosureOnly
        } else if !registry.supports(&request.capability) {
            CapabilityStatus::Unsupported
        } else {
            record.map_or(CapabilityStatus::NeedsConsent, |record| {
                status_for_record(package, request, record)
            })
        };
        effective.push(EffectiveGrant {
            request: request.clone(),
            status,
            approved_scope: record.map(|record| record.approved_scope.clone()),
            bindings: record
                .map(|record| record.bindings.clone())
                .unwrap_or_default(),
            from_session,
        });
    }
    let blocked = package.runtime == RuntimeKind::Component
        && effective
            .iter()
            .any(|grant| grant.request.required && !grant.allows());
    let trust = summarize_trust(
        package.runtime,
        effective
            .iter()
            .filter(|grant| grant.allows())
            .map(|grant| &grant.request),
    );
    EffectivePolicy {
        grants: effective,
        blocked,
        trust,
    }
}

fn status_for_record(
    package: &PackageInstance,
    request: &CapabilityRequest,
    record: &GrantRecord,
) -> CapabilityStatus {
    if record.decision == Decision::Deny {
        return CapabilityStatus::Denied;
    }
    let provenance_allows = match record.reuse {
        ReusePolicy::VerifiedSameSource => package.provenance == Provenance::VerifiedRelease,
        ReusePolicy::ExactDigest => package.digest == record.approved_digest,
    };
    if provenance_allows && request.scope.is_subset_of(&record.approved_scope) {
        CapabilityStatus::Granted
    } else {
        CapabilityStatus::NeedsConsent
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionChangeKind {
    Added,
    Removed,
    Expanded,
    Narrowed,
    RequirementChanged,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionChange {
    pub capability: CapabilityId,
    pub kind: PermissionChangeKind,
    pub old: Option<CapabilityRequest>,
    pub new: Option<CapabilityRequest>,
}

impl PermissionChange {
    pub fn requires_consent(&self) -> bool {
        matches!(
            self.kind,
            PermissionChangeKind::Added | PermissionChangeKind::Expanded
        )
    }
}

pub fn diff_permissions(
    old: &[CapabilityRequest],
    new: &[CapabilityRequest],
) -> Vec<PermissionChange> {
    let old = old
        .iter()
        .map(|request| (request.capability.clone(), request))
        .collect::<BTreeMap<_, _>>();
    let new = new
        .iter()
        .map(|request| (request.capability.clone(), request))
        .collect::<BTreeMap<_, _>>();
    let capabilities = old
        .keys()
        .chain(new.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    capabilities
        .into_iter()
        .map(|capability| {
            let old_request = old.get(&capability).copied();
            let new_request = new.get(&capability).copied();
            let kind = match (old_request, new_request) {
                (None, Some(_)) => PermissionChangeKind::Added,
                (Some(_), None) => PermissionChangeKind::Removed,
                (Some(old), Some(new)) => {
                    let new_subset = new.scope.is_subset_of(&old.scope);
                    let old_subset = old.scope.is_subset_of(&new.scope);
                    if !new_subset {
                        PermissionChangeKind::Expanded
                    } else if !old_subset {
                        PermissionChangeKind::Narrowed
                    } else if old.required != new.required {
                        PermissionChangeKind::RequirementChanged
                    } else {
                        PermissionChangeKind::Unchanged
                    }
                }
                (None, None) => unreachable!("capability came from one side of the union"),
            };
            PermissionChange {
                capability,
                kind,
                old: old_request.cloned(),
                new: new_request.cloned(),
            }
        })
        .collect()
}

fn validate_digest(digest: &str) -> Result<(), String> {
    let Some(value) = digest.strip_prefix("sha256:") else {
        return Err("artifact digest must use sha256:<64 lowercase hex>".into());
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("artifact digest must use sha256:<64 lowercase hex>".into());
    }
    Ok(())
}
