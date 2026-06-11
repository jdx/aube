//! Content-addressed virtual store path computation.
//!
//! Ports pnpm's `calcGraphNodeHash` from `/tmp/pnpm/deps/graph-hasher/` —
//! the mechanism that lets pnpm's global virtual store safely share
//! built packages across projects. The core idea:
//!
//! 1. Each lockfile node gets a **dep-graph hash** derived from its own
//!    identity (the integrity hash / fullPkgId) plus the recursively
//!    hashed dep-graph subtree. Two projects whose resolution produces
//!    the same `(foo, [same children, same versions, same identities])`
//!    end up with the same hash, so they share a virtual-store entry.
//! 2. For packages that **transitively depend on anything allowed to
//!    run build scripts**, the hash also folds in an engine string
//!    (os/arch/node-version). Building a native module against node 20
//!    produces a different hash than building it against node 22, so
//!    the two artifacts live at different paths and never collide.
//! 3. Everything else (pure-JS packages whose subtree contains nothing
//!    that builds) has a hash of `engine=null` — stable across
//!    architectures, so pure-JS trees are still shared globally.
//!
//! Unlike pnpm, we use BLAKE3 over a canonical JSON serialization —
//! aube's virtual store is internal to aube (the CAS under
//! `$XDG_DATA_HOME/aube/store/v1/files` is ours alone), so we don't
//! need bit-for-bit compatibility with pnpm's `object-hash`.
//! Determinism is all that matters, and `serde_json` plus `BTreeMap`
//! gives us alphabetized keys for free. BLAKE3 is the project default
//! for non-crypto-verifying hashes (3-5x faster than SHA-256).

use crate::{LockedPackage, LockfileGraph};
use serde::Serialize;
use std::collections::BTreeMap;

use aube_util::collections::FxMap as FxHashMap;
use aube_util::collections::FxSet as FxHashSet;

/// A callback the caller provides to tell the hasher which
/// `(name, version)` combinations are allowed to run lifecycle
/// scripts. Implemented by `aube-scripts::BuildPolicy` in practice,
/// but the hasher stays oblivious to the policy crate so the lockfile
/// crate doesn't depend on it.
pub type AllowBuildFn<'a> = &'a dyn Fn(&LockedPackage) -> bool;

/// Engine fingerprint folded into a node's hash when any of its
/// transitive deps are allowed to build. Callers compute this once
/// per install; see [`engine_name_default`] for the standard format.
#[derive(Debug, Clone)]
pub struct EngineName(pub String);

/// `<os>-<arch>-node<major>` — e.g. `linux-x64-node20`. Enough to
/// distinguish builds across the axes that actually break native
/// modules. The arch string is translated from Rust's naming
/// (`x86_64`, `aarch64`) to Node's (`x64`, `arm64`) so the virtual
/// store directories look familiar next to `process.arch` output.
/// Libc detection is a known gap (TODO: musl vs glibc).
pub fn engine_name_default(node_version: &str) -> EngineName {
    let os = std::env::consts::OS;
    let arch = node_arch(std::env::consts::ARCH);
    let major = node_version
        .trim_start_matches('v')
        .split('.')
        .next()
        .unwrap_or("");
    EngineName(format!("{os}-{arch}-node{major}"))
}

/// Map Rust `std::env::consts::ARCH` values to Node's `process.arch`
/// convention. Unknown inputs pass through unchanged — better to leak
/// a Rust-flavored name into a debug path than to silently collapse
/// two distinct architectures onto the same bucket.
fn node_arch(rust_arch: &str) -> &str {
    match rust_arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "powerpc" => "ppc",
        other => other,
    }
}

/// Result of a full hashing pass over a `LockfileGraph`.
#[derive(Debug, Default, Clone)]
pub struct GraphHashes {
    /// Per-dep_path final hash used as the virtual-store subdir suffix.
    pub node_hash: BTreeMap<String, String>,
}

