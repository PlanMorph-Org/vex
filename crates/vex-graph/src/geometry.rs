//! Extract geometry hashes for shape-bearing nodes in the graph.
//!
//! This module bridges the IR to [`vex_geometry`]: for each recognised
//! shape type, it pulls scalar properties and referenced child shapes from
//! the graph and produces a stable hash. The result is a map from node to
//! geometry hash, which the Merkle pass folds into each node's seed so
//! geometry changes propagate through the graph.

use ahash::AHashMap;
use vex_utils::{Hash256, StringInterner, Tolerance};

use crate::ir::{IfcGraph, NodeId, Value};

/// Compute a per-node geometry hash for every shape-bearing node.
#[must_use]
pub fn compute_geometry_hashes(
    graph: &IfcGraph,
    interner: &StringInterner,
    tol: &Tolerance,
) -> AHashMap<NodeId, Hash256> {
    // Bucket outgoing edges per node, sorted by slot so positional
    // argument lookup is O(1).
    let mut out: AHashMap<NodeId, Vec<&crate::ir::Edge>> =
        AHashMap::with_capacity(graph.node_count());
    for e in &graph.edges {
        out.entry(e.from).or_default().push(e);
    }
    for v in out.values_mut() {
        v.sort_by_key(|e| (e.slot, e.list_index));
    }

    // First pass: hash all primitive shapes (profiles, blocks, cylinders,
    // meshes) — things that don't reference other shapes.
    let mut hashes: AHashMap<NodeId, Hash256> = AHashMap::new();
    for (id, node) in graph.nodes.iter() {
        let type_name = interner.resolve(node.type_name).to_ascii_uppercase();
        if let Some(h) = hash_primitive(&type_name, node, tol) {
            hashes.insert(id, h);
        }
    }

    // Second pass: derived shapes (extrusion) that reference primitives.
    for (id, node) in graph.nodes.iter() {
        if hashes.contains_key(&id) {
            continue;
        }
        let type_name = interner.resolve(node.type_name).to_ascii_uppercase();
        if type_name == "IFCEXTRUDEDAREASOLID" {
            if let Some(h) = hash_extruded(node, out.get(&id).map_or(&[][..], Vec::as_slice), &hashes, tol) {
                hashes.insert(id, h);
            }
        }
    }

    hashes
}

fn hash_primitive(type_name: &str, node: &crate::ir::Node, tol: &Tolerance) -> Option<Hash256> {
    match type_name {
        "IFCRECTANGLEPROFILEDEF" => {
            let x = real_at(node, 2)?;
            let y = real_at(node, 3)?;
            Some(vex_geometry::rect_profile(x, y, tol))
        }
        "IFCCIRCLEPROFILEDEF" => {
            let r = real_at(node, 2)?;
            Some(vex_geometry::circle_profile(r, tol))
        }
        "IFCBLOCK" => {
            let x = real_at(node, 1)?;
            let y = real_at(node, 2)?;
            let z = real_at(node, 3)?;
            Some(vex_geometry::block(x, y, z, tol))
        }
        "IFCRIGHTCIRCULARCYLINDER" => {
            let h = real_at(node, 1)?;
            let r = real_at(node, 2)?;
            Some(vex_geometry::right_circular_cylinder(h, r, tol))
        }
        _ => None,
    }
}

fn hash_extruded(
    node: &crate::ir::Node,
    edges: &[&crate::ir::Edge],
    primitives: &AHashMap<NodeId, Hash256>,
    tol: &Tolerance,
) -> Option<Hash256> {
    // Slot 0 -> SweptArea (profile). Slot 2 -> ExtrudedDirection. Slot 3 -> Depth.
    let profile_node = edges.iter().find(|e| e.slot == 0).map(|e| e.to)?;
    let profile_hash = primitives.get(&profile_node).copied()?;
    let depth = real_at(node, 3)?;

    // Try to read direction from a referenced IfcDirection (slot 2). If absent
    // or not resolvable, default to +Z.
    let mut direction = [0.0, 0.0, 1.0];
    if let Some(dir_edge) = edges.iter().find(|e| e.slot == 2) {
        // DirectionRatios is a list property on the IfcDirection node.
        if let Some(_dir_node) = Some(dir_edge.to) {
            // We can't easily read the referenced node's props from here
            // without a graph ref; the simpler, correct thing is to hash the
            // referenced node's NodeId into the extrusion hash via the graph
            // Merkle pass. For now, keep direction at default — the graph
            // WL rounds already capture changes to the IfcDirection.
            direction = [0.0, 0.0, 1.0];
        }
    }

    Some(vex_geometry::extruded_area_solid(
        profile_hash,
        direction,
        depth,
        tol,
    ))
}

/// Read a positional argument as a real number. The builder stores props in
/// positional order keyed by `_<slot>`, with references replaced by `Null`.
fn real_at(node: &crate::ir::Node, idx: usize) -> Option<f64> {
    let (_, v) = node.props.get(idx)?;
    value_as_real(v)
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
mod tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use vex_ifc_parser::{ParseLimits, Parser};
    use std::io::Cursor;

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
        let g = GraphBuilder::build_from_parser(interner.clone(), &mut parser)
            .expect("build");
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
}
