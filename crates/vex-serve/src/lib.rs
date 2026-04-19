//! Server-side implementation of the vex remote protocol.
//!
//! [`serve_session`] owns one client conversation: it reads [`Frame`]s from
//! `reader`, dispatches them against a [`Repository`], and writes replies to
//! `writer`. Designed to be driven by `vex-serve` over stdin/stdout (the
//! canonical SSH `ForceCommand` invocation), but trivially testable with
//! in-memory pipes.
//!
//! Repository resolution: the client's [`Frame::Hello`] supplies a logical
//! repository name (e.g. `"<org>/<project>"`) which is resolved against
//! [`ServeConfig::repo_root`] using [`resolve_repo_path`] — a hardened
//! parser that rejects `..`, absolute paths, control characters, and
//! anything other than a single `<owner>/<name>` pair. This is the only
//! security-critical input on the server side.

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use vex_core::Repository;
use vex_protocol::{
    default_server_capabilities, read_frame, write_frame, Frame, PackEntry, ProtocolError,
    UpdateRefStatus, PROTOCOL_VERSION,
};
use vex_storage::ObjectKind;
use vex_utils::{Hash256, VexError, VexResult};

pub mod architur;
use crate::architur::{ArchiturClient, RefUpdatedCommit, RefUpdatedPayload};

/// Maximum number of pack entries packed into a single [`Frame::PackChunk`].
/// 256 keeps each frame comfortably under the 64 MiB cap even with large
/// tree blobs.
const PACK_CHUNK_ENTRIES: usize = 256;

/// Server configuration. One instance per `vex-serve` process.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// Filesystem directory that contains all hosted repositories. Every
    /// resolved repository path MUST be a child of this root.
    pub repo_root: PathBuf,
    /// Identity advertised in [`Frame::HelloOk::server_version`].
    pub server_version: String,
    /// If `false`, the server refuses any push (client may still fetch).
    /// Used by read-only mirrors and by sessions where the user lacks
    /// write authorization.
    pub allow_push: bool,
    /// SSH-authenticated user id (UUID string from architur). When set
    /// together with [`Self::architur`], every command is authorized.
    pub user_id: Option<String>,
    /// Optional architur API client. If `None`, vex-serve runs standalone
    /// (useful for local dev). If `Some` and `fail_closed = true`, denial
    /// or unreachable architur stops the session.
    pub architur: Option<ArchiturClient>,
}

impl ServeConfig {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            server_version: format!("vex-serve {}", env!("CARGO_PKG_VERSION")),
            allow_push: true,
            user_id: None,
            architur: None,
        }
    }
}

/// Top-level server errors. Wire-level [`ProtocolError`]s and storage
/// [`VexError`]s are both surfaced to the client as [`Frame::Error`].
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("storage: {0}")]
    Storage(#[from] VexError),
    #[error("invalid repository name: {0}")]
    InvalidRepo(String),
    #[error("repository does not exist: {0}")]
    UnknownRepo(String),
}

pub type ServeResult<T> = Result<T, ServeError>;

/// Parse and validate the client-supplied repo identifier and resolve it
/// to a filesystem path under `root`.
///
/// Acceptable shape: `"<owner>/<name>"` — exactly two path components, each
/// matching `[A-Za-z0-9_.-]{1,64}` and not starting with a dot. Anything
/// else returns [`ServeError::InvalidRepo`]. The resulting path is then
/// canonicalised against `root` to defeat any residual traversal attempts.
pub fn resolve_repo_path(root: &Path, requested: &str) -> ServeResult<PathBuf> {
    let parts: Vec<&str> = requested.split('/').collect();
    if parts.len() != 2 {
        return Err(ServeError::InvalidRepo(format!(
            "expected '<owner>/<name>', got '{requested}'"
        )));
    }
    for p in &parts {
        if p.is_empty() || p.len() > 64 {
            return Err(ServeError::InvalidRepo(requested.into()));
        }
        if p.starts_with('.') {
            return Err(ServeError::InvalidRepo(requested.into()));
        }
        if !p.bytes().all(is_repo_char) {
            return Err(ServeError::InvalidRepo(requested.into()));
        }
    }
    let candidate = root.join(parts[0]).join(parts[1]);
    Ok(candidate)
}

fn is_repo_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-')
}

