#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! End-to-end integration test driving the compiled `vex` binary.
//!
//! This is a `#[test]` harness that shells out to `cargo run --bin vex`
//! so the entire CLI surface is exercised, not just the library API.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the vex-cli crate; walk up to the workspace root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-cli-it-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn vex(repo: &Path, args: &[&str]) -> String {
    // Use the binary that cargo test built.
    // `CARGO_BIN_EXE_vex` is set automatically for integration tests of a
    // crate whose `[[bin]]` is named "vex".
    let bin = env!("CARGO_BIN_EXE_vex");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn vex");
    assert!(
        output.status.success(),
        "vex {args:?} failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn end_to_end_workflow() {
    let root = workspace_root();
    let fixture_a = root.join("examples/tiny.min.ifc");
    let fixture_b = root.join("examples/tiny-v2.min.ifc");
    assert!(fixture_a.is_file());
    assert!(fixture_b.is_file());

    let repo = tempdir("e2e");

    // init
    let out = vex(&repo, &["init"]);
    assert!(out.contains("Initialized"));

    // import + commit v1
    vex(&repo, &["import", fixture_a.to_str().unwrap()]);
    let commit1 = vex(&repo, &["commit", "-m", "v1"]);
    assert!(commit1.contains("committed"));

    // import + commit v2
    vex(&repo, &["import", fixture_b.to_str().unwrap()]);
    let commit2 = vex(&repo, &["commit", "-m", "v2"]);
    assert!(commit2.contains("committed"));

    // log shows both
    let log = vex(&repo, &["log"]);
    assert_eq!(log.matches("commit ").count(), 2, "log: {log}");

    // diff between refs
    let diff_out = vex(&repo, &["diff", "HEAD", "HEAD"]);
    assert!(
        diff_out.contains("0 added, 0 removed, 0 modified"),
        "diff HEAD HEAD: {diff_out}"
    );

    // JSON log has 2 entries
    let log_json = vex(&repo, &["--json", "log"]);
    let v: serde_json::Value = serde_json::from_str(&log_json).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);

    // verify succeeds
    let verify = vex(&repo, &["verify"]);
    assert!(verify.contains("verified"));
}

#[test]
fn diff_between_two_revisions_reports_changes() {
    let root = workspace_root();
    let fixture_a = root.join("examples/tiny.min.ifc");
    let fixture_b = root.join("examples/tiny-v2.min.ifc");

    let repo = tempdir("diff");
    vex(&repo, &["init"]);

    vex(&repo, &["import", fixture_a.to_str().unwrap()]);
    vex(&repo, &["commit", "-m", "v1"]);

    // Capture HEAD before second commit.
    let log_json = vex(&repo, &["--json", "log"]);
    let v: serde_json::Value = serde_json::from_str(&log_json).unwrap();
    let v1_hash = v[0]["commit"].as_str().unwrap().to_string();

    vex(&repo, &["import", fixture_b.to_str().unwrap()]);
    vex(&repo, &["commit", "-m", "v2"]);

    let log_json = vex(&repo, &["--json", "log"]);
    let v: serde_json::Value = serde_json::from_str(&log_json).unwrap();
    let v2_hash = v[0]["commit"].as_str().unwrap().to_string();

    let diff_json = vex(&repo, &["--json", "diff", &v1_hash, &v2_hash]);
    let report: serde_json::Value = serde_json::from_str(&diff_json).unwrap();
    let summary = &report["summary"];
    let added = summary["added"].as_u64().unwrap();
    let removed = summary["removed"].as_u64().unwrap();
    let modified = summary["modified"].as_u64().unwrap();

    // Wall-A → Wall-B is a property change, new Column is an add.
    assert!(modified >= 1, "report: {report:#}");
    assert!(added >= 1, "report: {report:#}");
    assert_eq!(removed, 0, "report: {report:#}");
}
