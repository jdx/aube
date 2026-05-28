//! Hoisted (`node-linker=hoisted`) layout.
//!
//! Unlike the isolated layout — which materializes every package under
//! a per-project `.aube/<dep_path>/` virtual store and builds Node's
//! module graph out of symlinks — the hoisted layout writes real
//! package directories straight into `node_modules/`, nesting
//! conflicting versions under the parent that requires them. This
//! matches npm / yarn-classic's flat tree and is what certain legacy
//! toolchains (React Native's Metro, some Jest plugins) require.
//!
//! Placement algorithm (npm-style, per importer):
//!
//! 1. Start with a `TreeNode` for the importer — its `node_modules`
//!    directory and an empty child map.
//! 2. BFS from the importer's direct deps. For each `(requester, name,
//!    dep_path)` pair, walk up from the requester looking for the
//!    shallowest ancestor whose `children[name]` is either absent or
//!    points at the same `dep_path`. That ancestor becomes the
//!    placement site.
//! 3. If a matching entry already exists at that ancestor, reuse it
//!    (dedupe). Otherwise create a new child node and enqueue every
//!    transitive dep of the placed package with the new node as
//!    requester.
//! 4. Conflicting versions naturally nest: when walking up from the
//!    requester we stop as soon as we find a different `dep_path`
//!    under the same name, so the conflict forces the new entry to
//!    live below the blocker (typically inside the requester's own
//!    `node_modules/`).
//!
//! The planner operates purely on dep_path strings — the same keys
//! aube-lockfile uses — so peer-context dep_paths like
//! `react-router@6(react@18)` are treated as distinct and won't
//! collapse onto a plain `react-router@6` placement. The side effect
//! is that peer-variant conflicts nest deeper in hoisted mode than in
//! isolated mode, which is the correct-but-slightly-inefficient
//! fallback.
//!
//! The planner output (`PlacementPlan`) is consumed by the
//! materializer in `link_hoisted_importer` and also surfaced to the
//! install driver via `HoistedPlacements` so bin linking and
//! dependency lifecycle scripts can locate a package's on-disk
//! directory without recomputing the tree.

use crate::{Error, LinkStats, Linker, apply_multi_file_patch};
use aube_lockfile::{DirectDep, LocalSource, LockfileGraph};
use aube_store::PackageIndex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

/// Map from lockfile `dep_path` to the absolute on-disk directories
/// where that package ended up. Most entries have exactly one path;
/// packages whose name conflicts with a shallower version end up
/// duplicated across multiple parent `node_modules/` directories so
/// each gets its own on-disk copy.
#[derive(Debug, Default, Clone)]
pub struct HoistedPlacements {
    by_dep_path: BTreeMap<String, Vec<PathBuf>>,
}

impl HoistedPlacements {
    /// Recompute hoisted placement paths for an already-linked graph
    /// without touching disk. Used by commands like `aube rebuild`
    /// that need to find package directories after install, but must
    /// not relink node_modules. `modules_dir_name` must match the
    /// `modulesDir` setting the install used, or the computed paths
    /// won't match what's on disk.
    pub fn from_graph(root_dir: &Path, graph: &LockfileGraph, modules_dir_name: &str) -> Self {
        let mut placements = Self::default();
        for (importer_path, deps) in &graph.importers {
            if !crate::is_physical_importer(importer_path) {
                continue;
            }
            let importer_dir = if importer_path == "." {
                root_dir.to_path_buf()
            } else {
                root_dir.join(importer_path)
            };
            let nm = importer_dir.join(modules_dir_name);
            let Ok(plan) = plan_importer(&nm, deps, graph) else {
                continue;
            };
            for node in &plan.nodes {
                let (Some(dep_path), Some(pkg_dir)) = (&node.dep_path, &node.pkg_dir) else {
                    continue;
                };
                if pkg_dir.exists() {
                    placements.record(dep_path, pkg_dir.clone());
                }
            }
        }
        placements
    }

