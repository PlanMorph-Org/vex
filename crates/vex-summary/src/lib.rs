//! Render a [`VisualDiff`] into a one-paragraph human summary.
//!
//! Example:
//!
//! ```text
//! 3 walls moved, 2 doors added, 1 column added
//! ```
//!
//! Conventions:
//! - Type names are stripped of the `Ifc`/`IFC` prefix and lower-cased.
//! - Counts are pluralized with a naive `s` suffix (good enough for IFC type
//!   names: `wall`s, `door`s, `column`s — no irregulars in the schema).
//! - Buckets are sorted by count descending, then alphabetically by label,
//!   so output is stable.

use std::collections::BTreeMap;

use vex_visual_diff::{ChangeKind, ElementChange, VisualDiff};

/// Render a one-paragraph summary of `diff`. Returns `"No changes."` when
/// nothing changed.
#[must_use]
pub fn render(diff: &VisualDiff) -> String {
    render_changes(&diff.elements)
}

/// Same as [`render`] but takes the element list directly — useful when the
/// caller doesn't yet have a `VisualDiff` envelope.
#[must_use]
pub fn render_changes(elements: &[ElementChange]) -> String {
    if elements.is_empty() {
        return "No changes.".to_string();
    }
    let mut buckets: BTreeMap<(String, ChangeKind), u32> = BTreeMap::new();
    for e in elements {
        let label = friendly_type(&e.type_name);
        *buckets.entry((label, e.kind)).or_insert(0) += 1;
    }
    let mut entries: Vec<(String, ChangeKind, u32)> = buckets
        .into_iter()
        .map(|((label, kind), n)| (label, kind, n))
        .collect();
    // Highest impact first, then stable alpha order.
    entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

    let parts: Vec<String> = entries
        .into_iter()
        .map(|(label, kind, n)| format!("{n} {} {}", pluralize(&label, n), verb(kind)))
        .collect();
    parts.join(", ")
}

fn verb(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Moved => "moved",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Modified => "modified",
    }
}

fn pluralize(label: &str, n: u32) -> String {
    if n == 1 {
        label.to_string()
    } else {
        format!("{label}s")
    }
}

/// Strip the `Ifc`/`IFC` prefix and lower-case. `IFCWALL` → `wall`.
/// Falls back to the original (lower-cased) string if no prefix present.
fn friendly_type(type_name: &str) -> String {
    let lower = type_name.to_ascii_lowercase();
    lower
        .strip_prefix("ifc")
        .map(str::to_string)
        .unwrap_or(lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_visual_diff::{ChangeKind, Counts, ElementChange, Identity, VisualDiff};

    fn elt(type_name: &str, kind: ChangeKind) -> ElementChange {
        ElementChange {
            id: Identity::GlobalId("X".into()),
            type_name: type_name.into(),
            kind,
            deltas: Vec::new(),
            hint: None,
            layer: None,
        }
    }

    #[test]
    fn empty_returns_no_changes() {
        let v = VisualDiff {
            schema: vex_visual_diff::SCHEMA.to_string(),
            from: "a".into(),
            to: "b".into(),
            elements: Vec::new(),
            summary: String::new(),
            counts: Counts::default(),
        };
        assert_eq!(render(&v), "No changes.");
    }

    #[test]
    fn pluralizes_and_strips_ifc_prefix() {
        let elements = vec![
            elt("IFCWALL", ChangeKind::Moved),
            elt("IFCWALL", ChangeKind::Moved),
            elt("IFCWALL", ChangeKind::Moved),
            elt("IFCDOOR", ChangeKind::Added),
            elt("IFCCOLUMN", ChangeKind::Added),
        ];
        // 3 walls (plural) > 1 door + 1 column (singular each).
        let s = render_changes(&elements);
        assert_eq!(s, "3 walls moved, 1 column added, 1 door added");
    }

    #[test]
    fn singular_when_count_is_one() {
        let elements = vec![elt("IFCSLAB", ChangeKind::Renamed)];
        assert_eq!(render_changes(&elements), "1 slab renamed");
    }

    #[test]
    fn unknown_prefix_lowercased_only() {
        let elements = vec![elt("CustomType", ChangeKind::Modified)];
        assert_eq!(render_changes(&elements), "1 customtype modified");
    }
}
