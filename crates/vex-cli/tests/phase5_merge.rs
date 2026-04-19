//! Phase 5 — `vex merge` end-to-end: up-to-date, fast-forward, clean,
//! conflicts, --strategy and --ff-only.

use std::path::{Path, PathBuf};
use std::process::Command;

use vex_core::Repository;
use vex_storage::{Commit, Identity};

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-p5-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn vex(repo: &Path, args: &[&str]) -> (bool, String, String) {
    let bin = env!("CARGO_BIN_EXE_vex");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn vex");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn vex_ok(repo: &Path, args: &[&str]) -> String {
    let (ok, out, err) = vex(repo, args);
    assert!(ok, "vex {args:?} failed: out={out} err={err}");
    out
}

/// Build a divergent branch by importing `ifc` and then writing a commit whose
/// parent is `parent` (rather than HEAD). Returns the new commit hash.
fn divergent_commit(
    repo_path: &Path,
    ifc: &Path,
    parent: vex_utils::Hash256,
    branch_ref: &str,
    msg: &str,
) -> vex_utils::Hash256 {
    // Import via CLI to populate the staging tree under the same store schema.
    vex_ok(repo_path, &["import", ifc.to_str().unwrap()]);
    let repo = Repository::open(repo_path).unwrap();
    let tree = repo.staged_tree().unwrap().expect("staged tree");
    let commit = Commit {
        tree,
        parents: vec![parent],
        author: Identity {
            name: "test".into(),
            email: "test@vex".into(),
        },
        committer: Identity {
            name: "vex".into(),
            email: "system@vex".into(),
        },
        timestamp: 0,
        message: msg.into(),
        signature: None,
        profile_hash: repo.profile().hash(),
    };
    let h = repo.store().put_commit(&commit).unwrap();
    repo.store().set_ref(branch_ref, h).unwrap();
    h
}

fn seed_base(tag: &str) -> (PathBuf, vex_utils::Hash256) {
    let repo_path = tempdir(tag);
    let fixture = workspace_root().join("examples/tiny.min.ifc");
    vex_ok(&repo_path, &["init"]);
    vex_ok(&repo_path, &["import", fixture.to_str().unwrap()]);
    vex_ok(&repo_path, &["commit", "-m", "base"]);
    let repo = Repository::open(&repo_path).unwrap();
    let base = repo.resolve_head().unwrap().unwrap();
    (repo_path, base)
}

#[test]
fn merge_up_to_date() {
    let (repo_path, _) = seed_base("uptodate");
    let out = vex_ok(&repo_path, &["merge", "main", "main"]);
    assert!(
        out.contains("Already up to date"),
        "expected up-to-date, got: {out}"
    );
}

#[test]
fn merge_fast_forward() {
    let (repo_path, base) = seed_base("ff");
    // Advance main to v2.
    let v2 = workspace_root().join("examples/tiny-v2.min.ifc");
    vex_ok(&repo_path, &["import", v2.to_str().unwrap()]);
    vex_ok(&repo_path, &["commit", "-m", "v2"]);
    // Create a branch pointing at base — main is now strictly ahead of it.
    {
        let repo = Repository::open(&repo_path).unwrap();
        repo.store().set_ref("refs/heads/old", base).unwrap();
    }
    // Merging main into old must fast-forward old → main.
    // We invoke as `merge old main` since CLI applies HEAD/main update semantics
    // identically; verify outcome string only.
    let out = vex_ok(&repo_path, &["merge", "old", "main"]);
    assert!(
        out.contains("Fast-forward to"),
        "expected fast-forward, got: {out}"
    );
}

#[test]
fn merge_clean_requires_strategy_then_creates() {
    let (repo_path, base) = seed_base("clean");
    // Side A on main: tiny-v2 (Wall-A → Wall-B, +Column).
    let v2 = workspace_root().join("examples/tiny-v2.min.ifc");
    vex_ok(&repo_path, &["import", v2.to_str().unwrap()]);
    vex_ok(&repo_path, &["commit", "-m", "ours"]);
    // Side B: tiny-merge-theirs as a divergent commit off `base`.
    let theirs_ifc = workspace_root().join("examples/tiny-merge-theirs.min.ifc");
    let _theirs = divergent_commit(&repo_path, &theirs_ifc, base, "refs/heads/theirs", "theirs");

    // Without --strategy: clean but no commit recorded.
    let out = vex_ok(&repo_path, &["merge", "main", "theirs"]);
    assert!(
        out.contains("Clean merge") || out.contains("re-run with --strategy"),
        "expected clean-without-commit message, got: {out}"
    );

    // With --strategy=ours: records a 2-parent commit and advances HEAD.
    let head_before = {
        let repo = Repository::open(&repo_path).unwrap();
        repo.resolve_head().unwrap().unwrap()
    };
    let out = vex_ok(
        &repo_path,
        &["merge", "main", "theirs", "--strategy", "ours", "-m", "M"],
    );
    assert!(
        out.contains("Merge commit"),
        "expected merge-commit message, got: {out}"
    );
    let head_after = {
        let repo = Repository::open(&repo_path).unwrap();
        repo.resolve_head().unwrap().unwrap()
    };
    assert_ne!(head_before, head_after, "HEAD must advance");
    // The new commit must have two parents.
    let repo = Repository::open(&repo_path).unwrap();
    let c = repo.store().get_commit(head_after).unwrap();
    assert_eq!(c.parents.len(), 2, "merge commit must have two parents");
}

#[test]
fn merge_ff_only_refuses_non_ff() {
    let (repo_path, base) = seed_base("ffonly");
    // Diverge: main → v2; theirs → tiny-merge-theirs off base.
    let v2 = workspace_root().join("examples/tiny-v2.min.ifc");
    vex_ok(&repo_path, &["import", v2.to_str().unwrap()]);
    vex_ok(&repo_path, &["commit", "-m", "ours"]);
    let theirs_ifc = workspace_root().join("examples/tiny-merge-theirs.min.ifc");
    let _ = divergent_commit(&repo_path, &theirs_ifc, base, "refs/heads/theirs", "theirs");

    let (ok, out, err) = vex(&repo_path, &["merge", "main", "theirs", "--ff-only"]);
    assert!(!ok, "--ff-only must refuse, but succeeded: {out}");
    assert!(
        err.contains("ff-only") || err.contains("non-fast-forward"),
        "expected ff-only diagnostic, got err={err} out={out}"
    );
}
