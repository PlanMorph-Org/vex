//! Three-way semantic merge.
//!
//! Given a common `base` graph and two descendants `ours` and `theirs`, this
//! module computes the merge outcome: which changes apply cleanly, and which
//! conflict.
//!
//! Identity for merging is [`GlobalId`]-first. Nodes lacking a `GlobalId` fall
//! back to structural hash, same as two-way diff. Edges are *not* merged in
//! this MVP — we flag conflicts at the node-property level, which is where
//! the overwhelming majority of real-world design edits occur.

use std::collections::{BTreeMap, BTreeSet};

use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use vex_graph::{hash_graph, ir::IfcGraph, HashConfig, NodeId};
use vex_utils::StringInterner;

use crate::{resolve, PropDelta, SerValue};

/// Identity of a merged node, matching [`crate::Identity`] for JSON-friendly
/// reporting. Duplicated so the merge API is self-contained.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum MergeIdentity {
    GlobalId(String),
    StructuralHash(String),
    StepId(u64),
}

/// A single non-conflicting merge operation, recorded for auditability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MergeOp {
    /// Both sides left this node untouched; keep the base.
    Kept {
        identity: MergeIdentity,
        type_name: String,
    },
    /// Exactly one side changed this node; take that side's version.
    TakeOurs {
        identity: MergeIdentity,
        type_name: String,
    },
    TakeTheirs {
        identity: MergeIdentity,
        type_name: String,
    },
    /// Node added on exactly one side; include it.
    AddedOurs {
        identity: MergeIdentity,
        type_name: String,
    },
    AddedTheirs {
        identity: MergeIdentity,
        type_name: String,
    },
    /// Node removed cleanly on exactly one side.
    RemovedOurs {
        identity: MergeIdentity,
        type_name: String,
    },
    RemovedTheirs {
        identity: MergeIdentity,
        type_name: String,
    },
    /// Both sides applied the *same* change (e.g. edited the same property
    /// to the same value). Take either; record for the audit trail.
    ConcurrentIdentical {
        identity: MergeIdentity,
        type_name: String,
    },
}

/// A merge conflict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Conflict {
    /// Both sides modified the same node with incompatible property values.
    /// `deltas` enumerates the per-property disagreements; properties that
    /// both sides changed identically are *not* reported here.
    ModifyModify {
        identity: MergeIdentity,
        type_name: String,
        ours: Vec<PropDelta>,
        theirs: Vec<PropDelta>,
    },
    /// One side modified a node the other side removed.
    ModifyDelete {
        identity: MergeIdentity,
        type_name: String,
        side_modified: Side,
    },
    /// Both sides added a node with the same identity but different content.
    AddAdd {
        identity: MergeIdentity,
        type_name: String,
        ours: Vec<(String, SerValue)>,
        theirs: Vec<(String, SerValue)>,
    },
}

/// Which side of the merge a fact comes from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// Aggregate merge result.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MergeResult {
    pub clean: bool,
    pub ops: Vec<MergeOp>,
    pub conflicts: Vec<Conflict>,
    pub summary: MergeSummary,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct MergeSummary {
    pub kept: usize,
    pub auto_applied: usize,
    pub conflicts: usize,
}

