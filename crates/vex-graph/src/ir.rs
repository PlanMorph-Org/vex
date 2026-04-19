//! Normalized in-memory IR for an IFC model.
//!
//! The IR is **property-graph shaped**: nodes carry type + properties, and
//! edges carry a kind + positional slot. References between entities in the
//! STEP file become directed edges here.
//!
//! Design principles:
//! - Strings (type names, property keys, enum values) are interned: graph
//!   size scales with unique vocabulary, not with entity count.
//! - Float values are stored unquantized; canonicalization applies tolerance
//!   during the Merkle hashing pass so the original precision is preserved
//!   on checkout.
//! - Node identity is a local `NodeId`; the original STEP `#N` id and the
//!   IFC `GlobalId` (when present) are stored as attributes, not keys.

use serde::{Deserialize, Serialize};
use slotmap::{new_key_type, SlotMap};
use smallvec::SmallVec;

use vex_utils::StringId;

new_key_type! {
    /// Opaque handle to a node in an [`IfcGraph`].
    pub struct NodeId;
}

/// The IFC GlobalId, preserved as the original 22-char base64 string.
/// We don't force a decode at ingest time — comparison is string-level.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GlobalId(pub String);

/// Scalar property value.
///
/// This is a flattened, lossy projection of `vex_ifc_parser::Value` that is
/// convenient for diffing. Nested lists become `List(Vec<Value>)`; references
/// are lifted into edges (see [`Edge`]) and therefore don't appear here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    /// Interned string (literal / typed-value wrapper / binary blob).
    Text(StringId),
    /// Interned enum symbol.
    Enum(StringId),
    List(Vec<Value>),
    /// A typed wrapper: `IFCLENGTHMEASURE(3.0)` → `Typed { name_id, inner }`.
    Typed { name: StringId, inner: Box<Value> },
}

/// Edge classification — which relationship the positional argument expresses.
///
/// For MVP we classify coarsely; precise semantic classification (e.g. recognising
/// `IfcRelAggregates` as parent→child aggregation) is applied by the graph
/// builder using the entity type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Unspecified — the caller didn't have enough information to classify.
    Other,
    /// Structural containment (spatial hierarchy).
    Contains,
    /// Parent ↔ child aggregation.
    Aggregates,
    /// Type / property-set definition.
    Defines,
    /// Connection (opening in wall, adjacency, port).
    Connects,
    /// Assignment relationship (group membership).
    Assigns,
    /// Material / classification association.
    Associates,
    /// Reference to a type or style definition.
    TypeRef,
    /// Reference to a property.
    PropertyRef,
}

/// A directed edge between two nodes.
///
/// `slot` is the index of the positional argument in the source entity that
/// produced this edge. It matters for canonical hashing: two edges of the
/// same kind going to the same target are still distinguishable by slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub slot: u16,
    /// Index within a list argument, if the reference came from inside a list.
    /// `u16::MAX` means "not inside a list".
    pub list_index: u16,
}

/// A node in the IFC graph.
#[derive(Debug, Clone)]
pub struct Node {
    /// Interned type name (e.g. `IFCWALL`).
    pub type_name: StringId,
    /// Original STEP entity id from the source file. Not used for identity —
    /// purely diagnostic.
    pub step_id: u64,
    /// IFC GlobalId if the entity carries one. This is the *primary* identity
    /// for the semantic diff.
    pub global_id: Option<GlobalId>,
    /// Flat property list — (key_id, value). Canonical form sorts by key.
    /// Most IFC entities have < 10 properties, so SmallVec avoids heap traffic.
    pub props: SmallVec<[(StringId, Value); 8]>,
}

/// The in-memory IFC graph.
#[derive(Debug, Default)]
pub struct IfcGraph {
    pub nodes: SlotMap<NodeId, Node>,
    /// All edges, flat. Per-node adjacency indexes live in `out_edges`.
    pub edges: Vec<Edge>,
    /// Schema string from the original `FILE_SCHEMA` header entry.
    pub schema: Option<String>,
}

impl IfcGraph {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_node(&mut self, node: Node) -> NodeId {
        self.nodes.insert(node)
    }

    pub fn add_edge(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Return all outgoing edges from `src`. O(E); callers that need this
    /// repeatedly should pre-build an adjacency index.
    pub fn out_edges<'a>(&'a self, src: NodeId) -> impl Iterator<Item = &'a Edge> + 'a {
        self.edges.iter().filter(move |e| e.from == src)
    }
}
