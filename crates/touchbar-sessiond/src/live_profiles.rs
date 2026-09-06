//! Live adapter between strict user profile configuration and Wayland items.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use touchbar_layout::{BarSpec, Element, GroupElement, GroupSpec, ItemSpec};
use touchbar_model::{
    CompositionController, CompositionDelta, ContextSnapshot, ContextValue, ControllerCommand,
    ProfileCompositionSnapshot, ProfileId,
};
use touchbar_profile_config::ProfileDocument;

#[cfg(test)]
const REQUIRED_ITEM_IDS: [&str; 4] = [
    "touchbar.ui-demo#audio.volume",
    "demo.terminal#demo.terminal",
    "demo.browser#demo.browser",
    "demo.status#demo.status",
];

const DEMO_PROFILE: &str = r#"
version = 1
fallback = "default"

[[contribution]]
id = "persistent.volume"
items = [{ plugin = "touchbar.ui-demo", item = "audio.volume", required = true }]

[[contribution]]
id = "application.terminal"
items = [{ plugin = "demo.terminal", item = "demo.terminal", required = true }]

[[contribution]]
id = "application.browser"
items = [{ plugin = "demo.browser", item = "demo.browser", required = true }]
scope = "application"
priority = 100
when = { kind = "any", predicates = [
    { kind = "text-equals", key = "application.id", value = "firefox" },
    { kind = "text-equals", key = "application.id", value = "org.mozilla.firefox" },
    { kind = "text-equals", key = "application.id", value = "chromium" },
    { kind = "text-equals", key = "application.id", value = "google-chrome" },
] }

[[contribution]]
id = "persistent.status"
items = [{ plugin = "demo.status", item = "demo.status", required = true }]

[[profile]]
id = "default"
[[profile.element]]
kind = "slot"
id = "persistent"
policy = "fixed"
contributions = ["persistent.volume"]
[[profile.element]]
kind = "flexible-space"
minimum = 8
weight = 1
[[profile.element]]
kind = "slot"
id = "application"
policy = "select"
contributions = ["application.terminal", "application.browser"]
[[profile.element]]
kind = "flexible-space"
minimum = 8
weight = 1
[[profile.element]]
kind = "slot"
id = "status"
policy = "fixed"
contributions = ["persistent.status"]
"#;

pub struct LiveProfiles {
    document: ProfileDocument,
    context: ContextSnapshot,
    controller: Option<CompositionController>,
    current: Option<ProfileCompositionSnapshot>,
    connected: BTreeMap<String, ItemSpec>,
    pending_connected: Option<BTreeMap<String, ItemSpec>>,
    pending_document: Option<ProfileDocument>,
    captured_contacts: BTreeSet<u32>,
    manual_profile: Option<ProfileId>,
    next_generation: u64,
    last_bar: Option<BarSpec>,
}

impl LiveProfiles {
    pub fn demo() -> Self {
        Self::new(
            ProfileDocument::from_toml(DEMO_PROFILE)
                .expect("the built-in profile fixture must remain valid"),
            ContextSnapshot {
                generation: 1,
                ..ContextSnapshot::default().text("application.id", "terminal")
            },
        )
    }

    pub fn configured(document: ProfileDocument) -> Self {
        Self::new(
            document,
            ContextSnapshot {
                generation: 1,
                ..ContextSnapshot::default()
            },
        )
    }

    fn new(document: ProfileDocument, context: ContextSnapshot) -> Self {
        Self {
            document,
            context,
            controller: None,
            current: None,
            connected: BTreeMap::new(),
            pending_connected: None,
            pending_document: None,
            captured_contacts: BTreeSet::new(),
            manual_profile: None,
            next_generation: 1,
            last_bar: None,
        }
    }

    pub fn ready(&self) -> bool {
        self.controller.is_some()
    }

    pub fn profile_ids(&self) -> Vec<String> {
        self.document.profile_ids()
    }

    pub fn manual_profile(&self) -> Option<&str> {
        self.manual_profile.as_ref().map(ProfileId::as_str)
    }

    pub fn active_profile(&self) -> Option<&str> {
        self.current
            .as_ref()
            .map(|snapshot| snapshot.composition.profile.as_str())
    }

    pub fn region(&self, id: &str) -> Option<(u32, u32)> {
        self.document.region(id)
    }

    pub fn missing_required_items(&self) -> Vec<String> {
        let connected = self.connected.keys().cloned().collect::<BTreeSet<_>>();
        self.document
            .required_item_ids()
            .difference(&connected)
            .cloned()
            .collect()
    }

    pub fn try_initialize(
        &mut self,
        connected: &BTreeMap<String, ItemSpec>,
    ) -> Result<Option<ProfileCompositionSnapshot>> {
        let allowed = self.document.item_ids();
        let relevant = connected
            .iter()
            .filter(|(id, _)| allowed.contains(*id))
            .map(|(id, spec)| (id.clone(), spec.clone()))
            .collect::<BTreeMap<_, _>>();
        if relevant == self.connected && self.pending_connected.is_none() {
            return Ok(None);
        }
        if !self.captured_contacts.is_empty() {
            self.pending_connected = Some(relevant);
            return Ok(None);
        }
        self.rebuild(relevant)
    }

