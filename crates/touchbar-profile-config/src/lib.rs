//! Strict user-owned serialization for the pure profile composition model.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use touchbar_layout::{GroupLayout, ItemId, ItemSpec, MAX_GROUP_DEPTH};
use touchbar_model::{
    BarElement, CompositionController, ContextPredicate, ContextScope, ContextSnapshot,
    ContextValue, ContributionBinding, ContributionDefinition, ItemDefinition, ProfileCatalog,
    ProfileDefinition, ProfileElement, ProfileGroup, ProfileGroupElement, ProfileRule,
    ProfileSelector, Registry, SlotDefinition, SlotPolicy,
};

pub const PROFILE_CONFIG_VERSION: u32 = 1;
pub const PROFILE_CONFIG_FILE: &str = "profiles.toml";
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_CONTRIBUTIONS: usize = 256;
const MAX_RULES: usize = 256;
const MAX_REGIONS: usize = 64;
const MAX_ELEMENTS: usize = 512;
const MAX_ITEMS: usize = 256;
const MAX_PREDICATE_DEPTH: usize = 8;
const MAX_PREDICATE_NODES: usize = 128;
const MAX_ID_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDocument {
    pub version: u32,
    pub fallback: String,
    #[serde(default, rename = "contribution")]
    pub contributions: Vec<ContributionConfig>,
    #[serde(default, rename = "profile")]
    pub profiles: Vec<ProfileConfig>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<ProfileRuleConfig>,
    #[serde(default, rename = "region")]
    pub regions: Vec<RegionConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContributionConfig {
    pub id: String,
    #[serde(default)]
    pub items: Vec<ProfileItemConfig>,
    #[serde(default)]
    pub scope: ScopeConfig,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub when: PredicateConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileItemConfig {
    pub plugin: String,
    pub item: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileItemRefConfig {
    pub plugin: String,
    pub item: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub id: String,
    #[serde(default, rename = "element")]
    pub elements: Vec<ProfileElementConfig>,
    pub principal_item: Option<ProfileItemRefConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ProfileElementConfig {
    Slot {
        id: String,
        policy: SlotPolicyConfig,
        contributions: Vec<String>,
    },
    Group {
        id: String,
        #[serde(default)]
        layout: GroupLayoutConfig,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        visibility_priority: i32,
        #[serde(default)]
        compression_priority: i32,
        #[serde(default, rename = "element")]
        elements: Vec<ProfileGroupElementConfig>,
    },
    FixedSpace {
        width: u32,
    },
    FlexibleSpace {
        minimum: u32,
        weight: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ProfileGroupElementConfig {
    Slot {
        id: String,
        policy: SlotPolicyConfig,
        contributions: Vec<String>,
    },
    Group {
        id: String,
        #[serde(default)]
        layout: GroupLayoutConfig,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        visibility_priority: i32,
        #[serde(default)]
        compression_priority: i32,
        #[serde(default, rename = "element")]
        elements: Vec<ProfileGroupElementConfig>,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GroupLayoutConfig {
    #[default]
    Natural,
    EqualWidth,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRuleConfig {
    pub profile: String,
    #[serde(default)]
    pub priority: i32,
    pub when: PredicateConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegionConfig {
    pub id: String,
    pub x: u32,
    pub width: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PredicateConfig {
    #[default]
    Always,
    Present {
        key: String,
    },
    TextEquals {
        key: String,
        value: String,
    },
    BooleanEquals {
        key: String,
        value: bool,
    },
    All {
        predicates: Vec<PredicateConfig>,
    },
    Any {
        predicates: Vec<PredicateConfig>,
    },
    Not {
        predicate: Box<PredicateConfig>,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScopeConfig {
    #[default]
    Global,
    Workspace,
    Application,
    Window,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SlotPolicyConfig {
    Collect,
    Select,
    Fixed,
}

pub struct BuiltProfile {
    pub controller: CompositionController,
    pub item_ids: BTreeSet<String>,
    pub profile_ids: Vec<String>,
}

impl ProfileItemConfig {
    fn qualified(&self) -> String {
        qualified_item_id_unchecked(&self.plugin, &self.item)
    }
}

impl ProfileItemRefConfig {
    fn qualified(&self) -> String {
        qualified_item_id_unchecked(&self.plugin, &self.item)
    }
}

/// Produces the collision-free internal identity shared by profile configuration
/// and runtime surfaces. Both components remain visible in the serialized v1
/// format; this encoding is deliberately not itself a user-facing identifier.
pub fn qualified_item_id(plugin: &str, item: &str) -> Result<String> {
    validate_plugin_id("plugin id", plugin)?;
    validate_id("item id", item)?;
    Ok(qualified_item_id_unchecked(plugin, item))
}

fn qualified_item_id_unchecked(plugin: &str, item: &str) -> String {
    format!("{plugin}#{item}")
}

impl ProfileDocument {
    pub fn from_toml(source: &str) -> Result<Self> {
        if source.len() as u64 > MAX_CONFIG_BYTES {
            bail!("profile configuration exceeds {MAX_CONFIG_BYTES} bytes");
        }
        let document: Self = toml::from_str(source).context("parse profile configuration")?;
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != PROFILE_CONFIG_VERSION {
            bail!(
                "unsupported profile configuration version {}; expected {}",
                self.version,
                PROFILE_CONFIG_VERSION
            );
        }
        validate_id("fallback", &self.fallback)?;
        if self.profiles.is_empty() || self.profiles.len() > MAX_PROFILES {
            bail!("profiles must contain 1..={MAX_PROFILES} entries");
        }
        if self.contributions.len() > MAX_CONTRIBUTIONS {
            bail!("contributions exceed the {MAX_CONTRIBUTIONS} entry limit");
        }
        if self.rules.len() > MAX_RULES {
            bail!("rules exceed the {MAX_RULES} entry limit");
        }
        if self.regions.len() > MAX_REGIONS {
            bail!("regions exceed the {MAX_REGIONS} entry limit");
        }

        let mut region_ids = BTreeSet::new();
        for region in &self.regions {
            validate_id("region id", &region.id)?;
            if !region_ids.insert(&region.id) {
                bail!("duplicate region `{}`", region.id);
            }
            if region.width == 0
                || region
                    .x
                    .checked_add(region.width)
                    .is_none_or(|end| end > 2008)
            {
                bail!("region `{}` exceeds the Touch Bar bounds", region.id);
            }
        }

        let mut contribution_ids = BTreeSet::new();
        let mut all_items = BTreeSet::new();
        let mut predicate_nodes = 0;
        for contribution in &self.contributions {
            validate_id("contribution id", &contribution.id)?;
            if !contribution_ids.insert(&contribution.id) {
                bail!("duplicate contribution `{}`", contribution.id);
            }
            if contribution.items.is_empty() {
                bail!("contribution `{}` must contain an item", contribution.id);
            }
            let mut local_items = BTreeSet::new();
            for item in &contribution.items {
                validate_plugin_id("plugin id", &item.plugin)?;
                validate_id("item id", &item.item)?;
                let qualified = item.qualified();
                if !local_items.insert(qualified.clone()) {
                    bail!(
                        "contribution `{}` repeats item `{}:{}`",
                        contribution.id,
                        item.plugin,
                        item.item
                    );
                }
                all_items.insert(qualified);
            }
            validate_predicate(&contribution.when, 1, &mut predicate_nodes)?;
        }
        if all_items.len() > MAX_ITEMS {
            bail!("profile configuration references more than {MAX_ITEMS} items");
        }

        let mut profile_ids = BTreeSet::new();
        let mut element_count = 0;
        for profile in &self.profiles {
            validate_id("profile id", &profile.id)?;
            if !profile_ids.insert(&profile.id) {
                bail!("duplicate profile `{}`", profile.id);
            }
            if profile.elements.is_empty() {
                bail!("profile `{}` must contain an element", profile.id);
            }
            if let Some(principal) = &profile.principal_item {
                validate_plugin_id("principal plugin id", &principal.plugin)?;
                validate_id("principal item id", &principal.item)?;
                let qualified = principal.qualified();
                if !all_items.contains(&qualified) {
                    bail!(
                        "profile `{}` principal item `{}:{}` is not allowed by a contribution",
                        profile.id,
                        principal.plugin,
                        principal.item
                    );
                }
            }
            let mut slots = BTreeSet::new();
            let mut groups = BTreeSet::new();
            for element in &profile.elements {
                validate_profile_element(
                    element,
                    &profile.id,
                    &contribution_ids,
                    &mut slots,
                    &mut groups,
                    &mut element_count,
                )?;
            }
        }
        if element_count > MAX_ELEMENTS {
            bail!("profile elements exceed the {MAX_ELEMENTS} entry limit");
        }
        if !profile_ids.contains(&self.fallback) {
            bail!("fallback profile `{}` does not exist", self.fallback);
        }
        for rule in &self.rules {
            if !profile_ids.contains(&rule.profile) {
                bail!("rule references unknown profile `{}`", rule.profile);
            }
            validate_predicate(&rule.when, 1, &mut predicate_nodes)?;
        }
        Ok(())
    }

    pub fn item_ids(&self) -> BTreeSet<String> {
        self.contributions
            .iter()
            .flat_map(|contribution| contribution.items.iter().map(ProfileItemConfig::qualified))
            .collect()
    }

    pub fn required_item_ids(&self) -> BTreeSet<String> {
        self.contributions
            .iter()
            .flat_map(|contribution| contribution.items.iter())
            .filter(|item| item.required)
            .map(ProfileItemConfig::qualified)
            .collect()
    }

    pub fn profile_ids(&self) -> Vec<String> {
        self.profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect()
    }

    pub fn region(&self, id: &str) -> Option<(u32, u32)> {
        self.regions
            .iter()
            .find(|region| region.id == id)
            .map(|region| (region.x, region.width))
    }

    pub fn build(
        &self,
        connected: &BTreeMap<String, ItemSpec>,
        context: ContextSnapshot,
    ) -> Result<BuiltProfile> {
        self.validate()?;
        let item_ids = self.item_ids();
        let missing = self
            .required_item_ids()
            .iter()
            .filter(|item| !connected.contains_key(*item))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "required profile items are not connected: {}",
                missing.join(", ")
            );
        }

        let mut registry = Registry::default();
        for item in item_ids.iter().filter(|item| connected.contains_key(*item)) {
            let spec = connected
                .get(item)
                .expect("filtered connected item")
                .clone();
            if spec.id.as_str() != item {
                bail!("connected item map key does not match its layout ID");
            }
            registry.add_item(ItemDefinition::new(spec, item))?;
        }

        let mut catalog = ProfileCatalog::default();
        for contribution in &self.contributions {
            catalog.add_contribution(
                ContributionDefinition::new(
                    contribution.id.clone(),
                    contribution
                        .items
                        .iter()
                        .map(ProfileItemConfig::qualified)
                        .filter(|item| connected.contains_key(item))
                        .map(|item| BarElement::Item(ItemId::new(item)))
                        .collect(),
                )
                .scope(contribution.scope.into())
                .priority(contribution.priority)
                .when(contribution.when.to_model()),
            )?;
        }
        for profile in &self.profiles {
            let mut definition = ProfileDefinition::new(
                profile.id.clone(),
                profile
                    .elements
                    .iter()
                    .map(ProfileElementConfig::to_model)
                    .collect(),
            );
            if let Some(principal) = &profile.principal_item {
                let principal = principal.qualified();
                if connected.contains_key(&principal) {
                    definition = definition.principal_item(principal);
                }
            }
            catalog.add_profile(definition)?;
        }
        let selector = ProfileSelector {
            fallback: self.fallback.clone().into(),
            rules: self
                .rules
                .iter()
                .map(|rule| ProfileRule {
                    profile: rule.profile.clone().into(),
                    priority: rule.priority,
                    predicate: rule.when.to_model(),
                })
                .collect(),
        };
        Ok(BuiltProfile {
            controller: CompositionController::new(catalog, registry, selector, context)?,
            item_ids,
            profile_ids: self.profile_ids(),
        })
    }
}

impl ProfileElementConfig {
    fn to_model(&self) -> ProfileElement {
        match self {
            Self::Slot {
                id,
                policy,
                contributions,
            } => ProfileElement::Slot(SlotDefinition::new(
                id.clone(),
                (*policy).into(),
                contributions
                    .iter()
                    .enumerate()
                    .map(|(order, contribution)| {
                        ContributionBinding::new(contribution.clone()).order(order as i32)
                    })
                    .collect(),
            )),
            Self::Group {
                id,
                layout,
                spacing,
                visibility_priority,
                compression_priority,
                elements,
            } => ProfileElement::Group(ProfileGroup {
                id: id.clone(),
                elements: elements
                    .iter()
                    .map(ProfileGroupElementConfig::to_model)
                    .collect(),
                layout: (*layout).into(),
                spacing: *spacing,
                visibility_priority: *visibility_priority,
                compression_priority: *compression_priority,
            }),
            Self::FixedSpace { width } => ProfileElement::FixedSpace { width: *width },
            Self::FlexibleSpace { minimum, weight } => ProfileElement::FlexibleSpace {
                minimum: *minimum,
                weight: *weight,
            },
        }
    }
}

impl ProfileGroupElementConfig {
    fn to_model(&self) -> ProfileGroupElement {
        match self {
            Self::Slot {
                id,
                policy,
                contributions,
            } => ProfileGroupElement::Slot(SlotDefinition::new(
                id.clone(),
                (*policy).into(),
                contributions
                    .iter()
                    .enumerate()
                    .map(|(order, contribution)| {
                        ContributionBinding::new(contribution.clone()).order(order as i32)
                    })
                    .collect(),
            )),
            Self::Group {
                id,
                layout,
                spacing,
                visibility_priority,
                compression_priority,
                elements,
            } => ProfileGroupElement::Group(ProfileGroup {
                id: id.clone(),
                elements: elements.iter().map(Self::to_model).collect(),
                layout: (*layout).into(),
                spacing: *spacing,
                visibility_priority: *visibility_priority,
                compression_priority: *compression_priority,
            }),
        }
    }
}

impl From<GroupLayoutConfig> for GroupLayout {
    fn from(value: GroupLayoutConfig) -> Self {
        match value {
            GroupLayoutConfig::Natural => Self::Natural,
            GroupLayoutConfig::EqualWidth => Self::EqualWidth,
        }
    }
}

impl PredicateConfig {
    fn to_model(&self) -> ContextPredicate {
        match self {
            Self::Always => ContextPredicate::Always,
            Self::Present { key } => ContextPredicate::Present(key.clone()),
            Self::TextEquals { key, value } => {
                ContextPredicate::Equals(key.clone(), ContextValue::Text(value.clone()))
            }
            Self::BooleanEquals { key, value } => {
                ContextPredicate::Equals(key.clone(), ContextValue::Boolean(*value))
            }
            Self::All { predicates } => {
                ContextPredicate::All(predicates.iter().map(Self::to_model).collect())
            }
            Self::Any { predicates } => {
                ContextPredicate::Any(predicates.iter().map(Self::to_model).collect())
            }
            Self::Not { predicate } => ContextPredicate::Not(Box::new(predicate.to_model())),
        }
    }
}

impl From<ScopeConfig> for ContextScope {
    fn from(value: ScopeConfig) -> Self {
        match value {
            ScopeConfig::Global => Self::Global,
            ScopeConfig::Workspace => Self::Workspace,
            ScopeConfig::Application => Self::Application,
            ScopeConfig::Window => Self::Window,
        }
    }
}

impl From<SlotPolicyConfig> for SlotPolicy {
    fn from(value: SlotPolicyConfig) -> Self {
        match value {
            SlotPolicyConfig::Collect => Self::Collect,
            SlotPolicyConfig::Select => Self::Select,
            SlotPolicyConfig::Fixed => Self::Fixed,
        }
    }
}

pub fn discover_path() -> Result<PathBuf> {
    if let Some(root) = env::var_os("TOUCHBAR_HOME") {
        return Ok(PathBuf::from(root).join(PROFILE_CONFIG_FILE));
    }
    let root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME or XDG_CONFIG_HOME is required")?;
    Ok(root.join("touchbar").join(PROFILE_CONFIG_FILE))
}

pub fn load(path: &Path) -> Result<ProfileDocument> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open profile configuration {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect profile configuration {}", path.display()))?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.len() > MAX_CONFIG_BYTES
    {
        bail!(
            "profile configuration {} must be one owner-writable regular file owned by the current user and at most {MAX_CONFIG_BYTES} bytes",
            path.display()
        );
    }
    let mut source = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut source)
        .with_context(|| format!("read profile configuration {}", path.display()))?;
    ProfileDocument::from_toml(&source)
}

fn validate_id(label: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        });
    if !valid {
        bail!("{label} `{value}` is not a bounded lowercase identifier");
    }
    Ok(())
}

fn validate_profile_element(
    element: &ProfileElementConfig,
    profile: &str,
    contribution_ids: &BTreeSet<&String>,
    slots: &mut BTreeSet<String>,
    groups: &mut BTreeSet<String>,
    element_count: &mut usize,
) -> Result<()> {
    *element_count += 1;
    match element {
        ProfileElementConfig::Slot {
            id, contributions, ..
        } => validate_slot_config(id, contributions, profile, contribution_ids, slots),
        ProfileElementConfig::Group {
            id,
            spacing,
            elements,
            ..
        } => validate_group_config(
            id,
            *spacing,
            elements,
            profile,
            contribution_ids,
            slots,
            groups,
            element_count,
            1,
        ),
        ProfileElementConfig::FixedSpace { width } if *width > 2008 => {
            bail!("fixed space exceeds the Touch Bar width")
        }
        ProfileElementConfig::FlexibleSpace { minimum, weight }
            if *minimum > 2008 || *weight == 0 =>
        {
            bail!("flexible space has invalid minimum or zero weight")
        }
        ProfileElementConfig::FixedSpace { .. } | ProfileElementConfig::FlexibleSpace { .. } => {
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_group_config(
    id: &String,
    spacing: u32,
    elements: &[ProfileGroupElementConfig],
    profile: &str,
    contribution_ids: &BTreeSet<&String>,
    slots: &mut BTreeSet<String>,
    groups: &mut BTreeSet<String>,
    element_count: &mut usize,
    depth: usize,
) -> Result<()> {
    validate_id("group id", id)?;
    if !groups.insert(id.clone()) {
        bail!("profile `{profile}` repeats group `{id}`");
    }
    if depth > MAX_GROUP_DEPTH {
        bail!("profile `{profile}` group `{id}` exceeds the maximum nesting depth");
    }
    if spacing > 2008 {
        bail!("profile `{profile}` group `{id}` spacing exceeds the Touch Bar width");
    }
    if elements.is_empty() {
        bail!("profile `{profile}` group `{id}` must contain an element");
    }
    for element in elements {
        *element_count += 1;
        match element {
            ProfileGroupElementConfig::Slot {
                id: slot,
                contributions,
                ..
            } => {
                if depth >= MAX_GROUP_DEPTH {
                    bail!(
                        "profile `{profile}` group `{id}` cannot contain a slot beyond the maximum nesting depth"
                    );
                }
                validate_slot_config(slot, contributions, profile, contribution_ids, slots)?;
            }
            ProfileGroupElementConfig::Group {
                id,
                spacing,
                elements,
                ..
            } => validate_group_config(
                id,
                *spacing,
                elements,
                profile,
                contribution_ids,
                slots,
                groups,
                element_count,
                depth + 1,
            )?,
        }
    }
    Ok(())
}

fn validate_slot_config(
    id: &String,
    contributions: &[String],
    profile: &str,
    contribution_ids: &BTreeSet<&String>,
    slots: &mut BTreeSet<String>,
) -> Result<()> {
    validate_id("slot id", id)?;
    if !slots.insert(id.clone()) {
        bail!("profile `{profile}` repeats slot `{id}`");
    }
    if contributions.is_empty() {
        bail!("profile `{profile}` slot `{id}` has no contributions");
    }
    let mut bindings = BTreeSet::new();
    for contribution in contributions {
        validate_id("bound contribution", contribution)?;
        if !contribution_ids.contains(contribution) {
            bail!(
                "profile `{profile}` slot `{id}` references unknown contribution `{contribution}`"
            );
        }
        if !bindings.insert(contribution) {
            bail!("profile `{profile}` slot `{id}` repeats contribution `{contribution}`");
        }
    }
    Ok(())
}

fn validate_plugin_id(label: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_VALUE_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        });
    if !valid {
        bail!("{label} `{value}` is not a bounded lowercase plugin identifier");
    }
    Ok(())
}

fn validate_fact(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("{label} is empty, oversized, or contains whitespace/control characters");
    }
    Ok(())
}

fn validate_predicate(predicate: &PredicateConfig, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    if depth > MAX_PREDICATE_DEPTH || *nodes > MAX_PREDICATE_NODES {
        bail!("context predicates exceed their depth or node budget");
    }
    match predicate {
        PredicateConfig::Always => {}
        PredicateConfig::Present { key } | PredicateConfig::BooleanEquals { key, .. } => {
            validate_fact("context key", key, MAX_ID_BYTES)?;
        }
        PredicateConfig::TextEquals { key, value } => {
            validate_fact("context key", key, MAX_ID_BYTES)?;
            validate_fact("context value", value, MAX_VALUE_BYTES)?;
        }
        PredicateConfig::All { predicates } | PredicateConfig::Any { predicates } => {
            if predicates.is_empty() {
                bail!("all/any predicate must contain a child");
            }
            for child in predicates {
                validate_predicate(child, depth + 1, nodes)?;
            }
        }
        PredicateConfig::Not { predicate } => {
            validate_predicate(predicate, depth + 1, nodes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1
fallback = "default"

[[contribution]]
id = "persistent"
items = [
    { plugin = "github:example/controls", item = "volume", required = true },
    { plugin = "github:example/controls", item = "battery", required = true },
]

[[contribution]]
id = "browser"
items = [{ plugin = "github:example/media", item = "timeline", required = false }]
scope = "application"
priority = 100
when = { kind = "text-equals", key = "application.id", value = "firefox" }

[[profile]]
id = "default"
principal_item = { plugin = "github:example/media", item = "timeline" }
[[profile.element]]
kind = "slot"
id = "always"
policy = "fixed"
contributions = ["persistent"]
[[profile.element]]
kind = "flexible-space"
minimum = 8
weight = 1
[[profile.element]]
kind = "slot"
id = "context"
policy = "select"
contributions = ["browser"]

[[rule]]
profile = "default"
priority = 10
when = { kind = "present", key = "workspace.id" }
"#;

    fn connected() -> BTreeMap<String, ItemSpec> {
        [
            ("github:example/controls", "volume"),
            ("github:example/controls", "battery"),
            ("github:example/media", "timeline"),
        ]
        .into_iter()
        .map(|(plugin, item)| {
            let id = qualified_item_id(plugin, item).unwrap();
            (id.clone(), ItemSpec::new(id, 40, 80, 500))
        })
        .collect()
    }

    #[test]
    fn strict_document_builds_the_existing_profile_model() {
        let document = ProfileDocument::from_toml(VALID).unwrap();
        let built = document
            .build(
                &connected(),
                ContextSnapshot::default().text("application.id", "firefox"),
            )
            .unwrap();
        let items = built
            .controller
            .current()
            .composition
            .bar
            .elements
            .iter()
            .filter_map(|element| match element {
                touchbar_layout::Element::Item(item) => Some(item.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            items,
            [
                "github:example/controls#volume",
                "github:example/controls#battery",
                "github:example/media#timeline"
            ]
        );
        assert_eq!(built.profile_ids, ["default"]);
    }

    #[test]
    fn missing_items_and_ambient_authority_fail_closed() {
        let document = ProfileDocument::from_toml(VALID).unwrap();
        let mut missing = connected();
        missing.remove("github:example/controls#volume");
        assert!(
            document
                .build(&missing, ContextSnapshot::default())
                .is_err()
        );
        let mut optional_missing = connected();
        optional_missing.remove("github:example/media#timeline");
        assert!(
            document
                .build(&optional_missing, ContextSnapshot::default())
                .is_ok()
        );

        let unknown = VALID.replace(
            "contributions = [\"browser\"]",
            "contributions = [\"other\"]",
        );
        assert!(ProfileDocument::from_toml(&unknown).is_err());
        assert!(ProfileDocument::from_toml(&format!("{VALID}\nunknown = true")).is_err());
    }

    #[test]
    fn predicate_bombs_and_unsafe_files_are_rejected() {
        let mut predicate = "{ kind = \"always\" }".to_owned();
        for _ in 0..MAX_PREDICATE_DEPTH {
            predicate = format!("{{ kind = \"not\", predicate = {predicate} }}");
        }
        let bomb = VALID.replace("{ kind = \"present\", key = \"workspace.id\" }", &predicate);
        assert!(ProfileDocument::from_toml(&bomb).is_err());

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(PROFILE_CONFIG_FILE);
        fs::write(&path, VALID).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(load(&path).is_err());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = root.path().join("hardlink.toml");
        fs::hard_link(&path, &hardlink).unwrap();
        assert!(load(&path).is_err());
        fs::remove_file(&hardlink).unwrap();

        let symlink = root.path().join("symlink.toml");
        std::os::unix::fs::symlink(&path, &symlink).unwrap();
        assert!(load(&symlink).is_err());

        let oversized = root.path().join("oversized.toml");
        fs::write(&oversized, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(load(&oversized).is_err());
    }

    #[test]
    fn package_qualification_prevents_local_item_collisions() {
        let document = ProfileDocument::from_toml(VALID).unwrap();
        assert!(
            document
                .item_ids()
                .contains("github:example/controls#volume")
        );
        assert!(
            document
                .item_ids()
                .contains("github:example/media#timeline")
        );

        let distinct_package = VALID
            .replace(
                "{ plugin = \"github:example/media\", item = \"timeline\", required = false }",
                "{ plugin = \"github:example/media\", item = \"volume\", required = false }",
            )
            .replace(
                "principal_item = { plugin = \"github:example/media\", item = \"timeline\" }",
                "principal_item = { plugin = \"github:example/media\", item = \"volume\" }",
            );
        assert!(ProfileDocument::from_toml(&distinct_package).is_ok());

        let duplicate = VALID.replace(
            "{ plugin = \"github:example/controls\", item = \"battery\", required = true }",
            "{ plugin = \"github:example/controls\", item = \"volume\", required = true }",
        );
        assert!(ProfileDocument::from_toml(&duplicate).is_err());
    }

    #[test]
    fn presentation_regions_are_named_unique_and_canvas_bounded() {
        let with_region = VALID.replace(
            "fallback = \"default\"",
            "fallback = \"default\"\n\n[[region]]\nid = \"palette\"\nx = 504\nwidth = 1000",
        );
        let document = ProfileDocument::from_toml(&with_region).unwrap();
        assert_eq!(document.region("palette"), Some((504, 1000)));
        assert_eq!(document.region("missing"), None);

        for invalid in [
            with_region.replace("width = 1000", "width = 0"),
            with_region.replace("x = 504", "x = 1500"),
            with_region.replace("id = \"palette\"", "id = \"../palette\""),
            format!("{with_region}\n[[region]]\nid = \"palette\"\nx = 0\nwidth = 1\n"),
        ] {
            assert!(ProfileDocument::from_toml(&invalid).is_err());
        }
    }

    #[test]
    fn checked_in_v1_example_is_valid_without_installed_optional_items() {
        let document =
            ProfileDocument::from_toml(include_str!("../../../config/profiles.toml.example"))
                .unwrap();
        assert_eq!(document.profile_ids(), ["default", "minimal", "media"]);
        assert!(
            document
                .build(&BTreeMap::new(), ContextSnapshot::default())
                .is_ok()
        );
    }

    #[test]
    fn v1_toml_builds_nested_equal_width_slot_groups() {
        let source = r#"
version = 1
fallback = "default"

[[contribution]]
id = "left"
items = [{ plugin = "github:example/controls", item = "volume", required = true }]

[[contribution]]
id = "right"
items = [{ plugin = "github:example/controls", item = "battery", required = true }]

[[profile]]
id = "default"

[[profile.element]]
kind = "group"
id = "system-controls"
layout = "equal-width"
spacing = 4
visibility_priority = 1000

[[profile.element.element]]
kind = "slot"
id = "left-slot"
policy = "fixed"
contributions = ["left"]

[[profile.element.element]]
kind = "group"
id = "nested"
layout = "natural"

[[profile.element.element.element]]
kind = "slot"
id = "right-slot"
policy = "fixed"
contributions = ["right"]
"#;
        let document = ProfileDocument::from_toml(source).unwrap();
        let built = document
            .build(&connected(), ContextSnapshot::default())
            .unwrap();
        let composition = &built.controller.current().composition;
        assert_eq!(composition.slots[0].path, [0, 0]);
        assert_eq!(composition.slots[1].path, [0, 1, 0]);
        let layout = touchbar_layout::resolve(&composition.bar, 164, &BTreeSet::new()).unwrap();
        assert_eq!(layout.placements.len(), 2);
        assert_eq!(layout.placements[0].width, 80);
        assert_eq!(layout.placements[1].width, 80);
    }
}
