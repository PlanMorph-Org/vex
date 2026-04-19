//! On-disk object model.
//!
//! Every object is serialized with a fixed header:
//!
//! ```text
//! [ magic: 4 bytes = b"VEX" ]
//! [ version: u8 = 1          ]
//! [ kind:    u8              ]  // ObjectKind
//! [ algo:    u8              ]  // HashAlgo
//! [ reserved: u8 = 0         ]
//! [ payload: variable        ]
//! ```
//!
//! The payload is zstd-compressed bincode. The object's *content hash* is
//! computed over `kind || algo || uncompressed payload` — **not** over the
//! compressed bytes, so changing compression parameters doesn't change
//! identity.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use vex_utils::{hash::HashAlgo, Hash256, Hasher, Profile, VexError, VexResult};

pub(crate) const MAGIC: &[u8; 4] = b"VEX0";
pub(crate) const VERSION: u8 = 1;

/// Object kinds stored in the content-addressable store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ObjectKind {
    /// A single graph node (one IFC entity) snapshot.
    Blob = 1,
    /// A whole graph snapshot (entries + edges, referencing Blobs by hash).
    Tree = 2,
    /// A commit pointing at a Tree, with parents + metadata + signature.
    Commit = 3,
    /// An annotated tag.
    Tag = 4,
    /// Per-repo schema + normalization profile manifest.
    SchemaManifest = 5,
}

impl ObjectKind {
    pub(crate) fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Blob,
            2 => Self::Tree,
            3 => Self::Commit,
            4 => Self::Tag,
            5 => Self::SchemaManifest,
            _ => return None,
        })
    }
}

/// A serialized graph-node blob. The specific fields mirror `vex_graph::Node`
/// but are fully self-contained (string literals not interned) so blobs are
/// portable across runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    pub type_name: String,
    pub step_id: u64,
    pub global_id: Option<String>,
    pub props: Vec<(String, SerValue)>,
}

/// Portable, self-contained property value. Mirrors `vex_graph::ir::Value`
/// but without any interner handles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SerValue {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
    Enum(String),
    List(Vec<SerValue>),
    Typed { name: String, inner: Box<SerValue> },
}

/// Tree object: sorted list of entries + edges, forming a graph snapshot.
///
/// Entries are keyed by *canonical node hash* (from `vex_graph::merkle`) which
/// gives automatic dedup across commits and branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub schema: Option<String>,
    pub entries: Vec<TreeEntry>,
    pub edges: Vec<TreeEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    /// Canonical Merkle hash of the node.
    pub node_hash: Hash256,
    /// Hash of the Blob storing this node's content.
    pub blob_hash: Hash256,
    /// Original GlobalId if the node carries one. Enables fast GlobalId lookup.
    pub global_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEdge {
    pub from: Hash256,
    pub to: Hash256,
    pub kind: u8,
    pub slot: u16,
    pub list_index: u16,
}

/// Commit object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub tree: Hash256,
    pub parents: Vec<Hash256>,
    pub author: Identity,
    pub committer: Identity,
    /// Unix timestamp (seconds).
    pub timestamp: i64,
    pub message: String,
    /// Ed25519 signature over the serialized commit body (excluding this field).
    pub signature: Option<Signature>,
    /// Hash of the normalization profile active when this commit was authored.
    /// Consumers compare this across commits to know whether re-normalization
    /// is required before a meaningful diff.
    pub profile_hash: Hash256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub algo: String,
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Repo-wide manifest object: schema version + normalization profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaManifest {
    pub ifc_schema: String,
    pub tolerance_linear: f64,
    pub tolerance_angular: f64,
    pub created: i64,
    pub profile: Profile,
}

impl SchemaManifest {
    pub fn new_with_clock(ifc_schema: impl Into<String>, tol: vex_utils::Tolerance) -> Self {
        let profile = Profile {
            tolerance_linear: tol.linear,
            tolerance_angular: tol.angular,
            ..Profile::default()
        };
        Self {
            ifc_schema: ifc_schema.into(),
            tolerance_linear: tol.linear,
            tolerance_angular: tol.angular,
            created: OffsetDateTime::now_utc().unix_timestamp(),
            profile,
        }
    }

    #[must_use]
    pub fn with_profile(ifc_schema: impl Into<String>, profile: Profile) -> Self {
        Self {
            ifc_schema: ifc_schema.into(),
            tolerance_linear: profile.tolerance_linear,
            tolerance_angular: profile.tolerance_angular,
            created: OffsetDateTime::now_utc().unix_timestamp(),
            profile,
        }
    }
}

/// Encode + frame an object for storage.
///
/// Returns `(content_hash, framed_bytes)` where `content_hash` is the stable
/// identity (over kind + algo + uncompressed payload) and `framed_bytes` is
/// what actually lands on disk.
pub fn encode(kind: ObjectKind, payload: &[u8], algo: HashAlgo) -> VexResult<(Hash256, Vec<u8>)> {
    // Content hash.
    let mut h = Hasher::new(algo);
    h.update(&[kind as u8, algo as u8]);
    h.update(payload);
    let hash = h.finalize();

    // Compress payload.
    let compressed =
        zstd::encode_all(payload, 3).map_err(|e| VexError::Storage(format!("zstd: {e}")))?;

    let mut framed = Vec::with_capacity(8 + compressed.len());
    framed.extend_from_slice(MAGIC);
    framed.push(VERSION);
    framed.push(kind as u8);
    framed.push(algo as u8);
    framed.push(0); // reserved
    framed.extend_from_slice(&compressed);
    Ok((hash, framed))
}

/// Decode a framed object blob, returning the decompressed payload plus
/// the kind/algo header.
///
/// Verifies the content hash against `expected`.
pub fn decode(framed: &[u8], expected: Hash256) -> VexResult<(ObjectKind, HashAlgo, Vec<u8>)> {
    if framed.len() < 8 {
        return Err(VexError::Storage("object too small".into()));
    }
    if &framed[0..4] != MAGIC {
        return Err(VexError::Storage("bad magic".into()));
    }
    if framed[4] != VERSION {
        return Err(VexError::Storage(format!(
            "unsupported object version {}",
            framed[4]
        )));
    }
    let kind = ObjectKind::from_u8(framed[5])
        .ok_or_else(|| VexError::Storage(format!("unknown kind {}", framed[5])))?;
    let algo = match framed[6] {
        1 => HashAlgo::Blake3,
        2 => HashAlgo::Sha256,
        other => {
            return Err(VexError::Storage(format!("unknown hash algo {other}")));
        }
    };

    let payload =
        zstd::decode_all(&framed[8..]).map_err(|e| VexError::Storage(format!("zstd: {e}")))?;

    let mut h = Hasher::new(algo);
    h.update(&[kind as u8, algo as u8]);
    h.update(&payload);
    let actual = h.finalize();
    if actual != expected {
        return Err(VexError::HashMismatch {
            expected: expected.to_hex(),
            actual: actual.to_hex(),
        });
    }
    Ok((kind, algo, payload))
}
