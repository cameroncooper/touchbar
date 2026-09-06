use std::collections::BTreeSet;

use anyhow::{Result, bail};
use touchbar_layout::{
    BarSpec, Element, GroupElement, GroupSpec, ItemSpec, LayoutError, Placement, resolve,
};
use touchbar_model::ComposedSlot;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Anchor {
    pub x: u32,
    pub width: u32,
}

/// Resolve one bounded presentation and then anchor it to its compact item.
/// Sizing comes from the same resolver as normal bar content; anchoring only
/// translates the resulting interval and clamps it to the available bar.
pub fn resolve_popover(
    anchor: Anchor,
    item: ItemSpec,
    available_width: u32,
) -> Result<Placement, LayoutError> {
    let bar = BarSpec {
        elements: vec![Element::Item(item)],
        principal_item: None,
    };
    let mut placement = resolve(&bar, available_width, &BTreeSet::new())?
        .placements
        .into_iter()
        .next()
        .expect("a valid one-item bar always produces one placement");
    let center = anchor.x.saturating_add(anchor.width / 2);
    placement.x = center
        .saturating_sub(placement.width / 2)
        .min(available_width - placement.width);
    Ok(placement)
}

/// Replaces the source item's compact constraints and lets the ordinary bar
/// resolver reflow every neighbor around it.
pub fn in_place_bar(compact: &BarSpec, source: &str, mut expanded: ItemSpec) -> Result<BarSpec> {
    expanded.id = source.to_owned().into();
    in_place_content_bar(
        compact,
        source,
        BarSpec {
            elements: vec![Element::Item(expanded)],
            principal_item: None,
        },
    )
}

/// Splices a complete presentation bar at the source item's compact position.
/// Any presented items already present elsewhere in the compact bar are removed
/// first, so a surface can only have one placement.
pub fn in_place_content_bar(compact: &BarSpec, source: &str, content: BarSpec) -> Result<BarSpec> {
    let content_items = item_ids(&content);
    let (path, index) = find_item_container(&compact.elements, source)
        .ok_or_else(|| anyhow::anyhow!("presentation source is not in the active bar"))?;
    let elements = rewrite_bar(
        &compact.elements,
        &path,
        index..index + 1,
        &content.elements,
        &content_items,
    )?;
    let resolved_items = item_ids_from_elements(&elements);
    Ok(BarSpec {
        elements,
        principal_item: content.principal_item.or_else(|| {
            compact
                .principal_item
                .as_ref()
                .filter(|principal| resolved_items.contains(principal))
                .cloned()
        }),
    })
}

/// Replaces one composed profile slot with the presenting surface. The source
/// is removed from any other slot so one Wayland surface can never appear
/// twice in a resolved bar.
pub fn slot_bar(
    compact: &BarSpec,
    slots: &[ComposedSlot],
    target: &str,
    source: &str,
    mut expanded: ItemSpec,
) -> Result<BarSpec> {
    expanded.id = source.to_owned().into();
    slot_content_bar(
        compact,
        slots,
        target,
        BarSpec {
            elements: vec![Element::Item(expanded)],
            principal_item: None,
        },
    )
}

/// Replaces one composed profile slot with a complete presentation bar.
pub fn slot_content_bar(
    compact: &BarSpec,
    slots: &[ComposedSlot],
    target: &str,
    content: BarSpec,
) -> Result<BarSpec> {
    let slot = slots
        .iter()
        .find(|slot| slot.id.as_str() == target)
        .ok_or_else(|| anyhow::anyhow!("active profile has no slot `{target}`"))?;
    let content_items = item_ids(&content);
    let elements = rewrite_bar(
        &compact.elements,
        &slot.path,
        slot.elements.clone(),
        &content.elements,
        &content_items,
    )?;
    let resolved_items = item_ids_from_elements(&elements);
    Ok(BarSpec {
        elements,
        principal_item: content.principal_item.or_else(|| {
            compact
                .principal_item
                .as_ref()
                .filter(|principal| resolved_items.contains(principal))
                .cloned()
        }),
    })
}

fn item_ids(bar: &BarSpec) -> BTreeSet<touchbar_layout::ItemId> {
    item_ids_from_elements(&bar.elements)
}

fn item_ids_from_elements(elements: &[Element]) -> BTreeSet<touchbar_layout::ItemId> {
    let mut ids = BTreeSet::new();
    for element in elements {
        collect_element_item_ids(element, &mut ids);
    }
    ids
}

