//! Hard safety limits for the parser.
//!
//! IFC files are untrusted input. Without limits, a crafted file can trivially
//! exhaust memory or stack via deeply nested lists, extreme reference ids, or
//! massive entity counts. These caps are generous for real models (tested
//! against airport-scale federations) but bound the worst case.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseLimits {
    /// Maximum number of top-level entities in the DATA section.
    pub max_entities: u64,
    /// Maximum nested list depth inside a single entity's argument list.
    pub max_list_depth: u32,
    /// Maximum length (bytes) of a single logical STEP statement.
    pub max_statement_bytes: usize,
    /// Maximum string literal length after unescaping.
    pub max_string_bytes: usize,
    /// Maximum entity reference id (the `#N` integer).
    pub max_entity_id: u64,
    /// Total input size cap (bytes). `None` means unlimited (caller-chosen).
    pub max_input_bytes: Option<u64>,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_entities: 50_000_000,
            max_list_depth: 64,
            max_statement_bytes: 4 * 1024 * 1024,
            max_string_bytes: 1 * 1024 * 1024,
            max_entity_id: u32::MAX as u64,
            max_input_bytes: None,
        }
    }
}
