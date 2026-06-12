//! Canonical geometry hashing primitives.
//!
//! Phase 3 scope: pure-Rust analytic + tessellated hashes. No FFI. The values
//! passed in must already be extracted from the IR; this crate is deliberately
//! agnostic of the IR/parser so it can be reused by alternative front-ends.
//!
//! Hash construction always starts from a type tag byte so two different
//! shape kinds can never collide by coincidence of field contents.

#![allow(clippy::pedantic)]

use vex_utils::hash::HashAlgo;
use vex_utils::{Hash256, Hasher, Tolerance};

/// Kind tag written as the first bytes of every shape hash. New kinds go at
/// the end; existing values must not be renumbered.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    RectProfile = 1,
    CircleProfile = 2,
    ArbitraryProfile = 3,
    Block = 10,
    RightCircularCylinder = 11,
    ExtrudedAreaSolid = 20,
    TriangulatedFaceSet = 30,
    PolygonalFaceSet = 31,
    FacetedBrep = 32,
    PointSet2 = 40,
    PointSet3 = 41,
    FaceRing = 42,
}

fn start(kind: ShapeKind) -> Hasher {
    let mut h = Hasher::new(HashAlgo::Blake3);
    h.update(b"geom:");
    h.update(&[kind as u8]);
    h
}

fn push_f64(h: &mut Hasher, q: f64) {
    let q = if q == 0.0 { 0.0 } else { q };
    h.update(&q.to_bits().to_be_bytes());
}

fn push_usize(h: &mut Hasher, n: usize) {
    h.update(&(n as u64).to_be_bytes());
}

#[must_use]
pub fn rect_profile(x: f64, y: f64, tol: &Tolerance) -> Hash256 {
    let mut h = start(ShapeKind::RectProfile);
    push_f64(&mut h, tol.quantize_linear(x));
    push_f64(&mut h, tol.quantize_linear(y));
    h.finalize()
}

#[must_use]
pub fn circle_profile(r: f64, tol: &Tolerance) -> Hash256 {
    let mut h = start(ShapeKind::CircleProfile);
    push_f64(&mut h, tol.quantize_linear(r));
    h.finalize()
}

/// Hash an arbitrary closed profile polyline. Rotation- and reversal-
/// invariant via canonicalization to the lex-smallest rotation of the
/// lex-smaller orientation.
#[must_use]
pub fn arbitrary_profile(vertices: &[[f64; 2]], tol: &Tolerance) -> Hash256 {
    let quant: Vec<[f64; 2]> = vertices
        .iter()
        .map(|v| [tol.quantize_linear(v[0]), tol.quantize_linear(v[1])])
        .collect();
    let fwd = best_rotation(&quant, false);
    let rev = best_rotation(&quant, true);
    let canon = if seq_le(&fwd, &rev) { fwd } else { rev };
    let mut h = start(ShapeKind::ArbitraryProfile);
    push_usize(&mut h, canon.len());
    for v in &canon {
        push_f64(&mut h, v[0]);
        push_f64(&mut h, v[1]);
    }
    h.finalize()
}

fn best_rotation<const N: usize>(verts: &[[f64; N]], reversed: bool) -> Vec<[f64; N]> {
    let n = verts.len();
    if n == 0 {
        return Vec::new();
    }
    let mut best: Option<Vec<[f64; N]>> = None;
    for start_idx in 0..n {
        let mut seq: Vec<[f64; N]> = Vec::with_capacity(n);
        for i in 0..n {
            let idx = if reversed {
                (n + start_idx - i) % n
            } else {
                (start_idx + i) % n
            };
            seq.push(verts[idx]);
        }
        if best.as_ref().is_none_or(|b| seq_lt(&seq, b)) {
            best = Some(seq);
        }
    }
    best.unwrap_or_default()
}

fn seq_lt<const N: usize>(a: &[[f64; N]], b: &[[f64; N]]) -> bool {
    for (x, y) in a.iter().zip(b.iter()) {
        for d in 0..N {
            match x[d].to_bits().cmp(&y[d].to_bits()) {
                std::cmp::Ordering::Less => return true,
                std::cmp::Ordering::Greater => return false,
                std::cmp::Ordering::Equal => {}
            }
        }
    }
    false
}

fn seq_le<const N: usize>(a: &[[f64; N]], b: &[[f64; N]]) -> bool {
    !seq_lt(b, a)
}

/// Canonicalize a closed ring of points: lex-smallest rotation of the
/// lex-smaller orientation (rotation- and reversal-invariant).
fn canon_ring<const N: usize>(ring: &[[f64; N]], tol: &Tolerance) -> Vec<[f64; N]> {
    let quant: Vec<[f64; N]> = ring
        .iter()
        .map(|v| {
            let mut q = [0.0; N];
            for d in 0..N {
                q[d] = tol.quantize_linear(v[d]);
            }
            q
        })
        .collect();
    let fwd = best_rotation(&quant, false);
    let rev = best_rotation(&quant, true);
    if seq_le(&fwd, &rev) {
        fwd
    } else {
        rev
    }
}

