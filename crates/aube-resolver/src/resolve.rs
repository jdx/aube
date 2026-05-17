mod fetch;
mod seed;
mod vulnerable;

use crate::local_source::{
    dep_path_for, is_non_registry_specifier, read_local_manifest, rebase_local,
    resolve_exec_manifest, resolve_git_source, resolve_remote_tarball, should_block_exotic_subdep,
};
use crate::package_ext::{apply_package_extensions, pick_override_spec};
use crate::semver_util::{PickResult, pick_version, version_satisfies};
use crate::{
    Error, ExoticSubdepDetails, PeerContextOptions, ResolutionMode, ResolveTask, ResolvedPackage,
    Resolver, apply_peer_contexts, catalog, error, hoist_auto_installed_peers,
    is_deprecation_allowed, is_supported,
};
use crate::{FxHashMap, FxHashSet};
use aube_lockfile::{DepType, DirectDep, LocalSource, LockedPackage, LockfileGraph};
use aube_manifest::PackageJson;
use aube_registry::Packument;
use fetch::{FetchInputs, fetch_one_packument};
use seed::seed_direct_deps;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use vulnerable::{is_vulnerable, prefer_non_vulnerable_pick};

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
        let resolve_start = std::time::Instant::now();
        let mut packument_fetch_count = 0u32;
        let mut packument_fetch_time = std::time::Duration::ZERO;
        let mut lockfile_reuse_count = 0u32;
        let mut resolved: BTreeMap<String, LockedPackage> = BTreeMap::new();
        // 1024 covers typical monorepo. 5000-dep graphs take one grow.
        let mut resolved_versions: FxHashMap<String, Vec<String>> =
            FxHashMap::with_capacity_and_hasher(1024, Default::default());
        let mut importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
        let mut queue: VecDeque<ResolveTask> = VecDeque::with_capacity(512);
        let mut visited: FxHashSet<std::sync::Arc<str>> =
            FxHashSet::with_capacity_and_hasher(2048, Default::default());
        // Round-tripped to the lockfile's top-level `time:` block so
        // subsequent installs can reuse them for the cutoff computation.
        // Populated opportunistically from whatever packuments we fetch:
        // empty when the metadata omits `time` (corgi from npmjs.org in
        // default mode), filled when it doesn't (Verdaccio, or the
        // full-packument path taken for time-based resolution and
        // `minimumReleaseAge`). This matches pnpm's `publishedAt` wiring.
        let mut resolved_times: BTreeMap<String, String> = BTreeMap::new();
        // Per-importer record of optionals the resolver intentionally
        // dropped on this run — either filtered by os/cpu/libc or
        // named in `pnpm.ignoredOptionalDependencies`. Round-tripped
        // through the lockfile so drift detection on subsequent
        // installs can distinguish "previously skipped" from "newly
        // added by the user".
        let mut skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>> =
            BTreeMap::new();
        // Catalog picks gathered as the BFS rewrites `catalog:` task
        // ranges. Outer key: catalog name. Inner: package name → spec.
        // Resolved versions are filled in post-resolution by walking
        // `resolved_versions` for the spec, since the picked version is
        // an output the BFS doesn't know until version_satisfies fires.
        let mut catalog_picks: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let importer_declared_dep_names: BTreeMap<String, BTreeSet<String>> = manifests
            .iter()
            .map(|(importer_path, manifest)| {
                let names = manifest
                    .dependencies
                    .keys()
                    .chain(manifest.dev_dependencies.keys())
                    .chain(manifest.optional_dependencies.keys())
                    .cloned()
                    .collect();
                (importer_path.clone(), names)
            })
            .collect();
        // ISO-8601 UTC cutoff string. npm's registry `time` map uses
        // `Z`-suffixed UTC timestamps throughout, which sort
        // lexicographically — so a raw `String` doubles as a
        // comparable instant without pulling in a date library.
        //
        // Two independent features feed this cutoff:
        //   - `minimum_release_age` (pnpm v11 default, supply-chain
        //     mitigation): seeded *before* wave 0 so even direct deps
        //     are filtered. The exclude list and strict-mode behavior
        //     are scoped per-package by `pick_version` below.
        //   - `resolution-mode=time-based`: derived from the max
        //     publish time across direct deps once wave 0 finishes,
        //     then constrains transitives only.
        // When both are configured, the resolver carries both cutoffs
        // and the picker takes the more restrictive (earlier) one.
        let mut published_by: Option<String> =
            self.minimum_release_age.as_ref().and_then(|m| m.cutoff());
        if let Some(c) = published_by.as_deref() {
            tracing::debug!("minimumReleaseAge cutoff: {}", c);
        }

        seed_direct_deps(
            manifests,
            &self.ignored_optional_dependencies,
            &mut queue,
            &mut importers,
        );

        // Pipelined resolver state. The resolver is strictly serial in
        // its *processing* order (tasks are popped and version-picked
        // in seed/BFS order, which is what keeps the output lockfile
        // byte-deterministic across runs) but fetches run freely in
        // the background via `in_flight`. When a popped task's
        // packument isn't in the cache, the main loop waits inline on
        // `in_flight.join_next()` — harvesting whatever other fetches
        // happen to land in the meantime — until this task's
        // packument is available. Because `ensure_fetch!` is called
        // speculatively at every enqueue site, by the time a task is
        // popped its packument is usually already cached, so the
        // wait is short.
        /*
         * Adaptive packument concurrency. Loaded from the cross run
         * persistent store when available so the limiter resumes
         * the converged operating point of the previous run instead
         * of cold ramping. Falls back to seed 256 (h2 stream cap)
         * on a fresh install. The CUSUM gated AIMD controller in
         * `aube_util::adaptive` shrinks on real back pressure
         * (HTTP 429 / 503 / timeout) and on sustained latency
         * regime rise. Floor 4 keeps progress under continuous
         * throttling.
         */
        // User-configured `networkConcurrency` (or `env_concurrency`)
        // is honored as the seed: it's the operating cap they
        // explicitly chose for their environment (constrained CI
        // runner, private registry rate-limit, fat residential
        // pipe). Adaptive shrink/grow still kicks in around it.
        // Floor stays at 4 so even an over-aggressive user value
        // can't deadlock progress on continuous throttling.
        let packument_seed = self.packument_network_concurrency.unwrap_or(256).max(4);
        let packument_max = packument_seed.max(256);
        let persistent = aube_util::adaptive::global_persistent_state();
        let shared_semaphore = match persistent.as_ref() {
            Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
                state,
                "packument:default",
                packument_seed,
                4,
                packument_max,
            ),
            None => aube_util::adaptive::AdaptiveLimit::new(packument_seed, 4, packument_max),
        };
        let packument_persist_handle = persistent
            .as_ref()
            .map(|p| (Arc::clone(p), Arc::clone(&shared_semaphore)));
        // Time-based mode and `minimumReleaseAge` both need the
        // packument's `time:` map. The abbreviated (corgi) response
        // omits `time` by default, so we normally fall back to the
        // full packument. `registry-supports-time-field=true` flips
        // that: the user is asserting the configured registry ships
        // `time` in corgi too (Verdaccio 5.15.1+, JSR, etc.), so the
        // cheaper abbreviated path stays on the hot path and we save
        // one full-packument fetch per distinct package.
        let needs_time = (self.resolution_mode == ResolutionMode::TimeBased
            || self.minimum_release_age.is_some()
            || self.dependency_policy.trust_policy == crate::TrustPolicy::NoDowngrade)
            && !self.registry_supports_time_field;
        // When time data is required, fetch the full packument directly.
        // The previous corgi-first shortcut saved bytes for old packages
        // but cost an extra round trip for active packages whose top-level
        // `modified` timestamp was newer than the cutoff. Clean installs of
        // modern dependency graphs are dominated by those active packages.

        // In-flight packument fetches. The spawned task returns the
        // `(name, packument, from_primer)` tuple so `join_next` gives
        // us back the identity of whichever fetch landed next without
        // a side table lookup. `from_primer` matters because the
        // bundled primer intentionally keeps only a capped slice of
        // high-traffic package histories; a range miss against that
        // slice must fall through to the live registry before we
        // report `ERR_AUBE_NO_MATCHING_VERSION`.
        #[allow(clippy::type_complexity)]
        let mut in_flight: tokio::task::JoinSet<Result<(String, Packument, bool), Error>> =
            tokio::task::JoinSet::new();
        // Names whose fetch has been spawned but not yet harvested.
        // Dedupes spawn calls when multiple tasks discover the same
        // transitive before any of them has been processed.
        let mut in_flight_names: FxHashSet<String> = FxHashSet::default();
        let mut primer_seeded_names: FxHashSet<String> = FxHashSet::default();
        // TimeBased wave-0 gate: the publish-time cutoff is derived
        // from the direct deps' resolved versions, so transitives
        // that reach the version-pick step before all directs have
        // completed must wait. Populated only when
        // `cutoff_pending == true` (TimeBased mode); `Highest` mode
        // leaves these at their defaults and the gate is a no-op.
        let mut direct_deps_pending: usize = queue.len();
        let mut cutoff_pending = self.resolution_mode == ResolutionMode::TimeBased;
        let mut deferred_transitives: Vec<ResolveTask> = Vec::new();

        // Set of names present in the existing lockfile. Used as a
        // prefetch gate: names the lockfile already covers will hit
        // the lockfile-reuse path and don't need their packuments
        // fetched, so prefetching them is wasted tokio-spawn
        // overhead. Load-bearing for `aube add` and
        // frozen-lockfile-install scenarios where most tasks go
        // through lockfile-reuse.
        //
        // This is strictly a *prefetch* gate, not a correctness
        // gate: a task that fails sibling dedupe AND lockfile reuse
        // (because its range doesn't match any of the lockfile's
        // versions for that name) still needs a fresh fetch, and
        // the wait-for-fetch loop below calls `ensure_fetch!`
        // without consulting `existing_names`.
        // Borrow names from `existing` instead of cloning. The set
        // lives only inside `Resolver::resolve` and the prior
        // lockfile graph outlives it. Skips 5000 String allocations
        // on a 5000-pkg lockfile at resolve-entry.
        let existing_names: FxHashSet<&str> = existing
            .map(|g| g.packages.values().map(|p| p.name.as_str()).collect())
            .unwrap_or_default();

        // Spawn a packument fetch into `in_flight` if one isn't
        // already running for `name` and the packument isn't
        // already cached. Gated *only* on in-flight + cache —
        // callers that want to skip prefetching names already
        // covered by the lockfile check `existing_names` explicitly
        // before invoking the macro.
        macro_rules! ensure_fetch {
            ($name:expr) => {{
                let name: &str = $name;
                if !in_flight_names.contains(name) && !self.cache.contains_key(name) {
                    in_flight_names.insert(name.to_string());
                    let mra_excludes_name = self
                        .minimum_release_age
                        .as_ref()
                        .is_some_and(|mra| mra.exclude.contains(name));
                    let primer_covers_cutoff = mra_excludes_name
                        || published_by
                            .as_deref()
                            .is_none_or(crate::primer::covers_cutoff);
                    in_flight.spawn(fetch_one_packument(FetchInputs {
                        name: name.to_string(),
                        client: self.client.clone(),
                        cache_dir: self.packument_cache_dir.clone(),
                        full_cache_dir: self.packument_full_cache_dir.clone(),
                        primer_covers_cutoff,
                        force_metadata_primer: self.force_metadata_primer,
                        sem: shared_semaphore.clone(),
                        needs_time,
                    }));
                }
            }};
        }

        // Decrement the pending-directs counter when a root task
        // reaches a terminal state. Used by the TimeBased cutoff
        // trigger at the top of the outer loop.
        macro_rules! note_root_done {
            () => {
                if direct_deps_pending > 0 {
                    direct_deps_pending -= 1;
                }
            };
        }

        // `(name, range)` is safe to speculatively prefetch against
        // the registry when:
        //
        //   - The range isn't a protocol we rewrite in preprocessing
        //     (`workspace:` / `catalog:` / `npm:` alias) — for those
        //     we don't know the real package name yet, so fetching
        //     the raw task name is either useless (preprocessing
        //     won't go through the registry at all) or wrong (we'd
        //     fetch the alias key instead of the real package).
        //   - The range isn't a `file:` / `link:` / `git:` /
        //     remote-tarball spec (covered by
        //     `is_non_registry_specifier`).
        //   - The name isn't in the overrides map — an override can
        //     rewrite the range into any of the above, and we can't
        //     cheaply tell whether it will, so be conservative.
        //
        // Called both from the upfront prefetch loop over seeded
        // root deps *and* from the three transitive-enqueue sites
        // inside the version-pick body, where the same class of
        // unsafe specs can arrive via a published package's
        // `dependencies` / `optionalDependencies` / `peerDependencies`
        // maps (real-world case: a package whose dependency entry
        // is an npm alias).
        // Fire prefetches for every seeded root dep up front, so
        // their packuments are already in flight by the time the
        // first task is popped. Skip lockfile-covered names —
        // they'll hit the lockfile-reuse path and never need their
        // packuments — and anything `is_prefetchable` rejects.
        for task in queue.iter() {
            if !self.is_prefetchable(task.name.as_str(), task.range.as_str(), workspace_packages) {
                continue;
            }
            if existing_names.contains(task.name.as_str()) {
                continue;
            }
            ensure_fetch!(&task.name);
        }

        'outer: loop {
            // TimeBased cutoff trigger. Fires the first time
            // `direct_deps_pending` hits zero with the cutoff still
            // pending — at which point every direct dep has been
            // version-picked (or terminated in preprocessing),
            // `resolved_times` holds their publish times, and we can
            // derive the max to seed `published_by` for the
            // transitives we deferred.
            if cutoff_pending && direct_deps_pending == 0 {
                let direct_dep_paths: FxHashSet<&String> = importers
                    .values()
                    .flat_map(|deps| deps.iter().map(|d| &d.dep_path))
                    .collect();
                let mut max_time: Option<&String> = None;
                for (dep_path, t) in resolved_times.iter() {
                    if !direct_dep_paths.contains(dep_path) {
                        continue;
                    }
                    if max_time.map(|m| t > m).unwrap_or(true) {
                        max_time = Some(t);
                    }
                }
                if let Some(existing_graph) = existing {
                    for (dep_path, t) in &existing_graph.times {
                        if !direct_dep_paths.contains(dep_path) {
                            continue;
                        }
                        if max_time.map(|m| t > m).unwrap_or(true) {
                            max_time = Some(t);
                        }
                    }
                }
                if let Some(m) = max_time {
                    tracing::debug!("time-based resolution cutoff: {}", m);
                    published_by = Some(match published_by.take() {
                        Some(existing) if existing.as_str() < m.as_str() => existing,
                        _ => m.clone(),
                    });
                }
                cutoff_pending = false;
                queue.extend(deferred_transitives.drain(..));
            }

            let Some(mut task) = queue.pop_front() else {
                if !deferred_transitives.is_empty() {
                    return Err(Error::Registry(
                        "(resolver)".to_string(),
                        format!(
                            "{} transitives still deferred when resolve completed",
                            deferred_transitives.len()
                        ),
                    ));
                }
                break 'outer;
            };

            // Body of the former per-task preprocessing loop.
            // The old wave-based code split this into a
            // preprocessing pass and a post-fetch version-pick
            // pass with a fetch barrier between them. Here both
            // passes run inline for a single task: preprocess →
            // sibling dedupe → lockfile reuse → wait on this
            // task's packument → version-pick → enqueue
            // transitives. The bare block keeps the original
            // indentation so the diff stays readable against the
            // prior shape; `continue` inside it still continues
            // the 'outer loop because a bare block is not itself
            // a loop.
            {
                // Apply bare-name overrides + npm-alias rewrites in a
                // small fixed-point loop. Two interleavings need to
                // work simultaneously:
                //   1. The override *value* is itself a `npm:` alias
                //      (e.g. `"foo": "npm:bar@^2"`). The first override
                //      pass rewrites `task.range`; the alias pass then
                //      rewrites `task.name` to `bar`.
                //   2. The user's *declared dep* is an `npm:` alias
                //      (e.g. `"foo": "npm:bar@^1"`) and the override
                //      targets the real package (`"overrides":
                //      {"bar": "2.0.0"}`). The first override pass
                //      misses (`task.name` is still `foo`), the alias
                //      pass rewrites `task.name = "bar"`, and the
                //      second override pass catches it.
                // A two-iteration cap is enough — after one alias
                // rewrite the name is canonical, and an override that
                // points at a third package is itself constrained by
                // the same rule, so there's no infinite chain.
                //
                // We deliberately don't touch `original_specifier`,
                // since the lockfile/importer record should still
                // reflect what the user wrote in package.json —
                // overrides are a graph-shaping rule, not a rewrite of
                // the user's declared deps.
                // Catalog protocol: rewrite `catalog:` and
                // `catalog:<name>` to the workspace catalog's actual
                // range *before* the override loop, so overrides can
                // still target a catalog dep by bare name. The original
                // `catalog:...` text stays in `original_specifier` so
                // the lockfile importer keeps the catalog reference and
                // drift detection works.
                if let Some((catalog_name, real_range)) =
                    self.resolve_catalog_spec(&task.name, &task.range)?
                {
                    tracing::trace!("catalog: {} {} -> {}", task.name, task.range, real_range);
                    catalog_picks
                        .entry(catalog_name)
                        .or_default()
                        .insert(task.name.clone(), real_range.clone());
                    task.range = real_range;
                }

                for _ in 0..2 {
                    let mut changed = false;
                    if let Some(override_spec) = pick_override_spec(
                        &self.override_rules,
                        &task.name,
                        &task.range,
                        &task.ancestors,
                    ) {
                        // pnpm's removal marker: an override value of
                        // `"-"` drops the dep edge entirely. Skip before
                        // catalog/alias rewrites so `-` never reaches
                        // the registry resolver. The dropped edge never
                        // gets written to the parent's `.dependencies`
                        // map (that write happens downstream) and, for
                        // direct deps, never gets pushed into the
                        // importer's direct-dep list.
                        if override_spec == "-" {
                            tracing::trace!("override: {}@{} -> dropped", task.name, task.range,);
                            if task.is_root {
                                note_root_done!();
                            }
                            continue 'outer;
                        }
                        // An override may itself point at a catalog
                        // entry (e.g. `"overrides": {"foo": "catalog:"}`).
                        // The catalog pre-pass above already ran against
                        // the original range, so resolve the indirection
                        // here before assigning — otherwise `catalog:`
                        // leaks through to the registry resolver.
                        // Stash the catalog pick in a local so we only
                        // record it if the override actually moves
                        // `task.range`.
                        let (effective_spec, pending_pick) =
                            match self.resolve_catalog_spec(&task.name, &override_spec)? {
                                Some((catalog_name, real_range)) => {
                                    (real_range.clone(), Some((catalog_name, real_range)))
                                }
                                None => (override_spec, None),
                            };
                        if task.range != effective_spec {
                            if let Some((catalog_name, real_range)) = pending_pick {
                                catalog_picks
                                    .entry(catalog_name)
                                    .or_default()
                                    .insert(task.name.clone(), real_range);
                            }
                            tracing::trace!(
                                "override: {}@{} -> {}",
                                task.name,
                                task.range,
                                effective_spec
                            );
                            // Overrides are declared at the project root,
                            // so a substituted `link:./libs/x` /
                            // `file:./vendor/y` path is project-root-
                            // relative — never importer- or parent-
                            // relative. Mark the task so the local-source
                            // branch anchors the path correctly even when
                            // the consumer is a workspace pkg or a nested
                            // local parent.
                            if is_non_registry_specifier(&effective_spec) {
                                task.range_from_override = true;
                            }
                            task.range = effective_spec;
                            // If the override replaced the spec with a
                            // bare range (not itself an `npm:` / `jsr:`
                            // alias), it's targeting `task.name` —
                            // implicitly undoing any prior alias
                            // rewrite. Without this, an override that
                            // fires after a catalog-aliased entry
                            // (e.g. catalog `js-yaml:
                            // npm:@zkochan/js-yaml@0.0.11`, override
                            // `js-yaml@<3.14.2: ^3.14.2`) would keep
                            // `task.real_name = @zkochan/js-yaml` and
                            // try to fetch `^3.14.2` from a packument
                            // that only carries `0.0.x`. If the
                            // override's value is itself an alias, the
                            // alias pass below picks up the new target
                            // on the next loop iteration.
                            if task.real_name.is_some()
                                && !task.range.starts_with("npm:")
                                && !task.range.starts_with("jsr:")
                            {
                                task.real_name = None;
                            }
                            changed = true;
                        }
                    }
                    if let Some(rest) = task.range.strip_prefix("npm:")
                        && let Some(at_idx) = rest.rfind('@')
                    {
                        let real_name = rest[..at_idx].to_string();
                        let real_range = rest[at_idx + 1..].to_string();
                        // Keep `task.name` as the user-facing alias
                        // (the key the package.json used) and stash
                        // the registry name on `real_name` so every
                        // identity-facing site — dep_path formation,
                        // direct-dep records, parent wiring — sees
                        // the alias, while only packument/tarball
                        // fetch sites (via `task.registry_name()`)
                        // hit the real package. Overwriting
                        // `task.name` here would collapse
                        // `node_modules/h3-v2/` to `node_modules/h3/`
                        // and any `require("h3-v2")` would break.
                        if task.real_name.as_deref() != Some(real_name.as_str())
                            || real_range != task.range
                        {
                            tracing::trace!(
                                "npm alias: {} -> {}@{}",
                                task.name,
                                real_name,
                                real_range
                            );
                            task.real_name = Some(real_name);
                            task.range = real_range;
                            changed = true;
                        }
                    }
                    // `jsr:<range>` and `jsr:<@scope/name>[@<range>]` both
                    // land here. JSR's npm-compat endpoint serves every
                    // package under `@jsr/<scope>__<name>`, but the
                    // user-facing dependency name stays the JSR name (or
                    // explicit alias) from package.json. Keep `task.name`
                    // unchanged for dep_path/importer/link identity and
                    // stash the npm-compat name in `real_name`, matching
                    // the npm-alias path above. Only registry IO should
                    // see `@jsr/...`.
                    if let Some(rest) = task.range.strip_prefix("jsr:") {
                        let (jsr_name_raw, jsr_range) = if let Some(body) = rest.strip_prefix('@') {
                            match body.rfind('@') {
                                Some(rel_at) => {
                                    // Indices are relative to `body`; add 1 for
                                    // the `@` we just stripped so we can slice
                                    // against the original `rest`.
                                    let at_idx = rel_at + 1;
                                    (rest[..at_idx].to_string(), rest[at_idx + 1..].to_string())
                                }
                                None => (rest.to_string(), "latest".to_string()),
                            }
                        } else {
                            // Bare range form — the manifest key carries the
                            // JSR name (e.g. `"@std/collections": "jsr:^1"`).
                            (task.name.clone(), rest.to_string())
                        };
                        match aube_registry::jsr::jsr_to_npm_name(&jsr_name_raw) {
                            Some(npm_name) => {
                                if task.real_name.as_deref() != Some(npm_name.as_str())
                                    || jsr_range != task.range
                                {
                                    tracing::trace!(
                                        "jsr: {} -> {}@{}",
                                        task.name,
                                        npm_name,
                                        jsr_range,
                                    );
                                    task.real_name = Some(npm_name);
                                    task.range = jsr_range;
                                    changed = true;
                                }
                            }
                            None => {
                                return Err(Error::Registry(
                                    task.name.clone(),
                                    format!(
                                        "invalid jsr: spec `{}` — expected `jsr:@scope/name[@range]`",
                                        task.range,
                                    ),
                                ));
                            }
                        }
                    }
                    if !changed {
                        break;
                    }
                }

                // Handle file: / link: / git: protocols — the dep points
                // at a path on disk or a remote git repo rather than a
                // registry package. Root deps anchor on the importer's
                // directory; transitive `link:`/`file:` deps anchor on
                // the parent package's source root, but only when the
                // parent itself was a `file:`/`link:` source (a workspace
                // sibling or a directly-linked local dir). Registry-
                // hosted parents have no on-disk source to resolve a
                // relative path against, so transitive `link:`/`file:`
                // from them stays an error.
                if is_non_registry_specifier(&task.range) {
                    // Root-declared `pnpm.overrides` opts the user into
                    // the rewritten `link:`/`file:` target by name, so
                    // they bypass the exotic-subdep block — otherwise
                    // an override aimed at a transitive of a registry
                    // package would always lose to the default-on
                    // guard.
                    if !task.range_from_override
                        && should_block_exotic_subdep(
                            &task,
                            &resolved,
                            self.dependency_policy.block_exotic_subdeps,
                        )
                    {
                        return Err(Error::BlockedExoticSubdep(Box::new(ExoticSubdepDetails {
                            name: task.name.clone(),
                            spec: task.range.clone(),
                            parent: task
                                .parent
                                .clone()
                                .unwrap_or_else(|| "<unknown>".to_string()),
                            ancestors: task.ancestors.clone(),
                            importer: task.importer.clone(),
                        })));
                    }
                    // Pull the parent's on-disk package root, when the
                    // parent is a directory-backed source. `exec:` stores
                    // the generator script path, not the generated package
                    // directory, so it cannot safely anchor relative
                    // transitive local specifiers.
                    let parent_source_root: Option<std::path::PathBuf> = (!task.is_root)
                        .then(|| {
                            task.parent
                                .as_ref()
                                .and_then(|dp| resolved.get(dp))
                                .and_then(|pkg| pkg.local_source.as_ref())
                                .and_then(|src| match src {
                                    LocalSource::Directory(p)
                                    | LocalSource::Link(p)
                                    | LocalSource::Portal(p) => Some(self.project_root.join(p)),
                                    _ => None,
                                })
                        })
                        .flatten();
                    // Override-substituted link:/file: paths are
                    // project-root-relative regardless of where the
                    // consumer lives — pin them at the root before any
                    // importer/parent fallback wins.
                    let importer_root = if task.range_from_override {
                        self.project_root.clone()
                    } else {
                        parent_source_root.clone().unwrap_or_else(|| {
                            if task.importer == "." {
                                self.project_root.clone()
                            } else {
                                self.project_root.join(&task.importer)
                            }
                        })
                    };
                    let Some(raw_local) = LocalSource::parse(&task.range, &importer_root) else {
                        return Err(Error::Registry(
                            task.name.clone(),
                            format!("unparseable local specifier: {}", task.range),
                        ));
                    };
                    // Git and remote-tarball specifiers don't reference
                    // a path, so they pass through regardless of parent
                    // shape. `link:`/`file:` transitives only resolve
                    // when we either (a) located a parent source root
                    // or (b) inherited the path from a project-root-
                    // anchored override.
                    if !task.is_root
                        && parent_source_root.is_none()
                        && !task.range_from_override
                        && matches!(
                            raw_local,
                            LocalSource::Directory(_)
                                | LocalSource::Tarball(_)
                                | LocalSource::Link(_)
                                | LocalSource::Portal(_)
                                | LocalSource::Exec(_)
                        )
                    {
                        return Err(Error::Registry(
                            task.name.clone(),
                            format!(
                                "transitive local specifier {} cannot be resolved without the parent package source root",
                                task.range
                            ),
                        ));
                    }
                    let (local, real_version, target_deps) = if let LocalSource::Git(ref g) =
                        raw_local
                    {
                        let shallow = aube_store::git_host_in_list(&g.url, &self.git_shallow_hosts);
                        let (resolved_local, version, deps) =
                            resolve_git_source(&task.name, g, shallow, Some(self.client.as_ref()))
                                .await
                                .map_err(|e| {
                                    Error::Registry(
                                        task.name.clone(),
                                        format!("git resolve {}: {e}", task.range),
                                    )
                                })?;
                        (resolved_local, version, deps)
                    } else if let LocalSource::RemoteTarball(ref t) = raw_local {
                        let (resolved_local, version, deps) =
                            resolve_remote_tarball(&task.name, t, self.client.as_ref())
                                .await
                                .map_err(|e| {
                                    Error::Registry(
                                        task.name.clone(),
                                        format!("remote tarball {}: {e}", task.range),
                                    )
                                })?;
                        (resolved_local, version, deps)
                    } else {
                        // Rewrite the path to be relative to the
                        // project root so every downstream consumer
                        // can resolve it with a single
                        // `project_root.join(rel)`.
                        let local = rebase_local(&raw_local, &importer_root, &self.project_root);
                        let (version, deps) = if matches!(local, LocalSource::Exec(_)) {
                            if self.ignore_scripts {
                                return Err(Error::Registry(
                                    task.name.clone(),
                                    format!(
                                        "{} requires executing its generator, but scripts are disabled",
                                        local.specifier()
                                    ),
                                ));
                            }
                            resolve_exec_manifest(&task.name, &local, &self.project_root).await?
                        } else {
                            let (_target_name, version, deps) =
                                read_local_manifest(&raw_local, &importer_root).unwrap_or_else(
                                    |_| (task.name.clone(), "0.0.0".to_string(), BTreeMap::new()),
                                );
                            (version, deps)
                        };
                        (local, version, deps)
                    };
                    let dep_path = local.dep_path(&task.name);
                    let linked_name = task.name.clone();

                    if task.is_root
                        && let Some(deps) = importers.get_mut(&task.importer)
                    {
                        deps.push(DirectDep {
                            name: task.name.clone(),
                            dep_path: dep_path.clone(),
                            dep_type: task.dep_type,
                            specifier: task.original_specifier.clone(),
                        });
                    }

                    // Wire parent -> this exotic transitive. Without
                    // this, the parent snapshot's `dependencies` map
                    // omits the git/url/file subdep entirely, so the
                    // linker never creates the sibling symlink inside
                    // the parent's node_modules and the package fails
                    // to resolve at runtime. The value is the dep_path
                    // tail (e.g. `git+<hash>`) so the linker can
                    // reconstruct the full dep_path by concatenating
                    // `{name}@{value}` — matching the key format used
                    // when inserting the resolved package below.
                    if let Some(ref parent_dp) = task.parent
                        && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                    {
                        // `local.dep_path(name)` always returns
                        // `{name}@{tail}`; if that invariant ever
                        // breaks we'd silently store a malformed dep
                        // value that the pnpm writer would emit as-is.
                        let name_prefix = format!("{}@", task.name);
                        debug_assert!(
                            dep_path.starts_with(&name_prefix),
                            "local.dep_path returned {dep_path:?} without expected prefix {name_prefix:?}"
                        );
                        let dep_tail = dep_path
                            .strip_prefix(&name_prefix)
                            .unwrap_or(&dep_path)
                            .to_string();
                        parent_pkg
                            .dependencies
                            .insert(task.name.clone(), dep_tail.clone());
                        if task.dep_type == DepType::Optional {
                            parent_pkg
                                .optional_dependencies
                                .insert(task.name.clone(), dep_tail);
                        }
                    }

                    if visited.insert(std::sync::Arc::from(dep_path.as_str())) {
                        resolved.insert(
                            dep_path.clone(),
                            LockedPackage {
                                name: linked_name.clone(),
                                version: real_version.clone(),
                                dep_path: dep_path.clone(),
                                local_source: Some(local.clone()),
                                ..Default::default()
                            },
                        );
                        if let Some(ref tx) = self.resolved_tx {
                            let pending =
                                queue.len() + in_flight.len() + deferred_transitives.len();
                            let _ = tx
                                .send(ResolvedPackage {
                                    dep_path: dep_path.clone(),
                                    name: linked_name.clone(),
                                    version: real_version.clone(),
                                    integrity: None,
                                    tarball_url: None,
                                    // local_source deps aren't aliased —
                                    // `file:`/`link:` specifiers go
                                    // through the local-source branch,
                                    // not the `npm:` rewrite.
                                    alias_of: None,
                                    local_source: Some(local.clone()),
                                    // Local `file:`/`link:` packages never
                                    // carry npm-style platform constraints
                                    // — they're whatever the user points
                                    // at, so the fetch coordinator treats
                                    // them as unconstrained (always fetch).
                                    os: aube_lockfile::PlatformList::new(),
                                    cpu: aube_lockfile::PlatformList::new(),
                                    libc: aube_lockfile::PlatformList::new(),
                                    deprecated: None,
                                    unpacked_size: None,
                                    pending,
                                })
                                .await;
                        }
                        // Enqueue transitive deps of the local package
                        // (directories, tarballs, portals, and exec
                        // outputs — `link:` deps are fully the
                        // target's responsibility).
                        if !matches!(local, LocalSource::Link(_)) {
                            let mut child_ancestors = task.ancestors.clone();
                            child_ancestors.push((linked_name.clone(), real_version.clone()));
                            for (child_name, child_range) in target_deps {
                                queue.push_back(ResolveTask::transitive(
                                    child_name,
                                    child_range,
                                    DepType::Production,
                                    dep_path.clone(),
                                    task.importer.clone(),
                                    child_ancestors.clone(),
                                ));
                            }
                        }
                    }
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }

                // Handle workspace linkage. Two cases resolve to the
                // workspace package rather than the registry:
                //   1. Explicit `workspace:` protocol (pnpm/yarn-berry
                //      style). The range after the prefix is accepted
                //      unconditionally — the user asserted this should
                //      link.
                //   2. Bare semver range whose name matches a workspace
                //      package whose version satisfies the range. This
                //      is the yarn-v1 / npm / bun default: siblings pin
                //      each other with normal version strings and
                //      expect the workspace to win over the registry.
                //      A workspace is typically either unpublished or
                //      is itself the source of truth for its name, so
                //      preferring the local copy matches every other
                //      mainstream pm.
                if let Some(ws_version) = workspace_packages.get(&task.name)
                    && (match task.range.strip_prefix("workspace:") {
                        // workspace:*, workspace:^, workspace:~
                        // bind to whatever local workspace version is.
                        // These are pnpm's "don't pin me, just track
                        // local" sigils. Match them before range check.
                        Some("" | "*" | "^" | "~") => true,
                        // workspace:<range> like workspace:^2.0.0 or
                        // workspace:1.x. Must still satisfy local
                        // version. Before this fix, any workspace:
                        // prefix short-circuited. Consumer could pin
                        // workspace:^2 against local 1.0.0 and aube
                        // would silently link the wrong version.
                        // pnpm errors here with no-matching-version.
                        Some(rest) => version_satisfies(ws_version, rest),
                        // Bare semver (no workspace: prefix) path.
                        // Linker walks up to workspace yarn-v1 style.
                        // Special case `*` and `""` (bare catch-all)
                        // to always match the workspace copy, even
                        // when the ws version is a prerelease like
                        // `0.0.0-0` which semver strict rules would
                        // otherwise exclude. Placeholder versions
                        // are common in fresh changesets-managed
                        // workspaces and would silently fall through
                        // to registry resolution otherwise, picking
                        // up a stale published build instead of the
                        // local source.
                        None if task.range.is_empty() || task.range == "*" => true,
                        None => version_satisfies(ws_version, &task.range),
                    })
                {
                    let dep_path = dep_path_for(&task.name, ws_version);
                    if task.is_root
                        && let Some(deps) = importers.get_mut(&task.importer)
                    {
                        deps.push(DirectDep {
                            name: task.name.clone(),
                            dep_path: dep_path.clone(),
                            dep_type: task.dep_type,
                            specifier: task.original_specifier.clone(),
                        });
                    }
                    if let Some(ref parent_dp) = task.parent
                        && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                    {
                        parent_pkg
                            .dependencies
                            .insert(task.name.clone(), ws_version.clone());
                        if task.dep_type == DepType::Optional {
                            parent_pkg
                                .optional_dependencies
                                .insert(task.name.clone(), ws_version.clone());
                        }
                    }
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }

                // Sibling dedupe. If another task for this same name
                // has already settled on a version that satisfies
                // this task's range, wire up to that resolution and
                // short-circuit. In the old wave code this check
                // lived in the post-fetch loop as `existing_match`;
                // in the pipelined loop we run it up front so
                // dedupable tasks never block on a fetch or a
                // lockfile scan.
                if let Some(matched_ver) = resolved_versions.get(&task.name).and_then(|versions| {
                    versions
                        .iter()
                        .find(|v| {
                            version_satisfies(v, &task.range)
                                && !is_vulnerable(task.registry_name(), v, &self.vulnerable_ranges)
                        })
                        .cloned()
                }) {
                    let dep_path = dep_path_for(&task.name, &matched_ver);
                    if task.is_root
                        && let Some(deps) = importers.get_mut(&task.importer)
                    {
                        deps.push(DirectDep {
                            name: task.name.clone(),
                            dep_path: dep_path.clone(),
                            dep_type: task.dep_type,
                            specifier: task.original_specifier.clone(),
                        });
                    }
                    if let Some(ref parent_dp) = task.parent
                        && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                    {
                        parent_pkg
                            .dependencies
                            .insert(task.name.clone(), matched_ver.clone());
                        if task.dep_type == DepType::Optional {
                            parent_pkg
                                .optional_dependencies
                                .insert(task.name.clone(), matched_ver);
                        }
                    }
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }

                // Lockfile reuse. Runs unconditionally after sibling
                // dedupe fails — the old code gated this behind a
                // `cache.contains_key` check, but in the pipelined
                // loop the cache is populated incrementally and the
                // gate was a false optimization.
                {
                    if let Some(locked_pkg) = existing.and_then(|g| {
                        g.packages.values().find(|p| {
                            p.name == task.name
                                && version_satisfies(&p.version, &task.range)
                                && !is_vulnerable(
                                    task.registry_name(),
                                    &p.version,
                                    &self.vulnerable_ranges,
                                )
                        })
                    }) {
                        // Drop optional deps whose platform constraints
                        // don't match the active host / supported set.
                        // This is the path that handles frozen/lockfile
                        // installs on a different machine than the one
                        // that wrote the lockfile.
                        if task.dep_type == DepType::Optional
                            && !is_supported(
                                &locked_pkg.os,
                                &locked_pkg.cpu,
                                &locked_pkg.libc,
                                &self.supported_architectures,
                            )
                        {
                            tracing::debug!(
                                "skipping optional dep {}@{}: platform mismatch",
                                task.name,
                                locked_pkg.version
                            );
                            if task.is_root
                                && let Some(spec) = task.original_specifier.as_ref()
                            {
                                skipped_optional_dependencies
                                    .entry(task.importer.clone())
                                    .or_default()
                                    .insert(task.name.clone(), spec.clone());
                            }
                            if task.is_root {
                                note_root_done!();
                            }
                            continue;
                        }
                        let version = locked_pkg.version.clone();
                        let dep_path = dep_path_for(&task.name, &version);

                        if task.is_root
                            && let Some(deps) = importers.get_mut(&task.importer)
                        {
                            deps.push(DirectDep {
                                name: task.name.clone(),
                                dep_path: dep_path.clone(),
                                dep_type: task.dep_type,
                                specifier: task.original_specifier.clone(),
                            });
                        }
                        if let Some(ref parent_dp) = task.parent
                            && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                        {
                            parent_pkg
                                .dependencies
                                .insert(task.name.clone(), version.clone());
                            if task.dep_type == DepType::Optional {
                                parent_pkg
                                    .optional_dependencies
                                    .insert(task.name.clone(), version.clone());
                            }
                        }
                        if visited.insert(std::sync::Arc::from(dep_path.as_str())) {
                            resolved_versions
                                .entry(task.name.clone())
                                .or_default()
                                .push(version.clone());

                            // Carry any round-tripped publish time
                            // forward so (a) the cutoff computation at
                            // the end of wave 0 can see reused directs
                            // alongside freshly-resolved ones and
                            // (b) the next lockfile write preserves the
                            // existing `time:` entry even when this
                            // install reuses the locked version without
                            // re-fetching a packument.
                            if self.should_record_times()
                                && let Some(g) = existing
                                && let Some(t) = g.times.get(&dep_path)
                            {
                                resolved_times.insert(dep_path.clone(), t.clone());
                            }

                            if let Some(ref tx) = self.resolved_tx {
                                let pending =
                                    queue.len() + in_flight.len() + deferred_transitives.len();
                                let _ = tx
                                    .send(ResolvedPackage {
                                        dep_path: dep_path.clone(),
                                        name: task.name.clone(),
                                        version: version.clone(),
                                        integrity: locked_pkg.integrity.clone(),
                                        tarball_url: locked_pkg.tarball_url.clone(),
                                        // Carry the alias identity
                                        // through the reuse path — the
                                        // existing `locked_pkg` already
                                        // records it if the lockfile held
                                        // an aliased entry, so the
                                        // streaming fetch still hits the
                                        // real registry name.
                                        alias_of: locked_pkg.alias_of.clone(),
                                        local_source: locked_pkg.local_source.clone(),
                                        os: locked_pkg.os.clone(),
                                        cpu: locked_pkg.cpu.clone(),
                                        libc: locked_pkg.libc.clone(),
                                        // Lockfile reuse skips the packument
                                        // fetch, so we have no deprecation
                                        // message to forward here. The
                                        // `aube deprecations` command re-queries
                                        // packuments live for the
                                        // after-the-fact view.
                                        deprecated: None,
                                        // Same reasoning: lockfile reuse
                                        // doesn't refetch the packument and
                                        // LockedPackage doesn't carry size
                                        // metadata, so the size-estimate
                                        // segment stays absent for these
                                        // packages. The progress UI displays
                                        // a running download total instead
                                        // when the estimate is unavailable.
                                        unpacked_size: None,
                                        pending,
                                    })
                                    .await;
                            }

                            // Carry declared peer deps forward from the
                            // existing lockfile so subsequent peer-context
                            // computation sees them without a re-fetch.
                            resolved.insert(
                                dep_path.clone(),
                                LockedPackage {
                                    name: task.name.clone(),
                                    version: version.clone(),
                                    integrity: locked_pkg.integrity.clone(),
                                    dependencies: BTreeMap::new(),
                                    optional_dependencies: BTreeMap::new(),
                                    peer_dependencies: locked_pkg.peer_dependencies.clone(),
                                    peer_dependencies_meta: locked_pkg
                                        .peer_dependencies_meta
                                        .clone(),
                                    dep_path: dep_path.clone(),
                                    local_source: locked_pkg.local_source.clone(),
                                    os: locked_pkg.os.clone(),
                                    cpu: locked_pkg.cpu.clone(),
                                    libc: locked_pkg.libc.clone(),
                                    bundled_dependencies: locked_pkg.bundled_dependencies.clone(),
                                    optional: locked_pkg.optional,
                                    transitive_peer_dependencies: locked_pkg
                                        .transitive_peer_dependencies
                                        .clone(),
                                    tarball_url: locked_pkg.tarball_url.clone(),
                                    alias_of: locked_pkg.alias_of.clone(),
                                    yarn_checksum: locked_pkg.yarn_checksum.clone(),
                                    engines: locked_pkg.engines.clone(),
                                    bin: locked_pkg.bin.clone(),
                                    declared_dependencies: locked_pkg.declared_dependencies.clone(),
                                    license: locked_pkg.license.clone(),
                                    funding_url: locked_pkg.funding_url.clone(),
                                    extra_meta: locked_pkg.extra_meta.clone(),
                                },
                            );

                            // Enqueue transitive deps from the locked package.
                            // Strip any peer-context suffix off the version
                            // before treating it as a semver range — a
                            // locked `"18.2.0(react@18.2.0)"` tail should
                            // match against packuments as just `18.2.0`.
                            // Also strip a leading `name@` if present:
                            // bun/yarn parsers store transitive deps in
                            // `name@version` (full dep_path) form, while
                            // pnpm stores bare versions. Without the
                            // strip, a yarn/bun-locked `is-odd` would
                            // emit a transitive task for is-number with
                            // range `"is-number@6.0.0"`, which doesn't
                            // parse as semver and fails resolution.
                            // The lockfile already omitted bundled dep
                            // edges on write, so iterating
                            // `locked_pkg.dependencies` naturally skips them.
                            let mut child_ancestors = task.ancestors.clone();
                            child_ancestors.push((task.name.clone(), version.clone()));
                            for (dep_name, dep_version) in &locked_pkg.dependencies {
                                let prefix = format!("{dep_name}@");
                                let stripped =
                                    dep_version.strip_prefix(&prefix).unwrap_or(dep_version);
                                let canonical_version =
                                    stripped.split('(').next().unwrap_or(stripped).to_string();
                                let dep_type =
                                    if locked_pkg.optional_dependencies.contains_key(dep_name) {
                                        DepType::Optional
                                    } else {
                                        DepType::Production
                                    };
                                queue.push_back(ResolveTask::transitive(
                                    dep_name.clone(),
                                    canonical_version,
                                    dep_type,
                                    dep_path.clone(),
                                    task.importer.clone(),
                                    child_ancestors.clone(),
                                ));
                            }
                        }
                        lockfile_reuse_count += 1;
                        if task.is_root {
                            note_root_done!();
                        }
                        continue;
                    }
                }

                // Packument not in cache. Spawn its fetch if one
                // isn't already running, then wait for packument
                // fetches to land until this task's packument is
                // available. Other fetches that happen to complete
                // while we're waiting get cached opportunistically,
                // which is exactly what lets the pipeline overlap
                // network and CPU: by the time a later task is
                // popped its packument is usually already sitting
                // in the cache because it landed while an earlier
                // task was being waited on.
                let wait_start = std::time::Instant::now();
                // Cache is keyed by the *registry* name — for aliased
                // tasks `task.name` is the user-facing alias (e.g.
                // `h3-v2`), which would never hit. `registry_name()`
                // returns the alias-resolved target (`h3`) on
                // aliased tasks and `task.name` otherwise.
                let fetch_name = task.registry_name().to_string();
                let _diag_task_wait = aube_util::diag::Span::new(
                    aube_util::diag::Category::Resolver,
                    "task_wait_packument",
                )
                .with_meta_fn(|| format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&fetch_name)));
                while !self.cache.contains_key(&fetch_name) {
                    ensure_fetch!(&fetch_name);
                    match in_flight.join_next().await {
                        Some(Ok(Ok((name, packument, from_primer)))) => {
                            in_flight_names.remove(&name);
                            if from_primer {
                                primer_seeded_names.insert(name.clone());
                            }
                            self.cache.insert(name, packument);
                            packument_fetch_count += 1;
                        }
                        Some(Ok(Err(e))) => return Err(e),
                        Some(Err(join_err)) => {
                            return Err(Error::Registry(
                                "(join)".to_string(),
                                join_err.to_string(),
                            ));
                        }
                        None => {
                            // ensure_fetch! guarantees something is
                            // in flight if the cache still doesn't
                            // hold this name, so a None here means
                            // the spawn failed silently. Surface it.
                            return Err(Error::Registry(
                                fetch_name.clone(),
                                "packument fetch disappeared before completing".to_string(),
                            ));
                        }
                    }
                }
                packument_fetch_time += wait_start.elapsed();

                // TimeBased wave-0 gate. Transitives that reach
                // the version-pick step while the cutoff is still
                // unknown must wait until the direct deps have
                // been picked and the cutoff has been derived;
                // otherwise they'd pick against a `None` cutoff
                // and miss the filter. In `Highest` mode (the
                // default), `cutoff_pending` starts false and this
                // is a no-op.
                if cutoff_pending && !task.is_root {
                    deferred_transitives.push(task);
                    continue;
                }

                // Version-pick + transitive enqueue. Was a separate
                // sub-loop over `processed_batch` in the old wave
                // code; here it's inline as the tail of the per-task
                // pipeline now that we know the packument is in
                // cache. `registry_name()` is the cache key for
                // aliased tasks (cache is populated under the real
                // registry name), so use the same accessor here.
                // Find locked version
                let locked_version = existing.and_then(|g| {
                    g.packages
                        .values()
                        .find(|p| p.name == task.name && version_satisfies(&p.version, &task.range))
                        .map(|p| p.version.as_str())
                        .filter(|v| {
                            !is_vulnerable(task.registry_name(), v, &self.vulnerable_ranges)
                        })
                });

                // Direct deps in time-based mode pick the lowest
                // satisfying version; everything else (transitives,
                // and all picks in Highest mode) picks highest.
                let pick_lowest = self.resolution_mode == ResolutionMode::TimeBased && task.is_root;
                // Apply the cutoff unless this package is on the
                // minimumReleaseAge exclude list. The exclude list only
                // suppresses the *minimumReleaseAge* leg, not the
                // time-based-mode leg — but since we collapse both
                // into the same `published_by` string at this point,
                // we have to skip the cutoff entirely for excluded
                // names. Acceptable: time-based mode and exclude
                // lists aren't expected to coexist in the wild.
                let cutoff_for_pkg = match self.minimum_release_age.as_ref() {
                    Some(mra) if mra.exclude.contains(&task.name) => None,
                    _ => published_by.as_deref(),
                };
                // Strict semantics in two cases:
                //   - `minimumReleaseAgeStrict=true` (the user opted in
                //     to hard failures), or
                //   - the cutoff comes from `--resolution-mode=time-based`
                //     alone, with no `minimumReleaseAge` configured. The
                //     time-based cutoff is intended as a hard wall — if
                //     no version fits, the *correct* fix is for the user
                //     to update the lockfile, not for the resolver to
                //     silently pick a different version.
                let strict = match self.minimum_release_age.as_ref() {
                    Some(m) => m.strict,
                    None => true,
                };
                let registry_name = task.registry_name().to_string();
                let selected_pick = loop {
                    let packument = self.cache.get(&registry_name).ok_or_else(|| {
                        Error::Registry(registry_name.clone(), "packument not in cache".to_string())
                    })?;
                    let pick = pick_version(
                        packument,
                        &task.range,
                        locked_version,
                        pick_lowest,
                        cutoff_for_pkg,
                        strict,
                    );
                    match pick {
                        PickResult::Found(meta) => break meta.clone(),
                        PickResult::AgeGated | PickResult::NoMatch
                            if primer_seeded_names.remove(&registry_name) =>
                        {
                            let fetch_start = std::time::Instant::now();
                            let live = if needs_time {
                                match self.packument_full_cache_dir.as_ref() {
                                    Some(dir) => {
                                        self.client
                                            .fetch_packument_with_time_cached(&registry_name, dir)
                                            .await
                                    }
                                    None => self.client.fetch_packument(&registry_name).await,
                                }
                            } else {
                                match self.client.fetch_packument(&registry_name).await {
                                    Ok(live) => {
                                        if let Some(dir) = self.packument_cache_dir.as_ref() {
                                            self.client.replace_packument_cache(
                                                &registry_name,
                                                dir,
                                                &live,
                                            );
                                        }
                                        Ok(live)
                                    }
                                    Err(err) => Err(err),
                                }
                            }
                            .map_err(|e| Error::Registry(registry_name.clone(), e.to_string()))?;
                            packument_fetch_time += fetch_start.elapsed();
                            packument_fetch_count += 1;
                            self.cache.insert(registry_name.clone(), live);
                        }
                        // Only surface `AgeGate` when the cutoff actually
                        // came from `minimumReleaseAge`. When it came from
                        // `--resolution-mode=time-based` alone, the user
                        // never opted into the supply-chain age gate, so
                        // the failure should report as a plain no-match
                        // instead of a misleading "older than 0 minutes".
                        PickResult::AgeGated => match self.minimum_release_age.as_ref() {
                            Some(mra) => {
                                return Err(Error::AgeGate(Box::new(error::build_age_gate(
                                    &task,
                                    packument,
                                    mra.minutes,
                                ))));
                            }
                            None => {
                                return Err(Error::NoMatch(Box::new(error::build_no_match(
                                    &task, packument,
                                ))));
                            }
                        },
                        PickResult::NoMatch => {
                            return Err(Error::NoMatch(Box::new(error::build_no_match(
                                &task, packument,
                            ))));
                        }
                    }
                };
                let packument = self.cache.get(&registry_name).ok_or_else(|| {
                    Error::Registry(registry_name.clone(), "packument not in cache".to_string())
                })?;
                let picked_ref = prefer_non_vulnerable_pick(
                    task.registry_name(),
                    packument,
                    &task.range,
                    &selected_pick,
                    pick_lowest,
                    cutoff_for_pkg,
                    &self.vulnerable_ranges,
                );
                // Trust-policy enforcement runs *before* any other
                // post-pick processing (mirrors pnpm's placement
                // immediately after `pickPackage`). Skip when policy is
                // off so the off-by-default case is a single enum
                // compare. The check needs the live packument's `time`
                // map and all version metadata, both of which are still
                // in scope here from L1191.
                if self.dependency_policy.trust_policy == crate::TrustPolicy::NoDowngrade {
                    crate::trust::check_no_downgrade(
                        packument,
                        &picked_ref.version,
                        picked_ref,
                        &self.dependency_policy.trust_policy_exclude,
                        self.dependency_policy.trust_policy_ignore_after,
                    )
                    .map_err(|e| match e {
                        crate::trust::TrustCheckError::Downgrade(d) => {
                            Error::TrustDowngrade(Box::new(d))
                        }
                        crate::trust::TrustCheckError::MissingTime(d) => {
                            Error::TrustCheckMissingTime(Box::new(d))
                        }
                    })?;
                }

                // Clone the picked metadata into an owned value so we can
                // both run the `readPackage` hook (which needs a
                // disjoint `&mut self` borrow) and, later, mutate the
                // resolver's own caches without holding a borrow into
                // `self.cache`. Also grab the publish-time entry now,
                // for the same reason.
                let mut picked_owned = picked_ref.clone();
                let picked_publish_time = packument.time.get(&picked_ref.version).cloned();
                // Skip the readPackage hook entirely for a `(name, version)`
                // pair we've already fully processed via a prior task. The
                // mutated dep maps only drive the transitive enqueue below,
                // and that block is short-circuited by the `visited` guard
                // later in this iteration — so running the hook here would
                // just burn an IPC round-trip whose result is discarded.
                let prehook_dep_path = dep_path_for(&task.name, &picked_ref.version);
                let already_visited = visited.contains(prehook_dep_path.as_str());

                if !already_visited {
                    apply_package_extensions(
                        &mut picked_owned,
                        &self.dependency_policy.package_extensions,
                    );
                }

                // readPackage hook. Runs at most once per version-picked
                // package, before transitive enqueue. We honor edits to
                // the four dep maps and warn on (then discard) edits to
                // name/version/dist/platform/`hasInstallScript` — pnpm
                // tolerates readPackage returning a hollowed-out
                // object, so we restore those fields from the original
                // packument entry after the call.
                if !already_visited && let Some(hook) = self.read_package_hook.as_mut() {
                    let before_name = picked_owned.name.clone();
                    let before_version = picked_owned.version.clone();
                    let before_dist = picked_owned.dist.clone();
                    let before_os = picked_owned.os.clone();
                    let before_cpu = picked_owned.cpu.clone();
                    let before_libc = picked_owned.libc.clone();
                    let before_bundled = picked_owned.bundled_dependencies.clone();
                    let before_has_install_script = picked_owned.has_install_script;
                    let before_deprecated = picked_owned.deprecated.clone();
                    let input = picked_owned.clone();
                    let mut after = hook.read_package(input).await.map_err(|e| {
                        Error::Registry(before_name.clone(), format!("readPackage hook: {e}"))
                    })?;
                    if after.name != before_name || after.version != before_version {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_HOOK_IDENTITY_REWRITTEN,
                            "[pnpmfile] readPackage rewrote {}@{} identity to {}@{}; \
                             aube ignores identity edits",
                            before_name,
                            before_version,
                            after.name,
                            after.version,
                        );
                    }
                    after.name = before_name;
                    after.version = before_version;
                    after.dist = before_dist;
                    after.os = before_os;
                    after.cpu = before_cpu;
                    after.libc = before_libc;
                    after.bundled_dependencies = before_bundled;
                    after.has_install_script = before_has_install_script;
                    after.deprecated = before_deprecated;
                    picked_owned = after;
                }
                let version_meta = &picked_owned;

                // Optional deps that don't match the host platform get
                // silently dropped — pnpm parity. Required deps with a
                // bad platform still get installed; the warning matches
                // pnpm's `packageIsInstallable` behavior.
                let platform_ok = is_supported(
                    &version_meta.os,
                    &version_meta.cpu,
                    &version_meta.libc,
                    &self.supported_architectures,
                );
                if !platform_ok {
                    if task.dep_type == DepType::Optional {
                        tracing::debug!(
                            "skipping optional dep {}@{}: unsupported platform (os={:?} cpu={:?} libc={:?})",
                            task.name,
                            version_meta.version,
                            version_meta.os,
                            version_meta.cpu,
                            version_meta.libc
                        );
                        if task.is_root
                            && let Some(spec) = task.original_specifier.as_ref()
                        {
                            skipped_optional_dependencies
                                .entry(task.importer.clone())
                                .or_default()
                                .insert(task.name.clone(), spec.clone());
                        }
                        if task.is_root {
                            note_root_done!();
                        }
                        continue;
                    }
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_UNSUPPORTED_PLATFORM_INSTALL,
                        "required dep {}@{} declares unsupported platform (os={:?} cpu={:?} libc={:?}); installing anyway",
                        task.name,
                        version_meta.version,
                        version_meta.os,
                        version_meta.cpu,
                        version_meta.libc
                    );
                }

                let version = version_meta.version.clone();
                let dep_path = dep_path_for(&task.name, &version);

                // Record publish time for the cutoff / `time:` block
                // whenever the packument carries one — matches pnpm,
                // which populates `publishedAt` opportunistically via
                // `meta.time?.[version]` regardless of resolution mode.
                // Corgi packuments from npmjs.org omit `time`, so in
                // Highest mode this is usually a no-op; Verdaccio
                // (v5.15.1+) and full-packument fetches do include it,
                // and then we round-trip it into the lockfile just like
                // pnpm does.
                //
                // Fall back to the prior lockfile's time when the
                // packument doesn't carry one — `aube update` filters
                // direct deps out of `existing.packages` to force a
                // fresh resolve, so the lockfile-reuse fallback further
                // up doesn't fire for them. Without this fallback the
                // resolver-fetched corgi (no time) would silently drop
                // the dep's `time:` entry on every update, even when
                // the version didn't change. Reported in discussion
                // #345 (mrazauskas).
                if self.should_record_times() {
                    if let Some(t) = picked_publish_time.as_ref() {
                        resolved_times.insert(dep_path.clone(), t.clone());
                    } else if let Some(g) = existing
                        && let Some(t) = g.times.get(&dep_path)
                    {
                        resolved_times.insert(dep_path.clone(), t.clone());
                    }
                }

                // Record root dep
                if task.is_root
                    && let Some(deps) = importers.get_mut(&task.importer)
                {
                    deps.push(DirectDep {
                        name: task.name.clone(),
                        dep_path: dep_path.clone(),
                        dep_type: task.dep_type,
                        specifier: task.original_specifier.clone(),
                    });
                }

                // Wire parent
                if let Some(ref parent_dp) = task.parent
                    && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                {
                    parent_pkg
                        .dependencies
                        .insert(task.name.clone(), version.clone());
                    if task.dep_type == DepType::Optional {
                        parent_pkg
                            .optional_dependencies
                            .insert(task.name.clone(), version.clone());
                    }
                }

                // Skip if already fully processed this exact version
                if visited.contains(dep_path.as_str()) {
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }
                visited.insert(std::sync::Arc::from(dep_path.as_str()));

                tracing::trace!("resolved {}@{}", task.name, version);

                // Forward a deprecation message to the install command,
                // subject to `allowedDeprecatedVersions` suppression.
                // User-facing rendering is the CLI's job — doing it here
                // would fire per resolved version with no way for the
                // caller to batch or filter direct-vs-transitive.
                let deprecated_msg: Option<Arc<str>> =
                    version_meta.deprecated.as_deref().and_then(|msg| {
                        let suppressed = is_deprecation_allowed(
                            &task.name,
                            &version,
                            &self.dependency_policy.allowed_deprecated_versions,
                        );
                        (!suppressed).then(|| Arc::<str>::from(msg))
                    });

                // Track this version
                resolved_versions
                    .entry(task.name.clone())
                    .or_default()
                    .push(version.clone());

                let integrity = version_meta.dist.as_ref().and_then(|d| d.integrity.clone());
                // Always stash the registry tarball URL on the locked
                // package. pnpm / yarn writers gate emission on
                // `lockfile_include_tarball_url` (so the pnpm
                // round-trip stays byte-identical for projects that
                // opted out); the npm writer emits `resolved:` on
                // every package entry unconditionally, which is what
                // npm itself writes. Carrying the URL on every
                // LockedPackage lets both policies work without a
                // second packument fetch at write time.
                let tarball_url = version_meta.dist.as_ref().map(|d| d.tarball.clone());

                // Stream this resolved package for early tarball fetching.
                // `alias_of` mirrors what the LockedPackage below
                // will carry — the streaming fetch consumer in
                // install.rs uses it to derive the real tarball URL
                // for aliased packages where `name` alone (`h3-v2`)
                // would 404.
                if let Some(ref tx) = self.resolved_tx {
                    let pending = queue.len() + in_flight.len() + deferred_transitives.len();
                    let _ = tx
                        .send(ResolvedPackage {
                            dep_path: dep_path.clone(),
                            name: task.name.clone(),
                            version: version.clone(),
                            integrity: integrity.clone(),
                            tarball_url: tarball_url.clone(),
                            alias_of: task.real_name.clone(),
                            local_source: None,
                            os: version_meta.os.iter().cloned().collect(),
                            cpu: version_meta.cpu.iter().cloned().collect(),
                            libc: version_meta.libc.iter().cloned().collect(),
                            deprecated: deprecated_msg.clone(),
                            unpacked_size: version_meta.dist.as_ref().and_then(|d| d.unpacked_size),
                            pending,
                        })
                        .await;
                }

                // Capture the declared peer deps now so the post-pass can
                // compute each consumer's peer context without re-reading
                // the packument.
                let peer_deps = version_meta.peer_dependencies.clone();
                let peer_meta: BTreeMap<String, aube_lockfile::PeerDepMeta> = version_meta
                    .peer_dependencies_meta
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            aube_lockfile::PeerDepMeta {
                                optional: v.optional,
                            },
                        )
                    })
                    .collect();
                // `bundledDependencies` names are shipped inside the
                // tarball itself and must not be resolved from the
                // registry. If we did enqueue them, we'd fetch a
                // (possibly different) version and plant a sibling
                // symlink inside `.aube/<parent>@ver/node_modules/`
                // that would shadow the bundled copy during Node's
                // directory walk. Compute the skip set once here and
                // store the names on the LockedPackage so restore
                // (from lockfile, skipping this code path) also
                // knows to avoid the sibling symlinks — see the
                // `.dependencies` write-through downstream.
                let bundled_names: FxHashSet<String> = version_meta
                    .bundled_dependencies
                    .as_ref()
                    .map(|b| {
                        b.names(&version_meta.dependencies)
                            .into_iter()
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();

                resolved.insert(
                    dep_path.clone(),
                    LockedPackage {
                        name: task.name.clone(),
                        version: version.clone(),
                        integrity,
                        dependencies: BTreeMap::new(),
                        optional_dependencies: BTreeMap::new(),
                        peer_dependencies: peer_deps,
                        peer_dependencies_meta: peer_meta,
                        dep_path: dep_path.clone(),
                        local_source: None,
                        os: version_meta.os.iter().cloned().collect(),
                        cpu: version_meta.cpu.iter().cloned().collect(),
                        libc: version_meta.libc.iter().cloned().collect(),
                        bundled_dependencies: {
                            let mut v: Vec<String> = bundled_names.iter().cloned().collect();
                            v.sort();
                            v
                        },
                        tarball_url,
                        // `name` is the alias for npm-aliased tasks
                        // (`"h3-v2": "npm:h3@..."` → name = "h3-v2"),
                        // so stash the real registry name here. The
                        // lockfile writer + installer consult
                        // `alias_of` whenever they need to hit the
                        // registry, matching how the npm-lockfile
                        // reader populates this field.
                        alias_of: task.real_name.clone(),
                        yarn_checksum: None,
                        engines: version_meta.engines.clone(),
                        // Rehydrate a string-form bin (`"bin": "cli.js"`)
                        // into `{<package_name>: "cli.js"}` — registry
                        // packuments leave the name off, expecting
                        // consumers to default it to the package name.
                        // Doing it here keeps bun's per-entry meta
                        // byte-identical to bun's own output without
                        // pushing the fixup into every writer.
                        bin: {
                            let mut m = version_meta.bin.clone();
                            if let Some(path) = m.remove("") {
                                // String-form `bin` in a packument
                                // (`"bin": "cli.js"`) is implicitly
                                // named after the real registry
                                // package — not the alias. For an
                                // aliased dep (`"h3-v2": "npm:h3@…"`)
                                // the bun writer must emit the bin
                                // under `h3`, not `h3-v2`, or the
                                // map drifts against bun's own
                                // output (and the shim install path
                                // creates the wrong binary name).
                                let bin_name =
                                    task.real_name.as_deref().unwrap_or(&task.name).to_string();
                                m.insert(bin_name, path);
                            }
                            m
                        },
                        // Declared ranges straight from the packument's
                        // `dependencies` / `optionalDependencies`. Fed
                        // back out by npm / yarn / bun writers so
                        // nested package entries keep the original
                        // specifiers instead of collapsing to pins.
                        declared_dependencies: {
                            let mut m = version_meta.dependencies.clone();
                            for (k, v) in &version_meta.optional_dependencies {
                                m.insert(k.clone(), v.clone());
                            }
                            m
                        },
                        license: version_meta.license.clone(),
                        funding_url: version_meta.funding_url.clone(),
                        optional: false,
                        transitive_peer_dependencies: Vec::new(),
                        extra_meta: BTreeMap::new(),
                    },
                );

                // Enqueue transitive deps. Kick off a background
                // packument fetch the instant we discover the dep
                // name — so by the time the task is popped off the
                // queue below, its packument is usually already in
                // flight (and often already in cache). This is where
                // the pipeline overlaps fetches with CPU work without
                // any explicit wave barrier.
                //
                // Compute the child ancestor chain once — the same
                // frame (this package's name + resolved version)
                // applies to every dep / optionalDep / peer we enqueue
                // below.
                let mut child_ancestors = task.ancestors.clone();
                child_ancestors.push((task.name.clone(), version.clone()));

                for (dep_name, dep_range) in &version_meta.dependencies {
                    if bundled_names.contains(dep_name) {
                        continue;
                    }
                    if self.dependency_policy.block_exotic_subdeps
                        && is_non_registry_specifier(dep_range)
                    {
                        return Err(Error::Registry(
                            dep_name.clone(),
                            format!(
                                "uses exotic specifier \"{dep_range}\" which is blocked \
                                 by blockExoticSubdeps (declared by {})",
                                task.name
                            ),
                        ));
                    }
                    if !existing_names.contains(dep_name.as_str())
                        && self.is_prefetchable(
                            dep_name.as_str(),
                            dep_range.as_str(),
                            workspace_packages,
                        )
                    {
                        ensure_fetch!(dep_name);
                    }
                    queue.push_back(ResolveTask::transitive(
                        dep_name.clone(),
                        dep_range.clone(),
                        DepType::Production,
                        dep_path.clone(),
                        task.importer.clone(),
                        child_ancestors.clone(),
                    ));
                }

                for (dep_name, dep_range) in &version_meta.optional_dependencies {
                    if bundled_names.contains(dep_name) {
                        continue;
                    }
                    if self.ignored_optional_dependencies.contains(dep_name) {
                        continue;
                    }
                    if self.dependency_policy.block_exotic_subdeps
                        && is_non_registry_specifier(dep_range)
                    {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_EXOTIC_SUBDEP_SKIPPED,
                            "skipping optional dependency {dep_name} of {} — \
                             exotic specifier \"{dep_range}\" blocked by blockExoticSubdeps",
                            task.name
                        );
                        continue;
                    }
                    if !existing_names.contains(dep_name.as_str())
                        && self.is_prefetchable(
                            dep_name.as_str(),
                            dep_range.as_str(),
                            workspace_packages,
                        )
                    {
                        ensure_fetch!(dep_name);
                    }
                    queue.push_back(ResolveTask::transitive(
                        dep_name.clone(),
                        dep_range.clone(),
                        DepType::Optional,
                        dep_path.clone(),
                        task.importer.clone(),
                        child_ancestors.clone(),
                    ));
                }

                // Peer dependencies: enqueue only required peers that
                // are truly missing from the importer/root scope. The
                // post-pass below (`apply_peer_contexts`) computes
                // which version each consumer sees, via ancestor
                // scope, and assigns peer-suffixed dep_paths.
                //
                // pnpm's `auto-install-peers=true` fills in missing
                // required peers, but it does not install optional peer
                // alternatives that the user did not ask for, and it
                // does not install a second compatible peer when the
                // importer already declares that peer name at an
                // incompatible version. In the latter case pnpm keeps
                // the user's direct dependency and reports an unmet
                // peer warning.
                //
                // When `auto-install-peers=false`, we skip enqueueing
                // peers entirely. Users are on the hook for adding
                // them to `package.json` themselves. Unmet peers still
                // surface as warnings via `detect_unmet_peers` after
                // resolve — in fact more so, since nothing gets
                // auto-installed.
                //
                // Skip peers that are already declared as regular or
                // optional deps of the same package — those already have a
                // task queued via the loops above, and duplicating would
                // just burn a queue slot.
                if self.auto_install_peers {
                    for (dep_name, dep_range) in &version_meta.peer_dependencies {
                        let peer_optional = version_meta
                            .peer_dependencies_meta
                            .get(dep_name)
                            .map(|m| m.optional)
                            .unwrap_or(false);
                        // Optional peers are opt-in integrations, not
                        // auto-install candidates. Users who need one must
                        // declare it in their own manifest so the normal dep
                        // loops above resolve it explicitly.
                        if peer_optional {
                            continue;
                        }
                        let importer_declares_peer = importer_declared_dep_names
                            .get(&task.importer)
                            .is_some_and(|names| names.contains(dep_name));
                        let root_declares_peer = self.resolve_peers_from_workspace_root
                            && task.importer != "."
                            && importer_declared_dep_names
                                .get(".")
                                .is_some_and(|names| names.contains(dep_name));
                        let peer_dep_is_ancestor =
                            task.ancestors.iter().any(|(name, _)| name == dep_name);
                        if importer_declares_peer || root_declares_peer || peer_dep_is_ancestor {
                            continue;
                        }
                        if version_meta.dependencies.contains_key(dep_name)
                            || version_meta.optional_dependencies.contains_key(dep_name)
                            || bundled_names.contains(dep_name)
                        {
                            continue;
                        }
                        if self.dependency_policy.block_exotic_subdeps
                            && is_non_registry_specifier(dep_range)
                        {
                            tracing::warn!(
                                code = aube_codes::warnings::WARN_AUBE_EXOTIC_SUBDEP_SKIPPED,
                                "skipping peer dependency {dep_name} of {} — \
                                 exotic specifier \"{dep_range}\" blocked \
                                 by blockExoticSubdeps",
                                task.name
                            );
                            continue;
                        }
                        if !existing_names.contains(dep_name.as_str())
                            && self.is_prefetchable(
                                dep_name.as_str(),
                                dep_range.as_str(),
                                workspace_packages,
                            )
                        {
                            ensure_fetch!(dep_name);
                        }
                        queue.push_back(ResolveTask::transitive(
                            dep_name.clone(),
                            dep_range.clone(),
                            DepType::Production,
                            dep_path.clone(),
                            task.importer.clone(),
                            child_ancestors.clone(),
                        ));
                    }
                }

                // Root task just completed its full version-pick
                // path. Decrement the pending-directs counter so
                // the TimeBased cutoff trigger at the top of the
                // outer loop can fire once wave 0 is resolved.
                if task.is_root {
                    note_root_done!();
                }
            }
        }

        // Drain any remaining in-flight fetches so their tasks get
        // cleanly joined. Normally the main loop has harvested every
        // spawned fetch by the time the queue drains, but a few may
        // still be pending if the resolver short-circuited via
        // sibling dedupe or lockfile reuse after ensure_fetch! had
        // already spawned them.
        while in_flight.join_next().await.is_some() {}

        let resolve_elapsed = resolve_start.elapsed();
        tracing::debug!(
            "resolver: {:.1?} total, {} packuments fetched ({:.1?} wall), {} reused from lockfile, {} packages resolved",
            resolve_elapsed,
            packument_fetch_count,
            packument_fetch_time,
            lockfile_reuse_count,
            resolved.len()
        );
        // Surface the resolver mix to the diag analyzer so the lockfile
        // reuse path can be spotted independently of the cold no-lockfile
        // path. Counts of: total packages resolved, of which N reused
        // from a prior lockfile and M required a network packument fetch.
        let resolved_count = resolved.len();
        aube_util::diag::instant_lazy(aube_util::diag::Category::Resolver, "decision_mix", || {
            format!(
                r#"{{"resolved":{},"lockfile_reused":{},"packuments_fetched":{}}}"#,
                resolved_count, lockfile_reuse_count, packument_fetch_count
            )
        });

        let contextualized = self.finalize_resolved_graph(
            importers,
            resolved,
            &resolved_versions,
            resolved_times,
            skipped_optional_dependencies,
            catalog_picks,
        )?;
        if let Some((state, sem)) = packument_persist_handle {
            sem.persist(&state, "packument:default");
        }
        Ok(contextualized)
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
