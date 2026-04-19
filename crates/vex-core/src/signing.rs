//! Ed25519 commit signing.
//!
//! Signing uses the [`ed25519-dalek`] crate with the standard PureEd25519
//! algorithm (RFC 8032). The signing payload is the `bincode`-serialized
//! commit body **with `signature` field set to `None`** — verification
//! reproduces the exact same bytes and checks the attached signature.
//!
//! Keys are stored as raw 32-byte files under `.vex/keys/`:
//!
//! - `<name>.sk`  — 32-byte secret seed (0600 recommended)
//! - `<name>.pk`  — 32-byte verifying (public) key
//!
//! Using the OS keyring via the `keyring` crate is a future enhancement; the
//! simple file layout keeps the MVP portable and easy to inspect.

use std::fs;
use std::path::{Path, PathBuf};

use vex_storage::{Commit, Signature};
use vex_utils::{VexError, VexResult};
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};

pub const SIGNATURE_ALGO: &str = "ed25519";

const SK_LEN: usize = 32;
const PK_LEN: usize = 32;

/// Resolves the keystore directory inside a `.vex/` directory.
#[must_use]
pub fn keys_dir(vex_dir: &Path) -> PathBuf {
    vex_dir.join("keys")
}

/// Generate a fresh Ed25519 keypair and persist it under `keys_dir`.
///
/// Overwrites any existing key files for the same `name` — the caller is
/// responsible for confirming before clobbering.
pub fn generate_key(vex_dir: &Path, name: &str) -> VexResult<VerifyingKey> {
    validate_key_name(name)?;
    let dir = keys_dir(vex_dir);
    fs::create_dir_all(&dir).map_err(|e| VexError::io_at(&dir, e))?;
    let sk = SigningKey::generate(&mut rand_core::OsRng);
    let pk = sk.verifying_key();
    let sk_path = dir.join(format!("{name}.sk"));
    let pk_path = dir.join(format!("{name}.pk"));
    fs::write(&sk_path, sk.to_bytes()).map_err(|e| VexError::io_at(&sk_path, e))?;
    fs::write(&pk_path, pk.to_bytes()).map_err(|e| VexError::io_at(&pk_path, e))?;
    set_private_perms(&sk_path)?;
    Ok(pk)
}

/// Load a signing key by name.
pub fn load_signing_key(vex_dir: &Path, name: &str) -> VexResult<SigningKey> {
    validate_key_name(name)?;
    let path = keys_dir(vex_dir).join(format!("{name}.sk"));
    let bytes = fs::read(&path).map_err(|e| VexError::io_at(&path, e))?;
    if bytes.len() != SK_LEN {
        return Err(VexError::Config(format!(
            "{path:?}: expected {SK_LEN} bytes, found {}",
            bytes.len()
        )));
    }
    let mut arr = [0u8; SK_LEN];
    arr.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&arr))
}

