use super::dep_path::{dep_path_tail, parse_dep_path, peerless_dep_path, version_to_dep_path};
use super::format::reformat_for_pnpm_parity;
use crate::{DepType, Error, LocalSource, LockfileGraph};
use aube_manifest::PackageJson;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Write a LockfileGraph as pnpm-lock.yaml v9 format.
pub fn write(path: &Path, graph: &LockfileGraph, manifest: &PackageJson) -> Result<(), Error> {
    let native_pnpm_aliases = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "pnpm-lock.yaml");
    let mut importers = BTreeMap::new();
    let exclude_links = graph.settings.exclude_links_from_lockfile;
    for (importer_path, deps) in &graph.importers {
        let mut importer = WritableImporter::default();

        for dep in deps {
            // `excludeLinksFromLockfile: true` drops `link:` entries
            // from importer dep maps so a sibling-workspace symlink
            // change doesn't churn the lockfile. We check the package
            // table rather than `dep.specifier` because the importer's
            // DirectDep only carries the manifest-written range, not
            // the resolved source kind — the LocalSource lives on the
            // LockedPackage the dep_path points to.
            if exclude_links
                && matches!(
                    graph
                        .packages
                        .get(&dep.dep_path)
                        .and_then(|p| p.local_source.as_ref()),
                    Some(LocalSource::Link(_))
                )
            {
                continue;
            }
            // Specifier sources, in priority order:
            //   1. The specifier recorded on the DirectDep. For workspace
            //      importers this is the only manifest-local specifier the
            //      writer has, because `manifest` is the root package.json.
            //      Hoisted auto-installed peers also use this path.
            //   2. The root manifest entry for old hand-built graphs that
            //      omitted DirectDep.specifier.
            //   3. Fall back to `*` as a last resort.
            let root_manifest_specifier = (importer_path == ".")
                .then(|| match dep.dep_type {
                    DepType::Production => manifest.dependencies.get(&dep.name),
                    DepType::Dev => manifest.dev_dependencies.get(&dep.name),
                    DepType::Optional => manifest.optional_dependencies.get(&dep.name),
                })
                .flatten()
                .map(|s| s.as_str());
            let specifier = dep
                .specifier
                .as_deref()
                .or(root_manifest_specifier)
                .unwrap_or("*");

            // Local deps render with the canonical `file:<path>` /
            // `link:<path>` specifier, not the FS-safe encoded form
            // that lives in `dep_path`.
            let version = if let Some(local) = graph
                .packages
                .get(&dep.dep_path)
                .and_then(|p| p.local_source.as_ref())
            {
                local.specifier()
            } else if native_pnpm_aliases
                && let Some(pkg) = graph.packages.get(&dep.dep_path)
                && let Some(real_name) = pkg.alias_of.as_deref()
            {
                format!("{real_name}@{}", dep_path_tail(&dep.dep_path, &dep.name))
            } else {
                dep.dep_path
                    .strip_prefix(&format!("{}@", dep.name))
                    .unwrap_or(&dep.dep_path)
                    .to_string()
            };

            let spec = WritableDepSpec {
                specifier: specifier.to_string(),
                version,
            };

            match dep.dep_type {
                DepType::Production => {
                    importer
                        .dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
                DepType::Dev => {
                    importer
                        .dev_dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
                DepType::Optional => {
                    importer
                        .optional_dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
            }
        }

        // Runtime pins render as synthetic deps on the root importer
        // (pnpm 10.14+ shape): `node: {specifier: runtime:^24.4.0,
        // version: runtime:24.4.1}`. Only the root carries them — the
        // pin comes from the root manifest's devEngines.
        if importer_path == "." {
            for (name, pin) in &graph.runtimes {
                let spec = WritableDepSpec {
                    specifier: format!("runtime:{}", pin.specifier),
                    version: format!("runtime:{}", pin.version),
                };
                let slot = if pin.dev {
                    importer.dev_dependencies.get_or_insert_with(BTreeMap::new)
                } else {
                    importer.dependencies.get_or_insert_with(BTreeMap::new)
                };
                slot.insert(name.clone(), spec);
            }
        }

        if let Some(skipped) = graph.skipped_optional_dependencies.get(importer_path)
            && !skipped.is_empty()
        {
            let mut map: BTreeMap<String, WritableDepSpec> = BTreeMap::new();
            for (name, specifier) in skipped {
                map.insert(
                    name.clone(),
                    WritableDepSpec {
                        specifier: specifier.clone(),
                        // No installed version on this platform — use a
                        // sentinel that's still parseable as a dep_path
                        // tail by `parse_dep_path` if older code happens
                        // to walk it.
                        version: "0.0.0".to_string(),
                    },
                );
            }
            importer.skipped_optional_dependencies = Some(map);
        }

        importers.insert(importer_path.clone(), importer);
    }

    // pnpm v9 splits the lockfile into two sections:
    //   `packages:` — keyed by the canonical `name@version` (no peer suffix),
    //                 holds the integrity hash and declared peer deps. The
    //                 same package-version with two different peer contexts
    //                 dedupes to a single entry here.
    //   `snapshots:` — keyed by the full contextualized dep_path including
    //                  any `(peer@ver)` suffix, holds the resolved
    //                  `dependencies:` map that the linker walks.
    //
    // We dedupe the packages map via BTreeMap::insert so repeated canonical
    // keys (one per peer context) collapse cleanly, and we take the last
    // writer's integrity/peer decls — they should all agree because they
    // come from the same canonical package.
    let mut packages = BTreeMap::new();
    for pkg in graph.packages.values() {
        // Local deps use the canonical specifier in their key (e.g.
        // `foo@file:./vendor/foo`) so pnpm can read the lockfile.
        // `link:` deps are omitted from the packages section entirely,
        // matching pnpm. `exec:` has no pnpm resolution analogue, so
        // keep it out too instead of writing a package key with no
        // resolution block.
        // Non-registry transitive entries (github overrides, remote
        // tarballs fetched by URL) keep the URL in their dep-path key
        // and carry the real semver on `pkg.version`. `tarball_url`
        // carries the URL through the graph — when the dep-path's
        // version segment is that same URL, the entry was parsed from
        // a URL-keyed pnpm snapshot and needs to round-trip under the
        // same URL key. Paired with the parser's `version_is_http_url
        // && tarball_url.is_some()` gate.
        let url_keyed = pkg
            .tarball_url
            .as_ref()
            .is_some_and(|url| parse_dep_path(&pkg.dep_path).is_some_and(|(_, v)| v == *url));
        let canonical = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) | Some(LocalSource::Exec(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => {
                if native_pnpm_aliases && let Some(real_name) = pkg.alias_of.as_deref() {
                    version_to_dep_path(real_name, &pkg.version)
                } else if url_keyed {
                    // Strip any peer suffix; the packages section keys the
                    // canonical form (no peer contexts), the snapshots
                    // section keys the full dep_path.
                    let (name, version) = parse_dep_path(&pkg.dep_path)
                        .unwrap_or_else(|| (pkg.name.clone(), pkg.version.clone()));
                    format!("{name}@{version}")
                } else {
                    version_to_dep_path(&pkg.name, &pkg.version)
                }
            }
        };
        let peer_deps = if pkg.peer_dependencies.is_empty() {
            None
        } else {
            Some(pkg.peer_dependencies.clone())
        };
        let peer_meta = if pkg.peer_dependencies_meta.is_empty() {
            None
        } else {
            Some(
                pkg.peer_dependencies_meta
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            WritablePeerDepMeta {
                                optional: v.optional,
                            },
                        )
                    })
                    .collect(),
            )
        };
        // Always render the path through `path_posix()` so the
        // lockfile uses forward slashes regardless of the host OS —
        // a lockfile written on Windows must resolve identically on
        // Unix and vice versa. `Path::display()` honors the host
        // separator, so it would leak `\` into the YAML.
        let is_jsr_registry_pkg = pkg.registry_name().starts_with("@jsr/");
        let preserve_tarball_url = graph.settings.lockfile_include_tarball_url
            || is_jsr_registry_pkg
            || registry_tarball_url_is_not_derivable(
                pkg.registry_name(),
                &pkg.version,
                pkg.tarball_url.as_deref(),
            );
        debug_assert!(
            !is_jsr_registry_pkg || pkg.tarball_url.is_some(),
            "JSR packages must preserve dist.tarball for cold lockfile installs"
        );
        let resolution = match pkg.local_source.as_ref() {
            Some(local @ LocalSource::Directory(_)) => Some(WritableResolution {
                integrity: None,
                git_hosted: false,
                directory: Some(local.path_posix()),
                tarball: None,
                commit: None,
                repo: None,
                type_: Some("directory".to_string()),
                path: None,
                variants: None,
            }),
            Some(local @ LocalSource::Tarball(_)) => Some(WritableResolution {
                integrity: None,
                git_hosted: false,
                directory: None,
                tarball: Some(format!("file:{}", local.path_posix())),
                commit: None,
                repo: None,
                type_: None,
                path: None,
                variants: None,
            }),
            Some(LocalSource::Link(_)) | Some(LocalSource::Exec(_)) => None,
            Some(local @ LocalSource::Portal(_)) => Some(WritableResolution {
                integrity: None,
                git_hosted: false,
                directory: Some(local.path_posix()),
                tarball: None,
                commit: None,
                repo: None,
                type_: Some("directory".to_string()),
                path: None,
                variants: None,
            }),
            Some(LocalSource::Git(g)) => Some(WritableResolution {
                integrity: g.integrity.clone().or_else(|| pkg.integrity.clone()),
                git_hosted: crate::parse_hosted_git(&g.url).is_some(),
                directory: None,
                tarball: None,
                commit: Some(g.resolved.clone()),
                repo: Some(g.url.clone()),
                type_: Some("git".to_string()),
                // pnpm v9 emits `path: /<sub>` (with leading `/`) on
                // the resolution block when a git dep was installed
                // with a `&path:/<sub>` selector. Keep the same shape
                // so byte-identical round-trips survive.
                path: g.subpath.as_ref().map(|s| format!("/{s}")),
                variants: None,
            }),
            Some(LocalSource::RemoteTarball(t)) => Some(WritableResolution {
                integrity: if t.integrity.is_empty() {
                    None
                } else {
                    Some(t.integrity.clone())
                },
                git_hosted: t.git_hosted || super::tarball_url_is_hosted_git(&t.url),
                directory: None,
                tarball: Some(t.url.clone()),
                commit: None,
                repo: None,
                type_: None,
                path: None,
                variants: None,
            }),
            None if url_keyed => {
                // URL-keyed transitive entries (github overrides, etc.)
                // typically carry no integrity — just the tarball URL
                // in `resolution:`. Gating on `pkg.integrity` would
                // silently drop the tarball on round-trip, and a
                // re-parse would then have no way to fetch the package.
                Some(WritableResolution {
                    integrity: pkg.integrity.clone(),
                    git_hosted: pkg.registry_git_hosted
                        || pkg
                            .tarball_url
                            .as_deref()
                            .is_some_and(super::tarball_url_is_hosted_git),
                    directory: None,
                    tarball: pkg.tarball_url.clone(),
                    commit: None,
                    repo: None,
                    type_: None,
                    path: None,
                    variants: None,
                })
            }
            None if pkg.integrity.is_some() || preserve_tarball_url => Some(WritableResolution {
                integrity: pkg.integrity.clone(),
                git_hosted: pkg.registry_git_hosted
                    || pkg
                        .tarball_url
                        .as_deref()
                        .is_some_and(super::tarball_url_is_hosted_git),
                directory: None,
                // Emit the full registry tarball URL when the setting
                // opts in. JSR packages are the exception: npm.jsr.io
                // uses opaque `dist.tarball` paths that cannot be
                // reconstructed from package name + version, so the
                // URL must be preserved for cold installs from the
                // lockfile.
                tarball: if preserve_tarball_url {
                    pkg.tarball_url.clone()
                } else {
                    None
                },
                commit: None,
                repo: None,
                type_: None,
                path: None,
                variants: None,
            }),
            None => None,
        };
        // Mirror pnpm: emit `version:` alongside the resolution block
        // for URL-keyed transitive entries so tooling that matches
        // packages by (name, version) still has a handle on the real
        // semver. Ordinary registry entries skip this — the key already
        // carries the version, and adding a field would diverge from
        // byte-for-byte pnpm output.
        let write_version = url_keyed.then(|| pkg.version.clone());
        packages.insert(
            canonical,
            WritablePackageInfo {
                resolution,
                version: write_version,
                engines: if pkg.engines.is_empty() {
                    None
                } else {
                    Some(pkg.engines.clone())
                },
                os: pkg.os.to_vec(),
                cpu: pkg.cpu.to_vec(),
                libc: pkg.libc.to_vec(),
                has_bin: !pkg.bin.is_empty(),
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_meta,
                alias_of: (!native_pnpm_aliases)
                    .then(|| pkg.alias_of.clone())
                    .flatten(),
            },
        );
    }

    // Runtime pin packages entries: `node@runtime:24.4.1` with a
    // `variations` resolution carrying one binary artifact per
    // platform (pnpm 10.14+ shape). The matching snapshot entry is
    // empty — runtimes have no dependencies.
    for (name, pin) in &graph.runtimes {
        let variants: Vec<WritableRuntimeVariant> = pin
            .variants
            .iter()
            .map(|v| WritableRuntimeVariant {
                resolution: WritableRuntimeBinaryResolution {
                    archive: v.archive.clone(),
                    bin: if v.bin_is_bare_string && v.bin.len() == 1 {
                        WritableRuntimeBin::Single(
                            v.bin
                                .values()
                                .next()
                                .expect("bin.len() == 1 checked above")
                                .clone(),
                        )
                    } else {
                        WritableRuntimeBin::Map(v.bin.clone())
                    },
                    integrity: if v.integrity.is_empty() {
                        None
                    } else {
                        Some(v.integrity.clone())
                    },
                    prefix: v.prefix.clone(),
                    type_: "binary",
                    url: v.url.clone(),
                },
                targets: v
                    .targets
                    .iter()
                    .map(|t| WritableRuntimeTarget {
                        cpu: t.cpu.clone(),
                        libc: t.libc.clone(),
                        os: t.os.clone(),
                    })
                    .collect(),
            })
            .collect();
        packages.insert(
            format!("{name}@runtime:{}", pin.version),
            WritablePackageInfo {
                resolution: Some(WritableResolution {
                    integrity: None,
                    git_hosted: false,
                    directory: None,
                    tarball: None,
                    commit: None,
                    repo: None,
                    type_: Some("variations".to_string()),
                    path: None,
                    variants: Some(variants),
                }),
                version: Some(pin.version.clone()),
                engines: None,
                os: Vec::new(),
                cpu: Vec::new(),
                libc: Vec::new(),
                has_bin: pin.has_bin,
                peer_dependencies: None,
                peer_dependencies_meta: None,
                alias_of: None,
            },
        );
    }

    // Translate internal dep_path tails (`git+<hash>`, `url+<hash>`,
    // `file+<hash>`) to the specifier form pnpm expects in snapshot
    // dependency maps (`<url>#<sha>` for git, raw URL for tarball,
    // `file:<path>` for local). Registry deps keep their plain semver
    // values. The target package's `local_source` is authoritative:
    // the tail alone doesn't encode the URL.
    let rewrite_local_deps = |deps: BTreeMap<String, String>| -> BTreeMap<String, String> {
        deps.into_iter()
            .map(|(name, value)| {
                let dp = version_to_dep_path(&name, &value);
                let target = graph
                    .packages
                    .get(&dp)
                    .or_else(|| graph.packages.get(&peerless_dep_path(&name, &value)));
                if let Some(target) = target
                    && let Some(ref local) = target.local_source
                    && !matches!(local, LocalSource::Link(_))
                {
                    (name, local.specifier())
                } else if native_pnpm_aliases
                    && let Some(target) = target
                    && let Some(real_name) = target.alias_of.as_deref()
                {
                    (name, format!("{real_name}@{value}"))
                } else {
                    (name, value)
                }
            })
            .collect()
    };
    let mut snapshots = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        // `link:` deps are omitted from snapshots (pnpm parity). `exec:`
        // is omitted for the same reason it is omitted from packages:
        // pnpm has no resolution shape for generated packages.
        // Other local deps use the canonical specifier key so pnpm's
        // parser lines them up with the packages entry above.
        let key = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) | Some(LocalSource::Exec(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => {
                if native_pnpm_aliases && let Some(real_name) = pkg.alias_of.as_deref() {
                    format!("{real_name}@{}", dep_path_tail(dep_path, &pkg.name))
                } else {
                    dep_path.clone()
                }
            }
        };
        let pkg_deps = rewrite_local_deps(pkg.dependencies.clone());
        let pkg_opt_deps = rewrite_local_deps(pkg.optional_dependencies.clone());
        snapshots.insert(
            key,
            WritableSnapshot {
                dependencies: {
                    let mut deps = pkg_deps;
                    for name in pkg_opt_deps.keys() {
                        deps.remove(name);
                    }
                    if deps.is_empty() { None } else { Some(deps) }
                },
                optional_dependencies: if pkg_opt_deps.is_empty() {
                    None
                } else {
                    Some(pkg_opt_deps)
                },
                transitive_peer_dependencies: if pkg.transitive_peer_dependencies.is_empty() {
                    None
                } else {
                    Some(pkg.transitive_peer_dependencies.clone())
                },
                optional: if pkg.optional { Some(true) } else { None },
                bundled_dependencies: if pkg.bundled_dependencies.is_empty() {
                    None
                } else {
                    Some(pkg.bundled_dependencies.clone())
                },
            },
        );
    }

    // Empty snapshot entries for runtime pins (`node@runtime:24.4.1: {}`),
    // matching pnpm's writer.
    for (name, pin) in &graph.runtimes {
        snapshots.insert(
            format!("{name}@runtime:{}", pin.version),
            WritableSnapshot {
                dependencies: None,
                optional_dependencies: None,
                transitive_peer_dependencies: None,
                optional: None,
                bundled_dependencies: None,
            },
        );
    }

    let time = pruned_time_entries(graph, native_pnpm_aliases);

    let catalogs = if graph.catalogs.is_empty() {
        None
    } else {
        Some(
            graph
                .catalogs
                .iter()
                .map(|(name, entries)| {
                    let inner: BTreeMap<String, WritableCatalogEntry> = entries
                        .iter()
                        .map(|(pkg, e)| {
                            (
                                pkg.clone(),
                                WritableCatalogEntry {
                                    specifier: e.specifier.clone(),
                                    version: e.version.clone(),
                                },
                            )
                        })
                        .collect();
                    (name.clone(), inner)
                })
                .collect(),
        )
    };

    let lockfile = WritablePnpmLockfile {
        lockfile_version: "9.0".to_string(),
        settings: WritableSettings {
            auto_install_peers: graph.settings.auto_install_peers,
            exclude_links_from_lockfile: graph.settings.exclude_links_from_lockfile,
            lockfile_include_tarball_url: graph.settings.lockfile_include_tarball_url,
        },
        catalogs,
        // Skipped at serialization time when empty so the YAML stays
        // byte-identical to a no-overrides install.
        overrides: if graph.overrides.is_empty() {
            None
        } else {
            Some(graph.overrides.clone())
        },
        // Already `sha256-`-prefixed (or `None`) on the graph; emitted
        // verbatim. pnpm omits these when absent, and `skip_serializing_if`
        // mirrors that.
        package_extensions_checksum: graph.package_extensions_checksum.clone(),
        pnpmfile_checksum: graph.pnpmfile_checksum.clone(),
        ignored_optional_dependencies: if graph.ignored_optional_dependencies.is_empty() {
            None
        } else {
            Some(
                graph
                    .ignored_optional_dependencies
                    .iter()
                    .cloned()
                    .collect(),
            )
        },
        // pnpm v9 emits patched deps as `{ path, hash }`. We don't
        // track the patch hash on the graph (install-time concern),
        // so write the path form which pnpm still accepts. Skipped
        // when empty to keep parity with no-patch installs.
        patched_dependencies: if graph.patched_dependencies.is_empty() {
            None
        } else {
            Some(graph.patched_dependencies.clone())
        },
        time,
        importers,
        packages,
        snapshots,
    };

    let yaml = yaml_serde::to_string(&lockfile).map_err(|e| Error::parse(path, e.to_string()))?;
    let yaml = reformat_for_pnpm_parity(&yaml);
    // Atomic via tempfile + persist. Crash, Ctrl+C, or AV
    // quarantine during the write used to leave the user with a
    // truncated pnpm-lock.yaml on disk, next install failed to
    // parse and the user thought their lockfile was gone. See
    // atomic_write_lockfile for full rationale.
    crate::atomic_write_lockfile(path, yaml.as_bytes())?;
    Ok(())
}

