//! Repository orchestrator — the high-level API the CLI drives.

pub mod repo;
pub mod signing;

pub use repo::{MergeOutcome, MergeStrategy, Repository, Status, StatusSummary};
pub use signing::{generate_key, list_keys, sign_commit, verify_commit, SIGNATURE_ALGO};
