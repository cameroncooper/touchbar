//! Stable item registry, user-owned profiles, and contextual composition.
//!
//! This crate does not discover context or parse plugin manifests. A caller
//! supplies typed context snapshots and profile policy. The pure model produces
//! deterministic ordered bars and transaction metadata without knowing about
//! Wayland, Hyprland, configuration syntax, or process supervision.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use touchbar_layout::{
    BarSpec, Element, GroupElement, GroupLayout, GroupSpec, ItemId, ItemSpec, MAX_GROUP_DEPTH,
};

mod profiles;
pub use profiles::*;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BarId(String);

impl BarId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for BarId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for BarId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for BarId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextValue {
    Text(String),
    Boolean(bool),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextSnapshot {
    pub generation: u64,
    pub facts: BTreeMap<String, ContextValue>,
}

impl ContextSnapshot {
    pub fn text(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts
            .insert(key.into(), ContextValue::Text(value.into()));
        self
    }

    pub fn boolean(mut self, key: impl Into<String>, value: bool) -> Self {
        self.facts.insert(key.into(), ContextValue::Boolean(value));
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextPredicate {
    Always,
    Present(String),
    Equals(String, ContextValue),
    All(Vec<ContextPredicate>),
    Any(Vec<ContextPredicate>),
    Not(Box<ContextPredicate>),
}

impl ContextPredicate {
    pub fn matches(&self, context: &ContextSnapshot) -> bool {
        match self {
            Self::Always => true,
            Self::Present(key) => context.facts.contains_key(key),
            Self::Equals(key, value) => context.facts.get(key) == Some(value),
            Self::All(predicates) => predicates
                .iter()
                .all(|predicate| predicate.matches(context)),
            Self::Any(predicates) => predicates
                .iter()
                .any(|predicate| predicate.matches(context)),
            Self::Not(predicate) => !predicate.matches(context),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContextScope {
    Global,
    Workspace,
    Application,
    Window,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextRule {
    pub bar: BarId,
    pub scope: ContextScope,
    pub priority: i32,
    pub predicate: ContextPredicate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSelector {
    pub fallback: BarId,
    pub rules: Vec<ContextRule>,
}

impl ContextSelector {
    /// Select at most one bar per scope, ordered most-specific to least-specific.
    /// Earlier rules win equal-priority ties, keeping replay deterministic.
    pub fn select_chain(&self, context: &ContextSnapshot) -> Vec<BarId> {
        let mut selected = BTreeMap::<ContextScope, (i32, usize, BarId)>::new();
        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.predicate.matches(context) {
                continue;
            }
            let replace = selected
                .get(&rule.scope)
                .is_none_or(|(priority, previous, _)| {
                    rule.priority > *priority || (rule.priority == *priority && index < *previous)
                });
            if replace {
                selected.insert(rule.scope, (rule.priority, index, rule.bar.clone()));
            }
        }
        let mut chain = selected
            .into_iter()
            .rev()
            .map(|(_, (_, _, bar))| bar)
            .collect::<Vec<_>>();
        if !chain.iter().any(|bar| bar == &self.fallback) {
            chain.push(self.fallback.clone());
        }
        chain
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionSnapshot {
    pub generation: u64,
    pub context_generation: u64,
    pub active_bars: Vec<BarId>,
}

pub struct TransactionalContext {
    selector: ContextSelector,
    current: CompositionSnapshot,
    pending: Option<CompositionSnapshot>,
    captured_contacts: BTreeSet<u32>,
    next_generation: u64,
}

impl TransactionalContext {
    pub fn new(selector: ContextSelector, initial: &ContextSnapshot) -> Self {
        let current = CompositionSnapshot {
            generation: 1,
            context_generation: initial.generation,
            active_bars: selector.select_chain(initial),
        };
        Self {
            selector,
            current,
            pending: None,
            captured_contacts: BTreeSet::new(),
            next_generation: 2,
        }
    }

    pub fn current(&self) -> &CompositionSnapshot {
        &self.current
    }

    /// Prepare the newest context atomically. While a gesture is captured the
    /// newest candidate replaces any older pending candidate but is not shown.
    pub fn update(&mut self, context: &ContextSnapshot) -> Option<&CompositionSnapshot> {
        let candidate = CompositionSnapshot {
            generation: self.next_generation,
            context_generation: context.generation,
            active_bars: self.selector.select_chain(context),
        };
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        if self.captured_contacts.is_empty() {
            self.current = candidate;
            Some(&self.current)
        } else {
            self.pending = Some(candidate);
            None
        }
    }

    pub fn capture_started(&mut self, contact_id: u32) {
        self.captured_contacts.insert(contact_id);
    }

    pub fn capture_ended(&mut self, contact_id: u32) -> Option<&CompositionSnapshot> {
        self.captured_contacts.remove(&contact_id);
        if self.captured_contacts.is_empty()
            && let Some(pending) = self.pending.take()
        {
            self.current = pending;
            return Some(&self.current);
        }
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemDefinition {
    pub layout: ItemSpec,
    /// Used by customization and accessibility interfaces.
    pub label: String,
    /// A temporary replacement bar opened by an ordinary activation.
    pub expanded_bar: Option<BarId>,
    /// An optional alternative bar opened by press-and-hold.
    pub press_and_hold_bar: Option<BarId>,
}

impl ItemDefinition {
    pub fn new(layout: ItemSpec, label: impl Into<String>) -> Self {
        Self {
            layout,
            label: label.into(),
            expanded_bar: None,
            press_and_hold_bar: None,
        }
    }

    pub fn expanded_bar(mut self, bar: impl Into<BarId>) -> Self {
        self.expanded_bar = Some(bar.into());
        self
    }

    pub fn press_and_hold_bar(mut self, bar: impl Into<BarId>) -> Self {
        self.press_and_hold_bar = Some(bar.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BarElement {
    Item(ItemId),
    Group(BarGroup),
    FixedSpace {
        width: u32,
    },
    FlexibleSpace {
        minimum: u32,
        weight: u32,
    },
    /// Content from the next more-specific active bar is inserted here.
    ContextProxy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BarGroupElement {
    Item(ItemId),
    Group(BarGroup),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BarGroup {
    pub id: String,
    pub elements: Vec<BarGroupElement>,
    pub layout: GroupLayout,
    pub spacing: u32,
    pub visibility_priority: i32,
    pub compression_priority: i32,
}

impl BarGroup {
    pub fn new(id: impl Into<String>, elements: Vec<BarGroupElement>) -> Self {
        Self {
            id: id.into(),
            elements,
            layout: GroupLayout::Natural,
            spacing: 0,
            visibility_priority: 0,
            compression_priority: 0,
        }
    }

    pub fn layout(mut self, layout: GroupLayout) -> Self {
        self.layout = layout;
        self
    }

    pub fn spacing(mut self, spacing: u32) -> Self {
        self.spacing = spacing;
        self
    }

    pub fn visibility_priority(mut self, priority: i32) -> Self {
        self.visibility_priority = priority;
        self
    }

    pub fn compression_priority(mut self, priority: i32) -> Self {
        self.compression_priority = priority;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BarDefinition {
    pub id: BarId,
    pub elements: Vec<BarElement>,
    pub principal_item: Option<ItemId>,
}

impl BarDefinition {
    pub fn new(id: impl Into<BarId>, elements: Vec<BarElement>) -> Self {
        Self {
            id: id.into(),
            elements,
            principal_item: None,
        }
    }

    pub fn principal_item(mut self, item: impl Into<ItemId>) -> Self {
        self.principal_item = Some(item.into());
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct Registry {
    items: BTreeMap<ItemId, ItemDefinition>,
    bars: BTreeMap<BarId, BarDefinition>,
}

impl Registry {
    pub fn add_item(&mut self, item: ItemDefinition) -> Result<(), ModelError> {
        let id = item.layout.id.clone();
        if id.as_str().is_empty() {
            return Err(ModelError::EmptyItemId);
        }
        if item.label.trim().is_empty() {
            return Err(ModelError::EmptyItemLabel(id));
        }
        if self.items.contains_key(&id) {
            return Err(ModelError::DuplicateItem(id));
        }
        self.items.insert(id, item);
        Ok(())
    }

    pub fn add_bar(&mut self, bar: BarDefinition) -> Result<(), ModelError> {
        if bar.id.as_str().is_empty() {
            return Err(ModelError::EmptyBarId);
        }
        let id = bar.id.clone();
        if self.bars.contains_key(&id) {
            return Err(ModelError::DuplicateBar(id));
        }
        self.bars.insert(id, bar);
        Ok(())
    }

    pub fn item(&self, id: &ItemId) -> Option<&ItemDefinition> {
        self.items.get(id)
    }

    pub fn bar(&self, id: &BarId) -> Option<&BarDefinition> {
        self.bars.get(id)
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        for item in self.items.values() {
            for target in [&item.expanded_bar, &item.press_and_hold_bar]
                .into_iter()
                .flatten()
            {
                if !self.bars.contains_key(target) {
                    return Err(ModelError::UnknownExpandedBar {
                        item: item.layout.id.clone(),
                        bar: target.clone(),
                    });
                }
            }
        }
        for bar in self.bars.values() {
            let mut group_ids = BTreeSet::new();
            let proxy_count = bar
                .elements
                .iter()
                .filter(|element| matches!(element, BarElement::ContextProxy))
                .count();
            if proxy_count > 1 {
                return Err(ModelError::MultipleContextProxies(bar.id.clone()));
            }
            for element in &bar.elements {
                match element {
                    BarElement::Item(id) if !self.items.contains_key(id) => {
                        return Err(ModelError::UnknownItem {
                            bar: bar.id.clone(),
                            item: id.clone(),
                        });
                    }
                    BarElement::Group(group) => {
                        self.validate_bar_group(&bar.id, group, 1, &mut group_ids)?;
                    }
                    BarElement::FlexibleSpace { weight: 0, .. } => {
                        return Err(ModelError::ZeroFlexibleWeight(bar.id.clone()));
                    }
                    _ => {}
                }
            }
            if let Some(principal) = &bar.principal_item
                && !bar
                    .elements
                    .iter()
                    .any(|element| bar_element_contains_item(element, principal))
            {
                return Err(ModelError::PrincipalNotInBar {
                    bar: bar.id.clone(),
                    item: principal.clone(),
                });
            }
        }
        Ok(())
    }

    fn validate_bar_group(
        &self,
        bar: &BarId,
        group: &BarGroup,
        depth: usize,
        group_ids: &mut BTreeSet<String>,
    ) -> Result<(), ModelError> {
        if group.id.trim().is_empty() {
            return Err(ModelError::EmptyGroupId(bar.clone()));
        }
        if depth > MAX_GROUP_DEPTH {
            return Err(ModelError::GroupDepthExceeded {
                bar: bar.clone(),
                group: group.id.clone(),
            });
        }
        if !group_ids.insert(group.id.clone()) {
            return Err(ModelError::DuplicateGroup {
                bar: bar.clone(),
                group: group.id.clone(),
            });
        }
        for element in &group.elements {
            match element {
                BarGroupElement::Item(item) if !self.items.contains_key(item) => {
                    return Err(ModelError::UnknownItem {
                        bar: bar.clone(),
                        item: item.clone(),
                    });
                }
                BarGroupElement::Group(child) => {
                    self.validate_bar_group(bar, child, depth + 1, group_ids)?;
                }
                BarGroupElement::Item(_) => {}
            }
        }
        Ok(())
    }

    /// Compose active bars ordered from the focused/most-specific bar to the
    /// system/least-specific bar.
    ///
    /// A less-specific bar participates only when it contains `ContextProxy`.
    /// This mirrors the useful part of AppKit's other-items proxy behavior:
    /// focused content wins by default, while an enclosing bar can explicitly
    /// place persistent controls around it.
    pub fn compose_active_chain(&self, active: &[BarId]) -> Result<BarSpec, ModelError> {
        self.validate()?;
        let first = active.first().ok_or(ModelError::EmptyActiveChain)?;
        let first_bar = self
            .bars
            .get(first)
            .ok_or_else(|| ModelError::UnknownActiveBar(first.clone()))?;
        let mut composed = without_proxy(&first_bar.elements);

        for id in &active[1..] {
            let outer = self
                .bars
                .get(id)
                .ok_or_else(|| ModelError::UnknownActiveBar(id.clone()))?;
            let Some(proxy) = outer
                .elements
                .iter()
                .position(|element| matches!(element, BarElement::ContextProxy))
            else {
                continue;
            };
            let mut wrapped = Vec::with_capacity(outer.elements.len() + composed.len());
            wrapped.extend_from_slice(&outer.elements[..proxy]);
            wrapped.append(&mut composed);
            wrapped.extend_from_slice(&outer.elements[proxy + 1..]);
            composed = wrapped;
        }

        let principal_item = active.iter().find_map(|id| {
            self.bars.get(id).and_then(|bar| {
                bar.principal_item.as_ref().and_then(|principal| {
                    composed
                        .iter()
                        .any(|element| bar_element_contains_item(element, principal))
                        .then(|| principal.clone())
                })
            })
        });
        let elements = composed
            .into_iter()
            .map(|element| match element {
                BarElement::Item(id) => self
                    .items
                    .get(&id)
                    .map(|item| Element::Item(item.layout.clone()))
                    .ok_or(ModelError::UnknownComposedItem(id)),
                BarElement::Group(group) => self.resolve_bar_group(group).map(Element::Group),
                BarElement::FixedSpace { width } => Ok(Element::FixedSpace { width }),
                BarElement::FlexibleSpace { minimum, weight } => {
                    Ok(Element::FlexibleSpace { minimum, weight })
                }
                BarElement::ContextProxy => unreachable!("all context proxies are removed"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BarSpec {
            elements,
            principal_item,
        })
    }

    fn resolve_bar_group(&self, group: BarGroup) -> Result<GroupSpec, ModelError> {
        let elements = group
            .elements
            .into_iter()
            .map(|element| match element {
                BarGroupElement::Item(id) => self
                    .items
                    .get(&id)
                    .map(|item| GroupElement::Item(item.layout.clone()))
                    .ok_or(ModelError::UnknownComposedItem(id)),
                BarGroupElement::Group(group) => {
                    self.resolve_bar_group(group).map(GroupElement::Group)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(GroupSpec {
            id: group.id,
            elements,
            layout: group.layout,
            spacing: group.spacing,
            visibility_priority: group.visibility_priority,
            compression_priority: group.compression_priority,
        })
    }
}

fn without_proxy(elements: &[BarElement]) -> Vec<BarElement> {
    elements
        .iter()
        .filter(|element| !matches!(element, BarElement::ContextProxy))
        .cloned()
        .collect()
}

fn bar_element_contains_item(element: &BarElement, target: &ItemId) -> bool {
    match element {
        BarElement::Item(item) => item == target,
        BarElement::Group(group) => bar_group_contains_item(group, target),
        BarElement::FixedSpace { .. }
        | BarElement::FlexibleSpace { .. }
        | BarElement::ContextProxy => false,
    }
}

fn bar_group_contains_item(group: &BarGroup, target: &ItemId) -> bool {
    group.elements.iter().any(|element| match element {
        BarGroupElement::Item(item) => item == target,
        BarGroupElement::Group(group) => bar_group_contains_item(group, target),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelError {
    EmptyItemId,
    EmptyItemLabel(ItemId),
    EmptyBarId,
    DuplicateItem(ItemId),
    DuplicateBar(BarId),
    UnknownItem { bar: BarId, item: ItemId },
    UnknownExpandedBar { item: ItemId, bar: BarId },
    MultipleContextProxies(BarId),
    ZeroFlexibleWeight(BarId),
    EmptyGroupId(BarId),
    DuplicateGroup { bar: BarId, group: String },
    GroupDepthExceeded { bar: BarId, group: String },
    PrincipalNotInBar { bar: BarId, item: ItemId },
    EmptyActiveChain,
    UnknownActiveBar(BarId),
    UnknownComposedItem(ItemId),
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyItemId => write!(formatter, "item identifiers must not be empty"),
            Self::EmptyItemLabel(id) => write!(formatter, "item {id} has no accessible label"),
            Self::EmptyBarId => write!(formatter, "bar identifiers must not be empty"),
            Self::DuplicateItem(id) => write!(formatter, "duplicate item identifier {id}"),
            Self::DuplicateBar(id) => write!(formatter, "duplicate bar identifier {id}"),
            Self::UnknownItem { bar, item } => {
                write!(formatter, "bar {bar} references unknown item {item}")
            }
            Self::UnknownExpandedBar { item, bar } => {
                write!(
                    formatter,
                    "item {item} references unknown expanded bar {bar}"
                )
            }
            Self::MultipleContextProxies(bar) => {
                write!(formatter, "bar {bar} contains multiple context proxies")
            }
            Self::ZeroFlexibleWeight(bar) => {
                write!(formatter, "bar {bar} contains a zero-weight flexible space")
            }
            Self::EmptyGroupId(bar) => write!(formatter, "bar {bar} contains an empty group ID"),
            Self::DuplicateGroup { bar, group } => {
                write!(formatter, "bar {bar} contains duplicate group {group}")
            }
            Self::GroupDepthExceeded { bar, group } => write!(
                formatter,
                "bar {bar} group {group} exceeds the maximum nesting depth"
            ),
            Self::PrincipalNotInBar { bar, item } => {
                write!(formatter, "principal item {item} is not in bar {bar}")
            }
            Self::EmptyActiveChain => write!(formatter, "the active bar chain is empty"),
            Self::UnknownActiveBar(bar) => write!(formatter, "active bar {bar} is not registered"),
            Self::UnknownComposedItem(item) => {
                write!(formatter, "composed bar contains unknown item {item}")
            }
        }
    }
}

impl Error for ModelError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use touchbar_layout::resolve;

    fn definition(id: &str) -> ItemDefinition {
        ItemDefinition::new(ItemSpec::new(id, 40, 40, 40), id)
    }

    fn registry() -> Registry {
        let mut registry = Registry::default();
        for id in ["focused", "system-left", "system-right", "general"] {
            registry.add_item(definition(id)).unwrap();
        }
        registry
            .add_bar(
                BarDefinition::new("focused-bar", vec![BarElement::Item("focused".into())])
                    .principal_item("focused"),
            )
            .unwrap();
        registry
            .add_bar(BarDefinition::new(
                "system-bar",
                vec![
                    BarElement::Item("system-left".into()),
                    BarElement::FlexibleSpace {
                        minimum: 0,
                        weight: 1,
                    },
                    BarElement::ContextProxy,
                    BarElement::Item("system-right".into()),
                ],
            ))
            .unwrap();
        registry
            .add_bar(BarDefinition::new(
                "general-without-proxy",
                vec![BarElement::Item("general".into())],
            ))
            .unwrap();
        registry
    }

    fn item_ids(bar: &BarSpec) -> Vec<&str> {
        bar.elements
            .iter()
            .filter_map(|element| match element {
                Element::Item(item) => Some(item.id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn outer_proxy_wraps_more_specific_content() {
        let registry = registry();
        let composed = registry
            .compose_active_chain(&["focused-bar".into(), "system-bar".into()])
            .unwrap();
        assert_eq!(
            item_ids(&composed),
            ["system-left", "focused", "system-right"]
        );
        assert_eq!(composed.principal_item, Some("focused".into()));
    }

    #[test]
    fn outer_bar_without_proxy_does_not_replace_focused_content() {
        let registry = registry();
        let composed = registry
            .compose_active_chain(&["focused-bar".into(), "general-without-proxy".into()])
            .unwrap();
        assert_eq!(item_ids(&composed), ["focused"]);
    }

    #[test]
    fn closest_visible_principal_wins() {
        let mut registry = registry();
        registry
            .bars
            .get_mut(&BarId::from("system-bar"))
            .unwrap()
            .principal_item = Some("system-left".into());
        let composed = registry
            .compose_active_chain(&["focused-bar".into(), "system-bar".into()])
            .unwrap();
        assert_eq!(composed.principal_item, Some("focused".into()));
    }

    #[test]
    fn validates_expanded_bar_references_before_composition() {
        let mut registry = Registry::default();
        registry
            .add_item(definition("popover").expanded_bar("missing"))
            .unwrap();
        assert_eq!(
            registry.validate(),
            Err(ModelError::UnknownExpandedBar {
                item: "popover".into(),
                bar: "missing".into(),
            })
        );
    }

    #[test]
    fn rejects_multiple_proxy_slots() {
        let mut registry = Registry::default();
        registry
            .add_bar(BarDefinition::new(
                "invalid",
                vec![BarElement::ContextProxy, BarElement::ContextProxy],
            ))
            .unwrap();
        assert_eq!(
            registry.validate(),
            Err(ModelError::MultipleContextProxies("invalid".into()))
        );
    }

    #[test]
    fn duplicate_registration_does_not_replace_the_original() {
        let mut registry = Registry::default();
        registry.add_item(definition("item")).unwrap();
        let replacement = ItemDefinition::new(ItemSpec::new("item", 80, 80, 80), "replacement");
        assert_eq!(
            registry.add_item(replacement),
            Err(ModelError::DuplicateItem("item".into()))
        );
        assert_eq!(registry.item(&"item".into()).unwrap().label, "item");
    }

    #[test]
    fn composed_bar_resolves_principal_and_system_content_together() {
        let registry = registry();
        let composed = registry
            .compose_active_chain(&["focused-bar".into(), "system-bar".into()])
            .unwrap();
        let layout = resolve(&composed, 1000, &BTreeSet::new()).unwrap();
        let focused = layout
            .placements
            .iter()
            .find(|placement| placement.id.as_str() == "focused")
            .unwrap();
        assert_eq!(focused.x, 480);
        assert_eq!(focused.width, 40);
    }

    #[test]
    fn bar_registry_resolves_nested_equal_width_groups_and_principal_items() {
        let mut registry = registry();
        registry
            .add_bar(
                BarDefinition::new(
                    "grouped",
                    vec![
                        BarElement::FlexibleSpace {
                            minimum: 0,
                            weight: 1,
                        },
                        BarElement::Group(
                            BarGroup::new(
                                "system-actions",
                                vec![
                                    BarGroupElement::Item("system-left".into()),
                                    BarGroupElement::Item("system-right".into()),
                                ],
                            )
                            .layout(GroupLayout::EqualWidth)
                            .spacing(4),
                        ),
                        BarElement::FlexibleSpace {
                            minimum: 0,
                            weight: 1,
                        },
                    ],
                )
                .principal_item("system-right"),
            )
            .unwrap();

        let bar = registry.compose_active_chain(&["grouped".into()]).unwrap();
        let layout = resolve(&bar, 200, &BTreeSet::new()).unwrap();
        assert_eq!(layout.placements[0].width, 40);
        assert_eq!(layout.placements[1].width, 40);
        assert_eq!(layout.placements[1].x, 80);
    }

    #[test]
    fn context_selector_builds_a_deterministic_specificity_chain() {
        let selector = ContextSelector {
            fallback: "system".into(),
            rules: vec![
                ContextRule {
                    bar: "browser".into(),
                    scope: ContextScope::Application,
                    priority: 10,
                    predicate: ContextPredicate::Equals(
                        "application.id".into(),
                        ContextValue::Text("firefox".into()),
                    ),
                },
                ContextRule {
                    bar: "browser-lower-priority".into(),
                    scope: ContextScope::Application,
                    priority: 5,
                    predicate: ContextPredicate::Present("application.id".into()),
                },
                ContextRule {
                    bar: "private-window".into(),
                    scope: ContextScope::Window,
                    priority: 10,
                    predicate: ContextPredicate::Equals(
                        "window.private".into(),
                        ContextValue::Boolean(true),
                    ),
                },
            ],
        };
        let context = ContextSnapshot::default()
            .text("application.id", "firefox")
            .boolean("window.private", true);

        assert_eq!(
            selector.select_chain(&context),
            vec![
                BarId::from("private-window"),
                BarId::from("browser"),
                BarId::from("system")
            ]
        );
    }

    #[test]
    fn focus_updates_commit_after_all_captured_contacts_end() {
        let selector = ContextSelector {
            fallback: "system".into(),
            rules: vec![ContextRule {
                bar: "editor".into(),
                scope: ContextScope::Application,
                priority: 0,
                predicate: ContextPredicate::Equals(
                    "application.id".into(),
                    ContextValue::Text("editor".into()),
                ),
            }],
        };
        let initial = ContextSnapshot::default().text("application.id", "terminal");
        let mut context = TransactionalContext::new(selector, &initial);
        context.capture_started(7);
        context.capture_started(8);

        let editor = ContextSnapshot {
            generation: 2,
            ..ContextSnapshot::default().text("application.id", "editor")
        };
        assert_eq!(context.update(&editor), None);
        assert_eq!(context.current().active_bars, vec![BarId::from("system")]);
        assert_eq!(context.capture_ended(7), None);
        assert_eq!(context.current().active_bars, vec![BarId::from("system")]);

        let committed = context.capture_ended(8).unwrap();
        assert_eq!(committed.context_generation, 2);
        assert_eq!(
            committed.active_bars,
            vec![BarId::from("editor"), BarId::from("system")]
        );
    }
}