fn registry_tarball_url_is_not_derivable(
    name: &str,
    version: &str,
    tarball_url: Option<&str>,
) -> bool {
    let Some(url) = tarball_url else {
        return false;
    };
    let basename = name.rsplit('/').next().unwrap_or(name);
    let expected_suffix = format!("/-/{basename}-{version}.tgz");
    let path_only = url.split_once('?').map_or(url, |(path, _)| path);
    let path_only = path_only
        .split_once('#')
        .map_or(path_only, |(path, _)| path);
    !path_only.ends_with(&expected_suffix)
}

fn pruned_time_entries(
    graph: &LockfileGraph,
    native_pnpm_aliases: bool,
) -> Option<BTreeMap<String, String>> {
    if graph.times.is_empty() {
        return None;
    }

    let mut time = BTreeMap::new();
    for deps in graph.importers.values() {
        for dep in deps {
            let Some(pkg) = graph.packages.get(&dep.dep_path) else {
                tracing::debug!(
                    dep_path = %dep.dep_path,
                    "direct importer dep missing from package table while pruning pnpm time entries"
                );
                continue;
            };
            if pkg.local_source.is_some() {
                continue;
            }
            let name = if native_pnpm_aliases {
                pkg.alias_of.as_deref().unwrap_or(dep.name.as_str())
            } else {
                dep.name.as_str()
            };
            let tail = dep_path_tail(&dep.dep_path, &dep.name);
            let version = tail.split('(').next().unwrap_or(tail);
            let key = version_to_dep_path(name, version);
            let internal_key = version_to_dep_path(&dep.name, version);
            let value = graph
                .times
                .get(&key)
                .or_else(|| graph.times.get(&internal_key))
                .or_else(|| {
                    (!native_pnpm_aliases)
                        .then_some(pkg.alias_of.as_deref())
                        .flatten()
                        .and_then(|real_name| {
                            graph.times.get(&version_to_dep_path(real_name, version))
                        })
                });
            if let Some(value) = value {
                time.insert(key, value.clone());
            }
        }
    }

    (!time.is_empty()).then_some(time)
}

