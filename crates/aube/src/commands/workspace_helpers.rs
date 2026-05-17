use std::path::{Path, PathBuf};

use miette::{Context, IntoDiagnostic, miette};

use super::DepFilter;

pub(crate) fn retarget_cwd(path: &Path) -> miette::Result<()> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().into_diagnostic()?.join(path)
    };
    std::env::set_current_dir(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to chdir into {}", path.display()))?;
    crate::dirs::set_cwd(&path)?;
    Ok(())
}

/// Parse the project lockfile, mapping `NotFound` to a user-facing hint
/// that includes `context` (e.g. `"aube audit"`).
pub(crate) fn load_graph(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
    missing_hint: &str,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    match aube_lockfile::parse_lockfile(project_dir, manifest) {
        Ok(g) => Ok(g),
        Err(aube_lockfile::Error::NotFound(_)) => Err(miette!("{missing_hint}")),
        Err(e) => Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    }
}

/// Collect the transitive dep-path closure reachable from the filtered
/// root deps, keyed by dep_path for stable iteration. Used by audit,
/// sbom, and anything else that needs "which packages would apply if
/// the user ran install in this mode".
pub(crate) fn collect_dep_closure(
    graph: &aube_lockfile::LockfileGraph,
    filter: DepFilter,
    no_optional: bool,
) -> std::collections::BTreeMap<String, &aube_lockfile::LockedPackage> {
    let mut out: std::collections::BTreeMap<String, &aube_lockfile::LockedPackage> =
        std::collections::BTreeMap::new();
    let mut stack: Vec<String> = graph
        .root_deps()
        .iter()
        .filter(|d| filter.keeps(d.dep_type))
        .filter(|d| !(no_optional && matches!(d.dep_type, aube_lockfile::DepType::Optional)))
        .map(|d| d.dep_path.clone())
        .collect();
    while let Some(dep_path) = stack.pop() {
        if out.contains_key(&dep_path) {
            continue;
        }
        let Some(pkg) = graph.get_package(&dep_path) else {
            continue;
        };
        out.insert(dep_path.clone(), pkg);
        for (name, version) in &pkg.dependencies {
            stack.push(format!("{name}@{version}"));
        }
    }
    out
}

/// Restore `cwd` after a filtered-workspace loop and fold any restore
/// error into the original `result`. Filter loops mutate the process
/// cwd so they can run per-package commands as if the user were in
/// that directory; this puts things back exactly once, even when the
/// loop itself failed.
pub(crate) fn finish_filtered_workspace(
    cwd: &Path,
    result: miette::Result<()>,
) -> miette::Result<()> {
    let restore =
        retarget_cwd(cwd).wrap_err_with(|| format!("failed to restore cwd to {}", cwd.display()));
    match result {
        Ok(()) => restore,
        Err(err) => {
            let _ = restore;
            Err(err)
        }
    }
}

/// Write lockfile preserving existing format and log the file name.
pub(crate) fn write_and_log_lockfile(
    cwd: &Path,
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<PathBuf> {
    let written_path = aube_lockfile::write_lockfile_preserving_existing(cwd, graph, manifest)
        .into_diagnostic()
        .wrap_err("failed to write lockfile")?;
    eprintln!(
        "Wrote {}",
        written_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| written_path.display().to_string())
    );
    Ok(written_path)
}

/// Walk up from `start` looking for a directory that marks a workspace
/// root — either an `aube-workspace.yaml` / `pnpm-workspace.yaml` file
/// or a `package.json` with a `workspaces` field.
pub(crate) fn find_workspace_root(start: &Path) -> miette::Result<PathBuf> {
    crate::dirs::find_workspace_root(start).ok_or_else(|| {
        miette!(
            "no workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) found above {}",
            start.display()
        )
    })
}

/// Resolve `--filter` to the matching workspace packages, returning the
/// workspace root alongside the matches. Callers need the root to
/// compute importer paths, resolve the lockfile, etc., and `cwd`
/// alone isn't it in yarn / npm / bun subpackage installs where only
/// the monorepo root carries `package.json#workspaces`.
pub(crate) fn select_workspace_packages(
    cwd: &Path,
    filter: &aube_workspace::selector::EffectiveFilter,
    command: &str,
) -> miette::Result<(PathBuf, Vec<aube_workspace::selector::SelectedPackage>)> {
    let root = crate::dirs::find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let workspace_pkgs = aube_workspace::find_workspace_packages(&root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "aube {command}: --filter requires a workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) at or above {}",
            cwd.display()
        ));
    }
    let matched =
        aube_workspace::selector::select_workspace_packages(&root, &workspace_pkgs, filter)
            .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if matched.is_empty() {
        return Err(miette!(
            "aube {command}: filter {filter:?} did not match any workspace package"
        ));
    }
    Ok((root, matched))
}

pub(crate) fn workspace_importer_path(workspace_root: &Path, dir: &Path) -> miette::Result<String> {
    // `pathdiff` produces parent-relative keys (`../sibling`) for
    // workspaces whose `pnpm-workspace.yaml#packages` reaches above
    // the yaml's directory via `../**`. The shared lockfile records
    // the same `..`-prefixed key, so the linker and drift check
    // line up with what `find_workspace_packages` returns.
    let rel = pathdiff::diff_paths(dir, workspace_root).ok_or_else(|| {
        miette!(
            "could not compute path of workspace package {} relative to {}",
            dir.display(),
            workspace_root.display()
        )
    })?;
    if rel.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }
}