fn collect_element_item_ids(element: &Element, ids: &mut BTreeSet<touchbar_layout::ItemId>) {
    match element {
        Element::Item(item) => {
            ids.insert(item.id.clone());
        }
        Element::Group(group) => collect_group_item_ids(group, ids),
        Element::FixedSpace { .. } | Element::FlexibleSpace { .. } => {}
    }
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

fn find_item_container(elements: &[Element], source: &str) -> Option<(Vec<usize>, usize)> {
    for (index, element) in elements.iter().enumerate() {
        match element {
            Element::Item(item) if item.id.as_str() == source => return Some((Vec::new(), index)),
            Element::Group(group) => {
                let mut path = vec![index];
                if let Some(item_index) = find_item_in_group(group, source, &mut path) {
                    return Some((path, item_index));
                }
            }
            _ => {}
        }
    }
    None
}

fn find_item_in_group(group: &GroupSpec, source: &str, path: &mut Vec<usize>) -> Option<usize> {
    for (index, element) in group.elements.iter().enumerate() {
        match element {
            GroupElement::Item(item) if item.id.as_str() == source => return Some(index),
            GroupElement::Group(group) => {
                path.push(index);
                if let Some(item_index) = find_item_in_group(group, source, path) {
                    return Some(item_index);
                }
                path.pop();
            }
            _ => {}
        }
    }
    None
}

fn rewrite_bar(
    elements: &[Element],
    target_path: &[usize],
    target_range: std::ops::Range<usize>,
    replacement: &[Element],
    replacement_items: &BTreeSet<touchbar_layout::ItemId>,
) -> Result<Vec<Element>> {
    if target_path.is_empty() {
        if target_range.start > target_range.end || target_range.end > elements.len() {
            bail!("active profile returned an invalid slot span");
        }
        return Ok(rewrite_top_range(
            elements,
            target_range,
            replacement,
            replacement_items,
        ));
    }
    let grouped_replacement = replacement
        .iter()
        .map(element_to_group_element)
        .collect::<Result<Vec<_>>>()?;
    let target_index = target_path[0];
    if target_index >= elements.len() {
        bail!("active profile returned an invalid slot path");
    }
    let mut rewritten = Vec::with_capacity(elements.len());
    for (index, element) in elements.iter().enumerate() {
        if index == target_index {
            let Element::Group(group) = element else {
                bail!("active profile slot path does not identify a group");
            };
            rewritten.push(Element::Group(rewrite_group(
                group,
                &target_path[1..],
                target_range.clone(),
                &grouped_replacement,
                replacement_items,
            )?));
        } else if let Some(element) = prune_element(element, replacement_items) {
            rewritten.push(element);
        }
    }
    Ok(rewritten)
}

fn rewrite_top_range(
    elements: &[Element],
    target: std::ops::Range<usize>,
    replacement: &[Element],
    replacement_items: &BTreeSet<touchbar_layout::ItemId>,
) -> Vec<Element> {
    let mut rewritten = Vec::with_capacity(elements.len() + replacement.len());
    for index in 0..=elements.len() {
        if index == target.start {
            rewritten.extend_from_slice(replacement);
        }
        if index == elements.len() || target.contains(&index) {
            continue;
        }
        if let Some(element) = prune_element(&elements[index], replacement_items) {
            rewritten.push(element);
        }
    }
    rewritten
}

fn rewrite_group(
    group: &GroupSpec,
    path: &[usize],
    target_range: std::ops::Range<usize>,
    replacement: &[GroupElement],
    replacement_items: &BTreeSet<touchbar_layout::ItemId>,
) -> Result<GroupSpec> {
    let mut rewritten = group.clone();
    if path.is_empty() {
        if target_range.start > target_range.end || target_range.end > group.elements.len() {
            bail!("active profile returned an invalid slot span");
        }
        let mut elements = Vec::with_capacity(group.elements.len() + replacement.len());
        for index in 0..=group.elements.len() {
            if index == target_range.start {
                elements.extend_from_slice(replacement);
            }
            if index == group.elements.len() || target_range.contains(&index) {
                continue;
            }
            if let Some(element) = prune_group_element(&group.elements[index], replacement_items) {
                elements.push(element);
            }
        }
        rewritten.elements = elements;
        return Ok(rewritten);
    }

    let target_index = path[0];
    if target_index >= group.elements.len() {
        bail!("active profile returned an invalid slot path");
    }
    let mut elements = Vec::with_capacity(group.elements.len());
    for (index, element) in group.elements.iter().enumerate() {
        if index == target_index {
            let GroupElement::Group(child) = element else {
                bail!("active profile slot path does not identify a group");
            };
            elements.push(GroupElement::Group(rewrite_group(
                child,
                &path[1..],
                target_range.clone(),
                replacement,
                replacement_items,
            )?));
        } else if let Some(element) = prune_group_element(element, replacement_items) {
            elements.push(element);
        }
    }
    rewritten.elements = elements;
    Ok(rewritten)
}

fn element_to_group_element(element: &Element) -> Result<GroupElement> {
    match element {
        Element::Item(item) => Ok(GroupElement::Item(item.clone())),
        Element::Group(group) => Ok(GroupElement::Group(group.clone())),
        Element::FixedSpace { .. } | Element::FlexibleSpace { .. } => {
            bail!("a grouped presentation cannot contain top-level space elements")
        }
    }
}

fn prune_element(
    element: &Element,
    replacement_items: &BTreeSet<touchbar_layout::ItemId>,
) -> Option<Element> {
    match element {
        Element::Item(item) if replacement_items.contains(&item.id) => None,
        Element::Group(group) => Some(Element::Group(prune_group(group, replacement_items))),
        element => Some(element.clone()),
    }
}

fn prune_group(
    group: &GroupSpec,
    replacement_items: &BTreeSet<touchbar_layout::ItemId>,
) -> GroupSpec {
    let mut pruned = group.clone();
    pruned.elements = group
        .elements
        .iter()
        .filter_map(|element| prune_group_element(element, replacement_items))
        .collect();
    pruned
}

fn prune_group_element(
    element: &GroupElement,
    replacement_items: &BTreeSet<touchbar_layout::ItemId>,
) -> Option<GroupElement> {
    match element {
        GroupElement::Item(item) if replacement_items.contains(&item.id) => None,
        GroupElement::Group(group) => {
            Some(GroupElement::Group(prune_group(group, replacement_items)))
        }
        element => Some(element.clone()),
    }
}

pub fn full_bar(source: &str, available_width: u32) -> BarSpec {
    BarSpec {
        elements: vec![Element::Item(ItemSpec::new(
            source,
            available_width,
            available_width,
            available_width,
        ))],
        principal_item: Some(source.to_owned().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn popover(anchor: Anchor) -> Placement {
        resolve_popover(anchor, ItemSpec::new("volume.popover", 220, 360, 500), 1004).unwrap()
    }

    #[test]
    fn clamps_a_left_edge_popover() {
        assert_eq!(
            (
                popover(Anchor { x: 0, width: 80 }).x,
                popover(Anchor { x: 0, width: 80 }).width
            ),
            (0, 360)
        );
    }

    #[test]
    fn centers_a_middle_popover_on_its_anchor() {
        let placement = popover(Anchor { x: 400, width: 80 });
        assert_eq!((placement.x, placement.width), (260, 360));
    }

    #[test]
    fn clamps_a_right_edge_popover() {
        let placement = popover(Anchor { x: 924, width: 80 });
        assert_eq!((placement.x, placement.width), (644, 360));
    }

    fn compact_bar() -> BarSpec {
        BarSpec {
            elements: vec![
                Element::Item(ItemSpec::new("left", 40, 80, 120)),
                Element::FlexibleSpace {
                    minimum: 8,
                    weight: 1,
                },
                Element::Item(ItemSpec::new("source", 40, 80, 120)),
                Element::Item(ItemSpec::new("status", 40, 80, 120)),
            ],
            principal_item: Some("source".into()),
        }
    }

    fn items(bar: &BarSpec) -> Vec<(&str, u32)> {
        bar.elements
            .iter()
            .filter_map(|element| match element {
                Element::Item(item) => Some((item.id.as_str(), item.preferred_width)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn in_place_expansion_reuses_the_source_identity_and_reflows() {
        let bar = in_place_bar(
            &compact_bar(),
            "source",
            ItemSpec::new("client-local-popover-id", 200, 300, 500),
        )
        .unwrap();
        assert_eq!(items(&bar), [("left", 80), ("source", 300), ("status", 80)]);
        assert!(resolve(&bar, 600, &BTreeSet::new()).is_ok());
        assert!(in_place_bar(&compact_bar(), "missing", ItemSpec::new("x", 1, 1, 1)).is_err());
    }

    #[test]
    fn in_place_content_splices_multiple_items_without_duplicates() {
        let content = BarSpec {
            elements: vec![
                Element::Item(ItemSpec::new("source", 100, 160, 220)),
                Element::FixedSpace { width: 8 },
                Element::Item(ItemSpec::new("status", 60, 80, 120)),
            ],
            principal_item: Some("status".into()),
        };
        let bar = in_place_content_bar(&compact_bar(), "source", content).unwrap();
        assert_eq!(items(&bar), [("left", 80), ("source", 160), ("status", 80)]);
        assert_eq!(bar.principal_item, Some("status".into()));
    }

    #[test]
    fn named_slot_replaces_its_span_and_removes_a_source_from_elsewhere() {
        let slots = [
            ComposedSlot {
                id: "leading".into(),
                path: vec![],
                elements: 0..1,
            },
            ComposedSlot {
                id: "content".into(),
                path: vec![],
                elements: 2..3,
            },
            ComposedSlot {
                id: "status".into(),
                path: vec![],
                elements: 3..4,
            },
        ];
        let bar = slot_bar(
            &compact_bar(),
            &slots,
            "leading",
            "source",
            ItemSpec::new("ignored", 200, 260, 400),
        )
        .unwrap();
        assert_eq!(items(&bar), [("source", 260), ("status", 80)]);
        assert_eq!(bar.principal_item, Some("source".into()));
        assert!(
            slot_bar(
                &compact_bar(),
                &slots,
                "missing",
                "source",
                ItemSpec::new("ignored", 1, 1, 1)
            )
            .is_err()
        );
    }

    #[test]
    fn named_slot_accepts_a_complete_content_bar() {
        let slots = [
            ComposedSlot {
                id: "leading".into(),
                path: vec![],
                elements: 0..1,
            },
            ComposedSlot {
                id: "content".into(),
                path: vec![],
                elements: 2..3,
            },
            ComposedSlot {
                id: "status".into(),
                path: vec![],
                elements: 3..4,
            },
        ];
        let content = BarSpec {
            elements: vec![
                Element::Item(ItemSpec::new("source", 100, 180, 220)),
                Element::FlexibleSpace {
                    minimum: 8,
                    weight: 1,
                },
                Element::Item(ItemSpec::new("left", 60, 80, 100)),
            ],
            principal_item: Some("source".into()),
        };
        let bar = slot_content_bar(&compact_bar(), &slots, "content", content).unwrap();
        assert_eq!(items(&bar), [("source", 180), ("left", 80), ("status", 80)]);
        assert_eq!(bar.principal_item, Some("source".into()));
    }

    #[test]
    fn nested_slot_replacement_preserves_groups_and_removes_duplicate_surfaces() {
        let compact = BarSpec {
            elements: vec![Element::Group(GroupSpec::new(
                "profile-group",
                vec![
                    GroupElement::Group(GroupSpec::new(
                        "slot-wrapper",
                        vec![GroupElement::Item(ItemSpec::new("source", 40, 80, 120))],
                    )),
                    GroupElement::Item(ItemSpec::new("status", 40, 80, 120)),
                ],
            ))],
            principal_item: Some("source".into()),
        };
        let slots = [ComposedSlot {
            id: "nested".into(),
            path: vec![0, 0],
            elements: 0..1,
        }];
        let content = BarSpec {
            elements: vec![
                Element::Item(ItemSpec::new("source", 80, 120, 160)),
                Element::Item(ItemSpec::new("status", 60, 80, 100)),
            ],
            principal_item: Some("status".into()),
        };

        let bar = slot_content_bar(&compact, &slots, "nested", content).unwrap();
        let Element::Group(root) = &bar.elements[0] else {
            panic!("profile group was not preserved");
        };
        assert_eq!(root.elements.len(), 1);
        let GroupElement::Group(slot) = &root.elements[0] else {
            panic!("slot wrapper was not preserved");
        };
        assert_eq!(slot.elements.len(), 2);
        assert_eq!(item_ids(&bar).len(), 2);
        assert_eq!(bar.principal_item, Some("status".into()));
        assert!(resolve(&bar, 240, &BTreeSet::new()).is_ok());
    }

    #[test]
    fn in_place_expansion_finds_a_source_nested_inside_a_group() {
        let compact = BarSpec {
            elements: vec![Element::Group(GroupSpec::new(
                "controls",
                vec![GroupElement::Item(ItemSpec::new("source", 40, 80, 200))],
            ))],
            principal_item: Some("source".into()),
        };

        let bar =
            in_place_bar(&compact, "source", ItemSpec::new("ignored", 100, 160, 220)).unwrap();
        let layout = resolve(&bar, 160, &BTreeSet::new()).unwrap();
        assert_eq!(layout.placements[0].id.as_str(), "source");
        assert_eq!(layout.placements[0].width, 160);
    }

    #[test]
    fn full_bar_is_explicit_and_exact() {
        let bar = full_bar("source", 2008);
        assert_eq!(items(&bar), [("source", 2008)]);
        let placement = resolve(&bar, 2008, &BTreeSet::new()).unwrap();
        assert_eq!(
            (placement.placements[0].x, placement.placements[0].width),
            (0, 2008)
        );
    }
}
