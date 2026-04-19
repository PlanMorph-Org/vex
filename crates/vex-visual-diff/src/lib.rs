//! Classify a [`vex_diff::DiffReport`] into per-element [`ElementChange`]
//! records suitable for BIM-tool overlays.
//!
//! Plugin hosts (Revit/Archicad/Tekla add-ins) and the future web viewer
//! consume [`VisualDiff`] verbatim; the JSON shape produced here is the
//! public contract.

use serde::{Deserialize, Serialize};

pub use vex_diff::{DiffReport, Identity, Layer, PropDelta, SerValue};

/// One classified element change. Stable across IFC re-export thanks to
/// [`Identity`] (`GlobalId` primary, structural-hash fallback).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementChange {
    pub id: Identity,
    /// IFC type name as it appears in the source — e.g. `IFCWALL`.
    pub type_name: String,
    pub kind: ChangeKind,
    /// Property-level deltas; empty for `Added`/`Removed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deltas: Vec<PropDelta>,
    /// One-line human explanation, e.g. `"Name: Wall-A → Wall-B"`. `None` for
    /// `Added`/`Removed` (the type + identity is enough).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// What happened to an element. `Moved` and `Renamed` are refinements of
/// `Modified` for common, easy-to-explain cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Moved,
    Renamed,
    Modified,
}

/// Aggregate counts. JSON `snake_case` field names are part of the public
/// schema.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Counts {
    pub added: u32,
    pub removed: u32,
    pub moved: u32,
    pub renamed: u32,
    pub modified: u32,
}

impl Counts {
    pub fn total(&self) -> u32 {
        self.added + self.removed + self.moved + self.renamed + self.modified
    }
}

/// Public schema identifier for [`VisualDiff`]. Bump on any
/// breaking change to field names, types, or semantics.
pub const SCHEMA: &str = "vex.visual-diff/1";

/// Default value used by serde when an older payload lacks the field.
fn default_schema() -> String {
    SCHEMA.to_string()
}

/// Top-level visual diff payload exposed to plugin hosts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualDiff {
    /// Schema version tag. Always [`SCHEMA`] for payloads produced by this
    /// crate; reserved so plugins can refuse unknown versions.
    #[serde(default = "default_schema")]
    pub schema: String,
    /// Source revision. May be a commit hash, ref name, or empty if not
    /// commit-bound (e.g. comparing two ad-hoc files).
    pub from: String,
    /// Target revision.
    pub to: String,
    pub elements: Vec<ElementChange>,
    /// Single-paragraph human summary, e.g.
    /// `"3 walls moved, 2 doors added, 1 column added"`.
    pub summary: String,
    pub counts: Counts,
}

/// Classify a [`DiffReport`] into a [`VisualDiff`] (without summary — the
/// caller fills [`VisualDiff::summary`] via `vex-summary`).
///
/// `from`/`to` are passed through verbatim into the result so callers can
/// stamp the payload with whatever revision identifiers make sense.
pub fn classify(report: &DiffReport, from: &str, to: &str) -> VisualDiff {
    let mut elements = Vec::with_capacity(report.changes.len());
    let mut counts = Counts::default();

    for change in &report.changes {
        match change {
            vex_diff::Change::Added {
                identity,
                type_name,
            } => {
                counts.added += 1;
                elements.push(ElementChange {
                    id: identity.clone(),
                    type_name: type_name.clone(),
                    kind: ChangeKind::Added,
                    deltas: Vec::new(),
                    hint: None,
                });
            }
            vex_diff::Change::Removed {
                identity,
                type_name,
            } => {
                counts.removed += 1;
                elements.push(ElementChange {
                    id: identity.clone(),
                    type_name: type_name.clone(),
                    kind: ChangeKind::Removed,
                    deltas: Vec::new(),
                    hint: None,
                });
            }
            vex_diff::Change::Modified {
                identity,
                type_name,
                deltas,
                layer,
            } => {
                let (kind, hint) = classify_modified(type_name, deltas, *layer);
                match kind {
                    ChangeKind::Moved => counts.moved += 1,
                    ChangeKind::Renamed => counts.renamed += 1,
                    _ => counts.modified += 1,
                }
                elements.push(ElementChange {
                    id: identity.clone(),
                    type_name: type_name.clone(),
                    kind,
                    deltas: deltas.clone(),
                    hint,
                });
            }
        }
    }

    VisualDiff {
        schema: SCHEMA.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        elements,
        summary: String::new(),
        counts,
    }
}

