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

/// Schema family name (the part before the `/<major>` suffix). Stable across
/// compatible revisions.
pub const SCHEMA_NAME: &str = "vex.visual-diff";

/// Current major version of [`SCHEMA`]. Consumers that understand this major
/// can deserialize any payload tagged with the same name + major.
pub const SCHEMA_MAJOR: u32 = 1;

/// Why a schema tag was rejected. Surfaced at trust boundaries (the bridge,
/// the cloud ingest, the viewer) so a producer/consumer version skew fails
/// loudly instead of silently mis-rendering a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// The tag did not match the `name/<major>` shape at all.
    Malformed(String),
    /// The tag is well-formed but for a different schema family or an
    /// incompatible major version.
    Incompatible { found: String, expected: String },
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::Malformed(tag) => {
                write!(
                    f,
                    "malformed visual-diff schema tag `{tag}` (expected `name/<major>`)"
                )
            }
            SchemaError::Incompatible { found, expected } => write!(
                f,
                "incompatible visual-diff schema `{found}` (this build expects `{expected}`)"
            ),
        }
    }
}

impl std::error::Error for SchemaError {}

/// Split a schema tag like `vex.visual-diff/1` into its `(name, major)` parts.
/// Returns `None` if the tag does not have exactly one `/` separating a
/// non-empty name from a numeric major.
pub fn parse_schema(tag: &str) -> Option<(&str, u32)> {
    let (name, major) = tag.rsplit_once('/')?;
    if name.is_empty() {
        return None;
    }
    Some((name, major.parse().ok()?))
}

/// Whether `tag` names the same schema family and major version this build
/// produces, and is therefore safe to deserialize into [`VisualDiff`].
pub fn is_compatible(tag: &str) -> bool {
    matches!(parse_schema(tag), Some((name, major)) if name == SCHEMA_NAME && major == SCHEMA_MAJOR)
}

/// Strictly validate a schema tag, returning a descriptive error instead of
/// silently accepting unknown versions (which the serde default does for the
/// *absent*-field case only). Call this at any boundary that ingests a
/// `VisualDiff` from another process.
pub fn validate_schema(tag: &str) -> Result<(), SchemaError> {
    match parse_schema(tag) {
        None => Err(SchemaError::Malformed(tag.to_string())),
        Some((name, major)) if name == SCHEMA_NAME && major == SCHEMA_MAJOR => Ok(()),
        Some(_) => Err(SchemaError::Incompatible {
            found: tag.to_string(),
            expected: SCHEMA.to_string(),
        }),
    }
}

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

impl VisualDiff {
    /// Verify this payload's `schema` tag is compatible with the version this
    /// build understands. Producers can assert before emitting; consumers can
    /// assert after deserializing.
    pub fn validate_schema(&self) -> Result<(), SchemaError> {
        validate_schema(&self.schema)
    }
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

    #[test]
    fn classify_stamps_current_schema_and_validates() {
        let r = DiffReport {
            changes: vec![],
            summary: DiffSummary {
                added: 0,
                removed: 0,
                modified: 0,
            },
        };
        let v = classify(&r, "a", "b");
        assert_eq!(v.schema, SCHEMA);
        assert!(v.validate_schema().is_ok());
    }

    #[test]
    fn parse_schema_splits_name_and_major() {
        assert_eq!(
            parse_schema("vex.visual-diff/1"),
            Some(("vex.visual-diff", 1))
        );
        assert_eq!(
            parse_schema("vex.visual-diff/2"),
            Some(("vex.visual-diff", 2))
        );
        assert_eq!(parse_schema("no-major"), None);
        assert_eq!(parse_schema("/1"), None);
        assert_eq!(parse_schema("name/x"), None);
    }

    #[test]
    fn compat_and_validation_reject_skew() {
        assert!(is_compatible(SCHEMA));
        assert!(!is_compatible("vex.visual-diff/2"));
        assert!(!is_compatible("other.schema/1"));

        assert_eq!(validate_schema(SCHEMA), Ok(()));
        assert!(matches!(
            validate_schema("vex.visual-diff/2"),
            Err(SchemaError::Incompatible { .. })
        ));
        assert!(matches!(
            validate_schema("garbage"),
            Err(SchemaError::Malformed(_))
        ));
    }

    #[test]
    fn absent_schema_field_defaults_to_current() {
        // A legacy payload that predates the `schema` field still loads and
        // is treated as the current version (back-compat for stored diffs).
        let json = r#"{"from":"a","to":"b","elements":[],"summary":"","counts":{"added":0,"removed":0,"moved":0,"renamed":0,"modified":0}}"#;
        let v: VisualDiff = serde_json::from_str(json).expect("legacy payload loads");
        assert_eq!(v.schema, SCHEMA);
        assert!(v.validate_schema().is_ok());
    }

    #[test]
    fn golden_payload_round_trips_field_for_field() {
        // Golden contract sample. If a field name/shape changes without a
        // schema major bump, this fails — forcing an explicit version decision.
        let golden = r#"{
  "schema": "vex.visual-diff/1",
  "from": "rev-a",
  "to": "rev-b",
  "elements": [
    {
      "id": { "GlobalId": "1hWHpL2eHpg_GUuvaySq00" },
      "type_name": "IFCWALL",
      "kind": "renamed",
      "deltas": [
        { "key": "_2", "before": { "Text": "Wall-A" }, "after": { "Text": "Wall-B" } }
      ],
      "hint": "Name: Wall-A → Wall-B"
    }
  ],
  "summary": "1 wall renamed",
  "counts": { "added": 0, "removed": 0, "moved": 0, "renamed": 1, "modified": 0 }
}"#;
        let parsed: VisualDiff = serde_json::from_str(golden).expect("golden parses");
        assert!(parsed.validate_schema().is_ok());
        assert_eq!(parsed.from, "rev-a");
        assert_eq!(parsed.to, "rev-b");
        assert_eq!(parsed.counts.renamed, 1);
        assert_eq!(parsed.counts.total(), 1);
        assert_eq!(parsed.elements.len(), 1);
        assert_eq!(parsed.elements[0].type_name, "IFCWALL");
        assert_eq!(parsed.elements[0].kind, ChangeKind::Renamed);

        // Re-serializing and re-parsing must be stable (no field drift).
        let reserialized = serde_json::to_string(&parsed).expect("serializes");
        let reparsed: VisualDiff = serde_json::from_str(&reserialized).expect("round-trips");
        assert_eq!(reparsed.schema, parsed.schema);
        assert_eq!(reparsed.elements[0].hint, parsed.elements[0].hint);
    }
}