    /// Shallowest placement for `dep_path`, or `None` if the dep is
    /// not in the hoisted tree (e.g. filtered by `--prod` /
    /// `--no-optional`). Used by the install driver as the canonical
    /// location for bin linking and lifecycle-script cwds.
    pub fn package_dir(&self, dep_path: &str) -> Option<&Path> {
        self.by_dep_path
            .get(dep_path)
            .and_then(|v| v.first())
            .map(|p| p.as_path())
    }

    /// Every placement site for `dep_path`. When a name conflicts
    /// with a shallower version the same dep_path may appear at
    /// multiple depths; lifecycle scripts run once per site so each
    /// copy has its native-build artifacts in place.
    pub fn all_package_dirs(&self, dep_path: &str) -> &[PathBuf] {
        self.by_dep_path
            .get(dep_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Iterate `(dep_path, placement_path)` pairs in BTree order.
    /// Primarily used by the top-level installer when it wants to
    /// walk every placed copy (e.g. the stale-directory sweep or the
    /// lifecycle-script dispatcher).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.by_dep_path
            .iter()
            .flat_map(|(k, v)| v.iter().map(move |p| (k.as_str(), p.as_path())))
    }

    pub(crate) fn record(&mut self, dep_path: &str, path: PathBuf) {
        self.by_dep_path
            .entry(dep_path.to_string())
            .or_default()
            .push(path);
    }
}

/// One node in the placement tree. A node is either the importer
/// root (`pkg_dir == None`) or a placed package. `nm_dir` is the
/// `node_modules/` directory underneath this node where its children
/// live — for the importer that's `<importer>/node_modules`, for a
/// placed package it's `<parent.nm_dir>/<name>/node_modules`.
struct TreeNode {
    pkg_dir: Option<PathBuf>,
    nm_dir: PathBuf,
    parent: Option<usize>,
    children: BTreeMap<String, usize>,
    dep_path: Option<String>,
}

/// Arena-backed placement tree.
pub(crate) struct PlacementPlan {
    nodes: Vec<TreeNode>,
    root_idx: usize,
}

struct PlaceOutcome {
    node_idx: usize,
    created: bool,
}

impl PlacementPlan {
    fn new(importer_nm: PathBuf) -> Self {
        let root = TreeNode {
            pkg_dir: None,
            nm_dir: importer_nm,
            parent: None,
            children: BTreeMap::new(),
            dep_path: None,
        };
        Self {
            nodes: vec![root],
            root_idx: 0,
        }
    }

    /// Place `(name, dep_path)` under the ancestor chain rooted at
    /// `requester`. Returns the resulting node index and whether a
    /// fresh entry was created (so the caller knows whether to
    /// enqueue transitive deps).
    fn place(
        &mut self,
        requester: usize,
        name: &str,
        dep_path: &str,
    ) -> Result<PlaceOutcome, Error> {
        crate::validate_package_link_name(name)?;
        // Walk up from the requester looking for the shallowest
        // ancestor that doesn't already host a different version of
        // `name`. If any ancestor has a matching entry, reuse it.
        let mut cursor = requester;
        let mut candidate = requester;
        loop {
            if let Some(&existing) = self.nodes[cursor].children.get(name) {
                if self.nodes[existing].dep_path.as_deref() == Some(dep_path) {
                    return Ok(PlaceOutcome {
                        node_idx: existing,
                        created: false,
                    });
                }
                // Conflict: must stay at or below `candidate`.
                break;
            }
            candidate = cursor;
            match self.nodes[cursor].parent {
                Some(p) => cursor = p,
                None => break,
            }
        }

        let parent_nm = self.nodes[candidate].nm_dir.clone();
        let pkg_dir = parent_nm.join(name);
        let nm_dir = pkg_dir.join("node_modules");
        let new_idx = self.nodes.len();
        self.nodes.push(TreeNode {
            pkg_dir: Some(pkg_dir),
            nm_dir,
            parent: Some(candidate),
            children: BTreeMap::new(),
            dep_path: Some(dep_path.to_string()),
        });
        self.nodes[candidate]
            .children
            .insert(name.to_string(), new_idx);
        Ok(PlaceOutcome {
            node_idx: new_idx,
            created: true,
        })
    }

