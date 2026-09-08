//! Package-manifest types shared by plugin tooling, installers, and hosts.
//!
//! Trust information such as release digests, attestations, and catalog review
//! status deliberately does not live in the package-controlled manifest.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    path::{Component, Path},
    str::FromStr,
};

use semver::{Version, VersionReq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

mod asset_bundle;

pub use asset_bundle::{
    AssetBundleError, BundledAsset, MAX_ASSET_BUNDLE_BYTES, MAX_ASSET_FILE_BYTES,
    decode_asset_bundle, encode_asset_bundle,
};

pub const MANIFEST_FILE_NAME: &str = "touchbar-plugin.toml";
pub const SUPPORTED_MANIFEST_VERSION: u32 = 1;
pub const SUPPORTED_HOST_API_VERSION: &str = "1.0.0";
pub const SUPPORTED_COMPONENT_WORLD: &str = "touchbar:plugin/plugin@1.0.0";
pub const MAX_ASSETS: usize = 64;
/// Largest width a manifest may declare, shared with the profile and replay
/// parsers so a wider panel never needs a manifest contract change.
pub const MAX_TOUCHBAR_WIDTH: u32 = touchbar_layout::MAX_CANVAS_WIDTH;
/// Representative full-strip width for render tests. Not a bound.
pub const REFERENCE_TOUCHBAR_WIDTH: u32 = touchbar_layout::REFERENCE_CANVAS_WIDTH;
pub const MAX_ASSET_WIDTH: u32 = MAX_TOUCHBAR_WIDTH;
pub const MAX_ASSET_HEIGHT: u32 = 240;
pub const MAX_ASSET_PIXELS: u64 = 2 * 1024 * 1024;
pub const MAX_PRESENTATION_BARS: usize = 64;
pub const MAX_PRESENTATION_BAR_ELEMENTS: usize = 64;
pub const MAX_AUTOMATIC_PROFILES: usize = 64;
pub const MAX_AUTOMATIC_PROFILE_ITEMS: usize = 64;
pub const MAX_AUTOMATIC_PROFILE_CONTEXTS: usize = 64;
pub const MAX_APPEARANCE_PROVIDERS: usize = 8;
pub const MAX_LOCAL_ID_BYTES: usize = 64;
/// Groups are deliberately shallow: the compositor can resolve their complete
/// geometry without turning an untrusted manifest into an unbounded tree walk.
pub const MAX_PRESENTATION_GROUP_DEPTH: usize = 8;
pub const MAX_PRESENTATION_SPACE: u32 = MAX_TOUCHBAR_WIDTH;
pub const MAX_FLEXIBLE_SPACE_WEIGHT: u32 = 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub manifest_version: u32,
    pub plugin: PluginMetadata,
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub items: Vec<ItemContribution>,
    #[serde(default, rename = "bar")]
    pub bars: Vec<PresentationBar>,
    #[serde(default, rename = "profile")]
    pub profiles: Vec<AutomaticProfile>,
    #[serde(default, rename = "appearance-provider")]
    pub appearance_providers: Vec<AppearanceProvider>,
    #[serde(default, rename = "asset")]
    pub assets: Vec<AssetDefinition>,
    #[serde(default, rename = "permission")]
    pub permissions: Vec<PermissionRequest>,
}

