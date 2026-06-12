//! Extract geometry hashes for shape-bearing nodes in the graph.
//!
//! This module bridges the IR to [`vex_geometry`]: for each recognised
//! shape type, it resolves scalar properties and referenced component
//! entities (point lists, loops, shells, profiles) from the graph and
//! produces a stable hash. The result is a map from node to geometry hash,
//! which the Merkle pass folds into each node's seed so geometry changes
//! propagate through the graph — crucially, the hash lands directly on the
//! shape node itself, so a deep mesh edit changes the owning product's hash
//! within the WL refinement budget.
//!
//! Supported shapes: rectangle/circle/arbitrary-closed profiles, blocks,
//! right circular cylinders, extruded area solids (including real extrusion
//! direction), triangulated face sets, polygonal face sets, and faceted
//! breps (with voids). Unrecognised or malformed shapes return `None` and
//! fall back to plain WL property hashing.

use ahash::AHashMap;
use vex_utils::{Hash256, StringId, StringInterner, Tolerance};

use crate::ir::{Edge, IfcGraph, Node, NodeId, Value};

/// Max recursion depth when resolving shape component references. Real IFC
/// shape trees are shallow (brep → shell → face → loop → point ≈ 5); the cap
/// guards against pathological reference chains.
const MAX_SHAPE_DEPTH: usize = 16;

/// How many positional slot keys (`_0`, `_1`, …) to pre-intern for O(1)
/// property lookup. The deepest slot any extractor reads is 4.
const PREINTERNED_SLOTS: usize = 8;

struct Ctx<'a> {
    graph: &'a IfcGraph,
    interner: &'a StringInterner,
    /// Outgoing edges per node, sorted by `(slot, list_index)`.
    out: AHashMap<NodeId, Vec<&'a Edge>>,
    /// Smallest referencing node per target — used by face extractors to
    /// find their owning face set (faces are referenced, never referencing).
    parent: AHashMap<NodeId, NodeId>,
    tol: &'a Tolerance,
    slot_keys: Vec<StringId>,
}

impl<'a> Ctx<'a> {
    fn new(graph: &'a IfcGraph, interner: &'a StringInterner, tol: &'a Tolerance) -> Self {
        let mut out: AHashMap<NodeId, Vec<&Edge>> = AHashMap::with_capacity(graph.node_count());
        let mut parent: AHashMap<NodeId, NodeId> = AHashMap::with_capacity(graph.node_count());
        for e in &graph.edges {
            out.entry(e.from).or_default().push(e);
            parent
                .entry(e.to)
                .and_modify(|p| {
                    if e.from < *p {
                        *p = e.from;
                    }
                })
                .or_insert(e.from);
        }
        for v in out.values_mut() {
            v.sort_by_key(|e| (e.slot, e.list_index));
        }
        let slot_keys = (0..PREINTERNED_SLOTS)
            .map(|i| interner.intern(&format!("_{i}")))
            .collect();
        Self {
            graph,
            interner,
            out,
            parent,
            tol,
            slot_keys,
        }
    }

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.graph.nodes.get(id)
    }

    /// Upper-cased type name of a node.
    fn type_of(&self, id: NodeId) -> Option<String> {
        self.node(id)
            .map(|n| self.interner.resolve(n.type_name).to_ascii_uppercase())
    }

    fn edges(&self, id: NodeId) -> &[&'a Edge] {
        self.out.get(&id).map_or(&[], Vec::as_slice)
    }

    /// First referenced node at positional `slot`.
    fn ref_at(&self, id: NodeId, slot: u16) -> Option<NodeId> {
        self.edges(id).iter().find(|e| e.slot == slot).map(|e| e.to)
    }

    /// All referenced nodes at positional `slot`, in list order.
    fn refs_at(&self, id: NodeId, slot: u16) -> Vec<NodeId> {
        self.edges(id)
            .iter()
            .filter(|e| e.slot == slot)
            .map(|e| e.to)
            .collect()
    }

    /// Property value stored at positional slot `slot` (key `_<slot>`).
    /// Key-based rather than positional lookup so profiles that drop
    /// property keys don't shift the meaning of later slots.
    fn prop(&self, id: NodeId, slot: u16) -> Option<&Value> {
        let node = self.node(id)?;
        if let Some(key) = self.slot_keys.get(slot as usize) {
            node.props.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            let key = format!("_{slot}");
            node.props
                .iter()
                .find(|(k, _)| self.interner.resolve(*k) == key)
                .map(|(_, v)| v)
        }
    }

    fn real_prop(&self, id: NodeId, slot: u16) -> Option<f64> {
        value_as_real(self.prop(id, slot)?)
    }
}

