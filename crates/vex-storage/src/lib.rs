//! Content-addressable object store for Vex.
//!
//! The store is the high-level façade vex-core consumes. It frames typed
//! objects with [`crate::object::encode`], dispatches the bytes to a
//! pluggable [`ObjectBackend`], and tracks named refs through a
//! [`RefBackend`].
//!
//! Two construction paths:
//!
//! - [`ObjectStore::open_or_create`] — single-file `redb` store on disk
//!   (default for local CLI workflows). Backwards-compatible: existing
//!   repositories continue to work without any migration.
//! - [`ObjectStore::with_backends`] — bring your own `ObjectBackend` +
//!   `RefBackend` (e.g. S3 + Postgres for the cloud deployment).
//!
//! Object integrity: every `get` validates the stored hash against the
//! recomputed hash, and `verify()` walks the entire store for scrubbing.

pub mod backend;
pub mod object;
pub mod redb_backend;
pub mod store;

#[cfg(feature = "s3-backend")]
pub mod s3_backend;

#[cfg(feature = "postgres-backend")]
pub mod postgres_backend;

pub use backend::{ObjectBackend, RefBackend};
pub use object::{
    Blob, Commit, Identity, ObjectKind, SchemaManifest, SerValue, Signature, Tree, TreeEdge,
    TreeEntry,
};
pub use store::ObjectStore;
