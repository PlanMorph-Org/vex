# Vex

> Semantic, graph-based version control for IFC/BIM models.

Vex is to BIM what Git is to source code — but built from first principles for the
semantic, graph-shaped nature of IFC data. Instead of line-by-line file diffs, Vex
parses IFC into a normalized property graph, canonicalizes it, and produces
meaning-level diffs ("wall moved 200 mm", "property set added to slab"), stored in
a content-addressable object database.

**Status:** early MVP. APIs unstable. Not for production use yet.

## Quick start

```sh
cargo build --release
./target/release/vex init ./my-project
./target/release/vex import model.ifc
./target/release/vex commit -m "initial model"
./target/release/vex diff HEAD~1 HEAD
```

## Architecture

See [`instructions.md`](./instructions.md) for the full engineering blueprint, and
each crate's README for implementation details.

```
crates/
├── vex-utils/        # shared primitives: hashing, interning, errors, tolerance
├── vex-ifc-parser/   # streaming STEP Part 21 parser
├── vex-graph/        # normalized graph IR + canonicalization + Merkle hashing
├── vex-geometry/     # geometry hashing (Phase 3)
├── vex-storage/      # content-addressable object store (redb backend)
├── vex-diff/         # semantic diff (+ 3-way merge, Phase 2)
├── vex-core/         # repository orchestrator
└── vex-cli/          # `vex` binary (clap-based, Git-like UX)
```

## License

Licensed under either of Apache License 2.0 or MIT license, at your option.
