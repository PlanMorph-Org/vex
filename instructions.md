# 🏗️ BIM Version Control System (Vex) — Engineering Blueprint

## 🧠 Core Philosophy

We are NOT building:

* a file diff tool

We ARE building:

* a **semantic, graph-based version control system for IFC models**

Think:

> Git × Graph DB × Geometry Engine

---

# 🧱 1. High-Level Architecture

```
IFC File (.ifc)
   ↓
Parser (Rust)
   ↓
Intermediate Representation (IR) ← normalized graph
   ↓
Diff Engine (semantic)
   ↓
Storage Engine (object store + snapshots)
   ↓
CLI / API (vex)
```

---

# 🗂️ 2. Project Structure (Rust Monorepo)

```
vex/
├── crates/
│   ├── vex-cli/              # CLI interface (like git)
│   ├── vex-core/             # Core logic (diff, graph ops)
│   ├── vex-ifc-parser/       # IFC STEP parser
│   ├── vex-graph/            # Graph data structures
│   ├── vex-geometry/         # Geometry hashing + simplification
│   ├── vex-storage/          # Object store (snapshots, blobs)
│   ├── vex-diff/             # Semantic diff engine
│   └── vex-utils/            # shared utils
│
├── examples/
├── tests/
└── Cargo.toml
```

---

# ⚙️ 3. IFC Parser (Rust)

## Goal:

Convert `.ifc` → structured graph

---

## STEP Format Example

```
#42 = IFCWALL('3fG8k2...', ...);
#87 = IFCRELDEFINESBYPROPERTIES(...);
```

---

## Parsing Strategy

### Phase 1: Tokenization

* Stream file line-by-line (DO NOT load entire file)
* Extract:

  * entity id (#42)
  * type (IFCWALL)
  * arguments

---

### Phase 2: AST Representation

```rust
struct IfcEntity {
    id: u32,
    type_name: String,
    attributes: Vec<IfcValue>,
}
```

---

### Phase 3: Reference Resolution

Convert:

```
#42 references #87
```

Into:

```
Graph edge
```

---

## ⚡ Performance Tip

* Use **arena allocation** (e.g., `typed_arena`)
* Avoid string cloning (`&str` slices where possible)

---

# 🧠 4. Internal Graph Model

We DO NOT keep IFC as-is.

---

## Node Model

```rust
struct Node {
    global_id: Option<String>,
    type_name: String,
    properties: HashMap<String, Value>,
}
```

---

## Edge Model

```rust
enum EdgeType {
    Aggregates,
    Contains,
    Defines,
    Connects,
}

struct Edge {
    from: NodeId,
    to: NodeId,
    edge_type: EdgeType,
}
```

---

## Why Graph?

IFC is inherently:

* relational
* non-linear
* dependency-heavy

---

# 🔄 5. Normalization Layer (CRITICAL)

Before diffing, normalize EVERYTHING.

---

## Steps:

### 1. Sort Entities

* Deterministic ordering

### 2. Normalize Floats

* Round to tolerance (e.g., 1e-6)

### 3. Strip Noise

Remove:

* timestamps
* export metadata
* software-specific junk

---

## Result:

Two IFC files → comparable structure

---

# 🔍 6. Diff Engine (Semantic)

## NOT:

```
line-by-line diff
```

## BUT:

```
entity-level + meaning-level diff
```

---

## Algorithm

### Step 1: Match Entities

* Primary: `GlobalId`
* Fallback: structural hash

---

### Step 2: Compare

```rust
enum Change {
    Added(Node),
    Removed(Node),
    Modified {
        before: Node,
        after: Node,
    }
}
```

---

## Semantic Layers

| Layer        | Meaning           |
| ------------ | ----------------- |
| Geometry     | shape changed     |
| Property     | metadata changed  |
| Relationship | structure changed |

---

# 📦 7. Storage Engine (Git-like)

---

## Object Model

```
Blob → Node snapshot
Tree → Graph snapshot
Commit → Change set
```

---

## Example

```rust
struct Commit {
    id: Hash,
    parent: Option<Hash>,
    changes: Vec<Change>,
    timestamp: u64,
}
```

---

## Storage Strategy

* Content-addressable (SHA-256)
* Deduplicate identical nodes
* Store deltas, not full files

---

# ⚡ 8. Geometry Handling (Hard Part)

---

## Problem:

Geometry creates massive noise in diffs.

---

## Solution:

### 1. Geometry Hashing

* Convert geometry → canonical form
* Hash it

```rust
fn hash_geometry(geom: &Geometry) -> Hash
```

---

### 2. Tolerance-Based Comparison

* Ignore tiny changes
* Use bounding boxes / centroids

---

### 3. Dual Mode

| Mode    | Use                |
| ------- | ------------------ |
| Fast    | hash only          |
| Precise | full geometry diff |

---

# 🚀 9. Performance Strategy (THIS is where you win)

---

## 1. Streaming Parser

* Never load full IFC into memory

---

## 2. Parallel Processing

Use:

```rust
rayon
```

Parallelize:

* entity parsing
* hashing
* diff computation

---

## 3. Memory Efficiency

* Use IDs instead of strings
* Intern strings (`string_interner`)

---

## 4. Incremental Diff

Only process:

* changed subgraphs

---

## 5. Lazy Geometry Evaluation

Don’t compute geometry unless needed.

---

# 🧰 10. CLI Design (Git-like UX)

```
vex init
vex import model.ifc
vex commit -m "Added structural walls"
vex diff v1 v2
vex log
```

---

# 🧠 11. Handling IFC Challenges (Real Solutions)

---

## ❌ Problem: Broken GlobalIds

### Solution:

* fallback hash:

```
(type + geometry + location)
```

---

## ❌ Problem: Export noise

### Solution:

* normalization layer (strict rules)

---

## ❌ Problem: Different BIM tools

* Autodesk Revit
* Archicad

### Solution:

* tool-specific adapters
* normalization profiles

---

## ❌ Problem: Huge files

### Solution:

* chunk parsing
* graph partitioning

---

# 🔥 12. What Makes This System Elite

---

## 1. Semantic Diff

“Wall moved 200mm” instead of raw changes

---

## 2. Change Graph

Visualize evolution of building over time

---

## 3. Conflict Resolution

Like Git merge, but for buildings

---

## 4. API Layer

Future:

* plugins for BIM tools
* cloud collaboration

---

# 🧭 13. Build Roadmap (DO THIS)

---

## Phase 1 (MVP)

* IFC parser
* Graph model
* Basic diff

---

## Phase 2

* normalization
* commit system
* CLI

---

## Phase 3

* geometry hashing
* performance optimization

---

## Phase 4

* visualization
* integrations

---

# 🚀 Final Thought

You are not building a tool.

You are building:

> “The infrastructure layer for tracking change in the built world.”

If done right, this sits next to:

* Autodesk
* Bentley Systems

---

And Rust?

Perfect choice.

Because speed here isn’t optional — it’s the difference between:

> usable vs ignored in real construction workflows.
