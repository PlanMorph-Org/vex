#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! `vex checkout --storey <GlobalId>` — opt-in partial spatial checkout.
//!
//! Validates that a single authoritative storey containment group is
//! materialized as a valid IFC subset (project/site/building context +
//! contained elements + their geometry dependencies), with original
//! containment relations preserved, no dangling STEP references, deterministic
//! output, safe rejection of bad ids, and no impact on the default full
//! checkout.

use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir().join("tests").join("fixtures").join(name)
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-storey-{tag}-{}",
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

fn seed(tag: &str, fixture_name: &str) -> PathBuf {
    let repo = tempdir(tag);
    vex_ok(&repo, &["init"]);
    vex_ok(&repo, &["import", fixture(fixture_name).to_str().unwrap()]);
    vex_ok(&repo, &["commit", "-m", "v1"]);
    repo
}

/// Test `GlobalId` helper: pad a short label out to the 22-char IFC width with
/// trailing zeros (matches the fixtures' id scheme).
fn gid(label: &str) -> String {
    let mut s = label.to_string();
    while s.len() < 22 {
        s.push('0');
    }
    s
}

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}

fn container<'a>(v: &'a serde_json::Value, g: &str) -> Option<&'a serde_json::Value> {
    v["containers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["entity"]["global_id"] == g)
}

// --- valid output / no dangling references -------------------------------

#[test]
fn storey_checkout_output_reimports_cleanly() {
    // A file with dangling `#N` references would fail to re-import, so a clean
    // round-trip is the strongest proof that the subset has no invalid refs.
    let repo = seed("valid", "storey-geometry.min.ifc");
    let out = repo.join("lvl1.ifc");
    vex_ok(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--storey",
            &gid("LVL1"),
            "-o",
            out.to_str().unwrap(),
        ],
    );

    let repo2 = tempdir("valid-reimport");
    vex_ok(&repo2, &["init"]);
    vex_ok(&repo2, &["import", out.to_str().unwrap()]);
    vex_ok(&repo2, &["commit", "-m", "roundtrip"]);
}

// --- exact membership + required hierarchy -------------------------------

