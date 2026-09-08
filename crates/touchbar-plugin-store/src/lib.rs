//! Local, content-addressed TouchBar plugin installation state.
//!
//! This crate performs no network or catalog access. It turns a validated
//! package into an immutable snapshot and records installer-owned origin and
//! release history needed for launch, update policy, and rollback.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use touchbar_package::{
    GithubSource, MANIFEST_FILE_NAME, MAX_TOUCHBAR_WIDTH, PluginManifest, RuntimeSpec,
    SUPPORTED_COMPONENT_WORLD, SUPPORTED_HOST_API_VERSION,
};
use touchbar_policy::{
    CapabilityRegistry, CapabilityRequest, PermissionChange, Provenance, diff_permissions,
};

const STORE_SCHEMA_VERSION: u32 = 1;
const ARCHIVE_MAGIC: &[u8; 8] = b"OTBPKG01";
const MAX_PACKAGE_FILES: usize = 1024;
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PACKAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LOCK_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_PATH_BYTES: usize = 4096;
const MAX_RELEASE_HISTORY: usize = 1024;
const MAX_RELEASES_PER_SOURCE: usize = 32;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledItem {
    pub id: String,
    pub label: String,
    pub width: u32,
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledProfile {
    pub id: String,
    pub label: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledPlugin {
    pub source: GithubSource,
    pub version: Version,
    pub package_digest: String,
    pub artifacts: BTreeMap<String, String>,
    pub enabled: bool,
    pub runtime: InstalledRuntime,
    pub origin: InstalledOrigin,
    pub items: Vec<InstalledItem>,
    #[serde(default)]
    pub profiles: Vec<InstalledProfile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InstalledOrigin {
    LocalDevelopment,
    GithubRelease {
        release_id: u64,
        tag: String,
        asset_id: u64,
        asset_name: String,
        asset_digest: String,
        immutable: bool,
        attested: bool,
    },
}

impl InstalledOrigin {
    pub fn provenance(&self) -> Provenance {
        match self {
            Self::LocalDevelopment => Provenance::LocalDevelopment,
            Self::GithubRelease {
                immutable: true, ..
            } => Provenance::VerifiedRelease,
            Self::GithubRelease {
                immutable: false, ..
            } => Provenance::UnverifiedRelease,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSnapshot {
    pub source: GithubSource,
    pub version: Version,
    pub package_digest: String,
    pub origin: InstalledOrigin,
}

#[derive(Clone, Debug)]
pub struct ReleaseInstall {
    pub installed: InstalledPlugin,
    pub permission_changes: Vec<PermissionChange>,
    pub disabled_for_consent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstalledRuntime {
    Component,
    Native,
}

#[derive(Clone, Debug)]
pub struct PackageInspection {
    pub root: PathBuf,
    pub manifest: PluginManifest,
    pub requests: Vec<CapabilityRequest>,
    pub package_digest: String,
    pub artifacts: BTreeMap<String, String>,
    members: Vec<PackageMember>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackageMember {
    relative: PathBuf,
    executable: bool,
    length: u64,
}

#[derive(Clone, Debug)]
pub struct StorePaths {
    pub root: PathBuf,
    pub packages: PathBuf,
    pub lock: PathBuf,
    pub grants: PathBuf,
    pub session_grants: PathBuf,
    pub state: PathBuf,
    pub audit: PathBuf,
    pub control: PathBuf,
}

impl StorePaths {
    pub fn discover() -> Result<Self> {
        if let Some(root) = env::var_os("TOUCHBAR_HOME") {
            return Ok(Self::under(PathBuf::from(root)));
        }
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
            .context("HOME or XDG_DATA_HOME is required")?;
        let mut paths = Self::under(data.join("touchbar"));
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is required for session-only plugin permissions")?;
        paths.session_grants = runtime.join("touchbar/session-permissions.toml");
        Ok(paths)
    }

    pub fn under(root: PathBuf) -> Self {
        Self {
            packages: root.join("packages"),
            lock: root.join("plugins.toml"),
            grants: root.join("permissions.toml"),
            session_grants: root.join("session-permissions.toml"),
            state: root.join("state"),
            audit: root.join("audit.jsonl"),
            control: root.join("control.sock"),
            root,
        }
    }

    pub fn package(&self, digest: &str) -> Result<PathBuf> {
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .context("invalid installed package digest")?;
        Ok(self.packages.join(hex))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreDocument {
    schema_version: u32,
    plugins: Vec<InstalledPlugin>,
    release_history: Vec<ReleaseSnapshot>,
}

pub struct PluginStore {
    paths: StorePaths,
    plugins: BTreeMap<GithubSource, InstalledPlugin>,
    release_history: Vec<ReleaseSnapshot>,
}

impl PluginStore {
    pub fn open(paths: StorePaths) -> Result<Self> {
        ensure_private_directory(&paths.root)?;
        ensure_private_directory(&paths.packages)?;
        ensure_private_directory(&paths.state)?;
        let document = read_document(&paths.lock)?;
        if document.release_history.len() > MAX_RELEASE_HISTORY {
            bail!("plugin release history exceeds its entry limit");
        }
        let mut plugins = BTreeMap::new();
        for plugin in document.plugins {
            validate_installed(&plugin)?;
            if plugins.insert(plugin.source.clone(), plugin).is_some() {
                bail!("plugin lock contains a duplicate source");
            }
        }
        for snapshot in &document.release_history {
            validate_release_snapshot(snapshot)?;
        }
        Ok(Self {
            paths,
            plugins,
            release_history: document.release_history,
        })
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    pub fn plugins(&self) -> impl Iterator<Item = &InstalledPlugin> {
        self.plugins.values()
    }

    pub fn get(&self, source: &GithubSource) -> Option<&InstalledPlugin> {
        self.plugins.get(source)
    }

    pub fn release_history<'a>(
        &'a self,
        source: &'a GithubSource,
    ) -> impl Iterator<Item = &'a ReleaseSnapshot> {
        self.release_history
            .iter()
            .filter(move |snapshot| snapshot.source == *source)
    }

    pub fn package_path(&self, plugin: &InstalledPlugin) -> Result<PathBuf> {
        self.paths.package(&plugin.package_digest)
    }

    pub fn install_directory(&mut self, source: impl AsRef<Path>) -> Result<InstalledPlugin> {
        Ok(self
            .install_directory_as(
                source.as_ref(),
                InstalledOrigin::LocalDevelopment,
                false,
                false,
            )?
            .installed)
    }

    fn install_directory_as(
        &mut self,
        source: &Path,
        origin: InstalledOrigin,
        record_release: bool,
        enforce_remote_update_policy: bool,
    ) -> Result<ReleaseInstall> {
        let inspection = inspect_package(source)?;
        validate_origin(&origin, &inspection.manifest.plugin.version)?;
        let destination = self.paths.package(&inspection.package_digest)?;
        if !destination.exists() {
            let temporary = temporary_path(&self.paths.packages, "install");
            ensure_private_directory(&temporary)?;
            let result = copy_members(&inspection, &temporary)
                .and_then(|_| inspect_package(&temporary))
                .and_then(|copied| {
                    if copied.package_digest != inspection.package_digest
                        || copied.artifacts != inspection.artifacts
                    {
                        bail!("package changed while it was being installed");
                    }
                    fs::rename(&temporary, &destination).with_context(|| {
                        format!("commit installed package {}", destination.display())
                    })?;
                    sync_directory(&self.paths.packages)
                });
            if result.is_err() {
                let _ = fs::remove_dir_all(&temporary);
            }
            result?;
        } else {
            let installed = inspect_package(&destination)?;
            if installed.package_digest != inspection.package_digest {
                bail!("content-addressed package directory is inconsistent");
            }
        }

        let previous = self.plugins.get(&inspection.manifest.plugin.source);
        let previous_requests = previous
            .map(|plugin| self.package_path(plugin))
            .transpose()?
            .map(|path| inspect_package(&path))
            .transpose()?
            .map(|package| package.requests)
            .unwrap_or_default();
        let permission_changes = diff_permissions(&previous_requests, &inspection.requests);
        let previous_enabled = previous.is_some_and(|plugin| plugin.enabled);
        let previous_items = previous
            .map(|plugin| {
                plugin
                    .items
                    .iter()
                    .map(|item| (item.id.as_str(), (item.enabled, item.width)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let previous_profiles = previous
            .map(|plugin| {
                plugin
                    .profiles
                    .iter()
                    .map(|profile| (profile.id.as_str(), profile.enabled))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let runtime = match inspection.manifest.runtime {
            RuntimeSpec::Component { .. } => InstalledRuntime::Component,
            RuntimeSpec::Native { .. } => InstalledRuntime::Native,
        };
        let package_changed = previous.is_some_and(|plugin| {
            plugin.package_digest != inspection.package_digest
                || plugin.version != inspection.manifest.plugin.version
        });
        let disabled_for_consent = enforce_remote_update_policy
            && previous_enabled
            && (permission_changes
                .iter()
                .any(PermissionChange::requires_consent)
                || (runtime == InstalledRuntime::Native && package_changed));
        let plugin = InstalledPlugin {
            source: inspection.manifest.plugin.source.clone(),
            version: inspection.manifest.plugin.version.clone(),
            package_digest: inspection.package_digest,
            artifacts: inspection.artifacts,
            enabled: previous_enabled && !disabled_for_consent,
            runtime,
            origin: origin.clone(),
            items: inspection
                .manifest
                .items
                .into_iter()
                .map(|item| {
                    let (enabled, width) = previous_items
                        .get(item.id.as_str())
                        .copied()
                        .unwrap_or((true, item.default_width));
                    InstalledItem {
                        id: item.id,
                        label: item.label,
                        width,
                        enabled,
                    }
                })
                .collect(),
            profiles: inspection
                .manifest
                .profiles
                .into_iter()
                .map(|profile| InstalledProfile {
                    enabled: previous_profiles
                        .get(profile.id.as_str())
                        .copied()
                        .unwrap_or(profile.enabled_by_default),
                    id: profile.id,
                    label: profile.label,
                })
                .collect(),
        };
        self.plugins.insert(plugin.source.clone(), plugin.clone());
        if record_release {
            self.record_release(ReleaseSnapshot {
                source: plugin.source.clone(),
                version: plugin.version.clone(),
                package_digest: plugin.package_digest.clone(),
                origin,
            });
        }
        self.save()?;
        Ok(ReleaseInstall {
            installed: plugin,
            permission_changes,
            disabled_for_consent,
        })
    }

    pub fn install_archive(&mut self, archive: impl AsRef<Path>) -> Result<InstalledPlugin> {
        let temporary = temporary_path(&self.paths.root, "archive");
        ensure_private_directory(&temporary)?;
        let result = unpack_archive(archive.as_ref(), &temporary)
            .and_then(|_| self.install_directory(&temporary));
        let _ = fs::remove_dir_all(&temporary);
        result
    }

    pub fn install_release_archive(
        &mut self,
        archive: impl AsRef<Path>,
        expected_source: &GithubSource,
        expected_version: &Version,
        origin: InstalledOrigin,
    ) -> Result<ReleaseInstall> {
        if !matches!(origin, InstalledOrigin::GithubRelease { .. }) {
            bail!("a release install requires GitHub release provenance");
        }
        let temporary = temporary_path(&self.paths.root, "release");
        ensure_private_directory(&temporary)?;
        let result = unpack_archive(archive.as_ref(), &temporary).and_then(|_| {
            let inspection = inspect_package(&temporary)?;
            if inspection.manifest.plugin.source != *expected_source
                || inspection.manifest.plugin.version != *expected_version
            {
                bail!("release package identity does not match the requested source and version");
            }
            self.install_directory_as(&temporary, origin, true, true)
        });
        let _ = fs::remove_dir_all(&temporary);
        result
    }

    pub fn rollback_release(
        &mut self,
        source: &GithubSource,
        version: Option<&Version>,
    ) -> Result<ReleaseInstall> {
        let current = self
            .plugins
            .get(source)
            .with_context(|| format!("plugin {source} is not installed"))?;
        let snapshot = self
            .release_history
            .iter()
            .rev()
            .find(|snapshot| {
                snapshot.source == *source
                    && snapshot.package_digest != current.package_digest
                    && version.is_none_or(|version| snapshot.version == *version)
            })
            .cloned()
            .with_context(|| match version {
                Some(version) => format!("no retained {source} release {version} is available"),
                None => format!("no previous retained release of {source} is available"),
            })?;
        let path = self.paths.package(&snapshot.package_digest)?;
        if !path.is_dir() {
            bail!("retained rollback package is missing")
        }
        self.install_directory_as(&path, snapshot.origin, true, true)
    }

    pub fn set_enabled(&mut self, source: &GithubSource, enabled: bool) -> Result<()> {
        let plugin = self
            .plugins
            .get_mut(source)
            .with_context(|| format!("plugin {source} is not installed"))?;
        plugin.enabled = enabled;
        self.save()
    }

    pub fn set_item_enabled(
        &mut self,
        source: &GithubSource,
        item_id: &str,
        enabled: bool,
    ) -> Result<()> {
        let plugin = self
            .plugins
            .get_mut(source)
            .with_context(|| format!("plugin {source} is not installed"))?;
        let item = plugin
            .items
            .iter_mut()
            .find(|item| item.id == item_id)
            .with_context(|| format!("plugin {source} has no item {item_id}"))?;
        item.enabled = enabled;
        self.save()
    }

    pub fn set_item_width(
        &mut self,
        source: &GithubSource,
        item_id: &str,
        width: u32,
    ) -> Result<()> {
        if !(1..=MAX_TOUCHBAR_WIDTH).contains(&width) {
            bail!("item width must be between 1 and {MAX_TOUCHBAR_WIDTH}");
        }
        let plugin = self
            .plugins
            .get_mut(source)
            .with_context(|| format!("plugin {source} is not installed"))?;
        let item = plugin
            .items
            .iter_mut()
            .find(|item| item.id == item_id)
            .with_context(|| format!("plugin {source} has no item {item_id}"))?;
        item.width = width;
        self.save()
    }

    pub fn set_profile_enabled(
        &mut self,
        source: &GithubSource,
        profile_id: &str,
        enabled: bool,
    ) -> Result<()> {
        let plugin = self
            .plugins
            .get_mut(source)
            .with_context(|| format!("plugin {source} is not installed"))?;
        let profile = plugin
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
            .with_context(|| format!("plugin {source} has no profile {profile_id}"))?;
        profile.enabled = enabled;
        self.save()
    }

    pub fn remove(&mut self, source: &GithubSource) -> Result<bool> {
        let Some(removed) = self.plugins.remove(source) else {
            return Ok(false);
        };
        let mut removable = self
            .release_history
            .iter()
            .filter(|snapshot| snapshot.source == *source)
            .map(|snapshot| snapshot.package_digest.clone())
            .collect::<BTreeSet<_>>();
        removable.insert(removed.package_digest);
        self.release_history
            .retain(|snapshot| snapshot.source != *source);
        self.save()?;
        for digest in removable {
            let referenced = self
                .plugins
                .values()
                .any(|plugin| plugin.package_digest == digest)
                || self
                    .release_history
                    .iter()
                    .any(|snapshot| snapshot.package_digest == digest);
            if !referenced {
                let package = self.paths.package(&digest)?;
                if package.exists() {
                    fs::remove_dir_all(&package)
                        .with_context(|| format!("remove package {}", package.display()))?;
                }
            }
        }
        sync_directory(&self.paths.packages)?;
        Ok(true)
    }

    fn record_release(&mut self, snapshot: ReleaseSnapshot) {
        if self.release_history.last() == Some(&snapshot) {
            return;
        }
        self.release_history.push(snapshot.clone());
        let mut matching = self
            .release_history
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.source == snapshot.source)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        while matching.len() > MAX_RELEASES_PER_SOURCE {
            self.release_history.remove(matching.remove(0));
            matching = self
                .release_history
                .iter()
                .enumerate()
                .filter(|(_, candidate)| candidate.source == snapshot.source)
                .map(|(index, _)| index)
                .collect();
        }
        while self.release_history.len() > MAX_RELEASE_HISTORY {
            self.release_history.remove(0);
        }
    }

    fn save(&self) -> Result<()> {
        let document = StoreDocument {
            schema_version: STORE_SCHEMA_VERSION,
            plugins: self.plugins.values().cloned().collect(),
            release_history: self.release_history.clone(),
        };
        let encoded = toml::to_string_pretty(&document)?;
        if encoded.len() as u64 > MAX_LOCK_BYTES {
            bail!("plugin lock exceeds its size limit");
        }
        atomic_private_write(&self.paths.lock, encoded.as_bytes())
    }
}

pub fn inspect_package(root: &Path) -> Result<PackageInspection> {
    let root = root
        .canonicalize()
        .with_context(|| format!("open package {}", root.display()))?;
    if !root.is_dir() {
        bail!("package path must be a directory");
    }
    let manifest_path = root.join(MANIFEST_FILE_NAME);
    let manifest_source = read_bounded_regular(&manifest_path, 1024 * 1024)?;
    let manifest = PluginManifest::from_toml(
        std::str::from_utf8(&manifest_source).context("plugin manifest is not UTF-8")?,
    )?;
    let host_api =
        Version::parse(SUPPORTED_HOST_API_VERSION).expect("compiled host API version is valid");
    if !manifest.supports_host_api(&host_api) {
        bail!(
            "plugin requires host API {}, but this host provides {host_api}",
            manifest.plugin.api
        );
    }
    let requests = CapabilityRegistry::default()
        .normalize(&manifest)
        .map_err(|errors| {
            anyhow::anyhow!(
                errors
                    .iter()
                    .map(|error| format!("{}: {}", error.field, error.message))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })?;
    let mut paths = BTreeSet::from([PathBuf::from(MANIFEST_FILE_NAME)]);
    for artifact in manifest.artifact_paths() {
        paths.insert(PathBuf::from(artifact));
    }
    for name in ["README.md", "LICENSE", "LICENSE.md", "LICENSE.txt"] {
        if root.join(name).exists() {
            paths.insert(PathBuf::from(name));
        }
    }
    for directory in ["assets", "licenses", "screenshots"] {
        let path = root.join(directory);
        if path.exists() {
            collect_directory_members(&root, &path, &mut paths)?;
        }
    }
    if paths.len() > MAX_PACKAGE_FILES {
        bail!("package contains too many files");
    }
    let executable_paths = match &manifest.runtime {
        RuntimeSpec::Component { .. } => BTreeSet::new(),
        RuntimeSpec::Native { targets } => targets
            .iter()
            .map(|target| PathBuf::from(&target.entrypoint))
            .collect(),
    };
    let mut members = Vec::new();
    let mut total = 0_u64;
    for relative in paths {
        validate_relative_path(&relative)?;
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect package member {}", relative.display()))?;
        if !metadata.file_type().is_file() || metadata.nlink() != 1 {
            bail!(
                "package member {} must be a single-link regular file",
                relative.display()
            );
        }
        if metadata.len() > MAX_PACKAGE_FILE_BYTES {
            bail!("package member {} exceeds 64 MiB", relative.display());
        }
        total = total
            .checked_add(metadata.len())
            .context("package size overflow")?;
        if total > MAX_PACKAGE_BYTES {
            bail!("package exceeds 256 MiB");
        }
        members.push(PackageMember {
            relative,
            executable: executable_paths.contains(&path.strip_prefix(&root).unwrap().to_path_buf()),
            length: metadata.len(),
        });
    }
    let package_digest = digest_members(&root, &members)?;
    let artifacts = manifest
        .artifact_paths()
        .map(|path| {
            let digest = digest_file(&root.join(path))?;
            Ok((path.to_owned(), digest))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(PackageInspection {
        root,
        manifest,
        requests,
        package_digest,
        artifacts,
        members,
    })
}

pub fn pack_directory(root: &Path, output: &Path) -> Result<PackageInspection> {
    let inspection = inspect_package(root)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = output.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("create package archive {}", temporary.display()))?;
    file.write_all(ARCHIVE_MAGIC)?;
    file.write_all(&(inspection.members.len() as u32).to_le_bytes())?;
    for member in &inspection.members {
        let encoded = member.relative.to_string_lossy();
        let length = u16::try_from(encoded.len()).context("archive path is too long")?;
        file.write_all(&length.to_le_bytes())?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(&[u8::from(member.executable)])?;
        file.write_all(&member.length.to_le_bytes())?;
        let mut source = open_regular_nofollow(&inspection.root.join(&member.relative))?;
        io::copy(&mut source, &mut file)?;
    }
    file.sync_all()?;
    fs::rename(&temporary, output)
        .with_context(|| format!("commit package archive {}", output.display()))?;
    if let Some(parent) = output.parent() {
        sync_directory(parent)?;
    }
    Ok(inspection)
}

fn unpack_archive(archive: &Path, destination: &Path) -> Result<()> {
    let mut file = open_regular_nofollow(archive)?;
    if file.metadata()?.len() > MAX_PACKAGE_BYTES + 8 * 1024 * 1024 {
        bail!("package archive exceeds its size limit");
    }
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != ARCHIVE_MAGIC {
        bail!("not a TouchBar package archive");
    }
    let count = read_u32(&mut file)? as usize;
    if count == 0 || count > MAX_PACKAGE_FILES {
        bail!("archive file count is invalid");
    }
    let mut seen = BTreeSet::new();
    let mut total = 0_u64;
    for _ in 0..count {
        let path_length = read_u16(&mut file)? as usize;
        if path_length == 0 || path_length > MAX_ARCHIVE_PATH_BYTES {
            bail!("archive path length is invalid");
        }
        let mut encoded = vec![0_u8; path_length];
        file.read_exact(&mut encoded)?;
        let relative =
            PathBuf::from(std::str::from_utf8(&encoded).context("archive path is not UTF-8")?);
        validate_relative_path(&relative)?;
        if !seen.insert(relative.clone()) {
            bail!("archive contains duplicate paths");
        }
        let mut executable = [0_u8; 1];
        file.read_exact(&mut executable)?;
        if executable[0] > 1 {
            bail!("archive executable flag is invalid");
        }
        let length = read_u64(&mut file)?;
        if length > MAX_PACKAGE_FILE_BYTES {
            bail!("archive member exceeds 64 MiB");
        }
        total = total.checked_add(length).context("archive size overflow")?;
        if total > MAX_PACKAGE_BYTES {
            bail!("archive expands beyond 256 MiB");
        }
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut target_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable[0] == 1 { 0o700 } else { 0o600 })
            .open(&target)?;
        let copied = io::copy(
            &mut std::io::Read::by_ref(&mut file).take(length),
            &mut target_file,
        )?;
        if copied != length {
            bail!("archive member is truncated");
        }
        target_file.sync_all()?;
    }
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        bail!("archive contains trailing data");
    }
    Ok(())
}

fn collect_directory_members(
    root: &Path,
    directory: &Path,
    output: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir() {
        bail!(
            "optional package asset root {} must be a directory",
            directory.display()
        );
    }
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!("package assets cannot contain symlinks");
        }
        if metadata.file_type().is_dir() {
            collect_directory_members(root, &path, output)?;
        } else if metadata.file_type().is_file() {
            output.insert(path.strip_prefix(root)?.to_path_buf());
        } else {
            bail!("package assets must contain only regular files and directories");
        }
        if output.len() > MAX_PACKAGE_FILES {
            bail!("package contains too many files");
        }
    }
    Ok(())
}

fn copy_members(inspection: &PackageInspection, destination: &Path) -> Result<()> {
    for member in &inspection.members {
        let target = destination.join(&member.relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut source = open_regular_nofollow(&inspection.root.join(&member.relative))?;
        let mut target_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if member.executable { 0o700 } else { 0o600 })
            .open(&target)?;
        let copied = io::copy(&mut source, &mut target_file)?;
        if copied != member.length {
            bail!(
                "package changed while member {} was copied",
                member.relative.display()
            );
        }
        target_file.sync_all()?;
    }
    Ok(())
}

fn digest_members(root: &Path, members: &[PackageMember]) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"touchbar-package-v1\0");
    for member in members {
        let path = member.relative.to_string_lossy();
        digest.update((path.len() as u64).to_le_bytes());
        digest.update(path.as_bytes());
        digest.update([u8::from(member.executable)]);
        digest.update(member.length.to_le_bytes());
        let mut file = open_regular_nofollow(&root.join(&member.relative))?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn digest_file(path: &Path) -> Result<String> {
    let mut file = open_regular_nofollow(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn read_bounded_regular(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = open_regular_nofollow(path)?;
    let length = file.metadata()?.len();
    if length > maximum {
        bail!("{} exceeds its size limit", path.display());
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        bail!("{} exceeds its size limit", path.display());
    }
    Ok(bytes)
}

fn open_regular_nofollow(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open package file {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        bail!("{} must be a single-link regular file", path.display());
    }
    Ok(file)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.as_os_str().as_encoded_bytes().len() > MAX_ARCHIVE_PATH_BYTES
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("package path must be normalized and relative");
    }
    path.to_str().context("package paths must be UTF-8")?;
    Ok(())
}

fn validate_installed(plugin: &InstalledPlugin) -> Result<()> {
    StorePaths::under(PathBuf::new()).package(&plugin.package_digest)?;
    validate_origin(&plugin.origin, &plugin.version)?;
    if plugin.items.is_empty() {
        bail!("installed plugin has no items");
    }
    let mut ids = BTreeSet::new();
    for item in &plugin.items {
        if !ids.insert(&item.id) || !(1..=MAX_TOUCHBAR_WIDTH).contains(&item.width) {
            bail!("installed plugin item state is invalid");
        }
    }
    let mut profile_ids = BTreeSet::new();
    for profile in &plugin.profiles {
        if profile.id.is_empty()
            || profile.label.trim().is_empty()
            || !profile_ids.insert(&profile.id)
        {
            bail!("installed plugin profile state is invalid");
        }
    }
    if plugin.artifacts.is_empty() {
        bail!("installed plugin has no artifact digests");
    }
    for (path, digest) in &plugin.artifacts {
        validate_relative_path(Path::new(path))?;
        StorePaths::under(PathBuf::new()).package(digest)?;
    }
    Ok(())
}

fn validate_release_snapshot(snapshot: &ReleaseSnapshot) -> Result<()> {
    StorePaths::under(PathBuf::new()).package(&snapshot.package_digest)?;
    if !matches!(snapshot.origin, InstalledOrigin::GithubRelease { .. }) {
        bail!("release history contains non-release provenance");
    }
    validate_origin(&snapshot.origin, &snapshot.version)
}

fn validate_origin(origin: &InstalledOrigin, version: &Version) -> Result<()> {
    let InstalledOrigin::GithubRelease {
        release_id,
        tag,
        asset_id,
        asset_name,
        asset_digest,
        ..
    } = origin
    else {
        return Ok(());
    };
    if *release_id == 0 || *asset_id == 0 {
        bail!("GitHub release and asset IDs must be nonzero");
    }
    if tag != &format!("v{version}") {
        bail!("GitHub release tag does not match the package version");
    }
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("stable GitHub releases require a plain semantic version");
    }
    if asset_name.is_empty()
        || asset_name.len() > 255
        || asset_name.bytes().any(|byte| byte.is_ascii_control())
        || Path::new(asset_name)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("GitHub release asset name is invalid");
    }
    StorePaths::under(PathBuf::new()).package(asset_digest)?;
    Ok(())
}

fn read_document(path: &Path) -> Result<StoreDocument> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(StoreDocument {
                schema_version: STORE_SCHEMA_VERSION,
                plugins: Vec::new(),
                release_history: Vec::new(),
            });
        }
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file.metadata()?;
    validate_private_file(path, &metadata)?;
    if metadata.len() > MAX_LOCK_BYTES {
        bail!("plugin lock exceeds its size limit");
    }
    let mut source = String::new();
    file.take(MAX_LOCK_BYTES + 1).read_to_string(&mut source)?;
    let document: StoreDocument = toml::from_str(&source)?;
    if document.schema_version != STORE_SCHEMA_VERSION {
        bail!("unsupported plugin lock schema");
    }
    Ok(document)
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!("{} must be a private user-owned directory", path.display());
    }
    Ok(())
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "{} must be a private single-link user-owned file",
            path.display()
        );
    }
    Ok(())
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("plugin lock has no parent")?;
    ensure_private_directory(parent)?;
    let temporary = temporary_path(parent, "lock");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(parent: &Path, kind: &str) -> PathBuf {
    parent.join(format!(
        ".{kind}-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn read_u16(reader: &mut impl Read) -> Result<u16> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub fn sdk_context_markdown() -> String {
    format!(
        concat!(
            "# TouchBar component context\n\n",
            "- Manifest: `{MANIFEST_FILE_NAME}` v1\n",
            "- Host API: `{SUPPORTED_HOST_API_VERSION}`\n",
            "- WIT world: `{SUPPORTED_COMPONENT_WORLD}`\n",
            "- Runtime: one sandboxed WebAssembly Component per supervised host\n",
            "- UI: retained, semantic, theme-token based; never hard-code a background-dependent foreground\n",
            "- Assets: declare bounded PNG or symbolic SVG files under `assets/`; render only by logical ID and use semantic mask/multiply tint\n",
            "- Presentations: declare package-local bars with container and per-item sizing; request them from input callbacks and handle compositor lifecycle events\n",
            "- Automatic profiles: declare package-local `[[profile]]` entries with exact lowercase `applications` and/or `activities`; use `show_in_default_profile = false` for contextual-only items; user rules retain precedence\n",
            "- Packages: stable item IDs, normalized relative artifact paths, no symlinks\n",
            "- Test widths: 80, 160, 320, 1004, and {MAX_TOUCHBAR_WIDTH} pixels at 60 pixels high plus every presentation width\n",
            "- Deterministic interaction: `touchbarctl plugin replay --scenario tests/interaction.json`; add `--screenshots DIR` for named GPU PNGs; use exact scope-checked D-Bus, HTTP, command, filesystem-read, local-service, notification, URI-open, clipboard, and secret-read fixtures for offline integration state; commit synthetic secret values only\n",
            "- Interactive development: `touchbarctl plugin dev` opens the whole pack in an isolated production-compositor desktop preview; simulator input is synthetic and cannot authorize OS actions\n",
            "- Local workflow: `touchbarctl plugin check`, `test --format json`, `replay`, `dev`, `pack`, `add --path`, then `enable`\n",
            "- Release asset: `touchbar-plugin.touchbar` on a canonical `vMAJOR.MINOR.PATCH` GitHub Release\n",
            "- User workflow: `touchbarctl plugin add github:owner/repository`, `update`, and offline `rollback`\n",
        ),
        MANIFEST_FILE_NAME = MANIFEST_FILE_NAME,
        SUPPORTED_HOST_API_VERSION = SUPPORTED_HOST_API_VERSION,
        SUPPORTED_COMPONENT_WORLD = SUPPORTED_COMPONENT_WORLD,
        MAX_TOUCHBAR_WIDTH = MAX_TOUCHBAR_WIDTH,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(root: &Path) {
        package_version(root, "0.1.0", false);
    }

    fn package_version(root: &Path, version: &str, permission: bool) {
        fs::create_dir_all(root.join("component")).unwrap();
        fs::write(
            root.join("component/plugin.wasm"),
            format!("component-{version}"),
        )
        .unwrap();
        let permission = if permission {
            r#"
[[permission]]
capability = "context.read.v1"
required = false
reason = "Adapt to the focused application"

[permission.scope]
facts = ["application.id"]
maximum_updates_per_second = 10
"#
        } else {
            ""
        };
        fs::write(
            root.join(MANIFEST_FILE_NAME),
            format!(
                r#"manifest_version = 1

[plugin]
name = "Test"
version = "{version}"
description = "Test plugin"
license = "MIT"
source = "github:alice/test"
api = "^1.0"

[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/plugin@1.0.0"

[[items]]
id = "test"
label = "Test"
{permission}"#
            ),
        )
        .unwrap();
    }

    fn add_symbolic_asset(root: &Path, contents: &[u8]) {
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets/mark.svg"), contents).unwrap();
        let manifest = fs::read_to_string(root.join(MANIFEST_FILE_NAME)).unwrap();
        fs::write(
            root.join(MANIFEST_FILE_NAME),
            format!(
                "{manifest}\n\n[[asset]]\nid = \"mark\"\npath = \"assets/mark.svg\"\nkind = \"symbolic-svg\"\nwidth = 24\nheight = 24\n"
            ),
        )
        .unwrap();
    }

    fn release_origin(version: &Version, immutable: bool) -> InstalledOrigin {
        InstalledOrigin::GithubRelease {
            release_id: version.minor + 1,
            tag: format!("v{version}"),
            asset_id: version.minor + 10,
            asset_name: "touchbar-plugin.touchbar".into(),
            asset_digest: format!("sha256:{}", "a".repeat(64)),
            immutable,
            attested: true,
        }
    }

    #[test]
    fn install_is_content_addressed_and_state_round_trips() {
        let source = tempfile::tempdir().unwrap();
        package(source.path());
        let manifest_path = source.path().join(MANIFEST_FILE_NAME);
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        fs::write(
            &manifest_path,
            format!(
                r#"{manifest}
default_width = 2008

[[profile]]
id = "focused"
label = "Focused"
items = ["test"]
applications = ["test-app"]
"#
            ),
        )
        .unwrap();
        let home = tempfile::tempdir().unwrap();
        let paths = StorePaths::under(home.path().join("store"));
        let mut store = PluginStore::open(paths.clone()).unwrap();
        let installed = store.install_directory(source.path()).unwrap();
        assert!(!installed.enabled);
        assert_eq!(installed.items[0].width, 2008);
        assert!(installed.profiles[0].enabled);
        assert!(store.package_path(&installed).unwrap().is_dir());
        store.set_enabled(&installed.source, true).unwrap();
        store
            .set_item_width(&installed.source, "test", 240)
            .unwrap();
        store
            .set_item_enabled(&installed.source, "test", false)
            .unwrap();
        store
            .set_profile_enabled(&installed.source, "focused", false)
            .unwrap();
        store.install_directory(source.path()).unwrap();
        drop(store);
        let store = PluginStore::open(paths).unwrap();
        let loaded = store.get(&installed.source).unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.items[0].width, 240);
        assert!(!loaded.items[0].enabled);
        assert!(!loaded.profiles[0].enabled);
    }

    #[test]
    fn archive_round_trip_and_traversal_are_strict() {
        let source = tempfile::tempdir().unwrap();
        package(source.path());
        let archive_dir = tempfile::tempdir().unwrap();
        let archive = archive_dir.path().join("test.touchbar");
        let expected = pack_directory(source.path(), &archive).unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut store = PluginStore::open(StorePaths::under(home.path().join("store"))).unwrap();
        let installed = store.install_archive(&archive).unwrap();
        assert_eq!(installed.package_digest, expected.package_digest);

        let malicious = archive_dir.path().join("malicious.touchbar");
        let mut file = File::create(&malicious).unwrap();
        file.write_all(ARCHIVE_MAGIC).unwrap();
        file.write_all(&1_u32.to_le_bytes()).unwrap();
        file.write_all(&9_u16.to_le_bytes()).unwrap();
        file.write_all(b"../escape").unwrap();
        file.write_all(&[0]).unwrap();
        file.write_all(&0_u64.to_le_bytes()).unwrap();
        drop(file);
        assert!(store.install_archive(&malicious).is_err());
        assert!(!home.path().join("escape").exists());
    }

    #[test]
    fn package_members_reject_symlinks_and_unrequested_files_are_not_packed() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        package(source.path());
        fs::write(source.path().join("private-key"), b"never package this").unwrap();
        fs::create_dir(source.path().join("assets")).unwrap();
        symlink("../private-key", source.path().join("assets/key")).unwrap();
        assert!(inspect_package(source.path()).is_err());
        fs::remove_file(source.path().join("assets/key")).unwrap();
        let inspection = inspect_package(source.path()).unwrap();
        assert!(
            inspection
                .members
                .iter()
                .all(|member| member.relative != Path::new("private-key"))
        );
    }

    #[test]
    fn declared_asset_digest_is_installer_owned_and_detects_tampering() {
        let source = tempfile::tempdir().unwrap();
        package(source.path());
        add_symbolic_asset(source.path(), b"<svg/>");

        let before = inspect_package(source.path()).unwrap();
        let locked = before.artifacts.get("assets/mark.svg").unwrap();
        assert_eq!(locked, &format!("sha256:{:x}", Sha256::digest(b"<svg/>")));

        fs::write(source.path().join("assets/mark.svg"), b"<svg>changed</svg>").unwrap();
        let after = inspect_package(source.path()).unwrap();
        assert_ne!(after.artifacts["assets/mark.svg"], *locked);
        assert_ne!(after.package_digest, before.package_digest);
    }

    #[test]
    fn verified_release_update_is_locked_and_rolls_back_from_retained_content() {
        let source_id = "github:alice/test".parse::<GithubSource>().unwrap();
        let version_one = Version::new(0, 1, 0);
        let version_two = Version::new(0, 2, 0);
        let packages = tempfile::tempdir().unwrap();
        let first = packages.path().join("first");
        let second = packages.path().join("second");
        package_version(&first, "0.1.0", false);
        package_version(&second, "0.2.0", false);
        let first_archive = packages.path().join("first.touchbar");
        let second_archive = packages.path().join("second.touchbar");
        pack_directory(&first, &first_archive).unwrap();
        pack_directory(&second, &second_archive).unwrap();

        let home = tempfile::tempdir().unwrap();
        let paths = StorePaths::under(home.path().join("store"));
        let mut store = PluginStore::open(paths.clone()).unwrap();
        let installed = store
            .install_release_archive(
                &first_archive,
                &source_id,
                &version_one,
                release_origin(&version_one, true),
            )
            .unwrap();
        assert_eq!(
            installed.installed.origin.provenance(),
            Provenance::VerifiedRelease
        );
        store.set_enabled(&source_id, true).unwrap();
        let updated = store
            .install_release_archive(
                &second_archive,
                &source_id,
                &version_two,
                release_origin(&version_two, false),
            )
            .unwrap();
        assert!(updated.installed.enabled);
        assert_eq!(
            updated.installed.origin.provenance(),
            Provenance::UnverifiedRelease
        );

        let rolled_back = store
            .rollback_release(&source_id, Some(&version_one))
            .unwrap();
        assert_eq!(rolled_back.installed.version, version_one);
        assert!(rolled_back.installed.enabled);
        drop(store);

        let reopened = PluginStore::open(paths).unwrap();
        assert_eq!(reopened.get(&source_id).unwrap().version, version_one);
    }

    #[test]
    fn release_update_with_new_authority_is_disabled_until_explicit_consent() {
        let source_id = "github:alice/test".parse::<GithubSource>().unwrap();
        let version_one = Version::new(0, 1, 0);
        let version_two = Version::new(0, 2, 0);
        let packages = tempfile::tempdir().unwrap();
        let first = packages.path().join("first");
        let second = packages.path().join("second");
        package_version(&first, "0.1.0", false);
        package_version(&second, "0.2.0", true);
        let first_archive = packages.path().join("first.touchbar");
        let second_archive = packages.path().join("second.touchbar");
        pack_directory(&first, &first_archive).unwrap();
        pack_directory(&second, &second_archive).unwrap();

        let home = tempfile::tempdir().unwrap();
        let mut store = PluginStore::open(StorePaths::under(home.path().join("store"))).unwrap();
        store
            .install_release_archive(
                &first_archive,
                &source_id,
                &version_one,
                release_origin(&version_one, true),
            )
            .unwrap();
        store.set_enabled(&source_id, true).unwrap();
        let updated = store
            .install_release_archive(
                &second_archive,
                &source_id,
                &version_two,
                release_origin(&version_two, true),
            )
            .unwrap();
        assert!(updated.disabled_for_consent);
        assert!(!updated.installed.enabled);
        assert!(
            updated
                .permission_changes
                .iter()
                .any(PermissionChange::requires_consent)
        );
    }

    #[test]
    fn mismatched_release_identity_cannot_replace_the_current_lock() {
        let source_id = "github:alice/test".parse::<GithubSource>().unwrap();
        let version_one = Version::new(0, 1, 0);
        let version_two = Version::new(0, 2, 0);
        let packages = tempfile::tempdir().unwrap();
        let first = packages.path().join("first");
        let forged = packages.path().join("forged");
        package_version(&first, "0.1.0", false);
        package_version(&forged, "0.2.0", false);
        let manifest = forged.join(MANIFEST_FILE_NAME);
        fs::write(
            &manifest,
            fs::read_to_string(&manifest)
                .unwrap()
                .replace("github:alice/test", "github:mallory/test"),
        )
        .unwrap();
        let first_archive = packages.path().join("first.touchbar");
        let forged_archive = packages.path().join("forged.touchbar");
        pack_directory(&first, &first_archive).unwrap();
        pack_directory(&forged, &forged_archive).unwrap();

        let home = tempfile::tempdir().unwrap();
        let paths = StorePaths::under(home.path().join("store"));
        let mut store = PluginStore::open(paths.clone()).unwrap();
        store
            .install_release_archive(
                &first_archive,
                &source_id,
                &version_one,
                release_origin(&version_one, true),
            )
            .unwrap();
        store.set_enabled(&source_id, true).unwrap();
        assert!(
            store
                .install_release_archive(
                    &forged_archive,
                    &source_id,
                    &version_two,
                    release_origin(&version_two, true),
                )
                .is_err()
        );
        drop(store);

        let reopened = PluginStore::open(paths).unwrap();
        let current = reopened.get(&source_id).unwrap();
        assert_eq!(current.version, version_one);
        assert!(current.enabled);
    }
}
