use super::dep_path::{
    parse_dep_path, peerless_alias_target, rewrite_snapshot_alias_deps, version_to_dep_path,
};
use super::raw::{RawDepSpec, local_source_from_resolution, parse_raw_lockfile};
use crate::{
    CatalogEntry, DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph,
    PeerDepMeta, git_commits_match,
};
use std::collections::BTreeMap;
use std::path::Path;

/// Parse a pnpm-lock.yaml file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let content = crate::read_lockfile(path)?;
    let raw = parse_raw_lockfile(&content)
        .map_err(|e| Error::parse_yaml_err(path, content.clone(), &e))?;

    // Parse importers (direct deps of each workspace package).
    // We track synthesized LockedPackages for local (`file:` / `link:`)
    // deps here so the main packages loop below doesn't try to process
    // them off the canonical lockfile key.
    let mut importers = BTreeMap::new();
    let mut local_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>> =
        BTreeMap::new();
    // pnpm v9 encodes npm-aliases implicitly: the importer key is
    // the alias (`express-fork`), `specifier:` carries `npm:<real>@<range>`,
    // and `version:` is `<real>@<resolved>`. There is no `aliasOf:`
    // field — that's an aube-specific writer extension. We record
    // each alias here and synthesize an alias-keyed LockedPackage
    // after the canonical packages loop, mirroring the shape the
    // resolver-fresh path emits so the linker stays single-shape.
    // Tuple: (alias_dep_path, real_dep_path, alias_name, real_name).
    let mut alias_remaps: Vec<(String, String, String, String)> = Vec::new();

    let mut push_direct = |deps: &mut Vec<DirectDep>,
                           alias_remaps: &mut Vec<(String, String, String, String)>,
                           name: &str,
                           info: &RawDepSpec,
                           dep_type: DepType| {
        // pnpm appends a `(peer@ver)` suffix to the importer
        // `version:` of URL- and git-based direct deps when the
        // resolved snapshot carries peer context, the same way it
        // does for semver versions. `LocalSource::parse` treats the
        // whole string as the URL, so a RemoteTarballSource built
        // from the raw value fetches `…/tar.gz/SHA(peer@ver)` and
        // 404s. Strip it here so the URL that reaches the fetcher
        // and the dep_path hash are both peer-context-free —
        // consistent with what `parse_dep_path` does for snapshot
        // keys downstream.
        let classify_version = info.version.split('(').next().unwrap_or(&info.version);
        if let Some(local) = LocalSource::parse(classify_version, Path::new("")) {
            // `Path::new("")` means tarball-vs-dir classification is
            // skipped; we default to Directory and rely on the
            // resolver's on-disk re-read for the authoritative source
            // type during a subsequent `aube install` (lockfile-only
            // path never materializes local deps anyway before the
            // fetch step re-classifies).
            //
            // Re-classify Directory → Tarball if the path looks
            // like a tarball filename, so `.tgz`/`.tar.gz`
            // targets round-trip correctly even when the file
            // isn't present at parse time. The filename
            // heuristic lives on `LocalSource` so this stays in
            // lockstep with `LocalSource::parse`.
            let local = match local {
                LocalSource::Directory(p) if LocalSource::path_looks_like_tarball(&p) => {
                    LocalSource::Tarball(p)
                }
                // Importer `version:` for git deps is the canonical
                // `<url>#<commit>` form pnpm writes. The parser
                // puts the `<commit>` into `committish`; since
                // this is a lockfile round-trip (not a raw user
                // spec), treat it as the pinned commit.
                LocalSource::Git(mut g) if g.resolved.is_empty() => {
                    if let Some(c) = g.committish.take() {
                        g.resolved = c;
                    }
                    LocalSource::Git(g)
                }
                other => other,
            };
            let dep_path = local.dep_path(name);
            deps.push(DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type,
                specifier: Some(info.specifier.clone()),
            });
            local_packages
                .entry(dep_path.clone())
                .or_insert_with(|| LockedPackage {
                    name: name.to_string(),
                    version: "0.0.0".to_string(),
                    integrity: None,
                    dependencies: BTreeMap::new(),
                    peer_dependencies: BTreeMap::new(),
                    peer_dependencies_meta: BTreeMap::new(),
                    dep_path,
                    local_source: Some(local),
                    ..Default::default()
                });
        } else {
            // Detect npm-aliased deps purely from the shape of
            // `version:`. pnpm encodes aliases as
            // `<real_name>@<resolved>(peers…)` regardless of how the
            // alias was declared:
            //   - direct:  `specifier: npm:beamcoder-prebuild@0.7.1`
            //   - catalog: `specifier: 'catalog:'` (the alias lives
            //              in `pnpm-workspace.yaml#catalog`)
            // The earlier `specifier.starts_with("npm:")` gate missed
            // the catalog flavor and silently dropped those deps.
            // Strip any peer suffix before parsing so `version:
            // 18.2.0(react@18.2.0)` (a regular dep with peers) does
            // not parse as `name="18.2.0(react"`.
            let bare_version = info
                .version
                .split('(')
                .next()
                .unwrap_or(info.version.as_str());
            let dep_path = if let Some((real_name, resolved)) = parse_dep_path(bare_version)
                && real_name != name
            {
                let peer_suffix = info
                    .version
                    .find('(')
                    .map(|i| &info.version[i..])
                    .unwrap_or("");
                let alias_dep_path = format!("{name}@{resolved}{peer_suffix}");
                let real_dep_path = info.version.clone();
                alias_remaps.push((
                    alias_dep_path.clone(),
                    real_dep_path,
                    name.to_string(),
                    real_name,
                ));
                alias_dep_path
            } else {
                version_to_dep_path(name, &info.version)
            };
            deps.push(DirectDep {
                name: name.to_string(),
                dep_path,
                dep_type,
                specifier: Some(info.specifier.clone()),
            });
        }
    };

    for (importer_path, importer) in &raw.importers {
        // pnpm writes the workspace root as either `'.'` (most
        // common / current) or `''` (seen on v9 lockfiles in the
        // wild, e.g. npmx.dev). Both mean "the repo root" — we key
        // the graph on `.` everywhere downstream (linker, filters,
        // stats), so normalize at parse time and keep the rest of
        // the pipeline single-shape.
        let importer_path = if importer_path.is_empty() {
            "."
        } else {
            importer_path.as_str()
        };

        // Guard against a malformed lockfile that writes both `''`
        // and `'.'` for root — `BTreeMap` iteration visits `''`
        // first, so the real `'.'` entry would otherwise silently
        // overwrite the normalized empty-key entry. pnpm never
        // emits this, but skipping the second visit is cheap and
        // makes the intent explicit.
        if importers.contains_key(importer_path) {
            continue;
        }

        let mut deps = Vec::new();

        if let Some(ref d) = importer.dependencies {
            for (name, info) in d {
                push_direct(
                    &mut deps,
                    &mut alias_remaps,
                    name,
                    info,
                    DepType::Production,
                );
            }
        }
        if let Some(ref d) = importer.dev_dependencies {
            for (name, info) in d {
                push_direct(&mut deps, &mut alias_remaps, name, info, DepType::Dev);
            }
        }
        if let Some(ref d) = importer.optional_dependencies {
            for (name, info) in d {
                push_direct(&mut deps, &mut alias_remaps, name, info, DepType::Optional);
            }
        }

        if let Some(ref d) = importer.skipped_optional_dependencies
            && !d.is_empty()
        {
            let mut map = BTreeMap::new();
            for (name, info) in d {
                map.insert(name.clone(), info.specifier.clone());
            }
            skipped_optional_dependencies.insert(importer_path.to_string(), map);
        }

        importers.insert(importer_path.to_string(), deps);
    }

    // pnpm v9 splits packages (canonical, keyed by `name@version`) from
    // snapshots (contextualized, keyed by the full dep_path with any
    // `(peer@ver)` suffix). The LockfileGraph needs one entry per snapshot
    // — the same canonical package can produce multiple snapshots when
    // different parts of the tree resolve its peers differently.
    //
    // If `snapshots:` is missing (older aube lockfiles where we wrote
    // everything into packages), fall back to iterating packages directly.
    let mut packages = BTreeMap::new();

    // Harvest snapshot dependencies for any local (`file:`) package
    // that showed up in the importers loop. The canonical snapshot
    // key for a local dep is `<name>@<specifier>` — e.g.
    // `foo@file:./vendor/foo` — so we construct it from each
    // synthesized entry and pull its `dependencies` block out of the
    // raw snapshots map.
    for local_pkg in local_packages.values_mut() {
        if let Some(ref local) = local_pkg.local_source {
            let canonical = format!("{}@{}", local_pkg.name, local.specifier());
            // URL-based direct deps have their peer-context suffix
            // stripped (see `push_direct`), but the matching snapshot
            // entry pnpm wrote still carries the suffix. Fall back to
            // any snapshot whose peer-stripped canonical matches so
            // transitive dependency metadata still flows through.
            let snap = raw.snapshots.get(&canonical).or_else(|| {
                raw.snapshots.iter().find_map(|(k, v)| {
                    parse_dep_path(k)
                        .filter(|(n, ver)| format!("{n}@{ver}") == canonical)
                        .map(|_| v)
                })
            });
            if let Some(snap) = snap
                && let Some(mut deps) = snap.dependencies.clone()
            {
                rewrite_snapshot_alias_deps(&mut deps, &mut alias_remaps);
                local_pkg.dependencies = deps;
            }
            if let Some(snap) = snap
                && let Some(mut opt_deps) = snap.optional_dependencies.clone()
            {
                rewrite_snapshot_alias_deps(&mut opt_deps, &mut alias_remaps);
                local_pkg.dependencies.extend(opt_deps.clone());
                local_pkg.optional_dependencies = opt_deps;
            }
            // Prefer the authoritative LocalSource classification
            // from the `resolution:` block over the guess the
            // importers loop made from the bare specifier. For git
            // deps, preserve any `path:` selector already captured
            // from the importer's `version:` URL — pnpm v9 encodes
            // the subpath in the snapshot key and doesn't always
            // echo it on the resolution block.
            let pkg_info = raw.packages.get(&canonical).or_else(|| match local {
                LocalSource::Git(git) => raw.packages.iter().find_map(|(key, pkg_info)| {
                    parse_dep_path(key)
                        .filter(|(name, _)| name == &local_pkg.name)
                        .and(pkg_info.resolution.as_ref())
                        .and_then(local_source_from_resolution)
                        .and_then(|candidate| match candidate {
                            LocalSource::Git(candidate)
                                if git_commits_match(&candidate.resolved, &git.resolved)
                                    && candidate.subpath == git.subpath =>
                            {
                                Some(pkg_info)
                            }
                            _ => None,
                        })
                }),
                LocalSource::RemoteTarball(tarball) => {
                    raw.packages.iter().find_map(|(key, pkg_info)| {
                        parse_dep_path(key)
                            .filter(|(name, _)| name == &local_pkg.name)
                            .and(pkg_info.resolution.as_ref())
                            .and_then(local_source_from_resolution)
                            .and_then(|candidate| match candidate {
                                LocalSource::RemoteTarball(candidate)
                                    if candidate.url == tarball.url =>
                                {
                                    Some(pkg_info)
                                }
                                _ => None,
                            })
                    })
                }
                _ => None,
            });
            if let Some(pkg_info) = pkg_info
                && let Some(ref res) = pkg_info.resolution
                && let Some(mut ls) = local_source_from_resolution(res)
            {
                if matches!(ls, LocalSource::Git(_) | LocalSource::RemoteTarball(_)) {
                    local_pkg.integrity = res.integrity.clone();
                }
                if let LocalSource::Git(ref mut g) = ls
                    && let Some(LocalSource::Git(prior)) = &local_pkg.local_source
                {
                    if git_commits_match(&g.resolved, &prior.resolved) {
                        g.resolved = prior.resolved.clone();
                    }
                    if g.subpath.is_none() {
                        g.subpath = prior.subpath.clone();
                    }
                }
                local_pkg.local_source = Some(ls);
            }
        }
    }
    // Rebuild keys in case the local_source rewrite above changed
    // the classification — kind alone doesn't affect the encoded
    // dep_path (the hash is over the path string only), but the
    // `resolution:` block can also hand us a *different path* than
    // the importer's specifier, which does. Recompute both the map
    // key and the struct field from the final `local_source` so
    // `graph.packages.get(&dep.dep_path)` stays consistent with how
    // DirectDeps were keyed up in the importer loop above. Resolution
    // metadata can refine the importer `version:` form (for example,
    // preserving a `git+ssh://` repo URL), so update DirectDeps for
    // any key that shifts during reclassification.
    let mut rekeyed: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut local_rekeys: BTreeMap<String, String> = BTreeMap::new();
    for (old_key, mut pkg) in local_packages {
        let new_key = pkg.local_source.as_ref().unwrap().dep_path(&pkg.name);
        pkg.dep_path = new_key.clone();
        if old_key != new_key {
            local_rekeys.insert(old_key, new_key.clone());
        }
        rekeyed.insert(new_key, pkg);
    }
    let local_packages = rekeyed;
    if !local_rekeys.is_empty() {
        for deps in importers.values_mut() {
            for dep in deps {
                if let Some(new_key) = local_rekeys.get(&dep.dep_path) {
                    dep.dep_path.clone_from(new_key);
                }
            }
        }
    }
    // Canonical keys the main loop should ignore — those are the
    // snapshot keys we already absorbed above.
    let local_canonical_keys: std::collections::HashSet<String> = local_packages
        .values()
        .filter_map(|p| {
            p.local_source
                .as_ref()
                .map(|l| format!("{}@{}", p.name, l.specifier()))
        })
        .collect();

    let snapshot_keys: Vec<String> = if raw.snapshots.is_empty() {
        raw.packages.keys().cloned().collect()
    } else {
        raw.snapshots.keys().cloned().collect()
    };

    for dep_path in snapshot_keys {
        if local_canonical_keys.contains(&dep_path) {
            continue;
        }
        let (name, version) = parse_dep_path(&dep_path)
            .ok_or_else(|| Error::parse(path, format!("invalid dep path: {dep_path}")))?;
        // URL-based direct deps are absorbed into `local_packages`
        // under the peer-stripped URL form (see `push_direct`), but the
        // snapshot key still carries any `(peer@ver)` suffix pnpm
        // appended. Check the peer-stripped canonical too so we don't
        // create a duplicate entry that round-trips as a stray
        // `packages:` block.
        if local_canonical_keys.contains(&format!("{name}@{version}")) {
            continue;
        }

        // Look up the canonical package entry by stripping any peer suffix.
        let canonical_key = version_to_dep_path(&name, &version);
        let pkg_info = raw
            .packages
            .get(&canonical_key)
            .or_else(|| raw.packages.get(&dep_path));

        let integrity = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.integrity.clone());

        // Registry packages record a `tarball:` URL only when
        // `lockfileIncludeTarballUrl=true` was active at write time.
        // Preserve it on read so the round-trip writes the same URL
        // back without having to reconsult the registry client.
        //
        // pnpm also writes a `tarball:` entry for non-registry transitive
        // deps whose key is a URL (remote tarball from a github override,
        // pkg.pr.new, etc.) — capture those on the same field so the
        // install path can fetch them verbatim instead of deriving a
        // registry URL that would 404.
        let tarball_url = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.tarball.as_ref())
            .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
            .cloned();
        let registry_git_hosted = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .is_some_and(|r| r.git_hosted);

        // pnpm writes `version: <semver>` alongside non-registry entries
        // whose dep-path key is a URL. Prefer that over the URL itself
        // when the dep-path version isn't a real semver — the install
        // path uses `pkg.version` for the store-content cross-check,
        // and comparing a URL to the tarball's declared `2.4.1` would
        // fail every github override'd package.
        //
        // Gated on `tarball_url.is_some()` so the swap only applies to
        // the remote-tarball case (where the URL is recoverable from
        // `resolution.tarball` at write time). `git+`/`git://` /
        // `.git#sha` transitive entries resolve through
        // `resolution: {type: git, commit, repo}` and keep the URL as
        // their version so install can fetch the git source.
        let version_is_http_url = version.starts_with("http://") || version.starts_with("https://");
        let dep_path_git_commit = git_commit_from_dep_path_version(&version).map(str::to_string);
        let version = if version_is_http_url && tarball_url.is_some() {
            pkg_info.and_then(|p| p.version.clone()).unwrap_or(version)
        } else {
            version
        };

        let snapshot = raw.snapshots.get(&dep_path);
        let mut optional_dependencies = snapshot
            .and_then(|s| s.optional_dependencies.clone())
            .unwrap_or_default();
        let mut dependencies = snapshot
            .and_then(|s| s.dependencies.clone())
            .unwrap_or_default();
        rewrite_snapshot_alias_deps(&mut dependencies, &mut alias_remaps);
        rewrite_snapshot_alias_deps(&mut optional_dependencies, &mut alias_remaps);
        dependencies.extend(optional_dependencies.clone());
        let bundled_dependencies = snapshot
            .and_then(|s| s.bundled_dependencies.clone())
            .unwrap_or_default();
        let optional = snapshot.and_then(|s| s.optional).unwrap_or(false);
        let transitive_peer_dependencies = snapshot
            .and_then(|s| s.transitive_peer_dependencies.clone())
            .unwrap_or_default();

        let peer_dependencies = pkg_info
            .and_then(|p| p.peer_dependencies.clone())
            .unwrap_or_default();
        let peer_dependencies_meta = pkg_info
            .and_then(|p| p.peer_dependencies_meta.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    PeerDepMeta {
                        optional: v.optional,
                    },
                )
            })
            .collect();
        let os = pkg_info.map(|p| p.os.clone()).unwrap_or_default();
        let cpu = pkg_info.map(|p| p.cpu.clone()).unwrap_or_default();
        let libc = pkg_info.map(|p| p.libc.clone()).unwrap_or_default();
        let engines = pkg_info.map(|p| p.engines.clone()).unwrap_or_default();
        // pnpm's lockfile only stores `hasBin: true/false` (no paths);
        // reconstruct an opaque single-entry map on parse so
        // `!bin.is_empty()` stays equivalent to `hasBin`, then let
        // downstream writers fill in real paths when they have them.
        // The map key + value are placeholders — writers that care
        // about bin names (bun) read from richer sources.
        let bin = if pkg_info.map(|p| p.has_bin).unwrap_or(false) {
            let mut m = BTreeMap::new();
            m.insert(String::new(), String::new());
            m
        } else {
            BTreeMap::new()
        };
        // Aube-specific extension (see `WritablePackageInfo::alias_of`)
        // — ordinary pnpm lockfiles never carry it, so this stays
        // `None` on pnpm-authored input and round-trips the resolver-
        // emitted value for aliased packages.
        let alias_of = pkg_info.and_then(|p| p.alias_of.clone());

        // Reclassify transitive URL-keyed entries — github forks,
        // pkg.pr.new, `file:` targets — so they round-trip with the
        // right `local_source`. Without this, the install path sees
        // `local_source: None` + a URL-form version and tries to
        // fetch the dep from the npm registry (404).
        let local_source = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(local_source_from_resolution);
        let local_source = local_source.map(|mut source| {
            if let LocalSource::Git(git) = &mut source
                && let Some(commit) = dep_path_git_commit.as_deref()
                && git_commits_match(&git.resolved, commit)
            {
                git.resolved = commit.to_string();
            }
            source
        });
        // `lockfileIncludeTarballUrl` puts registry tarball URLs on
        // ordinary `name@version` entries; only URL-keyed entries are
        // true remote-tarball deps.
        let local_source = match local_source {
            Some(LocalSource::RemoteTarball(_)) if !version_is_http_url => None,
            other => other,
        };
        if integrity.is_none()
            && resolution_requires_integrity(
                pkg_info.and_then(|p| p.resolution.as_ref()),
                &local_source,
            )
        {
            return Err(Error::parse(
                path,
                format!(
                    "lockfile entry {dep_path:?} has a remote tarball resolution without integrity"
                ),
            ));
        }

        packages.insert(
            dep_path.clone(),
            LockedPackage {
                name,
                version,
                integrity,
                dependencies,
                optional_dependencies,
                peer_dependencies,
                peer_dependencies_meta,
                dep_path,
                local_source,
                os: os.into(),
                cpu: cpu.into(),
                libc: libc.into(),
                bundled_dependencies,
                optional,
                transitive_peer_dependencies,
                tarball_url,
                registry_git_hosted,
                alias_of,
                yarn_checksum: None,
                engines,
                bin,
                // pnpm's `snapshots:` only records resolved pins, so
                // the parser has no declared ranges to restore. Left
                // empty; npm / yarn / bun writers fall back to pins
                // when re-emitting a pnpm-sourced graph into one of
                // their formats.
                declared_dependencies: BTreeMap::new(),
                // pnpm's format doesn't carry per-package license or
                // funding metadata, so a pnpm → npm conversion
                // degrades to empty rather than re-fetching each
                // packument. npm writers skip these fields when
                // `None`.
                license: None,
                funding_url: None,
                extra_meta: BTreeMap::new(),
            },
        );
    }

    // Synthesize alias-keyed LockedPackages for npm-aliased importer
    // deps. pnpm v9 only writes the canonical (real-name-keyed) entry
    // in `packages:`; we clone it under the alias dep_path with
    // `name=alias` and `alias_of=Some(real)` so the linker — which
    // already supports this shape via the resolver-fresh path — can
    // create `node_modules/<alias>` symlinks correctly.
    for (alias_dep_path, real_dep_path, alias_name, real_name) in alias_remaps {
        // Skip if the alias entry already exists (aube-written
        // lockfile that emitted both `aliasOf:` and an alias-keyed
        // packages entry).
        if packages.contains_key(&alias_dep_path) {
            continue;
        }
        let Some(real_pkg) = packages
            .get(&real_dep_path)
            .or_else(|| peerless_alias_target(&packages, &real_dep_path))
        else {
            return Err(Error::parse(
                path,
                format!(
                    "npm-alias references missing package {real_dep_path} (alias dep_path: {alias_dep_path})"
                ),
            ));
        };
        let mut aliased = real_pkg.clone();
        aliased.name = alias_name;
        aliased.dep_path = alias_dep_path.clone();
        aliased.alias_of = Some(real_name);
        packages.insert(alias_dep_path, aliased);
    }

    for (k, v) in local_packages {
        packages.insert(k, v);
    }

    let settings = raw
        .settings
        .map(|s| crate::LockfileSettings {
            auto_install_peers: s.auto_install_peers.unwrap_or(true),
            exclude_links_from_lockfile: s.exclude_links_from_lockfile.unwrap_or(false),
            lockfile_include_tarball_url: s.lockfile_include_tarball_url.unwrap_or(false),
        })
        .unwrap_or_default();

    let times = raw.time.unwrap_or_default();

    let catalogs = raw
        .catalogs
        .unwrap_or_default()
        .into_iter()
        .map(|(name, entries)| {
            let inner = entries
                .into_iter()
                .map(|(pkg, e)| {
                    (
                        pkg,
                        CatalogEntry {
                            specifier: e.specifier,
                            version: e.version,
                        },
                    )
                })
                .collect();
            (name, inner)
        })
        .collect();

    let patched_dependencies: BTreeMap<String, String> = raw
        .patched_dependencies
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, v.into_path()))
        .collect();

    Ok(LockfileGraph {
        importers,
        packages,
        settings,
        overrides: raw.overrides.unwrap_or_default(),
        ignored_optional_dependencies: raw
            .ignored_optional_dependencies
            .unwrap_or_default()
            .into_iter()
            .collect(),
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version: None,
        patched_dependencies,
        trusted_dependencies: Vec::new(),
        extra_fields: BTreeMap::new(),
        workspace_extra_fields: BTreeMap::new(),
    })
}

fn resolution_requires_integrity(
    resolution: Option<&super::raw::Resolution>,
    local_source: &Option<LocalSource>,
) -> bool {
    let Some(resolution) = resolution else {
        return false;
    };
    if resolution.integrity.is_some() || resolution.git_hosted {
        return false;
    }
    match local_source {
        Some(LocalSource::Tarball(_))
        | Some(LocalSource::Directory(_))
        | Some(LocalSource::Link(_))
        | Some(LocalSource::Portal(_))
        | Some(LocalSource::Git(_))
        | Some(LocalSource::Exec(_)) => false,
        Some(LocalSource::RemoteTarball(t)) => {
            !t.git_hosted && !super::tarball_url_is_hosted_git(&t.url)
        }
        None => resolution
            .tarball
            .as_deref()
            .is_some_and(|t| is_http_url(t) && !super::tarball_url_is_hosted_git(t)),
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn git_commit_from_dep_path_version(version: &str) -> Option<&str> {
    let (_, fragment) = version.rsplit_once('#')?;
    let commit = fragment.split('&').next().unwrap_or(fragment);
    if commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(commit)
    } else {
        None
    }
}