impl PluginManifest {
    pub fn from_toml(source: &str) -> Result<Self, ManifestError> {
        let manifest = toml::from_str::<Self>(source).map_err(ManifestError::Parse)?;
        manifest.validate().map_err(ManifestError::Invalid)?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();

        if self.manifest_version != SUPPORTED_MANIFEST_VERSION {
            issues.push(ValidationIssue::new(
                "manifest_version",
                format!(
                    "unsupported manifest version {}; this host supports {}",
                    self.manifest_version, SUPPORTED_MANIFEST_VERSION
                ),
            ));
        }

        validate_nonempty(&mut issues, "plugin.name", &self.plugin.name);
        validate_nonempty(&mut issues, "plugin.description", &self.plugin.description);
        validate_nonempty(&mut issues, "plugin.license", &self.plugin.license);

        if self.items.is_empty() {
            issues.push(ValidationIssue::new(
                "items",
                "a plugin pack must contribute at least one stable item",
            ));
        }

        let mut item_ids = BTreeSet::new();
        for (index, item) in self.items.iter().enumerate() {
            let field = format!("items[{index}].id");
            validate_kebab_id(&mut issues, &field, &item.id);
            validate_nonempty(&mut issues, &format!("items[{index}].label"), &item.label);
            if !item_ids.insert(&item.id) {
                issues.push(ValidationIssue::new(field, "duplicate item id"));
            }
            if !(1..=MAX_TOUCHBAR_WIDTH).contains(&item.default_width) {
                issues.push(ValidationIssue::new(
                    format!("items[{index}].default_width"),
                    format!("must be between 1 and {MAX_TOUCHBAR_WIDTH}"),
                ));
            }
        }

        if self.profiles.len() > MAX_AUTOMATIC_PROFILES {
            issues.push(ValidationIssue::new(
                "profile",
                format!(
                    "a package may declare at most {MAX_AUTOMATIC_PROFILES} automatic profiles"
                ),
            ));
        }
        let mut profile_ids = BTreeSet::new();
        for (index, profile) in self.profiles.iter().enumerate() {
            let prefix = format!("profile[{index}]");
            validate_kebab_id(&mut issues, &format!("{prefix}.id"), &profile.id);
            validate_nonempty(&mut issues, &format!("{prefix}.label"), &profile.label);
            if !profile_ids.insert(&profile.id) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.id"),
                    "duplicate automatic profile id",
                ));
            }
            if profile.items.is_empty() || profile.items.len() > MAX_AUTOMATIC_PROFILE_ITEMS {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.items"),
                    format!(
                        "must contain 1..={MAX_AUTOMATIC_PROFILE_ITEMS} package-local item IDs"
                    ),
                ));
            }
            let mut profile_items = BTreeSet::new();
            for (item_index, item) in profile.items.iter().enumerate() {
                validate_kebab_id(&mut issues, &format!("{prefix}.items[{item_index}]"), item);
                if !item_ids.contains(item) {
                    issues.push(ValidationIssue::new(
                        format!("{prefix}.items[{item_index}]"),
                        format!("references unknown package item `{item}`"),
                    ));
                }
                if !profile_items.insert(item) {
                    issues.push(ValidationIssue::new(
                        format!("{prefix}.items[{item_index}]"),
                        format!("repeats package item `{item}`"),
                    ));
                }
            }
            if let Some(principal) = &profile.principal_item {
                validate_kebab_id(&mut issues, &format!("{prefix}.principal_item"), principal);
                if !profile_items.contains(principal) {
                    issues.push(ValidationIssue::new(
                        format!("{prefix}.principal_item"),
                        "must reference an item in this automatic profile",
                    ));
                }
            }
            let context_count = profile
                .applications
                .len()
                .saturating_add(profile.activities.len());
            if context_count == 0 || context_count > MAX_AUTOMATIC_PROFILE_CONTEXTS {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.applications"),
                    format!(
                        "applications and activities must contain 1..={MAX_AUTOMATIC_PROFILE_CONTEXTS} exact identities in total"
                    ),
                ));
            }
            validate_context_identities(
                &mut issues,
                &format!("{prefix}.applications"),
                &profile.applications,
            );
            validate_context_identities(
                &mut issues,
                &format!("{prefix}.activities"),
                &profile.activities,
            );
        }

        if self.appearance_providers.len() > MAX_APPEARANCE_PROVIDERS {
            issues.push(ValidationIssue::new(
                "appearance-provider",
                format!(
                    "a package may declare at most {MAX_APPEARANCE_PROVIDERS} appearance providers"
                ),
            ));
        }
        let mut provider_ids = BTreeSet::new();
        for (index, provider) in self.appearance_providers.iter().enumerate() {
            let prefix = format!("appearance-provider[{index}]");
            validate_kebab_id(&mut issues, &format!("{prefix}.id"), &provider.id);
            validate_nonempty(&mut issues, &format!("{prefix}.label"), &provider.label);
            validate_kebab_id(&mut issues, &format!("{prefix}.mount"), &provider.mount);
            validate_relative_path(&mut issues, &format!("{prefix}.path"), &provider.path);
            if !provider_ids.insert(&provider.id) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.id"),
                    "duplicate appearance provider id",
                ));
            }
            if provider.desktop_sessions.is_empty() {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.desktop_sessions"),
                    "must contain at least one exact desktop session identity",
                ));
            }
            validate_context_identities(
                &mut issues,
                &format!("{prefix}.desktop_sessions"),
                &provider.desktop_sessions,
            );
            for (field, key) in provider.fields.entries() {
                if key.is_empty()
                    || key.len() > MAX_LOCAL_ID_BYTES
                    || !key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    issues.push(ValidationIssue::new(
                        format!("{prefix}.fields.{field}"),
                        "must be a non-empty flat TOML key containing only ASCII letters, digits, '-' or '_'",
                    ));
                }
            }
        }

        if self.bars.len() > MAX_PRESENTATION_BARS {
            issues.push(ValidationIssue::new(
                "bar",
                format!("a package may declare at most {MAX_PRESENTATION_BARS} presentation bars"),
            ));
        }
        let mut bar_ids = BTreeSet::new();
        for (index, bar) in self.bars.iter().enumerate() {
            let prefix = format!("bar[{index}]");
            validate_kebab_id(&mut issues, &format!("{prefix}.id"), &bar.id);
            if bar.minimum_width == 0
                || bar.minimum_width > bar.preferred_width
                || bar.preferred_width > bar.maximum_width
                || bar.maximum_width > MAX_PRESENTATION_SPACE
            {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.minimum_width"),
                    format!(
                        "presentation sizing must satisfy 1 <= minimum_width <= preferred_width <= maximum_width <= {MAX_PRESENTATION_SPACE}"
                    ),
                ));
            }
            if !bar_ids.insert(&bar.id) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.id"),
                    "duplicate presentation bar id",
                ));
            }
            if bar.elements.is_empty() {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.element"),
                    "a presentation bar must contain at least one element",
                ));
            }
            let mut content = PresentationContentValidation::default();
            for (element_index, element) in bar.elements.iter().enumerate() {
                let field = format!("{prefix}.element[{element_index}]");
                let sizing = validate_presentation_bar_element(
                    &mut issues,
                    element,
                    &field,
                    0,
                    &item_ids,
                    &mut content,
                );
                content.sizing.minimum = content.sizing.minimum.saturating_add(sizing.minimum);
                content.sizing.preferred =
                    content.sizing.preferred.saturating_add(sizing.preferred);
                content.sizing.maximum = content.sizing.maximum.saturating_add(sizing.maximum);
            }
            if content.element_count > MAX_PRESENTATION_BAR_ELEMENTS {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.element"),
                    format!(
                        "a presentation bar may contain at most {MAX_PRESENTATION_BAR_ELEMENTS} elements across its group tree"
                    ),
                ));
            }
            if content.sizing.minimum > u64::from(bar.minimum_width) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.minimum_width"),
                    format!(
                        "must be at least the {}-pixel sum of element minimums",
                        content.sizing.minimum
                    ),
                ));
            }
            if content.items.is_empty() {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.element"),
                    "a presentation bar must contain at least one item",
                ));
            }
            if let Some(principal) = &bar.principal_item {
                validate_kebab_id(&mut issues, &format!("{prefix}.principal_item"), principal);
                if !content.items.contains(principal) {
                    issues.push(ValidationIssue::new(
                        format!("{prefix}.principal_item"),
                        "must name an item in this presentation bar",
                    ));
                }
            }
        }
        for (index, item) in self.items.iter().enumerate() {
            for (field, target) in [
                ("expanded_bar", &item.expanded_bar),
                ("press_and_hold_bar", &item.press_and_hold_bar),
            ] {
                if let Some(target) = target {
                    validate_kebab_id(&mut issues, &format!("items[{index}].{field}"), target);
                    if !bar_ids.contains(target) {
                        issues.push(ValidationIssue::new(
                            format!("items[{index}].{field}"),
                            "references an unknown presentation bar",
                        ));
                    }
                }
            }
        }

        match &self.runtime {
            RuntimeSpec::Component { entrypoint, world } => {
                validate_package_path(&mut issues, "runtime.entrypoint", entrypoint);
                if Path::new(entrypoint)
                    .extension()
                    .and_then(|value| value.to_str())
                    != Some("wasm")
                {
                    issues.push(ValidationIssue::new(
                        "runtime.entrypoint",
                        "a component entrypoint must have a .wasm extension",
                    ));
                }
                validate_world(&mut issues, world);
            }
            RuntimeSpec::Native { targets } => {
                if targets.is_empty() {
                    issues.push(ValidationIssue::new(
                        "runtime.targets",
                        "a native runtime must provide at least one target artifact",
                    ));
                }
                let mut triples = BTreeSet::new();
                for (index, target) in targets.iter().enumerate() {
                    let prefix = format!("runtime.targets[{index}]");
                    validate_target_triple(
                        &mut issues,
                        &format!("{prefix}.target"),
                        &target.target,
                    );
                    validate_package_path(
                        &mut issues,
                        &format!("{prefix}.entrypoint"),
                        &target.entrypoint,
                    );
                    if !triples.insert(&target.target) {
                        issues.push(ValidationIssue::new(
                            format!("{prefix}.target"),
                            "duplicate native target",
                        ));
                    }
                }
            }
        }

        let mut asset_ids = BTreeSet::new();
        let mut asset_paths = BTreeSet::new();
        let mut total_asset_pixels = 0_u64;
        if self.assets.len() > MAX_ASSETS {
            issues.push(ValidationIssue::new(
                "asset",
                format!("a package may declare at most {MAX_ASSETS} assets"),
            ));
        }
        for (index, asset) in self.assets.iter().enumerate() {
            let prefix = format!("asset[{index}]");
            validate_kebab_id(&mut issues, &format!("{prefix}.id"), &asset.id);
            validate_package_path(&mut issues, &format!("{prefix}.path"), &asset.path);
            let path = Path::new(&asset.path);
            if path.components().next() != Some(Component::Normal(std::ffi::OsStr::new("assets"))) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.path"),
                    "an asset path must be beneath assets/",
                ));
            }
            let expected_extension = match asset.kind {
                AssetKind::Png => "png",
                AssetKind::SymbolicSvg => "svg",
            };
            if path.extension().and_then(|value| value.to_str()) != Some(expected_extension) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.path"),
                    format!(
                        "a {} asset must have a .{expected_extension} extension",
                        asset.kind
                    ),
                ));
            }
            if !asset_ids.insert(&asset.id) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.id"),
                    "duplicate asset id",
                ));
            }
            if !asset_paths.insert(&asset.path) {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.path"),
                    "duplicate asset path",
                ));
            }
            if asset.width == 0 || asset.width > MAX_ASSET_WIDTH {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.width"),
                    format!("asset width must be between 1 and {MAX_ASSET_WIDTH}"),
                ));
            }
            if asset.height == 0 || asset.height > MAX_ASSET_HEIGHT {
                issues.push(ValidationIssue::new(
                    format!("{prefix}.height"),
                    format!("asset height must be between 1 and {MAX_ASSET_HEIGHT}"),
                ));
            }
            total_asset_pixels =
                total_asset_pixels.saturating_add(u64::from(asset.width) * u64::from(asset.height));
        }
        if total_asset_pixels > MAX_ASSET_PIXELS {
            issues.push(ValidationIssue::new(
                "asset",
                format!("declared assets exceed the {MAX_ASSET_PIXELS}-pixel package budget"),
            ));
        }

        for (index, permission) in self.permissions.iter().enumerate() {
            validate_dotted_id(
                &mut issues,
                &format!("permission[{index}].capability"),
                &permission.capability,
            );
            validate_nonempty(
                &mut issues,
                &format!("permission[{index}].reason"),
                &permission.reason,
            );
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(issues))
        }
    }

    pub fn supports_host_api(&self, host_api: &Version) -> bool {
        self.plugin.api.matches(host_api)
    }

    pub fn artifact_paths(&self) -> ArtifactPaths<'_> {
        let runtime = match &self.runtime {
            RuntimeSpec::Component { entrypoint, .. } => {
                RuntimeArtifactPaths::One(Some(entrypoint.as_str()))
            }
            RuntimeSpec::Native { targets } => RuntimeArtifactPaths::Many(targets.iter()),
        };
        ArtifactPaths {
            runtime,
            assets: self.assets.iter(),
        }
    }
}