/// Compute a per-node geometry hash for every shape-bearing node.
#[must_use]
pub fn compute_geometry_hashes(
    graph: &IfcGraph,
    interner: &StringInterner,
    tol: &Tolerance,
) -> AHashMap<NodeId, Hash256> {
    let ctx = Ctx::new(graph, interner, tol);
    let mut memo: AHashMap<NodeId, Option<Hash256>> = AHashMap::new();
    let mut result: AHashMap<NodeId, Hash256> = AHashMap::new();
    for (id, _) in &graph.nodes {
        if let Some(h) = shape_hash(&ctx, id, &mut memo, 0) {
            result.insert(id, h);
        }
    }
    result
}

/// Memoized, cycle-safe shape hash. In-flight nodes are pre-seeded with
/// `None` so reference cycles resolve to "no geometry" instead of recursing.
fn shape_hash(
    ctx: &Ctx<'_>,
    id: NodeId,
    memo: &mut AHashMap<NodeId, Option<Hash256>>,
    depth: usize,
) -> Option<Hash256> {
    if depth > MAX_SHAPE_DEPTH {
        return None;
    }
    if let Some(h) = memo.get(&id) {
        return *h;
    }
    memo.insert(id, None);
    let h = shape_hash_inner(ctx, id, memo, depth);
    memo.insert(id, h);
    h
}

fn shape_hash_inner(
    ctx: &Ctx<'_>,
    id: NodeId,
    memo: &mut AHashMap<NodeId, Option<Hash256>>,
    depth: usize,
) -> Option<Hash256> {
    let type_name = ctx.type_of(id)?;
    match type_name.as_str() {
        // IfcRectangleProfileDef(ProfileType, ProfileName, Position, XDim, YDim)
        "IFCRECTANGLEPROFILEDEF" => {
            let x = ctx.real_prop(id, 3)?;
            let y = ctx.real_prop(id, 4)?;
            Some(vex_geometry::rect_profile(x, y, ctx.tol))
        }
        // IfcCircleProfileDef(ProfileType, ProfileName, Position, Radius)
        "IFCCIRCLEPROFILEDEF" => {
            let r = ctx.real_prop(id, 3)?;
            Some(vex_geometry::circle_profile(r, ctx.tol))
        }
        // IfcArbitraryClosedProfileDef(ProfileType, ProfileName, OuterCurve)
        "IFCARBITRARYCLOSEDPROFILEDEF" => {
            let curve = ctx.ref_at(id, 2)?;
            let pts = curve_points_2d(ctx, curve)?;
            Some(vex_geometry::arbitrary_profile(&pts, ctx.tol))
        }
        // IfcBlock(Position, XLength, YLength, ZLength)
        "IFCBLOCK" => {
            let x = ctx.real_prop(id, 1)?;
            let y = ctx.real_prop(id, 2)?;
            let z = ctx.real_prop(id, 3)?;
            Some(vex_geometry::block(x, y, z, ctx.tol))
        }
        // IfcRightCircularCylinder(Position, Height, Radius)
        "IFCRIGHTCIRCULARCYLINDER" => {
            let h = ctx.real_prop(id, 1)?;
            let r = ctx.real_prop(id, 2)?;
            Some(vex_geometry::right_circular_cylinder(h, r, ctx.tol))
        }
        // IfcExtrudedAreaSolid(SweptArea, Position, ExtrudedDirection, Depth)
        "IFCEXTRUDEDAREASOLID" => {
            let profile = ctx.ref_at(id, 0)?;
            let profile_hash = shape_hash(ctx, profile, memo, depth + 1)?;
            let extrude_depth = ctx.real_prop(id, 3)?;
            let direction = ctx
                .ref_at(id, 2)
                .and_then(|d| direction3(ctx, d))
                .unwrap_or([0.0, 0.0, 1.0]);
            Some(vex_geometry::extruded_area_solid(
                profile_hash,
                direction,
                extrude_depth,
                ctx.tol,
            ))
        }
        // IfcTriangulatedFaceSet(Coordinates, Normals, Closed, CoordIndex, PnIndex)
        "IFCTRIANGULATEDFACESET" => hash_triangulated(ctx, id),
        // IfcPolygonalFaceSet(Coordinates, Closed, Faces, PnIndex)
        "IFCPOLYGONALFACESET" => hash_polygonal(ctx, id),
        // IfcIndexedPolygonalFace(CoordIndex) — hashed as resolved point
        // rings so the raw indices (exporter-dependent) never leak into
        // structural identity.
        "IFCINDEXEDPOLYGONALFACE" | "IFCINDEXEDPOLYGONALFACEWITHVOIDS" => {
            hash_indexed_face(ctx, id, &type_name)
        }
        // IfcFacetedBrep(Outer) / IfcFacetedBrepWithVoids(Outer, Voids)
        "IFCFACETEDBREP" | "IFCFACETEDBREPWITHVOIDS" => hash_brep(ctx, id, &type_name),
        // Point-list carriers: row order is exporter noise once consumers
        // index canonically, so hash them as quantized multisets.
        "IFCCARTESIANPOINTLIST3D" => Some(vex_geometry::point_set_3d(
            &point_list_3d(ctx, id)?,
            ctx.tol,
        )),
        "IFCCARTESIANPOINTLIST2D" => Some(vex_geometry::point_set_2d(
            &point_list_2d(ctx, id)?,
            ctx.tol,
        )),
        _ => None,
    }
}

