mod driver;
mod fetch;
mod seed;
mod vulnerable;

use crate::local_source::is_non_registry_specifier;
use crate::semver_util::version_satisfies;
use crate::{
    Error, FxHashMap, PeerContextOptions, Resolver, apply_peer_contexts, catalog,
    hoist_auto_installed_peers,
};
use aube_lockfile::{DirectDep, LockedPackage, LockfileGraph};
use aube_manifest::PackageJson;
use std::collections::{BTreeMap, HashMap};

impl Resolver {
    /// Resolve all dependencies from a package.json.
    ///
    /// Uses batch-parallel BFS: each "wave" drains the queue, identifies
    /// uncached package names, fetches their packuments concurrently, then
    /// processes the entire batch before starting the next wave.
    pub async fn resolve(
        &mut self,
        manifest: &PackageJson,
        existing: Option<&LockfileGraph>,
    ) -> Result<LockfileGraph, Error> {
        self.resolve_workspace(
            &[(".".to_string(), manifest.clone())],
            existing,
            &HashMap::new(),
        )
        .await
    }

    /// Resolve all dependencies for a workspace (multiple importers).
    ///
    /// `manifests` is a list of (importer_path, PackageJson) — e.g. (".", root), ("packages/app", app).
    /// `workspace_packages` maps package name → version. Used both for
    /// explicit `workspace:` protocol resolution and for yarn/npm/bun
    /// style linkage where a bare semver range on a workspace-package
    /// name resolves to the local copy when its version satisfies the
    /// range.
    pub async fn resolve_workspace(
        &mut self,
        manifests: &[(String, PackageJson)],
        existing: Option<&LockfileGraph>,
        workspace_packages: &HashMap<String, String>,
    ) -> Result<LockfileGraph, Error> {
        driver::ResolveDriver::new(self, manifests, existing, workspace_packages)
            .run()
            .await
    }

    /// Is `(name, range)` safe to speculatively prefetch against the
    /// registry?
    ///
    /// Returns false for any spec that won't go through the registry
    /// resolver at all — workspace/catalog/npm-alias/jsr ranges, local
    /// (`file:`/`link:`/`git:`) specifiers, and bare ranges that match
    /// a workspace package. Also false for any name listed in
    /// `pnpm.overrides`, since the override may rewrite the spec into
    /// one of the above and we can't cheaply tell ahead of time.
    fn is_prefetchable(
        &self,
        name: &str,
        range: &str,
        workspace_packages: &HashMap<String, String>,
    ) -> bool {
        let workspace_hit = workspace_packages
            .get(name)
            .is_some_and(|ws_v| version_satisfies(ws_v, range));
        !aube_util::pkg::is_workspace_spec(range)
            && !aube_util::pkg::is_catalog_spec(range)
            && !aube_util::pkg::is_npm_spec(range)
            && !aube_util::pkg::is_jsr_spec(range)
            && !is_non_registry_specifier(range)
            && !self.overrides.contains_key(name)
            && !workspace_hit
    }

    /// Build the final `LockfileGraph` from accumulated resolver state.
    ///
    /// Runs the catalog-pick materialization, hoists auto-installed
    /// peers when `auto_install_peers` is on, and applies peer-context
    /// suffixes. Returns the post-peer-context graph ready for lockfile
    /// emission.
    fn finalize_resolved_graph(
        &self,
        importers: BTreeMap<String, Vec<DirectDep>>,
        resolved: BTreeMap<String, LockedPackage>,
        resolved_versions: &FxHashMap<String, Vec<String>>,
        resolved_times: BTreeMap<String, String>,
        skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>>,
        catalog_picks: BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<LockfileGraph, Error> {
        let resolved_catalogs =
            catalog::materialize_catalog_picks(catalog_picks, resolved_versions);

        let canonical = LockfileGraph {
            importers,
            packages: resolved,
            settings: aube_lockfile::LockfileSettings {
                auto_install_peers: self.auto_install_peers,
                exclude_links_from_lockfile: self.exclude_links_from_lockfile,
                // Tarball-URL recording is a lockfile-writer concern; the
                // resolver never populates URLs itself. Install flips this
                // on after the graph is built when the setting is active.
                lockfile_include_tarball_url: false,
            },
            // Stamp the resolver's overrides into the output graph so the
            // lockfile writer can round-trip them and the next install's
            // drift check can compare them against the manifest.
            overrides: self.overrides.clone(),
            ignored_optional_dependencies: self.ignored_optional_dependencies.clone(),
            times: resolved_times,
            skipped_optional_dependencies,
            catalogs: resolved_catalogs,
            // Resolver output is format-agnostic; the bun writer layer
            // defaults `configVersion` to 1 when emitting a fresh
            // lockfile.
            bun_config_version: None,
            // Fresh resolves don't carry over unknown blocks; the
            // install-side merge (`overlay_metadata_from`) copies
            // them back from the prior lockfile when round-tripping.
            patched_dependencies: BTreeMap::new(),
            trusted_dependencies: Vec::new(),
            extra_fields: BTreeMap::new(),
            workspace_extra_fields: BTreeMap::new(),
        };

        // Second pass: hoist every auto-installed peer to its importer's
        // direct deps so pnpm-style `node_modules/<peer>` top-level
        // symlinks get created and the lockfile's `importers.` section
        // lists them the way pnpm does with `auto-install-peers=true`.
        // Skipped entirely when the setting is off — matches pnpm, which
        // leaves the importer's `dependencies` untouched in that mode.
        let hoisted = if self.auto_install_peers {
            hoist_auto_installed_peers(canonical)
        } else {
            canonical
        };

        // Third pass: compute peer-context suffixes for every reachable
        // package. See `apply_peer_contexts` for the details.
        let peer_options = PeerContextOptions {
            dedupe_peer_dependents: self.dedupe_peer_dependents,
            dedupe_peers: self.dedupe_peers,
            resolve_from_workspace_root: self.resolve_peers_from_workspace_root,
            peers_suffix_max_length: self.peers_suffix_max_length,
        };
        let _diag_peer =
            aube_util::diag::Span::new(aube_util::diag::Category::Resolver, "peer_context_apply");
        let contextualized = apply_peer_contexts(hoisted, &peer_options)?;
        drop(_diag_peer);
        tracing::debug!(
            "peer-context pass produced {} contextualized packages",
            contextualized.packages.len()
        );
        Ok(contextualized)
    }
}
