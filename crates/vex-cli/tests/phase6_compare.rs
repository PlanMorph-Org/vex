//! Phase 6 — `vex compare` and `vex changes` end-to-end.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-p6-{tag}-{}",
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

fn seed_two_versions(tag: &str) -> (PathBuf, String, String) {
    let repo = tempdir(tag);
    vex_ok(&repo, &["init"]);
    vex_ok(
        &repo,
        &["import", workspace_root().join("examples/tiny.min.ifc").to_str().unwrap()],
    );
    vex_ok(&repo, &["commit", "-m", "v1"]);
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
    vex_ok(&repo, &["commit", "-m", "v2"]);
    let log: serde_json::Value =
        serde_json::from_str(&vex_ok(&repo, &["--json", "log"])).unwrap();
    let to = log[0]["commit"].as_str().unwrap().to_string();
    let from = log[1]["commit"].as_str().unwrap().to_string();
    (repo, from, to)
}

#[test]
fn compare_text_summary_matches_expectation() {
    let (repo, from, to) = seed_two_versions("text");
    let out = vex_ok(&repo, &["compare", &from, &to]);
    // Expect: 1 wall renamed (Wall-A→Wall-B), 1 column added.
    assert!(
        out.contains("renamed") && out.contains("added"),
        "expected renamed+added in summary, got: {out}"
    );
    assert!(out.contains("wall"), "expected 'wall' in summary, got: {out}");
    assert!(
        out.contains("column"),
        "expected 'column' in summary, got: {out}"
    );
}

#[test]
fn compare_json_schema_is_stable() {
    let (repo, from, to) = seed_two_versions("json");
    let out = vex_ok(&repo, &["--json", "compare", &from, &to]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();

    // Top-level shape.
    for key in ["schema", "from", "to", "elements", "summary", "counts"] {
        assert!(v.get(key).is_some(), "missing top-level key {key}: {v}");
    }
    assert_eq!(v["schema"], "vex.visual-diff/1");
    let counts = &v["counts"];
    for key in ["added", "removed", "moved", "renamed", "modified"] {
        assert!(counts.get(key).is_some(), "missing counts.{key}: {v}");
    }

    assert_eq!(v["counts"]["added"], 1, "expected exactly one Added");
    assert_eq!(v["counts"]["renamed"], 1, "expected exactly one Renamed");
    assert_eq!(v["counts"]["removed"], 0);
    assert_eq!(v["counts"]["modified"], 0);
    assert_eq!(v["counts"]["moved"], 0);

    let elements = v["elements"].as_array().unwrap();
    assert_eq!(elements.len(), 2);

    // Each element carries: id, type_name, kind. deltas/hint optional per kind.
    let kinds: Vec<&str> = elements
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"added"));
    assert!(kinds.contains(&"renamed"));

    // Hints + deltas show up on the Renamed element only.
    let renamed = elements
        .iter()
        .find(|e| e["kind"] == "renamed")
        .unwrap();
    assert!(
        renamed["hint"]
            .as_str()
            .unwrap()
            .contains("Wall-A → Wall-B"),
        "expected rename hint, got: {renamed}"
    );
}

#[test]
fn changes_alias_works_after_two_commits() {
    let (repo, _from, _to) = seed_two_versions("changes");
    let out = vex_ok(&repo, &["changes"]);
    assert!(
        out.contains("renamed") || out.contains("added"),
        "unexpected changes output: {out}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&vex_ok(&repo, &["--json", "changes"])).unwrap();
    assert_eq!(json["counts"]["added"], 1);
}

#[test]
fn changes_at_root_reports_no_previous_version() {
    let repo = tempdir("root");
    vex_ok(&repo, &["init"]);
    vex_ok(
        &repo,
        &["import", workspace_root().join("examples/tiny.min.ifc").to_str().unwrap()],
    );
    vex_ok(&repo, &["commit", "-m", "first"]);
    let out = vex_ok(&repo, &["changes"]);
    assert!(
        out.contains("No previous version"),
        "unexpected output at root: {out}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&vex_ok(&repo, &["--json", "changes"])).unwrap();
    assert_eq!(json["status"], "no-previous-version");
}

/// Locks the JSON output byte-for-byte against `fixtures/visual_diff.golden.json`
/// (with `from`/`to` redacted because they're commit hashes). Any change to the
/// schema, field order, or classifier output trips this test — bump `SCHEMA`
/// and regenerate the golden fixture deliberately.
#[test]
fn compare_json_matches_golden() {
    let (repo, from, to) = seed_two_versions("golden");
    let raw = vex_ok(&repo, &["--json", "compare", &from, &to]);
    let mut actual: serde_json::Value = serde_json::from_str(&raw).unwrap();
    actual["from"] = serde_json::Value::String("<from>".into());
    actual["to"] = serde_json::Value::String("<to>".into());

    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/visual_diff.golden.json");
    let golden_text = std::fs::read_to_string(&golden_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", golden_path.display()));
    let golden: serde_json::Value = serde_json::from_str(&golden_text).unwrap();

    assert_eq!(
        actual, golden,
        "VisualDiff JSON drifted from golden fixture.\n\
         Regenerate intentionally with:\n\
         vex --json compare <a> <b> | <redact from/to> > {}\n\
         actual:\n{}",
        golden_path.display(),
        serde_json::to_string_pretty(&actual).unwrap()
    );
}
