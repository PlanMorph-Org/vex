#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! End-to-end integration test of the vex remote protocol.
//!
//! Exercises [`serve_session`] across an in-memory full-duplex pipe with a
//! hand-written client. This proves the wire protocol round-trips a real
//! repository (init, commit, list-refs, fetch, push) before we wire up the
//! CLI client in Phase 1C.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use vex_core::Repository;
use vex_protocol::{read_frame, write_frame, Frame, PackEntry, UpdateRefStatus, PROTOCOL_VERSION};
use vex_serve::{serve_session, ServeConfig};
use vex_storage::{Blob, ObjectKind};
use vex_utils::Hash256;

/// Half-duplex byte pipe built on `mpsc::channel`. Each direction needs one
/// pipe; client.write() reads in server.read() and vice versa.
struct PipeReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
}
struct PipeWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

fn pipe() -> (PipeReader, PipeWriter) {
    let (tx, rx) = mpsc::channel();
    (
        PipeReader {
            rx,
            buf: Vec::new(),
        },
        PipeWriter { tx },
    )
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.buf.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.buf = chunk,
                Err(_) => return Ok(0), // sender dropped — clean EOF
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.tx
            .send(b.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed"))?;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Build a tiny but real repository on disk: init + put a Blob + set a ref.
/// Returns (repo working dir, the blob hash, the ref name pointing at it).
fn seed_repo(root: &std::path::Path) -> (PathBuf, Hash256) {
    // The repo lives at <root>/acme/tower.
    let repo_dir = root.join("acme").join("tower");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let repo = Repository::init(&repo_dir).unwrap();
    let blob = Blob {
        type_name: "IFCWALL".into(),
        step_id: 1,
        global_id: Some("0O2Fr$t4X7Zf8NOew3FNr2".into()),
        props: vec![],
    };
    let h = repo.store().put_blob(&blob).unwrap();
    repo.store().set_ref("refs/heads/main", h).unwrap();
    (repo_dir, h)
}

// ---------------------------------------------------------------------------
// Real harness: runs the server in a thread and drives it from the test.
// ---------------------------------------------------------------------------

/// Spawn a `serve_session` against a temp repo, returning the handles the
/// test needs to talk to it as a client. Joins the server thread on drop.
struct Harness {
    client_writer: Option<PipeWriter>,
    client_reader: PipeReader,
    server: Option<thread::JoinHandle<()>>,
    _tmp: tempfile::TempDir,
}

impl Harness {
    fn writer(&mut self) -> &mut PipeWriter {
        self.client_writer.as_mut().expect("writer dropped")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Close the write side so the server's `read_frame` returns EOF and the
        // session exits cleanly. *Then* it is safe to join.
        self.client_writer.take();
        if let Some(h) = self.server.take() {
            let _ = h.join();
        }
    }
}

fn start_server(cfg: ServeConfig) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(tmp.path());
    start_server_with_root(cfg, tmp)
}

fn start_server_with_root(mut cfg: ServeConfig, tmp: tempfile::TempDir) -> Harness {
    cfg.repo_root = tmp.path().to_path_buf();
    let (server_in, client_writer) = pipe();
    let (client_reader, server_out) = pipe();
    let cfg_clone = cfg.clone();
    let server = thread::spawn(move || {
        let mut r = server_in;
        let mut w = server_out;
        let _ = serve_session(&cfg_clone, &mut r, &mut w);
    });
    Harness {
        client_writer: Some(client_writer),
        client_reader,
        server: Some(server),
        _tmp: tmp,
    }
}

fn handshake(h: &mut Harness, repo: &str) {
    write_frame(
        h.writer(),
        &Frame::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: "test-client".into(),
            repo: repo.into(),
        },
    )
    .unwrap();
    match read_frame(&mut h.client_reader).unwrap() {
        Frame::HelloOk { protocol, .. } => assert_eq!(protocol, PROTOCOL_VERSION),
        other => panic!("expected HelloOk, got {other:?}"),
    }
}

#[test]
fn full_handshake_and_ls_refs() {
    let cfg = ServeConfig::new("/replaced");
    let mut h = start_server(cfg);
    handshake(&mut h, "acme/tower");

    write_frame(h.writer(), &Frame::LsRefs).unwrap();
    let refs = match read_frame(&mut h.client_reader).unwrap() {
        Frame::Refs(r) => r,
        other => panic!("expected Refs, got {other:?}"),
    };
    let names: Vec<&str> = refs.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"refs/heads/main"));
}