    /// Names placed directly in the importer root's `node_modules/`.
    /// Drives the stale-entry sweep in `link_hoisted_importer`.
    pub(crate) fn root_names(&self) -> impl Iterator<Item = &str> {
        self.nodes[self.root_idx]
            .children
            .keys()
            .map(|s| s.as_str())
    }
}

/// Build a placement plan for a single importer.
pub(crate) fn plan_importer(
    importer_nm: &Path,
    root_deps: &[DirectDep],
    graph: &LockfileGraph,
) -> Result<PlacementPlan, Error> {
    let mut plan = PlacementPlan::new(importer_nm.to_path_buf());
    let mut queue: VecDeque<(usize, String, String)> = VecDeque::new();

    // Seed the queue with the importer's direct deps in declaration
    // order. BFS makes shallower deps win placement ties over
    // deeper ones, which matches npm's first-writer-wins policy.
    for dep in root_deps {
        if !graph.packages.contains_key(&dep.dep_path) {
            continue;
        }
        queue.push_back((plan.root_idx, dep.name.clone(), dep.dep_path.clone()));
    }

    while let Some((requester, name, dep_path)) = queue.pop_front() {
        let outcome = plan.place(requester, &name, &dep_path)?;
        if !outcome.created {
            continue;
        }
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        // Skip transitives for `link:` deps — their target directory
        // holds its own node_modules and Node resolves through it
        // naturally. Materializing a copy would fight with a live
        // workspace package.
        if matches!(pkg.local_source.as_ref(), Some(LocalSource::Link(_))) {
            continue;
        }
        for (dep_name, dep_tail) in &pkg.dependencies {
            let child_dep_path = format!("{dep_name}@{dep_tail}");
            if !graph.packages.contains_key(&child_dep_path) {
                continue;
            }
            queue.push_back((outcome.node_idx, dep_name.clone(), child_dep_path));
        }
    }

    Ok(plan)
}

/// Materialize a planned tree onto disk for a single importer.
///
/// Called by `Linker::link_all` and `Linker::link_workspace` when the
/// linker is configured with `NodeLinker::Hoisted`. The importer's
/// existing `node_modules/` is swept of any top-level entries the
/// plan doesn't claim (direct deps from a previous install may have
/// changed); placed packages are then materialized in two passes —
/// local (`file:`/`link:`) first, then registry packages via the
/// standard reflink/hardlink/copy file-linker.
///
/// Every placed package is recorded in `placements` so the install
/// driver can later resolve `dep_path -> on-disk dir` for bin
/// linking and lifecycle scripts without recomputing the plan.
pub(crate) fn link_hoisted_importer(
    linker: &Linker,
    importer_dir: &Path,
    root_deps: &[DirectDep],
    graph: &LockfileGraph,
    package_indices: &BTreeMap<String, PackageIndex>,
    stats: &mut LinkStats,
    placements: &mut HoistedPlacements,
) -> Result<(), Error> {
    let nm = importer_dir.join(linker.modules_dir_name());
    crate::mkdirp(&nm)?;

    let plan = plan_importer(&nm, root_deps, graph)?;

    // Sweep any top-level entries that are no longer claimed by the
    // plan. Dotfiles (`.aube`, `.bin`, …) are preserved — .aube in
    // particular may hold a previous isolated tree that the user
    // hasn't switched off; we leave it alone rather than wiping
    // bytes the other layout owns.
    let keep_root: std::collections::HashSet<&str> = plan.root_names().collect();
    crate::sweep_stale_top_level_entries(&nm, &keep_root, None);

    // Materialize every non-root node. Order doesn't matter for
    // correctness (each package's files are written into its own
    // directory) but we iterate by index so the BFS order surfaces
    // in progress/debug logs.
    for idx in 0..plan.nodes.len() {
        if idx == plan.root_idx {
            continue;
        }
        // Borrow scoping: take a clone of the fields we need out of
        // the node before calling methods that re-borrow `linker`
        // with `&mut stats`. The arena is read-only from here on.
        let (dep_path, pkg_dir) = {
            let node = &plan.nodes[idx];
            (
                node.dep_path.clone().expect("non-root node has dep_path"),
                node.pkg_dir.clone().expect("non-root node has pkg_dir"),
            )
        };
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };

        // `link:` deps: symlink the package dir straight at the
        // target. `link:` packages were excluded from the dependency
        // plan above because their target owns its deps. `portal:`
        // packages stay on the materialized-package path so their
        // graph-visible deps are linked like Yarn expects.
        // `rebase_local` in the resolver already normalized the
        // relative path to be importer-relative.
        if let Some(LocalSource::Link(rel)) = pkg.local_source.as_ref() {
            if let Some(parent) = pkg_dir.parent() {
                crate::mkdirp(parent)?;
            }
            crate::try_remove_entry(&pkg_dir);
            let abs_target = importer_dir.join(rel);
            let link_parent = pkg_dir.parent().unwrap_or(&nm);
            let rel_target = pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
            crate::sys::create_dir_link(&rel_target, &pkg_dir)
                .map_err(|e| Error::Io(pkg_dir.clone(), e))?;
            placements.record(&dep_path, pkg_dir);
            // Don't bump `top_level_linked` here: the post-loop
            // `children.len()` add below already counts every root
            // child including `link:` direct deps. Incrementing in
            // both places would double-count.
            continue;
        }

        // Registry (or `file:`) package — needs a PackageIndex to
        // find the store-backed file set. `package_indices` is sparse
        // on warm installs, so lazy-load from the store on miss.
        let owned_index;
        let index = match package_indices.get(&dep_path) {
            Some(i) => i,
            None => {
                // `registry_name()` is the lookup key for npm-aliased
                // packages (`"h3-v2": "npm:h3@..."`), which saved the
                // index under the real package name at fetch time.
                // Integrity is part of the cache key so a same-name
                // dep resolved from a non-registry source (git, remote
                // tarball, file:) can't pick up a registry-sourced
                // cache entry and get a different file list than its
                // own tarball actually contains.
                let loaded = linker
                    .store
                    .load_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())
                    .ok_or_else(|| Error::MissingPackageIndex(dep_path.clone()))?;
                owned_index = loaded;
                &owned_index
            }
        };