/// Property slots fully covered by a node's canonical geometry hash. When a
/// node carries a geometry hash, these slots are *excluded* from its WL seed
/// so that exporter-order noise (vertex tables, raw index lists, derived
/// normals) cannot perturb structural identity. Slots not listed here are
/// still hashed raw. Applied only when geometry extraction succeeded —
/// malformed shapes conservatively keep their raw property hash.
#[must_use]
pub fn consumed_prop_slots(type_name: &str) -> &'static [u16] {
    match type_name {
        // XDim, YDim
        "IFCRECTANGLEPROFILEDEF" => &[3, 4],
        // Radius / Depth / PnIndex
        "IFCCIRCLEPROFILEDEF" | "IFCEXTRUDEDAREASOLID" | "IFCPOLYGONALFACESET" => &[3],
        // XLength, YLength, ZLength
        "IFCBLOCK" => &[1, 2, 3],
        // Height, Radius
        "IFCRIGHTCIRCULARCYLINDER" => &[1, 2],
        // Normals (derived), CoordIndex, PnIndex
        "IFCTRIANGULATEDFACESET" => &[1, 3, 4],
        // CoordIndex / CoordList
        "IFCINDEXEDPOLYGONALFACE" | "IFCCARTESIANPOINTLIST3D" | "IFCCARTESIANPOINTLIST2D" => &[0],
        // CoordIndex, InnerCoordIndices
        "IFCINDEXEDPOLYGONALFACEWITHVOIDS" => &[0, 1],
        _ => &[],
    }
}

fn hash_triangulated(ctx: &Ctx<'_>, id: NodeId) -> Option<Hash256> {
    let coords = ctx.ref_at(id, 0)?;
    let verts = point_list_3d(ctx, coords)?;
    // Optional PnIndex (slot 4): 1-based indirection into the point list.
    let pn = ctx.prop(id, 4).and_then(int_list);
    let Value::List(rows) = strip_typed(ctx.prop(id, 3)?) else {
        return None;
    };
    let mut faces: Vec<[usize; 3]> = Vec::with_capacity(rows.len());
    for row in rows {
        let idx = int_list(row)?;
        if idx.len() != 3 {
            return None;
        }
        let mut tri = [0usize; 3];
        for (k, raw) in idx.iter().enumerate() {
            tri[k] = resolve_index(*raw, pn.as_deref(), verts.len())?;
        }
        faces.push(tri);
    }
    if faces.is_empty() {
        return None;
    }
    Some(vex_geometry::triangulated_face_set(&verts, &faces, ctx.tol))
}

fn hash_polygonal(ctx: &Ctx<'_>, id: NodeId) -> Option<Hash256> {
    let coords = ctx.ref_at(id, 0)?;
    let verts = point_list_3d(ctx, coords)?;
    let pn = ctx.prop(id, 3).and_then(int_list);
    let face_nodes = ctx.refs_at(id, 2);
    if face_nodes.is_empty() {
        return None;
    }
    let mut faces: Vec<Vec<usize>> = Vec::with_capacity(face_nodes.len());
    for f in face_nodes {
        match ctx.type_of(f)?.as_str() {
            // IfcIndexedPolygonalFace(CoordIndex)
            "IFCINDEXEDPOLYGONALFACE" => {
                faces.push(index_loop(ctx.prop(f, 0)?, pn.as_deref(), verts.len())?);
            }
            // IfcIndexedPolygonalFaceWithVoids(CoordIndex, InnerCoordIndices)
            "IFCINDEXEDPOLYGONALFACEWITHVOIDS" => {
                faces.push(index_loop(ctx.prop(f, 0)?, pn.as_deref(), verts.len())?);
                let Value::List(voids) = strip_typed(ctx.prop(f, 1)?) else {
                    return None;
                };
                for v in voids {
                    faces.push(index_loop(v, pn.as_deref(), verts.len())?);
                }
            }
            _ => return None,
        }
    }
    Some(vex_geometry::polygonal_face_set(&verts, &faces, ctx.tol))
}