impl FromStr for PluginManifest {
    type Err = ManifestError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        Self::from_toml(source)
    }
}

enum RuntimeArtifactPaths<'a> {
    One(Option<&'a str>),
    Many(std::slice::Iter<'a, NativeTarget>),
}

impl<'a> Iterator for RuntimeArtifactPaths<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::One(value) => value.take(),
            Self::Many(targets) => targets.next().map(|target| target.entrypoint.as_str()),
        }
    }
}

pub struct ArtifactPaths<'a> {
    runtime: RuntimeArtifactPaths<'a>,
    assets: std::slice::Iter<'a, AssetDefinition>,
}

impl<'a> Iterator for ArtifactPaths<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.runtime
            .next()
            .or_else(|| self.assets.next().map(|asset| asset.path.as_str()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMetadata {
    pub name: String,
    pub version: Version,
    pub description: String,
    pub license: String,
    pub source: GithubSource,
    pub api: VersionReq,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

/// Canonical decentralized package identity. Display names and catalog aliases
/// are intentionally not identities.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GithubSource {
    owner: String,
    repository: String,
}

impl GithubSource {
    pub fn new(owner: impl Into<String>, repository: impl Into<String>) -> Result<Self, String> {
        let source = Self {
            owner: owner.into(),
            repository: repository.into(),
        };
        source.validate()?;
        Ok(source)
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    fn validate(&self) -> Result<(), String> {
        if !valid_github_owner(&self.owner) {
            return Err(
                "GitHub owner must be lowercase ASCII letters, digits, or single hyphens".into(),
            );
        }
        if !valid_github_repository(&self.repository) {
            return Err(
                "GitHub repository must be a lowercase name without a .git suffix or path separators"
                    .into(),
            );
        }
        Ok(())
    }
}

impl fmt::Display for GithubSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "github:{}/{}", self.owner, self.repository)
    }
}

impl FromStr for GithubSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let path = value
            .strip_prefix("github:")
            .ok_or_else(|| "source must begin with github:".to_owned())?;
        let (owner, repository) = path
            .split_once('/')
            .ok_or_else(|| "source must have the form github:owner/repository".to_owned())?;
        if repository.contains('/') {
            return Err("source must have the form github:owner/repository".into());
        }
        Self::new(owner, repository)
    }
}