        // Wipe any previous contents at this path so a re-run after
        // changing versions doesn't leave stale files behind, then
        // batch-create every intermediate parent directory the index
        // will write into.
        crate::try_remove_entry(&pkg_dir);
        let mut parents: BTreeSet<PathBuf> = BTreeSet::new();
        parents.insert(pkg_dir.clone());
        // Validate every key once here. The file-linking loop below
        // walks the same immutable index, so skipping the check
        // there is safe.
        for rel_path in index.keys() {
            crate::validate_index_key(rel_path)?;
            let target = pkg_dir.join(rel_path);
            if let Some(parent) = target.parent() {
                parents.insert(parent.to_path_buf());
            }
        }
        for parent in &parents {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.clone(), e))?;
        }

        for (rel_path, stored) in index {
            // Key already validated in the parent-collection loop
            // above. The index is immutable between the two loops.
            let target = pkg_dir.join(rel_path);
            if let Err(e) = linker.link_file_fresh(stored, rel_path, &target) {
                if let Error::MissingStoreFile { .. } = &e {
                    crate::invalidate_stale_index_for_package(&linker.store, pkg);
                }
                return Err(e);
            }
            stats.files_linked += 1;
            if stored.executable {
                #[cfg(unix)]
                xx::file::make_executable(&target).map_err(|e| Error::Xx(e.to_string()))?;
            }
        }

        let patch_key = pkg.spec_key();
        if let Some(patch_text) = linker.patches.get(&patch_key) {
            apply_multi_file_patch(&pkg_dir, patch_text)
                .map_err(|msg| Error::Patch(patch_key.clone(), msg))?;
        }

        stats.packages_linked += 1;
        placements.record(&dep_path, pkg_dir);
    }

    stats.top_level_linked += plan.nodes[plan.root_idx].children.len();
    Ok(())
}