/// Decide whether a `Modified` change is really a `Moved`, a `Renamed`, or a
/// generic `Modified`, and produce a one-line hint when possible.
fn classify_modified(
    type_name: &str,
    deltas: &[PropDelta],
    _layer: Layer,
) -> (ChangeKind, Option<String>) {
    if deltas.is_empty() {
        return (ChangeKind::Modified, None);
    }
    let meaning_of = |key: &str| slot_meaning(type_name, key);
    let all_renames = deltas.iter().all(|d| {
        matches!(
            meaning_of(&d.key),
            Some(SlotMeaning::Name | SlotMeaning::Description | SlotMeaning::Tag)
        )
    });
    let all_moves = deltas
        .iter()
        .all(|d| matches!(meaning_of(&d.key), Some(SlotMeaning::Placement)));

    let kind = match (all_renames, all_moves) {
        (true, _) => ChangeKind::Renamed,
        (false, true) => ChangeKind::Moved,
        _ => ChangeKind::Modified,
    };
    let hint = first_hint(deltas, &meaning_of);
    (kind, hint)
}

fn first_hint(
    deltas: &[PropDelta],
    meaning_of: &dyn Fn(&str) -> Option<SlotMeaning>,
) -> Option<String> {
    let d = deltas.first()?;
    let label = match meaning_of(&d.key) {
        Some(m) => m.label(),
        None => d.key.as_str(),
    };
    Some(format!(
        "{label}: {} → {}",
        render_value(d.before.as_ref()),
        render_value(d.after.as_ref())
    ))
}

fn render_value(v: Option<&SerValue>) -> String {
    match v {
        None => "∅".to_string(),
        Some(SerValue::Null) => "$".to_string(),
        Some(SerValue::Bool(b)) => b.to_string(),
        Some(SerValue::Int(i)) => i.to_string(),
        Some(SerValue::Real(r)) => format!("{r}"),
        Some(SerValue::Text(s) | SerValue::Enum(s)) => s.clone(),
        Some(SerValue::List(_)) => "[…]".to_string(),
        Some(SerValue::Typed { name, .. }) => format!("{name}(…)"),
    }
}

/// Common slot-meaning derived from the IFC inheritance chain.
#[derive(Debug, Clone, Copy)]
enum SlotMeaning {
    Name,
    Description,
    Placement,
    Tag,
}

impl SlotMeaning {
    fn label(self) -> &'static str {
        match self {
            SlotMeaning::Name => "Name",
            SlotMeaning::Description => "Description",
            SlotMeaning::Placement => "Placement",
            SlotMeaning::Tag => "Tag",
        }
    }
}

/// Map a positional property key (e.g. `_2`) to its semantic role for a given
/// IFC type. Conservative — only the slots that are stable across the common
/// IfcRoot/IfcProduct/IfcElement subtypes.
fn slot_meaning(type_name: &str, key: &str) -> Option<SlotMeaning> {
    let upper = type_name.to_ascii_uppercase();
    if !upper.starts_with("IFC") {
        return None;
    }
    // Every IfcRoot subtype: _0 GlobalId, _1 OwnerHistory, _2 Name, _3 Description.
    match key {
        "_2" => return Some(SlotMeaning::Name),
        "_3" => return Some(SlotMeaning::Description),
        _ => {}
    }
    // IfcProduct subtypes (most building elements): _5 ObjectPlacement.
    if is_product_subtype(&upper) && key == "_5" {
        return Some(SlotMeaning::Placement);
    }
    // IfcElement subtypes: _7 Tag.
    if is_element_subtype(&upper) && key == "_7" {
        return Some(SlotMeaning::Tag);
    }
    None
}

fn is_product_subtype(upper: &str) -> bool {
    matches!(
        upper,
        "IFCWALL"
            | "IFCWALLSTANDARDCASE"
            | "IFCSLAB"
            | "IFCSLABELEMENTEDCASE"
            | "IFCSLABSTANDARDCASE"
            | "IFCBEAM"
            | "IFCBEAMSTANDARDCASE"
            | "IFCCOLUMN"
            | "IFCCOLUMNSTANDARDCASE"
            | "IFCDOOR"
            | "IFCDOORSTANDARDCASE"
            | "IFCWINDOW"
            | "IFCWINDOWSTANDARDCASE"
            | "IFCROOF"
            | "IFCSTAIR"
            | "IFCSTAIRFLIGHT"
            | "IFCRAILING"
            | "IFCRAMP"
            | "IFCRAMPFLIGHT"
            | "IFCCOVERING"
            | "IFCCURTAINWALL"
            | "IFCFOOTING"
            | "IFCPLATE"
            | "IFCPILE"
            | "IFCMEMBER"
            | "IFCBUILDINGELEMENTPROXY"
            | "IFCSPACE"
            | "IFCSITE"
            | "IFCBUILDING"
            | "IFCBUILDINGSTOREY"
            | "IFCFURNISHINGELEMENT"
            | "IFCFURNITURE"
    )
}