impl GraphHashes {
    /// Look up a hashed subdir name for `dep_path`, falling back to the
    /// raw dep_path when the hash is unknown. Callers threading this
    /// through the linker can use it as a drop-in for the bare
    /// dep_path when constructing virtual-store paths.
    pub fn hashed_dep_path(&self, dep_path: &str) -> String {
        match self.node_hash.get(dep_path) {
            Some(hex) => append_hex_to_leaf(dep_path, hex),
            None => dep_path.to_string(),
        }
    }
}

/// Append `-<hex>` to the final slash-separated component of `dep_path`.
/// For scoped packages like `@scope/name@ver` this preserves the scope
/// prefix and only decorates the leaf, so the existing 2-component
/// directory layout carries through unchanged except for a longer leaf
/// name.
fn append_hex_to_leaf(dep_path: &str, hex: &str) -> String {
    // 16 chars of sha256 hex = 64 bits, more than enough to avoid
    // collisions inside one project's lockfile (which typically has a
    // few thousand nodes at most). Using the full 64 would just make
    // paths awkward to stare at in `ls`.
    let short = &hex[..hex.len().min(16)];
    match dep_path.rfind('/') {
        Some(i) => format!("{}/{}-{}", &dep_path[..i], &dep_path[i + 1..], short),
        None => format!("{dep_path}-{short}"),
    }
}

/// Per-`(name, version)` patch fingerprint. Folded into `full_pkg_id`
/// so a patched node hashes differently from the unpatched one — and
/// because the recursive `calc_deps_hash` mixes child hashes into
/// every ancestor, every dep that transitively pulls in the patched
/// package also lands at a fresh virtual-store path.
pub type PatchHashFn<'a> = &'a dyn Fn(&str, &str) -> Option<String>;

/// Compute final hashes for every package in `graph`. When
/// `engine` is `Some`, packages whose transitive subtree contains a
/// build-allowed package fold the engine name into their hash; when
/// `None` or when no package in the subtree is allowed to build, the
/// hash is engine-agnostic.
pub fn compute_graph_hashes(
    graph: &LockfileGraph,
    allow_build: AllowBuildFn<'_>,
    engine: Option<&EngineName>,
) -> GraphHashes {
    compute_graph_hashes_with_patches(graph, allow_build, engine, &|_, _| None)
}

/// Variant of [`compute_graph_hashes`] that also folds per-package
/// patch fingerprints into the hash, so patched packages live at
/// distinct virtual-store paths.
pub fn compute_graph_hashes_with_patches(
    graph: &LockfileGraph,
    allow_build: AllowBuildFn<'_>,
    engine: Option<&EngineName>,
    patch_hash: PatchHashFn<'_>,
) -> GraphHashes {
    // Pass 1: identify every dep_path whose `(name, version)` is
    // allowed to run its scripts. This is the "builds" set.
    let mut builds: FxHashSet<String> = FxHashSet::default();
    for (dep_path, pkg) in &graph.packages {
        if allow_build(pkg) {
            builds.insert(dep_path.clone());
        }
    }

    // Pass 2: per-package dep-graph hash (recursive, memoized).
    let mut deps_hash_cache: FxHashMap<String, String> = FxHashMap::default();
    for dep_path in graph.packages.keys() {
        let _ = calc_deps_hash(
            graph,
            dep_path,
            &mut deps_hash_cache,
            &mut FxHashSet::default(),
            patch_hash,
        );
    }

    // Pass 3: per-package "does the subtree transitively need engine
    // tainting?" cache.
    let mut requires_build_cache: FxHashMap<String, bool> = FxHashMap::default();
    for dep_path in graph.packages.keys() {
        transitively_requires_build(
            graph,
            &builds,
            dep_path,
            &mut requires_build_cache,
            &mut FxHashSet::default(),
        );
    }

    // Pass 4: final `node_hash(engine?, deps)` per package.
    let mut node_hash: BTreeMap<String, String> = BTreeMap::new();
    for dep_path in graph.packages.keys() {
        let include_engine =
            engine.is_some() && *requires_build_cache.get(dep_path).unwrap_or(&false);
        let engine_str = if include_engine {
            Some(engine.unwrap().0.as_str())
        } else {
            None
        };
        let deps_hash = deps_hash_cache.get(dep_path).cloned().unwrap_or_default();
        let hex = hash_canonical(&NodeHashInput {
            engine: engine_str,
            deps: &deps_hash,
        });
        node_hash.insert(dep_path.clone(), hex);
    }

    GraphHashes { node_hash }
}