/// Hash one `IfcIndexedPolygonalFace(WithVoids)` as resolved point rings.
/// The face's raw `CoordIndex` is meaningless without its owning face set's
/// vertex table, so we resolve through the parent (`IfcPolygonalFaceSet`
/// referencing this face) and hash actual coordinates — invariant to vertex
/// table permutation across re-exports.
fn hash_indexed_face(ctx: &Ctx<'_>, id: NodeId, type_name: &str) -> Option<Hash256> {
    let parent = ctx.parent.get(&id).copied()?;
    if ctx.type_of(parent)? != "IFCPOLYGONALFACESET" {
        return None;
    }
    let coords = ctx.ref_at(parent, 0)?;
    let verts = point_list_3d(ctx, coords)?;
    let pn = ctx.prop(parent, 3).and_then(int_list);
    let resolve = |v: &Value| -> Option<Vec<[f64; 3]>> {
        index_loop(v, pn.as_deref(), verts.len())
            .map(|ix| ix.into_iter().map(|i| verts[i]).collect())
    };
    let outer = resolve(ctx.prop(id, 0)?)?;
    let mut voids: Vec<Vec<[f64; 3]>> = Vec::new();
    if type_name == "IFCINDEXEDPOLYGONALFACEWITHVOIDS" {
        let Value::List(rows) = strip_typed(ctx.prop(id, 1)?) else {
            return None;
        };
        for row in rows {
            voids.push(resolve(row)?);
        }
    }
    Some(vex_geometry::face_ring_3d(&outer, &voids, ctx.tol))
}

fn hash_brep(ctx: &Ctx<'_>, id: NodeId, type_name: &str) -> Option<Hash256> {
    let mut shells = vec![ctx.ref_at(id, 0)?];
    if type_name.ends_with("WITHVOIDS") {
        shells.extend(ctx.refs_at(id, 1));
    }
    // Collect every loop point; duplicates are collapsed canonically inside
    // vex_geometry's vertex table, so a simple append is correct.
    let mut verts: Vec<[f64; 3]> = Vec::new();
    let mut faces: Vec<Vec<usize>> = Vec::new();
    for shell in shells {
        // IfcClosedShell(CfsFaces)
        for face in ctx.refs_at(shell, 0) {
            // IfcFace(Bounds)
            for bound in ctx.refs_at(face, 0) {
                // IfcFaceBound / IfcFaceOuterBound (Bound, Orientation)
                let loop_node = ctx.ref_at(bound, 0)?;
                if ctx.type_of(loop_node)? != "IFCPOLYLOOP" {
                    return None;
                }
                // IfcPolyLoop(Polygon)
                let mut idxs: Vec<usize> = Vec::new();
                for pt in ctx.refs_at(loop_node, 0) {
                    let p = cartesian_point_3(ctx, pt)?;
                    idxs.push(verts.len());
                    verts.push(p);
                }
                if idxs.len() < 3 {
                    return None;
                }
                // Orientation = false means the loop runs opposite the face
                // normal; normalise to forward order.
                if matches!(ctx.prop(bound, 1), Some(Value::Bool(false))) {
                    idxs.reverse();
                }
                faces.push(idxs);
            }
        }
    }
    if faces.is_empty() {
        return None;
    }
    Some(vex_geometry::faceted_brep(&verts, &faces, ctx.tol))
}

// -------- component readers --------

/// Points of a profile outer curve, as 2D coordinates.
fn curve_points_2d(ctx: &Ctx<'_>, id: NodeId) -> Option<Vec<[f64; 2]>> {
    let mut pts: Vec<[f64; 2]> = match ctx.type_of(id)?.as_str() {
        // IfcPolyline(Points)
        "IFCPOLYLINE" => ctx
            .refs_at(id, 0)
            .iter()
            .map(|&p| cartesian_point_2(ctx, p))
            .collect::<Option<Vec<_>>>()?,
        // IfcIndexedPolyCurve(Points, Segments, SelfIntersect) — only the
        // segment-free form (pure point sequence); arc segments would need
        // tessellation, which is out of scope for hashing.
        "IFCINDEXEDPOLYCURVE" => {
            if ctx.prop(id, 1).is_some_and(|v| !matches!(v, Value::Null)) {
                return None;
            }
            point_list_2d(ctx, ctx.ref_at(id, 0)?)?
        }
        _ => return None,
    };
    // Closed polylines conventionally repeat the first point; drop the
    // trailing duplicate so both encodings hash identically.
    if pts.len() >= 2 {
        let first = pts[0];
        let last = pts[pts.len() - 1];
        let q = |x: f64| ctx.tol.quantize_linear(x).to_bits();
        if q(first[0]) == q(last[0]) && q(first[1]) == q(last[1]) {
            pts.pop();
        }
    }
    if pts.len() < 3 {
        return None;
    }
    Some(pts)
}