    pub fn replace_document(
        &mut self,
        document: ProfileDocument,
        connected: &BTreeMap<String, ItemSpec>,
    ) -> Result<Option<ProfileCompositionSnapshot>> {
        document.validate()?;
        let allowed = document.item_ids();
        let relevant = connected
            .iter()
            .filter(|(id, _)| allowed.contains(*id))
            .map(|(id, spec)| (id.clone(), spec.clone()))
            .collect::<BTreeMap<_, _>>();
        if document
            .required_item_ids()
            .iter()
            .all(|id| relevant.contains_key(id))
        {
            document.build(&relevant, self.context.clone())?;
        }
        if !self.captured_contacts.is_empty() {
            self.pending_document = Some(document);
            self.pending_connected = Some(relevant);
            return Ok(None);
        }
        self.document = document;
        self.manual_profile = self.manual_profile.take().filter(|profile| {
            self.document
                .profile_ids()
                .iter()
                .any(|id| id == profile.as_str())
        });
        self.connected.clear();
        self.try_initialize(&relevant)
    }

    pub fn select_profile(
        &mut self,
        profile: impl Into<ProfileId>,
    ) -> Result<Option<ProfileCompositionSnapshot>> {
        let profile = profile.into();
        if !self
            .document
            .profile_ids()
            .iter()
            .any(|id| id == profile.as_str())
        {
            bail!("unknown profile `{profile}`");
        }
        self.manual_profile = Some(profile.clone());
        let Some(controller) = &mut self.controller else {
            return Ok(None);
        };
        let raw = controller
            .apply(ControllerCommand::SelectProfile(profile))?
            .cloned();
        Ok(raw.map(|snapshot| self.normalize(snapshot)))
    }

    pub fn use_automatic_profile(&mut self) -> Result<Option<ProfileCompositionSnapshot>> {
        self.manual_profile = None;
        let Some(controller) = &mut self.controller else {
            return Ok(None);
        };
        let raw = controller
            .apply(ControllerCommand::UseAutomaticProfile)?
            .cloned();
        Ok(raw.map(|snapshot| self.normalize(snapshot)))
    }

    fn rebuild(
        &mut self,
        connected: BTreeMap<String, ItemSpec>,
    ) -> Result<Option<ProfileCompositionSnapshot>> {
        self.connected = connected;
        self.pending_connected = None;
        let required = self.document.required_item_ids();
        if !required.iter().all(|id| self.connected.contains_key(id)) {
            self.controller = None;
            self.current = None;
            self.last_bar = Some(BarSpec {
                elements: Vec::new(),
                principal_item: None,
            });
            return Ok(None);
        }

        let built = self.document.build(&self.connected, self.context.clone())?;
        let mut controller = built.controller;
        if let Some(profile) = &self.manual_profile {
            controller.apply(ControllerCommand::SelectProfile(profile.clone()))?;
        }
        let raw = controller.current().clone();
        self.controller = Some(controller);
        Ok(Some(self.normalize(raw)))
    }

    pub fn set_fact(
        &mut self,
        key: impl Into<String>,
        value: ContextValue,
    ) -> Result<Option<ProfileCompositionSnapshot>> {
        self.context.generation = self.context.generation.wrapping_add(1).max(1);
        self.context.facts.insert(key.into(), value);
        let Some(controller) = &mut self.controller else {
            return Ok(None);
        };
        let raw = controller
            .apply(ControllerCommand::SetContext(self.context.clone()))?
            .cloned();
        Ok(raw.map(|snapshot| self.normalize(snapshot)))
    }

    pub fn capture_started(&mut self, contact_id: u32) {
        self.captured_contacts.insert(contact_id);
        if let Some(controller) = &mut self.controller {
            controller.capture_started(contact_id);
        }
    }

    pub fn capture_ended(&mut self, contact_id: u32) -> Result<Option<ProfileCompositionSnapshot>> {
        self.captured_contacts.remove(&contact_id);
        let context_snapshot = self
            .controller
            .as_mut()
            .and_then(|controller| controller.capture_ended(contact_id).cloned());
        if self.captured_contacts.is_empty() {
            if let Some(document) = self.pending_document.take() {
                self.document = document;
                self.manual_profile = self.manual_profile.take().filter(|profile| {
                    self.document
                        .profile_ids()
                        .iter()
                        .any(|id| id == profile.as_str())
                });
            }
            if let Some(connected) = self.pending_connected.take() {
                return self.rebuild(connected);
            }
        }
        Ok(context_snapshot.map(|snapshot| self.normalize(snapshot)))
    }

    pub fn current(&self) -> Option<&ProfileCompositionSnapshot> {
        self.current.as_ref()
    }

    fn normalize(
        &mut self,
        mut snapshot: ProfileCompositionSnapshot,
    ) -> ProfileCompositionSnapshot {
        snapshot.generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        snapshot.context_generation = self.context.generation;
        snapshot.delta = delta(self.last_bar.as_ref(), &snapshot.composition.bar);
        self.last_bar = Some(snapshot.composition.bar.clone());
        self.current = Some(snapshot.clone());
        snapshot
    }
}

