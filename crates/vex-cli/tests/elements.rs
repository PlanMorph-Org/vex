#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! `vex elements` — authoritative element inventory from a committed tree.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-elements-{tag}-{}",
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

fn seed(tag: &str) -> PathBuf {
    let repo = tempdir(tag);
    vex_ok(&repo, &["init"]);
    vex_ok(
        &repo,
        &[
            "import",
            workspace_root()
                .join("examples/tiny-v2.min.ifc")
                .to_str()
                .unwrap(),
        ],
    );
    vex_ok(&repo, &["commit", "-m", "v1"]);
    repo
}

#[test]
fn elements_json_reports_real_global_ids_and_names() {
    let repo = seed("json");
    let out = vex_ok(&repo, &["--json", "elements", "--rooted", "HEAD"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();

    assert_eq!(v["schema"], "vex.elements/1");
    assert!(v["commit"].as_str().unwrap().len() == 64);
    let elements = v["elements"].as_array().unwrap();
    assert!(!elements.is_empty(), "expected rooted elements");

    // Every rooted element must carry a real GlobalId (not a step-id stand-in).
    for e in elements {
        let gid = e["global_id"].as_str().unwrap();
        assert!(gid.len() >= 16, "global_id looks synthetic: {gid}");
        assert!(e["type_name"].as_str().unwrap().starts_with("IFC"));
    }

    // Authoritative human names are surfaced, not fabricated from step ids.
    let names: Vec<&str> = elements.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(
        names.contains(&"Wall-B"),
        "expected real IFC Name 'Wall-B', got {names:?}"
    );
}

#[test]
fn rooted_flag_drops_non_rooted_entities() {
    let repo = seed("rooted");
    let all: serde_json::Value =
        serde_json::from_str(&vex_ok(&repo, &["--json", "elements", "HEAD"])).unwrap();
    let rooted: serde_json::Value =
        serde_json::from_str(&vex_ok(&repo, &["--json", "elements", "--rooted", "HEAD"])).unwrap();
    let all_n = all["count"].as_u64().unwrap();
    let rooted_n = rooted["count"].as_u64().unwrap();
    assert!(
        rooted_n <= all_n,
        "rooted count {rooted_n} must not exceed total {all_n}"
    );
}
