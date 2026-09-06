//! Strict, reviewer-owned discovery metadata for GitHub-hosted TouchBar packs.
//!
//! Catalog aliases are discovery conveniences, never package identities or a
//! substitute for release provenance. The catalog is bundled with the core
//! build so a mutable network document cannot silently retarget an install.

use std::{collections::BTreeSet, error::Error, fmt};

use serde::{Deserialize, Serialize};
use touchbar_package::GithubSource;

pub const CATALOG_VERSION: u32 = 1;
pub const MAX_CATALOG_BYTES: usize = 1024 * 1024;
pub const MAX_CATALOG_ENTRIES: usize = 4096;
pub const BUNDLED_CATALOG: &str = include_str!("../../../catalog/plugins.toml");

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub catalog_version: u32,
    #[serde(rename = "plugin")]
    pub plugins: Vec<CatalogEntry>,
}

impl Catalog {
    pub fn from_toml(source: &str) -> Result<Self, CatalogError> {
        if source.len() > MAX_CATALOG_BYTES {
            return Err(CatalogError::TooLarge);
        }
        let catalog = toml::from_str::<Self>(source).map_err(CatalogError::Parse)?;
        catalog.validate().map_err(CatalogError::Invalid)?;
        Ok(catalog)
    }

    pub fn bundled() -> Result<Self, CatalogError> {
        Self::from_toml(BUNDLED_CATALOG)
    }

    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();
        if self.catalog_version != CATALOG_VERSION {
            issue(
                &mut issues,
                "catalog_version",
                format!(
                    "unsupported catalog version {}; this build supports {CATALOG_VERSION}",
                    self.catalog_version
                ),
            );
        }
        if self.plugins.len() > MAX_CATALOG_ENTRIES {
            issue(
                &mut issues,
                "plugin",
                format!("catalog exceeds {MAX_CATALOG_ENTRIES} entries"),
            );
        }

