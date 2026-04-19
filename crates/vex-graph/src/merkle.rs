//! Canonical Merkle hashing of the graph.
//!
//! Algorithm (Weisfeiler-Lehman-inspired):
//!
//! 1. Quantize floats per the [`Tolerance`] config so exporter precision noise
//!    doesn't affect the hash.
//! 2. Hash each node's "local" content (type + sorted props) to seed the table.
//! 3. Iteratively refine each node's hash by mixing in the sorted multiset of
//!    `(edge_kind, slot, list_index, neighbor_hash)` tuples from its outgoing
//!    edges. Repeat for `depth` rounds (we use 3, which empirically suffices
//!    to distinguish isomorphically-distinct subgraphs for IFC's shape space).
//! 4. Cycles are handled naturally: the iterative fixed-point lets cycle
//!    members converge to stable mutual hashes.
//!
//! The result is a `Hash256` per node. The *graph* hash is the Merkle root
//! over the sorted list of `(node_hash)` entries — it's invariant to node
//! insertion order.

use ahash::AHashMap;
use rayon::prelude::*;
use vex_utils::hash::HashAlgo;
use vex_utils::{hash::hash_default, Hash256, Hasher, Profile, StringInterner, Tolerance};

use crate::geometry;
use crate::ir::{Edge, IfcGraph, NodeId, Value};

/// How many WL refinement rounds to run. Three rounds is sufficient for
/// distinguishing distinct subgraph shapes in the IFC context where most
/// meaningful neighborhoods are shallow.
pub const WL_ROUNDS: usize = 3;

/// Configuration for the canonical hash pass.
#[derive(Debug, Clone)]
pub struct HashConfig {
    pub tolerance: Tolerance,
    pub rounds: usize,
    /// Property keys to skip during hashing. Default: empty.
    pub ignore_prop_keys: std::collections::BTreeSet<String>,
}

impl Default for HashConfig {
    fn default() -> Self {
        Self {
            tolerance: Tolerance::default(),
            rounds: WL_ROUNDS,
            ignore_prop_keys: std::collections::BTreeSet::new(),
        }
    }
}

impl HashConfig {
    /// Build a hash config from a normalization profile.
    #[must_use]
    pub fn from_profile(p: &Profile) -> Self {
        Self {
            tolerance: p.tolerance(),
            rounds: WL_ROUNDS,
            ignore_prop_keys: p.ignore_prop_keys.clone(),
        }
    }
}

/// Result of a canonicalization + hashing pass.
#[derive(Debug)]
pub struct GraphHashes {
    pub per_node: AHashMap<NodeId, Hash256>,
    pub root: Hash256,
}

/// Compute canonical hashes for every node and a root hash for the graph.
pub fn hash_graph(graph: &IfcGraph, interner: &StringInterner, cfg: HashConfig) -> GraphHashes {
    // Geometry hashes are computed up-front and folded into each node seed so
    // that shape changes propagate through the WL refinement rounds.
    let geom = geometry::compute_geometry_hashes(graph, interner, &cfg.tolerance);

    // Seed: local content hash per node (type + quantized props + geometry).
    // Parallelise over nodes — seeding is embarrassingly parallel.
    let seed_vec: Vec<(NodeId, Hash256)> = graph
        .nodes
        .iter()
        .collect::<Vec<_>>()
        .par_iter()
        .map(|(id, node)| {
            let mut h = Hasher::new(HashAlgo::Blake3);
            h.update(b"node:");
            h.update(interner.resolve(node.type_name).as_bytes());
            let mut props: Vec<_> = node
                .props
                .iter()
                .filter(|(k, _)| !cfg.ignore_prop_keys.contains(interner.resolve(*k)))
                .collect();
            props.sort_by_key(|(k, _)| interner.resolve(*k));
            for (k, v) in props {
                h.update(b"\0k:");
                h.update(interner.resolve(*k).as_bytes());
                h.update(b"\0v:");
                hash_value(&mut h, v, interner, &cfg.tolerance);
            }
            if let Some(gh) = geom.get(id) {
                h.update(b"\0geom:");
                h.update(gh.as_bytes());
            }
            (*id, h.finalize())
        })
        .collect();
    let mut current: AHashMap<NodeId, Hash256> = AHashMap::with_capacity(seed_vec.len());
    for (id, h) in seed_vec {
        current.insert(id, h);
    }

    // WL rounds: mix in neighbor hashes.
    // Pre-bucket outgoing edges per node for efficiency.
    let mut out: AHashMap<NodeId, Vec<&Edge>> = AHashMap::with_capacity(graph.node_count());
    for e in &graph.edges {
        out.entry(e.from).or_default().push(e);
    }

    for _round in 0..cfg.rounds {
        let node_ids: Vec<NodeId> = graph.nodes.iter().map(|(id, _)| id).collect();
        let next_vec: Vec<(NodeId, Hash256)> = node_ids
            .par_iter()
            .map(|id| {
                let mut h = Hasher::new(HashAlgo::Blake3);
                h.update(b"wl:");
                h.update(current[id].as_bytes());
                if let Some(edges) = out.get(id) {
                    let mut tuples: Vec<(u8, u16, u16, [u8; 32])> = edges
                        .iter()
                        .map(|e| {
                            let k = e.kind as u8;
                            let nh = current.get(&e.to).copied().unwrap_or(Hash256::ZERO);
                            (k, e.slot, e.list_index, *nh.as_bytes())
                        })
                        .collect();
                    tuples.sort();
                    for (k, slot, li, nh) in tuples {
                        h.update(&[k]);
                        h.update(&slot.to_be_bytes());
                        h.update(&li.to_be_bytes());
                        h.update(&nh);
                    }
                }
                (*id, h.finalize())
            })
            .collect();
        let mut next: AHashMap<NodeId, Hash256> = AHashMap::with_capacity(next_vec.len());
        for (id, h) in next_vec {
            next.insert(id, h);
        }
        current = next;
    }

    // Merkle root over sorted per-node hashes.
    let mut all: Vec<[u8; 32]> = current.values().map(|h| *h.as_bytes()).collect();
    all.sort();
    let mut h = Hasher::new(HashAlgo::Blake3);
    h.update(b"root:");
    h.update(&(all.len() as u64).to_be_bytes());
    for nh in &all {
        h.update(nh);
    }
    let root = h.finalize();

    GraphHashes {
        per_node: current,
        root,
    }
}

