//! Vex remote wire protocol.
//!
//! Vex repositories synchronise over a single duplex byte stream — typically
//! `ssh user@host vex-serve <org>/<project>`, where SSH provides
//! authentication, encryption, and transport. The protocol is intentionally
//! transport-agnostic: anything that yields a [`Read`]+[`Write`] pair (TCP
//! socket, Unix domain socket, in-memory pipe for tests) works.
//!
//! ## Frame format
//!
//! Each message is a length-prefixed bincode-serialized [`Frame`]:
//!
//! ```text
//! [ length: u32 LE ] [ frame: bincode(Frame) ]
//! ```
//!
//! Frames are bounded ([`MAX_FRAME_BYTES`] = 64 MiB) so a malicious peer
//! cannot exhaust memory by claiming a huge length. Pack data is split
//! across multiple [`Frame::PackChunk`] frames to stay under the cap.
//!
//! ## Conversation
//!
//! 1. Both sides exchange [`Frame::Hello`] / [`Frame::HelloOk`] for version
//!    handshake and to identify the target repository on the server side.
//! 2. The client requests a ref listing with [`Frame::LsRefs`]; the server
//!    replies with [`Frame::Refs`].
//! 3. Optional negotiation: client sends `Want`/`Have` to determine the
//!    smallest set of objects to transfer.
//! 4. Whichever side is sending objects emits a sequence of `PackChunk`
//!    frames terminated by [`Frame::PackEnd`].
//! 5. For pushes, the client sends [`Frame::UpdateRef`] (compare-and-set);
//!    the server replies with [`Frame::UpdateRefAck`] reporting `Ok` or
//!    `Conflict`.
//! 6. Either side may send [`Frame::Done`] (clean shutdown) or
//!    [`Frame::Error`] (fatal protocol/auth failure).
//!
//! Object transfer is content-addressed — each `PackEntry` carries the
//! exact framed bytes from `vex_storage::object::encode`, which already
//! include a self-validating header and zstd-compressed payload. The
//! receiver validates the hash before writing.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use vex_utils::Hash256;

/// Current wire protocol version. Bumped on incompatible frame changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on a single frame's serialized size (64 MiB). Larger payloads —
/// such as large packs — must be split into multiple [`Frame::PackChunk`]
/// frames. Bounding the per-frame size protects servers from memory abuse.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Errors raised by the framing layer.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame encoding failed: {0}")]
    Encode(String),
    #[error("frame decoding failed: {0}")]
    Decode(String),
    #[error("frame too large: {actual} bytes (max {max})")]
    FrameTooLarge { actual: u32, max: u32 },
    #[error("peer sent unexpected frame: {0}")]
    Unexpected(String),
    #[error("peer closed the stream prematurely")]
    UnexpectedEof,
    #[error("protocol mismatch: client {client}, server {server}")]
    VersionMismatch { client: u32, server: u32 },
}

pub type ProtocolResult<T> = Result<T, ProtocolError>;

/// One pack entry: framed object bytes (as produced by
/// `vex_storage::object::encode`) keyed by their content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    pub hash: Hash256,
    /// On-disk framed bytes. Self-validating: the receiver MUST recompute
    /// the hash and reject mismatches.
    pub bytes: Vec<u8>,
}

/// Result of a `UpdateRef` request on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateRefStatus {
    /// CAS succeeded; the ref now points at `target`.
    Ok,
    /// CAS failed: the ref's current value differed from `expected_old`.
    /// The server includes the actual current value (or `None` if absent).
    Conflict { actual: Option<Hash256> },
    /// Server policy rejected the update (e.g. ref name forbidden, signature
    /// required but missing, push not authorized for this ref).
    Rejected { reason: String },
}

/// Every wire message is a `Frame`. Add new variants only at the end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    /// Initial client → server greeting.
    Hello {
        protocol: u32,
        client_version: String,
        /// Target repository identifier, e.g. `"<org>/<project>"`.
        /// The server resolves this to a backend-configured `Repository`.
        repo: String,
    },
    /// Server → client acknowledgement.
    HelloOk {
        protocol: u32,
        server_version: String,
        /// Server-assigned opaque identifier (UUID string) for audit trails.
        repo_id: String,
        /// Capabilities the server supports (forward-compatible feature flags).
        capabilities: Vec<String>,
    },

    /// Client asks for the server's ref advertisement.
    LsRefs,
    /// Server's reply: `(name, target_hash)` pairs.
    Refs(Vec<(String, Hash256)>),

    /// Client (during fetch) tells server which commits it wants.
    Want(Vec<Hash256>),
    /// Client tells server which commits it already has, so the server can
    /// skip them and any reachable transitive objects.
    Have(Vec<Hash256>),
    /// Client signals end of negotiation; server may now send a pack.
    Done,

    /// Sender declares how many entries the upcoming pack contains. Optional —
    /// receivers must not require it.
    PackStart { entry_count: u64 },
    /// One slice of pack data. Multiple chunks may arrive between
    /// `PackStart` and `PackEnd`. Each chunk is a self-contained vector of
    /// entries so it is safe to interleave with progress reporting.
    PackChunk(Vec<PackEntry>),
    /// Sender has no more entries to transmit.
    PackEnd,

    /// Push: client asks the server to compare-and-set a ref atomically.
    UpdateRef {
        name: String,
        expected_old: Option<Hash256>,
        new: Hash256,
    },
    /// Server's response to an `UpdateRef`.
    UpdateRefAck {
        name: String,
        status: UpdateRefStatus,
    },

    /// Either side reports a fatal error and closes the conversation.
    Error { message: String },
}

