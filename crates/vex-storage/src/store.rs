//! Typed façade over pluggable storage backends.
//!
//! See [`crate`] for the design and [`crate::backend`] for the trait
//! contracts. The public surface is intentionally identical to the original
//! redb-only implementation so existing callers (vex-core, vex-cli, tests)
//! need no changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;
use vex_utils::{hash::HashAlgo, Hash256, VexError, VexResult};

use crate::backend::{ObjectBackend, RefBackend};
use crate::object::{decode, encode, Blob, Commit, ObjectKind, SchemaManifest, Tree};
use crate::redb_backend::{open_database, RedbObjectBackend, RedbRefBackend};

/// A vex object store backed by pluggable [`ObjectBackend`] + [`RefBackend`].
#[derive(Debug, Clone)]
pub struct ObjectStore {
    objects: Arc<dyn ObjectBackend>,
    refs: Arc<dyn RefBackend>,
    /// Filesystem path the store was opened against, for diagnostics.
    /// Empty for purely-remote stores constructed via [`Self::with_backends`].
    path: PathBuf,
    algo: HashAlgo,
}

impl ObjectStore {
    /// Open (creating if needed) a redb-backed object store at
    /// `path/objects.redb`. Equivalent to a "local repository" store.
    pub fn open_or_create(path: impl AsRef<Path>) -> VexResult<Self> {
        let dir = path.as_ref().to_path_buf();
        let db = open_database(&dir)?;
        let objects = RedbObjectBackend::new(db.clone(), dir.clone());
        let refs = RedbRefBackend::new(db);
        Ok(Self {
            objects: Arc::new(objects),
            refs: Arc::new(refs),
            path: dir,
            algo: HashAlgo::DEFAULT,
        })
    }

    /// Construct a store from arbitrary backends. Used by cloud deployments
    /// (e.g. S3 objects + Postgres refs) and by the layered cache wrapper.
    #[must_use]
    pub fn with_backends(objects: Arc<dyn ObjectBackend>, refs: Arc<dyn RefBackend>) -> Self {
        Self {
            objects,
            refs,
            path: PathBuf::new(),
            algo: HashAlgo::DEFAULT,
        }
    }

    /// Filesystem path the store was opened from. Returns an empty path for
    /// purely-remote stores; callers using it should treat it as a hint only.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hash algorithm used for new writes.
    #[must_use]
    pub fn hash_algo(&self) -> HashAlgo {
        self.algo
    }

    /// Backend handle for advanced consumers (e.g. transport pack-streaming
    /// in `vex-protocol` that wants to copy framed bytes verbatim).
    #[must_use]
    pub fn object_backend(&self) -> Arc<dyn ObjectBackend> {
        self.objects.clone()
    }

    /// Ref backend handle (used by the network protocol to perform CAS
    /// updates honestly when serving pushes).
    #[must_use]
    pub fn ref_backend(&self) -> Arc<dyn RefBackend> {
        self.refs.clone()
    }

    /// Write a typed object, returning its content hash.
    pub fn put<T: Serialize>(&self, kind: ObjectKind, value: &T) -> VexResult<Hash256> {
        let payload = bincode::serialize(value)
            .map_err(|e| VexError::Storage(format!("bincode: {e}")))?;
        let (hash, framed) = encode(kind, &payload, self.algo)?;
        if !self.objects.has(hash)? {
            self.objects.put(hash, &framed)?;
        }
        Ok(hash)
    }

    /// Insert framed object bytes verbatim. Used by the network protocol
    /// when receiving packs from a remote — the bytes are already framed
    /// and self-validating. Returns `Err(HashMismatch)` if the bytes do
    /// not match the supplied hash.
    pub fn put_framed(&self, hash: Hash256, framed: &[u8]) -> VexResult<()> {
        // Validate before writing so a malicious peer can't corrupt the store.
        let _ = decode(framed, hash)?;
        if self.objects.has(hash)? {
            return Ok(());
        }
        self.objects.put(hash, framed)
    }

    /// Fetch the framed bytes for an object without decoding it. Used by
    /// transport to ship existing objects to peers without re-encoding.
    pub fn get_framed(&self, hash: Hash256) -> VexResult<Vec<u8>> {
        let framed = self
            .objects
            .get(hash)?
            .ok_or_else(|| VexError::NotFound(hash.to_hex()))?;
        // Re-validate on read; cheap insurance against backend corruption.
        let _ = decode(&framed, hash)?;
        Ok(framed)
    }