        let mut aliases = BTreeSet::new();
        let mut sources = BTreeSet::new();
        let mut previous_alias: Option<&str> = None;
        for (index, plugin) in self.plugins.iter().enumerate() {
            let prefix = format!("plugin[{index}]");
            validate_identifier(&mut issues, &format!("{prefix}.alias"), &plugin.alias, 64);
            validate_text(&mut issues, &format!("{prefix}.name"), &plugin.name, 80);
            validate_text(
                &mut issues,
                &format!("{prefix}.description"),
                &plugin.description,
                280,
            );
            if plugin.source.owner() == "local" {
                issue(
                    &mut issues,
                    format!("{prefix}.source"),
                    "catalog entries require a public GitHub repository identity",
                );
            }
            if plugin.categories.is_empty() || plugin.categories.len() > 8 {
                issue(
                    &mut issues,
                    format!("{prefix}.categories"),
                    "must contain between one and eight categories",
                );
            }
            let mut categories = BTreeSet::new();
            let mut previous_category: Option<&str> = None;
            for (category_index, category) in plugin.categories.iter().enumerate() {
                validate_identifier(
                    &mut issues,
                    &format!("{prefix}.categories[{category_index}]"),
                    category,
                    32,
                );
                if !categories.insert(category) {
                    issue(
                        &mut issues,
                        format!("{prefix}.categories[{category_index}]"),
                        "duplicate category",
                    );
                }
                if previous_category.is_some_and(|previous| previous >= category.as_str()) {
                    issue(
                        &mut issues,
                        format!("{prefix}.categories"),
                        "categories must be uniquely sorted",
                    );
                }
                previous_category = Some(category);
            }
            if !aliases.insert(&plugin.alias) {
                issue(
                    &mut issues,
                    format!("{prefix}.alias"),
                    "duplicate catalog alias",
                );
            }
            if !sources.insert(&plugin.source) {
                issue(
                    &mut issues,
                    format!("{prefix}.source"),
                    "a GitHub source may have only one catalog alias",
                );
            }
            if previous_alias.is_some_and(|previous| previous >= plugin.alias.as_str()) {
                issue(
                    &mut issues,
                    "plugin",
                    "entries must be uniquely sorted by alias",
                );
            }
            previous_alias = Some(&plugin.alias);
            if plugin.tier == CatalogTier::FirstParty && plugin.source.owner() != "cameroncooper" {
                issue(
                    &mut issues,
                    format!("{prefix}.tier"),
                    "first-party entries must be owned by github:cameroncooper",
                );
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(issues))
        }
    }

    pub fn find(&self, alias: &str) -> Option<&CatalogEntry> {
        self.plugins
            .binary_search_by(|entry| entry.alias.as_str().cmp(alias))
            .ok()
            .map(|index| &self.plugins[index])
    }

    pub fn resolve(&self, alias: &str) -> Option<&CatalogEntry> {
        self.find(alias)
            .filter(|entry| entry.state == CatalogState::Active)
    }

    pub fn find_source(&self, source: &GithubSource) -> Option<&CatalogEntry> {
        self.plugins.iter().find(|entry| entry.source == *source)
    }

    pub fn search<'a>(&'a self, query: &str, category: Option<&str>) -> Vec<&'a CatalogEntry> {
        let query = query.trim().to_ascii_lowercase();
        let tokens = query.split_whitespace().collect::<Vec<_>>();
        let mut matches = self
            .plugins
            .iter()
            .filter(|entry| entry.state == CatalogState::Active)
            .filter(|entry| {
                category.is_none_or(|wanted| entry.categories.iter().any(|v| v == wanted))
            })
            .filter(|entry| {
                let haystack = format!(
                    "{} {} {} {} {}",
                    entry.alias,
                    entry.name,
                    entry.description,
                    entry.source,
                    entry.categories.join(" ")
                )
                .to_ascii_lowercase();
                tokens.iter().all(|token| haystack.contains(token))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|entry| {
            (
                entry.alias != query,
                !entry.alias.starts_with(&query),
                std::cmp::Reverse(entry.tier.rank()),
                &entry.alias,
            )
        });
        matches
    }

    pub fn validate_transition(&self, previous: &Catalog) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();
        for old in &previous.plugins {
            let Some(new) = self.find(&old.alias) else {
                issue(
                    &mut issues,
                    format!("plugin.{}", old.alias),
                    "an accepted alias must remain as an active entry or retired tombstone",
                );
                continue;
            };
            if new.source != old.source {
                issue(
                    &mut issues,
                    format!("plugin.{}.source", old.alias),
                    "an accepted alias may never change GitHub identity",
                );
            }
            if old.state == CatalogState::Retired && new.state != CatalogState::Retired {
                issue(
                    &mut issues,
                    format!("plugin.{}.state", old.alias),
                    "a retired alias may never be reactivated",
                );
            }
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(issues))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub alias: String,
    pub source: GithubSource,
    pub name: String,
    pub description: String,
    pub categories: Vec<String>,
    pub tier: CatalogTier,
    pub state: CatalogState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatalogTier {
    Listed,
    Curated,
    FirstParty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatalogState {
    Active,
    Retired,
}

impl CatalogTier {
    fn rank(self) -> u8 {
        match self {
            Self::Listed => 0,
            Self::Curated => 1,
            Self::FirstParty => 2,
        }
    }
}

impl fmt::Display for CatalogTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Listed => "listed",
            Self::Curated => "curated",
            Self::FirstParty => "first-party",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug)]
pub enum CatalogError {
    TooLarge,
    Parse(toml::de::Error),
    Invalid(ValidationErrors),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(formatter, "catalog exceeds {MAX_CATALOG_BYTES} bytes"),
            Self::Parse(error) => write!(formatter, "could not parse catalog: {error}"),
            Self::Invalid(error) => error.fmt(formatter),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Invalid(error) => Some(error),
            Self::TooLarge => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationErrors(pub Vec<ValidationIssue>);

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "invalid TouchBar catalog:")?;
        for issue in &self.0 {
            writeln!(formatter, "- {}: {}", issue.field, issue.message)?;
        }
        Ok(())
    }
}

impl Error for ValidationErrors {}

fn issue(issues: &mut Vec<ValidationIssue>, field: impl Into<String>, message: impl Into<String>) {
    issues.push(ValidationIssue {
        field: field.into(),
        message: message.into(),
    });
}

fn validate_identifier(
    issues: &mut Vec<ValidationIssue>,
    field: &str,
    value: &str,
    maximum: usize,
) {
    if value.is_empty()
        || value.len() > maximum
        || value.starts_with('-')
        || value.ends_with('-')
        || value.contains("--")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
    {
        issue(
            issues,
            field,
            format!("must be a lowercase kebab-case identifier of at most {maximum} bytes"),
        );
    }
}

