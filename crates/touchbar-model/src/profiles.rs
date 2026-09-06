//! User-owned profile templates and deterministic contextual composition.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    ops::Range,
};

use touchbar_layout::{
    BarSpec, Element, GroupElement, GroupLayout, GroupSpec, ItemId, MAX_GROUP_DEPTH,
};

use crate::{
    BarElement, BarGroup, BarGroupElement, ContextPredicate, ContextScope, ContextSnapshot,
    Registry,
};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

string_id!(ProfileId);
string_id!(SlotId);
string_id!(ContributionId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotPolicy {
    /// Include every active binding in user-defined order.
    Collect,
    /// Include only the highest priority active binding.
    Select,
    /// Include every binding regardless of its context predicate.
    Fixed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContributionDefinition {
    pub id: ContributionId,
    pub elements: Vec<BarElement>,
    pub predicate: ContextPredicate,
    pub scope: ContextScope,
    pub priority: i32,
}

impl ContributionDefinition {
    pub fn new(id: impl Into<ContributionId>, elements: Vec<BarElement>) -> Self {
        Self {
            id: id.into(),
            elements,
            predicate: ContextPredicate::Always,
            scope: ContextScope::Global,
            priority: 0,
        }
    }

    pub fn when(mut self, predicate: ContextPredicate) -> Self {
        self.predicate = predicate;
        self
    }

    pub fn scope(mut self, scope: ContextScope) -> Self {
        self.scope = scope;
        self
    }

    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContributionBinding {
    pub contribution: ContributionId,
    pub order: i32,
}

impl ContributionBinding {
    pub fn new(contribution: impl Into<ContributionId>) -> Self {
        Self {
            contribution: contribution.into(),
            order: 0,
        }
    }

    pub fn order(mut self, order: i32) -> Self {
        self.order = order;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotDefinition {
    pub id: SlotId,
    pub policy: SlotPolicy,
    pub bindings: Vec<ContributionBinding>,
}

impl SlotDefinition {
    pub fn new(
        id: impl Into<SlotId>,
        policy: SlotPolicy,
        bindings: Vec<ContributionBinding>,
    ) -> Self {
        Self {
            id: id.into(),
            policy,
            bindings,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileElement {
    Slot(SlotDefinition),
    Group(ProfileGroup),
    FixedSpace { width: u32 },
    FlexibleSpace { minimum: u32, weight: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileGroupElement {
    Slot(SlotDefinition),
    Group(ProfileGroup),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileGroup {
    pub id: String,
    pub elements: Vec<ProfileGroupElement>,
    pub layout: GroupLayout,
    pub spacing: u32,
    pub visibility_priority: i32,
    pub compression_priority: i32,
}

impl ProfileGroup {
    pub fn new(id: impl Into<String>, elements: Vec<ProfileGroupElement>) -> Self {
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
pub struct ProfileDefinition {
    pub id: ProfileId,
    pub elements: Vec<ProfileElement>,
    pub principal_item: Option<ItemId>,
}

impl ProfileDefinition {
    pub fn new(id: impl Into<ProfileId>, elements: Vec<ProfileElement>) -> Self {
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
pub struct ProfileCatalog {
    contributions: BTreeMap<ContributionId, ContributionDefinition>,
    profiles: BTreeMap<ProfileId, ProfileDefinition>,
}

fn validate_contribution_group(
    registry: &Registry,
    contribution: &ContributionDefinition,
    group: &BarGroup,
    depth: usize,
    group_ids: &mut BTreeSet<String>,
) -> Result<(), ProfileError> {
    if group.id.trim().is_empty() {
        return Err(ProfileError::InvalidContributionGroupId {
            contribution: contribution.id.clone(),
            group: group.id.clone(),
        });
    }
    if depth > MAX_GROUP_DEPTH {
        return Err(ProfileError::ContributionGroupDepthExceeded {
            contribution: contribution.id.clone(),
            group: group.id.clone(),
        });
    }
    if !group_ids.insert(group.id.clone()) {
        return Err(ProfileError::DuplicateContributionGroup {
            contribution: contribution.id.clone(),
            group: group.id.clone(),
        });
    }
    for element in &group.elements {
        match element {
            BarGroupElement::Item(item) if registry.item(item).is_none() => {
                return Err(ProfileError::UnknownItem {
                    contribution: contribution.id.clone(),
                    item: item.clone(),
                });
            }
            BarGroupElement::Group(child) => {
                validate_contribution_group(registry, contribution, child, depth + 1, group_ids)?;
            }
            BarGroupElement::Item(_) => {}
        }
    }
    Ok(())
}

impl ProfileCatalog {
    pub fn add_contribution(
        &mut self,
        contribution: ContributionDefinition,
    ) -> Result<(), ProfileError> {
        if contribution.id.as_str().trim().is_empty() {
            return Err(ProfileError::EmptyContributionId);
        }
        let id = contribution.id.clone();
        if self.contributions.contains_key(&id) {
            return Err(ProfileError::DuplicateContribution(id));
        }
        self.contributions.insert(id, contribution);
        Ok(())
    }

    pub fn add_profile(&mut self, profile: ProfileDefinition) -> Result<(), ProfileError> {
        if profile.id.as_str().trim().is_empty() {
            return Err(ProfileError::EmptyProfileId);
        }
        let id = profile.id.clone();
        if self.profiles.contains_key(&id) {
            return Err(ProfileError::DuplicateProfile(id));
        }
        self.profiles.insert(id, profile);
        Ok(())
    }

    pub fn profile(&self, id: &ProfileId) -> Option<&ProfileDefinition> {
        self.profiles.get(id)
    }

    pub fn contribution(&self, id: &ContributionId) -> Option<&ContributionDefinition> {
        self.contributions.get(id)
    }

    pub fn validate(&self, registry: &Registry) -> Result<(), ProfileError> {
        for contribution in self.contributions.values() {
            let mut group_ids = BTreeSet::new();
            for element in &contribution.elements {
                match element {
                    BarElement::Item(item) if registry.item(item).is_none() => {
                        return Err(ProfileError::UnknownItem {
                            contribution: contribution.id.clone(),
                            item: item.clone(),
                        });
                    }
                    BarElement::Group(group) => validate_contribution_group(
                        registry,
                        contribution,
                        group,
                        1,
                        &mut group_ids,
                    )?,
                    BarElement::FlexibleSpace { weight: 0, .. } => {
                        return Err(ProfileError::ZeroFlexibleWeightInContribution(
                            contribution.id.clone(),
                        ));
                    }
                    BarElement::ContextProxy => {
                        return Err(ProfileError::ContextProxyInContribution(
                            contribution.id.clone(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        for profile in self.profiles.values() {
            let mut slots = BTreeSet::new();
            let mut groups = BTreeSet::new();
            for element in &profile.elements {
                match element {
                    ProfileElement::Slot(slot) => {
                        self.validate_slot(profile, slot, false, &mut slots)?;
                    }
                    ProfileElement::Group(group) => {
                        self.validate_profile_group(profile, group, 1, &mut slots, &mut groups)?;
                    }
                    ProfileElement::FlexibleSpace { weight: 0, .. } => {
                        return Err(ProfileError::ZeroFlexibleWeightInProfile(
                            profile.id.clone(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn validate_slot(
        &self,
        profile: &ProfileDefinition,
        slot: &SlotDefinition,
        nested: bool,
        slots: &mut BTreeSet<SlotId>,
    ) -> Result<(), ProfileError> {
        if slot.id.as_str().trim().is_empty() {
            return Err(ProfileError::EmptySlotId(profile.id.clone()));
        }
        if !slots.insert(slot.id.clone()) {
            return Err(ProfileError::DuplicateSlot {
                profile: profile.id.clone(),
                slot: slot.id.clone(),
            });
        }
        for binding in &slot.bindings {
            let contribution = self
                .contributions
                .get(&binding.contribution)
                .ok_or_else(|| ProfileError::UnknownContribution {
                    profile: profile.id.clone(),
                    slot: slot.id.clone(),
                    contribution: binding.contribution.clone(),
                })?;
            if nested
                && contribution.elements.iter().any(|element| {
                    matches!(
                        element,
                        BarElement::FixedSpace { .. } | BarElement::FlexibleSpace { .. }
                    )
                })
            {
                return Err(ProfileError::SpaceInGroupedSlot {
                    profile: profile.id.clone(),
                    slot: slot.id.clone(),
                    contribution: binding.contribution.clone(),
                });
            }
        }
        Ok(())
    }

    fn validate_profile_group(
        &self,
        profile: &ProfileDefinition,
        group: &ProfileGroup,
        depth: usize,
        slots: &mut BTreeSet<SlotId>,
        groups: &mut BTreeSet<String>,
    ) -> Result<(), ProfileError> {
        if group.id.trim().is_empty() || group.id.starts_with("@slot/") {
            return Err(ProfileError::InvalidGroupId {
                profile: profile.id.clone(),
                group: group.id.clone(),
            });
        }
        if depth > MAX_GROUP_DEPTH {
            return Err(ProfileError::GroupDepthExceeded {
                profile: profile.id.clone(),
                group: group.id.clone(),
            });
        }
        if !groups.insert(group.id.clone()) {
            return Err(ProfileError::DuplicateGroup {
                profile: profile.id.clone(),
                group: group.id.clone(),
            });
        }
        for element in &group.elements {
            match element {
                ProfileGroupElement::Slot(slot) => {
                    if depth >= MAX_GROUP_DEPTH {
                        return Err(ProfileError::GroupDepthExceeded {
                            profile: profile.id.clone(),
                            group: group.id.clone(),
                        });
                    }
                    self.validate_slot(profile, slot, true, slots)?;
                }
                ProfileGroupElement::Group(child) => {
                    self.validate_profile_group(profile, child, depth + 1, slots, groups)?
                }
            }
        }
        Ok(())
    }

    pub fn compose(
        &self,
        registry: &Registry,
        profile_id: &ProfileId,
        context: &ContextSnapshot,
    ) -> Result<ProfileComposition, ProfileError> {
        self.validate(registry)?;
        let profile = self
            .profiles
            .get(profile_id)
            .ok_or_else(|| ProfileError::UnknownProfile(profile_id.clone()))?;
        let mut elements = Vec::new();
        let mut active_contributions = Vec::new();
        let mut slots = Vec::new();
        let mut item_ids = BTreeSet::new();
        let mut group_ids = BTreeSet::new();

        for profile_element in &profile.elements {
            match profile_element {
                ProfileElement::FixedSpace { width } => {
                    elements.push(Element::FixedSpace { width: *width });
                }
                ProfileElement::FlexibleSpace { minimum, weight } => {
                    elements.push(Element::FlexibleSpace {
                        minimum: *minimum,
                        weight: *weight,
                    });
                }
                ProfileElement::Slot(slot) => {
                    let start = elements.len();
                    elements.extend(self.compose_top_slot(
                        registry,
                        slot,
                        context,
                        &mut active_contributions,
                        &mut item_ids,
                        &mut group_ids,
                    )?);
                    slots.push(ComposedSlot {
                        id: slot.id.clone(),
                        path: Vec::new(),
                        elements: start..elements.len(),
                    });
                }
                ProfileElement::Group(group) => {
                    let path = vec![elements.len()];
                    elements.push(Element::Group(self.compose_profile_group(
                        registry,
                        &profile.id,
                        group,
                        context,
                        path,
                        &mut active_contributions,
                        &mut slots,
                        &mut item_ids,
                        &mut group_ids,
                    )?));
                }
            }
        }

        let principal_item = profile
            .principal_item
            .as_ref()
            .filter(|principal| item_ids.contains(*principal))
            .cloned();
        Ok(ProfileComposition {
            profile: profile.id.clone(),
            active_contributions,
            slots,
            bar: BarSpec {
                elements,
                principal_item,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn compose_profile_group(
        &self,
        registry: &Registry,
        profile: &ProfileId,
        group: &ProfileGroup,
        context: &ContextSnapshot,
        path: Vec<usize>,
        active_contributions: &mut Vec<ContributionId>,
        slots: &mut Vec<ComposedSlot>,
        item_ids: &mut BTreeSet<ItemId>,
        group_ids: &mut BTreeSet<String>,
    ) -> Result<GroupSpec, ProfileError> {
        insert_composed_group(group_ids, &group.id)?;
        let mut elements = Vec::new();
        for element in &group.elements {
            let mut child_path = path.clone();
            child_path.push(elements.len());
            match element {
                ProfileGroupElement::Slot(slot) => {
                    let slot_elements = self.compose_group_slot(
                        registry,
                        slot,
                        context,
                        active_contributions,
                        item_ids,
                        group_ids,
                    )?;
                    let slot_group_id = format!("@slot/{profile}/{slot_id}", slot_id = slot.id);
                    insert_composed_group(group_ids, &slot_group_id)?;
                    slots.push(ComposedSlot {
                        id: slot.id.clone(),
                        path: child_path,
                        elements: 0..slot_elements.len(),
                    });
                    elements.push(GroupElement::Group(GroupSpec {
                        id: slot_group_id,
                        elements: slot_elements,
                        layout: GroupLayout::Natural,
                        spacing: 0,
                        visibility_priority: 0,
                        compression_priority: 0,
                    }));
                }
                ProfileGroupElement::Group(child) => {
                    elements.push(GroupElement::Group(self.compose_profile_group(
                        registry,
                        profile,
                        child,
                        context,
                        child_path,
                        active_contributions,
                        slots,
                        item_ids,
                        group_ids,
                    )?));
                }
            }
        }
        Ok(GroupSpec {
            id: group.id.clone(),
            elements,
            layout: group.layout,
            spacing: group.spacing,
            visibility_priority: group.visibility_priority,
            compression_priority: group.compression_priority,
        })
    }

    fn compose_top_slot(
        &self,
        registry: &Registry,
        slot: &SlotDefinition,
        context: &ContextSnapshot,
        active_contributions: &mut Vec<ContributionId>,
        item_ids: &mut BTreeSet<ItemId>,
        group_ids: &mut BTreeSet<String>,
    ) -> Result<Vec<Element>, ProfileError> {
        let mut elements = Vec::new();
        for contribution in self.select_bindings(slot, context) {
            active_contributions.push(contribution.id.clone());
            for element in &contribution.elements {
                elements.push(resolve_top_contribution_element(
                    registry,
                    contribution,
                    element,
                    item_ids,
                    group_ids,
                )?);
            }
        }
        Ok(elements)
    }

    fn compose_group_slot(
        &self,
        registry: &Registry,
        slot: &SlotDefinition,
        context: &ContextSnapshot,
        active_contributions: &mut Vec<ContributionId>,
        item_ids: &mut BTreeSet<ItemId>,
        group_ids: &mut BTreeSet<String>,
    ) -> Result<Vec<GroupElement>, ProfileError> {
        let mut elements = Vec::new();
        for contribution in self.select_bindings(slot, context) {
            active_contributions.push(contribution.id.clone());
            for element in &contribution.elements {
                elements.push(resolve_grouped_contribution_element(
                    registry,
                    contribution,
                    element,
                    item_ids,
                    group_ids,
                )?);
            }
        }
        Ok(elements)
    }

    fn select_bindings<'a>(
        &'a self,
        slot: &SlotDefinition,
        context: &ContextSnapshot,
    ) -> Vec<&'a ContributionDefinition> {
        let mut bindings = slot.bindings.iter().enumerate().collect::<Vec<_>>();
        bindings.sort_by_key(|(index, binding)| (binding.order, *index));
        let candidates = bindings
            .into_iter()
            .enumerate()
            .filter_map(|(selection_index, (_, binding))| {
                let contribution = self.contributions.get(&binding.contribution)?;
                (slot.policy == SlotPolicy::Fixed || contribution.predicate.matches(context))
                    .then_some((selection_index, contribution))
            })
            .collect::<Vec<_>>();

        if slot.policy != SlotPolicy::Select {
            return candidates
                .into_iter()
                .map(|(_, contribution)| contribution)
                .collect();
        }
        candidates
            .into_iter()
            .max_by(|(left_index, left), (right_index, right)| {
                left.priority
                    .cmp(&right.priority)
                    .then_with(|| left.scope.cmp(&right.scope))
                    .then_with(|| right_index.cmp(left_index))
            })
            .map(|(_, contribution)| vec![contribution])
            .unwrap_or_default()
    }
}

fn insert_composed_group(
    group_ids: &mut BTreeSet<String>,
    group: &str,
) -> Result<(), ProfileError> {
    if !group_ids.insert(group.to_owned()) {
        return Err(ProfileError::DuplicateComposedGroup(group.to_owned()));
    }
    Ok(())
}

fn resolve_top_contribution_element(
    registry: &Registry,
    contribution: &ContributionDefinition,
    element: &BarElement,
    item_ids: &mut BTreeSet<ItemId>,
    group_ids: &mut BTreeSet<String>,
) -> Result<Element, ProfileError> {
    match element {
        BarElement::Item(id) => {
            resolve_item(registry, contribution, id, item_ids).map(Element::Item)
        }
        BarElement::Group(group) => {
            resolve_contribution_group(registry, contribution, group, item_ids, group_ids)
                .map(Element::Group)
        }
        BarElement::FixedSpace { width } => Ok(Element::FixedSpace { width: *width }),
        BarElement::FlexibleSpace { minimum, weight } => Ok(Element::FlexibleSpace {
            minimum: *minimum,
            weight: *weight,
        }),
        BarElement::ContextProxy => {
            unreachable!("profile validation rejects contribution context proxies")
        }
    }
}

fn resolve_grouped_contribution_element(
    registry: &Registry,
    contribution: &ContributionDefinition,
    element: &BarElement,
    item_ids: &mut BTreeSet<ItemId>,
    group_ids: &mut BTreeSet<String>,
) -> Result<GroupElement, ProfileError> {
    match element {
        BarElement::Item(id) => {
            resolve_item(registry, contribution, id, item_ids).map(GroupElement::Item)
        }
        BarElement::Group(group) => {
            resolve_contribution_group(registry, contribution, group, item_ids, group_ids)
                .map(GroupElement::Group)
        }
        BarElement::FixedSpace { .. } | BarElement::FlexibleSpace { .. } => {
            unreachable!("profile validation rejects spaces inside grouped slots")
        }
        BarElement::ContextProxy => {
            unreachable!("profile validation rejects contribution context proxies")
        }
    }
}

fn resolve_item(
    registry: &Registry,
    contribution: &ContributionDefinition,
    id: &ItemId,
    item_ids: &mut BTreeSet<ItemId>,
) -> Result<touchbar_layout::ItemSpec, ProfileError> {
    if !item_ids.insert(id.clone()) {
        return Err(ProfileError::DuplicateComposedItem(id.clone()));
    }
    registry
        .item(id)
        .map(|definition| definition.layout.clone())
        .ok_or_else(|| ProfileError::UnknownItem {
            contribution: contribution.id.clone(),
            item: id.clone(),
        })
}

fn resolve_contribution_group(
    registry: &Registry,
    contribution: &ContributionDefinition,
    group: &BarGroup,
    item_ids: &mut BTreeSet<ItemId>,
    group_ids: &mut BTreeSet<String>,
) -> Result<GroupSpec, ProfileError> {
    insert_composed_group(group_ids, &group.id)?;
    let elements = group
        .elements
        .iter()
        .map(|element| match element {
            BarGroupElement::Item(id) => {
                resolve_item(registry, contribution, id, item_ids).map(GroupElement::Item)
            }
            BarGroupElement::Group(child) => {
                resolve_contribution_group(registry, contribution, child, item_ids, group_ids)
                    .map(GroupElement::Group)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GroupSpec {
        id: group.id.clone(),
        elements,
        layout: group.layout,
        spacing: group.spacing,
        visibility_priority: group.visibility_priority,
        compression_priority: group.compression_priority,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposedSlot {
    pub id: SlotId,
    /// Group indices from `ProfileComposition::bar.elements` to the container.
    /// An empty path identifies the top-level bar.
    pub path: Vec<usize>,
    /// Half-open indices within the addressed container. Empty slots retain an
    /// insertion point with equal start and end indices.
    pub elements: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileComposition {
    pub profile: ProfileId,
    pub active_contributions: Vec<ContributionId>,
    pub slots: Vec<ComposedSlot>,
    pub bar: BarSpec,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileRule {
    pub profile: ProfileId,
    pub priority: i32,
    pub predicate: ContextPredicate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSelector {
    pub fallback: ProfileId,
    pub rules: Vec<ProfileRule>,
}

impl ProfileSelector {
    pub fn select(&self, context: &ContextSnapshot) -> ProfileId {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.predicate.matches(context))
            .max_by(|(left_index, left), (right_index, right)| {
                left.priority
                    .cmp(&right.priority)
                    .then_with(|| right_index.cmp(left_index))
            })
            .map_or_else(|| self.fallback.clone(), |(_, rule)| rule.profile.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ControlOwner(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControllerCommand {
    SetContext(ContextSnapshot),
    SelectProfile(ProfileId),
    UseAutomaticProfile,
    ActivateMode {
        owner: ControlOwner,
        mode: String,
        profile: ProfileId,
        priority: i32,
    },
    ReleaseMode {
        owner: ControlOwner,
        mode: String,
    },
    ReleaseOwner(ControlOwner),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompositionDelta {
    pub retained_items: Vec<ItemId>,
    pub entering_items: Vec<ItemId>,
    pub leaving_items: Vec<ItemId>,
}

impl CompositionDelta {
    fn between(previous: Option<&BarSpec>, next: &BarSpec) -> Self {
        let previous = previous.map_or_else(BTreeSet::new, item_ids);
        let next = item_ids(next);
        Self {
            retained_items: previous.intersection(&next).cloned().collect(),
            entering_items: next.difference(&previous).cloned().collect(),
            leaving_items: previous.difference(&next).cloned().collect(),
        }
    }
}

fn item_ids(bar: &BarSpec) -> BTreeSet<ItemId> {
    let mut ids = BTreeSet::new();
    for element in &bar.elements {
        match element {
            Element::Item(item) => {
                ids.insert(item.id.clone());
            }
            Element::Group(group) => collect_layout_group_item_ids(group, &mut ids),
            Element::FixedSpace { .. } | Element::FlexibleSpace { .. } => {}
        }
    }
    ids
}

fn collect_layout_group_item_ids(group: &GroupSpec, ids: &mut BTreeSet<ItemId>) {
    for element in &group.elements {
        match element {
            GroupElement::Item(item) => {
                ids.insert(item.id.clone());
            }
            GroupElement::Group(group) => collect_layout_group_item_ids(group, ids),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileCompositionSnapshot {
    pub generation: u64,
    pub context_generation: u64,
    pub composition: ProfileComposition,
    pub delta: CompositionDelta,
}

#[derive(Clone, Debug)]
struct ProfileLease {
    profile: ProfileId,
    priority: i32,
    sequence: u64,
}

pub struct CompositionController {
    catalog: ProfileCatalog,
    registry: Registry,
    selector: ProfileSelector,
    context: ContextSnapshot,
    manual_profile: Option<ProfileId>,
    modes: BTreeMap<(ControlOwner, String), ProfileLease>,
    current: ProfileCompositionSnapshot,
    pending: Option<ProfileCompositionSnapshot>,
    captured_contacts: BTreeSet<u32>,
    next_generation: u64,
    next_mode_sequence: u64,
}

impl CompositionController {
    pub fn new(
        catalog: ProfileCatalog,
        registry: Registry,
        selector: ProfileSelector,
        context: ContextSnapshot,
    ) -> Result<Self, ProfileError> {
        catalog.validate(&registry)?;
        validate_selector(&catalog, &selector)?;
        let profile = selector.select(&context);
        let composition = catalog.compose(&registry, &profile, &context)?;
        let delta = CompositionDelta::between(None, &composition.bar);
        Ok(Self {
            catalog,
            registry,
            selector,
            context: context.clone(),
            manual_profile: None,
            modes: BTreeMap::new(),
            current: ProfileCompositionSnapshot {
                generation: 1,
                context_generation: context.generation,
                composition,
                delta,
            },
            pending: None,
            captured_contacts: BTreeSet::new(),
            next_generation: 2,
            next_mode_sequence: 1,
        })
    }

    pub fn current(&self) -> &ProfileCompositionSnapshot {
        &self.current
    }

    pub fn context(&self) -> &ContextSnapshot {
        &self.context
    }

    pub fn apply(
        &mut self,
        command: ControllerCommand,
    ) -> Result<Option<&ProfileCompositionSnapshot>, ProfileError> {
        let previous_context = self.context.clone();
        let previous_manual_profile = self.manual_profile.clone();
        let previous_modes = self.modes.clone();
        let previous_mode_sequence = self.next_mode_sequence;
        let changed = match command {
            ControllerCommand::SetContext(context) => {
                self.context = context;
                true
            }
            ControllerCommand::SelectProfile(profile) => {
                self.require_profile(&profile)?;
                self.manual_profile = Some(profile);
                true
            }
            ControllerCommand::UseAutomaticProfile => self.manual_profile.take().is_some(),
            ControllerCommand::ActivateMode {
                owner,
                mode,
                profile,
                priority,
            } => {
                self.require_profile(&profile)?;
                if mode.trim().is_empty() {
                    return Err(ProfileError::EmptyModeId);
                }
                let lease = ProfileLease {
                    profile,
                    priority,
                    sequence: self.next_mode_sequence,
                };
                self.next_mode_sequence = self.next_mode_sequence.wrapping_add(1).max(1);
                self.modes.insert((owner, mode), lease);
                true
            }
            ControllerCommand::ReleaseMode { owner, mode } => {
                self.modes.remove(&(owner, mode)).is_some()
            }
            ControllerCommand::ReleaseOwner(owner) => {
                let before = self.modes.len();
                self.modes.retain(|(candidate, _), _| *candidate != owner);
                self.modes.len() != before
            }
        };
        if !changed {
            return Ok(None);
        }
        let candidate = match self.build_snapshot() {
            Ok(candidate) => candidate,
            Err(error) => {
                self.context = previous_context;
                self.manual_profile = previous_manual_profile;
                self.modes = previous_modes;
                self.next_mode_sequence = previous_mode_sequence;
                return Err(error);
            }
        };
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        if self.captured_contacts.is_empty() {
            self.current = candidate;
            Ok(Some(&self.current))
        } else {
            self.pending = Some(candidate);
            Ok(None)
        }
    }

    pub fn capture_started(&mut self, contact_id: u32) {
        self.captured_contacts.insert(contact_id);
    }

    pub fn capture_ended(&mut self, contact_id: u32) -> Option<&ProfileCompositionSnapshot> {
        self.captured_contacts.remove(&contact_id);
        if self.captured_contacts.is_empty()
            && let Some(pending) = self.pending.take()
        {
            self.current = pending;
            return Some(&self.current);
        }
        None
    }

    fn build_snapshot(&self) -> Result<ProfileCompositionSnapshot, ProfileError> {
        let profile = self.effective_profile();
        let composition = self
            .catalog
            .compose(&self.registry, &profile, &self.context)?;
        let delta =
            CompositionDelta::between(Some(&self.current.composition.bar), &composition.bar);
        Ok(ProfileCompositionSnapshot {
            generation: self.next_generation,
            context_generation: self.context.generation,
            composition,
            delta,
        })
    }

    fn effective_profile(&self) -> ProfileId {
        if let Some(profile) = &self.manual_profile {
            return profile.clone();
        }
        self.modes
            .values()
            .max_by_key(|lease| (lease.priority, lease.sequence))
            .map_or_else(
                || self.selector.select(&self.context),
                |lease| lease.profile.clone(),
            )
    }

    fn require_profile(&self, profile: &ProfileId) -> Result<(), ProfileError> {
        self.catalog
            .profile(profile)
            .map(|_| ())
            .ok_or_else(|| ProfileError::UnknownProfile(profile.clone()))
    }
}

fn validate_selector(
    catalog: &ProfileCatalog,
    selector: &ProfileSelector,
) -> Result<(), ProfileError> {
    if catalog.profile(&selector.fallback).is_none() {
        return Err(ProfileError::UnknownProfile(selector.fallback.clone()));
    }
    for rule in &selector.rules {
        if catalog.profile(&rule.profile).is_none() {
            return Err(ProfileError::UnknownProfile(rule.profile.clone()));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    EmptyContributionId,
    EmptyProfileId,
    EmptySlotId(ProfileId),
    EmptyModeId,
    DuplicateContribution(ContributionId),
    DuplicateProfile(ProfileId),
    DuplicateSlot {
        profile: ProfileId,
        slot: SlotId,
    },
    InvalidGroupId {
        profile: ProfileId,
        group: String,
    },
    DuplicateGroup {
        profile: ProfileId,
        group: String,
    },
    GroupDepthExceeded {
        profile: ProfileId,
        group: String,
    },
    InvalidContributionGroupId {
        contribution: ContributionId,
        group: String,
    },
    DuplicateContributionGroup {
        contribution: ContributionId,
        group: String,
    },
    ContributionGroupDepthExceeded {
        contribution: ContributionId,
        group: String,
    },
    DuplicateComposedGroup(String),
    UnknownContribution {
        profile: ProfileId,
        slot: SlotId,
        contribution: ContributionId,
    },
    UnknownProfile(ProfileId),
    UnknownItem {
        contribution: ContributionId,
        item: ItemId,
    },
    DuplicateComposedItem(ItemId),
    ContextProxyInContribution(ContributionId),
    ZeroFlexibleWeightInContribution(ContributionId),
    ZeroFlexibleWeightInProfile(ProfileId),
    SpaceInGroupedSlot {
        profile: ProfileId,
        slot: SlotId,
        contribution: ContributionId,
    },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyContributionId => write!(formatter, "contribution IDs must not be empty"),
            Self::EmptyProfileId => write!(formatter, "profile IDs must not be empty"),
            Self::EmptySlotId(profile) => {
                write!(formatter, "profile {profile} has an empty slot ID")
            }
            Self::EmptyModeId => write!(formatter, "mode IDs must not be empty"),
            Self::DuplicateContribution(id) => write!(formatter, "duplicate contribution {id}"),
            Self::DuplicateProfile(id) => write!(formatter, "duplicate profile {id}"),
            Self::DuplicateSlot { profile, slot } => {
                write!(
                    formatter,
                    "profile {profile} contains duplicate slot {slot}"
                )
            }
            Self::InvalidGroupId { profile, group } => {
                write!(
                    formatter,
                    "profile {profile} has invalid group ID {group:?}"
                )
            }
            Self::DuplicateGroup { profile, group } => {
                write!(
                    formatter,
                    "profile {profile} contains duplicate group {group}"
                )
            }
            Self::GroupDepthExceeded { profile, group } => write!(
                formatter,
                "profile {profile} group {group} exceeds the maximum nesting depth"
            ),
            Self::InvalidContributionGroupId {
                contribution,
                group,
            } => write!(
                formatter,
                "contribution {contribution} has invalid group ID {group:?}"
            ),
            Self::DuplicateContributionGroup {
                contribution,
                group,
            } => write!(
                formatter,
                "contribution {contribution} contains duplicate group {group}"
            ),
            Self::ContributionGroupDepthExceeded {
                contribution,
                group,
            } => write!(
                formatter,
                "contribution {contribution} group {group} exceeds the maximum nesting depth"
            ),
            Self::DuplicateComposedGroup(group) => {
                write!(
                    formatter,
                    "composed profile contains group {group} more than once"
                )
            }
            Self::UnknownContribution {
                profile,
                slot,
                contribution,
            } => write!(
                formatter,
                "profile {profile} slot {slot} references unknown contribution {contribution}"
            ),
            Self::UnknownProfile(profile) => write!(formatter, "unknown profile {profile}"),
            Self::UnknownItem { contribution, item } => {
                write!(
                    formatter,
                    "contribution {contribution} references unknown item {item}"
                )
            }
            Self::DuplicateComposedItem(item) => {
                write!(
                    formatter,
                    "composed profile contains item {item} more than once"
                )
            }
            Self::ContextProxyInContribution(contribution) => write!(
                formatter,
                "contribution {contribution} cannot contain a context proxy"
            ),
            Self::ZeroFlexibleWeightInContribution(contribution) => write!(
                formatter,
                "contribution {contribution} contains a zero-weight flexible space"
            ),
            Self::ZeroFlexibleWeightInProfile(profile) => write!(
                formatter,
                "profile {profile} contains a zero-weight flexible space"
            ),
            Self::SpaceInGroupedSlot {
                profile,
                slot,
                contribution,
            } => write!(
                formatter,
                "profile {profile} grouped slot {slot} binds contribution {contribution} containing a space"
            ),
        }
    }
}

impl Error for ProfileError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContextValue, ItemDefinition};
    use touchbar_layout::{ItemSpec, resolve};

    fn item(id: &str) -> ItemDefinition {
        ItemDefinition::new(ItemSpec::new(id, 40, 80, 240), id)
    }

    fn slot(id: &str, policy: SlotPolicy, contributions: &[(&str, i32)]) -> ProfileElement {
        ProfileElement::Slot(SlotDefinition::new(
            id,
            policy,
            contributions
                .iter()
                .map(|(id, order)| ContributionBinding::new(*id).order(*order))
                .collect(),
        ))
    }

    fn fixture() -> (Registry, ProfileCatalog, ProfileSelector) {
        let mut registry = Registry::default();
        for id in [
            "volume",
            "terminal",
            "browser",
            "status",
            "recording",
            "play",
            "timeline",
        ] {
            registry.add_item(item(id)).unwrap();
        }

        let mut catalog = ProfileCatalog::default();
        catalog
            .add_contribution(
                ContributionDefinition::new("persistent", vec![BarElement::Item("volume".into())])
                    // Fixed slots deliberately ignore this predicate.
                    .when(ContextPredicate::Present("never".into())),
            )
            .unwrap();
        catalog
            .add_contribution(
                ContributionDefinition::new("terminal", vec![BarElement::Item("terminal".into())])
                    .priority(-100),
            )
            .unwrap();
        catalog
            .add_contribution(
                ContributionDefinition::new("browser", vec![BarElement::Item("browser".into())])
                    .when(ContextPredicate::Equals(
                        "application.id".into(),
                        ContextValue::Text("firefox".into()),
                    ))
                    .scope(ContextScope::Application)
                    .priority(10),
            )
            .unwrap();
        catalog
            .add_contribution(ContributionDefinition::new(
                "status",
                vec![BarElement::Item("status".into())],
            ))
            .unwrap();
        catalog
            .add_contribution(
                ContributionDefinition::new(
                    "recording",
                    vec![BarElement::Item("recording".into())],
                )
                .when(ContextPredicate::Equals(
                    "recording".into(),
                    ContextValue::Boolean(true),
                )),
            )
            .unwrap();
        catalog
            .add_contribution(ContributionDefinition::new(
                "media",
                vec![
                    BarElement::Item("play".into()),
                    BarElement::Item("timeline".into()),
                ],
            ))
            .unwrap();

        catalog
            .add_profile(ProfileDefinition::new(
                "default",
                vec![
                    slot("persistent", SlotPolicy::Fixed, &[("persistent", 0)]),
                    ProfileElement::FlexibleSpace {
                        minimum: 8,
                        weight: 1,
                    },
                    slot(
                        "application",
                        SlotPolicy::Select,
                        &[("terminal", 0), ("browser", 10)],
                    ),
                    ProfileElement::FlexibleSpace {
                        minimum: 8,
                        weight: 1,
                    },
                    slot(
                        "status",
                        SlotPolicy::Collect,
                        &[("recording", -10), ("status", 0)],
                    ),
                ],
            ))
            .unwrap();
        catalog
            .add_profile(ProfileDefinition::new(
                "media",
                vec![
                    slot("persistent", SlotPolicy::Fixed, &[("persistent", 0)]),
                    ProfileElement::FlexibleSpace {
                        minimum: 8,
                        weight: 1,
                    },
                    slot("transport", SlotPolicy::Collect, &[("media", 0)]),
                ],
            ))
            .unwrap();

        let selector = ProfileSelector {
            fallback: "default".into(),
            rules: vec![ProfileRule {
                profile: "media".into(),
                priority: 10,
                predicate: ContextPredicate::Equals(
                    "mode.media".into(),
                    ContextValue::Boolean(true),
                ),
            }],
        };
        (registry, catalog, selector)
    }

    fn ordered_items(bar: &BarSpec) -> Vec<&str> {
        bar.elements
            .iter()
            .filter_map(|element| match element {
                Element::Item(item) => Some(item.id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn app_changes_replace_only_the_select_slot() {
        let (registry, catalog, _) = fixture();
        let terminal = ContextSnapshot::default().text("application.id", "terminal");
        let browser = ContextSnapshot::default().text("application.id", "firefox");

        let before = catalog
            .compose(&registry, &"default".into(), &terminal)
            .unwrap();
        let after = catalog
            .compose(&registry, &"default".into(), &browser)
            .unwrap();
        assert_eq!(ordered_items(&before.bar), ["volume", "terminal", "status"]);
        assert_eq!(ordered_items(&after.bar), ["volume", "browser", "status"]);
        assert_eq!(
            after.slots,
            [
                ComposedSlot {
                    id: "persistent".into(),
                    path: vec![],
                    elements: 0..1,
                },
                ComposedSlot {
                    id: "application".into(),
                    path: vec![],
                    elements: 2..3,
                },
                ComposedSlot {
                    id: "status".into(),
                    path: vec![],
                    elements: 4..5,
                },
            ]
        );
        assert_eq!(
            after.active_contributions,
            [
                ContributionId::from("persistent"),
                ContributionId::from("browser"),
                ContributionId::from("status")
            ]
        );
        assert!(resolve(&after.bar, 2008, &BTreeSet::new()).is_ok());
    }

    #[test]
    fn collect_slots_include_all_active_contributions_in_user_order() {
        let (registry, catalog, _) = fixture();
        let context = ContextSnapshot::default().boolean("recording", true);
        let composition = catalog
            .compose(&registry, &"default".into(), &context)
            .unwrap();
        assert_eq!(
            ordered_items(&composition.bar),
            ["volume", "terminal", "recording", "status"]
        );
    }

    #[test]
    fn profile_switch_reports_stable_items_for_surface_reconciliation() {
        let (registry, catalog, selector) = fixture();
        let context = ContextSnapshot::default().text("application.id", "firefox");
        let mut controller =
            CompositionController::new(catalog, registry, selector, context).unwrap();
        let snapshot = controller
            .apply(ControllerCommand::SelectProfile("media".into()))
            .unwrap()
            .unwrap();

        assert_eq!(
            ordered_items(&snapshot.composition.bar),
            ["volume", "play", "timeline"]
        );
        assert_eq!(snapshot.delta.retained_items, [ItemId::from("volume")]);
        assert_eq!(
            snapshot.delta.entering_items,
            [ItemId::from("play"), ItemId::from("timeline")]
        );
        assert_eq!(
            snapshot.delta.leaving_items,
            [ItemId::from("browser"), ItemId::from("status")]
        );
    }

    #[test]
    fn manual_selection_overrides_leased_and_automatic_profiles() {
        let (registry, catalog, selector) = fixture();
        let context = ContextSnapshot::default().boolean("mode.media", false);
        let mut controller =
            CompositionController::new(catalog, registry, selector, context).unwrap();
        let owner = ControlOwner(7);

        controller
            .apply(ControllerCommand::ActivateMode {
                owner,
                mode: "editing".into(),
                profile: "media".into(),
                priority: 20,
            })
            .unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("media")
        );

        controller
            .apply(ControllerCommand::SelectProfile("default".into()))
            .unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("default")
        );

        controller
            .apply(ControllerCommand::UseAutomaticProfile)
            .unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("media")
        );

        controller
            .apply(ControllerCommand::ReleaseOwner(owner))
            .unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("default")
        );
    }

    #[test]
    fn automatic_profile_rules_follow_context() {
        let (registry, catalog, selector) = fixture();
        let context = ContextSnapshot::default().boolean("mode.media", true);
        let controller = CompositionController::new(catalog, registry, selector, context).unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("media")
        );
    }

    #[test]
    fn profile_leases_resolve_by_priority_then_recency() {
        let (registry, catalog, selector) = fixture();
        let mut controller =
            CompositionController::new(catalog, registry, selector, ContextSnapshot::default())
                .unwrap();
        controller
            .apply(ControllerCommand::ActivateMode {
                owner: ControlOwner(1),
                mode: "first".into(),
                profile: "media".into(),
                priority: 10,
            })
            .unwrap();
        controller
            .apply(ControllerCommand::ActivateMode {
                owner: ControlOwner(2),
                mode: "higher".into(),
                profile: "default".into(),
                priority: 20,
            })
            .unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("default")
        );

        controller
            .apply(ControllerCommand::ActivateMode {
                owner: ControlOwner(3),
                mode: "newer-equal".into(),
                profile: "media".into(),
                priority: 20,
            })
            .unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("media")
        );

        controller
            .apply(ControllerCommand::ReleaseOwner(ControlOwner(3)))
            .unwrap();
        assert_eq!(
            controller.current().composition.profile,
            ProfileId::from("default")
        );
    }

    #[test]
    fn latest_profile_change_waits_for_every_captured_contact() {
        let (registry, catalog, selector) = fixture();
        let context = ContextSnapshot::default().text("application.id", "terminal");
        let mut controller =
            CompositionController::new(catalog, registry, selector, context).unwrap();
        controller.capture_started(1);
        controller.capture_started(2);

        let browser = ContextSnapshot {
            generation: 2,
            ..ContextSnapshot::default().text("application.id", "firefox")
        };
        assert_eq!(
            controller.apply(ControllerCommand::SetContext(browser)),
            Ok(None)
        );
        assert_eq!(
            controller.apply(ControllerCommand::SelectProfile("media".into())),
            Ok(None)
        );
        assert_eq!(
            ordered_items(&controller.current().composition.bar),
            ["volume", "terminal", "status"]
        );
        assert_eq!(controller.capture_ended(1), None);

        let committed = controller.capture_ended(2).unwrap();
        assert_eq!(committed.context_generation, 2);
        assert_eq!(
            ordered_items(&committed.composition.bar),
            ["volume", "play", "timeline"]
        );
        assert_eq!(
            committed.delta.leaving_items,
            [ItemId::from("status"), ItemId::from("terminal")]
        );
    }

    #[test]
    fn invalid_contextual_composition_rolls_back_the_command() {
        let (mut registry, mut catalog, selector) = fixture();
        registry.add_item(item("duplicate")).unwrap();
        catalog
            .add_contribution(ContributionDefinition::new(
                "duplicate-a",
                vec![BarElement::Item("duplicate".into())],
            ))
            .unwrap();
        catalog
            .add_contribution(
                ContributionDefinition::new(
                    "duplicate-b",
                    vec![BarElement::Item("duplicate".into())],
                )
                .when(ContextPredicate::Equals(
                    "bad".into(),
                    ContextValue::Boolean(true),
                )),
            )
            .unwrap();
        catalog
            .profiles
            .get_mut(&ProfileId::from("default"))
            .unwrap()
            .elements
            .push(slot(
                "invalid-when-active",
                SlotPolicy::Collect,
                &[("duplicate-a", 0), ("duplicate-b", 1)],
            ));
        let initial = ContextSnapshot {
            generation: 1,
            ..ContextSnapshot::default().boolean("bad", false)
        };
        let mut controller =
            CompositionController::new(catalog, registry, selector, initial).unwrap();
        let invalid = ContextSnapshot {
            generation: 2,
            ..ContextSnapshot::default().boolean("bad", true)
        };

        assert_eq!(
            controller.apply(ControllerCommand::SetContext(invalid)),
            Err(ProfileError::DuplicateComposedItem("duplicate".into()))
        );
        assert_eq!(controller.context().generation, 1);
        assert_eq!(controller.current().generation, 1);
    }

    #[test]
    fn equal_priority_select_ties_follow_binding_order() {
        let (registry, mut catalog, _) = fixture();
        catalog
            .contributions
            .get_mut(&"terminal".into())
            .unwrap()
            .priority = 10;
        catalog
            .contributions
            .get_mut(&"terminal".into())
            .unwrap()
            .scope = ContextScope::Application;
        let context = ContextSnapshot::default().text("application.id", "firefox");
        let composition = catalog
            .compose(&registry, &"default".into(), &context)
            .unwrap();
        assert!(ordered_items(&composition.bar).contains(&"terminal"));
        assert!(!ordered_items(&composition.bar).contains(&"browser"));
    }

    #[test]
    fn grouped_slots_compose_to_stable_paths_and_equal_width_columns() {
        let mut registry = Registry::default();
        registry.add_item(item("left")).unwrap();
        registry.add_item(item("right")).unwrap();
        let mut catalog = ProfileCatalog::default();
        catalog
            .add_contribution(ContributionDefinition::new(
                "left-content",
                vec![BarElement::Item("left".into())],
            ))
            .unwrap();
        catalog
            .add_contribution(ContributionDefinition::new(
                "right-content",
                vec![BarElement::Item("right".into())],
            ))
            .unwrap();
        catalog
            .add_profile(ProfileDefinition::new(
                "grouped",
                vec![ProfileElement::Group(
                    ProfileGroup::new(
                        "columns",
                        vec![
                            ProfileGroupElement::Slot(SlotDefinition::new(
                                "left-slot",
                                SlotPolicy::Fixed,
                                vec![ContributionBinding::new("left-content")],
                            )),
                            ProfileGroupElement::Slot(SlotDefinition::new(
                                "right-slot",
                                SlotPolicy::Fixed,
                                vec![ContributionBinding::new("right-content")],
                            )),
                        ],
                    )
                    .layout(GroupLayout::EqualWidth)
                    .spacing(4),
                )],
            ))
            .unwrap();

        let composition = catalog
            .compose(&registry, &"grouped".into(), &ContextSnapshot::default())
            .unwrap();
        assert_eq!(composition.slots[0].path, [0, 0]);
        assert_eq!(composition.slots[1].path, [0, 1]);
        assert_eq!(composition.slots[0].elements, 0..1);
        let layout = resolve(&composition.bar, 164, &BTreeSet::new()).unwrap();
        assert_eq!(layout.placements[0].width, 80);
        assert_eq!(layout.placements[1].width, 80);
        assert_eq!(layout.placements[1].x, 84);
    }
}
