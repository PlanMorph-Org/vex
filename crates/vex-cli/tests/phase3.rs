#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! Phase 3 integration tests: geometry hashing via CLI + log rendering formats.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-p3-{tag}-{}",
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

const BLOCK_BASE: &str = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = IFCCARTESIANPOINT((0.0, 0.0, 0.0));
#2 = IFCAXIS2PLACEMENT3D(#1, $, $);
#3 = IFCBLOCK(#2, 1.0, 2.0, 3.0);
ENDSEC;
END-ISO-10303-21;
";

fn head_hash(repo: &Path) -> String {
    let out = vex_ok(repo, &["--json", "log"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("log json");
    v[0]["commit"].as_str().expect("commit hash").to_string()
}

#[test]
fn geometry_change_shows_in_diff() {
    let repo = tempdir("geom");
    let base_path = repo.join("base.ifc");
    let bigger_path = repo.join("bigger.ifc");
    std::fs::write(&base_path, BLOCK_BASE).unwrap();
    std::fs::write(
        &bigger_path,
        BLOCK_BASE.replace("1.0, 2.0, 3.0", "1.5, 2.0, 3.0"),
    )
    .unwrap();

    vex_ok(&repo, &["init"]);
    vex_ok(&repo, &["import", base_path.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v1"]);
    let c1 = head_hash(&repo);
    vex_ok(&repo, &["import", bigger_path.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v2"]);
    let c2 = head_hash(&repo);

    let out = vex_ok(&repo, &["--json", "diff", &c1, &c2]);
    let report: serde_json::Value = serde_json::from_str(&out).expect("json");
    let modified = report["summary"]["modified"].as_u64().unwrap_or(0);
    let added = report["summary"]["added"].as_u64().unwrap_or(0);
    let removed = report["summary"]["removed"].as_u64().unwrap_or(0);
    assert!(
        modified + added + removed >= 1,
        "expected at least one change in geometry, got report: {report:#}"
    );
}

#[test]
fn geometry_noise_within_tolerance_yields_empty_diff() {
    let repo = tempdir("geom-noise");
    let base_path = repo.join("base.ifc");
    let noisy_path = repo.join("noisy.ifc");
    std::fs::write(&base_path, BLOCK_BASE).unwrap();
    // Noise well below the default 1 µm linear tolerance.
    std::fs::write(
        &noisy_path,
        BLOCK_BASE.replace("1.0, 2.0, 3.0", "1.00000001, 2.0, 3.0"),
    )
    .unwrap();

    vex_ok(&repo, &["init"]);
    vex_ok(&repo, &["import", base_path.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v1"]);
    let c1 = head_hash(&repo);
    vex_ok(&repo, &["import", noisy_path.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v2"]);
    let c2 = head_hash(&repo);

    let out = vex_ok(&repo, &["--json", "diff", &c1, &c2]);
    let report: serde_json::Value = serde_json::from_str(&out).expect("json");
    let modified = report["summary"]["modified"].as_u64().unwrap_or(0);
    let added = report["summary"]["added"].as_u64().unwrap_or(0);
    let removed = report["summary"]["removed"].as_u64().unwrap_or(0);
    assert_eq!(
        (modified, added, removed),
        (0, 0, 0),
        "expected empty diff for sub-tolerance noise, got: {report:#}"
    );
}

#[test]
fn log_renders_mermaid_and_dot() {
    let repo = tempdir("log");
    let fixture = workspace_root().join("examples/tiny.min.ifc");
    vex_ok(&repo, &["init"]);
    vex_ok(&repo, &["import", fixture.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "first"]);

    let m = vex_ok(&repo, &["log", "--format", "mermaid"]);
    assert!(m.starts_with("graph TD"), "mermaid output: {m}");

    let d = vex_ok(&repo, &["log", "--format", "dot"]);
    assert!(d.starts_with("digraph vex"), "dot output: {d}");
    assert!(d.contains("rankdir=LR"));
}