/// Compute the recursive dep-graph hash for one package. Uses the
/// node's `full_pkg_id` (its integrity when present, else a stringified
/// fallback) plus a sorted map of `child_alias -> child_deps_hash`.
///
/// Cycle-safe: packages already on the current DFS stack return an
/// empty string, matching pnpm's behavior (the hash loses a small bit
/// of information for cyclic peer-dep contexts, but it stays stable
/// and deterministic).
fn calc_deps_hash(
    graph: &LockfileGraph,
    dep_path: &str,
    cache: &mut FxHashMap<String, String>,
    parents: &mut FxHashSet<String>,
    patch_hash: PatchHashFn<'_>,
) -> String {
    if let Some(cached) = cache.get(dep_path) {
        return cached.clone();
    }
    if !parents.insert(dep_path.to_string()) {
        // Cycle: contribute an empty hash to break the recursion.
        // (Pnpm's version of this fans out from `fullPkgId` → `deps:{}`
        // when a node is already a parent; empty string here does the
        // same job via the canonical serializer.)
        return String::new();
    }

    let hash = match graph.packages.get(dep_path) {
        Some(pkg) => {
            let id = full_pkg_id(pkg, patch_hash);
            let mut deps: BTreeMap<String, String> = BTreeMap::new();
            for (alias, child_tail) in &pkg.dependencies {
                let child_dep_path = format!("{alias}@{child_tail}");
                // The child might not be in the graph if the lockfile
                // has a dangling reference (e.g. after manual edits);
                // skip rather than panic.
                if !graph.packages.contains_key(&child_dep_path) {
                    continue;
                }
                let child_hash = calc_deps_hash(graph, &child_dep_path, cache, parents, patch_hash);
                deps.insert(alias.clone(), child_hash);
            }
            hash_canonical(&DepsHashInput {
                id: &id,
                deps: &deps,
            })
        }
        None => String::new(),
    };

    parents.remove(dep_path);
    cache.insert(dep_path.to_string(), hash.clone());
    hash
}

/// Returns `true` if `dep_path` is allowed to build, or if any of its
/// transitive children are. Mirrors pnpm's `transitivelyRequiresBuild`.
fn transitively_requires_build(
    graph: &LockfileGraph,
    builds: &FxHashSet<String>,
    dep_path: &str,
    cache: &mut FxHashMap<String, bool>,
    parents: &mut FxHashSet<String>,
) -> bool {
    if let Some(&cached) = cache.get(dep_path) {
        return cached;
    }
    if builds.contains(dep_path) {
        cache.insert(dep_path.to_string(), true);
        return true;
    }
    if !parents.insert(dep_path.to_string()) {
        return false;
    }
    let result = match graph.packages.get(dep_path) {
        Some(pkg) => pkg.dependencies.iter().any(|(alias, tail)| {
            let child_dep_path = format!("{alias}@{tail}");
            transitively_requires_build(graph, builds, &child_dep_path, cache, parents)
        }),
        None => false,
    };
    parents.remove(dep_path);
    cache.insert(dep_path.to_string(), result);
    result
}

