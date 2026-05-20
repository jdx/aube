use crate::commands::workspace_importer_path;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub(super) struct WorkspaceInstallPlan {
    pub workspace_packages: Vec<PathBuf>,
    pub has_workspace: bool,
    pub is_workspace_project: bool,
    pub link_all_workspace_importers: bool,
    pub manifests: Vec<(String, aube_manifest::PackageJson)>,
    pub ws_package_versions: HashMap<String, String>,
    pub ws_dirs: BTreeMap<String, PathBuf>,
    pub lifecycle_manifests: Vec<(String, aube_manifest::PackageJson)>,
}

pub(super) fn discover_workspace_plan(
    cwd: &Path,
    root_manifest: &aube_manifest::PackageJson,
    settings_ctx: &aube_settings::ResolveCtx<'_>,
    workspace_filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<WorkspaceInstallPlan> {
    let workspace_packages = aube_workspace::find_workspace_packages(cwd)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;
    let recursive_install = aube_settings::resolved::recursive_install(settings_ctx);
    let has_workspace = !workspace_packages.is_empty();
    // Distinct from `has_workspace`: `is_workspace_project` stays
    // true when every workspace sub-package was just removed from
    // disk but the workspace yaml / `workspaces` field is still in
    // place. The lockfile drift check needs this stronger signal so
    // it still prunes orphan importer entries on the all-packages-
    // gone boundary, where `manifests` collapses to `[(".", root)]`
    // and looks indistinguishable from a non-workspace install.
    let is_workspace_project = aube_workspace::is_workspace_project_root(cwd);
    let link_all_workspace_importers =
        has_workspace && (recursive_install || !workspace_filter.is_empty());

    let mut manifests = vec![(".".to_string(), root_manifest.clone())];
    let mut ws_package_versions = HashMap::new();
    let mut ws_dirs = BTreeMap::new();

    // Include the root package itself as a workspace target so
    // sub-packages can use `workspace:*` to depend on it. The
    // directory entry is needed for the linker to create symlinks
    // into child packages' node_modules.
    if let Some(ref name) = root_manifest.name {
        let version = root_manifest.version.as_deref().unwrap_or("0.0.0");
        ws_package_versions.insert(name.clone(), version.to_string());
        ws_dirs.insert(name.clone(), cwd.to_path_buf());
    }

    if has_workspace {
        let project_name = root_manifest.name.as_deref().unwrap_or("(unnamed)");
        tracing::debug!(
            "Workspace: {} packages for {project_name}",
            workspace_packages.len()
        );
        for pkg_dir in &workspace_packages {
            let pkg_manifest = aube_manifest::PackageJson::from_path(&pkg_dir.join("package.json"))
                .map_err(miette::Report::new)
                .wrap_err_with(|| format!("failed to read {}/package.json", pkg_dir.display()))?;

            // Importer key uses forward slash. pnpm lockfile convention
            // is always `/`. `pathdiff` lets workspace globs reach into
            // parent trees while still writing relative importer keys.
            let rel_path = pathdiff::diff_paths(pkg_dir, cwd)
                .unwrap_or_else(|| pkg_dir.clone())
                .to_string_lossy()
                .replace('\\', "/");

            if let Some(ref name) = pkg_manifest.name {
                // pnpm accepts workspace members without versions. Use
                // "0.0.0" so workspace protocol and bare `*` links can
                // still resolve locally while specific ranges fail when
                // they should.
                let version = pkg_manifest.version.as_deref().unwrap_or("0.0.0");
                ws_package_versions.insert(name.clone(), version.to_string());
                ws_dirs.insert(name.clone(), pkg_dir.clone());
                tracing::debug!("  {name}@{version} ({rel_path})");
            }

            // `pnpm-workspace.yaml: packages: ["."]` expands to the
            // root itself; skip the empty relative path because `"."`
            // is already seeded above.
            if !rel_path.is_empty() {
                manifests.push((rel_path, pkg_manifest));
            }
        }
    }

    let lifecycle_manifests = if has_workspace && link_all_workspace_importers {
        order_lifecycle_manifests(
            manifests
                .iter()
                .filter(|(importer, _)| aube_linker::is_physical_importer(importer))
                .cloned()
                .collect(),
        )
    } else {
        vec![(".".to_string(), root_manifest.clone())]
    };

    Ok(WorkspaceInstallPlan {
        workspace_packages,
        has_workspace,
        is_workspace_project,
        link_all_workspace_importers,
        manifests,
        ws_package_versions,
        ws_dirs,
        lifecycle_manifests,
    })
}

pub(super) fn filter_graph_to_workspace_selection(
    workspace_root: &std::path::Path,
    workspace_packages: &[std::path::PathBuf],
    graph: &aube_lockfile::LockfileGraph,
    filters: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    let selected = aube_workspace::selector::select_workspace_packages(
        workspace_root,
        workspace_packages,
        filters,
    )
    .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if selected.is_empty() {
        return Err(miette!(
            "aube install: filter {filters:?} did not match any workspace package"
        ));
    }
    let mut keep_importers = std::collections::BTreeSet::new();
    if graph.importers.contains_key(".") {
        keep_importers.insert(".".to_string());
    }
    for pkg in selected {
        keep_importers.insert(workspace_importer_path(workspace_root, &pkg.dir)?);
    }
    let importers: std::collections::BTreeMap<String, Vec<aube_lockfile::DirectDep>> = graph
        .importers
        .iter()
        .filter(|(importer, _)| keep_importers.contains(*importer))
        .map(|(importer, deps)| (importer.clone(), deps.clone()))
        .collect();
    let filtered = aube_lockfile::LockfileGraph {
        importers,
        ..graph.clone()
    };
    Ok(filtered.filter_deps(|_| true))
}

pub(super) fn importer_project_dir(
    workspace_root: &std::path::Path,
    importer_path: &str,
) -> std::path::PathBuf {
    if importer_path == "." {
        workspace_root.to_path_buf()
    } else {
        // Lexically collapse `..` from the join so a parent-relative
        // importer key (`../sibling`, written by `find_workspace_packages`
        // when `pnpm-workspace.yaml#packages` uses `../**`) lands at
        // the actual sibling directory rather than `<root>/../sibling`.
        // Downstream consumers — `pathdiff` for symlink targets and
        // `strip_prefix` for ancestor checks — give wrong results
        // against an unnormalized path with embedded `..` segments.
        aube_util::path::normalize_lexical(&workspace_root.join(importer_path))
    }
}

pub(super) fn order_lifecycle_manifests(
    manifests: Vec<(String, aube_manifest::PackageJson)>,
) -> Vec<(String, aube_manifest::PackageJson)> {
    if manifests.len() < 2 {
        return manifests;
    }

    let importer_index: std::collections::HashMap<&str, usize> = manifests
        .iter()
        .enumerate()
        .map(|(idx, (importer, _))| (importer.as_str(), idx))
        .collect();
    let workspace_name_to_importer: std::collections::HashMap<&str, &str> = manifests
        .iter()
        .filter_map(|(importer, manifest)| {
            manifest
                .name
                .as_deref()
                .map(|name| (name, importer.as_str()))
        })
        .collect();

    let mut edges = vec![Vec::<usize>::new(); manifests.len()];
    let mut indegree = vec![0usize; manifests.len()];
    for (dependent_idx, (dependent_importer, manifest)) in manifests.iter().enumerate() {
        for dep_name in manifest
            .dependencies
            .keys()
            .chain(manifest.dev_dependencies.keys())
            .chain(manifest.optional_dependencies.keys())
        {
            let Some(dependency_importer) = workspace_name_to_importer.get(dep_name.as_str())
            else {
                continue;
            };
            if *dependency_importer == dependent_importer {
                continue;
            }
            let Some(&dependency_idx) = importer_index.get(dependency_importer) else {
                continue;
            };
            if !edges[dependency_idx].contains(&dependent_idx) {
                edges[dependency_idx].push(dependent_idx);
                indegree[dependent_idx] += 1;
            }
        }
    }

    let mut ready: std::collections::VecDeque<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(idx, degree)| (*degree == 0).then_some(idx))
        .collect();
    let mut ordered = Vec::with_capacity(manifests.len());
    let mut emitted = vec![false; manifests.len()];
    while let Some(idx) = ready.pop_front() {
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        ordered.push(idx);
        for &dependent_idx in &edges[idx] {
            indegree[dependent_idx] -= 1;
            if indegree[dependent_idx] == 0 {
                ready.push_back(dependent_idx);
            }
        }
    }
    for (idx, is_emitted) in emitted.iter().enumerate() {
        if !is_emitted {
            ordered.push(idx);
        }
    }

    let mut manifests = manifests
        .into_iter()
        .map(Some)
        .collect::<Vec<Option<(String, aube_manifest::PackageJson)>>>();
    ordered
        .into_iter()
        .filter_map(|idx| manifests[idx].take())
        .collect()
}

