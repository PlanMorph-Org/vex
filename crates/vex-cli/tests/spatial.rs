#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! `vex spatial` — authoritative spatial containment metadata derived from a
//! committed tree's retained `Aggregates`/`Contains` relationships.

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
        "vex-spatial-{tag}-{}",
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

fn container<'a>(v: &'a serde_json::Value, gid: &str) -> &'a serde_json::Value {
    v["containers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["entity"]["global_id"] == gid)
        .unwrap_or_else(|| panic!("container {gid} not found"))
}

fn gid(label: &str) -> String {
    let mut s = label.to_string();
    while s.len() < 22 {
        s.push('0');
    }
    s
}

#[test]
fn spatial_json_reports_nested_hierarchy_and_membership() {
    let repo = seed("hier", "spatial-hierarchy.min.ifc");
    let out = vex_ok(&repo, &["--json", "spatial", "HEAD"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();

    // Versioned schema + resolved commit.
    assert_eq!(v["schema"], "vex.spatial/1");
    assert_eq!(v["ref"], "HEAD");
    assert_eq!(v["commit"].as_str().unwrap().len(), 64);

    // Project is the root (no spatial parent); Site/Building/Storey chain up.
    assert!(container(&v, &gid("PROJ"))["parent"].is_null());
    assert_eq!(
        container(&v, &gid("SITE"))["parent"]["global_id"],
        gid("PROJ")
    );
    assert_eq!(
        container(&v, &gid("BLDG"))["parent"]["global_id"],
        gid("SITE")
    );
    assert_eq!(
        container(&v, &gid("LVL1"))["parent"]["global_id"],
        gid("BLDG")
    );

    // Storey 1 directly contains Wall-1 and Slab-1 (element GlobalId membership),
    // reported sorted and de-duplicated.
    let lvl1: Vec<&str> = container(&v, &gid("LVL1"))["element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert!(lvl1.contains(&gid("WALL1").as_str()));
    assert!(lvl1.contains(&gid("SLAB1").as_str()));
    let mut sorted = lvl1.clone();
    sorted.sort_unstable();
    assert_eq!(lvl1, sorted, "element_global_ids must be sorted");

    // Containers ordered by spatial rank: Project, Site, Building, Storey…
    let ranks: Vec<&str> = v["containers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["entity"]["type_name"].as_str().unwrap())
        .collect();
    assert_eq!(ranks[0], "IFCPROJECT");
    assert_eq!(ranks[1], "IFCSITE");
    assert_eq!(ranks[2], "IFCBUILDING");
    assert!(ranks[3..].iter().all(|t| *t == "IFCBUILDINGSTOREY"));
}

#[test]
fn spatial_reports_unassigned_elements() {
    let repo = seed("unassigned", "spatial-hierarchy.min.ifc");
    let out = vex_ok(&repo, &["--json", "spatial", "HEAD"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();

    let unassigned: Vec<&str> = v["unassigned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["global_id"].as_str().unwrap())
        .collect();
    assert!(
        unassigned.contains(&gid("WALLU").as_str()),
        "expected the uncontained wall in unassigned, got {unassigned:?}"
    );
    // Spatial containers and relationship entities are never listed as unassigned.
    assert!(!unassigned.contains(&gid("PROJ").as_str()));
    assert!(!unassigned.contains(&gid("RELA1").as_str()));
    // Assigned elements are excluded from unassigned.
    assert!(!unassigned.contains(&gid("WALL1").as_str()));
}

#[test]
fn spatial_reports_ambiguous_multi_container_membership() {
    let repo = seed("ambiguous", "spatial-hierarchy.min.ifc");
    let out = vex_ok(&repo, &["--json", "spatial", "HEAD"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();

    let ambiguous = v["ambiguous"].as_array().unwrap();
    assert_eq!(ambiguous.len(), 1, "expected exactly one ambiguous element");
    let entry = &ambiguous[0];
    assert_eq!(entry["entity"]["global_id"], gid("PROXA"));

    // Both claiming storeys are preserved (multi-storey membership not collapsed).
    let containers: Vec<&str> = entry["containers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["global_id"].as_str().unwrap())
        .collect();
    assert!(containers.contains(&gid("LVL1").as_str()));
    assert!(containers.contains(&gid("LVL2").as_str()));

    // The element still appears under every claiming container.
    let in_lvl1 = container(&v, &gid("LVL1"))["element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g == &gid("PROXA"));
    let in_lvl2 = container(&v, &gid("LVL2"))["element_global_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g == &gid("PROXA"));
    assert!(in_lvl1 && in_lvl2);
}

#[test]
fn spatial_output_is_byte_stable_across_runs() {
    let repo = seed("stable", "spatial-hierarchy.min.ifc");
    let a = vex_ok(&repo, &["--json", "spatial", "HEAD"]);
    let b = vex_ok(&repo, &["--json", "spatial", "HEAD"]);
    assert_eq!(a, b, "spatial JSON must be deterministic across runs");
}

#[test]
fn spatial_is_resilient_to_malformed_graph() {
    let repo = seed("malformed", "spatial-malformed.min.ifc");
    // Must not panic/error on null relationship endpoints.
    let out = vex_ok(&repo, &["--json", "spatial", "HEAD"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();

    assert_eq!(v["schema"], "vex.spatial/1");

    // A Contains rel with a null RelatingStructure leaves the element unassigned.
    let unassigned: Vec<&str> = v["unassigned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["global_id"].as_str().unwrap())
        .collect();
    assert!(unassigned.contains(&gid("WALL1").as_str()));

    // An aggregation with a null RelatingObject leaves the storey parentless
    // rather than crashing.
    assert!(container(&v, &gid("LVL1"))["parent"].is_null());
}

#[test]
fn spatial_default_output_is_human_readable() {
    let repo = seed("human", "spatial-hierarchy.min.ifc");
    let out = vex_ok(&repo, &["spatial", "HEAD"]);
    assert!(out.contains("spatial containment at"));
    assert!(out.contains("IFCBUILDINGSTOREY"));
    assert!(out.contains("unassigned:"));
    assert!(out.contains("ambiguous"));
    // Human output must not be JSON.
    assert!(!out.trim_start().starts_with('{'));
}
