//! Build an [`IfcGraph`] from a stream of [`RawEntity`] values.
//!
//! Steps (two-pass):
//! 1. First pass: allocate a `NodeId` for each `#N` id, recording its type +
//!    attributes, but leaving references unresolved (stored as `u64`).
//! 2. Second pass: walk each node's raw args, lifting `Value::Ref(n)` into
//!    [`Edge`] entries and inlining everything else as [`ir::Value`].
//!
//! Forward references are trivial because pass 1 allocates every id before
//! pass 2 runs.

use ahash::AHashMap;
use smallvec::SmallVec;
use vex_ifc_parser::{Parser, RawEntity, Value as ParserValue};
use vex_utils::{Profile, StringInterner, VexError, VexResult};

use crate::ir::{Edge, EdgeKind, GlobalId, IfcGraph, Node, NodeId, Value};

/// Builder that translates a streamed parse into the normalized graph IR.
#[derive(Debug)]
pub struct GraphBuilder {
    interner: StringInterner,
    /// Raw entities, kept for the second pass.
    raw: Vec<RawEntity>,
    /// Normalization profile (default = drop `IfcOwnerHistory`).
    profile: Profile,
}

impl GraphBuilder {
    #[must_use]
    pub fn new(interner: StringInterner) -> Self {
        Self::with_profile(interner, Profile::default())
    }

    #[must_use]
    pub fn with_profile(interner: StringInterner, profile: Profile) -> Self {
        Self {
            interner,
            raw: Vec::new(),
            profile,
        }
    }

    #[must_use]
    pub fn interner(&self) -> &StringInterner {
        &self.interner
    }

    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Convenience: drive a parser to completion and build a graph.
    pub fn build_from_parser<R: std::io::BufRead>(
        interner: StringInterner,
        parser: &mut Parser<R>,
    ) -> VexResult<IfcGraph> {
        Self::build_from_parser_with_profile(interner, parser, Profile::default())
    }

    /// Like [`Self::build_from_parser`], but honours a normalization profile —
    /// entities whose type matches `profile.ignore_types` are dropped, and
    /// references pointing at them are filtered.
    pub fn build_from_parser_with_profile<R: std::io::BufRead>(
        interner: StringInterner,
        parser: &mut Parser<R>,
        profile: Profile,
    ) -> VexResult<IfcGraph> {
        let header = parser.parse_preamble()?;
        let mut builder = Self::with_profile(interner, profile);
        while let Some(entity) = parser.next_entity()? {
            builder.push_raw(entity);
        }
        let mut graph = builder.finish()?;
        graph.schema = header.schemas.first().cloned();
        Ok(graph)
    }

    /// Record a raw entity; first-pass work only.
    pub fn push_raw(&mut self, entity: RawEntity) {
        self.raw.push(entity);
    }

    /// Materialize the graph. Consumes the builder.
    pub fn finish(self) -> VexResult<IfcGraph> {
        let mut graph = IfcGraph::new();
        // First pass: create a node per entity, skipping ignored types.
        let mut id_map: AHashMap<u64, NodeId> = AHashMap::with_capacity(self.raw.len());
        let mut dropped: ahash::AHashSet<u64> = ahash::AHashSet::new();
        for entity in &self.raw {
            if self.profile.ignores_type(&entity.type_name) {
                dropped.insert(entity.id);
                continue;
            }
            if id_map.contains_key(&entity.id) {
                return Err(VexError::Graph(format!("duplicate STEP id #{}", entity.id)));
            }
            let type_name = self.interner.intern(&entity.type_name);
            let node_id = graph.insert_node(Node {
                type_name,
                step_id: entity.id,
                global_id: None,
                props: SmallVec::new(),
            });
            id_map.insert(entity.id, node_id);
        }

        // Second pass: lift refs into edges and materialize properties.
        for entity in &self.raw {
            let Some(&node_id) = id_map.get(&entity.id) else {
                continue; // type was ignored
            };
            let type_name_str = entity.type_name.as_str();
            let mut props: SmallVec<[(vex_utils::StringId, Value); 8]> = SmallVec::new();
            let mut global_id: Option<GlobalId> = None;
            let mut edges: Vec<Edge> = Vec::new();

            for (slot, raw_val) in entity.args.iter().enumerate() {
                let slot16 = u16::try_from(slot).unwrap_or(u16::MAX);
                lift_value(
                    &self.interner,
                    &id_map,
                    &dropped,
                    raw_val,
                    node_id,
                    slot16,
                    u16::MAX,
                    type_name_str,
                    &mut edges,
                )?;
                let inlined = inline_value(&self.interner, &id_map, raw_val)?;
                if slot == 0 && global_id.is_none() {
                    if let ParserValue::Str(ref s) = raw_val {
                        if looks_like_global_id(s) {
                            global_id = Some(GlobalId(s.clone()));
                        }
                    }
                }
                let key_str = format!("_{slot}");
                if self.profile.ignores_prop(&key_str) {
                    continue;
                }
                let key = self.interner.intern(&key_str);
                props.push((key, inlined));
            }

            let node = graph
                .nodes
                .get_mut(node_id)
                .ok_or_else(|| VexError::Graph("vanished node".into()))?;
            node.props = props;
            node.global_id = global_id;
            for edge in edges {
                graph.add_edge(edge);
            }
        }

        Ok(graph)
    }
}