#[test]
fn fetch_streams_pack_for_seeded_blob() {
    let cfg = ServeConfig::new("/replaced");
    let mut h = start_server(cfg);
    handshake(&mut h, "acme/tower");

    // Discover what to want.
    write_frame(h.writer(), &Frame::LsRefs).unwrap();
    let refs = match read_frame(&mut h.client_reader).unwrap() {
        Frame::Refs(r) => r,
        other => panic!("expected Refs, got {other:?}"),
    };
    let (_, target) = refs
        .into_iter()
        .find(|(n, _)| n == "refs/heads/main")
        .expect("ref present");

    // Negotiate a fetch.
    write_frame(h.writer(), &Frame::Want(vec![target])).unwrap();
    write_frame(h.writer(), &Frame::Have(vec![])).unwrap();
    write_frame(h.writer(), &Frame::Done).unwrap();

    // Drain the pack.
    let start = read_frame(&mut h.client_reader).unwrap();
    let entry_count = match start {
        Frame::PackStart { entry_count } => entry_count,
        other => panic!("expected PackStart, got {other:?}"),
    };
    assert_eq!(entry_count, 1, "seeded blob should be the only object");

    let mut received: Vec<PackEntry> = Vec::new();
    loop {
        match read_frame(&mut h.client_reader).unwrap() {
            Frame::PackChunk(es) => received.extend(es),
            Frame::PackEnd => break,
            other => panic!("expected PackChunk/PackEnd, got {other:?}"),
        }
    }
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].hash, target);
    // The framed bytes are self-validating; the receiver would normally call
    // `put_framed` which re-checks the hash. We just sanity-check the prefix.
    assert!(received[0].bytes.starts_with(b"VEX0"));
}

#[test]
fn push_round_trips_a_new_blob_and_advances_a_branch() {
    use vex_storage::object;

    let cfg = ServeConfig::new("/replaced");
    let mut h = start_server(cfg);
    handshake(&mut h, "acme/tower");

    // Build a blob locally and frame it the same way the server would.
    let blob = Blob {
        type_name: "IFCSLAB".into(),
        step_id: 99,
        global_id: Some("9newSlab9newSlab9newSl".into()),
        props: vec![],
    };
    let payload = bincode::serialize(&blob).unwrap();
    let (hash, framed) = object::encode(
        ObjectKind::Blob,
        &payload,
        vex_utils::hash::HashAlgo::DEFAULT,
    )
    .expect("encode");

    // Push it.
    write_frame(h.writer(), &Frame::PackStart { entry_count: 1 }).unwrap();
    write_frame(
        h.writer(),
        &Frame::PackChunk(vec![PackEntry {
            hash,
            bytes: framed,
        }]),
    )
    .unwrap();
    write_frame(h.writer(), &Frame::PackEnd).unwrap();

    // Now point a brand-new branch at it.
    write_frame(
        h.writer(),
        &Frame::UpdateRef {
            name: "refs/heads/feature-x".into(),
            expected_old: None, // create-if-absent
            new: hash,
        },
    )
    .unwrap();
    match read_frame(&mut h.client_reader).unwrap() {
        Frame::UpdateRefAck { name, status } => {
            assert_eq!(name, "refs/heads/feature-x");
            assert_eq!(status, UpdateRefStatus::Ok);
        }
        other => panic!("expected UpdateRefAck, got {other:?}"),
    }

    // CAS again with a stale precondition — must conflict.
    write_frame(
        h.writer(),
        &Frame::UpdateRef {
            name: "refs/heads/feature-x".into(),
            expected_old: None,
            new: hash,
        },
    )
    .unwrap();
    match read_frame(&mut h.client_reader).unwrap() {
        Frame::UpdateRefAck { status, .. } => match status {
            UpdateRefStatus::Conflict { actual } => {
                assert_eq!(actual, Some(hash));
            }
            other => panic!("expected Conflict, got {other:?}"),
        },
        other => panic!("expected UpdateRefAck, got {other:?}"),
    }

    // And updating a ref to point at an unknown object must be Rejected.
    let bogus = Hash256::from_bytes([0xAB; 32]);
    write_frame(
        h.writer(),
        &Frame::UpdateRef {
            name: "refs/heads/feature-y".into(),
            expected_old: None,
            new: bogus,
        },
    )
    .unwrap();
    match read_frame(&mut h.client_reader).unwrap() {
        Frame::UpdateRefAck { status, .. } => match status {
            UpdateRefStatus::Rejected { reason } => {
                assert!(reason.contains("missing"), "reason: {reason}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        },
        other => panic!("expected UpdateRefAck, got {other:?}"),
    }

    // Use ObjectKind to silence the unused import on some configurations.
    let _ = ObjectKind::Blob;
}

#[test]
fn unknown_repo_yields_error_frame() {
    let cfg = ServeConfig::new("/replaced");
    let mut h = start_server(cfg);
    write_frame(
        h.writer(),
        &Frame::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: "test".into(),
            repo: "acme/does-not-exist".into(),
        },
    )
    .unwrap();
    match read_frame(&mut h.client_reader).unwrap() {
        Frame::Error { message } => assert!(message.contains("does not exist")),
        other => panic!("expected Error, got {other:?}"),
    }
}
