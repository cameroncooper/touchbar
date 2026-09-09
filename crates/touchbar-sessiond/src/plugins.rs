use std::{
    collections::{BTreeMap, BTreeSet},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use touchbar_broker_schema::AppearancePublish;
use touchbar_control::ProcessStatus;
use touchbar_package::{
    AppearanceProvider, AutomaticProfile, GithubSource, PluginManifest, PresentationBar,
    PresentationBarElement, PresentationGroupElement, RuntimeSpec,
};
use touchbar_plugin_store::{
    InstalledOrigin, InstalledRuntime, PluginStore, StorePaths, inspect_package,
};
use touchbar_policy::{
    CapabilityId, CapabilityRegistry, CapabilityRequest, CapabilityScope, EffectivePolicy,
    GrantStore, PackageInstance, Provenance, RuntimeKind, SessionGrants,
    calculate_effective_policy,
};
use touchbar_profile_config::{
    ContributionConfig, PROFILE_CONFIG_VERSION, PredicateConfig, ProfileConfig, ProfileDocument,
    ProfileElementConfig, ProfileItemConfig, ProfileItemRefConfig, ProfileRuleConfig, ScopeConfig,
    SlotPolicyConfig,
};
use touchbar_protocol::broker_ipc::Seqpacket;

use crate::appearance::ProviderIdentity;

const MAX_AUTOMATIC_RESTARTS: u32 = 8;
const CHILD_STATUS_INTERVAL: Duration = Duration::from_millis(250);
const POLICY_STATUS_INTERVAL: Duration = Duration::from_millis(250);
const APPEARANCE_SINK_FD: i32 = 7;
const APPEARANCE_SINK_FD_ENV: &str = "TOUCHBAR_APPEARANCE_SINK_FD";
const APPEARANCE_PROCESS_PREFIX: &str = "appearance-provider:";

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
    AppearanceProvider {
        identity: ProviderIdentity,
        digest: String,
        package_digest: String,
    },
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

impl PolicyInput {
    fn visual_worker(&self) -> Self {
        Self {
            package: self.package.clone(),
            requests: self
                .requests
                .iter()
                .filter(|request| request.capability != CapabilityId::AppearanceProvideV1)
                .cloned()
                .collect(),
        }
    }

    fn appearance_worker(&self, provider: &AppearanceProvider) -> Self {
        let mounts = provider.mounts.iter().collect::<BTreeSet<_>>();
        let requests = self
            .requests
            .iter()
            .filter_map(|request| {
                let CapabilityScope::AppearanceProvide(mut scope) = request.scope.clone() else {
                    return None;
                };
                scope.providers.retain(|id| id == &provider.id);
                scope.mounts.retain(|mount| mounts.contains(&mount.label));
                Some(CapabilityRequest {
                    scope: CapabilityScope::AppearanceProvide(scope),
                    ..request.clone()
                })
            })
            .collect();
        Self {
            package: self.package.clone(),
            requests,
        }
    }
}

struct Managed {
    desired: Desired,
    child: Option<Child>,
    restarts: u32,
    next_start: Instant,
    detail: Option<String>,
    stopped: bool,
    permission_blocked: bool,
    appearance_channel: Option<Seqpacket>,
}

impl Managed {
    fn record_failure(&mut self, detail: String, now: Instant) {
        self.child = None;
        self.appearance_channel = None;
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
pub struct PackagedProfileCatalog {
    enabled_items: Vec<ProfileItemConfig>,
    profiles: Vec<PackagedProfile>,
    manages_default_visibility: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AppearanceProviderCatalog {
    active: Option<ProviderIdentity>,
    declared: usize,
    eligible: usize,
}

impl AppearanceProviderCatalog {
    fn add_manifest(
        &mut self,
        manifest: &PluginManifest,
        policy: Option<&EffectivePolicy>,
        desktop_sessions: &BTreeSet<String>,
    ) {
        self.declared = self
            .declared
            .saturating_add(manifest.appearance_providers.len());
        for provider in &manifest.appearance_providers {
            if !provider
                .desktop_sessions
                .iter()
                .any(|session| desktop_sessions.contains(session))
            {
                continue;
            }
            let candidate = authorized_provider_identity(manifest, provider, policy);
            let Some(candidate) = candidate else {
                continue;
            };
            self.eligible = self.eligible.saturating_add(1);
            if self.active.as_ref().is_none_or(|active| {
                (&candidate.plugin, &candidate.id) < (&active.plugin, &active.id)
            }) {
                self.active = Some(candidate);
            }
        }
    }

    pub fn active(&self) -> Option<ProviderIdentity> {
        self.active.clone()
    }

    pub fn declared_count(&self) -> usize {
        self.declared
    }

    pub fn eligible_count(&self) -> usize {
        self.eligible
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackagedProfile {
    source: GithubSource,
    definition: AutomaticProfile,
}

impl PackagedProfileCatalog {
    fn add_manifest(
        &mut self,
        source: &GithubSource,
        manifest: &PluginManifest,
        enabled_items: &BTreeSet<String>,
        enabled_profiles: &BTreeSet<String>,
    ) {
        self.manages_default_visibility |= !manifest.profiles.is_empty()
            || manifest
                .items
                .iter()
                .any(|item| enabled_items.contains(&item.id) && !item.show_in_default_profile);
        self.enabled_items.extend(
            manifest
                .items
                .iter()
                .filter(|item| item.show_in_default_profile && enabled_items.contains(&item.id))
                .map(|item| ProfileItemConfig {
                    plugin: source.to_string(),
                    item: item.id.clone(),
                    required: false,
                }),
        );
        self.profiles.extend(
            manifest
                .profiles
                .iter()
                .filter(|profile| {
                    enabled_profiles.contains(&profile.id)
                        && profile
                            .items
                            .iter()
                            .all(|item| enabled_items.contains(item))
                })
                .cloned()
                .map(|definition| PackagedProfile {
                    source: source.clone(),
                    definition,
                }),
        );
    }

    /// Compose enabled package profiles over the user-owned document without
    /// rewriting it. Package priorities are placed below the lowest user
    /// priority, so explicit configuration always wins.
    pub fn merge(&self, user: Option<&ProfileDocument>) -> Result<Option<ProfileDocument>> {
        if self.profiles.is_empty() && (user.is_some() || !self.manages_default_visibility) {
            return Ok(user.cloned());
        }
        let mut document = user.cloned().unwrap_or_else(|| {
            let (contributions, elements) = if self.enabled_items.is_empty() {
                (
                    Vec::new(),
                    vec![ProfileElementConfig::FlexibleSpace {
                        minimum: 0,
                        weight: 1,
                    }],
                )
            } else {
                (
                    vec![ContributionConfig {
                        id: "automatic.installed-items".into(),
                        items: self.enabled_items.clone(),
                        scope: ScopeConfig::Global,
                        priority: 0,
                        when: PredicateConfig::Always,
                    }],
                    vec![ProfileElementConfig::Slot {
                        id: "installed-items".into(),
                        policy: SlotPolicyConfig::Fixed,
                        contributions: vec!["automatic.installed-items".into()],
                    }],
                )
            };
            ProfileDocument {
                version: PROFILE_CONFIG_VERSION,
                fallback: "automatic.default".into(),
                contributions,
                profiles: vec![ProfileConfig {
                    id: "automatic.default".into(),
                    elements,
                    principal_item: None,
                }],
                rules: Vec::new(),
                regions: Vec::new(),
            }
        });

        let user_priority_floor = document
            .rules
            .iter()
            .map(|rule| rule.priority)
            .min()
            .unwrap_or(0);
        let application_priority = user_priority_floor.saturating_sub(2);
        let activity_priority = user_priority_floor.saturating_sub(1);

        for packaged in &self.profiles {
            let profile_id = packaged_definition_id(&packaged.source, &packaged.definition.id);
            let contribution_id = format!("{profile_id}.items");
            document.contributions.push(ContributionConfig {
                id: contribution_id.clone(),
                items: packaged
                    .definition
                    .items
                    .iter()
                    .map(|item| ProfileItemConfig {
                        plugin: packaged.source.to_string(),
                        item: item.clone(),
                        required: false,
                    })
                    .collect(),
                scope: ScopeConfig::Global,
                priority: 0,
                when: PredicateConfig::Always,
            });
            document.profiles.push(ProfileConfig {
                id: profile_id.clone(),
                elements: vec![ProfileElementConfig::Slot {
                    id: "package-items".into(),
                    policy: SlotPolicyConfig::Fixed,
                    contributions: vec![contribution_id],
                }],
                principal_item: packaged.definition.principal_item.as_ref().map(|item| {
                    ProfileItemRefConfig {
                        plugin: packaged.source.to_string(),
                        item: item.clone(),
                    }
                }),
            });
            // Foreground layers are more specific than the application below
            // them, so an open picker wins while preserving user precedence.
            if !packaged.definition.activities.is_empty() {
                document.rules.push(ProfileRuleConfig {
                    profile: profile_id.clone(),
                    priority: activity_priority,
                    when: exact_activity_matches(&packaged.definition.activities),
                });
            }
            if !packaged.definition.applications.is_empty() {
                document.rules.push(ProfileRuleConfig {
                    profile: profile_id,
                    priority: application_priority,
                    when: exact_text_matches("application.id", &packaged.definition.applications),
                });
            }
        }
        document.validate()?;
        Ok(Some(document))
    }

    pub fn profile_count(&self) -> usize {
        self.profiles.len()
    }
}

fn packaged_definition_id(source: &GithubSource, local: &str) -> String {
    format!(
        "package.{}.{}.{}.{}.{}",
        source.owner().len(),
        source.owner(),
        source.repository().len(),
        source.repository(),
        local
    )
}

fn exact_text_matches(key: &str, values: &[String]) -> PredicateConfig {
    let mut predicates = values
        .iter()
        .map(|value| PredicateConfig::TextEquals {
            key: key.into(),
            value: value.clone(),
        })
        .collect::<Vec<_>>();
    if predicates.len() == 1 {
        predicates.pop().unwrap()
    } else {
        PredicateConfig::Any { predicates }
    }
}

fn exact_activity_matches(values: &[String]) -> PredicateConfig {
    let mut predicates = values
        .iter()
        .map(|value| PredicateConfig::BooleanEquals {
            key: format!("activity.{value}"),
            value: true,
        })
        .collect::<Vec<_>>();
    if predicates.len() == 1 {
        predicates.pop().unwrap()
    } else {
        PredicateConfig::Any { predicates }
    }
}

fn desktop_session_identities() -> BTreeSet<String> {
    [
        "DESKTOP_SESSION",
        "XDG_SESSION_DESKTOP",
        "XDG_CURRENT_DESKTOP",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .flat_map(|value| {
        value
            .to_string_lossy()
            .split([':', ';'])
            .map(|part| part.trim().to_ascii_lowercase())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
    })
    .collect()
}

fn authorized_provider_identity(
    manifest: &PluginManifest,
    provider: &AppearanceProvider,
    policy: Option<&EffectivePolicy>,
) -> Option<ProviderIdentity> {
    let policy = policy?;
    policy.grants.iter().find(|grant| {
        grant.allows()
            && grant.request.capability == CapabilityId::AppearanceProvideV1
            && matches!(
                &grant.request.scope,
                CapabilityScope::AppearanceProvide(scope) if scope.providers.contains(&provider.id)
            )
    })?;
    Some(ProviderIdentity {
        plugin: manifest.plugin.source.to_string(),
        id: provider.id.clone(),
        label: provider.label.clone(),
    })
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
    profiles: PackagedProfileCatalog,
    appearance_providers: AppearanceProviderCatalog,
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
            profiles: PackagedProfileCatalog::default(),
            appearance_providers: AppearanceProviderCatalog::default(),
            next_policy_refresh: Instant::now(),
            trusted_components,
        };
        manager.reload()?;
        Ok(manager)
    }

    pub fn reload(&mut self) -> Result<()> {
        let store = PluginStore::open(self.paths.clone())?;
        let (desired, presentations, profiles, appearance_providers) =
            desired_state(&store, self.trusted_components)?;
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
                appearance_channel: None,
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
        self.profiles = profiles;
        println!(
            "appearance-provider-catalog declared={} eligible={}",
            appearance_providers.declared_count(),
            appearance_providers.eligible_count()
        );
        self.appearance_providers = appearance_providers;
        self.refresh_permission_states();
        Ok(())
    }

    pub fn poll(&mut self) -> Vec<ProviderPublication> {
        let mut publications = Vec::new();
        let now = Instant::now();
        if now >= self.next_policy_refresh {
            self.refresh_permission_states();
        }
        for process in self.processes.values_mut() {
            if let (Launch::AppearanceProvider { identity, .. }, Some(channel)) =
                (&process.desired.launch, &process.appearance_channel)
            {
                for _ in 0..32 {
                    match channel.try_recv_payload() {
                        Ok(Some(payload)) => match AppearancePublish::decode(&payload) {
                            Ok(publication) if publication.provider == identity.id => {
                                publications.push(ProviderPublication {
                                    identity: identity.clone(),
                                    publication,
                                });
                            }
                            Ok(_) => {
                                process.detail = Some(
                                    "appearance provider published a mismatched identity".into(),
                                );
                            }
                            Err(_) => {
                                process.detail =
                                    Some("appearance provider published malformed data".into());
                            }
                        },
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            }
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
        publications
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
        let _ = self.poll();
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

    pub fn profile_catalog(&self) -> &PackagedProfileCatalog {
        &self.profiles
    }

    pub fn appearance_provider(&self) -> Option<ProviderIdentity> {
        self.appearance_providers.active()
    }

    pub fn appearance_notification_fds(&self) -> Vec<i32> {
        self.processes
            .values()
            .filter_map(|process| process.appearance_channel.as_ref().map(AsRawFd::as_raw_fd))
            .collect()
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPublication {
    pub identity: ProviderIdentity,
    pub publication: AppearancePublish,
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
) -> Result<(
    BTreeMap<Key, Desired>,
    PresentationCatalog,
    PackagedProfileCatalog,
    AppearanceProviderCatalog,
)> {
    let mut output = BTreeMap::new();
    let mut presentations = PresentationCatalog::default();
    let mut profiles = PackagedProfileCatalog::default();
    let mut appearance_providers = AppearanceProviderCatalog::default();
    let persistent_grants = GrantStore::load(&store.paths().grants).unwrap_or_default();
    let session_store = GrantStore::load(&store.paths().session_grants).unwrap_or_default();
    let session_grants = SessionGrants::from_records(session_store.records()).unwrap_or_default();
    let desktop_sessions = desktop_session_identities();
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
        let enabled_profiles = installed
            .profiles
            .iter()
            .filter(|profile| profile.enabled)
            .map(|profile| profile.id.clone())
            .collect::<BTreeSet<_>>();
        presentations.add_manifest(&inspected.manifest, &enabled_items)?;
        profiles.add_manifest(
            &installed.source,
            &inspected.manifest,
            &enabled_items,
            &enabled_profiles,
        );
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
        let component_policy = matches!(installed.runtime, InstalledRuntime::Component)
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
        let policy = (!trusted_components)
            .then(|| component_policy.as_ref().map(PolicyInput::visual_worker))
            .flatten();
        let effective_policy = component_policy.as_ref().map(|policy| {
            calculate_effective_policy(
                &policy.package,
                &policy.requests,
                &persistent_grants,
                &session_grants,
                &CapabilityRegistry::default(),
            )
        });
        appearance_providers.add_manifest(
            &inspected.manifest,
            effective_policy.as_ref(),
            &desktop_sessions,
        );
        for provider in &inspected.manifest.appearance_providers {
            if !provider
                .desktop_sessions
                .iter()
                .any(|session| desktop_sessions.contains(session))
            {
                continue;
            }
            let identity = ProviderIdentity {
                plugin: installed.source.to_string(),
                id: provider.id.clone(),
                label: provider.label.clone(),
            };
            let key = Key {
                source: installed.source.to_string(),
                item: format!("{APPEARANCE_PROCESS_PREFIX}{}", provider.id),
            };
            let desired = Desired {
                key: key.clone(),
                package: package_path.clone(),
                version: installed.version.to_string(),
                provenance: provenance_arg(&installed.origin).into(),
                width: 0,
                launch: Launch::AppearanceProvider {
                    identity,
                    digest: installed
                        .artifacts
                        .get(&provider.entrypoint)
                        .with_context(|| {
                            format!("appearance provider digest is missing for {}", provider.id)
                        })?
                        .clone(),
                    package_digest: installed.package_digest.clone(),
                },
                policy: component_policy
                    .as_ref()
                    .map(|policy| policy.appearance_worker(provider)),
            };
            if output.insert(key, desired).is_some() {
                bail!("duplicate appearance provider process key")
            }
        }
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
    Ok((output, presentations, profiles, appearance_providers))
}

fn start(supervisor: &Path, host: &Path, paths: &StorePaths, display: &str, process: &mut Managed) {
    let result: Result<(Child, Option<Seqpacket>)> = (|| match &process.desired.launch {
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
            if let Some(policy) = &process.desired.policy {
                command.arg("--package-digest").arg(&policy.package.digest);
            }
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
                .map(|child| (child, None))
                .map_err(Into::into)
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
            .spawn()
            .map(|child| (child, None))
            .map_err(Into::into),
        Launch::AppearanceProvider {
            identity,
            digest,
            package_digest,
        } => {
            let (session_channel, supervisor_channel) = Seqpacket::pair()?;
            let source = supervisor_channel.as_raw_fd();
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
                    "--package-digest",
                    package_digest,
                    "--appearance-provider",
                    &identity.id,
                    "--provenance",
                    &process.desired.provenance,
                ])
                .arg("--state")
                .arg(&paths.state)
                .arg("--grants")
                .arg(&paths.grants)
                .arg("--session-grants")
                .arg(&paths.session_grants)
                .arg("--audit")
                .arg(&paths.audit)
                .env(APPEARANCE_SINK_FD_ENV, APPEARANCE_SINK_FD.to_string())
                .stdin(Stdio::null());
            // SAFETY: the closure performs only descriptor duplication before
            // exec of the trusted supervisor.
            unsafe {
                command.pre_exec(move || {
                    if source != APPEARANCE_SINK_FD && libc::dup2(source, APPEARANCE_SINK_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::fcntl(APPEARANCE_SINK_FD, libc::F_SETFD, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn()?;
            drop(supervisor_channel);
            Ok((child, Some(session_channel)))
        }
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
            .spawn()
            .map(|child| (child, None))
            .map_err(Into::into),
    })();
    match result {
        Ok((child, appearance_channel)) => {
            process.child = Some(child);
            process.appearance_channel = appearance_channel;
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
    use std::os::unix::fs::PermissionsExt;
    use touchbar_model::{ContextSnapshot, ContextValue};
    use touchbar_policy::{
        Decision, FilesystemMountBinding, GrantBindings, GrantRecord, ReusePolicy,
    };

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
            appearance_channel: None,
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

    fn packaged_profiles() -> PackagedProfileCatalog {
        let source = GithubSource::new("owner", "omarchy").unwrap();
        PackagedProfileCatalog {
            enabled_items: Vec::new(),
            profiles: vec![PackagedProfile {
                source,
                definition: AutomaticProfile {
                    id: "screensaver".into(),
                    label: "Screensaver".into(),
                    items: vec!["screensaver".into()],
                    principal_item: Some("screensaver".into()),
                    applications: vec!["org.omarchy.screensaver".into()],
                    activities: vec!["omarchy-image-selector".into()],
                    enabled_by_default: true,
                },
            }],
            manages_default_visibility: true,
        }
    }

    #[test]
    fn package_profiles_create_a_valid_contextual_document_without_user_config() {
        let document = packaged_profiles().merge(None).unwrap().unwrap();
        document.validate().unwrap();
        assert_eq!(document.fallback, "automatic.default");
        assert_eq!(document.profiles.len(), 2);
        assert_eq!(document.rules.len(), 2);
        assert_eq!(document.rules[0].priority, -1);
        assert_eq!(document.rules[1].priority, -2);
        assert_eq!(
            document.rules[0].when,
            PredicateConfig::BooleanEquals {
                key: "activity.omarchy-image-selector".into(),
                value: true,
            }
        );

        let built = document
            .build(
                &BTreeMap::new(),
                ContextSnapshot {
                    generation: 1,
                    facts: BTreeMap::from([(
                        "activity.omarchy-image-selector".into(),
                        ContextValue::Boolean(true),
                    )]),
                },
            )
            .unwrap();
        assert!(
            built
                .controller
                .current()
                .composition
                .profile
                .as_str()
                .ends_with(".screensaver")
        );
    }

    #[test]
    fn user_rules_are_kept_ahead_of_lower_priority_package_defaults() {
        let user = ProfileDocument::from_toml(
            r#"version = 1
fallback = "user-default"

[[profile]]
id = "user-default"
[[profile.element]]
kind = "flexible-space"
minimum = 0
weight = 1

[[profile]]
id = "user-override"
[[profile.element]]
kind = "flexible-space"
minimum = 0
weight = 1

[[rule]]
profile = "user-override"
priority = 0
[rule.when]
kind = "text-equals"
key = "application.id"
value = "org.omarchy.screensaver"
"#,
        )
        .unwrap();
        let document = packaged_profiles().merge(Some(&user)).unwrap().unwrap();
        assert_eq!(document.rules[0].profile, "user-override");
        assert_eq!(document.rules[1].priority, -1);
        assert_eq!(document.rules[2].priority, -2);
        document.validate().unwrap();
    }

    #[test]
    fn contextual_only_items_stay_out_of_the_generated_fallback() {
        let manifest = PluginManifest::from_toml(
            r#"manifest_version = 1
[plugin]
name = "Omarchy"
version = "0.1.0"
description = "Test"
license = "MIT"
source = "github:owner/omarchy"
api = "^1.0"
[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/plugin@1.0.0"
[[items]]
id = "screensaver"
label = "Screensaver"
show_in_default_profile = false
[[items]]
id = "palette"
label = "Palette"
[[profile]]
id = "screensaver"
label = "Screensaver"
items = ["screensaver"]
activities = ["theme-change"]
"#,
        )
        .unwrap();
        let source = manifest.plugin.source.clone();
        let mut catalog = PackagedProfileCatalog::default();
        catalog.add_manifest(
            &source,
            &manifest,
            &BTreeSet::from(["screensaver".into(), "palette".into()]),
            &BTreeSet::from(["screensaver".into()]),
        );
        assert_eq!(catalog.enabled_items.len(), 1);
        assert_eq!(catalog.enabled_items[0].item, "palette");
        let document = catalog.merge(None).unwrap().unwrap();
        assert_eq!(document.contributions[0].items[0].item, "palette");
        assert_eq!(document.contributions[1].items[0].item, "screensaver");
    }

    #[test]
    fn appearance_provider_requires_its_grant_and_an_exact_desktop_match() {
        let manifest = PluginManifest::from_toml(
            r#"manifest_version = 1
[plugin]
name = "Omarchy"
version = "0.1.0"
description = "Test"
license = "MIT"
source = "github:owner/omarchy"
api = "^1.0"
[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/plugin@1.0.0"
[[items]]
id = "screensaver"
label = "Screensaver"
[[appearance-provider]]
id = "omarchy"
label = "Omarchy"
entrypoint = "component/appearance-provider.wasm"
world = "touchbar:plugin/appearance-provider@1.0.0"
desktop_sessions = ["omarchy"]
mounts = ["omarchy-current"]
[[permission]]
capability = "appearance.provide.v1"
required = false
reason = "Provide colors"
[permission.scope]
providers = ["omarchy"]
maximum_file_bytes = 65536
maximum_updates_per_second = 4
[[permission.scope.mounts]]
label = "omarchy-current"
suggested_location = "xdg-state:omarchy/current"
"#,
        )
        .unwrap();
        let requests = CapabilityRegistry::default().normalize(&manifest).unwrap();
        let package = PackageInstance {
            source: manifest.plugin.source.clone(),
            version: manifest.plugin.version.clone(),
            digest: format!("sha256:{}", "a".repeat(64)),
            provenance: Provenance::LocalDevelopment,
            runtime: RuntimeKind::Component,
        };
        let mut catalog = AppearanceProviderCatalog::default();
        let no_grants = calculate_effective_policy(
            &package,
            &requests,
            &GrantStore::default(),
            &SessionGrants::default(),
            &CapabilityRegistry::default(),
        );
        catalog.add_manifest(
            &manifest,
            Some(&no_grants),
            &BTreeSet::from(["omarchy".into()]),
        );
        assert!(catalog.active().is_none());

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let grants_path = root.path().join("permissions.toml");
        for request in &requests {
            let bindings = match &request.scope {
                CapabilityScope::AppearanceProvide(_) => GrantBindings {
                    filesystem_mounts: BTreeMap::from([(
                        "omarchy-current".into(),
                        FilesystemMountBinding::from_directory(root.path()).unwrap(),
                    )]),
                    ..GrantBindings::default()
                },
                _ => GrantBindings::default(),
            };
            GrantStore::update_record(
                &grants_path,
                GrantRecord {
                    source: package.source.clone(),
                    capability: request.capability.clone(),
                    approved_scope: request.scope.clone(),
                    bindings,
                    decision: Decision::Allow,
                    reuse: ReusePolicy::ExactDigest,
                    approved_version: package.version.clone(),
                    approved_digest: package.digest.clone(),
                },
            )
            .unwrap();
        }
        let grants = GrantStore::load(grants_path).unwrap();
        let authorized = calculate_effective_policy(
            &package,
            &requests,
            &grants,
            &SessionGrants::default(),
            &CapabilityRegistry::default(),
        );
        let mut catalog = AppearanceProviderCatalog::default();
        catalog.add_manifest(
            &manifest,
            Some(&authorized),
            &BTreeSet::from(["omarchy".into()]),
        );
        let active = catalog.active().unwrap();
        assert_eq!(active.plugin, "github:owner/omarchy");
        assert_eq!(active.id, "omarchy");

        let mut wrong_desktop = AppearanceProviderCatalog::default();
        wrong_desktop.add_manifest(
            &manifest,
            Some(&authorized),
            &BTreeSet::from(["gnome".into()]),
        );
        assert!(wrong_desktop.active().is_none());
    }
}
