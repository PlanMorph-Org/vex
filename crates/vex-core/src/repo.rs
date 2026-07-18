//! Repository orchestrator.
//!
//! A [`Repository`] wraps an [`ObjectStore`] and a working directory, and
//! provides the high-level verbs the CLI exposes: `init`, `import`, `commit`,
//! `log`, `diff`, `checkout`, `verify`.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use vex_diff::{diff, render_text, DiffReport};
use vex_graph::{
    builder::GraphBuilder,
    hash_graph,
    ir::{EdgeKind, IfcGraph},
};
use vex_ifc_parser::{ParseLimits, Parser};
use vex_storage::{
    Blob, Commit, Identity, ObjectStore, SchemaManifest, SerValue, Tree, TreeEdge, TreeEntry,
};
use vex_utils::{Hash256, Profile, StringInterner, VexError, VexResult};

const DEFAULT_BRANCH: &str = "refs/heads/main";
const HEAD_REF: &str = "HEAD";
const STAGED_TREE: &str = "refs/staging/tree";
const CONFIG_FILE: &str = "config.toml";
const BLOB_WRITE_BATCH_SIZE: usize = 16_384;
const BLOB_READ_BATCH_SIZE: usize = 16_384;

/// An opened Vex repository.
#[derive(Debug)]
pub struct Repository {
    store: ObjectStore,
    root: PathBuf,
    profile: Profile,
}

/// Performance and size metadata for one IFC import.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportReport {
    pub tree: Hash256,
    pub nodes: usize,
    pub edges: usize,
    pub parse_ms: u128,
    pub persist_ms: u128,
    pub total_ms: u128,
}

/// One semantic element from a committed tree. Mirrors the stored [`Blob`]
/// identity fields without geometry or the full property payload. Produced by
/// [`Repository::elements`] for authoritative element inventories (no IFC
/// re-parsing or filename guessing).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ElementRecord {
    /// IFC entity type, upper-cased (e.g. `IFCWALLSTANDARDCASE`).
    pub type_name: String,
    /// STEP line id within the source file.
    pub step_id: u64,
    /// IFC `GlobalId` (22-char base64) when the entity carries one.
    pub global_id: Option<String>,
    /// Human label (IFC `Name` attribute, slot `_2`) when present.
    pub name: Option<String>,
}

/// Schema/version identifier for the spatial containment export. Bump the
/// trailing integer on any breaking change to [`SpatialContainment`].
pub const SPATIAL_SCHEMA: &str = "vex.spatial/1";

/// A light-weight identity reference to one IFC entity used inside the spatial
/// export. Carries both the `GlobalId` (when present) and the STEP id so
/// render workers can join on whichever is stable for them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpatialRef {
    /// IFC entity type, upper-cased (e.g. `IFCBUILDINGSTOREY`).
    pub type_name: String,
    /// STEP line id within the source file.
    pub step_id: u64,
    /// IFC `GlobalId` (22-char base64) when the entity carries one.
    pub global_id: Option<String>,
    /// Human label (IFC `Name` attribute, slot `_2`) when present.
    pub name: Option<String>,
}

/// One spatial structure container (Project / Site / Building / Storey / Space)
/// with its parent link and the elements directly contained in it. Derived
/// exclusively from retained graph relationships — `Aggregates` for the
/// container hierarchy and `Contains` for element membership — never from
/// rendered geometry bounds.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpatialContainer {
    /// Identity of the container entity itself.
    pub entity: SpatialRef,
    /// Spatial parent (via `Aggregates`). `None` for the project root or when
    /// no aggregation parent is retained in the committed graph.
    pub parent: Option<SpatialRef>,
    /// `GlobalId`s of elements directly contained via `Contains`, sorted and
    /// de-duplicated. Elements assigned to more than one container also appear
    /// in [`SpatialContainment::ambiguous`].
    pub element_global_ids: Vec<String>,
    /// STEP ids of directly contained elements that carry no `GlobalId`,
    /// sorted. Present for resilience against malformed graphs; normally empty.
    pub element_step_ids_without_global_id: Vec<u64>,
}

/// An element whose spatial containment is ambiguous — it is directly
/// contained by more than one spatial container. Multi-storey membership is
/// preserved (never silently collapsed) so downstream policy can decide.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AmbiguousMembership {
    /// Identity of the multiply-contained element.
    pub entity: SpatialRef,
    /// Every container that claims the element, sorted deterministically.
    pub containers: Vec<SpatialRef>,
}

/// Authoritative spatial containment metadata for one committed revision.
///
/// Produced by [`Repository::spatial_containment`] from the committed tree's
/// retained graph relationships. Ordering is deterministic and the shape is
/// versioned by [`SPATIAL_SCHEMA`], making it suitable as a stable feed for
/// render workers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpatialContainment {
    /// Spatial containers ordered by (spatial rank, STEP id): Project, Site,
    /// Building, Storey, Space, then any other container type.
    pub containers: Vec<SpatialContainer>,
    /// Rooted entities (carry a `GlobalId`) that are neither spatial containers
    /// nor IFC relationship entities and that are not contained by any spatial
    /// structure. Sorted by STEP id.
    pub unassigned: Vec<SpatialRef>,
    /// Elements contained by more than one spatial container. Sorted by STEP
    /// id. Each also appears under every claiming container above.
    pub ambiguous: Vec<AmbiguousMembership>,
}

impl Repository {
    /// Create a new repository at `path`, writing an initial manifest.
    pub fn init(path: impl AsRef<Path>) -> VexResult<Self> {
        let root = path.as_ref().join(".vex");
        std::fs::create_dir_all(&root).map_err(|e| VexError::io_at(&root, e))?;
        let store = ObjectStore::open_or_create(&root)?;
        let profile = Profile::default();
        let manifest = SchemaManifest::with_profile("IFC4", profile.clone());
        let _ = store.put_manifest(&manifest)?;
        // Emit a default config.toml the user can edit.
        let cfg_path = root.join(CONFIG_FILE);
        if !cfg_path.exists() {
            let toml = default_config_toml();
            std::fs::write(&cfg_path, toml).map_err(|e| VexError::io_at(&cfg_path, e))?;
        }
        Ok(Self {
            store,
            root: path.as_ref().to_path_buf(),
            profile,
        })
    }

