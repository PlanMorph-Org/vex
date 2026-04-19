//! Unified error type for the Vex workspace.
//!
//! Every crate is encouraged to re-export `VexResult` and map its own domain
//! errors into `VexError` variants via `#[from]` conversions where appropriate.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// The unified error type for Vex operations.
#[derive(Debug, Error)]
pub enum VexError {
    #[error("I/O error at {path:?}: {source}")]
    Io {
        path: Option<PathBuf>,
        #[source]
        source: io::Error,
    },

    #[error("IFC parse error at line {line}, column {column}: {message}")]
    Parse {
        line: u32,
        column: u32,
        message: String,
    },

    #[error("IFC parse limit exceeded: {0}")]
    ParseLimit(String),

    #[error("graph invariant violated: {0}")]
    Graph(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("object not found: {0}")]
    NotFound(String),

    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("invalid reference: {0}")]
    InvalidRef(String),

    #[error("unsupported IFC schema: {0}")]
    UnsupportedSchema(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("other: {0}")]
    Other(String),
}

impl From<io::Error> for VexError {
    fn from(source: io::Error) -> Self {
        VexError::Io {
            path: None,
            source,
        }
    }
}

/// Short alias used throughout the workspace.
pub type VexResult<T> = Result<T, VexError>;

impl VexError {
    #[must_use]
    pub fn io_at(path: impl Into<PathBuf>, source: io::Error) -> Self {
        VexError::Io {
            path: Some(path.into()),
            source,
        }
    }

    #[must_use]
    pub fn parse(line: u32, column: u32, message: impl Into<String>) -> Self {
        VexError::Parse {
            line,
            column,
            message: message.into(),
        }
    }
}
