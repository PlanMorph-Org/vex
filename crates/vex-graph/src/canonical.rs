//! Canonicalization utilities.
//!
//! This module is currently light — most canonicalization happens implicitly in
//! [`crate::merkle`] (sort properties, quantize floats, sort edges by kind+slot).
//! Further canonicalization (local-placement flattening, header stripping)
//! lands here as Phase 2 work.

use crate::ir::IfcGraph;

/// Run all canonicalization passes that mutate the graph in-place.
///
/// MVP is a no-op: the builder already interns strings and the hasher already
/// sorts for canonical order. The function exists so the API is stable for
/// callers as we add more passes.
pub fn canonicalize(_graph: &mut IfcGraph) {
    // Intentionally empty in MVP.
}
