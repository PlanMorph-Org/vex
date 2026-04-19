//! Pluggable storage backends.
//!
//! `vex-storage` separates **what** is stored (content-addressed objects and
//! named refs) from **where** it lives. The default backend is a single
//! `redb` file on disk (`RedbObjectBackend` + `RedbRefBackend`). Cloud
//! deployments swap in `S3ObjectBackend` (any S3-compatible service such as
//! AWS S3, DigitalOcean Spaces, MinIO, Cloudflare R2) for objects and
//! `PostgresRefBackend` for refs (transactional CAS prevents push races).
//!
//! Backends are object-safe trait objects so a process can mix and match
//! (e.g. cached local + remote) at runtime. All operations are synchronous
//! to keep the existing API stable; remote backends use a small internal
//! Tokio runtime.

use std::fmt::Debug;

use vex_utils::{Hash256, VexResult};

/// Storage of opaque framed object bytes, keyed by content hash.
///
/// The bytes passed in/out are the *framed* on-disk representation produced
/// by [`crate::object::encode`] — backends are unaware of object kinds or
/// hashing; they are pure key/value blob stores.
pub trait ObjectBackend: Send + Sync + Debug {
    /// Insert a framed object. Idempotent on the content hash.
    fn put(&self, hash: Hash256, framed: &[u8]) -> VexResult<()>;

    /// Fetch the framed bytes for a hash. Returns `Ok(None)` if absent.
    fn get(&self, hash: Hash256) -> VexResult<Option<Vec<u8>>>;

    /// Existence check without copying bytes.
    fn has(&self, hash: Hash256) -> VexResult<bool>;

    /// Remove a single object. Returns whether anything was removed.
    /// Callers must ensure the object is unreachable from any ref.
    fn delete(&self, hash: Hash256) -> VexResult<bool>;

    /// Enumerate every stored hash. Order is unspecified.
    fn list_hashes(&self) -> VexResult<Vec<Hash256>>;
}

/// Storage of named refs (branches, tags, `HEAD`, staging refs).
///
/// Implementations MUST provide compare-and-set for `compare_and_set`; the
/// default `set` method overwrites unconditionally and is intended only for
/// trusted local writes.
pub trait RefBackend: Send + Sync + Debug {
    fn get(&self, name: &str) -> VexResult<Option<Hash256>>;

    /// Unconditional write — overwrites any prior value. Used by local
    /// commits and tests; remote pushes should always go through
    /// [`Self::compare_and_set`].
    fn set(&self, name: &str, target: Hash256) -> VexResult<()>;

    /// Atomic compare-and-set.
    ///
    /// - `expected = Some(h)` requires the ref currently equals `h`.
    /// - `expected = None` requires the ref does not currently exist.
    ///
    /// Returns `Ok(true)` if the swap was applied, `Ok(false)` if the
    /// precondition failed.
    fn compare_and_set(
        &self,
        name: &str,
        expected: Option<Hash256>,
        target: Hash256,
    ) -> VexResult<bool>;

    fn delete(&self, name: &str) -> VexResult<bool>;

    fn list(&self) -> VexResult<Vec<(String, Hash256)>>;
}