/// Write one lockfile per non-root workspace importer when
/// `sharedWorkspaceLockfile=false` is set. Each lockfile contains
/// only the importer's own deps (remapped to `.`) plus the transitive
/// closure reachable from them. The workspace-root lockfile is not
/// written under this layout.
///
/// Importers without a corresponding manifest entry are skipped — the
/// resolver should never produce one, but defensive skipping keeps a
/// stale graph entry from triggering a write into a directory that
/// doesn't exist on disk.
pub(super) fn write_per_project_lockfiles(
    workspace_root: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    workspace_manifests: &[(String, aube_manifest::PackageJson)],
    write_kind: aube_lockfile::LockfileKind,
) -> miette::Result<()> {
    use miette::IntoDiagnostic;
    for (importer_path, pkg_manifest) in workspace_manifests {
        if importer_path == "." {
            // The root manifest gets no per-project lockfile under
            // sharedWorkspaceLockfile=false; it's the workspace anchor,
            // not an installable importer.
            continue;
        }
        let Some(subset) = graph.subset_to_importer(importer_path, |_| true) else {
            tracing::debug!(
                "sharedWorkspaceLockfile=false: skipping {importer_path} (no graph importer entry)"
            );
            continue;
        };
        let pkg_dir = workspace_root.join(importer_path);
        let written = aube_lockfile::write_lockfile_as(&pkg_dir, &subset, pkg_manifest, write_kind)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write per-project lockfile at {importer_path}"))?;
        tracing::debug!(
            "sharedWorkspaceLockfile=false: wrote {} for importer {importer_path}",
            written
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| written.display().to_string())
        );
    }
    Ok(())
}