fn delta(previous: Option<&BarSpec>, next: &BarSpec) -> CompositionDelta {
    let previous = previous.map_or_else(BTreeSet::new, item_ids);
    let next = item_ids(next);
    CompositionDelta {
        retained_items: previous.intersection(&next).cloned().collect(),
        entering_items: next.difference(&previous).cloned().collect(),
        leaving_items: previous.difference(&next).cloned().collect(),
    }
}

fn item_ids(bar: &BarSpec) -> BTreeSet<touchbar_layout::ItemId> {
    let mut ids = BTreeSet::new();
    for element in &bar.elements {
        match element {
            Element::Item(item) => {
                ids.insert(item.id.clone());
            }
            Element::Group(group) => collect_group_item_ids(group, &mut ids),
            Element::FixedSpace { .. } | Element::FlexibleSpace { .. } => {}
        }
    }
    ids
}

fn collect_group_item_ids(group: &GroupSpec, ids: &mut BTreeSet<touchbar_layout::ItemId>) {
    for element in &group.elements {
        match element {
            GroupElement::Item(item) => {
                ids.insert(item.id.clone());
            }
            GroupElement::Group(group) => collect_group_item_ids(group, ids),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs() -> BTreeMap<String, ItemSpec> {
        REQUIRED_ITEM_IDS
            .into_iter()
            .map(|id| (id.into(), ItemSpec::new(id, 40, 80, 600)))
            .collect()
    }

    fn items(snapshot: &ProfileCompositionSnapshot) -> Vec<&str> {
        snapshot
            .composition
            .bar
            .elements
            .iter()
            .filter_map(|element| match element {
                Element::Item(item) => Some(item.id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fixture_keeps_persistent_items_while_application_content_changes() {
        let mut profiles = LiveProfiles::demo();
        let initial = profiles.try_initialize(&specs()).unwrap().unwrap();
        assert_eq!(
            items(&initial),
            [
                "touchbar.ui-demo#audio.volume",
                "demo.terminal#demo.terminal",
                "demo.status#demo.status"
            ]
        );

        let browser = profiles
            .set_fact("application.id", ContextValue::Text("firefox".into()))
            .unwrap()
            .unwrap();
        assert_eq!(
            items(&browser),
            [
                "touchbar.ui-demo#audio.volume",
                "demo.browser#demo.browser",
                "demo.status#demo.status"
            ]
        );
        assert_eq!(
            browser.delta.retained_items,
            [
                "demo.status#demo.status".into(),
                "touchbar.ui-demo#audio.volume".into()
            ]
        );
    }

    #[test]
    fn required_items_fail_closed_and_reconnection_rebuilds() {
        let mut connected = specs();
        connected.remove("demo.status#demo.status");
        let mut profiles = LiveProfiles::demo();
        assert!(profiles.try_initialize(&connected).unwrap().is_none());
        assert!(!profiles.ready());
        assert_eq!(
            profiles.missing_required_items(),
            ["demo.status#demo.status"]
        );

        let initial = profiles.try_initialize(&specs()).unwrap().unwrap();
        assert!(profiles.ready());
        assert_eq!(initial.delta.entering_items.len(), 3);

        assert!(profiles.try_initialize(&connected).unwrap().is_none());
        assert!(!profiles.ready());
        assert!(profiles.current().is_none());
        let restored = profiles.try_initialize(&specs()).unwrap().unwrap();
        assert_eq!(restored.delta.entering_items.len(), 3);
    }

    #[test]
    fn profile_change_waits_for_active_capture() {
        let source = format!(
            "{DEMO_PROFILE}\n{}",
            r#"
[[profile]]
id = "minimal"
[[profile.element]]
kind = "slot"
id = "status"
policy = "fixed"
contributions = ["persistent.status"]
"#
        );
        let document = ProfileDocument::from_toml(&source).unwrap();
        let mut profiles = LiveProfiles::configured(document);
        profiles.try_initialize(&specs()).unwrap().unwrap();
        profiles.capture_started(7);
        assert!(profiles.select_profile("minimal").unwrap().is_none());
        assert_eq!(items(profiles.current().unwrap()).len(), 3);
        let committed = profiles.capture_ended(7).unwrap().unwrap();
        assert_eq!(items(&committed), ["demo.status#demo.status"]);
        assert_eq!(profiles.manual_profile(), Some("minimal"));
    }

    #[test]
    fn invalid_document_replacement_preserves_the_live_profile() {
        let mut profiles = LiveProfiles::demo();
        profiles.try_initialize(&specs()).unwrap().unwrap();
        let before = profiles.current().unwrap().clone();
        let mut invalid = ProfileDocument::from_toml(DEMO_PROFILE).unwrap();
        invalid.fallback = "missing".into();

        assert!(profiles.replace_document(invalid, &specs()).is_err());
        assert_eq!(profiles.active_profile(), Some("default"));
        assert_eq!(profiles.current().unwrap(), &before);
    }
}