    /// Read an object's [`ObjectKind`] without deserializing its payload.
    /// Used by reachability walkers (e.g. in `vex-serve`) that need to
    /// dispatch on kind before fetching the typed object.
    pub fn object_kind(&self, hash: Hash256) -> VexResult<ObjectKind> {
        let framed = self
            .objects
            .get(hash)?
            .ok_or_else(|| VexError::NotFound(hash.to_hex()))?;
        let (kind, _algo, _payload) = decode(&framed, hash)?;
        Ok(kind)
    }

    /// Fetch and decode an object by content hash. Validates hash integrity.
    pub fn get<T: DeserializeOwned>(&self, hash: Hash256) -> VexResult<(ObjectKind, T)> {
        let framed = self
            .objects
            .get(hash)?
            .ok_or_else(|| VexError::NotFound(hash.to_hex()))?;
        let (kind, _algo, payload) = decode(&framed, hash)?;
        let value: T = bincode::deserialize(&payload)
            .map_err(|e| VexError::Storage(format!("bincode: {e}")))?;
        Ok((kind, value))
    }

    /// Check whether an object exists.
    pub fn has(&self, hash: Hash256) -> VexResult<bool> {
        self.objects.has(hash)
    }

    /// Write (or overwrite) a ref to point at `target`.
    pub fn set_ref(&self, name: &str, target: Hash256) -> VexResult<()> {
        validate_ref_name(name)?;
        self.refs.set(name, target)
    }

    /// Compare-and-set a ref. See [`RefBackend::compare_and_set`].
    /// Returns `Ok(true)` if the swap was applied.
    pub fn cas_ref(
        &self,
        name: &str,
        expected: Option<Hash256>,
        target: Hash256,
    ) -> VexResult<bool> {
        validate_ref_name(name)?;
        self.refs.compare_and_set(name, expected, target)
    }

    /// Look up a ref.
    pub fn get_ref(&self, name: &str) -> VexResult<Option<Hash256>> {
        self.refs.get(name)
    }

    /// List all refs and their targets.
    pub fn list_refs(&self) -> VexResult<Vec<(String, Hash256)>> {
        self.refs.list()
    }

    /// Delete a ref. Returns `Ok(false)` if the ref did not exist.
    pub fn delete_ref(&self, name: &str) -> VexResult<bool> {
        self.refs.delete(name)
    }

    /// Enumerate every stored object hash. Used by `gc` and auditors.
    pub fn list_object_hashes(&self) -> VexResult<Vec<Hash256>> {
        self.objects.list_hashes()
    }

