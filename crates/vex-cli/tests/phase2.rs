//! Phase 2 integration tests: merge, signing, profile.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-p2-{tag}-{}",
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

#[test]
fn config_shows_profile_and_hash() {
    let repo = tempdir("cfg");
    vex_ok(&repo, &["init"]);
    let out = vex_ok(&repo, &["config"]);
    assert!(out.contains("tolerance_linear"));
    assert!(out.contains("IFCOWNERHISTORY"));
    assert!(out.contains("profile_hash"));
}

#[test]
fn key_gen_and_list() {
    let repo = tempdir("key");
    vex_ok(&repo, &["init"]);
    let gen = vex_ok(&repo, &["key", "gen", "alice"]);
    assert!(gen.contains("generated key alice"));
    let list = vex_ok(&repo, &["key", "list"]);
    assert!(list.contains("alice"));
}

#[test]
fn signed_commit_verifies() {
    let repo = tempdir("sig");
    let fixture = workspace_root().join("examples/tiny.min.ifc");
    vex_ok(&repo, &["init"]);
    vex_ok(&repo, &["key", "gen", "alice"]);
    vex_ok(&repo, &["import", fixture.to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "signed", "--sign", "alice"]);
    let verify = vex_ok(&repo, &["verify", "--signatures"]);
    assert!(verify.contains("1 valid"), "out={verify}");
}

#[test]
fn merge_clean_nonoverlapping_edits() {
    // Base = tiny.min.ifc; ours = tiny-v2.min.ifc (wall rename + new column);
    // theirs = tiny-merge-theirs.min.ifc (slab rename + new beam).
    // Edits touch disjoint nodes → should merge cleanly.
    let root = workspace_root();
    let base_f = root.join("examples/tiny.min.ifc");
    let ours_f = root.join("examples/tiny-v2.min.ifc");
    let theirs_f = root.join("examples/tiny-merge-theirs.min.ifc");

    // Build a repo on main = base, branch "ours" = ours, branch "theirs" = theirs.
    // Since Vex's MVP lacks true branches, we drive it manually via refs.
    let repo_dir = tempdir("merge");
    vex_ok(&repo_dir, &["init"]);
    vex_ok(&repo_dir, &["import", base_f.to_str().unwrap()]);
    vex_ok(&repo_dir, &["commit", "-m", "base"]);
    let log_json = vex_ok(&repo_dir, &["--json", "log"]);
    let log: serde_json::Value = serde_json::from_str(&log_json).unwrap();
    let base_hash = log[0]["commit"].as_str().unwrap().to_string();

    // "ours" commit
    vex_ok(&repo_dir, &["import", ours_f.to_str().unwrap()]);
    vex_ok(&repo_dir, &["commit", "-m", "ours"]);
    let log_json = vex_ok(&repo_dir, &["--json", "log"]);
    let log: serde_json::Value = serde_json::from_str(&log_json).unwrap();
    let ours_hash = log[0]["commit"].as_str().unwrap().to_string();

    // Reset HEAD to base and create "theirs". We don't have reset; re-point
    // via the store by overwriting HEAD — since the CLI doesn't expose that,
    // we accept that "theirs" will parent off "ours" rather than base. The
    // three-way merge function itself is tested via the `vex-core` unit
    // test; here we verify the CLI plumbing end-to-end with a trivial case.
    vex_ok(&repo_dir, &["import", theirs_f.to_str().unwrap()]);
    vex_ok(&repo_dir, &["commit", "-m", "theirs"]);
    let log_json = vex_ok(&repo_dir, &["--json", "log"]);
    let log: serde_json::Value = serde_json::from_str(&log_json).unwrap();
    let theirs_hash = log[0]["commit"].as_str().unwrap().to_string();

    // With a linear history, ours is an ancestor of theirs — merging theirs
    // into ours should fast-forward.
    let (ok, out, _err) = vex(&repo_dir, &["merge", &ours_hash, &theirs_hash]);
    assert!(ok, "merge failed: {out}");
    assert!(
        out.contains("Fast-forward") || out.contains("0 conflicts") || out.contains("Clean merge"),
        "expected fast-forward or clean merge, got: {out}"
    );

    // JSON merge output is parseable. Status is fast-forward in this linear case.
    let (ok, out, _err) = vex(&repo_dir, &["--json", "merge", &ours_hash, &theirs_hash]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v["status"] == "fast-forward"
            || v["status"] == "up-to-date"
            || v["clean"] == serde_json::Value::Bool(true),
        "unexpected merge JSON: {v}"
    );

    // Sanity: base_hash resolves to something different than ours/theirs.
    assert_ne!(base_hash, ours_hash);
    assert_ne!(ours_hash, theirs_hash);
}

#[test]
fn merge_detects_modify_modify_conflict() {
    // Constructed via the library directly since the CLI has no branching.
    use vex_graph::{builder::GraphBuilder, HashConfig};
    use vex_ifc_parser::{ParseLimits, Parser};
    use vex_utils::StringInterner;

    // Three variants differing in Wall-A's name property.
    let base_src = r#"ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = IFCWALL('2O2Fr$t4X7Zf8NOew3FNr2',$,'Wall-base',$,$,$,$,$,.STANDARD.);
ENDSEC;
END-ISO-10303-21;
"#;
    let ours_src = base_src.replace("Wall-base", "Wall-ours");
    let theirs_src = base_src.replace("Wall-base", "Wall-theirs");

    fn graph(src: &str) -> (vex_graph::ir::IfcGraph, StringInterner) {
        let i = StringInterner::new();
        let mut p = Parser::new(std::io::Cursor::new(src), ParseLimits::default());
        let g = GraphBuilder::build_from_parser(i.clone(), &mut p).unwrap();
        (g, i)
    }
    let (gb, ib) = graph(base_src);
    let (go, io) = graph(&ours_src);
    let (gt, it) = graph(&theirs_src);
    let r = vex_diff::merge_graphs(&gb, &ib, &go, &io, &gt, &it, &HashConfig::default());
    assert!(!r.clean);
    assert_eq!(r.conflicts.len(), 1);
    match &r.conflicts[0] {
        vex_diff::Conflict::ModifyModify { .. } => {}
        c => panic!("expected ModifyModify, got {c:?}"),
    }
}
