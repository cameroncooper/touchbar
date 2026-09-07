use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use touchbar_control::ProcessStatus;
use touchbar_package::{
    PluginManifest, PresentationBar, PresentationBarElement, PresentationGroupElement, RuntimeSpec,
};
use touchbar_plugin_store::{
    InstalledOrigin, InstalledRuntime, PluginStore, StorePaths, inspect_package,
};
use touchbar_policy::{
    CapabilityRegistry, CapabilityRequest, GrantStore, PackageInstance, Provenance, RuntimeKind,
    SessionGrants, calculate_effective_policy,
};

const MAX_AUTOMATIC_RESTARTS: u32 = 8;
const CHILD_STATUS_INTERVAL: Duration = Duration::from_millis(250);
const POLICY_STATUS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Key {
    source: String,
    item: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Launch {
    Component {
        digest: String,
        asset_digests: Vec<(String, String)>,
    },
    TrustedComponent,
    Native {
        executable: PathBuf,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Desired {
    key: Key,
    package: PathBuf,
    version: String,
    provenance: String,
    width: u32,
    launch: Launch,
    policy: Option<PolicyInput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PolicyInput {
    package: PackageInstance,
    requests: Vec<CapabilityRequest>,
}

struct Managed {
    desired: Desired,
    child: Option<Child>,
    restarts: u32,
    next_start: Instant,
    detail: Option<String>,
    stopped: bool,
    permission_blocked: bool,
}

impl Managed {
    fn record_failure(&mut self, detail: String, now: Instant) {
        self.child = None;
        self.restarts = self.restarts.saturating_add(1);
        self.detail = Some(detail);
        self.stopped = self.restarts >= MAX_AUTOMATIC_RESTARTS;
        self.next_start = now + restart_delay(self.restarts);
    }

    fn state(&self) -> &'static str {
        if self.permission_blocked {
            "awaiting-consent"
        } else if self.child.is_some() {
            "running"
        } else if self.stopped {
            "crash-loop-stopped"
        } else {
            "restarting"
        }
    }

    fn unavailable_reason(&self) -> UnavailableReason {
        if self.permission_blocked {
            UnavailableReason::AwaitingConsent
        } else if self.stopped {
            UnavailableReason::CrashLoopStopped
        } else if self.child.is_some() {
            UnavailableReason::NotConnected
        } else {
            UnavailableReason::Restarting
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    AwaitingConsent,
    CrashLoopStopped,
    Restarting,
    NotConnected,
    Unavailable,
}

impl UnavailableReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::AwaitingConsent => "Plugin needs permission",
            Self::CrashLoopStopped => "Plugin stopped after repeated crashes",
            Self::Restarting => "Plugin is restarting",
            Self::NotConnected => "Plugin has not connected",
            Self::Unavailable => "Plugin is unavailable",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PresentationCatalog {
    item_bars: BTreeMap<(String, String), ItemBars>,
    bars: BTreeMap<(String, String), PresentationBar>,
    enabled_items: BTreeSet<(String, String)>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ItemBars {
    expanded: Option<String>,
    press_and_hold: Option<String>,
}

impl PresentationCatalog {
    fn add_manifest(
        &mut self,
        manifest: &PluginManifest,
        enabled: &BTreeSet<String>,
    ) -> Result<()> {
        let source = manifest.plugin.source.to_string();
        for item in &manifest.items {
            if !enabled.contains(&item.id) {
                continue;
            }
            let key = (source.clone(), item.id.clone());
            if !self.enabled_items.insert(key.clone()) {
                bail!(
                    "duplicate installed presentation item {}:{}",
                    source,
                    item.id
                );
            }
            self.item_bars.insert(
                key,
                ItemBars {
                    expanded: item.expanded_bar.clone(),
                    press_and_hold: item.press_and_hold_bar.clone(),
                },
            );
        }
        for bar in &manifest.bars {
            let key = (source.clone(), bar.id.clone());
            if self.bars.insert(key, bar.clone()).is_some() {
                bail!("duplicate installed presentation bar {}:{}", source, bar.id);
            }
        }
        Ok(())
    }

    pub fn bar_for_item(
        &self,
        source: &str,
        item: &str,
        persistent: bool,
    ) -> Option<&PresentationBar> {
        let links = self.item_bars.get(&(source.to_owned(), item.to_owned()))?;
        let bar = if persistent {
            links.expanded.as_deref()
        } else {
            links.press_and_hold.as_deref()
        }?;
        let bar = self.bars.get(&(source.to_owned(), bar.to_owned()))?;
        bar.elements
            .iter()
            .any(|element| presentation_element_enabled(self, source, element))
            .then_some(bar)
    }

    pub fn item_enabled(&self, source: &str, item: &str) -> bool {
        self.enabled_items
            .contains(&(source.to_owned(), item.to_owned()))
    }

    pub fn bar_count(&self) -> usize {
        self.bars.len()
    }
}

fn presentation_element_enabled(
    catalog: &PresentationCatalog,
    source: &str,
    element: &PresentationBarElement,
) -> bool {
    match element {
        PresentationBarElement::Item { item, .. } => catalog.item_enabled(source, item),
        PresentationBarElement::Group { elements, .. } => elements
            .iter()
            .any(|element| presentation_group_element_enabled(catalog, source, element)),
        PresentationBarElement::FixedSpace { .. }
        | PresentationBarElement::FlexibleSpace { .. } => false,
    }
}

fn presentation_group_element_enabled(
    catalog: &PresentationCatalog,
    source: &str,
    element: &PresentationGroupElement,
) -> bool {
    match element {
        PresentationGroupElement::Item { item, .. } => catalog.item_enabled(source, item),
        PresentationGroupElement::Group { elements, .. } => elements
            .iter()
            .any(|element| presentation_group_element_enabled(catalog, source, element)),
    }
}

pub struct PluginManager {
    paths: StorePaths,
    supervisor: PathBuf,
    host: PathBuf,
    wayland_display: String,
    processes: BTreeMap<Key, Managed>,
    presentations: PresentationCatalog,
    next_policy_refresh: Instant,
    trusted_components: bool,
}

impl PluginManager {
    pub fn new(
        paths: StorePaths,
        supervisor: PathBuf,
        host: PathBuf,
        wayland_display: String,
        trusted_components: bool,
    ) -> Result<Self> {
        let mut manager = Self {
            paths,
            supervisor,
            host,
            wayland_display,
            processes: BTreeMap::new(),
            presentations: PresentationCatalog::default(),
            next_policy_refresh: Instant::now(),
            trusted_components,
        };
        manager.reload()?;
        Ok(manager)
    }

    pub fn reload(&mut self) -> Result<()> {
        let store = PluginStore::open(self.paths.clone())?;
        let (desired, presentations) = desired_state(&store, self.trusted_components)?;
        let old = std::mem::take(&mut self.processes);
        for (key, mut process) in old {
            if desired
                .get(&key)
                .is_some_and(|candidate| candidate == &process.desired)
                && process
                    .child
                    .as_mut()
                    .is_some_and(|child| child.try_wait().ok().flatten().is_none())
            {
                self.processes.insert(key, process);
            } else if let Some(mut child) = process.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        for (key, desired) in desired {
            if self.processes.contains_key(&key) {
                continue;
            }
            let mut process = Managed {
                desired,
                child: None,
                restarts: 0,
                next_start: Instant::now(),
                detail: None,
                stopped: false,
                permission_blocked: false,
            };
            start(
                &self.supervisor,
                &self.host,
                &self.paths,
                &self.wayland_display,
                &mut process,
            );
            self.processes.insert(key, process);
        }
        self.presentations = presentations;
        self.refresh_permission_states();
        Ok(())
    }

    pub fn poll(&mut self) {
        let now = Instant::now();
        if now >= self.next_policy_refresh {
            self.refresh_permission_states();
        }
        for process in self.processes.values_mut() {
            if let Some(child) = &mut process.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        process.record_failure(format!("exited with {status}"), Instant::now());
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        process.record_failure(format!("status failed: {error}"), Instant::now());
                    }
                }
            }
            if process.child.is_none() && !process.stopped && Instant::now() >= process.next_start {
                start(
                    &self.supervisor,
                    &self.host,
                    &self.paths,
                    &self.wayland_display,
                    process,
                );
            }
        }
    }

    /// Child exit is the sole source that does not currently expose a pollable
    /// descriptor. Bound that one maintenance check without forcing the whole
    /// compositor into a timer loop.
    pub fn next_poll_delay(&self) -> Option<Duration> {
        let child = self
            .processes
            .values()
            .filter(|process| !process.stopped)
            .map(|process| {
                if process.child.is_some() {
                    CHILD_STATUS_INTERVAL
                } else {
                    process.next_start.saturating_duration_since(Instant::now())
                }
            })
            .min();
        let policy = self
            .processes
            .values()
            .any(|process| process.desired.policy.is_some())
            .then(|| {
                self.next_policy_refresh
                    .saturating_duration_since(Instant::now())
            });
        [child, policy].into_iter().flatten().min()
    }

    pub fn status(&mut self) -> Vec<ProcessStatus> {
        self.poll();
        self.processes
            .values()
            .map(|process| ProcessStatus {
                source: process.desired.key.source.clone(),
                item: process.desired.key.item.clone(),
                state: process.state().into(),
                pid: process.child.as_ref().map(Child::id),
                restarts: process.restarts,
                detail: process
                    .permission_blocked
                    .then(|| "required component capabilities are not granted".to_owned())
                    .or_else(|| process.detail.clone()),
            })
            .collect()
    }

    pub fn presentation_catalog(&self) -> &PresentationCatalog {
        &self.presentations
    }

    pub fn unavailable_reason(&self, qualified_item: &str) -> Option<UnavailableReason> {
        let (source, item) = qualified_item.rsplit_once('#')?;
        let process = self.processes.get(&Key {
            source: source.to_owned(),
            item: item.to_owned(),
        })?;
        Some(process.unavailable_reason())
    }

    fn refresh_permission_states(&mut self) {
        let grants = GrantStore::load(&self.paths.grants).unwrap_or_default();
        let session_store = GrantStore::load(&self.paths.session_grants).unwrap_or_default();
        let session = SessionGrants::from_records(session_store.records()).unwrap_or_default();
        let registry = CapabilityRegistry::default();
        for process in self.processes.values_mut() {
            process.permission_blocked = process.desired.policy.as_ref().is_some_and(|policy| {
                calculate_effective_policy(
                    &policy.package,
                    &policy.requests,
                    &grants,
                    &session,
                    &registry,
                )
                .blocked
            });
        }
        self.next_policy_refresh = Instant::now() + POLICY_STATUS_INTERVAL;
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        for process in self.processes.values_mut() {
            if let Some(mut child) = process.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn desired_state(
    store: &PluginStore,
    trusted_components: bool,
) -> Result<(BTreeMap<Key, Desired>, PresentationCatalog)> {
    let mut output = BTreeMap::new();
    let mut presentations = PresentationCatalog::default();
    for installed in store.plugins().filter(|plugin| plugin.enabled) {
        let package_path = store.package_path(installed)?;
        let inspected = inspect_package(&package_path)
            .with_context(|| format!("verify installed plugin {}", installed.source))?;
        if inspected.manifest.plugin.source != installed.source
            || inspected.manifest.plugin.version != installed.version
            || inspected.package_digest != installed.package_digest
            || inspected.artifacts != installed.artifacts
        {
            bail!(
                "installed plugin {} does not match its installer-owned lock record",
                installed.source
            )
        }
        let enabled_items = installed
            .items
            .iter()
            .filter(|item| item.enabled)
            .map(|item| item.id.clone())
            .collect::<BTreeSet<_>>();
        presentations.add_manifest(&inspected.manifest, &enabled_items)?;
        let launch = match (&installed.runtime, &inspected.manifest.runtime) {
            (InstalledRuntime::Component, RuntimeSpec::Component { .. }) if trusted_components => {
                Launch::TrustedComponent
            }
            (InstalledRuntime::Component, RuntimeSpec::Component { entrypoint, .. }) => {
                Launch::Component {
                    digest: installed
                        .artifacts
                        .get(entrypoint)
                        .context("component digest is missing")?
                        .clone(),
                    asset_digests: inspected
                        .manifest
                        .assets
                        .iter()
                        .map(|asset| {
                            Ok((
                                asset.id.clone(),
                                installed
                                    .artifacts
                                    .get(&asset.path)
                                    .with_context(|| {
                                        format!("asset digest is missing for {}", asset.id)
                                    })?
                                    .clone(),
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?,
                }
            }
            (InstalledRuntime::Native, RuntimeSpec::Native { targets }) => {
                let target = current_target();
                let entrypoint = targets
                    .iter()
                    .find(|candidate| candidate.target == target)
                    .with_context(|| {
                        format!(
                            "native plugin {} has no artifact for {target}",
                            installed.source
                        )
                    })?;
                Launch::Native {
                    executable: package_path.join(&entrypoint.entrypoint),
                }
            }
            _ => bail!("installed runtime does not match package manifest"),
        };
        let policy = (!trusted_components
            && matches!(installed.runtime, InstalledRuntime::Component))
        .then(|| {
            let registry = CapabilityRegistry::default();
            Ok::<_, anyhow::Error>(PolicyInput {
                package: PackageInstance {
                    source: installed.source.clone(),
                    version: installed.version.clone(),
                    digest: installed.package_digest.clone(),
                    provenance: provenance(&installed.origin),
                    runtime: RuntimeKind::Component,
                },
                requests: registry.normalize(&inspected.manifest).map_err(|errors| {
                    anyhow::anyhow!(
                        "invalid component permissions: {}",
                        errors
                            .into_iter()
                            .map(|error| error.to_string())
                            .collect::<Vec<_>>()
                            .join("; ")
                    )
                })?,
            })
        })
        .transpose()?;
        for item in installed.items.iter().filter(|item| item.enabled) {
            let key = Key {
                source: installed.source.to_string(),
                item: item.id.clone(),
            };
            let desired = Desired {
                key: key.clone(),
                package: package_path.clone(),
                version: installed.version.to_string(),
                provenance: provenance_arg(&installed.origin).into(),
                width: item.width,
                launch: launch.clone(),
                policy: policy.clone(),
            };
            if output.insert(key, desired).is_some() {
                bail!("duplicate plugin process key")
            }
        }
    }
    Ok((output, presentations))
}

fn start(supervisor: &Path, host: &Path, paths: &StorePaths, display: &str, process: &mut Managed) {
    let result = match &process.desired.launch {
        Launch::Component {
            digest,
            asset_digests,
        } => {
            let mut command = Command::new(supervisor);
            command
                .arg(&process.desired.package)
                .arg("--host")
                .arg(host)
                .args([
                    "--source",
                    &process.desired.key.source,
                    "--version",
                    &process.desired.version,
                    "--digest",
                    digest,
                ]);
            for (id, digest) in asset_digests {
                command.arg("--asset-digest").arg(format!("{id}={digest}"));
            }
            command
                .args(["--provenance", &process.desired.provenance])
                .arg("--state")
                .arg(&paths.state)
                .arg("--grants")
                .arg(&paths.grants)
                .arg("--session-grants")
                .arg(&paths.session_grants)
                .arg("--audit")
                .arg(&paths.audit)
                .arg("--")
                .args([
                    "--live",
                    "--item",
                    &process.desired.key.item,
                    "--width",
                    &process.desired.width.to_string(),
                ])
                .env("WAYLAND_DISPLAY", display)
                .stdin(Stdio::null())
                .spawn()
        }
        Launch::TrustedComponent => Command::new(host)
            .arg(&process.desired.package)
            .args([
                "--live",
                "--item",
                &process.desired.key.item,
                "--width",
                &process.desired.width.to_string(),
            ])
            .env("WAYLAND_DISPLAY", display)
            .stdin(Stdio::null())
            .spawn(),
        Launch::Native { executable } => Command::new(executable)
            .args([
                "--live",
                "--item",
                &process.desired.key.item,
                "--width",
                &process.desired.width.to_string(),
            ])
            .env("WAYLAND_DISPLAY", display)
            .stdin(Stdio::null())
            .spawn(),
    };
    match result {
        Ok(child) => {
            process.child = Some(child);
            process.detail = None;
        }
        Err(error) => {
            process.record_failure(format!("launch failed: {error}"), Instant::now());
        }
    }
}

fn provenance_arg(origin: &InstalledOrigin) -> &'static str {
    match provenance(origin) {
        Provenance::LocalDevelopment => "local-development",
        Provenance::VerifiedRelease => "verified-release",
        Provenance::UnverifiedRelease => "unverified-release",
    }
}

fn provenance(origin: &InstalledOrigin) -> Provenance {
    match origin {
        InstalledOrigin::LocalDevelopment => Provenance::LocalDevelopment,
        InstalledOrigin::GithubRelease {
            immutable: true, ..
        } => Provenance::VerifiedRelease,
        InstalledOrigin::GithubRelease {
            immutable: false, ..
        } => Provenance::UnverifiedRelease,
    }
}

fn restart_delay(restarts: u32) -> Duration {
    Duration::from_secs(1_u64 << restarts.min(5))
}
fn current_target() -> &'static str {
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(not(any(
        all(target_arch = "aarch64", target_os = "linux"),
        all(target_arch = "x86_64", target_os = "linux")
    )))]
    {
        "unsupported-target"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed() -> Managed {
        Managed {
            desired: Desired {
                key: Key {
                    source: "github:owner/plugin".into(),
                    item: "item".into(),
                },
                package: "/package".into(),
                version: "1.0.0".into(),
                provenance: "verified-release".into(),
                width: 120,
                launch: Launch::Native {
                    executable: "/package/plugin".into(),
                },
                policy: None,
            },
            child: None,
            restarts: 0,
            next_start: Instant::now(),
            detail: None,
            stopped: false,
            permission_blocked: false,
        }
    }

    #[test]
    fn repeated_failures_stop_deterministically_and_reload_starts_clean() {
        let started = Instant::now();
        let mut process = managed();
        for restart in 1..=MAX_AUTOMATIC_RESTARTS {
            process.record_failure(format!("failure {restart}"), started);
            assert_eq!(process.restarts, restart);
            assert_eq!(process.stopped, restart == MAX_AUTOMATIC_RESTARTS);
        }
        assert_eq!(process.state(), "crash-loop-stopped");
        assert_eq!(
            process.unavailable_reason(),
            UnavailableReason::CrashLoopStopped
        );

        let replacement = managed();
        assert_eq!(replacement.restarts, 0);
        assert!(!replacement.stopped);
        assert_eq!(replacement.state(), "restarting");
    }

    #[test]
    fn required_consent_is_reported_before_process_liveness() {
        let mut process = managed();
        process.permission_blocked = true;
        assert_eq!(process.state(), "awaiting-consent");
        assert_eq!(
            process.unavailable_reason(),
            UnavailableReason::AwaitingConsent
        );
    }
}