/// `full_pkg_id` — pnpm uses `${pkgIdWithPatchHash}:${resolution}`; we
/// use `${name}@${version}[:patch:<hex>]:${integrity}`. Packages
/// without integrity (workspace or git deps in pnpm's lockfile) fall
/// back to `<no-integrity>` which keeps the hash deterministic
/// without perfectly encoding identity — good enough until those
/// sources actually matter.
fn full_pkg_id(pkg: &LockedPackage, patch_hash: PatchHashFn<'_>) -> String {
    let integrity = pkg.integrity.as_deref().unwrap_or("<no-integrity>");
    match patch_hash(&pkg.name, &pkg.version) {
        Some(hex) => format!("{}@{}:patch:{hex}:{integrity}", pkg.name, pkg.version),
        None => format!("{}@{}:{}", pkg.name, pkg.version, integrity),
    }
}

/// BLAKE3 over a canonical JSON serialization. `serde_json` plus
/// `BTreeMap` gives alphabetized keys; primitives serialize
/// deterministically. Return the full hex digest so callers can pick
/// whatever prefix length they want.
fn hash_canonical<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_vec(value).expect("graph hash input must serialize");
    blake3::hash(&json).to_hex().to_string()
}

#[derive(Serialize)]
struct NodeHashInput<'a> {
    engine: Option<&'a str>,
    deps: &'a str,
}

