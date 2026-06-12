#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::pedantic
)]
//! Fidelity guarantees the whole product stands on:
//!
//! 1. Import is deterministic — same bytes, same tree hash.
//! 2. Self-diff is empty — a model diffed against itself reports nothing.
//! 3. Round-trip is lossless — import → commit → checkout → re-import
//!    produces a semantically identical model (diff = 0).
//! 4. Exporter noise is absorbed — re-exported files with permuted mesh
//!    vertex arrays hash identically.
//! 5. Real edits surface with the right classification (property /
//!    placement / shape).
//!
//! The same battery runs over any real-world IFC corpus dropped into
//! `tests/fixtures/corpus/` (see `tools/fetch-fixtures.sh`).

mod common;

use common::{generate, ModelSpec, Mutation};
use std::path::{Path, PathBuf};
use vex_core::Repository;
use vex_diff::{Change, Layer};

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "vex-fidelity-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_model(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

/// Import + commit two model versions, return the repo.
fn seed(tag: &str, v1: &str, v2: &str) -> Repository {
    let dir = tempdir(tag);
    let repo = Repository::init(&dir).unwrap();
    let p1 = write_model(&dir, "v1.ifc", v1);
    repo.import(&p1).unwrap();
    repo.commit("v1", "test", "test@vex").unwrap();
    let p2 = write_model(&dir, "v2.ifc", v2);
    repo.import(&p2).unwrap();
    repo.commit("v2", "test", "test@vex").unwrap();
    repo
}

fn head_and_parent(repo: &Repository) -> (String, String) {
    let log = repo.log().unwrap();
    assert!(log.len() >= 2, "expected at least two commits");
    (log[1].0.to_hex(), log[0].0.to_hex())
}

// ── 1. Determinism ─────────────────────────────────────────────────────

#[test]
fn import_same_bytes_is_deterministic() {
    let text = generate(ModelSpec::default(), &[]);
    let dir = tempdir("det");
    let repo = Repository::init(&dir).unwrap();
    let p = write_model(&dir, "m.ifc", &text);
    let t1 = repo.import(&p).unwrap();
    let t2 = repo.import(&p).unwrap();
    assert_eq!(t1, t2, "same bytes must stage the same tree hash");
}

// ── 2. Self-diff ───────────────────────────────────────────────────────

#[test]
fn self_diff_is_empty() {
    let text = generate(ModelSpec::default(), &[]);
    let repo = seed("self", &text, &text);
    let (from, to) = head_and_parent(&repo);
    let report = repo.diff_refs(&from, &to).unwrap();
    assert!(
        report.changes.is_empty(),
        "identical models must diff clean, got: {:?}",
        report.changes
    );
}

// ── 3. Round-trip ──────────────────────────────────────────────────────

#[test]
fn checkout_reimport_round_trip_is_lossless() {
    let text = generate(ModelSpec::default(), &[]);
    let dir = tempdir("rt");
    let repo = Repository::init(&dir).unwrap();
    let p = write_model(&dir, "m.ifc", &text);
    repo.import(&p).unwrap();
    repo.commit("v1", "test", "test@vex").unwrap();

    let exported = dir.join("exported.ifc");
    repo.checkout("HEAD", &exported).unwrap();
    repo.import(&exported).unwrap();
    repo.commit("v2", "test", "test@vex").unwrap();

    let (from, to) = head_and_parent(&repo);
    let report = repo.diff_refs(&from, &to).unwrap();
    assert!(
        report.changes.is_empty(),
        "checkout → re-import must be semantically lossless, got: {:?}",
        report.changes
    );
}

// ── 4. Exporter noise ──────────────────────────────────────────────────

#[test]
fn permuted_mesh_vertices_diff_clean() {
    let spec = ModelSpec::default();
    let v1 = generate(spec, &[]);
    let v2 = generate(spec, &[Mutation::PermuteMeshVertices]);
    assert_ne!(v1, v2, "permutation must actually rewrite the file");
    let repo = seed("perm", &v1, &v2);
    let (from, to) = head_and_parent(&repo);
    let report = repo.diff_refs(&from, &to).unwrap();
    assert!(
        report.changes.is_empty(),
        "vertex-order re-export is semantically a no-op, got: {:?}",
        report.changes
    );
}

// ── 5. Real edits classify correctly ───────────────────────────────────

#[test]
fn rename_classifies_as_property() {
    let spec = ModelSpec::default();
    let v1 = generate(spec, &[]);
    let v2 = generate(spec, &[Mutation::RenameWall { storey: 0, wall: 1 }]);
    let repo = seed("rename", &v1, &v2);
    let (from, to) = head_and_parent(&repo);
    let report = repo.diff_refs(&from, &to).unwrap();
    assert_eq!(report.changes.len(), 1, "got: {:?}", report.changes);
    match &report.changes[0] {
        Change::Modified {
            layer, placement, ..
        } => {
            assert_eq!(*layer, Layer::Property);
            assert!(placement.is_none());
        }
        other => panic!("expected Modified, got {other:?}"),
    }
}