#[test]
fn storey_checkout_membership_and_hierarchy_are_exact() {
    let repo = seed("membership", "storey-geometry.min.ifc");
    let out = repo.join("lvl1.ifc");

    // The command reports the exact membership it wrote.
    let report = json(&vex_ok(
        &repo,
        &[
            "--json",
            "checkout",
            "HEAD",
            "--storey",
            &gid("LVL1"),
            "-o",
            out.to_str().unwrap(),
        ],
    ));
    assert_eq!(report["mode"], "storey");
    assert_eq!(report["storey"]["global_id"], gid("LVL1"));
    let els: Vec<&str> = report["element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert_eq!(
        els,
        vec![gid("SLBA").as_str(), gid("WALA").as_str()],
        "exact, sorted membership expected"
    );
    // Context chain up to the project root is carried along.
    let ctx: Vec<&str> = report["context"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["global_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ctx,
        vec![
            gid("BLDG").as_str(),
            gid("SITE").as_str(),
            gid("PROJ").as_str()
        ]
    );

    // Re-import the subset and let the authoritative `spatial` command confirm
    // the retained hierarchy and membership.
    let repo2 = tempdir("membership-reimport");
    vex_ok(&repo2, &["init"]);
    vex_ok(&repo2, &["import", out.to_str().unwrap()]);
    vex_ok(&repo2, &["commit", "-m", "roundtrip"]);
    let v = json(&vex_ok(&repo2, &["--json", "spatial", "HEAD"]));

    // Project → Site → Building → Storey chain is intact.
    assert!(container(&v, &gid("PROJ")).unwrap()["parent"].is_null());
    assert_eq!(
        container(&v, &gid("SITE")).unwrap()["parent"]["global_id"],
        gid("PROJ")
    );
    assert_eq!(
        container(&v, &gid("BLDG")).unwrap()["parent"]["global_id"],
        gid("SITE")
    );
    assert_eq!(
        container(&v, &gid("LVL1")).unwrap()["parent"]["global_id"],
        gid("BLDG")
    );

    // Storey 1 contains exactly Wall-A and Slab-A.
    let lvl1: Vec<&str> = container(&v, &gid("LVL1")).unwrap()["element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert_eq!(lvl1, vec![gid("SLBA").as_str(), gid("WALA").as_str()]);

    // Sibling storey and its element are entirely absent; nothing is orphaned.
    assert!(
        container(&v, &gid("LVL2")).is_none(),
        "LVL2 must not appear"
    );
    let unassigned = v["unassigned"].as_array().unwrap();
    assert!(
        unassigned.is_empty(),
        "no unassigned elements: {unassigned:?}"
    );
    assert!(v["ambiguous"].as_array().unwrap().is_empty());
}

// --- sibling exclusion in the raw STEP text ------------------------------

#[test]
fn storey_checkout_excludes_sibling_storey_and_geometry() {
    let repo = seed("exclude", "storey-geometry.min.ifc");
    let out = repo.join("lvl1.ifc");
    vex_ok(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--storey",
            &gid("LVL1"),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    let text = std::fs::read_to_string(&out).unwrap();

    // Valid STEP envelope.
    assert!(text.starts_with("ISO-10303-21;"));
    assert!(text.contains("FILE_SCHEMA(('IFC4'));"));
    assert!(text.trim_end().ends_with("END-ISO-10303-21;"));

    // Retained group + context present.
    for label in ["PROJ", "SITE", "BLDG", "LVL1", "WALA", "SLBA"] {
        assert!(text.contains(&gid(label)), "expected {label} in subset");
    }
    // Geometry dependencies pulled in (walls carry swept-solid bodies).
    assert!(text.contains("IFCEXTRUDEDAREASOLID"));
    assert!(text.contains("IFCSHAPEREPRESENTATION"));
    assert!(text.contains("IFCGEOMETRICREPRESENTATIONCONTEXT"));

    // Sibling storey (LVL2) and its exclusive element (Wall-B) are excluded.
    assert!(!text.contains(&gid("LVL2")), "LVL2 leaked into subset");
    assert!(!text.contains(&gid("WALB")), "Wall-B leaked into subset");
}

// --- determinism ----------------------------------------------------------

#[test]
fn storey_checkout_is_byte_deterministic() {
    let repo = seed("deterministic", "storey-geometry.min.ifc");
    let a = repo.join("a.ifc");
    let b = repo.join("b.ifc");
    vex_ok(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--storey",
            &gid("LVL1"),
            "-o",
            a.to_str().unwrap(),
        ],
    );
    vex_ok(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--storey",
            &gid("LVL1"),
            "-o",
            b.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "partial spatial checkout must be byte-deterministic"
    );
}

// --- safe rejection of bad ids -------------------------------------------

#[test]
fn storey_checkout_rejects_unknown_id() {
    let repo = seed("unknown", "storey-geometry.min.ifc");
    let out = repo.join("nope.ifc");
    let (ok, _stdout, stderr) = vex(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--storey",
            &gid("NOPE"),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(!ok, "unknown GlobalId must fail");
    assert!(
        stderr.contains("no entity with GlobalId"),
        "expected a clear unknown-id message, got: {stderr}"
    );
    assert!(!out.exists(), "no output should be written on rejection");
}

#[test]
fn storey_checkout_rejects_non_storey_id() {
    let repo = seed("nonstorey", "storey-geometry.min.ifc");
    let out = repo.join("nope.ifc");
    // The project GlobalId exists but is not a building storey.
    let (ok, _stdout, stderr) = vex(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--storey",
            &gid("PROJ"),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(!ok, "non-storey GlobalId must fail");
    assert!(
        stderr.contains("not IfcBuildingStorey"),
        "expected a clear non-storey message, got: {stderr}"
    );
    assert!(!out.exists(), "no output should be written on rejection");
}

// --- multi-storey policy: never silently split ---------------------------

#[test]
fn storey_checkout_preserves_multi_storey_element_in_full() {
    // In `spatial-hierarchy`, PROXA is contained by both LVL1 and LVL2.
    let repo = seed("multistorey", "spatial-hierarchy.min.ifc");
    let out = repo.join("lvl1.ifc");
    let report = json(&vex_ok(
        &repo,
        &[
            "--json",
            "checkout",
            "HEAD",
            "--storey",
            &gid("LVL1"),
            "-o",
            out.to_str().unwrap(),
        ],
    ));

    // The overlapping element is reported explicitly (policy: not collapsed).
    let multi: Vec<&str> = report["multi_storey_element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert_eq!(multi, vec![gid("PROXA").as_str()]);

    // LVL1's group is WALL1, SLAB1, PROXA — PROXA included in full.
    let els: Vec<&str> = report["element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert!(els.contains(&gid("WALL1").as_str()));
    assert!(els.contains(&gid("SLAB1").as_str()));
    assert!(els.contains(&gid("PROXA").as_str()));
    // LVL2's exclusive element must not be pulled in.
    assert!(!els.contains(&gid("COL2").as_str()));

    // Re-import and confirm PROXA is present exactly once under LVL1 and LVL2
    // is absent (not split, not duplicated across storeys).
    let repo2 = tempdir("multistorey-reimport");
    vex_ok(&repo2, &["init"]);
    vex_ok(&repo2, &["import", out.to_str().unwrap()]);
    vex_ok(&repo2, &["commit", "-m", "roundtrip"]);
    let v = json(&vex_ok(&repo2, &["--json", "spatial", "HEAD"]));
    assert!(container(&v, &gid("LVL2")).is_none());
    let lvl1: Vec<&str> = container(&v, &gid("LVL1")).unwrap()["element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert_eq!(
        lvl1.iter().filter(|g| **g == gid("PROXA")).count(),
        1,
        "PROXA present exactly once"
    );
    assert!(v["ambiguous"].as_array().unwrap().is_empty());
}

// --- default full checkout unaffected ------------------------------------

#[test]
fn full_checkout_is_unaffected_by_storey_feature() {
    let repo = seed("full", "storey-geometry.min.ifc");
    let a = repo.join("full-a.ifc");
    let b = repo.join("full-b.ifc");

    // Default checkout (no --storey) is unchanged and byte-stable.
    vex_ok(&repo, &["checkout", "HEAD", "-o", a.to_str().unwrap()]);
    vex_ok(&repo, &["checkout", "HEAD", "-o", b.to_str().unwrap()]);
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());

    let text = std::fs::read_to_string(&a).unwrap();
    // The full model retains BOTH storeys and every element.
    for label in [
        "PROJ", "SITE", "BLDG", "LVL1", "LVL2", "WALA", "SLBA", "WALB",
    ] {
        assert!(text.contains(&gid(label)), "full checkout missing {label}");
    }

    // And the full subset re-imports and reports both storeys' membership.
    let repo2 = tempdir("full-reimport");
    vex_ok(&repo2, &["init"]);
    vex_ok(&repo2, &["import", a.to_str().unwrap()]);
    vex_ok(&repo2, &["commit", "-m", "roundtrip"]);
    let v = json(&vex_ok(&repo2, &["--json", "spatial", "HEAD"]));
    assert!(container(&v, &gid("LVL1")).is_some());
    assert!(container(&v, &gid("LVL2")).is_some());
}
