//! Normalization profile.
//!
//! A [`Profile`] captures project-specific decisions about what counts as a
//! "semantically meaningful" change: which entity types to ignore entirely
//! (e.g. `IfcOwnerHistory` — pure export noise), which property keys to
//! ignore, and the floating-point tolerance used during canonical hashing.
//!
//! Profiles are versioned by their own hash. Each commit records the profile
//! hash it was authored under, so future diffs can detect when the two sides
//! used incompatible normalization rules.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::hash::hash_default;
use crate::{Hash256, Tolerance};

/// A canonicalization / hashing profile.
///
/// Fields are stable wire format — do not reorder. `BTreeSet` is used so
/// `bincode` serialization is deterministic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Floating-point tolerance used when hashing `Real` values.
    #[serde(default)]
    pub tolerance_linear: f64,
    #[serde(default)]
    pub tolerance_angular: f64,
    /// Entity types to drop from the graph entirely, uppercase.
    #[serde(default)]
    pub ignore_types: BTreeSet<String>,
    /// Property keys to drop before hashing, case-sensitive. For STEP positional
    /// args these are strings like `"_3"` (slot 3). Free-form keys become
    /// meaningful once we add `IfcPropertySet` lifting in Phase 3.
    #[serde(default)]
    pub ignore_prop_keys: BTreeSet<String>,
}

impl Default for Profile {
    fn default() -> Self {
        let t = Tolerance::default();
        let mut ignore_types = BTreeSet::new();
        // IfcOwnerHistory carries last-modified timestamps, change actions, and
        // application version strings — pure noise across re-exports.
        ignore_types.insert("IFCOWNERHISTORY".to_string());
        Self {
            tolerance_linear: t.linear,
            tolerance_angular: t.angular,
            ignore_types,
            ignore_prop_keys: BTreeSet::new(),
        }
    }
}

impl Profile {
    #[must_use]
    pub fn tolerance(&self) -> Tolerance {
        Tolerance::new(self.tolerance_linear, self.tolerance_angular)
    }

    /// Does this profile want us to drop the given entity type? The input is
    /// compared case-insensitively.
    #[must_use]
    pub fn ignores_type(&self, type_name: &str) -> bool {
        // BTreeSet is sorted, so we do a case-folded linear scan. In practice
        // the ignore list is tiny (< 10 entries), so this is fine.
        self.ignore_types
            .iter()
            .any(|t| t.eq_ignore_ascii_case(type_name))
    }

    /// Does this profile want us to drop the given property key?
    #[must_use]
    pub fn ignores_prop(&self, key: &str) -> bool {
        self.ignore_prop_keys.contains(key)
    }

    /// Stable hash of the profile. Recorded per commit.
    ///
    /// Uses `bincode` — same as every other persisted Vex object — so two
    /// profiles that deserialize to equal structs always hash equal.
    #[must_use]
    pub fn hash(&self) -> Hash256 {
        // `bincode::serialize` on `BTreeSet` is deterministic (in-order).
        match bincode::serialize(self) {
            Ok(bytes) => hash_default(&bytes),
            // Serialization of a `Profile` cannot fail with bincode, so fall
            // back to a hash of the debug repr if the impossible happens.
            Err(_) => hash_default(format!("{self:?}").as_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ignores_owner_history() {
        let p = Profile::default();
        assert!(p.ignores_type("IfcOwnerHistory"));
        assert!(p.ignores_type("IFCOWNERHISTORY"));
        assert!(!p.ignores_type("IfcWall"));
    }

    #[test]
    fn hash_is_deterministic_and_sensitive() {
        let a = Profile::default();
        let b = Profile::default();
        assert_eq!(a.hash(), b.hash());

        let mut c = Profile::default();
        c.ignore_types.insert("IFCWALL".into());
        assert_ne!(a.hash(), c.hash());
    }
}