/// Enumerate the names of available keys.
pub fn list_keys(vex_dir: &Path) -> VexResult<Vec<String>> {
    let dir = keys_dir(vex_dir);
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(&dir).map_err(|e| VexError::io_at(&dir, e))? {
        let entry = entry.map_err(|e| VexError::io_at(&dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("sk") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Produce the bytes that get signed for a commit.
fn signing_bytes(commit: &Commit) -> VexResult<Vec<u8>> {
    // Clone + clear signature field so signers and verifiers agree on the
    // canonical body bytes regardless of whether a signature is already
    // attached.
    let mut c = commit.clone();
    c.signature = None;
    bincode::serialize(&c)
        .map_err(|e| VexError::Storage(format!("commit serialize: {e}")))
}

/// Sign a commit in place with the named key. Returns the verifying key so
/// callers can persist / display it.
pub fn sign_commit(
    vex_dir: &Path,
    key_name: &str,
    commit: &mut Commit,
) -> VexResult<VerifyingKey> {
    let sk = load_signing_key(vex_dir, key_name)?;
    let body = signing_bytes(commit)?;
    let sig = sk.sign(&body);
    let pk = sk.verifying_key();
    commit.signature = Some(Signature {
        algo: SIGNATURE_ALGO.to_string(),
        public_key: pk.to_bytes().to_vec(),
        signature: sig.to_bytes().to_vec(),
    });
    Ok(pk)
}

/// Verify a commit's signature. Returns `Ok(true)` for a valid signature,
/// `Ok(false)` for an unsigned commit, and an error for algorithm mismatch
/// or verification failure.
pub fn verify_commit(commit: &Commit) -> VexResult<bool> {
    let Some(sig) = &commit.signature else {
        return Ok(false);
    };
    if sig.algo != SIGNATURE_ALGO {
        return Err(VexError::Config(format!(
            "unsupported signature algo: {}",
            sig.algo
        )));
    }
    if sig.public_key.len() != PK_LEN {
        return Err(VexError::Config("bad public key length".into()));
    }
    if sig.signature.len() != ed25519_dalek::SIGNATURE_LENGTH {
        return Err(VexError::Config("bad signature length".into()));
    }
    let mut pk_bytes = [0u8; PK_LEN];
    pk_bytes.copy_from_slice(&sig.public_key);
    let pk = VerifyingKey::from_bytes(&pk_bytes)
        .map_err(|e| VexError::Config(format!("bad public key: {e}")))?;
    let sig_bytes: [u8; ed25519_dalek::SIGNATURE_LENGTH] = sig
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| VexError::Config("bad signature length".into()))?;
    let body = signing_bytes(commit)?;
    pk.verify(&body, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
        .map_err(|e| VexError::Config(format!("signature verification failed: {e}")))?;
    Ok(true)
}

fn validate_key_name(name: &str) -> VexResult<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(VexError::InvalidRef(format!("bad key name: {name:?}")));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(VexError::InvalidRef(format!("bad key name: {name:?}")));
    }
    if name.starts_with('.') || name.contains("..") {
        return Err(VexError::InvalidRef(format!("bad key name: {name:?}")));
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_perms(path: &Path) -> VexResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, perms).map_err(|e| VexError::io_at(path, e))
}

#[cfg(not(unix))]
fn set_private_perms(_path: &Path) -> VexResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_storage::{Identity};
    use vex_utils::Hash256;

    fn sample_commit() -> Commit {
        Commit {
            tree: Hash256::ZERO,
            parents: Vec::new(),
            author: Identity {
                name: "a".into(),
                email: "a@b".into(),
            },
            committer: Identity {
                name: "a".into(),
                email: "a@b".into(),
            },
            timestamp: 0,
            message: "m".into(),
            signature: None,
            profile_hash: Hash256::ZERO,
        }
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "vex-sign-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    #[test]
    fn roundtrip_sign_verify() {
        let dir = tempdir();
        generate_key(&dir, "alice").expect("gen");
        let mut c = sample_commit();
        sign_commit(&dir, "alice", &mut c).expect("sign");
        assert!(verify_commit(&c).expect("verify"));
    }

    #[test]
    fn tampered_body_fails_verification() {
        let dir = tempdir();
        generate_key(&dir, "alice").expect("gen");
        let mut c = sample_commit();
        sign_commit(&dir, "alice", &mut c).expect("sign");
        c.message = "tampered".into();
        assert!(verify_commit(&c).is_err());
    }

    #[test]
    fn unsigned_commit_reports_false() {
        let c = sample_commit();
        assert_eq!(verify_commit(&c).expect("verify"), false);
    }

    #[test]
    fn bad_key_name_rejected() {
        let dir = tempdir();
        assert!(generate_key(&dir, "").is_err());
        assert!(generate_key(&dir, "../etc/passwd").is_err());
        assert!(generate_key(&dir, ".hidden").is_err());
    }
}