fn is_element_subtype(upper: &str) -> bool {
    // Same set in practice — every concrete building element above is also an
    // IfcElement subtype except IfcSpatialStructureElement (Site/Building/...).
    is_product_subtype(upper)
        && !matches!(
            upper,
            "IFCSPACE" | "IFCSITE" | "IFCBUILDING" | "IFCBUILDINGSTOREY"
        )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use vex_diff::{Change, DiffReport, DiffSummary, Identity, Layer, PropDelta, SerValue};

    fn modified(type_name: &str, key: &str, before: &str, after: &str) -> Change {
        Change::Modified {
            identity: Identity::GlobalId("X".into()),
            type_name: type_name.into(),
            deltas: vec![PropDelta {
                key: key.into(),
                before: Some(SerValue::Text(before.into())),
                after: Some(SerValue::Text(after.into())),
            }],
            layer: Layer::Property,
        }
    }

    #[test]
    fn renamed_when_only_name_changes() {
        let r = DiffReport {
            changes: vec![modified("IFCWALL", "_2", "Wall-A", "Wall-B")],
            summary: DiffSummary {
                added: 0,
                removed: 0,
                modified: 1,
            },
        };
        let v = classify(&r, "a", "b");
        assert_eq!(v.elements.len(), 1);
        assert_eq!(v.elements[0].kind, ChangeKind::Renamed);
        assert_eq!(v.counts.renamed, 1);
        assert!(v.elements[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("Wall-A → Wall-B"));
    }

    #[test]
    fn moved_when_only_placement_changes() {
        let r = DiffReport {
            changes: vec![modified("IFCWALL", "_5", "old", "new")],
            summary: DiffSummary {
                added: 0,
                removed: 0,
                modified: 1,
            },
        };
        let v = classify(&r, "a", "b");
        assert_eq!(v.elements[0].kind, ChangeKind::Moved);
        assert_eq!(v.counts.moved, 1);
    }

    #[test]
    fn modified_when_unknown_slot_or_mixed() {
        // Unknown slot _9 → generic Modified.
        let r = DiffReport {
            changes: vec![modified("IFCWALL", "_9", "x", "y")],
            summary: DiffSummary {
                added: 0,
                removed: 0,
                modified: 1,
            },
        };
        assert_eq!(
            classify(&r, "a", "b").elements[0].kind,
            ChangeKind::Modified
        );

        // Mixed Name + Placement → generic Modified (not all-renames, not
        // all-moves).
        let r2 = DiffReport {
            changes: vec![Change::Modified {
                identity: Identity::GlobalId("X".into()),
                type_name: "IFCWALL".into(),
                deltas: vec![
                    PropDelta {
                        key: "_2".into(),
                        before: Some(SerValue::Text("a".into())),
                        after: Some(SerValue::Text("b".into())),
                    },
                    PropDelta {
                        key: "_5".into(),
                        before: Some(SerValue::Text("old".into())),
                        after: Some(SerValue::Text("new".into())),
                    },
                ],
                layer: Layer::Property,
            }],
            summary: DiffSummary {
                added: 0,
                removed: 0,
                modified: 1,
            },
        };
        assert_eq!(
            classify(&r2, "a", "b").elements[0].kind,
            ChangeKind::Modified
        );
    }

    #[test]
    fn added_and_removed_passthrough() {
        let r = DiffReport {
            changes: vec![
                Change::Added {
                    identity: Identity::GlobalId("A".into()),
                    type_name: "IFCDOOR".into(),
                },
                Change::Removed {
                    identity: Identity::GlobalId("B".into()),
                    type_name: "IFCWINDOW".into(),
                },
            ],
            summary: DiffSummary {
                added: 1,
                removed: 1,
                modified: 0,
            },
        };
        let v = classify(&r, "a", "b");
        assert_eq!(v.counts.added, 1);
        assert_eq!(v.counts.removed, 1);
        assert!(v.elements[0].hint.is_none());
    }
}