#[test]
fn move_surfaces_placement_delta() {
    let spec = ModelSpec::default();
    let v1 = generate(spec, &[]);
    let v2 = generate(
        spec,
        &[Mutation::MoveWall {
            storey: 1,
            wall: 0,
            dx: 2.5,
        }],
    );
    let repo = seed("move", &v1, &v2);
    let (from, to) = head_and_parent(&repo);
    let report = repo.diff_refs(&from, &to).unwrap();
    let modified: Vec<_> = report
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::Modified {
                type_name,
                placement,
                ..
            } if type_name == "IFCWALL" => Some(placement.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(modified.len(), 1, "got: {:?}", report.changes);
    let delta = modified[0].as_ref().expect("placement delta expected");
    assert_eq!(delta.before, Some([0.0, 0.0, 3.0]));
    assert_eq!(delta.after, Some([2.5, 0.0, 3.0]));
}

#[test]
fn mesh_edit_classifies_as_shape() {
    let spec = ModelSpec::default();
    let v1 = generate(spec, &[]);
    let v2 = generate(spec, &[Mutation::EditMesh { storey: 0, mesh: 0 }]);
    let repo = seed("meshedit", &v1, &v2);
    let (from, to) = head_and_parent(&repo);
    let report = repo.diff_refs(&from, &to).unwrap();
    let shape: Vec<_> = report
        .changes
        .iter()
        .filter(|c| matches!(c, Change::Modified { layer, .. } if *layer == Layer::Shape))
        .collect();
    assert_eq!(
        shape.len(),
        1,
        "mesh edit must surface as a Shape-layer change, got: {:?}",
        report.changes
    );
}

// ── Real-world corpus (optional, see tools/fetch-fixtures.sh) ──────────

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus")
}

fn corpus_files() -> Vec<PathBuf> {
    let dir = corpus_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ifc")))
        .collect();
    files.sort();
    files
}

#[test]
fn corpus_determinism_and_self_diff() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("corpus empty — run tools/fetch-fixtures.sh to enable; skipping");
        return;
    }
    for f in &files {
        let dir = tempdir("corpus");
        let repo = Repository::init(&dir).unwrap();
        let t1 = repo.import(f).unwrap();
        let t2 = repo.import(f).unwrap();
        assert_eq!(t1, t2, "{}: import must be deterministic", f.display());
        repo.commit("v1", "test", "test@vex").unwrap();
        repo.import(f).unwrap();
        repo.commit("v2", "test", "test@vex").unwrap();
        let (from, to) = head_and_parent(&repo);
        let report = repo.diff_refs(&from, &to).unwrap();
        assert!(
            report.changes.is_empty(),
            "{}: self-diff must be empty, got {} changes",
            f.display(),
            report.changes.len()
        );
    }
}

#[test]
fn corpus_round_trip_is_lossless() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("corpus empty — run tools/fetch-fixtures.sh to enable; skipping");
        return;
    }
    for f in &files {
        let dir = tempdir("corpus-rt");
        let repo = Repository::init(&dir).unwrap();
        repo.import(f).unwrap();
        repo.commit("v1", "test", "test@vex").unwrap();
        let exported = dir.join("exported.ifc");
        repo.checkout("HEAD", &exported).unwrap();
        repo.import(&exported).unwrap();
        repo.commit("v2", "test", "test@vex").unwrap();
        let (from, to) = head_and_parent(&repo);
        let report = repo.diff_refs(&from, &to).unwrap();
        assert!(
            report.changes.is_empty(),
            "{}: round-trip must be lossless, got {} changes: {:?}",
            f.display(),
            report.changes.len(),
            report.changes.iter().take(3).collect::<Vec<_>>()
        );
    }
}

// ── Scale (opt-in: cargo test -p vex-core -- --ignored) ────────────────

#[test]
#[ignore = "scale smoke test — run explicitly with --ignored"]
fn scale_import_commit_diff() {
    let storeys: usize = std::env::var("VEX_SCALE_STOREYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let spec = ModelSpec {
        storeys,
        walls_per_storey: 40,
        meshes_per_storey: 10,
    };
    let v1 = generate(spec, &[]);
    let v2 = generate(
        spec,
        &[
            Mutation::RenameWall { storey: 0, wall: 0 },
            Mutation::MoveWall {
                storey: 1,
                wall: 1,
                dx: 1.0,
            },
        ],
    );
    let start = std::time::Instant::now();
    let repo = seed("scale", &v1, &v2);
    let (from, to) = head_and_parent(&repo);
    let report = repo.diff_refs(&from, &to).unwrap();
    eprintln!(
        "scale: {} storeys, {} changes, total {:?}",
        storeys,
        report.changes.len(),
        start.elapsed()
    );
    // Exactly two product-level edits; the moved wall also churns its
    // anonymous placement chain (point/axis/local placement), which is
    // expected at the graph level and folded into `internal` upstream.
    let modified = report
        .changes
        .iter()
        .filter(|c| matches!(c, Change::Modified { .. }))
        .count();
    assert_eq!(modified, 2, "exactly the two seeded product edits");
}
