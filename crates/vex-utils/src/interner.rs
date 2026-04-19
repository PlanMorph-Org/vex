//! Thread-safe string interning.
//!
//! IFC models contain massive duplication in type names and property keys
//! (the same ~900 IFC4x3 type strings appear millions of times). Interning
//! cuts graph memory by >10x and makes equality checks O(1) pointer compares.

use std::sync::Arc;

use lasso::{Spur, ThreadedRodeo};

/// Opaque interned string identifier. Cheap to copy and compare.
pub type StringId = Spur;

/// Thread-safe string interner backed by `lasso::ThreadedRodeo`.
///
/// Cloning is cheap (Arc). All handles share the same underlying table.
#[derive(Clone, Debug, Default)]
pub struct StringInterner {
    inner: Arc<ThreadedRodeo>,
}

impl StringInterner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ThreadedRodeo::default()),
        }
    }

    pub fn intern(&self, s: &str) -> StringId {
        self.inner.get_or_intern(s)
    }

    #[must_use]
    pub fn resolve(&self, id: StringId) -> &str {
        self.inner.resolve(&id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_deduplicates() {
        let i = StringInterner::new();
        let a = i.intern("IFCWALL");
        let b = i.intern("IFCWALL");
        let c = i.intern("IFCSLAB");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(i.resolve(a), "IFCWALL");
        assert_eq!(i.len(), 2);
    }
}