pub(super) fn filter_graph_to_importers<const N: usize>(
    graph: &aube_lockfile::LockfileGraph,
    keep_importers: [&str; N],
) -> aube_lockfile::LockfileGraph {
    let keep_importers: std::collections::BTreeSet<&str> = keep_importers.into_iter().collect();
    let importers: std::collections::BTreeMap<String, Vec<aube_lockfile::DirectDep>> = graph
        .importers
        .iter()
        .filter(|(importer, _)| keep_importers.contains(importer.as_str()))
        .map(|(importer, deps)| (importer.clone(), deps.clone()))
        .collect();
    let filtered = aube_lockfile::LockfileGraph {
        importers,
        ..graph.clone()
    };
    filtered.filter_deps(|_| true)
}

#[cfg(test)]
mod lifecycle_manifest_order_tests {
    use super::order_lifecycle_manifests;

    #[test]
    fn lifecycle_manifests_follow_workspace_dependency_order() {
        let ordered = order_lifecycle_manifests(vec![
            (".".to_string(), named_manifest("root")),
            (
                "packages/app".to_string(),
                manifest_with_dep("app", "@scope/lib"),
            ),
            ("packages/lib".to_string(), named_manifest("@scope/lib")),
        ]);
        let importers = ordered
            .iter()
            .map(|(importer, _)| importer.as_str())
            .collect::<Vec<_>>();

        assert_eq!(importers, [".", "packages/lib", "packages/app"]);
    }

    fn named_manifest(name: &str) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    fn manifest_with_dep(name: &str, dep: &str) -> aube_manifest::PackageJson {
        let mut manifest = named_manifest(name);
        manifest
            .dependencies
            .insert(dep.to_string(), "workspace:*".to_string());
        manifest
    }
}
