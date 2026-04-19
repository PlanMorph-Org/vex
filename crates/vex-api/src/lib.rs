//! User-verb façade over [`vex_core::Repository`].
//!
//! The CLI keeps Git verbs (`commit`, `merge`, `diff`, `log`) because that's
//! what developers expect. This crate is what plugin hosts (Revit/Archicad/
//! Tekla) and the future web viewer call instead — the verbs are renamed to
//! match how end-users actually think about a building model.
//!
//! | Internal (`vex-core`) | User-facing (`vex-api`) |
//! |---|---|
//! | `Repository::commit`         | `VexProject::save_version`     |
//! | `Repository::diff_refs`      | `VexProject::compare`          |
//! | `Repository::log`            | `VexProject::timeline`         |
//! | `Repository::merge_and_commit` | `VexProject::combine_changes` |
//!
//! All return values are JSON-serializable so the same struct round-trips
//! through `vex compare --json` today and the future `vex-bridge` FFI
//! tomorrow.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vex_core::Repository;
use vex_utils::{Hash256, VexResult};
pub use vex_visual_diff::{ChangeKind, Counts, ElementChange, VisualDiff};

/// One entry in the project [`timeline`](VexProject::timeline).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    /// Full hex commit hash.
    pub commit: String,
    /// Short (12-char) prefix for display.
    pub short: String,
    pub message: String,
    pub author: String,
    pub email: String,
    /// Unix timestamp seconds.
    pub timestamp: i64,
    /// Parent commit hashes (empty for the root).
    pub parents: Vec<String>,
}

/// User-verb facade over [`Repository`]. Cheap to construct, holds no extra
/// state beyond the wrapped repo.
#[derive(Debug)]
pub struct VexProject {
    repo: Repository,
    root: PathBuf,
}

impl VexProject {
    /// Initialize a new project at `path`.
    pub fn init(path: impl AsRef<Path>) -> VexResult<Self> {
        let repo = Repository::init(path.as_ref())?;
        Ok(Self {
            root: path.as_ref().to_path_buf(),
            repo,
        })
    }

    /// Open an existing project (looks up `.vex/` from `path`).
    pub fn open(path: impl AsRef<Path>) -> VexResult<Self> {
        let repo = Repository::open(path.as_ref())?;
        Ok(Self {
            root: path.as_ref().to_path_buf(),
            repo,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Underlying repository, for advanced use or migration to direct
    /// `vex-core` calls.
    #[must_use]
    pub fn repository(&self) -> &Repository {
        &self.repo
    }

    /// Stage `ifc_path` as the current model.
    pub fn import_model(&self, ifc_path: impl AsRef<Path>) -> VexResult<Hash256> {
        self.repo.import(ifc_path)
    }

    /// "Save Version" — commits the staged model. Equivalent to
    /// `Repository::commit`.
    pub fn save_version(
        &self,
        message: impl Into<String>,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
    ) -> VexResult<String> {
        let h = self.repo.commit(message, author_name, author_email)?;
        Ok(h.to_hex())
    }

    /// "View Changes" — produce a [`VisualDiff`] between two refs (or commit
    /// hashes). The summary string is filled in via `vex-summary`.
    pub fn compare(&self, from: &str, to: &str) -> VexResult<VisualDiff> {
        let report = self.repo.diff_refs(from, to)?;
        let mut visual = vex_visual_diff::classify(&report, from, to);
        visual.summary = vex_summary::render(&visual);
        Ok(visual)
    }

    /// "Timeline" — every saved version reachable from HEAD, newest first.
    pub fn timeline(&self) -> VexResult<Vec<Version>> {
        let entries = self.repo.log()?;
        Ok(entries
            .into_iter()
            .map(|(h, c)| {
                let hex = h.to_hex();
                Version {
                    short: hex[..12.min(hex.len())].to_string(),
                    commit: hex,
                    message: c.message,
                    author: c.author.name,
                    email: c.author.email,
                    timestamp: c.timestamp,
                    parents: c.parents.iter().map(Hash256::to_hex).collect(),
                }
            })
            .collect())
    }

    /// Convenience: changes between the previous version and HEAD. Returns
    /// `None` when HEAD has no parent (i.e. this is the first version).
    pub fn changes_since_last(&self) -> VexResult<Option<VisualDiff>> {
        let Some(head) = self.repo.resolve_head()? else {
            return Ok(None);
        };
        let commit = self.repo.store().get_commit(head)?;
        let parent = match commit.parents.first() {
            Some(p) => *p,
            None => return Ok(None),
        };
        let visual = self.compare(&parent.to_hex(), &head.to_hex())?;
        Ok(Some(visual))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn workspace_root() -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest.parent().unwrap().parent().unwrap().to_path_buf()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "vex-api-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn save_version_then_timeline() {
        let dir = tempdir("save");
        let p = VexProject::init(&dir).unwrap();
        p.import_model(workspace_root().join("examples/tiny.min.ifc"))
            .unwrap();
        let h = p.save_version("v1", "alice", "a@example.com").unwrap();
        assert_eq!(h.len(), 64);
        let tl = p.timeline().unwrap();
        assert_eq!(tl.len(), 1);
        assert_eq!(tl[0].message, "v1");
        assert_eq!(tl[0].short.len(), 12);
    }

    #[test]
    fn compare_round_trip_through_json() {
        let dir = tempdir("compare");
        let p = VexProject::init(&dir).unwrap();
        p.import_model(workspace_root().join("examples/tiny.min.ifc"))
            .unwrap();
        let a = p.save_version("a", "x", "x@y").unwrap();
        p.import_model(workspace_root().join("examples/tiny-v2.min.ifc"))
            .unwrap();
        let b = p.save_version("b", "x", "x@y").unwrap();

        let v = p.compare(&a, &b).unwrap();
        assert!(!v.summary.is_empty());
        assert_eq!(v.counts.added, 1);
        // Wall name change → Renamed.
        assert_eq!(v.counts.renamed, 1);

        let json = serde_json::to_string(&v).unwrap();
        let back: VisualDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(back.elements.len(), v.elements.len());
        assert_eq!(back.summary, v.summary);
    }

    #[test]
    fn changes_since_last_none_at_root() {
        let dir = tempdir("first");
        let p = VexProject::init(&dir).unwrap();
        p.import_model(workspace_root().join("examples/tiny.min.ifc"))
            .unwrap();
        p.save_version("only", "x", "x@y").unwrap();
        assert!(p.changes_since_last().unwrap().is_none());
    }

    #[test]
    fn changes_since_last_some_after_two_versions() {
        let dir = tempdir("two");
        let p = VexProject::init(&dir).unwrap();
        p.import_model(workspace_root().join("examples/tiny.min.ifc"))
            .unwrap();
        p.save_version("a", "x", "x@y").unwrap();
        p.import_model(workspace_root().join("examples/tiny-v2.min.ifc"))
            .unwrap();
        p.save_version("b", "x", "x@y").unwrap();

        let v = p.changes_since_last().unwrap().expect("some");
        assert!(v.counts.total() > 0);
    }
}