impl Frame {
    /// Convenience: is this a terminal frame after which the conversation
    /// should end?
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Frame::Error { .. })
    }
}

/// Serialize a frame onto `w` using the length-prefix format.
/// Writes are flushed at the end of every frame so that small RPC frames
/// reach the peer immediately even when the writer wraps a `LineWriter`
/// (e.g. `std::io::stdout()` in non-tty mode). Tests against `Vec<u8>`
/// remain a no-op.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> ProtocolResult<()> {
    let body = bincode::serialize(frame).map_err(|e| ProtocolError::Encode(e.to_string()))?;
    let len = u32::try_from(body.len()).map_err(|_| ProtocolError::FrameTooLarge {
        actual: u32::MAX,
        max: MAX_FRAME_BYTES,
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            actual: len,
            max: MAX_FRAME_BYTES,
        });
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Read a single frame from `r`, validating the length cap.
pub fn read_frame<R: Read>(r: &mut R) -> ProtocolResult<Frame> {
    let mut len_buf = [0u8; 4];
    read_exact_eof(r, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            actual: len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut body = vec![0u8; len as usize];
    read_exact_eof(r, &mut body)?;
    bincode::deserialize::<Frame>(&body).map_err(|e| ProtocolError::Decode(e.to_string()))
}

/// `read_exact` that maps the EOF case to [`ProtocolError::UnexpectedEof`]
/// for clearer diagnostics when the peer drops the connection.
fn read_exact_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> ProtocolResult<()> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(ProtocolError::UnexpectedEof)
        }
        Err(e) => Err(ProtocolError::Io(e)),
    }
}

/// Default capabilities advertised by `vex-serve`. Clients use this set to
/// negotiate optional features (e.g. signed-only push enforcement).
#[must_use]
pub fn default_server_capabilities() -> Vec<String> {
    vec![
        "refs".into(),
        "fetch".into(),
        "push".into(),
        "cas-refs".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: &Frame) -> Frame {
        let mut buf = Vec::new();
        write_frame(&mut buf, frame).expect("encode");
        let mut cursor = std::io::Cursor::new(buf);
        read_frame(&mut cursor).expect("decode")
    }

    #[test]
    fn hello_roundtrips() {
        let f = Frame::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: "vex 0.1.0".into(),
            repo: "acme/tower".into(),
        };
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn refs_roundtrips() {
        let f = Frame::Refs(vec![
            ("refs/heads/main".into(), Hash256::from_bytes([7u8; 32])),
            ("HEAD".into(), Hash256::from_bytes([7u8; 32])),
        ]);
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn pack_chunk_roundtrips() {
        let f = Frame::PackChunk(vec![PackEntry {
            hash: Hash256::from_bytes([1u8; 32]),
            bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }]);
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn update_ref_cas_status_variants() {
        for status in [
            UpdateRefStatus::Ok,
            UpdateRefStatus::Conflict {
                actual: Some(Hash256::from_bytes([2u8; 32])),
            },
            UpdateRefStatus::Conflict { actual: None },
            UpdateRefStatus::Rejected {
                reason: "signed-only policy".into(),
            },
        ] {
            let f = Frame::UpdateRefAck {
                name: "refs/heads/main".into(),
                status,
            };
            assert_eq!(roundtrip(&f), f);
        }
    }

    #[test]
    fn oversized_frame_is_rejected_on_read() {
        // Hand-craft a length prefix that exceeds the cap, then ensure
        // read_frame refuses without allocating.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge { .. }));
    }

    #[test]
    fn truncated_stream_is_unexpected_eof() {
        // Length says 16 bytes, but we only provide 4.
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]);
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, ProtocolError::UnexpectedEof));
    }

    #[test]
    fn pipe_roundtrip_multiple_frames() {
        // Simulate a full mini-conversation through an in-memory pipe.
        let frames = vec![
            Frame::Hello {
                protocol: PROTOCOL_VERSION,
                client_version: "test".into(),
                repo: "x/y".into(),
            },
            Frame::HelloOk {
                protocol: PROTOCOL_VERSION,
                server_version: "test-server".into(),
                repo_id: "00000000-0000-0000-0000-000000000000".into(),
                capabilities: default_server_capabilities(),
            },
            Frame::LsRefs,
            Frame::Refs(vec![]),
            Frame::Done,
        ];
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).expect("encode");
        }
        let mut cursor = std::io::Cursor::new(buf);
        for expected in &frames {
            assert_eq!(&read_frame(&mut cursor).expect("decode"), expected);
        }
    }
}