fn validate_text(issues: &mut Vec<ValidationIssue>, field: &str, value: &str, maximum: usize) {
    if value.is_empty()
        || value.trim() != value
        || value.chars().count() > maximum
        || value.chars().any(unsafe_catalog_text_character)
    {
        issue(
            issues,
            field,
            format!("must be trimmed, nonempty text of at most {maximum} characters"),
        );
    }
}

fn unsafe_catalog_text_character(value: char) -> bool {
    value.is_control()
        || matches!(
            value,
            '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
catalog_version = 1

[[plugin]]
alias = "media-controls"
source = "github:alice/touchbar-media"
name = "Media Controls"
description = "Playback controls and timeline scrubbing"
categories = ["audio", "media"]
tier = "curated"
state = "active"

[[plugin]]
alias = "system-controls"
source = "github:cameroncooper/touchbar-controls"
name = "System Controls"
description = "Volume, brightness, microphone, and power controls"
categories = ["audio", "system"]
tier = "first-party"
state = "active"
"#;

    #[test]
    fn bundled_catalog_is_strict_and_valid() {
        assert!(Catalog::bundled().is_ok());
    }

    #[test]
    fn aliases_are_exact_and_search_is_deterministic() {
        let catalog = Catalog::from_toml(VALID).unwrap();
        assert_eq!(
            catalog.find("media-controls").unwrap().tier,
            CatalogTier::Curated
        );
        assert!(catalog.find("Media-Controls").is_none());
        let matches = catalog.search("controls", Some("audio"));
        assert_eq!(
            matches
                .iter()
                .map(|entry| entry.alias.as_str())
                .collect::<Vec<_>>(),
            ["system-controls", "media-controls"]
        );
    }

    #[test]
    fn unknown_fields_and_oversized_documents_fail_closed() {
        assert!(Catalog::from_toml("catalog_version = 1\nplugins = []\nextra = true").is_err());
        assert!(Catalog::from_toml(&"x".repeat(MAX_CATALOG_BYTES + 1)).is_err());
    }

    #[test]
    fn duplicate_unsorted_and_forged_first_party_entries_are_rejected() {
        let invalid = VALID
            .replace("media-controls", "system-controls")
            .replace("github:alice/touchbar-media", "github:bob/touchbar-media")
            .replace("tier = \"curated\"", "tier = \"first-party\"");
        let error = Catalog::from_toml(&invalid).unwrap_err().to_string();
        assert!(error.contains("duplicate catalog alias"));
        assert!(error.contains("first-party entries must be owned"));
        assert!(error.contains("uniquely sorted by alias"));
    }

    #[test]
    fn categories_must_be_sorted_unique_identifiers() {
        let invalid = VALID.replace(
            "categories = [\"audio\", \"media\"]",
            "categories = [\"media\", \"audio\", \"audio\", \"Bad Category\"]",
        );
        let error = Catalog::from_toml(&invalid).unwrap_err().to_string();
        assert!(error.contains("categories must be uniquely sorted"));
        assert!(error.contains("duplicate category"));
        assert!(error.contains("lowercase kebab-case"));
    }

    #[test]
    fn display_metadata_rejects_invisible_and_bidi_control_characters() {
        for dangerous in ['\u{200b}', '\u{202e}', '\u{2067}', '\u{feff}'] {
            let invalid = VALID.replace("Media Controls", &format!("Media{dangerous} Controls"));
            assert!(Catalog::from_toml(&invalid).is_err());
        }
    }

    #[test]
    fn accepted_aliases_are_permanent_and_retirement_is_one_way() {
        let previous = Catalog::from_toml(VALID).unwrap();
        let removed = Catalog::from_toml(
            &VALID
                .split("[[plugin]]")
                .take(2)
                .collect::<Vec<_>>()
                .join("[[plugin]]"),
        )
        .unwrap();
        assert!(removed.validate_transition(&previous).is_err());

        let retired = Catalog::from_toml(&VALID.replace(
            "tier = \"curated\"\nstate = \"active\"",
            "tier = \"curated\"\nstate = \"retired\"",
        ))
        .unwrap();
        assert!(retired.validate_transition(&previous).is_ok());
        assert!(retired.resolve("media-controls").is_none());
        assert!(previous.validate_transition(&retired).is_err());

        let retargeted = Catalog::from_toml(
            &VALID.replace("github:alice/touchbar-media", "github:alice/other-media"),
        )
        .unwrap();
        assert!(retargeted.validate_transition(&previous).is_err());
    }
}