/// IfcCartesianPointList3D(CoordList) → rows of `[x, y, z]`.
fn point_list_3d(ctx: &Ctx<'_>, id: NodeId) -> Option<Vec<[f64; 3]>> {
    if ctx.type_of(id)? != "IFCCARTESIANPOINTLIST3D" {
        return None;
    }
    let Value::List(rows) = strip_typed(ctx.prop(id, 0)?) else {
        return None;
    };
    rows.iter()
        .map(|row| {
            let r = real_list(row)?;
            Some([
                r.first().copied()?,
                r.get(1).copied()?,
                r.get(2).copied().unwrap_or(0.0),
            ])
        })
        .collect()
}

/// IfcCartesianPointList2D(CoordList) → rows of `[x, y]`.
fn point_list_2d(ctx: &Ctx<'_>, id: NodeId) -> Option<Vec<[f64; 2]>> {
    if ctx.type_of(id)? != "IFCCARTESIANPOINTLIST2D" {
        return None;
    }
    let Value::List(rows) = strip_typed(ctx.prop(id, 0)?) else {
        return None;
    };
    rows.iter()
        .map(|row| {
            let r = real_list(row)?;
            Some([r.first().copied()?, r.get(1).copied()?])
        })
        .collect()
}

/// IfcCartesianPoint(Coordinates) → `[x, y, z]` (2D points padded with z=0).
fn cartesian_point_3(ctx: &Ctx<'_>, id: NodeId) -> Option<[f64; 3]> {
    if ctx.type_of(id)? != "IFCCARTESIANPOINT" {
        return None;
    }
    let r = real_list(ctx.prop(id, 0)?)?;
    Some([
        r.first().copied()?,
        r.get(1).copied()?,
        r.get(2).copied().unwrap_or(0.0),
    ])
}

/// IfcCartesianPoint(Coordinates) → `[x, y]`.
fn cartesian_point_2(ctx: &Ctx<'_>, id: NodeId) -> Option<[f64; 2]> {
    let p = cartesian_point_3(ctx, id)?;
    Some([p[0], p[1]])
}

/// IfcDirection(DirectionRatios) → `[x, y, z]` (2D directions padded).
fn direction3(ctx: &Ctx<'_>, id: NodeId) -> Option<[f64; 3]> {
    if ctx.type_of(id)? != "IFCDIRECTION" {
        return None;
    }
    let r = real_list(ctx.prop(id, 0)?)?;
    Some([
        r.first().copied()?,
        r.get(1).copied()?,
        r.get(2).copied().unwrap_or(0.0),
    ])
}

/// Resolve a 1-based STEP index, applying optional 1-based `PnIndex`
/// indirection, into a 0-based vertex index. `None` on any out-of-range or
/// non-positive value.
fn resolve_index(raw: i64, pn: Option<&[i64]>, n_verts: usize) -> Option<usize> {
    let i = usize::try_from(raw.checked_sub(1)?).ok()?;
    let v = match pn {
        Some(p) => usize::try_from(p.get(i).copied()?.checked_sub(1)?).ok()?,
        None => i,
    };
    (v < n_verts).then_some(v)
}

/// A face index loop: 1-based int list (with optional `PnIndex` indirection)
/// → 0-based vertex indices.
fn index_loop(v: &Value, pn: Option<&[i64]>, n_verts: usize) -> Option<Vec<usize>> {
    let idx = int_list(v)?;
    if idx.len() < 3 {
        return None;
    }
    idx.iter()
        .map(|&raw| resolve_index(raw, pn, n_verts))
        .collect()
}

fn strip_typed(v: &Value) -> &Value {
    match v {
        Value::Typed { inner, .. } => strip_typed(inner),
        _ => v,
    }
}

fn real_list(v: &Value) -> Option<Vec<f64>> {
    match strip_typed(v) {
        Value::List(xs) => xs.iter().map(value_as_real).collect(),
        _ => None,
    }
}