/// Walk a parser value looking for references; emit edges.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::only_used_in_recursion)]
fn lift_value(
    interner: &StringInterner,
    id_map: &AHashMap<u64, NodeId>,
    dropped: &ahash::AHashSet<u64>,
    v: &ParserValue,
    from: NodeId,
    slot: u16,
    list_index: u16,
    from_type: &str,
    out: &mut Vec<Edge>,
) -> VexResult<()> {
    match v {
        ParserValue::Ref(n) => {
            if let Some(&to) = id_map.get(n) {
                out.push(Edge {
                    from,
                    to,
                    kind: classify_edge(from_type, slot),
                    slot,
                    list_index,
                });
            } else if dropped.contains(n) {
                // Silently drop: target filtered by profile.
            } else {
                return Err(VexError::Graph(format!(
                    "reference to undefined entity #{n}"
                )));
            }
        }
        ParserValue::List(items) => {
            for (i, item) in items.iter().enumerate() {
                let idx = u16::try_from(i).unwrap_or(u16::MAX);
                lift_value(
                    interner, id_map, dropped, item, from, slot, idx, from_type, out,
                )?;
            }
        }
        ParserValue::Typed { value, .. } => {
            lift_value(
                interner, id_map, dropped, value, from, slot, list_index, from_type, out,
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Convert a parser value to the graph `Value` form, replacing any `Ref` with
/// a sentinel `Null` (real linkage lives in edges).
fn inline_value(
    interner: &StringInterner,
    id_map: &AHashMap<u64, NodeId>,
    v: &ParserValue,
) -> VexResult<Value> {
    Ok(match v {
        ParserValue::Null | ParserValue::Derived => Value::Null,
        ParserValue::Int(n) => Value::Int(*n),
        ParserValue::Real(x) => Value::Real(*x),
        ParserValue::Str(s) | ParserValue::Binary(s) => Value::Text(interner.intern(s)),
        ParserValue::Enum(s) => {
            // Decode common booleans.
            match s.as_str() {
                "T" => Value::Bool(true),
                "F" => Value::Bool(false),
                _ => Value::Enum(interner.intern(s)),
            }
        }
        ParserValue::Ref(n) => {
            // Replace with a stable placeholder so the shape of the property
            // list is preserved. The actual connection lives in edges.
            let _ = id_map.get(n); // validate existence already done in lift pass
            Value::Null
        }
        ParserValue::List(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs {
                out.push(inline_value(interner, id_map, x)?);
            }
            Value::List(out)
        }
        ParserValue::Typed { name, value } => Value::Typed {
            name: interner.intern(name),
            inner: Box::new(inline_value(interner, id_map, value)?),
        },
    })
}

/// IFC `GlobalIds` are exactly 22 characters and use the alphabet
/// `[0-9A-Za-z_$]`. We don't decode eagerly — equality on the string is the
/// cheap and correct identity check.
fn looks_like_global_id(s: &str) -> bool {
    s.len() == 22
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}

/// Coarse edge classification by source-entity name prefix. The full IFC
/// schema would let us be more precise (e.g. `IfcRelAggregates` has specific
/// parent/child slots), but the prefix heuristic is robust and cheap.
fn classify_edge(from_type: &str, _slot: u16) -> EdgeKind {
    let up = from_type; // already uppercased by parser
    if up.starts_with("IFCRELAGGREGATES") || up.starts_with("IFCRELDECOMPOSES") {
        EdgeKind::Aggregates
    } else if up.starts_with("IFCRELCONTAINED") || up.starts_with("IFCRELSPATIALSTRUCTURE") {
        EdgeKind::Contains
    } else if up.starts_with("IFCRELDEFINES") {
        EdgeKind::Defines
    } else if up.starts_with("IFCRELCONNECTS") {
        EdgeKind::Connects
    } else if up.starts_with("IFCRELASSIGNS") {
        EdgeKind::Assigns
    } else if up.starts_with("IFCRELASSOCIATES") {
        EdgeKind::Associates
    } else {
        EdgeKind::Other
    }
}
