//! Shared primitives for the Vex workspace.
//!
//! This crate intentionally has zero I/O and no external service dependencies.
//! Everything here is pure, deterministic, and safe to call from any layer.

pub mod error;
pub mod hash;
pub mod interner;
pub mod profile;
pub mod tolerance;

pub use error::{VexError, VexResult};
pub use hash::{Hash256, HashAlgo, Hasher};
pub use interner::{StringId, StringInterner};
pub use profile::Profile;
pub use tolerance::Tolerance;
