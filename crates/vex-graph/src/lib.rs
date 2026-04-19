//! Normalized graph IR for IFC models + canonical hashing.
//!
//! See:
//! - [`ir`]: core data structures (`IfcGraph`, `Node`, `Edge`).
//! - [`builder::GraphBuilder`]: convert parsed entities to the IR.
//! - [`merkle::hash_graph`]: canonical per-node and graph-root hashing.

pub mod builder;
pub mod canonical;
pub mod geometry;
pub mod ir;
pub mod merkle;

pub use builder::GraphBuilder;
pub use ir::{Edge, EdgeKind, GlobalId, IfcGraph, Node, NodeId, Value};
pub use merkle::{hash_graph, GraphHashes, HashConfig};
