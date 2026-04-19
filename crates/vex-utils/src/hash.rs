//! Hashing primitives.
//!
//! We use Blake3 as the primary content-addressing hash because of its speed and
//! parallelism. Every object header additionally records a SHA-256 to aid
//! external interop (signing, audit logs) — computed lazily by callers that
//! need it. All hashes are **256-bit** regardless of algorithm, so the on-disk
//! format stays fixed-width.
//!
//! SHA-1 is intentionally unsupported.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A 256-bit hash digest, algorithm-agnostic at the type level.
///
/// Equality and ordering are on the raw bytes; it is the caller's responsibility
/// to compare only hashes computed with the same [`HashAlgo`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash256(pub [u8; 32]);

impl Hash256 {
    pub const ZERO: Self = Self([0u8; 32]);

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse a 64-char lowercase hex string.
    pub fn from_hex(s: &str) -> Result<Self, HashParseError> {
        if s.len() != 64 {
            return Err(HashParseError::WrongLength(s.len()));
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(|_| HashParseError::InvalidHex)?;
        Ok(Self(out))
    }

    /// Prefix-match used for short hash resolution (à la `git log --oneline`).
    #[must_use]
    pub fn hex_prefix(&self, n: usize) -> String {
        let n = n.min(64);
        hex::encode(self.0)[..n].to_string()
    }
}

impl fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash256({})", self.hex_prefix(16))
    }
}

impl fmt::Display for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HashParseError {
    #[error("expected 64 hex chars, got {0}")]
    WrongLength(usize),
    #[error("invalid hex")]
    InvalidHex,
}

/// Which hash function a digest was produced with.
///
/// Stored alongside every object header so the on-disk format can evolve without
/// ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum HashAlgo {
    Blake3 = 1,
    Sha256 = 2,
}

impl HashAlgo {
    pub const DEFAULT: HashAlgo = HashAlgo::Blake3;
}

/// Streaming hasher that produces a [`Hash256`] for any supported algorithm.
#[derive(Debug)]
pub enum Hasher {
    Blake3(Box<blake3::Hasher>),
    Sha256(sha2::Sha256),
}

impl Hasher {
    #[must_use]
    pub fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Blake3 => Hasher::Blake3(Box::new(blake3::Hasher::new())),
            HashAlgo::Sha256 => {
                use sha2::Digest;
                Hasher::Sha256(sha2::Sha256::new())
            }
        }
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        match self {
            Hasher::Blake3(h) => {
                h.update(bytes);
            }
            Hasher::Sha256(h) => {
                use sha2::Digest;
                h.update(bytes);
            }
        }
        self
    }

    #[must_use]
    pub fn finalize(self) -> Hash256 {
        match self {
            Hasher::Blake3(h) => {
                let digest = h.finalize();
                Hash256(*digest.as_bytes())
            }
            Hasher::Sha256(h) => {
                use sha2::Digest;
                let digest = h.finalize();
                let mut out = [0u8; 32];
                out.copy_from_slice(&digest);
                Hash256(out)
            }
        }
    }
}

/// Convenience: hash a single byte buffer with the default algorithm.
#[must_use]
pub fn hash_default(bytes: &[u8]) -> Hash256 {
    let mut h = Hasher::new(HashAlgo::DEFAULT);
    h.update(bytes);
    h.finalize()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let h = hash_default(b"hello");
        let s = h.to_hex();
        assert_eq!(s.len(), 64);
        let back = Hash256::from_hex(&s).expect("roundtrip");
        assert_eq!(h, back);
    }

    #[test]
    fn algo_produces_different_digests() {
        let a = {
            let mut h = Hasher::new(HashAlgo::Blake3);
            h.update(b"hello");
            h.finalize()
        };
        let b = {
            let mut h = Hasher::new(HashAlgo::Sha256);
            h.update(b"hello");
            h.finalize()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_length_rejected() {
        assert!(Hash256::from_hex("deadbeef").is_err());
    }
}
