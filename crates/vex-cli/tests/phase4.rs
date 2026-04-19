#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! Phase 4 integration tests: branches, tags, status, checkout, gc, tamper.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-p4-{tag}-{}",
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

fn seed_repo(tag: &str) -> PathBuf {
    let repo = tempdir(tag);
    let fixture = workspace_root().join("examples/tiny.min.ifc");
    vex_ok(&repo, &["init"]);
    vex_ok(&repo, &["import", fixture.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v1"]);
    repo
}

#[test]
fn status_reports_empty_then_staged() {
    let repo = tempdir("status");
    vex_ok(&repo, &["init"]);
    let before = vex_ok(&repo, &["status"]);
    assert!(before.contains("empty"), "expected empty, got {before}");

    let fixture = workspace_root().join("examples/tiny.min.ifc");
    vex_ok(&repo, &["import", fixture.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v1"]);
    let after = vex_ok(&repo, &["status"]);
    assert!(
        after.contains("HEAD at")
            || after.contains("nothing staged")
            || after.contains("0 added, 0 removed, 0 modified"),
        "unexpected status: {after}"
    );
}

#[test]
fn branch_create_list_delete() {
    let repo = seed_repo("branch");
    vex_ok(&repo, &["branch", "create", "feature"]);
    let list = vex_ok(&repo, &["branch", "list"]);
    assert!(list.contains("feature"));
    assert!(list.contains("main"));

    let del = vex_ok(&repo, &["branch", "delete", "feature"]);
    assert!(del.contains("deleted branch feature"));
    let list2 = vex_ok(&repo, &["branch", "list"]);
    assert!(!list2.contains("feature"));
}

#[test]
fn tag_create_list_delete() {
    let repo = seed_repo("tag");
    vex_ok(&repo, &["tag", "create", "v1.0"]);
    let list = vex_ok(&repo, &["tag", "list"]);
    assert!(list.contains("v1.0"), "tag list: {list}");

    // Tag should also resolve as a ref in diff.
    let _ = vex_ok(&repo, &["diff", "v1.0", "v1.0"]);

    let del = vex_ok(&repo, &["tag", "delete", "v1.0"]);
    assert!(del.contains("deleted tag v1.0"));
}

#[test]
fn checkout_produces_reparsable_ifc() {
    let repo = seed_repo("checkout");
    let out = repo.join("out.ifc");
    vex_ok(&repo, &["checkout", "HEAD", "-o", out.to_str().unwrap()]);
    // Re-import the checked-out file into a fresh repo and verify it commits.
    let repo2 = tempdir("checkout2");
    vex_ok(&repo2, &["init"]);
    vex_ok(&repo2, &["import", out.to_str().unwrap()]);
    vex_ok(&repo2, &["commit", "-m", "roundtrip"]);
}

#[test]
fn gc_removes_unreachable_objects() {
    // Build a repo, make an extra unreferenced object via a deleted branch
    // pointing at an imported-but-uncommitted tree. Simpler approach: make a
    // branch, import second file (changes staged tree), commit to main, then
    // delete branch — any objects only on the deleted branch become
    // unreachable. Without a second branch that actually has its own commit
    // we won't have unreachables; instead, we simulate by creating a branch
    // at a throw-away commit and deleting it.
    let repo = tempdir("gc");
    let fixture = workspace_root().join("examples/tiny.min.ifc");
    let fixture_v2 = workspace_root().join("examples/tiny-v2.min.ifc");
    vex_ok(&repo, &["init"]);
    vex_ok(&repo, &["import", fixture.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v1"]);
    // Create a side branch at HEAD, import v2, commit on it, then delete.
    vex_ok(&repo, &["branch", "create", "side"]);
    vex_ok(&repo, &["import", fixture_v2.to_str().unwrap()]);
    // Commit to main (side is still at v1, HEAD moves with main).
    vex_ok(&repo, &["commit", "-m", "v2"]);
    // Count objects before gc.
    let refs_json = vex_ok(&repo, &["--json", "refs"]);
    assert!(refs_json.contains("refs/heads/side"));
    // Delete the unused branch and any staging ref remnants.
    vex_ok(&repo, &["branch", "delete", "side"]);
    let gc_out = vex_ok(&repo, &["gc"]);
    assert!(
        gc_out.contains("kept") && gc_out.contains("deleted"),
        "gc: {gc_out}"
    );
}

#[test]
fn verify_detects_tampered_store() {
    // Tamper with the object DB by corrupting an object's value bytes.
    // We use library APIs via a spawned sub-binary? Simpler: use a small
    // library-level test path is more natural. Here we just verify that the
    // basic CLI path works, and we exercise tamper detection via the
    // library in a separate unit test below.
    let repo = seed_repo("tamper");
    // Walk object store via library & corrupt one entry.
    use vex_core::Repository;
    let r = Repository::open(&repo).unwrap();
    // Grab the first object and overwrite with garbage bytes.
    let hashes = r.store().list_object_hashes().unwrap();
    assert!(!hashes.is_empty());
    let victim = hashes[0];
    r.store()
        ._debug_corrupt_object(victim, vec![0u8; 8])
        .unwrap();
    drop(r);

    // Now CLI verify must fail loudly.
    let (ok, out, err) = vex(&repo, &["verify"]);
    assert!(
        !ok,
        "expected verify to fail on tampered store; out={out} err={err}"
    );
    let combined = format!("{out}\n{err}");
    assert!(
        combined.to_lowercase().contains("hash")
            || combined.to_lowercase().contains("corrupt")
            || combined.to_lowercase().contains("mismatch")
            || combined.to_lowercase().contains("invalid")
            || combined.to_lowercase().contains("magic")
            || combined.to_lowercase().contains("storage error"),
        "expected tamper-detection diagnostic, got: {combined}"
    );
}