fn push_ring<const N: usize>(h: &mut Hasher, ring: &[[f64; N]]) {
    push_usize(h, ring.len());
    for v in ring {
        for c in v {
            push_f64(h, *c);
        }
    }
}

fn point_set_hash<const N: usize>(
    kind: ShapeKind,
    points: &[[f64; N]],
    tol: &Tolerance,
) -> Hash256 {
    let mut rows: Vec<[u64; N]> = points
        .iter()
        .map(|p| {
            let mut bits = [0u64; N];
            for d in 0..N {
                let q = tol.quantize_linear(p[d]);
                let q = if q == 0.0 { 0.0 } else { q };
                bits[d] = q.to_bits();
            }
            bits
        })
        .collect();
    rows.sort_unstable();
    let mut h = start(kind);
    push_usize(&mut h, rows.len());
    for row in &rows {
        for c in row {
            h.update(&c.to_be_bytes());
        }
    }
    h.finalize()
}

/// Order-invariant multiset hash of a 2D point collection. Used for
/// `IfcCartesianPointList2D`, whose row order is exporter noise once the
/// referencing faces are canonically indexed.
#[must_use]
pub fn point_set_2d(points: &[[f64; 2]], tol: &Tolerance) -> Hash256 {
    point_set_hash(ShapeKind::PointSet2, points, tol)
}

/// Order-invariant multiset hash of a 3D point collection.
#[must_use]
pub fn point_set_3d(points: &[[f64; 3]], tol: &Tolerance) -> Hash256 {
    point_set_hash(ShapeKind::PointSet3, points, tol)
}

