# Vex — Reference Manual

> Semantic, graph-based version control for IFC/BIM models.

Vex is to BIM what Git is to source code, but built from first principles for
how building models actually change: by element, not by line. Two `.ifc` files
that re-export with shuffled STEP IDs and re-generated owner-history records
look identical to Vex; a wall whose `Name` changed from `Wall-A` to `Wall-B`
shows up as exactly one `renamed` event.

This document covers:

1. [Quick start](#1-quick-start)
2. [Concepts](#2-concepts)
3. [Command reference](#3-command-reference)
4. [System design](#4-system-design)
5. [JSON contract for plugin hosts](#5-json-contract-for-plugin-hosts)
6. [Repository layout on disk](#6-repository-layout-on-disk)
7. [Roadmap](#7-roadmap)

---

## 1. Quick start

```bash
# Build once
cargo build --release --bin vex
alias vex=$PWD/target/release/vex   # or use the absolute path

# Make a project, save two versions, see what changed
mkdir /tmp/myproj && cd /tmp/myproj
vex init
vex import ~/projects/vex/examples/tiny.min.ifc
vex commit -m "initial wall"

vex import ~/projects/vex/examples/tiny-v2.min.ifc
vex commit -m "renamed wall + added column"

vex changes
# 1 column added, 1 wall renamed
#
#   * IFCWALL 2O2Fr$t4X7Zf8NOew3FNr2 — Name: Wall-A → Wall-B
#   + IFCCOLUMN 4O2Fr$t4X7Zf8NOew3FNr4

vex --json changes | jq .   # plugin-facing payload, see §5
```

---

## 2. Concepts

### Element identity is stable across re-export

Every classified change is attached to an `Identity`, in priority order:

1. **`GlobalId`** — the IFC `IfcRoot.GlobalId` (a 22-char base64 GUID). Survives
   STEP-ID renumbering, file re-export, and round-trips through other tools.
2. **`StructuralHash`** — content-addressed fallback for elements with no
   stable GlobalId (rare in well-formed IFC).
3. **`StepId`** — last resort; rarely used.

### Diff happens at the property layer, not the byte layer

Vex parses each IFC into a typed graph (`vex-graph`), normalizes it
(`vex-utils::profile`) to ignore noise like `IfcOwnerHistory` timestamps and
absolute path metadata, and then diffs node-by-node. Output is a list of
[`Change`](crates/vex-diff/src/lib.rs) values: `Added`, `Removed`, `Modified`
(with per-slot `PropDelta`s).

### Visual diff classifies changes for humans

`vex-visual-diff` consumes a raw `DiffReport` and refines `Modified` into one
of:

| Kind        | Meaning                                                      |
|-------------|--------------------------------------------------------------|
| `Added`     | Element exists in `to` only.                                 |
| `Removed`   | Element exists in `from` only.                               |
| `Renamed`   | Only `Name` / `Description` / `Tag` slots changed.           |
| `Moved`     | Only `ObjectPlacement` changed (position/rotation).          |
| `Modified`  | Anything else (geometry, type swap, mixed property edits).   |

Every classified change carries a one-line `hint`, e.g. `"Name: Wall-A → Wall-B"`.

### Git-like commit graph

Commits are content-addressed (Blake3), reference parent commits, and live
under `.vex/objects/`. Branches and tags are simple ref files. Signing uses
Ed25519 keys stored under `.vex/keys/`.

---

## 3. Command reference

All commands accept the global flags:

| Flag           | Purpose                                                    |
|----------------|------------------------------------------------------------|
| `--repo DIR`   | Repository root (default: cwd; env: `DELT_REPO`).          |
| `--json`       | Emit machine-readable JSON where supported.                |
| `-v, --verbose`| Increase log verbosity (`-vv` for debug, `-vvv` trace).    |

### Repository lifecycle

#### `vex init [PATH]`
Create a new repository. Initializes `.vex/` with `objects/`, `refs/`, and
`HEAD`. Idempotent — re-running on an existing repo is a no-op.

#### `vex import <FILE>`
Parse an IFC file, normalize it via the active profile, and stage it as the
next commit's tree. Replaces any previously staged tree. Prints the resulting
graph hash.

#### `vex commit -m <MSG> [--author NAME] [--email EMAIL] [--sign KEY]`
Record the staged tree as a commit on the current branch. With `--sign`, signs
the commit with the named Ed25519 key under `.vex/keys/`. Updates `HEAD` and
the current branch ref.

#### `vex status`
Show whether a tree is staged and the current `HEAD`.

#### `vex log [--format text|mermaid|dot]`
Show commit history. Default text format:
```
commit 69203b80d0ab  2026-04-18 06:40 UTC
Author: vex <user@vex>

    renamed wall + added column
```
- `--format mermaid` emits a Mermaid `gitGraph` block.
- `--format dot` emits Graphviz DOT.
- `--json` emits `[{commit, message, author, email, timestamp, parents}]`.

### Inspection

#### `vex diff <A> <B>`
Raw semantic diff between revisions `A` (older) and `B` (newer). Prints one
line per change at the property level. Use this when you want the unfiltered
delta stream; use `compare` for the human/plugin-facing shape.

#### `vex compare <FROM> <TO>`
Visual change report. Same diff engine as `diff` but classified into
Added/Removed/Moved/Renamed/Modified with a one-line summary at the top.
- Text mode: summary line, blank line, then one line per element with a glyph
  (`+` added, `-` removed, `~` moved, `*` renamed, `M` modified).
- `--json` mode: full [`VisualDiff`](#5-json-contract-for-plugin-hosts) payload
  ready for plugin overlays.

#### `vex changes`
Convenience alias of `compare HEAD~1 HEAD` — the *"View Changes since last
save"* button. Returns
```json
{ "status": "no-previous-version" }
```
(or a friendly text equivalent) when `HEAD` has no parent.

#### `vex verify [--signatures]`
Re-hash every stored object and confirm content integrity. With `--signatures`
also walks `HEAD`'s first-parent chain and verifies every Ed25519 signature.

#### `vex config`
Print the active normalization profile (merged config + defaults).

### Branches and tags

#### `vex branch create <NAME> [TARGET]`
Create a branch pointing at `TARGET` (default: `HEAD`).

#### `vex branch list`
List all branches. Marks the current one.

#### `vex branch delete <NAME>`
Delete a branch ref. Refuses to delete the current branch.

#### `vex tag create <NAME> [TARGET]`
Create a lightweight tag at `TARGET` (default: `HEAD`).

#### `vex tag list`
List all tags.

#### `vex tag delete <NAME>`
Delete a tag.

#### `vex refs`
List all refs (branches + tags) in one shot.

### Merging

#### `vex merge <OURS> <THEIRS> [-m MSG] [--strategy ours|theirs] [--ff-only] [--no-commit] [--sign KEY]`
Three-way merge between `ours` and `theirs` based on their common ancestor.
- Fast-forwards by default when possible.
- `--ff-only` refuses non-fast-forward.
- `--strategy ours|theirs` resolves clean non-FF merges by taking that side.
- Without a strategy, a clean merge needing resolution is *reported* and not
  committed.
- `--no-commit` reports only; never writes.
- `--sign KEY` signs the merge commit.

### Checkout

#### `vex checkout <REF> -o <FILE>`
Materialize a commit (or ref) back to a real `.ifc` file. Semantic checkout —
re-emits a valid STEP file from the stored graph.

### Maintenance

#### `vex gc`
Delete unreachable objects from the object store.

### Keys

#### `vex key generate <NAME>`
Generate a new Ed25519 keypair under `.vex/keys/<NAME>/`.

#### `vex key list`
List installed keys.

#### `vex key export <NAME>`
Print the public key (safe to share).

---

## 4. System design

### Crate map

```
                            ┌────────────────────┐
                            │   vex-cli (bin)    │
                            └─────────┬──────────┘
                                      │ uses
                            ┌─────────▼──────────┐
                            │      vex-api       │  user-verb façade
                            └─┬────┬────┬────────┘
                              │    │    │
       ┌──────────────────────┘    │    └────────────────┐
       │                           │                     │
┌──────▼──────────┐      ┌─────────▼──────────┐  ┌───────▼────────┐
│ vex-visual-diff │◄─────│      vex-core      │  │   vex-summary  │
│  classify diff  │      │ Repository, refs,  │  │ render summary │
└──────┬──────────┘      │ commits, merge     │  └────────────────┘
       │                 └────┬───────────────┘
       │                      │
┌──────▼─────────┐    ┌───────▼────────┐    ┌──────────────────┐
│   vex-diff     │    │  vex-storage   │    │    vex-utils     │
│ graph diffing  │    │  object store  │    │  profile + hash  │
└──────┬─────────┘    └───────┬────────┘    └──────────────────┘
       │                      │
┌──────▼─────────┐    ┌───────▼────────┐
│   vex-graph    │    │ vex-ifc-parser │
│ typed IFC tree │◄───│ STEP → graph   │
└────────────────┘    └────────────────┘

(vex-geometry: numeric helpers used by graph + diff.)
```

### Crate responsibilities

| Crate             | Role                                                                        |
|-------------------|-----------------------------------------------------------------------------|
| `vex-cli`         | The `vex` binary. Argument parsing, output formatting, no business logic.   |
| `vex-api`         | `VexProject` façade (open, import, save, compare, timeline, changes).       |
| `vex-core`        | `Repository`, refs, commits, merge engine, signing.                         |
| `vex-storage`     | Content-addressed object store (`.vex/objects/`), magic bytes `VEX0`.       |
| `vex-graph`       | Typed in-memory IFC graph (nodes, slots, layers).                           |
| `vex-ifc-parser`  | STEP-21 parser → `vex-graph`.                                               |
| `vex-utils`       | Hashing (`Blake3`), normalization profile, helpers.                         |
| `vex-diff`        | Graph differ → `DiffReport { changes: Vec<Change> }`.                       |
| `vex-visual-diff` | Classify `DiffReport` → `VisualDiff { elements, counts, summary }`.         |
| `vex-summary`     | Render `VisualDiff` → one-paragraph human summary.                          |
| `vex-geometry`    | Numeric helpers (transforms, fuzzy compare).                                |

### Pipeline (one commit cycle)

```
.ifc file ──► vex-ifc-parser ──► vex-graph ──► vex-utils::profile (normalize)
                                                            │
                                                            ▼
                                                     content-hash
                                                            │
                                                            ▼
                                              vex-storage::ObjectStore
                                                            │
                                                            ▼
                                                  Commit {tree, parents, …}
                                                            │
                                                            ▼
                                                .vex/refs/heads/<branch>
```

### Pipeline (one compare cycle)

```
.vex/objects/<from> ──► vex-graph A ─┐
                                     ├──► vex-diff::diff_graphs
.vex/objects/<to>   ──► vex-graph B ─┘            │
                                                  ▼
                                          DiffReport
                                                  │
                                                  ▼
                                  vex-visual-diff::classify
                                                  │
                                                  ▼
                                  vex-summary::render → summary string
                                                  │
                                                  ▼
                                          VisualDiff (JSON-stable)
```

### Why a graph, not a text diff

A line-based diff on `.ifc` is useless: STEP IDs renumber on re-export, owner
history bumps timestamps on every save, and entity ordering is implementation
defined. By parsing to a graph first and applying a deterministic normalization
profile, two semantically identical files produce the same hash, and a renamed
wall produces exactly one `Change::Modified` with a one-slot `PropDelta`.

### Stable identity strategy

In `vex-graph`, every node is keyed by `Identity`:
- `GlobalId(s)` if the entity inherits from `IfcRoot` and has a usable GUID.
- `StructuralHash(h)` derived from the node's typed slots and outgoing refs.
- `StepId(n)` only as a last resort.

`vex-diff` matches A↔B by `Identity`, then computes per-slot deltas. This is
the contract that lets a Revit add-in highlight "the same wall" across two
saves even when the file has been re-exported by a different application.

---

## 5. JSON contract for plugin hosts

The schema is **`vex.visual-diff/1`**, locked by a golden-file test
(`crates/vex-cli/tests/fixtures/visual_diff.golden.json`). Bump the version
and regenerate the fixture deliberately when changing it.

### Top-level shape

```json
{
  "schema": "vex.visual-diff/1",
  "from":   "<commit-hash-or-ref>",
  "to":     "<commit-hash-or-ref>",
  "elements": [ /* ElementChange[] */ ],
  "summary":  "1 column added, 1 wall renamed",
  "counts": {
    "added":    1,
    "removed":  0,
    "moved":    0,
    "renamed":  1,
    "modified": 0
  }
}
```

### `ElementChange`

```json
{
  "id":        { "GlobalId": "2O2Fr$t4X7Zf8NOew3FNr2" },
  "type_name": "IFCWALL",
  "kind":      "renamed",
  "deltas": [
    {
      "key":    "_2",
      "before": { "Text": "Wall-A" },
      "after":  { "Text": "Wall-B" }
    }
  ],
  "hint": "Name: Wall-A → Wall-B"
}
```

### Field semantics

| Field        | Type                                                          | Notes |
|--------------|---------------------------------------------------------------|-------|
| `schema`     | string, always `"vex.visual-diff/1"`                          | Plugins may refuse unknown versions. |
| `from`/`to`  | string                                                        | Verbatim from CLI args (commit, branch, tag, or empty). |
| `elements`   | `ElementChange[]`                                             | One entry per affected element. |
| `summary`    | string                                                        | Human one-liner. |
| `counts.*`   | u32                                                           | Sum equals `elements.length`. |
| `id`         | `{GlobalId} \| {StructuralHash} \| {StepId}` (one variant)    | Stable across re-export when `GlobalId` is present. |
| `type_name`  | string, e.g. `"IFCWALL"`                                      | Source-cased, no namespace prefix. |
| `kind`       | `"added" \| "removed" \| "moved" \| "renamed" \| "modified"`  | snake_case. |
| `deltas`     | `PropDelta[]`                                                 | Empty for `added`/`removed`. Positional slot keys (`_0`..`_N`). |
| `hint`       | string \| omitted                                             | Single-line human explanation when one is meaningful. |

### `vex --json changes` returns the same payload, plus this edge case:

```json
{ "status": "no-previous-version" }
```

When `HEAD` has no parent.

### Plugin integration shape

A Revit/Archicad/Tekla add-in shells out (or, later, calls a planned
`vex-bridge` cdylib) and overlays the JSON:

```pseudo
diff = json.parse(run("vex", "--json", "changes"))
for el in diff.elements:
    case el.kind:
        added:    color(el.id.GlobalId, GREEN)
        removed:  ghost(el.id.GlobalId)        # ghost geometry from `from`
        renamed:  badge(el.id.GlobalId, "✎")
        moved:    arrow(el.id.GlobalId)
        modified: color(el.id.GlobalId, YELLOW)
        tooltip(el.id.GlobalId, el.hint)
show_status_bar(diff.summary)
```

---

## 6. Repository layout on disk

```
.vex/
├── HEAD                 # e.g. "ref: refs/heads/main"
├── config               # optional normalization profile overrides
├── refs/
│   ├── heads/main       # 64-char commit hash
│   └── tags/v1.0
├── objects/             # content-addressed (Blake3), magic bytes "VEX0"
│   ├── 19/412ddfacdb…   # commits + trees + nodes, sharded by first byte
│   └── …
└── keys/
    └── <name>/          # Ed25519 keypair (signing)
        ├── public
        └── secret
```

Object format starts with the 4-byte magic `VEX0` followed by a typed payload.
The store is append-only; `vex gc` is the only writer that removes objects.

---

## 7. Roadmap

**Shipped (Phases 1–6 + GUI-pivot Step 1):**

- IFC parse → typed graph → normalization profile.
- Content-addressed storage, commits, branches, tags.
- Three-way merge with `ours`/`theirs` strategies and fast-forward.
- Ed25519 signing + `verify --signatures`.
- Semantic checkout back to `.ifc`.
- `vex-visual-diff` + `vex-summary` + `vex-api` plugin engine.
- Stable `vex.visual-diff/1` JSON contract with golden-file test.

**Next:**

- **`vex-bridge`** — `cdylib` exposing `vex-api` over a stable C ABI so plugins
  load the engine in-process instead of shelling out.
- **First plugin** — Revit add-in proof of concept consuming the JSON.
- **Engine depth** — geometry-resize detection, type-swap detection, IfcPropertySet
  diffs as a separate layer.
- **Web viewer** — render `VisualDiff` over a WebGL/three.js IFC viewer.
- **Daemon** — long-running indexer for very large models.