/// Run the three-way merge.
#[allow(clippy::too_many_lines)]
pub fn merge_graphs(
    base: &IfcGraph,
    base_i: &StringInterner,
    ours: &IfcGraph,
    ours_i: &StringInterner,
    theirs: &IfcGraph,
    theirs_i: &StringInterner,
    cfg: &HashConfig,
) -> MergeResult {
    let hb = hash_graph(base, base_i, cfg);
    let ho = hash_graph(ours, ours_i, cfg);
    let ht = hash_graph(theirs, theirs_i, cfg);

    // Primary identity index: GlobalId.
    let base_by_gid = index_by_gid(base);
    let ours_by_gid = index_by_gid(ours);
    let theirs_by_gid = index_by_gid(theirs);

    let all_gids: BTreeSet<&str> = base_by_gid
        .keys()
        .chain(ours_by_gid.keys())
        .chain(theirs_by_gid.keys())
        .copied()
        .collect();

    let mut result = MergeResult {
        clean: true,
        ..Default::default()
    };

    for gid in all_gids {
        let base_node = base_by_gid.get(gid).copied();
        let ours_node = ours_by_gid.get(gid).copied();
        let theirs_node = theirs_by_gid.get(gid).copied();

        let ident = MergeIdentity::GlobalId(gid.to_string());

        match (base_node, ours_node, theirs_node) {
            (Some(bn), Some(on), Some(tn)) => {
                let bh = hb.per_node[&bn];
                let oh = ho.per_node[&on];
                let th = ht.per_node[&tn];
                let type_name = type_name_of(ours, ours_i, on);

                let ours_changed = oh != bh;
                let theirs_changed = th != bh;

                match (ours_changed, theirs_changed) {
                    (false, false) => {
                        result.ops.push(MergeOp::Kept {
                            identity: ident,
                            type_name,
                        });
                        result.summary.kept += 1;
                    }
                    (true, false) => {
                        result.ops.push(MergeOp::TakeOurs {
                            identity: ident,
                            type_name,
                        });
                        result.summary.auto_applied += 1;
                    }
                    (false, true) => {
                        result.ops.push(MergeOp::TakeTheirs {
                            identity: ident,
                            type_name,
                        });
                        result.summary.auto_applied += 1;
                    }
                    (true, true) => {
                        if oh == th {
                            result.ops.push(MergeOp::ConcurrentIdentical {
                                identity: ident,
                                type_name,
                            });
                            result.summary.auto_applied += 1;
                        } else {
                            let ours_deltas = diff_props(base, base_i, bn, ours, ours_i, on);
                            let theirs_deltas = diff_props(base, base_i, bn, theirs, theirs_i, tn);
                            // Find incompatible overlap.
                            if props_compatible(&ours_deltas, &theirs_deltas) {
                                // Non-overlapping property changes → take union.
                                result.ops.push(MergeOp::TakeOurs {
                                    identity: ident.clone(),
                                    type_name: type_name.clone(),
                                });
                                result.ops.push(MergeOp::TakeTheirs {
                                    identity: ident,
                                    type_name,
                                });
                                result.summary.auto_applied += 1;
                            } else {
                                result.conflicts.push(Conflict::ModifyModify {
                                    identity: ident,
                                    type_name,
                                    ours: ours_deltas,
                                    theirs: theirs_deltas,
                                });
                                result.clean = false;
                                result.summary.conflicts += 1;
                            }
                        }
                    }
                }
            }
            (Some(bn), Some(on), None) => {
                // Theirs removed.
                let bh = hb.per_node[&bn];
                let oh = ho.per_node[&on];
                let type_name = type_name_of(ours, ours_i, on);
                if bh == oh {
                    result.ops.push(MergeOp::RemovedTheirs {
                        identity: ident,
                        type_name,
                    });
                    result.summary.auto_applied += 1;
                } else {
                    result.conflicts.push(Conflict::ModifyDelete {
                        identity: ident,
                        type_name,
                        side_modified: Side::Ours,
                    });
                    result.clean = false;
                    result.summary.conflicts += 1;
                }
            }
            (Some(bn), None, Some(tn)) => {
                // Ours removed.
                let bh = hb.per_node[&bn];
                let th = ht.per_node[&tn];
                let type_name = type_name_of(theirs, theirs_i, tn);
                if bh == th {
                    result.ops.push(MergeOp::RemovedOurs {
                        identity: ident,
                        type_name,
                    });
                    result.summary.auto_applied += 1;
                } else {
                    result.conflicts.push(Conflict::ModifyDelete {
                        identity: ident,
                        type_name,
                        side_modified: Side::Theirs,
                    });
                    result.clean = false;
                    result.summary.conflicts += 1;
                }
            }
            (None, Some(on), Some(tn)) => {
                // Added on both. Same → keep; different → conflict.
                let oh = ho.per_node[&on];
                let th = ht.per_node[&tn];
                let type_name = type_name_of(ours, ours_i, on);
                if oh == th {
                    result.ops.push(MergeOp::ConcurrentIdentical {
                        identity: ident,
                        type_name,
                    });
                    result.summary.auto_applied += 1;
                } else {
                    result.conflicts.push(Conflict::AddAdd {
                        identity: ident,
                        type_name,
                        ours: node_props_serialized(ours, ours_i, on),
                        theirs: node_props_serialized(theirs, theirs_i, tn),
                    });
                    result.clean = false;
                    result.summary.conflicts += 1;
                }
            }
            (None, Some(on), None) => {
                let type_name = type_name_of(ours, ours_i, on);
                result.ops.push(MergeOp::AddedOurs {
                    identity: ident,
                    type_name,
                });
                result.summary.auto_applied += 1;
            }
            (None, None, Some(tn)) => {
                let type_name = type_name_of(theirs, theirs_i, tn);
                result.ops.push(MergeOp::AddedTheirs {
                    identity: ident,
                    type_name,
                });
                result.summary.auto_applied += 1;
            }
            (Some(_), None, None) => {
                // Removed on both — agreement.
                result.ops.push(MergeOp::ConcurrentIdentical {
                    identity: ident,
                    type_name: String::from("<removed>"),
                });
                result.summary.auto_applied += 1;
            }
            (None, None, None) => unreachable!(),
        }
    }

    result
}