fn int_list(v: &Value) -> Option<Vec<i64>> {
    match strip_typed(v) {
        Value::List(xs) => xs
            .iter()
            .map(|x| match strip_typed(x) {
                Value::Int(n) => Some(*n),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn value_as_real(v: &Value) -> Option<f64> {
    match v {
        Value::Real(x) => Some(*x),
        Value::Int(n) => Some(*n as f64),
        Value::Typed { inner, .. } => value_as_real(inner),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use std::io::Cursor;
    use vex_ifc_parser::{ParseLimits, Parser};

    const BLOCK_IFC: &str = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = IFCCARTESIANPOINT((0.0, 0.0, 0.0));
#2 = IFCAXIS2PLACEMENT3D(#1, $, $);
#3 = IFCBLOCK(#2, 1.0, 2.0, 3.0);
ENDSEC;
END-ISO-10303-21;
";

    fn build(src: &str) -> (IfcGraph, StringInterner) {
        let interner = StringInterner::new();
        let mut parser = Parser::new(Cursor::new(src), ParseLimits::default());
        let g = GraphBuilder::build_from_parser(interner.clone(), &mut parser).expect("build");
        (g, interner)
    }

    #[test]
    fn block_gets_geometry_hash() {
        let (g, i) = build(BLOCK_IFC);
        let tol = Tolerance::default();
        let h = compute_geometry_hashes(&g, &i, &tol);
        assert_eq!(h.len(), 1, "only the IFCBLOCK should get a geometry hash");
    }

    #[test]
    fn block_dimensions_affect_geometry_hash() {
        let (g1, i1) = build(BLOCK_IFC);
        let bigger = BLOCK_IFC.replace("1.0, 2.0, 3.0", "1.5, 2.0, 3.0");
        let (g2, i2) = build(&bigger);
        let tol = Tolerance::default();
        let h1 = compute_geometry_hashes(&g1, &i1, &tol);
        let h2 = compute_geometry_hashes(&g2, &i2, &tol);
        let v1 = h1.values().next().copied().expect("hash");
        let v2 = h2.values().next().copied().expect("hash");
        assert_ne!(v1, v2);
    }

    #[test]
    fn tiny_exporter_noise_absorbed_by_tolerance() {
        let (g1, i1) = build(BLOCK_IFC);
        let noisy = BLOCK_IFC.replace("1.0, 2.0, 3.0", "1.0000001, 2.0, 3.0");
        let (g2, i2) = build(&noisy);
        let tol = Tolerance::default(); // 1 µm linear
        let h1 = compute_geometry_hashes(&g1, &i1, &tol);
        let h2 = compute_geometry_hashes(&g2, &i2, &tol);
        assert_eq!(h1.values().next(), h2.values().next());
    }

    fn wrap(data: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER; FILE_DESCRIPTION((''),'2;1'); \
             FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;\nDATA;\n{data}\nENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    fn hashes_for(src: &str) -> AHashMap<NodeId, Hash256> {
        let (g, i) = build(src);
        compute_geometry_hashes(&g, &i, &Tolerance::default())
    }

    /// Geometry hash of the first node with the given type name.
    fn hash_of(src: &str, ty: &str) -> Option<Hash256> {
        let (g, i) = build(src);
        let h = compute_geometry_hashes(&g, &i, &Tolerance::default());
        g.nodes
            .iter()
            .find(|(_, n)| i.resolve(n.type_name).eq_ignore_ascii_case(ty))
            .and_then(|(id, _)| h.get(&id).copied())
    }

    const TRI_MESH: &str = "\
#1 = IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(1.0,0.0,0.0),(1.0,1.0,0.0),(0.0,1.0,0.0)));
#2 = IFCTRIANGULATEDFACESET(#1, $, .T., ((1,2,3),(1,3,4)), $);";

    #[test]
    fn triangulated_face_set_gets_hash() {
        let src = wrap(TRI_MESH);
        assert!(hash_of(&src, "IFCTRIANGULATEDFACESET").is_some());
        assert!(
            hash_of(&src, "IFCCARTESIANPOINTLIST3D").is_some(),
            "point list carriers get an order-invariant hash too"
        );
    }

    #[test]
    fn triangulated_vertex_move_changes_hash() {
        let a = hash_of(&wrap(TRI_MESH), "IFCTRIANGULATEDFACESET");
        let moved = wrap(&TRI_MESH.replace("(1.0,1.0,0.0)", "(1.0,1.5,0.0)"));
        let b = hash_of(&moved, "IFCTRIANGULATEDFACESET");
        assert_ne!(a, b);
    }

    #[test]
    fn triangulated_point_permutation_stable() {
        // Reverse the point list and remap CoordIndex (1-based) accordingly:
        // old 1,2,3,4 -> new 4,3,2,1.
        let permuted = wrap(
            "#1 = IFCCARTESIANPOINTLIST3D(((0.0,1.0,0.0),(1.0,1.0,0.0),(1.0,0.0,0.0),(0.0,0.0,0.0)));
#2 = IFCTRIANGULATEDFACESET(#1, $, .T., ((4,3,2),(4,2,1)), $);",
        );
        let base = wrap(TRI_MESH);
        assert_eq!(
            hash_of(&base, "IFCTRIANGULATEDFACESET"),
            hash_of(&permuted, "IFCTRIANGULATEDFACESET")
        );
        assert_eq!(
            hash_of(&base, "IFCCARTESIANPOINTLIST3D"),
            hash_of(&permuted, "IFCCARTESIANPOINTLIST3D"),
            "point multiset hash must be row-order invariant"
        );
    }

    #[test]
    fn triangulated_pn_index_indirection() {
        // PnIndex maps face indices 1..3 to points 1, 2, 3 — same triangle as
        // direct indexing, so the hash must match the unindirected form.
        let direct = wrap(
            "#1 = IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(1.0,0.0,0.0),(1.0,1.0,0.0)));
#2 = IFCTRIANGULATEDFACESET(#1, $, .T., ((1,2,3)), $);",
        );
        let indirect = wrap(
            "#1 = IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(1.0,0.0,0.0),(1.0,1.0,0.0)));
#2 = IFCTRIANGULATEDFACESET(#1, $, .T., ((1,2,3)), (1,2,3));",
        );
        assert_eq!(
            hash_of(&direct, "IFCTRIANGULATEDFACESET"),
            hash_of(&indirect, "IFCTRIANGULATEDFACESET")
        );
    }

    #[test]
    fn triangulated_out_of_range_index_yields_no_hash() {
        let bad = wrap(
            "#1 = IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(1.0,0.0,0.0)));
#2 = IFCTRIANGULATEDFACESET(#1, $, .T., ((1,2,9)), $);",
        );
        assert!(hash_of(&bad, "IFCTRIANGULATEDFACESET").is_none());
    }

    #[test]
    fn polygonal_face_set_gets_hash() {
        let src = wrap(
            "#1 = IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(1.0,0.0,0.0),(1.0,1.0,0.0),(0.0,1.0,0.0)));
#2 = IFCINDEXEDPOLYGONALFACE((1,2,3,4));
#3 = IFCPOLYGONALFACESET(#1, .T., (#2), $);",
        );
        assert!(hash_of(&src, "IFCPOLYGONALFACESET").is_some());
        assert!(
            hash_of(&src, "IFCINDEXEDPOLYGONALFACE").is_some(),
            "faces hash as resolved point rings via their parent face set"
        );
    }

    #[test]
    fn polygonal_with_voids_differs_from_without() {
        let solid = wrap(
            "#1 = IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(4.0,0.0,0.0),(4.0,4.0,0.0),(0.0,4.0,0.0),(1.0,1.0,0.0),(2.0,1.0,0.0),(2.0,2.0,0.0),(1.0,2.0,0.0)));
#2 = IFCINDEXEDPOLYGONALFACE((1,2,3,4));
#3 = IFCPOLYGONALFACESET(#1, .T., (#2), $);",
        );
        let voided = wrap(
            "#1 = IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(4.0,0.0,0.0),(4.0,4.0,0.0),(0.0,4.0,0.0),(1.0,1.0,0.0),(2.0,1.0,0.0),(2.0,2.0,0.0),(1.0,2.0,0.0)));
#2 = IFCINDEXEDPOLYGONALFACEWITHVOIDS((1,2,3,4),((5,6,7,8)));
#3 = IFCPOLYGONALFACESET(#1, .T., (#2), $);",
        );
        let a = hashes_for(&solid);
        let b = hashes_for(&voided);
        assert!(!a.is_empty() && !b.is_empty());
        let ha = hash_of(&solid, "IFCPOLYGONALFACESET").expect("solid hash");
        let hb = hash_of(&voided, "IFCPOLYGONALFACESET").expect("voided hash");
        assert_ne!(ha, hb);
    }

    const BREP: &str = "\
#1 = IFCCARTESIANPOINT((0.0,0.0,0.0));
#2 = IFCCARTESIANPOINT((1.0,0.0,0.0));
#3 = IFCCARTESIANPOINT((1.0,1.0,0.0));
#4 = IFCPOLYLOOP((#1,#2,#3));
#5 = IFCFACEOUTERBOUND(#4, .T.);
#6 = IFCFACE((#5));
#7 = IFCCLOSEDSHELL((#6));
#8 = IFCFACETEDBREP(#7);";

    #[test]
    fn faceted_brep_gets_hash() {
        let h = hashes_for(&wrap(BREP));
        assert_eq!(h.len(), 1, "only the brep should get a geometry hash");
    }

    #[test]
    fn faceted_brep_vertex_move_changes_hash() {
        let a = hash_of(&wrap(BREP), "IFCFACETEDBREP");
        let moved = wrap(&BREP.replace("((1.0,1.0,0.0))", "((1.0,1.2,0.0))"));
        let b = hash_of(&moved, "IFCFACETEDBREP");
        assert_ne!(a, b);
    }

    #[test]
    fn arbitrary_profile_and_extrusion() {
        let src = wrap(
            "#1 = IFCCARTESIANPOINT((0.0,0.0));
#2 = IFCCARTESIANPOINT((2.0,0.0));
#3 = IFCCARTESIANPOINT((2.0,1.0));
#4 = IFCCARTESIANPOINT((0.0,1.0));
#5 = IFCPOLYLINE((#1,#2,#3,#4,#1));
#6 = IFCARBITRARYCLOSEDPROFILEDEF(.AREA., $, #5);
#7 = IFCDIRECTION((0.0,0.0,1.0));
#8 = IFCEXTRUDEDAREASOLID(#6, $, #7, 3.0);",
        );
        let h = hashes_for(&src);
        // Profile + extrusion both get hashes.
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn extrusion_direction_affects_hash() {
        let base = "\
#1 = IFCRECTANGLEPROFILEDEF(.AREA., $, $, 1.0, 2.0);
#2 = IFCDIRECTION((0.0,0.0,1.0));
#3 = IFCEXTRUDEDAREASOLID(#1, $, #2, 3.0);";
        let tilted = base.replace("IFCDIRECTION((0.0,0.0,1.0))", "IFCDIRECTION((1.0,0.0,0.0))");
        let (ga, ia) = build(&wrap(base));
        let (gb, ib) = build(&wrap(&tilted));
        let tol = Tolerance::default();
        let ha = compute_geometry_hashes(&ga, &ia, &tol);
        let hb = compute_geometry_hashes(&gb, &ib, &tol);
        // Find the extrusion node hash on each side (the one that is not the
        // profile hash, which is identical across both).
        let pa = vex_geometry::rect_profile(1.0, 2.0, &tol);
        let ea: Vec<_> = ha.values().filter(|h| **h != pa).collect();
        let eb: Vec<_> = hb.values().filter(|h| **h != pa).collect();
        assert_eq!(ea.len(), 1);
        assert_eq!(eb.len(), 1);
        assert_ne!(ea[0], eb[0]);
    }

    #[test]
    fn rect_profile_uses_correct_slots() {
        // IfcRectangleProfileDef(ProfileType, ProfileName, Position, XDim, YDim)
        // — XDim/YDim are slots 3 and 4, *after* the Position reference.
        let src = wrap("#1 = IFCRECTANGLEPROFILEDEF(.AREA., $, $, 1.5, 2.5);");
        let h = hashes_for(&src);
        let expect = vex_geometry::rect_profile(1.5, 2.5, &Tolerance::default());
        assert_eq!(h.values().next(), Some(&expect));
    }

    #[test]
    fn mesh_change_propagates_to_product_wl_hash() {
        // Full product chain: wall -> product definition shape -> shape
        // representation -> brep. A brep vertex move must change the WALL's
        // WL hash within the default round budget.
        let chain = |z: &str| {
            wrap(&format!(
                "#1 = IFCCARTESIANPOINT((0.0,0.0,0.0));
#2 = IFCCARTESIANPOINT((1.0,0.0,0.0));
#3 = IFCCARTESIANPOINT((1.0,1.0,{z}));
#4 = IFCPOLYLOOP((#1,#2,#3));
#5 = IFCFACEOUTERBOUND(#4, .T.);
#6 = IFCFACE((#5));
#7 = IFCCLOSEDSHELL((#6));
#8 = IFCFACETEDBREP(#7);
#9 = IFCSHAPEREPRESENTATION($, 'Body', 'Brep', (#8));
#10 = IFCPRODUCTDEFINITIONSHAPE($, $, (#9));
#11 = IFCWALL('2O2Fr$t4X7Zf8NOew3FNr2', $, 'Wall-1', $, $, $, #10, $, .STANDARD.);"
            ))
        };
        let (ga, ia) = build(&chain("0.0"));
        let (gb, ib) = build(&chain("0.7"));
        let cfg = crate::merkle::HashConfig::default();
        let ha = crate::merkle::hash_graph(&ga, &ia, &cfg);
        let hb = crate::merkle::hash_graph(&gb, &ib, &cfg);
        let wall_hash = |g: &IfcGraph, h: &crate::merkle::GraphHashes| {
            g.nodes
                .iter()
                .find(|(_, n)| n.global_id.is_some())
                .map(|(id, _)| h.per_node[&id])
                .expect("wall node")
        };
        assert_ne!(wall_hash(&ga, &ha), wall_hash(&gb, &hb));
        // And the geometry map must mark exactly the brep node.
        assert_eq!(ha.geometry.len(), 1);
    }
}