// -- Writable serde types for pnpm-lock.yaml v9 --

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePnpmLockfile {
    lockfile_version: String,
    settings: WritableSettings,
    // pnpm v9 places `overrides:` immediately after `settings:` and
    // before `importers:`. Field order matters because we serialize
    // through yaml_serde and want byte-for-byte parity with pnpm output
    // for the no-overrides case (the field is skipped when empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    overrides: Option<BTreeMap<String, String>>,
    /// pnpm v9's top-level `packageExtensionsChecksum:` — emitted right
    /// after `overrides:` (and before `patchedDependencies:`) when the
    /// effective config declares any `packageExtensions`. Already
    /// carries pnpm's `sha256-` prefix. Skipped when absent so a
    /// no-extensions install stays byte-identical to pnpm.
    #[serde(skip_serializing_if = "Option::is_none")]
    package_extensions_checksum: Option<String>,
    /// pnpm v9's top-level `pnpmfileChecksum:` — emitted immediately
    /// after `packageExtensionsChecksum:` when a local pnpmfile
    /// participates. Skipped when absent for byte-identical output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pnpmfile_checksum: Option<String>,
    /// pnpm v9+ top-level `patchedDependencies:` — preserved so a
    /// bun→aube-lock conversion keeps the user's patches and a
    /// re-emit doesn't strip the block. pnpm emits this block right
    /// after `overrides:` and before `catalogs:`, so the field order
    /// here follows the same sequence for byte-identical output.
    #[serde(skip_serializing_if = "Option::is_none")]
    patched_dependencies: Option<BTreeMap<String, String>>,
    /// pnpm v9 emits a top-level `catalogs:` map after
    /// `overrides:` and before `importers:` when `pnpm-workspace.yaml`
    /// declares any referenced catalog entries.
    /// Skipped when empty so a no-catalogs install stays byte-identical
    /// to pnpm output.
    #[serde(skip_serializing_if = "Option::is_none")]
    catalogs: Option<BTreeMap<String, BTreeMap<String, WritableCatalogEntry>>>,
    /// pnpm v9 emits a top-level `time:` map when `resolution-mode=time-based`
    /// is active. Keyed by canonical `name@version`; values are ISO-8601
    /// publish timestamps pulled from the registry packument. Placed
    /// after `overrides:` and before `importers:` to match pnpm's
    /// field order.
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<BTreeMap<String, String>>,
    importers: BTreeMap<String, WritableImporter>,
    packages: BTreeMap<String, WritablePackageInfo>,
    /// pnpm v9 emits a top-level `ignoredOptionalDependencies:` array
    /// after `packages:` and before `snapshots:` when the root
    /// manifest's `pnpm.ignoredOptionalDependencies` is non-empty.
    /// Skipped when empty so a no-ignored install stays byte-for-byte
    /// identical to pnpm's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignored_optional_dependencies: Option<Vec<String>>,
    snapshots: BTreeMap<String, WritableSnapshot>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSettings {
    auto_install_peers: bool,
    exclude_links_from_lockfile: bool,
    /// Skipped at serialization time when false so pnpm-parity
    /// projects that don't opt into the tarball-URL recording keep
    /// byte-identical lockfiles.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    lockfile_include_tarball_url: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableImporter {
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dev_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    /// Optionals the resolver intentionally skipped on this importer's
    /// platform — round-tripped so drift detection can distinguish
    /// "previously skipped" from "newly added". Aube-specific extension
    /// to pnpm v9's importer schema; the field is omitted when empty so
    /// no-skip projects stay byte-identical to pnpm output.
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_optional_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
}

#[derive(Debug, Serialize)]
struct WritableDepSpec {
    specifier: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct WritableCatalogEntry {
    specifier: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct WritableResolution {
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not", rename = "gitHosted")]
    git_hosted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tarball: Option<String>,
    // Git resolution fields (pnpm v9 `{type: git, repo, commit}` form).
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    type_: Option<String>,
    /// pnpm `&path:/<sub>` selector — emitted with leading `/` to
    /// match pnpm's own writer.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// `type: variations` artifact list for runtime pins. `None` for
    /// every ordinary package resolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    variants: Option<Vec<WritableRuntimeVariant>>,
}

/// One `variants:` entry of a runtime pin's `variations` resolution.
/// Field order is alphabetical (`resolution` before `targets`),
/// matching pnpm's sorted-key lockfile emitter.
#[derive(Debug, Serialize)]
struct WritableRuntimeVariant {
    resolution: WritableRuntimeBinaryResolution,
    targets: Vec<WritableRuntimeTarget>,
}

/// pnpm `BinaryResolution` — alphabetical field order to match pnpm's
/// sorted-key emitter.
#[derive(Debug, Serialize)]
struct WritableRuntimeBinaryResolution {
    archive: String,
    bin: WritableRuntimeBin,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(rename = "type")]
    type_: &'static str,
    url: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WritableRuntimeBin {
    /// Bare-string form (`bin: bin/node`) — a single executable named
    /// after the runtime itself.
    Single(String),
    Map(BTreeMap<String, String>),
}

#[derive(Debug, Serialize)]
struct WritableRuntimeTarget {
    cpu: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    libc: Option<String>,
    os: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePeerDepMeta {
    // pnpm v9 omits `optional: false` entirely; only the truthy form
    // shows up in real-world lockfiles. Skip the default so we stay
    // byte-identical for the rare case where a packument explicitly
    // marks a peer as non-optional.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
}

// Field order matches pnpm v9's `packages:` entries: resolution, then
// engines, then os/cpu/libc, then hasBin, then peerDependencies /
// peerDependenciesMeta. Don't reorder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePackageInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<WritableResolution>,
    /// Real semver for non-registry entries (remote tarball / git),
    /// where the dep-path key is a URL rather than a version. pnpm
    /// emits this field so tooling that reads lockfile entries by
    /// `(name, version)` still finds the right semver. Omitted for
    /// ordinary registry entries — the version lives in the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// pnpm writes `engines: {node: '>=8'}` in flow form immediately
    /// after `resolution:` when the package declared any engines.
    /// Emitted as a block map here — `reformat_for_pnpm_parity` flips it
    /// to flow form to match pnpm byte-for-byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    engines: Option<BTreeMap<String, String>>,
    // pnpm v9 emits os/cpu/libc after `engines` and before `hasBin`.
    // Keep this order to stay byte-identical with pnpm-written lockfiles
    // for native packages.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cpu: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    libc: Vec<String>,
    /// pnpm emits `hasBin: true` only when the package has executables;
    /// `hasBin: false` is never written. Skip the default to match.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    has_bin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies_meta: Option<BTreeMap<String, WritablePeerDepMeta>>,
    /// Real registry name for npm-alias deps. Aube-specific extension
    /// (pnpm encodes aliases in the snapshot key itself — e.g.
    /// `odd-alias@npm:is-odd@3.0.1` — but aube keys by `alias@version`
    /// for linker simplicity, so the real name has to round-trip
    /// out-of-band via this field). Omitted for non-aliased packages
    /// so non-alias lockfiles stay byte-identical to pnpm's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    alias_of: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSnapshot {
    // Order mirrors pnpm's `LockfilePackageSnapshot` emit order
    // (dependencies → optionalDependencies → transitivePeerDependencies
    // → optional) so a parse-then-write round-trip stays diff-clean
    // against pnpm's own output. `bundledDependencies` is not in pnpm's
    // snapshot schema (lives on `LockfilePackageInfo`, pre-existing
    // aube quirk) — placed last so it does not split the pnpm-
    // canonical block.
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transitive_peer_dependencies: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundled_dependencies: Option<Vec<String>>,
}