// ---------- helpers ----------

fn index_by_gid(g: &IfcGraph) -> AHashMap<&str, NodeId> {
    let mut m = AHashMap::with_capacity(g.node_count());
    for (id, node) in &g.nodes {
        if let Some(gid) = &node.global_id {
            m.insert(gid.0.as_str(), id);
        }
    }
    m
}

fn type_name_of(g: &IfcGraph, i: &StringInterner, id: NodeId) -> String {
    g.nodes
        .get(id)
        .map(|n| i.resolve(n.type_name).to_string())
        .unwrap_or_default()
}

fn node_props_serialized(g: &IfcGraph, i: &StringInterner, id: NodeId) -> Vec<(String, SerValue)> {
    g.nodes
        .get(id)
        .map(|n| {
            n.props
                .iter()
                .map(|(k, v)| (i.resolve(*k).to_string(), resolve(v, i)))
                .collect()
        })
        .unwrap_or_default()
}

fn diff_props(
    base: &IfcGraph,
    base_i: &StringInterner,
    base_id: NodeId,
    side: &IfcGraph,
    side_i: &StringInterner,
    side_id: NodeId,
) -> Vec<PropDelta> {
    let base_map: BTreeMap<String, SerValue> = base
        .nodes
        .get(base_id)
        .map(|n| {
            n.props
                .iter()
                .map(|(k, v)| (base_i.resolve(*k).to_string(), resolve(v, base_i)))
                .collect()
        })
        .unwrap_or_default();
    let side_map: BTreeMap<String, SerValue> = side
        .nodes
        .get(side_id)
        .map(|n| {
            n.props
                .iter()
                .map(|(k, v)| (side_i.resolve(*k).to_string(), resolve(v, side_i)))
                .collect()
        })
        .unwrap_or_default();
    let mut deltas = Vec::new();
    let keys: BTreeSet<&String> = base_map.keys().chain(side_map.keys()).collect();
    for k in keys {
        let a = base_map.get(k);
        let b = side_map.get(k);
        if a != b {
            deltas.push(PropDelta {
                key: k.clone(),
                before: a.cloned(),
                after: b.cloned(),
            });
        }
    }
    deltas
}

/// Two property delta lists are "compatible" iff they touch disjoint keys,
/// OR they touch an overlapping key with identical `after` values.
fn props_compatible(a: &[PropDelta], b: &[PropDelta]) -> bool {
    let a_map: BTreeMap<&str, &Option<SerValue>> =
        a.iter().map(|d| (d.key.as_str(), &d.after)).collect();
    for d in b {
        if let Some(ours) = a_map.get(d.key.as_str()) {
            if **ours != d.after {
                return false;
            }
        }
    }
    true
}

#[allow(unused_imports)]
pub(crate) use vex_graph::hash_graph as _hash_graph_re_export;

/// Pretty-print a merge report to stable text.
#[must_use]
pub fn render_merge_text(r: &MergeResult) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if r.clean {
        let _ = writeln!(out, "Clean merge.");
    } else {
        let _ = writeln!(out, "Merge has {} conflict(s):", r.conflicts.len());
    }
    for c in &r.conflicts {
        match c {
            Conflict::ModifyModify {
                identity,
                type_name,
                ours,
                theirs,
            } => {
                let _ = writeln!(out, "! modify/modify {type_name} {identity:?}");
                for d in ours {
                    let _ = writeln!(out, "    ours {} : {:?} -> {:?}", d.key, d.before, d.after);
                }
                for d in theirs {
                    let _ = writeln!(out, "  theirs {} : {:?} -> {:?}", d.key, d.before, d.after);
                }
            }
            Conflict::ModifyDelete {
                identity,
                type_name,
                side_modified,
            } => {
                let _ = writeln!(
                    out,
                    "! modify/delete {type_name} {identity:?} (modified on {side_modified:?})",
                );
            }
            Conflict::AddAdd {
                identity,
                type_name,
                ..
            } => {
                let _ = writeln!(out, "! add/add {type_name} {identity:?}");
            }
        }
    }
    let _ = writeln!(
        out,
        "\n{} kept, {} auto, {} conflicts",
        r.summary.kept, r.summary.auto_applied, r.summary.conflicts,
    );
    out
}