#[derive(Serialize)]
struct DepsHashInput<'a> {
    id: &'a str,
    deps: &'a BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirectDep, LockedPackage, LockfileGraph};

    fn mk_pkg(name: &str, ver: &str, integrity: Option<&str>) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: ver.into(),
            integrity: integrity.map(str::to_string),
            dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            dep_path: format!("{name}@{ver}"),
            ..Default::default()
        }
    }

    fn empty_graph() -> LockfileGraph {
        let mut importers = BTreeMap::new();
        importers.insert(".".into(), Vec::<DirectDep>::new());
        LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn hash_is_deterministic_across_runs() {
        let mut g = empty_graph();
        g.packages.insert(
            "foo@1.0.0".into(),
            mk_pkg("foo", "1.0.0", Some("sha512-ABC")),
        );
        let h1 = compute_graph_hashes(&g, &|_| false, None);
        let h2 = compute_graph_hashes(&g, &|_| false, None);
        assert_eq!(h1.node_hash, h2.node_hash);
    }

    #[test]
    fn different_integrity_produces_different_hash() {
        let mut g1 = empty_graph();
        g1.packages
            .insert("foo@1.0.0".into(), mk_pkg("foo", "1.0.0", Some("sha512-A")));
        let mut g2 = empty_graph();
        g2.packages
            .insert("foo@1.0.0".into(), mk_pkg("foo", "1.0.0", Some("sha512-B")));
        let h1 = compute_graph_hashes(&g1, &|_| false, None);
        let h2 = compute_graph_hashes(&g2, &|_| false, None);
        assert_ne!(h1.node_hash["foo@1.0.0"], h2.node_hash["foo@1.0.0"]);
    }

    #[test]
    fn child_change_cascades_to_parent() {
        let mut g1 = empty_graph();
        g1.packages
            .insert("foo@1.0.0".into(), mk_pkg("foo", "1.0.0", Some("sha512-F")));
        let mut foo = mk_pkg("foo", "1.0.0", Some("sha512-F"));
        foo.dependencies.insert("bar".into(), "1.0.0".into());
        g1.packages.insert("foo@1.0.0".into(), foo);
        g1.packages.insert(
            "bar@1.0.0".into(),
            mk_pkg("bar", "1.0.0", Some("sha512-B1")),
        );

        let mut g2 = g1.clone();
        g2.packages.insert(
            "bar@1.0.0".into(),
            mk_pkg("bar", "1.0.0", Some("sha512-B2")),
        );

        let h1 = compute_graph_hashes(&g1, &|_| false, None);
        let h2 = compute_graph_hashes(&g2, &|_| false, None);
        assert_ne!(h1.node_hash["foo@1.0.0"], h2.node_hash["foo@1.0.0"]);
        assert_ne!(h1.node_hash["bar@1.0.0"], h2.node_hash["bar@1.0.0"]);
    }

    #[test]
    fn engine_only_affects_packages_transitively_requiring_build() {
        let mut g = empty_graph();
        g.packages.insert(
            "pure@1.0.0".into(),
            mk_pkg("pure", "1.0.0", Some("sha512-P")),
        );
        g.packages.insert(
            "native@1.0.0".into(),
            mk_pkg("native", "1.0.0", Some("sha512-N")),
        );
        let mut consumer = mk_pkg("consumer", "1.0.0", Some("sha512-C"));
        consumer
            .dependencies
            .insert("native".into(), "1.0.0".into());
        g.packages.insert("consumer@1.0.0".into(), consumer);

        let allow_native = |pkg: &LockedPackage| pkg.registry_name() == "native";
        let engine_a = EngineName("linux-x64-node20".into());
        let engine_b = EngineName("linux-x64-node22".into());

        let h_a = compute_graph_hashes(&g, &allow_native, Some(&engine_a));
        let h_b = compute_graph_hashes(&g, &allow_native, Some(&engine_b));

        // `native` builds → engine-sensitive → different per engine
        assert_ne!(h_a.node_hash["native@1.0.0"], h_b.node_hash["native@1.0.0"]);
        // `consumer` depends on native → engine-sensitive
        assert_ne!(
            h_a.node_hash["consumer@1.0.0"],
            h_b.node_hash["consumer@1.0.0"]
        );
        // `pure` has no build in its subtree → engine-agnostic → stable
        assert_eq!(h_a.node_hash["pure@1.0.0"], h_b.node_hash["pure@1.0.0"]);
    }

    #[test]
    fn cycles_do_not_panic() {
        let mut g = empty_graph();
        let mut a = mk_pkg("a", "1.0.0", Some("sha512-A"));
        a.dependencies.insert("b".into(), "1.0.0".into());
        let mut b = mk_pkg("b", "1.0.0", Some("sha512-B"));
        b.dependencies.insert("a".into(), "1.0.0".into());
        g.packages.insert("a@1.0.0".into(), a);
        g.packages.insert("b@1.0.0".into(), b);

        let h = compute_graph_hashes(&g, &|_| false, None);
        assert!(h.node_hash.contains_key("a@1.0.0"));
        assert!(h.node_hash.contains_key("b@1.0.0"));
    }

    #[test]
    fn hashed_dep_path_appends_to_leaf() {
        let mut h = GraphHashes::default();
        h.node_hash.insert("foo@1.0.0".into(), "a".repeat(64));
        assert!(h.hashed_dep_path("foo@1.0.0").starts_with("foo@1.0.0-aa"));
    }

    #[test]
    fn hashed_dep_path_preserves_scope() {
        let mut h = GraphHashes::default();
        h.node_hash.insert("@swc/core@1.3.0".into(), "b".repeat(64));
        let got = h.hashed_dep_path("@swc/core@1.3.0");
        assert!(got.starts_with("@swc/core@1.3.0-bb"), "got: {got}");
        // Scope prefix survives unchanged so the existing directory
        // layout (`virtual_store/@scope/<leaf>`) still resolves.
        assert!(got.starts_with("@swc/"));
    }

    #[test]
    fn hashed_dep_path_falls_back_to_raw_when_absent() {
        let h = GraphHashes::default();
        assert_eq!(h.hashed_dep_path("foo@1.0.0"), "foo@1.0.0");
    }

    #[test]
    fn engine_name_parses_node_version() {
        let e = engine_name_default("v20.10.0");
        assert!(e.0.ends_with("-node20"));
        let e = engine_name_default("22.0.0");
        assert!(e.0.ends_with("-node22"));
    }

    #[test]
    fn node_arch_maps_to_node_conventions() {
        assert_eq!(node_arch("x86_64"), "x64");
        assert_eq!(node_arch("aarch64"), "arm64");
        assert_eq!(node_arch("x86"), "ia32");
        // Unknown architectures pass through rather than getting
        // silently remapped onto an adjacent bucket.
        assert_eq!(node_arch("riscv64"), "riscv64");
    }
}