/// Canonical hash of one polygonal face expressed as resolved point rings
/// (outer boundary + optional void rings). Rotation- and reversal-invariant
/// per ring; void rings are hashed as a sorted multiset.
#[must_use]
pub fn face_ring_3d(outer: &[[f64; 3]], voids: &[Vec<[f64; 3]>], tol: &Tolerance) -> Hash256 {
    let mut h = start(ShapeKind::FaceRing);
    push_ring(&mut h, &canon_ring(outer, tol));
    let mut canon_voids: Vec<Vec<[f64; 3]>> = voids.iter().map(|v| canon_ring(v, tol)).collect();
    canon_voids.sort_by(|a, b| {
        if seq_lt(a, b) {
            std::cmp::Ordering::Less
        } else if seq_lt(b, a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    push_usize(&mut h, canon_voids.len());
    for v in &canon_voids {
        push_ring(&mut h, v);
    }
    h.finalize()
}

#[must_use]
pub fn block(x: f64, y: f64, z: f64, tol: &Tolerance) -> Hash256 {
    let mut h = start(ShapeKind::Block);
    push_f64(&mut h, tol.quantize_linear(x));
    push_f64(&mut h, tol.quantize_linear(y));
    push_f64(&mut h, tol.quantize_linear(z));
    h.finalize()
}

#[must_use]
pub fn right_circular_cylinder(height: f64, radius: f64, tol: &Tolerance) -> Hash256 {
    let mut h = start(ShapeKind::RightCircularCylinder);
    push_f64(&mut h, tol.quantize_linear(height));
    push_f64(&mut h, tol.quantize_linear(radius));
    h.finalize()
}

/// `IfcExtrudedAreaSolid(profile_hash, direction, depth)`.
#[must_use]
pub fn extruded_area_solid(
    profile_hash: Hash256,
    direction: [f64; 3],
    depth: f64,
    tol: &Tolerance,
) -> Hash256 {
    let norm = (direction[0].powi(2) + direction[1].powi(2) + direction[2].powi(2)).sqrt();
    let d = if norm > 0.0 {
        [
            direction[0] / norm,
            direction[1] / norm,
            direction[2] / norm,
        ]
    } else {
        [0.0, 0.0, 1.0]
    };
    let mut h = start(ShapeKind::ExtrudedAreaSolid);
    h.update(profile_hash.as_bytes());
    let ang = tol.angular.max(1.0e-12);
    for c in &d {
        let q = (c / ang).round() * ang;
        push_f64(&mut h, q);
    }
    push_f64(&mut h, tol.quantize_linear(depth));
    h.finalize()
}

#[must_use]
pub fn triangulated_face_set(
    vertices: &[[f64; 3]],
    faces: &[[usize; 3]],
    tol: &Tolerance,
) -> Hash256 {
    mesh_hash(ShapeKind::TriangulatedFaceSet, vertices, faces, tol)
}

#[must_use]
pub fn polygonal_face_set(vertices: &[[f64; 3]], faces: &[Vec<usize>], tol: &Tolerance) -> Hash256 {
    polyhedron_hash(ShapeKind::PolygonalFaceSet, vertices, faces, tol)
}

#[must_use]
pub fn faceted_brep(vertices: &[[f64; 3]], faces: &[Vec<usize>], tol: &Tolerance) -> Hash256 {
    polyhedron_hash(ShapeKind::FacetedBrep, vertices, faces, tol)
}

/// Canonical vertex table: quantized vertices, sorted by bit pattern and
/// deduplicated, plus a remap from original index → canonical index.
///
/// Hashing faces through this remap makes the result invariant to vertex
/// array permutation and to exporter-style per-face vertex duplication —
/// both common across IFC re-exports of identical geometry.
struct CanonVerts {
    sorted: Vec<[u64; 3]>,
    remap: Vec<usize>,
    bbox: [[f64; 3]; 2],
}

fn canon_verts(vertices: &[[f64; 3]], tol: &Tolerance) -> CanonVerts {
    let quant = quantize_verts(vertices, tol);
    let bbox = bbox_of(&quant);
    let bits: Vec<[u64; 3]> = quant
        .iter()
        .map(|v| [v[0].to_bits(), v[1].to_bits(), v[2].to_bits()])
        .collect();
    let mut sorted = bits.clone();
    sorted.sort_unstable();
    sorted.dedup();
    let remap = bits
        .iter()
        .map(|b| sorted.binary_search(b).unwrap_or(0))
        .collect();
    CanonVerts {
        sorted,
        remap,
        bbox,
    }
}

impl CanonVerts {
    /// Remap an original vertex index to its canonical index. Out-of-range
    /// indices (malformed input) map to a deterministic sentinel rather
    /// than panicking.
    fn idx(&self, i: usize) -> usize {
        self.remap.get(i).copied().unwrap_or(usize::MAX)
    }

    fn hash_header(&self, h: &mut Hasher, face_count: usize) {
        push_usize(h, self.sorted.len());
        push_usize(h, face_count);
        for c in self.bbox.iter().flatten() {
            push_f64(h, *c);
        }
        for v in &self.sorted {
            for c in v {
                h.update(&c.to_be_bytes());
            }
        }
    }
}

fn mesh_hash(
    kind: ShapeKind,
    vertices: &[[f64; 3]],
    faces: &[[usize; 3]],
    tol: &Tolerance,
) -> Hash256 {
    let mut h = start(kind);
    let cv = canon_verts(vertices, tol);
    cv.hash_header(&mut h, faces.len());
    let mut canon_faces: Vec<[usize; 3]> = faces
        .iter()
        .map(|f| canon_tri(&[cv.idx(f[0]), cv.idx(f[1]), cv.idx(f[2])]))
        .collect();
    canon_faces.sort_unstable();
    for f in &canon_faces {
        for i in f {
            push_usize(&mut h, *i);
        }
    }
    h.finalize()
}

fn polyhedron_hash(
    kind: ShapeKind,
    vertices: &[[f64; 3]],
    faces: &[Vec<usize>],
    tol: &Tolerance,
) -> Hash256 {
    let mut h = start(kind);
    let cv = canon_verts(vertices, tol);
    cv.hash_header(&mut h, faces.len());
    let mut canon_faces: Vec<Vec<usize>> = faces
        .iter()
        .map(|f| {
            let remapped: Vec<usize> = f.iter().map(|&i| cv.idx(i)).collect();
            canon_face(&remapped)
        })
        .collect();
    canon_faces.sort_unstable();
    for f in &canon_faces {
        push_usize(&mut h, f.len());
        for i in f {
            push_usize(&mut h, *i);
        }
    }
    h.finalize()
}

fn quantize_verts(vs: &[[f64; 3]], tol: &Tolerance) -> Vec<[f64; 3]> {
    vs.iter()
        .map(|v| {
            [
                tol.quantize_linear(v[0]),
                tol.quantize_linear(v[1]),
                tol.quantize_linear(v[2]),
            ]
        })
        .collect()
}

fn bbox_of(vs: &[[f64; 3]]) -> [[f64; 3]; 2] {
    if vs.is_empty() {
        return [[0.0; 3]; 2];
    }
    let mut lo = vs[0];
    let mut hi = vs[0];
    for v in &vs[1..] {
        for i in 0..3 {
            if v[i] < lo[i] {
                lo[i] = v[i];
            }
            if v[i] > hi[i] {
                hi[i] = v[i];
            }
        }
    }
    [lo, hi]
}

fn canon_tri(f: &[usize; 3]) -> [usize; 3] {
    let min_idx = f
        .iter()
        .enumerate()
        .min_by_key(|(_, v)| **v)
        .map_or(0, |(i, _)| i);
    [f[min_idx], f[(min_idx + 1) % 3], f[(min_idx + 2) % 3]]
}

fn canon_face(f: &[usize]) -> Vec<usize> {
    if f.is_empty() {
        return Vec::new();
    }
    let n = f.len();
    let min_idx = f
        .iter()
        .enumerate()
        .min_by_key(|(_, v)| **v)
        .map_or(0, |(i, _)| i);
    (0..n).map(|i| f[(min_idx + i) % n]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tol() -> Tolerance {
        Tolerance::new(1e-3, 1e-6)
    }

    #[test]
    fn block_matches_on_quantization() {
        let a = block(1.0, 2.0, 3.0, &tol());
        let b = block(1.0001, 2.0003, 2.9998, &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn block_differs_beyond_tolerance() {
        let a = block(1.0, 2.0, 3.0, &tol());
        let b = block(1.01, 2.0, 3.0, &tol());
        assert_ne!(a, b);
    }

    #[test]
    fn kind_tags_prevent_cross_collision() {
        let cyl = right_circular_cylinder(1.0, 2.0, &tol());
        let rect = rect_profile(1.0, 2.0, &tol());
        assert_ne!(cyl, rect);
    }

    #[test]
    fn extrusion_direction_normalises() {
        let p = rect_profile(1.0, 2.0, &tol());
        let a = extruded_area_solid(p, [0.0, 0.0, 1.0], 3.0, &tol());
        let b = extruded_area_solid(p, [0.0, 0.0, 5.0], 3.0, &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn mesh_vertex_order_invariant() {
        let vs1 = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let fs1 = vec![[0usize, 1, 2]];
        let a = triangulated_face_set(&vs1, &fs1, &tol());
        let vs2 = vec![[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 0.0]];
        let fs2 = vec![[2usize, 0, 1]];
        let b = triangulated_face_set(&vs2, &fs2, &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn mesh_differs_on_vertex_move() {
        let vs = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let fs = vec![[0usize, 1, 2]];
        let a = triangulated_face_set(&vs, &fs, &tol());
        let vs2 = vec![[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let b = triangulated_face_set(&vs2, &fs, &tol());
        assert_ne!(a, b);
    }

    #[test]
    fn arbitrary_profile_rotation_invariant() {
        let vs1 = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let vs2 = vec![[1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]];
        let a = arbitrary_profile(&vs1, &tol());
        let b = arbitrary_profile(&vs2, &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn mesh_vertex_array_permutation_invariant() {
        // Two triangles sharing an edge; vs2 reverses the vertex array and
        // remaps faces accordingly — identical geometry, permuted storage.
        let vs1 = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let fs1 = vec![[0usize, 1, 2], [0, 2, 3]];
        let vs2: Vec<[f64; 3]> = vs1.iter().rev().copied().collect();
        let fs2 = vec![[3usize, 2, 1], [3, 1, 0]];
        let a = triangulated_face_set(&vs1, &fs1, &tol());
        let b = triangulated_face_set(&vs2, &fs2, &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn mesh_face_order_invariant() {
        let vs = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let a = triangulated_face_set(&vs, &[[0usize, 1, 2], [0, 2, 3]], &tol());
        let b = triangulated_face_set(&vs, &[[0usize, 2, 3], [0, 1, 2]], &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn mesh_duplicate_vertices_collapse() {
        // Exporter-style per-face vertex duplication hashes like the
        // deduplicated mesh.
        let vs1 = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let fs1 = vec![[0usize, 1, 2], [0, 2, 3]];
        let vs2 = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let fs2 = vec![[0usize, 1, 2], [3, 4, 5]];
        let a = triangulated_face_set(&vs1, &fs1, &tol());
        let b = triangulated_face_set(&vs2, &fs2, &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn polyhedron_vertex_array_permutation_invariant() {
        let vs1 = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let fs1 = vec![vec![0usize, 1, 2, 3]];
        let vs2: Vec<[f64; 3]> = vs1.iter().rev().copied().collect();
        let fs2 = vec![vec![3usize, 2, 1, 0]];
        let a = polygonal_face_set(&vs1, &fs1, &tol());
        let b = polygonal_face_set(&vs2, &fs2, &tol());
        assert_eq!(a, b);
    }

    #[test]
    fn mesh_out_of_range_index_is_deterministic() {
        let vs = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        let fs = vec![[0usize, 1, 99]];
        let a = triangulated_face_set(&vs, &fs, &tol());
        let b = triangulated_face_set(&vs, &fs, &tol());
        assert_eq!(a, b);
    }
}