fn hash_value(h: &mut Hasher, v: &Value, interner: &StringInterner, tol: &Tolerance) {
    match v {
        Value::Null => {
            h.update(b"n");
        }
        Value::Bool(b) => {
            h.update(b"b");
            h.update(&[u8::from(*b)]);
        }
        Value::Int(n) => {
            h.update(b"i");
            h.update(&n.to_be_bytes());
        }
        Value::Real(x) => {
            h.update(b"r");
            let q = tol.quantize_linear(*x);
            // Normalise -0.0 to 0.0 for stable hashing.
            let q = if q == 0.0 { 0.0 } else { q };
            h.update(&q.to_bits().to_be_bytes());
        }
        Value::Text(s) => {
            h.update(b"s");
            let bytes = interner.resolve(*s).as_bytes();
            h.update(&(bytes.len() as u32).to_be_bytes());
            h.update(bytes);
        }
        Value::Enum(s) => {
            h.update(b"e");
            let bytes = interner.resolve(*s).as_bytes();
            h.update(&(bytes.len() as u32).to_be_bytes());
            h.update(bytes);
        }
        Value::List(xs) => {
            h.update(b"l");
            h.update(&(xs.len() as u32).to_be_bytes());
            for x in xs {
                hash_value(h, x, interner, tol);
            }
        }
        Value::Typed { name, inner } => {
            h.update(b"t");
            let nb = interner.resolve(*name).as_bytes();
            h.update(&(nb.len() as u32).to_be_bytes());
            h.update(nb);
            hash_value(h, inner, interner, tol);
        }
    }
}

/// One-shot convenience: hash a byte slice with the default algo.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> Hash256 {
    hash_default(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::GraphBuilder;
    use std::io::Cursor;
    use vex_ifc_parser::{ParseLimits, Parser};

    fn graph_from(src: &str) -> (IfcGraph, StringInterner) {
        let interner = StringInterner::new();
        let mut parser = Parser::new(Cursor::new(src), ParseLimits::default());
        let g = GraphBuilder::build_from_parser(interner.clone(), &mut parser).expect("build");
        (g, interner)
    }

    const BASE: &str = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = IFCPROJECT('0YvctVUKr0kugbFTf53O9L',$,'Project',$,$,$,$,(#2),$);
#2 = IFCSITE('1YvctVUKr0kugbFTf53O9M',$,'Site',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
ENDSEC;
END-ISO-10303-21;
";

    #[test]
    fn identical_inputs_produce_identical_root() {
        let (g1, i1) = graph_from(BASE);
        let (g2, i2) = graph_from(BASE);
        let h1 = hash_graph(&g1, &i1, HashConfig::default());
        let h2 = hash_graph(&g2, &i2, HashConfig::default());
        assert_eq!(h1.root, h2.root);
    }

    #[test]
    fn attribute_change_changes_root() {
        let (g1, i1) = graph_from(BASE);
        let mutated = BASE.replace("'Project'", "'ProjectX'");
        let (g2, i2) = graph_from(&mutated);
        let h1 = hash_graph(&g1, &i1, HashConfig::default());
        let h2 = hash_graph(&g2, &i2, HashConfig::default());
        assert_ne!(h1.root, h2.root);
    }

    #[test]
    fn tolerance_absorbs_small_float_noise() {
        let a = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = IFCCARTESIANPOINT((1.0, 2.0, 3.0));
ENDSEC;
END-ISO-10303-21;
";
        // Sub-tolerance change in the last decimal place.
        let b = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = IFCCARTESIANPOINT((1.00000000001, 2.0, 3.0));
ENDSEC;
END-ISO-10303-21;
";
        let (g1, i1) = graph_from(a);
        let (g2, i2) = graph_from(b);
        let cfg = HashConfig {
            tolerance: Tolerance::new(1e-6, 1e-6),
            ..HashConfig::default()
        };
        let h1 = hash_graph(&g1, &i1, cfg.clone());
        let h2 = hash_graph(&g2, &i2, cfg);
        assert_eq!(h1.root, h2.root);
    }
}