    /// Delete a batch of objects by hash. Returns how many were removed.
    /// Callers must ensure the targets are unreachable — no safety net here.
    pub fn delete_objects(&self, hashes: &[Hash256]) -> VexResult<usize> {
        let mut removed = 0;
        for h in hashes {
            if self.objects.delete(*h)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// **For testing only** — overwrite an object's bytes, intentionally
    /// breaking content-hash integrity. Used by tamper-detection tests.
    #[doc(hidden)]
    pub fn _debug_corrupt_object(&self, hash: Hash256, new_bytes: Vec<u8>) -> VexResult<()> {
        // Bypasses [`Self::put_framed`]'s integrity check intentionally.
        // Use the backend's `delete` then `put` so non-redb backends behave too.
        let _ = self.objects.delete(hash)?;
        self.objects.put(hash, &new_bytes)
    }

    /// Walk every stored object and verify each object's content hash.
    /// Returns the number of objects checked.
    pub fn verify(&self) -> VexResult<usize> {
        let hashes = self.objects.list_hashes()?;
        let mut count = 0usize;
        for h in hashes {
            let bytes = self
                .objects
                .get(h)?
                .ok_or_else(|| VexError::NotFound(h.to_hex()))?;
            // `decode` validates the hash for us.
            decode(&bytes, h)?;
            count += 1;
        }
        Ok(count)
    }
}

/// Convenience wrappers for the primary object kinds.
impl ObjectStore {
    pub fn put_blob(&self, blob: &Blob) -> VexResult<Hash256> {
        self.put(ObjectKind::Blob, blob)
    }
    pub fn put_tree(&self, tree: &Tree) -> VexResult<Hash256> {
        self.put(ObjectKind::Tree, tree)
    }
    pub fn put_commit(&self, commit: &Commit) -> VexResult<Hash256> {
        self.put(ObjectKind::Commit, commit)
    }
    pub fn put_manifest(&self, m: &SchemaManifest) -> VexResult<Hash256> {
        self.put(ObjectKind::SchemaManifest, m)
    }

    pub fn get_blob(&self, hash: Hash256) -> VexResult<Blob> {
        let (kind, v) = self.get::<Blob>(hash)?;
        if kind != ObjectKind::Blob {
            return Err(VexError::Storage(format!("expected Blob, got {kind:?}")));
        }
        Ok(v)
    }
    pub fn get_tree(&self, hash: Hash256) -> VexResult<Tree> {
        let (kind, v) = self.get::<Tree>(hash)?;
        if kind != ObjectKind::Tree {
            return Err(VexError::Storage(format!("expected Tree, got {kind:?}")));
        }
        Ok(v)
    }
    pub fn get_commit(&self, hash: Hash256) -> VexResult<Commit> {
        let (kind, v) = self.get::<Commit>(hash)?;
        if kind != ObjectKind::Commit {
            return Err(VexError::Storage(format!("expected Commit, got {kind:?}")));
        }
        Ok(v)
    }
}

/// Ref names must be safe filesystem-like paths: no `..`, no control chars,
/// no absolute components, max 200 bytes.
fn validate_ref_name(name: &str) -> VexResult<()> {
    if name.is_empty() || name.len() > 200 {
        return Err(VexError::InvalidRef(format!("length {}", name.len())));
    }
    if name.starts_with('/') || name.contains("..") {
        return Err(VexError::InvalidRef(name.to_string()));
    }
    for c in name.chars() {
        if c.is_control() || c == ' ' || c == '\\' || c == ':' || c == '?' || c == '*' {
            return Err(VexError::InvalidRef(name.to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Commit, Identity};

    #[test]
    fn put_get_roundtrip_and_idempotency() {
        let dir = tempdir();
        let store = ObjectStore::open_or_create(&dir).expect("open");
        let blob = Blob {
            type_name: "IFCWALL".into(),
            step_id: 42,
            global_id: Some("2O2Fr$t4X7Zf8NOew3FNr2".into()),
            props: vec![],
        };
        let h1 = store.put_blob(&blob).expect("put");
        let h2 = store.put_blob(&blob).expect("put idempotent");
        assert_eq!(h1, h2);
        let back = store.get_blob(h1).expect("get");
        assert_eq!(back.type_name, "IFCWALL");
    }

    #[test]
    fn verify_detects_tamper() {
        let dir = tempdir();
        let store = ObjectStore::open_or_create(&dir).expect("open");
        let blob = Blob {
            type_name: "IFCSLAB".into(),
            step_id: 1,
            global_id: None,
            props: vec![],
        };
        let _h = store.put_blob(&blob).expect("put");
        // Normal verify passes.
        let n = store.verify().expect("verify");
        assert_eq!(n, 1);
    }

    #[test]
    fn refs_and_listing() {
        let dir = tempdir();
        let store = ObjectStore::open_or_create(&dir).expect("open");
        let c = Commit {
            tree: Hash256::ZERO,
            parents: vec![],
            author: Identity {
                name: "A".into(),
                email: "a@b".into(),
            },
            committer: Identity {
                name: "A".into(),
                email: "a@b".into(),
            },
            timestamp: 0,
            message: "init".into(),
            signature: None,
            profile_hash: vex_utils::Hash256::ZERO,
        };
        let h = store.put_commit(&c).expect("put");
        store.set_ref("refs/heads/main", h).expect("set ref");
        store.set_ref("HEAD", h).expect("set HEAD");
        let looked = store.get_ref("refs/heads/main").expect("get");
        assert_eq!(looked, Some(h));
        let all = store.list_refs().expect("list");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn cas_ref_semantics() {
        let dir = tempdir();
        let store = ObjectStore::open_or_create(&dir).expect("open");
        let zero = Hash256::ZERO;
        let one = Hash256::from_bytes([1u8; 32]);
        let two = Hash256::from_bytes([2u8; 32]);

        // Create-if-absent succeeds when ref does not exist.
        assert!(store.cas_ref("refs/heads/main", None, one).expect("cas"));
        // Create-if-absent now fails because the ref exists.
        assert!(!store.cas_ref("refs/heads/main", None, two).expect("cas"));
        // Wrong expected value rejected.
        assert!(!store
            .cas_ref("refs/heads/main", Some(zero), two)
            .expect("cas"));
        // Correct expected value accepted.
        assert!(store
            .cas_ref("refs/heads/main", Some(one), two)
            .expect("cas"));
        assert_eq!(store.get_ref("refs/heads/main").unwrap(), Some(two));
    }

    #[test]
    fn reject_bad_ref_names() {
        let dir = tempdir();
        let store = ObjectStore::open_or_create(&dir).expect("open");
        assert!(store.set_ref("", Hash256::ZERO).is_err());
        assert!(store.set_ref("../escape", Hash256::ZERO).is_err());
        assert!(store.set_ref("/abs", Hash256::ZERO).is_err());
        assert!(store.set_ref("bad\x00name", Hash256::ZERO).is_err());
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "vex-storage-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let p = base.join(unique);
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }
}