/// Read `<repo>/.vex/architur.toml` if present and return the architur
/// repository UUID. Architur writes this file at provision time. Format:
///
/// ```toml
/// repo_id = "00000000-0000-0000-0000-000000000000"
/// ```
fn read_architur_repo_id(repo_path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(repo_path.join(".vex/architur.toml")).ok()?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("repo_id") {
            let rest = rest.trim_start_matches([' ', '\t', '=']);
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn hex_hash(h: Hash256) -> String {
    let bytes = h.as_bytes();
    hex_encode(bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}

fn ref_kind(name: &str) -> String {
    if name.starts_with("refs/heads/") {
        "branch".into()
    } else if name.starts_with("refs/tags/") {
        "tag".into()
    } else {
        "other".into()
    }
}

fn rfc3339_from_unix(secs: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64);
    let total = dt
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (total / 86_400) as i64;
    let time = (total % 86_400) as i64;
    // Howard Hinnant civil-from-days, kept inline so we don't pull chrono.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let h = (time / 3600) as u32;
    let min = ((time % 3600) / 60) as u32;
    let s = (time % 60) as u32;
    format!("{year:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

/// Drive a single client conversation to completion.
///
/// Protocol:
/// 1. Read [`Frame::Hello`]; reply [`Frame::HelloOk`] (or `Error` on bad input).
/// 2. Loop on subsequent frames: `LsRefs`, fetch (`Want`/`Have`/`Done`), or
///    push (`PackStart`/`PackChunk`/`PackEnd` then `UpdateRef`).
/// 3. Conversation ends on `Done` after a stable state, on `Error`, or on
///    EOF.
///
/// Returns `Ok(())` on graceful shutdown. Any error is also reported to the
/// client as [`Frame::Error`] before being returned.
pub fn serve_session<R: Read, W: Write>(
    config: &ServeConfig,
    reader: &mut R,
    writer: &mut W,
) -> ServeResult<()> {
    match serve_session_inner(config, reader, writer) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Best-effort: tell the client what happened. Ignore secondary
            // I/O errors — the underlying socket may already be closed.
            let _ = write_frame(writer, &Frame::Error { message: err.to_string() });
            Err(err)
        }
    }
}

fn serve_session_inner<R: Read, W: Write>(
    config: &ServeConfig,
    reader: &mut R,
    writer: &mut W,
) -> ServeResult<()> {
    // ---- Handshake ---------------------------------------------------------
    let hello = read_frame(reader)?;
    let (proto, repo_name) = match hello {
        Frame::Hello { protocol, repo, .. } => (protocol, repo),
        other => {
            return Err(ServeError::Protocol(ProtocolError::Unexpected(format!(
                "expected Hello, got {other:?}"
            ))))
        }
    };
    if proto != PROTOCOL_VERSION {
        return Err(ServeError::Protocol(ProtocolError::VersionMismatch {
            client: proto,
            server: PROTOCOL_VERSION,
        }));
    }
    let path = resolve_repo_path(&config.repo_root, &repo_name)?;
    if !path.join(".vex").is_dir() {
        return Err(ServeError::UnknownRepo(repo_name));
    }
    let repo = Repository::open(&path)?;

    // Read the architur repo id sidecar (if present). Architur writes this
    // file at provision time so vex-serve can use it for authorize +
    // ref-updated calls. Missing file = standalone repo, no architur calls.
    let architur_repo_id = read_architur_repo_id(&path);

    // Per-session authorization. Skipped when no architur client is
    // configured (local dev) or when the repo has no architur sidecar.
    if let (Some(client), Some(repo_id), Some(user_id)) =
        (&config.architur, architur_repo_id.as_deref(), config.user_id.as_deref())
    {
        let op = if config.allow_push { "push" } else { "fetch" };
        if !client.authorize(user_id, repo_id, op) {
            return Err(ServeError::Protocol(ProtocolError::Unexpected(
                format!("architur denied {op} for repo {repo_id}"),
            )));
        }
    }

    write_frame(
        writer,
        &Frame::HelloOk {
            protocol: PROTOCOL_VERSION,
            server_version: config.server_version.clone(),
            // Use the on-disk path as the audit identifier. The .NET API
            // assigns its own UUID via the `authorize` callback in Phase 2.
            repo_id: path.display().to_string(),
            capabilities: default_server_capabilities(),
        },
    )?;

    // ---- Main loop ---------------------------------------------------------
    // Pending state across negotiation frames within a single fetch.
    let mut wants: Vec<Hash256> = Vec::new();
    let mut haves: Vec<Hash256> = Vec::new();

    loop {
        let frame = match read_frame(reader) {
            Ok(f) => f,
            Err(ProtocolError::UnexpectedEof) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match frame {
            Frame::LsRefs => {
                let refs = repo.store().list_refs()?;
                write_frame(writer, &Frame::Refs(refs))?;
            }

            Frame::Want(mut hs) => wants.append(&mut hs),
            Frame::Have(mut hs) => haves.append(&mut hs),
            Frame::Done => {
                if !wants.is_empty() {
                    // Client finished negotiating a fetch — ship a pack.
                    send_pack(writer, &repo, &wants, &haves)?;
                    wants.clear();
                    haves.clear();
                } else {
                    // Quiet ack — no work pending. Continue listening; the
                    // client may follow up with another command.
                }
            }

            Frame::PackStart { .. } => {
                if !config.allow_push {
                    write_frame(
                        writer,
                        &Frame::Error {
                            message: "push not allowed for this session".into(),
                        },
                    )?;
                    return Ok(());
                }
                receive_pack(reader, &repo)?;
            }
            Frame::PackChunk(_) | Frame::PackEnd => {
                // These should only ever arrive inside `receive_pack`. Seeing
                // one out-of-band is a protocol violation.
                return Err(ServeError::Protocol(ProtocolError::Unexpected(
                    "stray pack frame outside PackStart..PackEnd".into(),
                )));
            }

            Frame::UpdateRef {
                name,
                expected_old,
                new,
            } => {
                if !config.allow_push {
                    write_frame(
                        writer,
                        &Frame::UpdateRefAck {
                            name,
                            status: UpdateRefStatus::Rejected {
                                reason: "push not allowed".into(),
                            },
                        },
                    )?;
                    continue;
                }
                // Refuse to point a ref at an object the server doesn't have.
                // This is the cheapest defence against ref corruption; the
                // client should always upload its pack first.
                if !repo.store().has(new)? {
                    write_frame(
                        writer,
                        &Frame::UpdateRefAck {
                            name,
                            status: UpdateRefStatus::Rejected {
                                reason: "target object missing on server".into(),
                            },
                        },
                    )?;
                    continue;
                }
                let status = match repo.store().cas_ref(&name, expected_old, new) {
                    Ok(true) => UpdateRefStatus::Ok,
                    Ok(false) => {
                        let actual = repo.store().get_ref(&name)?;
                        UpdateRefStatus::Conflict { actual }
                    }
                    Err(e) => UpdateRefStatus::Rejected {
                        reason: e.to_string(),
                    },
                };
                // Mirror the move into architur so the studio UI updates.
                // Best-effort — a failure here doesn't block the SSH session.
                if let (UpdateRefStatus::Ok, Some(client), Some(repo_id)) = (
                    &status,
                    config.architur.as_ref(),
                    architur_repo_id.as_deref(),
                ) {
                    let commit = repo
                        .store()
                        .get_commit(new)
                        .ok()
                        .map(|c| RefUpdatedCommit {
                            commit_hash: hex_hash(new),
                            parent_hash: c.parents.first().map(|h| hex_hash(*h)),
                            tree_hash: hex_hash(c.tree),
                            author_name: c.author.name.clone(),
                            author_email: c.author.email.clone(),
                            message: c.message.clone(),
                            authored_at: rfc3339_from_unix(c.timestamp),
                            pushed_by_user_id: config.user_id.clone(),
                        });
                    let payload = RefUpdatedPayload {
                        repository_id: repo_id.to_string(),
                        ref_name: name.clone(),
                        kind: ref_kind(&name),
                        old_commit_hash: expected_old.map(hex_hash),
                        new_commit_hash: Some(hex_hash(new)),
                        // We don't track ref generation locally yet; let
                        // architur derive it (0 = "unknown, take next").
                        new_generation: 0,
                        commit,
                    };
                    client.ref_updated(&payload);
                }
                write_frame(writer, &Frame::UpdateRefAck { name, status })?;
            }

            Frame::Error { message } => {
                tracing::warn!(message, "client reported error; closing");
                return Ok(());
            }

            // The server should never receive these as commands.
            Frame::Hello { .. } | Frame::HelloOk { .. } | Frame::Refs(_) | Frame::UpdateRefAck { .. } => {
                return Err(ServeError::Protocol(ProtocolError::Unexpected(
                    "client sent server-only frame".into(),
                )));
            }
        }
    }
}

/// Compute the closure of every object reachable from `wants`, minus the
/// closure reachable from `haves`, and stream it to `writer` as a sequence
/// of pack frames.
fn send_pack<W: Write>(
    writer: &mut W,
    repo: &Repository,
    wants: &[Hash256],
    haves: &[Hash256],
) -> ServeResult<()> {
    // Collect "haves" first so we can subtract.
    let mut excluded: HashSet<Hash256> = HashSet::new();
    for h in haves {
        // Tolerate unknown haves — clients may legitimately list refs the
        // server has GC'd. Skip silently rather than fail the whole fetch.
        if repo.store().has(*h).unwrap_or(false) {
            collect_reachable(repo, *h, &mut excluded)?;
        }
    }

    let mut included: HashSet<Hash256> = HashSet::new();
    for h in wants {
        collect_reachable(repo, *h, &mut included)?;
    }
    let to_send: Vec<Hash256> = included.difference(&excluded).copied().collect();

    write_frame(
        writer,
        &Frame::PackStart {
            entry_count: to_send.len() as u64,
        },
    )?;
    for chunk in to_send.chunks(PACK_CHUNK_ENTRIES) {
        let mut entries = Vec::with_capacity(chunk.len());
        for h in chunk {
            let bytes = repo.store().get_framed(*h)?;
            entries.push(PackEntry { hash: *h, bytes });
        }
        write_frame(writer, &Frame::PackChunk(entries))?;
    }
    write_frame(writer, &Frame::PackEnd)?;
    Ok(())
}

/// Receive a pack from the client, verifying every entry's content hash and
/// writing accepted objects to the local store.
///
/// Pre: the caller already consumed [`Frame::PackStart`]. Reads frames
/// until [`Frame::PackEnd`].
fn receive_pack<R: Read>(reader: &mut R, repo: &Repository) -> ServeResult<()> {
    loop {
        match read_frame(reader)? {
            Frame::PackChunk(entries) => {
                for entry in entries {
                    // `put_framed` re-validates the content hash before
                    // committing — a malicious peer cannot poison the store.
                    repo.store().put_framed(entry.hash, &entry.bytes)?;
                }
            }
            Frame::PackEnd => return Ok(()),
            Frame::Error { message } => {
                return Err(ServeError::Protocol(ProtocolError::Unexpected(format!(
                    "client errored mid-pack: {message}"
                ))))
            }
            other => {
                return Err(ServeError::Protocol(ProtocolError::Unexpected(format!(
                    "expected PackChunk/PackEnd, got {other:?}"
                ))))
            }
        }
    }
}

/// BFS-walk the object graph from a commit (or tree, or blob) hash,
/// inserting every reachable hash into `out`.
fn collect_reachable(
    repo: &Repository,
    start: Hash256,
    out: &mut HashSet<Hash256>,
) -> VexResult<()> {
    let mut queue: VecDeque<Hash256> = VecDeque::new();
    queue.push_back(start);
    while let Some(h) = queue.pop_front() {
        if !out.insert(h) {
            continue;
        }
        // Decode just enough to know what kind of object this is and what
        // it points at. Errors propagate — corrupt objects must not silently
        // truncate a pack.
        let kind = repo.store().object_kind(h)?;
        match kind {
            ObjectKind::Commit => {
                let commit = repo.store().get_commit(h)?;
                queue.push_back(commit.tree);
                for p in commit.parents {
                    queue.push_back(p);
                }
            }
            ObjectKind::Tree => {
                let tree = repo.store().get_tree(h)?;
                for entry in tree.entries {
                    queue.push_back(entry.blob_hash);
                    // node_hash is a graph identity, not a stored object,
                    // so it is intentionally not enqueued.
                }
            }
            ObjectKind::Blob | ObjectKind::SchemaManifest | ObjectKind::Tag => {
                // Leaf — nothing to enqueue.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_resolver_accepts_simple() {
        let root = Path::new("/srv/vex");
        let p = resolve_repo_path(root, "acme/tower").unwrap();
        assert_eq!(p, Path::new("/srv/vex/acme/tower"));
    }

    #[test]
    fn repo_path_resolver_rejects_traversal() {
        let root = Path::new("/srv/vex");
        for bad in [
            "../etc/passwd",
            "acme/../etc",
            "/abs/path",
            "acme",
            "acme/tower/extra",
            "acme/.hidden",
            ".hidden/repo",
            "ac me/tower",
            "acme/tower\0null",
        ] {
            assert!(
                resolve_repo_path(root, bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn repo_path_resolver_rejects_long_components() {
        let root = Path::new("/srv/vex");
        let big = "a".repeat(65);
        let bad = format!("{big}/repo");
        assert!(resolve_repo_path(root, &bad).is_err());
    }
}