impl Serialize for GithubSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for GithubSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RuntimeSpec {
    Component { entrypoint: String, world: String },
    Native { targets: Vec<NativeTarget> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTarget {
    pub target: String,
    pub entrypoint: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemContribution {
    pub id: String,
    pub label: String,
    #[serde(default = "default_item_width")]
    pub default_width: u32,
    #[serde(default = "default_true")]
    pub show_in_default_profile: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded_bar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press_and_hold_bar: Option<String>,
}

fn default_item_width() -> u32 {
    160
}

/// A package-owned profile which becomes eligible from exact desktop context.
///
/// Automatic profiles are intentionally narrower than user profile documents:
/// they may arrange only items from their own package, and activation is an
/// exact application or foreground layer identity. The host evaluates these
/// declarations; plugin code never receives the focused application.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticProfile {
    pub id: String,
    pub label: String,
    pub items: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_item: Option<String>,
    #[serde(default)]
    pub applications: Vec<String>,
    #[serde(default)]
    pub activities: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,
}

/// A package-owned source of semantic Touch Bar colors.
///
/// The session daemon, rather than plugin code, reads this bounded file through
/// an installer-owned filesystem mount binding. `appearance.provide.v1`
/// separately controls whether the resulting palette may become global.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceProvider {
    pub id: String,
    pub label: String,
    pub mount: String,
    pub path: String,
    pub desktop_sessions: Vec<String>,
    #[serde(default)]
    pub fields: AppearanceProviderFields,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppearanceProviderFields {
    pub scheme: String,
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub selection: String,
    pub muted: String,
    pub destructive: String,
}

impl Default for AppearanceProviderFields {
    fn default() -> Self {
        Self {
            scheme: "mode".into(),
            background: "background".into(),
            foreground: "foreground".into(),
            accent: "accent".into(),
            selection: "selection".into(),
            muted: "muted".into(),
            destructive: "red".into(),
        }
    }
}

impl AppearanceProviderFields {
    fn entries(&self) -> [(&'static str, &str); 7] {
        [
            ("scheme", &self.scheme),
            ("background", &self.background),
            ("foreground", &self.foreground),
            ("accent", &self.accent),
            ("selection", &self.selection),
            ("muted", &self.muted),
            ("destructive", &self.destructive),
        ]
    }
}

fn default_true() -> bool {
    true
}

/// One package-owned ordered bar used as presentation content. References are
/// deliberately local item IDs; the installer/session compositor qualifies
/// them with the immutable package identity before composition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationBar {
    pub id: String,
    pub minimum_width: u32,
    pub preferred_width: u32,
    pub maximum_width: u32,
    pub dismiss_on_selection: bool,
    #[serde(default, rename = "element")]
    pub elements: Vec<PresentationBarElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_item: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PresentationBarElement {
    Item {
        item: String,
        minimum_width: u32,
        preferred_width: u32,
        maximum_width: u32,
    },
    FixedSpace {
        width: u32,
    },
    FlexibleSpace {
        minimum: u32,
        weight: u32,
    },
    /// An atomic, recursively composited cluster. Groups intentionally cannot
    /// contain spaces: their child geometry is entirely owned by the group.
    Group {
        id: String,
        #[serde(default)]
        layout: PresentationGroupLayout,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        visibility_priority: i32,
        #[serde(default)]
        compression_priority: i32,
        #[serde(default, rename = "element")]
        elements: Vec<PresentationGroupElement>,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentationGroupLayout {
    #[default]
    Natural,
    EqualWidth,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PresentationGroupElement {
    Item {
        item: String,
        minimum_width: u32,
        preferred_width: u32,
        maximum_width: u32,
    },
    Group {
        id: String,
        #[serde(default)]
        layout: PresentationGroupLayout,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        visibility_priority: i32,
        #[serde(default)]
        compression_priority: i32,
        #[serde(default, rename = "element")]
        elements: Vec<PresentationGroupElement>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetKind {
    Png,
    SymbolicSvg,
}

impl fmt::Display for AssetKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Png => "png",
            Self::SymbolicSvg => "symbolic-svg",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetDefinition {
    pub id: String,
    pub path: String,
    pub kind: AssetKind,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequest {
    pub capability: String,
    pub required: bool,
    pub reason: String,
    #[serde(default)]
    pub scope: BTreeMap<String, toml::Value>,
}

#[derive(Debug)]
pub enum ManifestError {
    Parse(toml::de::Error),
    Invalid(ValidationErrors),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => {
                write!(formatter, "could not parse {MANIFEST_FILE_NAME}: {error}")
            }
            Self::Invalid(errors) => errors.fmt(formatter),
        }
    }
}

impl Error for ManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Invalid(error) => Some(error),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
}

impl ValidationIssue {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationErrors(pub Vec<ValidationIssue>);

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "invalid {MANIFEST_FILE_NAME}:")?;
        for issue in &self.0 {
            writeln!(formatter, "- {}: {}", issue.field, issue.message)?;
        }
        Ok(())
    }
}

impl Error for ValidationErrors {}

#[derive(Clone, Copy, Debug, Default)]
struct PresentationSizing {
    minimum: u64,
    preferred: u64,
    maximum: u64,
}

#[derive(Debug, Default)]
struct PresentationContentValidation {
    element_count: usize,
    items: BTreeSet<String>,
    groups: BTreeSet<String>,
    sizing: PresentationSizing,
}

fn validate_presentation_bar_element(
    issues: &mut Vec<ValidationIssue>,
    element: &PresentationBarElement,
    field: &str,
    depth: usize,
    declared_items: &BTreeSet<&String>,
    content: &mut PresentationContentValidation,
) -> PresentationSizing {
    content.element_count = content.element_count.saturating_add(1);
    match element {
        PresentationBarElement::Item {
            item,
            minimum_width,
            preferred_width,
            maximum_width,
        } => validate_presentation_item(
            issues,
            field,
            item,
            *minimum_width,
            *preferred_width,
            *maximum_width,
            declared_items,
            &mut content.items,
        ),
        PresentationBarElement::FixedSpace { width } => {
            let sizing = PresentationSizing {
                minimum: u64::from(*width),
                preferred: u64::from(*width),
                maximum: u64::from(*width),
            };
            if *width == 0 || *width > MAX_PRESENTATION_SPACE {
                issues.push(ValidationIssue::new(
                    format!("{field}.width"),
                    format!("fixed-space width must be between 1 and {MAX_PRESENTATION_SPACE}"),
                ));
            }
            sizing
        }
        PresentationBarElement::FlexibleSpace { minimum, weight } => {
            let sizing = PresentationSizing {
                minimum: u64::from(*minimum),
                preferred: u64::from(*minimum),
                maximum: u64::from(MAX_PRESENTATION_SPACE),
            };
            if *minimum > MAX_PRESENTATION_SPACE {
                issues.push(ValidationIssue::new(
                    format!("{field}.minimum"),
                    format!("flexible-space minimum must not exceed {MAX_PRESENTATION_SPACE}"),
                ));
            }
            if *weight == 0 || *weight > MAX_FLEXIBLE_SPACE_WEIGHT {
                issues.push(ValidationIssue::new(
                    format!("{field}.weight"),
                    format!(
                        "flexible-space weight must be between 1 and {MAX_FLEXIBLE_SPACE_WEIGHT}"
                    ),
                ));
            }
            sizing
        }
        PresentationBarElement::Group {
            id,
            layout,
            spacing,
            elements,
            ..
        } => validate_presentation_group(
            issues,
            id,
            *layout,
            *spacing,
            elements,
            field,
            depth.saturating_add(1),
            declared_items,
            content,
        ),
    }
}

fn validate_presentation_group_element(
    issues: &mut Vec<ValidationIssue>,
    element: &PresentationGroupElement,
    field: &str,
    depth: usize,
    declared_items: &BTreeSet<&String>,
    content: &mut PresentationContentValidation,
) -> PresentationSizing {
    content.element_count = content.element_count.saturating_add(1);
    match element {
        PresentationGroupElement::Item {
            item,
            minimum_width,
            preferred_width,
            maximum_width,
        } => validate_presentation_item(
            issues,
            field,
            item,
            *minimum_width,
            *preferred_width,
            *maximum_width,
            declared_items,
            &mut content.items,
        ),
        PresentationGroupElement::Group {
            id,
            layout,
            spacing,
            elements,
            ..
        } => validate_presentation_group(
            issues,
            id,
            *layout,
            *spacing,
            elements,
            field,
            depth.saturating_add(1),
            declared_items,
            content,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_presentation_group(
    issues: &mut Vec<ValidationIssue>,
    id: &str,
    layout: PresentationGroupLayout,
    spacing: u32,
    elements: &[PresentationGroupElement],
    field: &str,
    depth: usize,
    declared_items: &BTreeSet<&String>,
    content: &mut PresentationContentValidation,
) -> PresentationSizing {
    validate_kebab_id(issues, &format!("{field}.id"), id);
    if !content.groups.insert(id.to_owned()) {
        issues.push(ValidationIssue::new(
            format!("{field}.id"),
            "duplicate group id in presentation bar",
        ));
    }
    if depth > MAX_PRESENTATION_GROUP_DEPTH {
        issues.push(ValidationIssue::new(
            format!("{field}.element"),
            format!(
                "presentation groups may be nested at most {MAX_PRESENTATION_GROUP_DEPTH} levels"
            ),
        ));
    }
    if elements.is_empty() {
        issues.push(ValidationIssue::new(
            format!("{field}.element"),
            "a presentation group must contain at least one item or group",
        ));
        return PresentationSizing::default();
    }
    if spacing > MAX_PRESENTATION_SPACE {
        issues.push(ValidationIssue::new(
            format!("{field}.spacing"),
            format!("group spacing must not exceed {MAX_PRESENTATION_SPACE}"),
        ));
    }

    let mut children = Vec::with_capacity(elements.len());
    for (index, element) in elements.iter().enumerate() {
        children.push(validate_presentation_group_element(
            issues,
            element,
            &format!("{field}.element[{index}]"),
            depth,
            declared_items,
            content,
        ));
    }
    let gaps = u64::try_from(elements.len().saturating_sub(1)).unwrap_or(u64::MAX);
    let gap_width = u64::from(spacing).saturating_mul(gaps);
    match layout {
        PresentationGroupLayout::Natural => PresentationSizing {
            minimum: children.iter().fold(gap_width, |total, child| {
                total.saturating_add(child.minimum)
            }),
            preferred: children.iter().fold(gap_width, |total, child| {
                total.saturating_add(child.preferred)
            }),
            maximum: children.iter().fold(gap_width, |total, child| {
                total.saturating_add(child.maximum)
            }),
        },
        PresentationGroupLayout::EqualWidth => {
            let common_minimum = children
                .iter()
                .map(|child| child.minimum)
                .max()
                .unwrap_or(0);
            let common_maximum = children
                .iter()
                .map(|child| child.maximum)
                .min()
                .unwrap_or(0);
            if common_minimum > common_maximum {
                issues.push(ValidationIssue::new(
                    format!("{field}.element"),
                    "equal-width group children have incompatible width ranges",
                ));
            }
            let preferred_candidate = children
                .iter()
                .map(|child| child.preferred)
                .max()
                .unwrap_or(common_minimum);
            let preferred = if common_minimum <= common_maximum {
                preferred_candidate.clamp(common_minimum, common_maximum)
            } else {
                common_minimum
            };
            let count = u64::try_from(children.len()).unwrap_or(u64::MAX);
            PresentationSizing {
                minimum: common_minimum
                    .saturating_mul(count)
                    .saturating_add(gap_width),
                preferred: preferred.saturating_mul(count).saturating_add(gap_width),
                maximum: common_maximum
                    .saturating_mul(count)
                    .saturating_add(gap_width),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_presentation_item(
    issues: &mut Vec<ValidationIssue>,
    field: &str,
    item: &str,
    minimum_width: u32,
    preferred_width: u32,
    maximum_width: u32,
    declared_items: &BTreeSet<&String>,
    items: &mut BTreeSet<String>,
) -> PresentationSizing {
    validate_kebab_id(issues, &format!("{field}.item"), item);
    if minimum_width == 0
        || minimum_width > preferred_width
        || preferred_width > maximum_width
        || maximum_width > MAX_PRESENTATION_SPACE
    {
        issues.push(ValidationIssue::new(
            format!("{field}.minimum_width"),
            format!(
                "item sizing must satisfy 1 <= minimum_width <= preferred_width <= maximum_width <= {MAX_PRESENTATION_SPACE}"
            ),
        ));
    }
    if !declared_items
        .iter()
        .any(|declared| declared.as_str() == item)
    {
        issues.push(ValidationIssue::new(
            format!("{field}.item"),
            "references an item not declared by this package",
        ));
    }
    if !items.insert(item.to_owned()) {
        issues.push(ValidationIssue::new(
            format!("{field}.item"),
            "duplicates an item in the same presentation bar",
        ));
    }
    PresentationSizing {
        minimum: u64::from(minimum_width),
        preferred: u64::from(preferred_width),
        maximum: u64::from(maximum_width),
    }
}

fn validate_nonempty(issues: &mut Vec<ValidationIssue>, field: &str, value: &str) {
    if value.trim().is_empty() {
        issues.push(ValidationIssue::new(field, "must not be empty"));
    }
}

fn validate_context_identities(issues: &mut Vec<ValidationIssue>, field: &str, values: &[String]) {
    let mut unique = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let valid = !value.is_empty()
            // `activity.` is prepended by the compositor and profile context
            // keys are bounded to 128 bytes.
            && value.len() <= 112
            && value == value.trim()
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.' | b':')
            });
        if !valid {
            issues.push(ValidationIssue::new(
                format!("{field}[{index}]"),
                "must be an exact lowercase application or activity identity",
            ));
        }
        if !unique.insert(value) {
            issues.push(ValidationIssue::new(
                format!("{field}[{index}]"),
                format!("repeats context identity `{value}`"),
            ));
        }
    }
}

fn validate_kebab_id(issues: &mut Vec<ValidationIssue>, field: &str, value: &str) {
    if value.is_empty()
        || value.len() > MAX_LOCAL_ID_BYTES
        || value.starts_with('-')
        || value.ends_with('-')
        || value.contains("--")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value.as_bytes()[0].is_ascii_lowercase()
    {
        issues.push(ValidationIssue::new(
            field,
            format!(
                "must be a lowercase kebab-case identifier beginning with a letter and at most {MAX_LOCAL_ID_BYTES} bytes"
            ),
        ));
    }
}

fn validate_dotted_id(issues: &mut Vec<ValidationIssue>, field: &str, value: &str) {
    if value.split('.').any(|segment| {
        segment.is_empty()
            || segment.starts_with('-')
            || segment.ends_with('-')
            || segment.contains("--")
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || !segment.as_bytes()[0].is_ascii_lowercase()
    }) {
        issues.push(ValidationIssue::new(
            field,
            "must be a dot-separated lowercase identifier",
        ));
    }
}

fn validate_package_path(issues: &mut Vec<ValidationIssue>, field: &str, value: &str) {
    let path = Path::new(value);
    let valid = !value.is_empty()
        && !value.contains('\\')
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !valid {
        issues.push(ValidationIssue::new(
            field,
            "must be a normalized relative path contained within the package",
        ));
    }
}

fn validate_relative_path(issues: &mut Vec<ValidationIssue>, field: &str, value: &str) {
    let path = Path::new(value);
    let valid = !value.is_empty()
        && !value.contains('\\')
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !valid {
        issues.push(ValidationIssue::new(
            field,
            "must be a normalized relative path contained within the granted mount",
        ));
    }
}

fn validate_world(issues: &mut Vec<ValidationIssue>, world: &str) {
    if world != SUPPORTED_COMPONENT_WORLD {
        issues.push(ValidationIssue::new(
            "runtime.world",
            format!("must be {SUPPORTED_COMPONENT_WORLD}"),
        ));
    }
}

fn validate_target_triple(issues: &mut Vec<ValidationIssue>, field: &str, value: &str) {
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() < 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
    {
        issues.push(ValidationIssue::new(
            field,
            "must be a lowercase Rust-style target triple",
        ));
    }
}

fn valid_github_owner(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 39
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_github_repository(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.ends_with(".git")
        && !value.contains('/')
        && !value.contains("..")
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPONENT: &str = include_str!("../tests/fixtures/valid-component.toml");
    const NATIVE: &str = include_str!("../tests/fixtures/valid-native.toml");

    #[test]
    fn parses_component_pack_and_exposes_artifact() {
        let manifest = PluginManifest::from_toml(COMPONENT).unwrap();
        assert_eq!(manifest.plugin.source.owner(), "alice");
        assert_eq!(manifest.items.len(), 2);
        assert_eq!(manifest.bars.len(), 2);
        assert_eq!(
            manifest.items[0].expanded_bar.as_deref(),
            Some("media-expanded")
        );
        assert_eq!(
            manifest.bars[0].principal_item.as_deref(),
            Some("media-timeline")
        );
        assert_eq!(
            (
                manifest.bars[0].minimum_width,
                manifest.bars[0].preferred_width,
                manifest.bars[0].maximum_width
            ),
            (256, 520, 1004)
        );
        assert!(manifest.supports_host_api(&Version::new(1, 0, 7)));
        assert_eq!(
            manifest.artifact_paths().collect::<Vec<_>>(),
            ["component/plugin.wasm"]
        );
    }

    #[test]
    fn parses_multi_arch_native_pack() {
        let manifest = PluginManifest::from_toml(NATIVE).unwrap();
        assert!(matches!(manifest.runtime, RuntimeSpec::Native { .. }));
        assert_eq!(manifest.artifact_paths().count(), 2);
    }

    #[test]
    fn declared_assets_are_typed_bounded_and_locked_as_artifacts() {
        let source = format!(
            "{COMPONENT}\n\n[[asset]]\nid = \"album-art\"\npath = \"assets/album.png\"\nkind = \"png\"\nwidth = 160\nheight = 60\n\n[[asset]]\nid = \"touchbar-mark\"\npath = \"assets/touchbar.svg\"\nkind = \"symbolic-svg\"\nwidth = 24\nheight = 24\n"
        );
        let manifest = PluginManifest::from_toml(&source).unwrap();
        assert_eq!(
            manifest.artifact_paths().collect::<Vec<_>>(),
            [
                "component/plugin.wasm",
                "assets/album.png",
                "assets/touchbar.svg"
            ]
        );
        assert_eq!(manifest.assets[1].kind, AssetKind::SymbolicSvg);
    }

    #[test]
    fn asset_paths_cannot_escape_or_alias_and_kind_must_match_extension() {
        let source = format!(
            "{COMPONENT}\n\n[[asset]]\nid = \"mark\"\npath = \"../mark.png\"\nkind = \"symbolic-svg\"\nwidth = 0\nheight = 241\n\n[[asset]]\nid = \"mark\"\npath = \"../mark.png\"\nkind = \"png\"\nwidth = 1\nheight = 1\n"
        );
        let error = PluginManifest::from_toml(&source).unwrap_err().to_string();
        for expected in [
            "must be a normalized relative path contained within the package",
            "must be beneath assets/",
            "must have a .svg extension",
            "duplicate asset id",
            "duplicate asset path",
            "asset width must be between",
            "asset height must be between",
        ] {
            assert!(error.contains(expected), "missing `{expected}` in {error}");
        }
    }

    #[test]
    fn rejects_a_different_component_world_version() {
        let error = PluginManifest::from_toml(
            &COMPONENT.replace(SUPPORTED_COMPONENT_WORLD, "touchbar:plugin/plugin@0.2.0"),
        )
        .unwrap_err();
        assert!(error.to_string().contains(SUPPORTED_COMPONENT_WORLD));
    }

    #[test]
    fn rejects_unknown_fields_before_execution() {
        let error = PluginManifest::from_toml(&COMPONENT.replace(
            "description = \"A theme-aware MPRIS controller\"",
            "description = \"A theme-aware MPRIS controller\"\ntrusted = true",
        ))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `trusted`"));
    }

    #[test]
    fn presentation_bars_are_package_local_bounded_and_strict() {
        let source = COMPONENT
            .replace("\nitem = \"media-timeline\"", "\nitem = \"foreign-item\"")
            .replace("width = 8", "width = 0")
            .replace("weight = 1", "weight = 0")
            .replace(
                "principal_item = \"media-timeline\"",
                "principal_item = \"missing\"",
            );
        let error = PluginManifest::from_toml(&source).unwrap_err().to_string();
        for expected in [
            "references an item not declared by this package",
            "fixed-space width must be between",
            "flexible-space weight must be between",
            "must name an item in this presentation bar",
        ] {
            assert!(error.contains(expected), "missing `{expected}` in {error}");
        }

        let error = PluginManifest::from_toml(&COMPONENT.replace(
            "expanded_bar = \"media-expanded\"",
            "expanded_bar = \"missing\"",
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("references an unknown presentation bar"));
    }

    #[test]
    fn presentation_bar_sizing_is_ordered_and_bounded() {
        let source = COMPONENT.replace("minimum_width = 256", "minimum_width = 600");
        let error = PluginManifest::from_toml(&source).unwrap_err().to_string();
        assert!(error.contains("presentation sizing must satisfy"));
    }

    #[test]
    fn automatic_profiles_are_exact_package_local_declarations() {
        let source = format!(
            "{COMPONENT}\n\n{}",
            r#"[[profile]]
id = "firefox"
label = "Firefox"
items = ["now-playing", "media-timeline"]
principal_item = "media-timeline"
applications = ["firefox", "org.mozilla.firefox"]
activities = ["omarchy-image-selector"]
"#
        );
        let manifest = PluginManifest::from_toml(&source).unwrap();
        assert_eq!(manifest.items[0].default_width, 160);
        assert_eq!(manifest.profiles.len(), 1);
        assert!(manifest.profiles[0].enabled_by_default);
        assert_eq!(manifest.profiles[0].applications[0], "firefox");
    }

    #[test]
    fn automatic_profiles_reject_ambiguous_or_foreign_content() {
        let source = format!(
            "{COMPONENT}\n\n{}",
            r#"[[profile]]
id = "firefox"
label = "Firefox"
items = ["missing", "missing"]
principal_item = "now-playing"
applications = ["Firefox", "Firefox"]
"#
        );
        let error = PluginManifest::from_toml(&source).unwrap_err().to_string();
        for expected in [
            "references unknown package item `missing`",
            "repeats package item `missing`",
            "must reference an item in this automatic profile",
            "must be an exact lowercase application or activity identity",
            "repeats context identity `Firefox`",
        ] {
            assert!(error.contains(expected), "missing `{expected}` in {error}");
        }

        let missing_context = format!(
            "{COMPONENT}\n\n{}",
            r#"[[profile]]
id = "firefox"
label = "Firefox"
items = ["now-playing"]
"#
        );
        assert!(
            PluginManifest::from_toml(&missing_context)
                .unwrap_err()
                .to_string()
                .contains("exact identities in total")
        );
    }

    #[test]
    fn presentation_bar_minimum_covers_every_required_element() {
        let source = COMPONENT.replace("minimum_width = 256", "minimum_width = 255");
        let error = PluginManifest::from_toml(&source).unwrap_err().to_string();
        assert!(
            error.contains("must be at least the 256-pixel sum of element minimums"),
            "{error}"
        );
    }

    #[test]
    fn presentation_bar_rejects_duplicate_items_and_unknown_fields() {
        let duplicate =
            COMPONENT.replace("\nitem = \"media-timeline\"", "\nitem = \"now-playing\"");
        assert!(
            PluginManifest::from_toml(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicates an item in the same presentation bar")
        );

        let unknown = COMPONENT.replace(
            "kind = \"fixed-space\"",
            "kind = \"fixed-space\"\nweight = 2",
        );
        assert!(
            PluginManifest::from_toml(&unknown)
                .unwrap_err()
                .to_string()
                .contains("unknown field `weight`")
        );
    }

    #[test]
    fn presentation_groups_are_recursive_bounded_and_item_unique() {
        let source = format!(
            r#"
manifest_version = 1

[plugin]
name = "Grouped controls"
version = "1.0.0"
description = "A grouped presentation"
license = "MIT"
source = "github:alice/grouped-controls"
api = "^1.0"

[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "{SUPPORTED_COMPONENT_WORLD}"

[[items]]
id = "left"
label = "Left"

[[items]]
id = "right"
label = "Right"

[[bar]]
id = "grouped"
minimum_width = 108
preferred_width = 160
maximum_width = 400
dismiss_on_selection = true
principal_item = "right"

[[bar.element]]
kind = "group"
id = "controls"
layout = "equal-width"
spacing = 8
visibility_priority = -10
compression_priority = 10

[[bar.element.element]]
kind = "item"
item = "left"
minimum_width = 50
preferred_width = 70
maximum_width = 100

[[bar.element.element]]
kind = "group"
id = "nested"
layout = "natural"

[[bar.element.element.element]]
kind = "item"
item = "right"
minimum_width = 50
preferred_width = 60
maximum_width = 80
"#
        );
        let manifest = PluginManifest::from_toml(&source).unwrap();
        let PresentationBarElement::Group {
            id,
            layout,
            elements,
            ..
        } = &manifest.bars[0].elements[0]
        else {
            panic!("expected group");
        };
        assert_eq!(id, "controls");
        assert_eq!(*layout, PresentationGroupLayout::EqualWidth);
        assert_eq!(elements.len(), 2);

        let duplicate = source.replace("item = \"right\"", "item = \"left\"");
        assert!(
            PluginManifest::from_toml(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicates an item in the same presentation bar")
        );

        let incompatible = source
            .replace("maximum_width = 80", "maximum_width = 40")
            .replace(
                "minimum_width = 50\npreferred_width = 60\nmaximum_width = 40",
                "minimum_width = 40\npreferred_width = 40\nmaximum_width = 40",
            );
        assert!(
            PluginManifest::from_toml(&incompatible)
                .unwrap_err()
                .to_string()
                .contains("equal-width group children have incompatible width ranges")
        );
    }

    #[test]
    fn presentation_groups_reject_duplicate_ids_and_excessive_depth() {
        let mut group = PresentationGroupElement::Item {
            item: "now-playing".to_owned(),
            minimum_width: 10,
            preferred_width: 10,
            maximum_width: 10,
        };
        for depth in 0..=MAX_PRESENTATION_GROUP_DEPTH {
            group = PresentationGroupElement::Group {
                id: format!("depth-{depth}"),
                layout: PresentationGroupLayout::Natural,
                spacing: 0,
                visibility_priority: 0,
                compression_priority: 0,
                elements: vec![group],
            };
        }
        let manifest = PluginManifest {
            manifest_version: SUPPORTED_MANIFEST_VERSION,
            plugin: PluginMetadata {
                name: "Nested".to_owned(),
                version: Version::new(1, 0, 0),
                description: "Nested group validation".to_owned(),
                license: "MIT".to_owned(),
                source: GithubSource::from_str("github:alice/nested").unwrap(),
                api: VersionReq::parse("^1.0").unwrap(),
                authors: Vec::new(),
                homepage: None,
            },
            runtime: RuntimeSpec::Component {
                entrypoint: "component/plugin.wasm".to_owned(),
                world: SUPPORTED_COMPONENT_WORLD.to_owned(),
            },
            items: vec![ItemContribution {
                id: "now-playing".to_owned(),
                label: "Now playing".to_owned(),
                default_width: default_item_width(),
                show_in_default_profile: true,
                expanded_bar: None,
                press_and_hold_bar: None,
            }],
            bars: vec![PresentationBar {
                id: "nested".to_owned(),
                minimum_width: 10,
                preferred_width: 10,
                maximum_width: 10,
                dismiss_on_selection: false,
                elements: vec![PresentationBarElement::Group {
                    id: "depth-0".to_owned(),
                    layout: PresentationGroupLayout::Natural,
                    spacing: 0,
                    visibility_priority: 0,
                    compression_priority: 0,
                    elements: vec![group],
                }],
                principal_item: None,
            }],
            profiles: Vec::new(),
            appearance_providers: Vec::new(),
            assets: Vec::new(),
            permissions: Vec::new(),
        };
        let error = manifest.validate().unwrap_err().to_string();
        assert!(error.contains("duplicate group id in presentation bar"));
        assert!(error.contains("presentation groups may be nested at most"));
    }

    #[test]
    fn reports_all_semantic_errors_together() {
        let source = COMPONENT
            .replace("manifest_version = 1", "manifest_version = 9")
            .replace("id = \"now-playing\"", "id = \"Bad ID\"")
            .replace("id = \"media-timeline\"", "id = \"Bad ID\"")
            .replace("component/plugin.wasm", "../plugin.bin")
            .replace("Read and control MPRIS players", "  ");
        let ManifestError::Invalid(errors) = PluginManifest::from_toml(&source).unwrap_err() else {
            panic!("expected semantic validation errors");
        };
        assert!(errors.0.len() >= 6, "{errors}");
        assert!(
            errors
                .0
                .iter()
                .any(|issue| issue.field == "manifest_version")
        );
        assert!(
            errors
                .0
                .iter()
                .any(|issue| issue.message == "duplicate item id")
        );
        assert!(
            errors
                .0
                .iter()
                .any(|issue| issue.field == "runtime.entrypoint")
        );
        assert!(
            errors
                .0
                .iter()
                .any(|issue| issue.field == "permission[0].reason")
        );
    }

    #[test]
    fn appearance_provider_is_bounded_and_defaults_to_canonical_palette_keys() {
        let source = format!(
            r#"{COMPONENT}

[[appearance-provider]]
id = "desktop-theme"
label = "Desktop theme"
mount = "desktop-state"
path = "current/theme/colors.toml"
desktop_sessions = ["example-desktop"]
"#
        );
        let manifest = PluginManifest::from_toml(&source).unwrap();
        let provider = &manifest.appearance_providers[0];
        assert_eq!(provider.fields, AppearanceProviderFields::default());
        assert_eq!(provider.path, "current/theme/colors.toml");

        let invalid = source.replace(
            "path = \"current/theme/colors.toml\"",
            "path = \"../private.toml\"",
        );
        assert!(
            PluginManifest::from_toml(&invalid)
                .unwrap_err()
                .to_string()
                .contains("granted mount")
        );
    }

    #[test]
    fn source_identity_is_canonical_and_typed() {
        let source = "github:cameroncooper/touchbar"
            .parse::<GithubSource>()
            .unwrap();
        assert_eq!(source.to_string(), "github:cameroncooper/touchbar");
        assert!(
            "https://github.com/cameroncooper/repo"
                .parse::<GithubSource>()
                .is_err()
        );
        assert!("github:Uppercase/repo".parse::<GithubSource>().is_err());
        assert!(
            "github:cameroncooper/repo.git"
                .parse::<GithubSource>()
                .is_err()
        );
    }
}
