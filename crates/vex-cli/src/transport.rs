//! Client-side wire protocol against a `vex-serve` instance reached over SSH.
//!
//! The transport is `std::process::Command::new("ssh") ... vex-serve <repo>`
//! with stdin and stdout piped. We wrap that pair into a [`SshChannel`] that
//! implements `Read + Write` so the protocol code stays transport-agnostic.

use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use vex_core::Repository;
use vex_protocol::{read_frame, write_frame, Frame, PackEntry, UpdateRefStatus, PROTOCOL_VERSION};
use vex_storage::{object, ObjectKind};
use vex_utils::Hash256;

use crate::remote::SshUrl;

/// Hard cap on how many entries we accept in a single fetched pack to keep
/// peak memory bounded. Tweakable; matches the server's chunk size *256x*.
const MAX_FETCH_ENTRIES: u64 = 1_000_000;

/// Read+Write wrapper over a child SSH process's stdio. Owns the child so it
/// is killed when the channel is dropped (no leaked SSH sessions).
pub(crate) struct SshChannel {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl SshChannel {
    /// Spawn `ssh -p <port> <user>@<host> vex-serve <repo>`.
    /// Honors `VEX_SSH_BIN` (default `ssh`) and `VEX_SSH_OPTS` (extra args,
    /// space-split) for environments that need a non-stock client.
    pub(crate) fn open(url: &SshUrl) -> Result<Self> {
        let ssh_bin = std::env::var("VEX_SSH_BIN").unwrap_or_else(|_| "ssh".into());
        let mut cmd = Command::new(&ssh_bin);
        cmd.arg("-p").arg(url.port.to_string());
        // Disable interactive prompting; tests + servers should rely on keys.
        cmd.args(["-o", "BatchMode=yes"]);
        cmd.args(["-o", "ServerAliveInterval=30"]);
        if let Ok(extra) = std::env::var("VEX_SSH_OPTS") {
            for tok in extra.split_whitespace() {
                cmd.arg(tok);
            }
        }
        cmd.arg(format!("{}@{}", url.user, url.host));
        cmd.arg("vex-serve").arg(&url.repo);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn `{}`", ssh_bin))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn close(mut self) -> Result<()> {
        // Closing stdin signals EOF to the server.
        drop(self.stdin);
        let status = self.child.wait().context("ssh wait")?;
        if !status.success() {
            bail!("ssh exited with status {status}");
        }
        Ok(())
    }
}

impl Read for SshChannel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl Write for SshChannel {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

/// Perform the protocol Hello/HelloOk exchange and return the negotiated
/// server version string for diagnostics.
fn handshake<C: Read + Write>(ch: &mut C, repo: &str) -> Result<String> {
    write_frame(
        ch,
        &Frame::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: format!("vex-cli/{}", env!("CARGO_PKG_VERSION")),
            repo: repo.to_string(),
        },
    )?;
    match read_frame(ch)? {
        Frame::HelloOk {
            protocol,
            server_version,
            ..
        } => {
            if protocol != PROTOCOL_VERSION {
                bail!("server speaks protocol v{protocol}, we speak v{PROTOCOL_VERSION}");
            }
            Ok(server_version)
        }
        Frame::Error { message } => bail!("server: {message}"),
        other => bail!("expected HelloOk, got {other:?}"),
    }
}

fn ls_refs<C: Read + Write>(ch: &mut C) -> Result<Vec<(String, Hash256)>> {
    write_frame(ch, &Frame::LsRefs)?;
    match read_frame(ch)? {
        Frame::Refs(rs) => Ok(rs),
        Frame::Error { message } => bail!("server: {message}"),
        other => bail!("expected Refs, got {other:?}"),
    }
}

/// Fetch all objects reachable from `wants` that the local repo doesn't
/// already have. Updates remote-tracking refs `refs/remotes/<remote>/*` to
/// the wanted heads.
pub(crate) fn fetch(repo: &Repository, url: &SshUrl, remote_name: &str) -> Result<FetchReport> {
    let mut ch = SshChannel::open(url)?;
    let _server = handshake(&mut ch, &url.repo)?;
    let refs = ls_refs(&mut ch)?;

    // Compute haves = objects we already have at the tip of any local ref.
    let store = repo.store();
    let mut haves: Vec<Hash256> = Vec::new();
    for (_, h) in store.list_refs()? {
        haves.push(h);
    }
    haves.sort();
    haves.dedup();

    let wants: Vec<Hash256> = refs.iter().map(|(_, h)| *h).collect();
    if wants.is_empty() {
        ch.close()?;
        return Ok(FetchReport {
            received: 0,
            refs_updated: 0,
        });
    }

    write_frame(&mut ch, &Frame::Want(wants.clone()))?;
    if !haves.is_empty() {
        write_frame(&mut ch, &Frame::Have(haves))?;
    }
    write_frame(&mut ch, &Frame::Done)?;

    // Drain the pack.
    let mut received = 0usize;
    match read_frame(&mut ch)? {
        Frame::PackStart { entry_count } => {
            if entry_count > MAX_FETCH_ENTRIES {
                bail!("server tried to send {entry_count} objects (cap {MAX_FETCH_ENTRIES})");
            }
        }
        Frame::Error { message } => bail!("server: {message}"),
        other => bail!("expected PackStart, got {other:?}"),
    }
    loop {
        match read_frame(&mut ch)? {
            Frame::PackChunk(es) => {
                for e in es {
                    // `put_framed` re-validates the hash, so a malicious
                    // server cannot poison our store.
                    store.put_framed(e.hash, &e.bytes)?;
                    received += 1;
                }
            }
            Frame::PackEnd => break,
            Frame::Error { message } => bail!("server: {message}"),
            other => bail!("expected PackChunk/PackEnd, got {other:?}"),
        }
    }

    // Mirror remote heads into refs/remotes/<remote>/*.
    let mut refs_updated = 0;
    for (name, target) in &refs {
        let local = match name.strip_prefix("refs/heads/") {
            Some(short) => format!("refs/remotes/{remote_name}/{short}"),
            None => continue, // skip tags etc. for now
        };
        store.set_ref(&local, *target)?;
        refs_updated += 1;
    }

    ch.close()?;
    Ok(FetchReport {
        received,
        refs_updated,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct FetchReport {
    pub received: usize,
    pub refs_updated: usize,
}

/// Upload `local_ref`'s tip to `remote_ref` on the remote. CAS uses the
/// **remote-tracking** ref `refs/remotes/<remote>/<short>` as `expected_old`
/// so concurrent server-side advances are detected (matching git's
/// rejected-non-fast-forward semantics). `force` skips the precondition.
///
/// On success, the local remote-tracking ref is advanced to `new`.
pub(crate) fn push(
    repo: &Repository,
    url: &SshUrl,
    remote_name: &str,
    local_ref: &str,
    remote_ref: &str,
    force: bool,
) -> Result<PushReport> {
    let store = repo.store();
    let new = store
        .get_ref(local_ref)?
        .ok_or_else(|| anyhow!("local ref `{local_ref}` does not exist"))?;

    // Compute the local view of the server: the remote-tracking ref.
    let short = remote_ref.strip_prefix("refs/heads/").unwrap_or(remote_ref);
    let tracking = format!("refs/remotes/{remote_name}/{short}");
    let expected_old: Option<Hash256> = if force {
        None
    } else {
        store.get_ref(&tracking)?
    };

    let mut ch = SshChannel::open(url)?;
    let _server = handshake(&mut ch, &url.repo)?;
    let remote_refs = ls_refs(&mut ch)?;

    // Plan the pack: every reachable object minus what the server already has
    // (approximated by the union of remote ref tips we just read).
    let server_has: Vec<Hash256> = remote_refs.iter().map(|(_, h)| *h).collect();
    let to_send = walk_reachable_minus(repo, &[new], &server_has)?;

    // Frame each object verbatim from the local store. `get_framed` returns
    // the on-the-wire bytes; the server re-hashes on receive.
    let mut entries: Vec<PackEntry> = Vec::with_capacity(to_send.len());
    for h in &to_send {
        let bytes = store.get_framed(*h)?;
        entries.push(PackEntry { hash: *h, bytes });
    }

    write_frame(
        &mut ch,
        &Frame::PackStart {
            entry_count: entries.len() as u64,
        },
    )?;
    for chunk in entries.chunks(256) {
        write_frame(&mut ch, &Frame::PackChunk(chunk.to_vec()))?;
    }
    write_frame(&mut ch, &Frame::PackEnd)?;

    write_frame(
        &mut ch,
        &Frame::UpdateRef {
            name: remote_ref.to_string(),
            expected_old,
            new,
        },
    )?;
    let status = match read_frame(&mut ch)? {
        Frame::UpdateRefAck { status, .. } => status,
        Frame::Error { message } => bail!("server: {message}"),
        other => bail!("expected UpdateRefAck, got {other:?}"),
    };

    // On success, advance the local view of the server.
    if let UpdateRefStatus::Ok = status {
        store.set_ref(&tracking, new)?;
    }

    ch.close()?;
    Ok(PushReport {
        sent: to_send.len(),
        status,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct PushReport {
    pub sent: usize,
    pub status: UpdateRefStatus,
}

/// Walk every object reachable from `wants` and stop at any object reachable
/// from `haves`. Returns objects in arbitrary order.
fn walk_reachable_minus(
    repo: &Repository,
    wants: &[Hash256],
    haves: &[Hash256],
) -> Result<Vec<Hash256>> {
    use std::collections::HashSet;
    let store = repo.store();
    let mut have_closure: HashSet<Hash256> = HashSet::new();
    let mut stack: Vec<Hash256> = haves.to_vec();
    while let Some(h) = stack.pop() {
        if !have_closure.insert(h) {
            continue;
        }
        if !store.has(h)? {
            continue;
        }
        for child in object_children(repo, h)? {
            if !have_closure.contains(&child) {
                stack.push(child);
            }
        }
    }

    let mut out: Vec<Hash256> = Vec::new();
    let mut seen: HashSet<Hash256> = HashSet::new();
    let mut stack: Vec<Hash256> = wants.to_vec();
    while let Some(h) = stack.pop() {
        if have_closure.contains(&h) || !seen.insert(h) {
            continue;
        }
        out.push(h);
        for child in object_children(repo, h)? {
            stack.push(child);
        }
    }
    Ok(out)
}

fn object_children(repo: &Repository, h: Hash256) -> Result<Vec<Hash256>> {
    let store = repo.store();
    if !store.has(h)? {
        return Ok(vec![]);
    }
    let framed = store.get_framed(h)?;
    let (kind, _algo, payload) = object::decode(&framed, h)?;
    let mut out = Vec::new();
    match kind {
        ObjectKind::Commit => {
            let c: vex_storage::Commit =
                bincode::deserialize(&payload).map_err(|e| anyhow!("decode commit {h}: {e}"))?;
            out.push(c.tree);
            out.extend(c.parents);
        }
        ObjectKind::Tree => {
            let t: vex_storage::Tree =
                bincode::deserialize(&payload).map_err(|e| anyhow!("decode tree {h}: {e}"))?;
            for entry in t.entries {
                // Mirror the server's collect_reachable: only blob_hash is a
                // stored object; node_hash is a graph-identity tag.
                out.push(entry.blob_hash);
            }
        }
        ObjectKind::Blob | ObjectKind::SchemaManifest | ObjectKind::Tag => {}
    }
    Ok(out)
}

/// Best-effort `clone`: init repo if absent, register the remote, fetch all
/// refs, then point `refs/heads/main` and `HEAD` at `refs/remotes/<name>/main`
/// if it exists.
pub(crate) fn clone(url: &SshUrl, dir: &Path, remote_name: &str) -> Result<FetchReport> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let repo = match Repository::open(dir) {
        Ok(r) => r,
        Err(_) => Repository::init(dir).context("init")?,
    };
    // Persist the remote in the new repo so subsequent fetch/push/pull
    // commands work without re-passing the URL.
    let mut remotes = crate::remote::RemoteStore::open(dir)?;
    if remotes.get(remote_name).is_none() {
        remotes.add(
            remote_name,
            &format!("ssh://{}@{}:{}/{}", url.user, url.host, url.port, url.repo),
        )?;
    }
    let report = fetch(&repo, url, remote_name)?;
    let mirror = format!("refs/remotes/{remote_name}/main");
    if let Some(target) = repo.store().get_ref(&mirror)? {
        repo.store().set_ref("refs/heads/main", target)?;
    }
    Ok(report)
}