    /// Open an existing repository. Looks for a `.vex/` directory upward from
    /// `path`; errors if none is found.
    pub fn open(path: impl AsRef<Path>) -> VexResult<Self> {
        let mut cur: PathBuf = path.as_ref().to_path_buf();
        loop {
            let candidate = cur.join(".vex");
            if candidate.is_dir() {
                let store = ObjectStore::open_or_create(&candidate)?;
                let profile = load_profile(&candidate)?;
                return Ok(Self {
                    store,
                    root: cur,
                    profile,
                });
            }
            if !cur.pop() {
                return Err(VexError::Config(format!(
                    "no .vex repository found at or above {}",
                    path.as_ref().display()
                )));
            }
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn store(&self) -> &ObjectStore {
        &self.store
    }

    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    fn hash_config(&self) -> vex_graph::HashConfig {
        vex_graph::HashConfig::from_profile(&self.profile)
    }

    /// Parse an IFC file, build a graph, serialize it as a staged Tree object,
    /// and update the staging ref to point at that tree. Returns the tree hash.
    pub fn import(&self, ifc_path: impl AsRef<Path>) -> VexResult<Hash256> {
        Ok(self.import_with_report(ifc_path)?.tree)
    }

    /// Import an IFC file and return stage timings for diagnostics and
    /// performance regression tracking.
    pub fn import_with_report(&self, ifc_path: impl AsRef<Path>) -> VexResult<ImportReport> {
        let started = Instant::now();
        let file =
            File::open(ifc_path.as_ref()).map_err(|e| VexError::io_at(ifc_path.as_ref(), e))?;
        let interner = StringInterner::new();
        let mut parser = Parser::new(BufReader::new(file), ParseLimits::default());
        let graph = GraphBuilder::build_from_parser_with_profile(
            interner.clone(),
            &mut parser,
            self.profile.clone(),
        )?;
        let parse_ms = started.elapsed().as_millis();
        let nodes = graph.node_count();
        let edges = graph.edge_count();
        let persist_started = Instant::now();
        let (tree_hash, _) = self.write_tree(&graph, &interner)?;
        self.store.set_ref(STAGED_TREE, tree_hash)?;
        Ok(ImportReport {
            tree: tree_hash,
            nodes,
            edges,
            parse_ms,
            persist_ms: persist_started.elapsed().as_millis(),
            total_ms: started.elapsed().as_millis(),
        })
    }

    /// Return the currently staged tree hash, if any.
    pub fn staged_tree(&self) -> VexResult<Option<Hash256>> {
        self.store.get_ref(STAGED_TREE)
    }

    /// Commit the staged tree (if any). The staging ref is cleared implicitly
    /// by advancing the branch pointer — we leave it in place for idempotency.
    pub fn commit(
        &self,
        message: impl Into<String>,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
    ) -> VexResult<Hash256> {
        self.commit_inner(
            message.into(),
            author_name.into(),
            author_email.into(),
            None,
        )
    }

    /// Commit and sign with the named Ed25519 key stored under `.vex/keys/`.
    pub fn commit_signed(
        &self,
        message: impl Into<String>,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
        key_name: &str,
    ) -> VexResult<Hash256> {
        self.commit_inner(
            message.into(),
            author_name.into(),
            author_email.into(),
            Some(key_name.to_string()),
        )
    }

    fn commit_inner(
        &self,
        message: String,
        author_name: String,
        author_email: String,
        sign_with: Option<String>,
    ) -> VexResult<Hash256> {
        let tree = self
            .staged_tree()?
            .ok_or_else(|| VexError::Config("nothing staged; run `vex import`".into()))?;
        let parent = self.resolve_head()?;
        let mut commit = Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: Identity {
                name: author_name,
                email: author_email,
            },
            committer: Identity {
                name: "vex".into(),
                email: "system@vex".into(),
            },
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            message,
            signature: None,
            profile_hash: self.profile.hash(),
        };
        if let Some(key) = sign_with {
            let vex_dir = self.root.join(".vex");
            let _ = crate::signing::sign_commit(&vex_dir, &key, &mut commit)?;
        }
        let hash = self.store.put_commit(&commit)?;
        self.store.set_ref(DEFAULT_BRANCH, hash)?;
        self.store.set_ref(HEAD_REF, hash)?;
        Ok(hash)
    }

    /// Walk every commit reachable from HEAD and verify signatures.
    ///
    /// Returns `(checked, signed, unsigned)` counts. Errors on the first
    /// signature that fails to verify.
    pub fn verify_signatures(&self) -> VexResult<(usize, usize, usize)> {
        let mut checked = 0usize;
        let mut signed = 0usize;
        let mut unsigned = 0usize;
        let mut cur = self.resolve_head()?;
        let mut seen: ahash::AHashSet<Hash256> = ahash::AHashSet::new();
        while let Some(h) = cur {
            if !seen.insert(h) {
                break;
            }
            let c = self.store.get_commit(h)?;
            checked += 1;
            if crate::signing::verify_commit(&c)? {
                signed += 1;
            } else {
                unsigned += 1;
            }
            cur = c.parents.first().copied();
        }
        Ok((checked, signed, unsigned))
    }

    /// Walk commit history from HEAD backwards.
    pub fn log(&self) -> VexResult<Vec<(Hash256, Commit)>> {
        let mut out = Vec::new();
        let mut cur = self.resolve_head()?;
        while let Some(h) = cur {
            let c = self.store.get_commit(h)?;
            let first_parent = c.parents.first().copied();
            out.push((h, c));
            cur = first_parent;
        }
        Ok(out)
    }

    /// Resolve HEAD (or its current branch) to a commit hash.
    pub fn resolve_head(&self) -> VexResult<Option<Hash256>> {
        if let Some(h) = self.store.get_ref(HEAD_REF)? {
            return Ok(Some(h));
        }
        self.store.get_ref(DEFAULT_BRANCH)
    }

    /// Resolve an arbitrary reference: `HEAD`, a branch name, or a 64-char hex
    /// commit hash. (Short prefix resolution is a future improvement.)
    pub fn resolve_ref(&self, name: &str) -> VexResult<Hash256> {
        if name.len() == 64 {
            if let Ok(h) = Hash256::from_hex(name) {
                if self.store.has(h)? {
                    return Ok(h);
                }
            }
        }
        if let Some(h) = self.store.get_ref(name)? {
            return Ok(h);
        }
        let branch = format!("refs/heads/{name}");
        if let Some(h) = self.store.get_ref(&branch)? {
            return Ok(h);
        }
        let tag = format!("refs/tags/{name}");
        if let Some(h) = self.store.get_ref(&tag)? {
            return Ok(h);
        }
        Err(VexError::InvalidRef(name.to_string()))
    }

    /// Diff two commit references and return a structured report.
    pub fn diff_refs(&self, a: &str, b: &str) -> VexResult<DiffReport> {
        let ha = self.resolve_ref(a)?;
        let hb = self.resolve_ref(b)?;
        let ca = self.store.get_commit(ha)?;
        let cb = self.store.get_commit(hb)?;
        let (ga, ia) = self.materialize_graph(ca.tree)?;
        let (gb, ib) = self.materialize_graph(cb.tree)?;
        Ok(diff(&ga, &ia, &gb, &ib, &self.hash_config()))
    }

    /// Render a diff between two refs to human-readable text.
    pub fn diff_refs_text(&self, a: &str, b: &str) -> VexResult<String> {
        let r = self.diff_refs(a, b)?;
        Ok(render_text(&r))
    }

    /// List the semantic elements present at a given ref. Reads node blobs
    /// directly from the committed tree, so the result is authoritative — it
    /// reflects exactly what was committed, with no IFC re-parsing or filename
    /// guessing. Returns the resolved commit hash and the element records,
    /// sorted by STEP id.
    pub fn elements(&self, reference: &str) -> VexResult<(Hash256, Vec<ElementRecord>)> {
        self.elements_rooted(reference, false)
    }

    /// List semantic elements, optionally limiting reads to IFC-rooted
    /// entities using the `global_id` already stored on each tree entry.
    pub fn elements_rooted(
        &self,
        reference: &str,
        rooted_only: bool,
    ) -> VexResult<(Hash256, Vec<ElementRecord>)> {
        let hash = self.resolve_ref(reference)?;
        let commit = self.store.get_commit(hash)?;
        let tree = self.store.get_tree(commit.tree)?;
        let selected: Vec<&TreeEntry> = tree
            .entries
            .iter()
            .filter(|entry| !rooted_only || entry.global_id.is_some())
            .collect();
        let mut out = Vec::with_capacity(selected.len());
        for entries in selected.chunks(BLOB_READ_BATCH_SIZE) {
            let hashes: Vec<Hash256> = entries.iter().map(|entry| entry.blob_hash).collect();
            for blob in self.store.get_blobs(&hashes)? {
                // The IFC `Name` attribute is positional slot `_2` on every
                // `IfcRoot`-derived entity (GlobalId, OwnerHistory, Name, ...).
                let name = blob.props.iter().find_map(|(k, v)| match v {
                    SerValue::Text(s) if k == "_2" && !s.is_empty() => Some(s.clone()),
                    _ => None,
                });
                out.push(ElementRecord {
                    type_name: blob.type_name,
                    step_id: blob.step_id,
                    global_id: blob.global_id,
                    name,
                });
            }
        }
        out.sort_by_key(|e| e.step_id);
        Ok((hash, out))
    }

    /// Export authoritative spatial containment metadata for a committed
    /// revision. Reads the committed tree's retained graph relationships —
    /// `Aggregates` for the Project → Site → Building → Storey hierarchy and
    /// `Contains` for element membership — so the result reflects exactly what
    /// was committed, with no IFC re-parsing and no rendered geometry bounds.
    ///
    /// The output is deterministic (stable ordering, de-duplicated element
    /// lists) and versioned by [`SPATIAL_SCHEMA`]. Elements assigned to more
    /// than one container are preserved in every container and additionally
    /// reported under [`SpatialContainment::ambiguous`]; rooted elements with
    /// no spatial assignment are reported under
    /// [`SpatialContainment::unassigned`]. Malformed relationships (missing
    /// endpoints) are skipped rather than treated as fatal.
    #[allow(clippy::too_many_lines, clippy::items_after_statements)]
    pub fn spatial_containment(&self, reference: &str) -> VexResult<(Hash256, SpatialContainment)> {
        use std::collections::{BTreeMap, BTreeSet};
        use vex_graph::ir::Value;
        use vex_graph::NodeId;

        // IFC positional attribute slots (0-based) for the two relationships
        // we consume. `IfcRelAggregates(GlobalId, OwnerHistory, Name,
        // Description, RelatingObject, RelatedObjects)` and
        // `IfcRelContainedInSpatialStructure(GlobalId, OwnerHistory, Name,
        // Description, RelatedElements, RelatingStructure)`.
        const AGG_PARENT_SLOT: u16 = 4; // RelatingObject (whole)
        const AGG_CHILD_SLOT: u16 = 5; // RelatedObjects (parts)
        const CONTAINS_ELEMENTS_SLOT: u16 = 4; // RelatedElements
        const CONTAINS_STRUCTURE_SLOT: u16 = 5; // RelatingStructure (container)

        let hash = self.resolve_ref(reference)?;
        let commit = self.store.get_commit(hash)?;
        let (graph, interner) = self.materialize_graph(commit.tree)?;

        // Resolve every node's identity once.
        struct Info {
            type_name: String,
            step_id: u64,
            global_id: Option<String>,
            name: Option<String>,
        }
        let info_of = |node: &vex_graph::ir::Node| -> Info {
            let name = node.props.iter().find_map(|(k, v)| match v {
                Value::Text(sid) if interner.resolve(*k) == "_2" => {
                    let s = interner.resolve(*sid);
                    (!s.is_empty()).then(|| s.to_string())
                }
                _ => None,
            });
            Info {
                type_name: interner.resolve(node.type_name).to_string(),
                step_id: node.step_id,
                global_id: node.global_id.as_ref().map(|g| g.0.clone()),
                name,
            }
        };
        let mut info: ahash::AHashMap<NodeId, Info> = ahash::AHashMap::new();
        for (id, node) in &graph.nodes {
            info.insert(id, info_of(node));
        }
        let to_ref = |id: NodeId| -> Option<SpatialRef> {
            info.get(&id).map(|i| SpatialRef {
                type_name: i.type_name.clone(),
                step_id: i.step_id,
                global_id: i.global_id.clone(),
                name: i.name.clone(),
            })
        };

        // Group the relationship edges by their originating relationship node,
        // splitting endpoints by IFC slot.
        #[derive(Default)]
        struct AggRel {
            parent: Option<NodeId>,
            children: Vec<NodeId>,
        }
        #[derive(Default)]
        struct ContainsRel {
            structure: Option<NodeId>,
            elements: Vec<NodeId>,
        }
        let mut aggs: ahash::AHashMap<NodeId, AggRel> = ahash::AHashMap::new();
        let mut contains: ahash::AHashMap<NodeId, ContainsRel> = ahash::AHashMap::new();
        for edge in &graph.edges {
            match edge.kind {
                EdgeKind::Aggregates => {
                    let e = aggs.entry(edge.from).or_default();
                    if edge.slot == AGG_PARENT_SLOT {
                        e.parent = Some(edge.to);
                    } else if edge.slot == AGG_CHILD_SLOT {
                        e.children.push(edge.to);
                    }
                }
                EdgeKind::Contains => {
                    let e = contains.entry(edge.from).or_default();
                    if edge.slot == CONTAINS_STRUCTURE_SLOT {
                        e.structure = Some(edge.to);
                    } else if edge.slot == CONTAINS_ELEMENTS_SLOT {
                        e.elements.push(edge.to);
                    }
                }
                _ => {}
            }
        }

        // A spatial container is any node of a recognized spatial-structure
        // type, plus any node used as the `RelatingStructure` of a `Contains`
        // relationship (resilient to unusual container types).
        let is_spatial_type = |t: &str| {
            matches!(
                t,
                "IFCPROJECT" | "IFCSITE" | "IFCBUILDING" | "IFCBUILDINGSTOREY" | "IFCSPACE"
            )
        };
        let mut container_ids: BTreeSet<NodeId> = BTreeSet::new();
        for (id, i) in &info {
            if is_spatial_type(&i.type_name) {
                container_ids.insert(*id);
            }
        }
        for rel in contains.values() {
            if let Some(s) = rel.structure {
                container_ids.insert(s);
            }
        }

        // Parent link per container from aggregation (deterministic on the
        // lowest parent STEP id when a container has multiple parents).
        let mut parent_of: ahash::AHashMap<NodeId, NodeId> = ahash::AHashMap::new();
        for rel in aggs.values() {
            let Some(parent) = rel.parent else { continue };
            for &child in &rel.children {
                if !container_ids.contains(&child) {
                    continue;
                }
                let take = match parent_of.get(&child) {
                    None => true,
                    Some(existing) => {
                        let new_sid = info.get(&parent).map_or(u64::MAX, |i| i.step_id);
                        let cur_sid = info.get(existing).map_or(u64::MAX, |i| i.step_id);
                        new_sid < cur_sid
                    }
                };
                if take {
                    parent_of.insert(child, parent);
                }
            }
        }

        // Element membership from containment.
        let mut members: ahash::AHashMap<NodeId, BTreeSet<NodeId>> = ahash::AHashMap::new();
        let mut membership: ahash::AHashMap<NodeId, BTreeSet<NodeId>> = ahash::AHashMap::new();
        for rel in contains.values() {
            let Some(structure) = rel.structure else {
                continue;
            };
            for &el in &rel.elements {
                members.entry(structure).or_default().insert(el);
                membership.entry(el).or_default().insert(structure);
            }
        }

        // Build the container records, ordered by (spatial rank, STEP id).
        let rank = |t: &str| match t {
            "IFCPROJECT" => 0u8,
            "IFCSITE" => 1,
            "IFCBUILDING" => 2,
            "IFCBUILDINGSTOREY" => 3,
            "IFCSPACE" => 4,
            _ => 5,
        };
        let mut containers: Vec<SpatialContainer> = Vec::with_capacity(container_ids.len());
        for &cid in &container_ids {
            let Some(entity) = to_ref(cid) else { continue };
            let mut gids: BTreeSet<String> = BTreeSet::new();
            let mut no_gid: BTreeSet<u64> = BTreeSet::new();
            if let Some(els) = members.get(&cid) {
                for &el in els {
                    if let Some(i) = info.get(&el) {
                        match &i.global_id {
                            Some(g) => {
                                gids.insert(g.clone());
                            }
                            None => {
                                no_gid.insert(i.step_id);
                            }
                        }
                    }
                }
            }
            containers.push(SpatialContainer {
                parent: parent_of.get(&cid).copied().and_then(to_ref),
                element_global_ids: gids.into_iter().collect(),
                element_step_ids_without_global_id: no_gid.into_iter().collect(),
                entity,
            });
        }
        containers.sort_by(|a, b| {
            rank(&a.entity.type_name)
                .cmp(&rank(&b.entity.type_name))
                .then(a.entity.step_id.cmp(&b.entity.step_id))
        });

        // Ambiguous memberships: elements claimed by more than one container.
        let mut ambiguous: Vec<AmbiguousMembership> = Vec::new();
        for (&el, structures) in &membership {
            if structures.len() < 2 {
                continue;
            }
            let Some(entity) = to_ref(el) else { continue };
            let mut refs: Vec<SpatialRef> = structures.iter().filter_map(|&s| to_ref(s)).collect();
            refs.sort_by_key(|r| r.step_id);
            ambiguous.push(AmbiguousMembership {
                entity,
                containers: refs,
            });
        }
        ambiguous.sort_by_key(|a| a.entity.step_id);

        // Unassigned rooted entities: carry a GlobalId, are not spatial
        // containers, are not IFC relationships, and are not contained.
        let mut unassigned_map: BTreeMap<u64, SpatialRef> = BTreeMap::new();
        for (id, i) in &info {
            if i.global_id.is_none()
                || container_ids.contains(id)
                || i.type_name.starts_with("IFCREL")
                || membership.contains_key(id)
            {
                continue;
            }
            if let Some(r) = to_ref(*id) {
                unassigned_map.insert(i.step_id, r);
            }
        }
        let unassigned: Vec<SpatialRef> = unassigned_map.into_values().collect();

        Ok((
            hash,
            SpatialContainment {
                containers,
                unassigned,
                ambiguous,
            },
        ))
    }

    /// Audit the entire object store. Returns the object count.
    pub fn verify(&self) -> VexResult<usize> {
        self.store.verify()
    }

    /// Compute the lowest common ancestor of two commits by walking parent
    /// links. O(N+M) in ancestor-set size; fine for local repos.
    pub fn lca(&self, a: Hash256, b: Hash256) -> VexResult<Option<Hash256>> {
        let ancestors_a = self.ancestors_of(a)?;
        // BFS from b; first ancestor also in ancestors_a wins.
        let mut queue: std::collections::VecDeque<Hash256> = std::collections::VecDeque::new();
        let mut seen: ahash::AHashSet<Hash256> = ahash::AHashSet::new();
        queue.push_back(b);
        seen.insert(b);
        while let Some(h) = queue.pop_front() {
            if ancestors_a.contains(&h) {
                return Ok(Some(h));
            }
            let c = self.store.get_commit(h)?;
            for p in c.parents {
                if seen.insert(p) {
                    queue.push_back(p);
                }
            }
        }
        Ok(None)
    }

    fn ancestors_of(&self, start: Hash256) -> VexResult<ahash::AHashSet<Hash256>> {
        let mut seen: ahash::AHashSet<Hash256> = ahash::AHashSet::new();
        let mut queue: std::collections::VecDeque<Hash256> = std::collections::VecDeque::new();
        queue.push_back(start);
        seen.insert(start);
        while let Some(h) = queue.pop_front() {
            let c = self.store.get_commit(h)?;
            for p in c.parents {
                if seen.insert(p) {
                    queue.push_back(p);
                }
            }
        }
        Ok(seen)
    }

    /// Three-way merge of `ours` and `theirs` based on their common ancestor.
    /// Returns a [`vex_diff::MergeResult`]; when `clean` is false, the caller
    /// is expected to surface conflicts to the user rather than advance HEAD.
    pub fn merge_refs(&self, ours: &str, theirs: &str) -> VexResult<vex_diff::MergeResult> {
        let ho = self.resolve_ref(ours)?;
        let ht = self.resolve_ref(theirs)?;
        let base = self
            .lca(ho, ht)?
            .ok_or_else(|| VexError::Config("no common ancestor".into()))?;
        let co = self.store.get_commit(ho)?;
        let ct = self.store.get_commit(ht)?;
        let cb = self.store.get_commit(base)?;
        let (gb, ib) = self.materialize_graph(cb.tree)?;
        let (go, io) = self.materialize_graph(co.tree)?;
        let (gt, it) = self.materialize_graph(ct.tree)?;
        Ok(vex_diff::merge_graphs(
            &gb,
            &ib,
            &go,
            &io,
            &gt,
            &it,
            &self.hash_config(),
        ))
    }

    /// Merge `theirs` into `ours` and (optionally) record the result.
    ///
    /// Behaviour:
    /// - If `theirs` is an ancestor of `ours` (or equal): [`MergeOutcome::UpToDate`].
    /// - If `ours` is an ancestor of `theirs`: fast-forward; HEAD/main advance to `theirs`
    ///   when `commit` is true. [`MergeOutcome::FastForward`].
    /// - Otherwise: run the 3-way merge.
    ///   - On conflicts: [`MergeOutcome::Conflicts`] (no commit).
    ///   - On a clean merge with `strategy = Some(side)` and `commit = true`: write a
    ///     2-parent commit whose tree is taken verbatim from the chosen side.
    ///     Full graph synthesis is deferred; this MVP records the merge in the DAG and
    ///     uses one side's tree as the merged content. [`MergeOutcome::Created`].
    ///   - On a clean merge without a strategy (or with `commit = false`):
    ///     [`MergeOutcome::Clean`] — caller must re-invoke with a strategy.
    #[allow(clippy::too_many_arguments)]
    pub fn merge_and_commit(
        &self,
        ours_ref: &str,
        theirs_ref: &str,
        message: Option<&str>,
        author_name: &str,
        author_email: &str,
        sign_with: Option<&str>,
        strategy: Option<MergeStrategy>,
        commit: bool,
    ) -> VexResult<MergeOutcome> {
        let ho = self.resolve_ref(ours_ref)?;
        let ht = self.resolve_ref(theirs_ref)?;
        if ho == ht {
            return Ok(MergeOutcome::UpToDate);
        }
        let anc_o = self.ancestors_of(ho)?;
        if anc_o.contains(&ht) {
            // theirs is already in ours' history.
            return Ok(MergeOutcome::UpToDate);
        }
        let anc_t = self.ancestors_of(ht)?;
        if anc_t.contains(&ho) {
            // Fast-forward: ours is ancestor of theirs.
            if commit {
                self.store.set_ref(DEFAULT_BRANCH, ht)?;
                self.store.set_ref(HEAD_REF, ht)?;
            }
            return Ok(MergeOutcome::FastForward(ht));
        }

        let result = self.merge_refs(ours_ref, theirs_ref)?;
        if !result.clean {
            return Ok(MergeOutcome::Conflicts(result));
        }
        let (Some(strat), true) = (strategy, commit) else {
            return Ok(MergeOutcome::Clean(result));
        };
        let co = self.store.get_commit(ho)?;
        let ct = self.store.get_commit(ht)?;
        let merged_tree = match strat {
            MergeStrategy::Ours => co.tree,
            MergeStrategy::Theirs => ct.tree,
        };
        let msg = message.map_or_else(
            || format!("Merge {theirs_ref} into {ours_ref}"),
            str::to_string,
        );
        let mut commit_obj = Commit {
            tree: merged_tree,
            parents: vec![ho, ht],
            author: Identity {
                name: author_name.to_string(),
                email: author_email.to_string(),
            },
            committer: Identity {
                name: "vex".into(),
                email: "system@vex".into(),
            },
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            message: msg,
            signature: None,
            profile_hash: self.profile.hash(),
        };
        if let Some(key) = sign_with {
            let vex_dir = self.root.join(".vex");
            let _ = crate::signing::sign_commit(&vex_dir, key, &mut commit_obj)?;
        }
        let hash = self.store.put_commit(&commit_obj)?;
        self.store.set_ref(DEFAULT_BRANCH, hash)?;
        self.store.set_ref(HEAD_REF, hash)?;
        Ok(MergeOutcome::Created {
            commit: hash,
            strategy: strat,
            result,
        })
    }

    // -------- tree write / read --------

    /// Serialize an in-memory graph into a Tree object (plus one Blob per node).
    fn write_tree(
        &self,
        graph: &IfcGraph,
        interner: &StringInterner,
    ) -> VexResult<(Hash256, Vec<Hash256>)> {
        let hashes = hash_graph(graph, interner, &self.hash_config());

        // Tree entries are addressed by node hash, so entry hashes must be
        // *unique* — otherwise WL-identical duplicates (identical entities
        // are routine in real exports: shared profiles, repeated meshes)
        // collapse on materialization, silently rewiring edges onto a single
        // survivor and orphaning its twins. Disambiguate duplicates with an
        // ordinal assigned in step-id order: deterministic, and isomorphic
        // across re-exports since the colliding nodes are indistinguishable.
        let mut ids: Vec<vex_graph::NodeId> = graph.nodes.iter().map(|(id, _)| id).collect();
        ids.sort_by_key(|id| graph.nodes[*id].step_id);
        let mut dup_count: ahash::AHashMap<Hash256, u32> = ahash::AHashMap::new();
        let mut entry_hash_of: ahash::AHashMap<vex_graph::NodeId, Hash256> =
            ahash::AHashMap::with_capacity(graph.node_count());
        for id in ids {
            let wl = hashes.per_node[&id];
            let n = dup_count.entry(wl).or_insert(0);
            let eh = if *n == 0 {
                wl
            } else {
                let mut h = vex_utils::Hasher::new(vex_utils::hash::HashAlgo::Blake3);
                h.update(b"dup:");
                h.update(wl.as_bytes());
                h.update(&n.to_be_bytes());
                h.finalize()
            };
            *n += 1;
            entry_hash_of.insert(id, eh);
        }

        let mut entries: Vec<TreeEntry> = Vec::with_capacity(graph.node_count());
        let mut blob_hashes: Vec<Hash256> = Vec::with_capacity(graph.node_count());

        let nodes: Vec<_> = graph.nodes.iter().collect();
        for chunk in nodes.chunks(BLOB_WRITE_BATCH_SIZE) {
            let blobs: Vec<Blob> = chunk
                .iter()
                .map(|(_, node)| Blob {
                    type_name: interner.resolve(node.type_name).to_string(),
                    step_id: node.step_id,
                    global_id: node.global_id.as_ref().map(|g| g.0.clone()),
                    props: node
                        .props
                        .iter()
                        .map(|(k, v)| (interner.resolve(*k).to_string(), to_ser(v, interner)))
                        .collect(),
                })
                .collect();
            let chunk_hashes = self.store.put_blobs(&blobs)?;
            for ((id, node), blob_hash) in chunk.iter().zip(chunk_hashes) {
                blob_hashes.push(blob_hash);
                entries.push(TreeEntry {
                    node_hash: entry_hash_of[id],
                    blob_hash,
                    global_id: node.global_id.as_ref().map(|g| g.0.clone()),
                });
            }
        }
        entries.sort_by_key(|e| *e.node_hash.as_bytes());

        let mut edges: Vec<TreeEdge> = graph
            .edges
            .iter()
            .map(|e| TreeEdge {
                from: entry_hash_of[&e.from],
                to: entry_hash_of[&e.to],
                kind: edge_kind_u8(e.kind),
                slot: e.slot,
                list_index: e.list_index,
            })
            .collect();
        edges.sort_by(|a, b| {
            (
                a.from.as_bytes(),
                a.to.as_bytes(),
                a.kind,
                a.slot,
                a.list_index,
            )
                .cmp(&(
                    b.from.as_bytes(),
                    b.to.as_bytes(),
                    b.kind,
                    b.slot,
                    b.list_index,
                ))
        });

        let tree = Tree {
            schema: graph.schema.clone(),
            entries,
            edges,
        };
        let tree_hash = self.store.put_tree(&tree)?;
        Ok((tree_hash, blob_hashes))
    }

    /// Rehydrate a Tree (and its Blobs) back into an in-memory graph for diffing.
    ///
    /// Note: the returned graph is *not* byte-identical to the original IFC —
    /// it's a canonical re-projection. That's sufficient for diffing, which is
    /// the only operation that consumes it today.
    fn materialize_graph(&self, tree_hash: Hash256) -> VexResult<(IfcGraph, StringInterner)> {
        use smallvec::SmallVec;
        use vex_graph::ir::{Edge, GlobalId, Node, Value};

        let tree = self.store.get_tree(tree_hash)?;
        let interner = StringInterner::new();
        let mut graph = IfcGraph::new();
        graph.schema.clone_from(&tree.schema);
        let mut hash_to_node: ahash::AHashMap<Hash256, vex_graph::NodeId> =
            ahash::AHashMap::with_capacity(tree.entries.len());

        for entries in tree.entries.chunks(BLOB_READ_BATCH_SIZE) {
            let hashes: Vec<Hash256> = entries.iter().map(|entry| entry.blob_hash).collect();
            let blobs = self.store.get_blobs(&hashes)?;
            for (entry, blob) in entries.iter().zip(blobs) {
                let type_id = interner.intern(&blob.type_name);
                let props: SmallVec<[(vex_utils::StringId, Value); 8]> = blob
                    .props
                    .iter()
                    .map(|(k, v)| (interner.intern(k), from_ser(v, &interner)))
                    .collect();
                let node_id = graph.insert_node(Node {
                    type_name: type_id,
                    step_id: blob.step_id,
                    global_id: blob.global_id.clone().map(GlobalId),
                    props,
                });
                hash_to_node.insert(entry.node_hash, node_id);
            }
        }

        for edge in &tree.edges {
            let from = hash_to_node
                .get(&edge.from)
                .copied()
                .ok_or_else(|| VexError::Graph("tree edge references unknown from-hash".into()))?;
            let to = hash_to_node
                .get(&edge.to)
                .copied()
                .ok_or_else(|| VexError::Graph("tree edge references unknown to-hash".into()))?;
            graph.add_edge(Edge {
                from,
                to,
                kind: edge_kind_from_u8(edge.kind),
                slot: edge.slot,
                list_index: edge.list_index,
            });
        }

        Ok((graph, interner))
    }

    // -------- Phase 4: branches / tags / status / checkout / gc --------

    /// Create a branch pointing at `target` (or HEAD when `None`).
    /// Errors if the branch already exists.
    pub fn branch_create(&self, name: &str, target: Option<&str>) -> VexResult<Hash256> {
        let ref_name = format!("refs/heads/{name}");
        if self.store.get_ref(&ref_name)?.is_some() {
            return Err(VexError::Config(format!("branch already exists: {name}")));
        }
        let hash = match target {
            Some(t) => self.resolve_ref(t)?,
            None => self
                .resolve_head()?
                .ok_or_else(|| VexError::Config("no commits yet".into()))?,
        };
        self.store.set_ref(&ref_name, hash)?;
        Ok(hash)
    }

    /// List all branches as (name, hash) pairs.
    pub fn branches(&self) -> VexResult<Vec<(String, Hash256)>> {
        let mut out: Vec<(String, Hash256)> = self
            .store
            .list_refs()?
            .into_iter()
            .filter_map(|(n, h)| n.strip_prefix("refs/heads/").map(|b| (b.to_string(), h)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Delete a branch by short name.
    pub fn branch_delete(&self, name: &str) -> VexResult<bool> {
        let ref_name = format!("refs/heads/{name}");
        self.store.delete_ref(&ref_name)
    }

    /// Create a lightweight tag pointing at `target` (or HEAD when `None`).
    pub fn tag_create(&self, name: &str, target: Option<&str>) -> VexResult<Hash256> {
        let ref_name = format!("refs/tags/{name}");
        if self.store.get_ref(&ref_name)?.is_some() {
            return Err(VexError::Config(format!("tag already exists: {name}")));
        }
        let hash = match target {
            Some(t) => self.resolve_ref(t)?,
            None => self
                .resolve_head()?
                .ok_or_else(|| VexError::Config("no commits yet".into()))?,
        };
        self.store.set_ref(&ref_name, hash)?;
        Ok(hash)
    }

    /// List all tags.
    pub fn tags(&self) -> VexResult<Vec<(String, Hash256)>> {
        let mut out: Vec<(String, Hash256)> = self
            .store
            .list_refs()?
            .into_iter()
            .filter_map(|(n, h)| n.strip_prefix("refs/tags/").map(|t| (t.to_string(), h)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Delete a tag by short name.
    pub fn tag_delete(&self, name: &str) -> VexResult<bool> {
        let ref_name = format!("refs/tags/{name}");
        self.store.delete_ref(&ref_name)
    }

    /// Compute a status report: staged vs HEAD (added/removed/modified counts).
    pub fn status(&self) -> VexResult<Status> {
        let staged = self.staged_tree()?;
        let head = self.resolve_head()?;
        match (staged, head) {
            (None, None) => Ok(Status {
                staged: None,
                head: None,
                summary: None,
            }),
            (Some(s), None) => Ok(Status {
                staged: Some(s),
                head: None,
                summary: None,
            }),
            (staged_opt, Some(head_hash)) => {
                let head_commit = self.store.get_commit(head_hash)?;
                let (g_head, i_head) = self.materialize_graph(head_commit.tree)?;
                if let Some(s) = staged_opt {
                    let tree = self.store.get_tree(s)?;
                    let (g_stg, i_stg) = self.materialize_graph_from_tree(&tree)?;
                    let report = diff(&g_head, &i_head, &g_stg, &i_stg, &self.hash_config());
                    Ok(Status {
                        staged: Some(s),
                        head: Some(head_hash),
                        summary: Some(StatusSummary {
                            added: report.summary.added,
                            removed: report.summary.removed,
                            modified: report.summary.modified,
                        }),
                    })
                } else {
                    Ok(Status {
                        staged: None,
                        head: Some(head_hash),
                        summary: None,
                    })
                }
            }
        }
    }

    fn materialize_graph_from_tree(&self, tree: &Tree) -> VexResult<(IfcGraph, StringInterner)> {
        // Stash the tree object addressable by its hash. We already have the
        // decoded tree; we need the blobs it references, which live in the
        // store. So just call the existing materializer on the tree_hash.
        // For staging, the tree is already written during import.
        let tree_hash = self.store.put_tree(tree)?;
        self.materialize_graph(tree_hash)
    }

    /// Checkout a commit: write a canonical IFC text representation to `out`.
    /// The round-trip is semantic, not byte-identical — headers are minimal.
    pub fn checkout(&self, reference: &str, out: impl AsRef<Path>) -> VexResult<usize> {
        let hash = self.resolve_ref(reference)?;
        let commit = self.store.get_commit(hash)?;
        let (graph, interner) = self.materialize_graph(commit.tree)?;
        let text = render_ifc(&graph, &interner);
        let bytes = text.as_bytes();
        std::fs::write(out.as_ref(), bytes).map_err(|e| VexError::io_at(out.as_ref(), e))?;
        Ok(bytes.len())
    }

    /// Garbage-collect unreachable objects. Keeps everything reachable from
    /// any ref (commits, their trees, their blobs, their manifests + parents).
    /// Returns `(kept, deleted)` counts.
    pub fn gc(&self) -> VexResult<(usize, usize)> {
        let mut reachable: ahash::AHashSet<Hash256> = ahash::AHashSet::new();
        let mut frontier: Vec<Hash256> = self
            .store
            .list_refs()?
            .into_iter()
            .map(|(_, h)| h)
            .collect();
        while let Some(h) = frontier.pop() {
            if !reachable.insert(h) {
                continue;
            }
            // Try commit → tree → blobs.
            if let Ok(commit) = self.store.get_commit(h) {
                frontier.push(commit.tree);
                for p in &commit.parents {
                    frontier.push(*p);
                }
                continue;
            }
            if let Ok(tree) = self.store.get_tree(h) {
                for entry in &tree.entries {
                    reachable.insert(entry.blob_hash);
                }
            }
            // Blobs are terminal; nothing to descend into.
        }
        let all = self.store.list_object_hashes()?;
        let to_delete: Vec<Hash256> = all
            .iter()
            .filter(|h| !reachable.contains(h))
            .copied()
            .collect();
        let deleted = self.store.delete_objects(&to_delete)?;
        let kept = all.len() - deleted;
        Ok((kept, deleted))
    }
}

/// Status of the working-tree vs HEAD.
#[derive(Debug)]
pub struct Status {
    pub staged: Option<Hash256>,
    pub head: Option<Hash256>,
    pub summary: Option<StatusSummary>,
}

#[derive(Debug)]
pub struct StatusSummary {
    pub added: u32,
    pub removed: u32,
    pub modified: u32,
}

/// Strategy for resolving a clean 3-way merge into a tree.
///
/// Full graph synthesis is deferred; this MVP records the merge in the DAG and
/// uses one side's tree as the merged content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategy {
    Ours,
    Theirs,
}

/// Outcome of [`Repository::merge_and_commit`].
#[derive(Debug)]
pub enum MergeOutcome {
    /// `theirs` is already reachable from `ours`; nothing to do.
    UpToDate,
    /// `ours` is an ancestor of `theirs`; HEAD advanced to `theirs` (when committing).
    FastForward(Hash256),
    /// 3-way merge succeeded but no commit was recorded — caller must pick a strategy.
    Clean(vex_diff::MergeResult),
    /// Merge could be auto-resolved and a commit was recorded.
    Created {
        commit: Hash256,
        strategy: MergeStrategy,
        result: vex_diff::MergeResult,
    },
    /// Merge has conflicts; no commit recorded.
    Conflicts(vex_diff::MergeResult),
}

/// Render an [`IfcGraph`] back to minimal IFC text. Not byte-identical to the
/// original source — arguments that were references become `#N` and numeric
/// values are printed in Rust `Display` form. Good enough for a
/// semantically-equivalent reload.
fn render_ifc(graph: &IfcGraph, interner: &StringInterner) -> String {
    use std::collections::BTreeMap;
    use std::fmt::Write;
    use vex_graph::ir::Value;

    // Stable ordering: sort nodes by step_id so output is reproducible.
    let mut ordered: Vec<(vex_graph::NodeId, &vex_graph::ir::Node)> = graph.nodes.iter().collect();
    ordered.sort_by_key(|(_, n)| n.step_id);
    // Remap step_ids to a dense 1..N space so checkout output is tidy.
    let mut step_of: ahash::AHashMap<vex_graph::NodeId, u64> =
        ahash::AHashMap::with_capacity(ordered.len());
    for (i, (id, _)) in ordered.iter().enumerate() {
        step_of.insert(*id, (i as u64) + 1);
    }

    // Bucket outgoing edges by (from, slot, list_index) for quick arg lookup.
    let mut edges_by_node: ahash::AHashMap<
        vex_graph::NodeId,
        BTreeMap<(u16, u16), vex_graph::NodeId>,
    > = ahash::AHashMap::new();
    for e in &graph.edges {
        edges_by_node
            .entry(e.from)
            .or_default()
            .insert((e.slot, e.list_index), e.to);
    }

    let schema = graph.schema.clone().unwrap_or_else(|| "IFC4".to_string());

    let mut out = String::new();
    out.push_str("ISO-10303-21;\n");
    out.push_str("HEADER;\n");
    out.push_str("FILE_DESCRIPTION((''),'2;1');\n");
    out.push_str("FILE_NAME('','',(''),(''),'vex-checkout','','');\n");
    let _ = writeln!(out, "FILE_SCHEMA(('{schema}'));");
    out.push_str("ENDSEC;\n");
    out.push_str("DATA;\n");

    for (id, node) in &ordered {
        let step = step_of[id];
        let type_name = interner.resolve(node.type_name);
        let _ = write!(out, "#{step} = {type_name}(");
        // Props are keyed `_0`, `_1`, ...; pull them back into positional
        // order. Fill gaps with `$` and replace Nulls that correspond to
        // edge slots with `#N`.
        let mut by_slot: BTreeMap<u16, &Value> = BTreeMap::new();
        for (k, v) in &node.props {
            let key = interner.resolve(*k);
            if let Some(rest) = key.strip_prefix('_') {
                if let Ok(slot) = rest.parse::<u16>() {
                    by_slot.insert(slot, v);
                }
            }
        }
        let max_slot = by_slot.keys().copied().max().unwrap_or(0);
        let mut first = true;
        for slot in 0..=max_slot {
            if !first {
                out.push(',');
            }
            first = false;
            // Edge in this slot?
            let edge_targets: Vec<_> = edges_by_node
                .get(id)
                .map(|m| m.range((slot, 0u16)..(slot + 1, 0u16)).collect::<Vec<_>>())
                .unwrap_or_default();
            if !edge_targets.is_empty() {
                if edge_targets.len() == 1 && edge_targets[0].0 .1 == u16::MAX {
                    let target = *edge_targets[0].1;
                    let _ = write!(out, "#{}", step_of[&target]);
                } else {
                    // List argument.
                    out.push('(');
                    let mut f2 = true;
                    for ((_, _li), to) in edge_targets {
                        if !f2 {
                            out.push(',');
                        }
                        f2 = false;
                        let _ = write!(out, "#{}", step_of[to]);
                    }
                    out.push(')');
                }
                continue;
            }
            match by_slot.get(&slot) {
                Some(v) => render_value(&mut out, v, interner),
                None => out.push('$'),
            }
        }
        out.push_str(");\n");
    }

    out.push_str("ENDSEC;\n");
    out.push_str("END-ISO-10303-21;\n");
    out
}

fn render_value(out: &mut String, v: &vex_graph::ir::Value, i: &StringInterner) {
    use vex_graph::ir::Value;
    match v {
        Value::Null => out.push('$'),
        Value::Bool(b) => out.push_str(if *b { ".T." } else { ".F." }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Real(x) => {
            let s = format!("{x}");
            if s.contains('.') || s.contains('e') || s.contains('E') {
                out.push_str(&s);
            } else {
                out.push_str(&s);
                out.push_str(".0");
            }
        }
        Value::Text(s) => {
            out.push('\'');
            for c in i.resolve(*s).chars() {
                if c == '\'' {
                    out.push_str("''");
                } else {
                    out.push(c);
                }
            }
            out.push('\'');
        }
        Value::Enum(s) => {
            out.push('.');
            out.push_str(i.resolve(*s));
            out.push('.');
        }
        Value::List(xs) => {
            out.push('(');
            let mut first = true;
            for x in xs {
                if !first {
                    out.push(',');
                }
                first = false;
                render_value(out, x, i);
            }
            out.push(')');
        }
        Value::Typed { name, inner } => {
            out.push_str(i.resolve(*name));
            out.push('(');
            render_value(out, inner, i);
            out.push(')');
        }
    }
}

fn to_ser(v: &vex_graph::ir::Value, i: &StringInterner) -> SerValue {
    use vex_graph::ir::Value;
    match v {
        Value::Null => SerValue::Null,
        Value::Bool(b) => SerValue::Bool(*b),
        Value::Int(n) => SerValue::Int(*n),
        Value::Real(x) => SerValue::Real(*x),
        Value::Text(s) => SerValue::Text(i.resolve(*s).to_string()),
        Value::Enum(s) => SerValue::Enum(i.resolve(*s).to_string()),
        Value::List(xs) => SerValue::List(xs.iter().map(|x| to_ser(x, i)).collect()),
        Value::Typed { name, inner } => SerValue::Typed {
            name: i.resolve(*name).to_string(),
            inner: Box::new(to_ser(inner, i)),
        },
    }
}

fn from_ser(v: &SerValue, i: &StringInterner) -> vex_graph::ir::Value {
    use vex_graph::ir::Value;
    match v {
        SerValue::Null => Value::Null,
        SerValue::Bool(b) => Value::Bool(*b),
        SerValue::Int(n) => Value::Int(*n),
        SerValue::Real(x) => Value::Real(*x),
        SerValue::Text(s) => Value::Text(i.intern(s)),
        SerValue::Enum(s) => Value::Enum(i.intern(s)),
        SerValue::List(xs) => Value::List(xs.iter().map(|x| from_ser(x, i)).collect()),
        SerValue::Typed { name, inner } => Value::Typed {
            name: i.intern(name),
            inner: Box::new(from_ser(inner, i)),
        },
    }
}

fn edge_kind_u8(k: EdgeKind) -> u8 {
    match k {
        EdgeKind::Other => 0,
        EdgeKind::Contains => 1,
        EdgeKind::Aggregates => 2,
        EdgeKind::Defines => 3,
        EdgeKind::Connects => 4,
        EdgeKind::Assigns => 5,
        EdgeKind::Associates => 6,
        EdgeKind::TypeRef => 7,
        EdgeKind::PropertyRef => 8,
    }
}

fn edge_kind_from_u8(b: u8) -> EdgeKind {
    match b {
        1 => EdgeKind::Contains,
        2 => EdgeKind::Aggregates,
        3 => EdgeKind::Defines,
        4 => EdgeKind::Connects,
        5 => EdgeKind::Assigns,
        6 => EdgeKind::Associates,
        7 => EdgeKind::TypeRef,
        8 => EdgeKind::PropertyRef,
        _ => EdgeKind::Other,
    }
}

fn default_config_toml() -> String {
    // Keep this in sync with `Profile::default`. Emitted verbatim on `init`.
    r#"# Vex repository configuration.
#
# The normalization profile controls how IFC data is canonicalized before
# hashing and diffing. Changing these values produces a different `profile_hash`
# on new commits; Vex records the hash per commit so mixed-profile histories
# remain detectable.

[normalization]
# Linear tolerance in meters. Real values are quantized to this bucket before
# hashing. Default: 1 micrometer.
tolerance_linear = 0.000001

# Angular tolerance in radians. Default: ~5.7 microradians.
tolerance_angular = 0.000001

# Entity types to drop from the graph entirely. Useful for export-noise types.
# Comparison is case-insensitive.
ignore_types = ["IFCOWNERHISTORY"]

# Property keys to drop before hashing. In the default MVP schema, keys are
# the positional slot names like "_3". In future releases these will include
# lifted IfcPropertySet keys.
ignore_prop_keys = []
"#
    .to_string()
}

#[derive(Debug, serde::Deserialize)]
struct RawConfig {
    #[serde(default)]
    normalization: RawNormalization,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawNormalization {
    #[serde(default)]
    tolerance_linear: Option<f64>,
    #[serde(default)]
    tolerance_angular: Option<f64>,
    #[serde(default)]
    ignore_types: Option<Vec<String>>,
    #[serde(default)]
    ignore_prop_keys: Option<Vec<String>>,
}

fn load_profile(vex_dir: &Path) -> VexResult<Profile> {
    let path = vex_dir.join(CONFIG_FILE);
    if !path.exists() {
        return Ok(Profile::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| VexError::io_at(&path, e))?;
    let cfg: RawConfig =
        toml::from_str(&text).map_err(|e| VexError::Config(format!("invalid config.toml: {e}")))?;
    let default = Profile::default();
    let n = cfg.normalization;
    let mut profile = Profile {
        tolerance_linear: n.tolerance_linear.unwrap_or(default.tolerance_linear),
        tolerance_angular: n.tolerance_angular.unwrap_or(default.tolerance_angular),
        ignore_types: n
            .ignore_types
            .map(|v| v.into_iter().collect())
            .unwrap_or(default.ignore_types),
        ignore_prop_keys: n
            .ignore_prop_keys
            .map(|v| v.into_iter().collect())
            .unwrap_or_default(),
    };
    // Normalize ignore_types to uppercase for consistent comparison.
    profile.ignore_types = profile
        .ignore_types
        .into_iter()
        .map(|s| s.to_ascii_uppercase())
        .collect();
    Ok(profile)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const SAMPLE_A: &str = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = IFCPROJECT('0YvctVUKr0kugbFTf53O9L',$,'Project',$,$,$,$,$,$);
#2 = IFCWALL('2O2Fr$t4X7Zf8NOew3FNr2',$,'Wall-1',$,$,$,$,$,.STANDARD.);
ENDSEC;
END-ISO-10303-21;
";

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "vex-core-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    fn write_ifc(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).expect("write");
        p
    }

    #[test]
    fn end_to_end_commit_log_diff() {
        let dir = tempdir();
        let repo = Repository::init(&dir).expect("init");
        let file_a = write_ifc(&dir, "a.ifc", SAMPLE_A);
        repo.import(&file_a).expect("import a");
        let h1 = repo
            .commit("initial", "Alice", "alice@ex.com")
            .expect("commit 1");

        let mutated = SAMPLE_A.replace("'Wall-1'", "'Wall-2'");
        let file_b = write_ifc(&dir, "b.ifc", &mutated);
        repo.import(&file_b).expect("import b");
        let h2 = repo
            .commit("renamed wall", "Alice", "alice@ex.com")
            .expect("commit 2");

        assert_ne!(h1, h2);

        let log = repo.log().expect("log");
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].0, h2);
        assert_eq!(log[1].0, h1);

        let report = repo.diff_refs(&h1.to_hex(), &h2.to_hex()).expect("diff");
        assert_eq!(report.summary.modified, 1);
        assert_eq!(report.summary.added, 0);
        assert_eq!(report.summary.removed, 0);

        let n = repo.verify().expect("verify");
        assert!(n >= 2);
    }
}
