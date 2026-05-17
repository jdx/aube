pub mod add;
pub mod add_supply_chain;
pub mod approve_builds;
pub mod audit;
pub mod bin;
pub mod cache;
pub mod cat_file;
pub mod cat_index;
pub mod catalogs;
pub mod check;
pub mod ci;
pub mod clean;
pub mod completion;
pub mod config;
pub mod create;
pub mod dedupe;
pub mod deploy;
pub mod deprecate;
pub mod deprecations;
pub mod diag;
pub mod dist_tag;
pub mod dlx;
pub mod doctor;
pub mod exec;
pub mod fetch;
pub mod find_hash;
pub mod global;
pub mod ignored_builds;
pub mod import;
pub mod init;
pub mod inject;
pub mod install;
pub mod install_test;
pub mod licenses;
pub mod link;
pub mod list;
pub mod login;
pub mod logout;
pub mod npm_fallback;
pub mod npmrc;
pub mod outdated;
pub mod pack;
pub mod patch;
pub mod patch_commit;
pub mod patch_remove;
pub mod peers;
pub mod prune;
pub mod publish;
pub mod publish_provenance;
pub mod query;
pub mod rebuild;
pub mod recursive;
pub mod remove;
pub mod restart;
pub mod root;
pub mod run;
pub mod run_output;
pub mod sbom;
pub mod security_scanner;
pub mod store;
pub mod undeprecate;
pub mod unlink;
pub mod unpublish;
pub mod update;
pub mod version;
pub mod view;
pub mod why;

use miette::{Context, IntoDiagnostic, miette};
use std::path::Path;

mod auto_install;
mod project_lock;
mod script_settings;
mod settings_context;

pub(crate) use auto_install::ensure_installed;
pub(crate) use project_lock::take_project_lock;
pub(crate) use script_settings::{configure_script_settings, configure_script_settings_for_cwd};
pub(crate) use settings_context::{
    FileSources, GlobalOutputFlags, build_resolver, chained_frozen_mode, ensure_registry_auth,
    expand_setting_path, global_frozen_override, global_output_flags, global_virtual_store_flags,
    load_npm_config, make_client, open_store, packument_cache_dir, packument_full_cache_dir,
    project_modules_dir, resolve_fetch_policy, resolve_modules_dir_name_for_cwd,
    resolve_virtual_store_dir, resolve_virtual_store_dir_for_cwd,
    resolve_virtual_store_dir_max_length, resolve_virtual_store_dir_max_length_for_cwd,
    resolved_cache_dir, run_pnpmfile_pre_resolution, set_fetch_cli_overrides,
    set_global_frozen_override, set_global_output_flags, set_global_virtual_store_flags,
    set_registry_override, set_skip_auto_install_on_package_manager_mismatch,
    skip_auto_install_on_package_manager_mismatch, with_settings_ctx,
};

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

/// Format the resolved `virtualStoreDir` as a display-ready prefix for
/// `aube list --long` and `aube why --long`, ending with a path
/// separator so callers can concatenate an encoded `dep_path`
/// filename. When `aube_dir` is a subdirectory of `ref_dir` the result
/// is relative (`./node_modules/.aube/`), matching the historical
/// output. For overrides that sit above or outside `ref_dir` (custom
/// `virtualStoreDir` like `~/.my-store/project` or `.vstore-out`) the
/// absolute path is returned so users can still find where packages
/// actually live — `../../../...` would be technically correct but
/// hard to paste into a shell.
pub(crate) fn format_virtual_store_display_prefix(
    aube_dir: &std::path::Path,
    ref_dir: &std::path::Path,
) -> String {
    if let Some(rel) = pathdiff::diff_paths(aube_dir, ref_dir)
        && !rel.as_os_str().is_empty()
        && !rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return format!("./{}/", rel.display());
    }
    format!("{}/", aube_dir.display())
}

/// Pick the highest version in `packument` that satisfies `range_str`.
/// Returns the *original packument key* (not a round-tripped `Version`
/// display string) so string comparisons against the lockfile's
/// `current` — which also comes from a packument key — stay stable
/// for versions whose `Display` differs from their original form
/// (e.g. leading zeros in prerelease identifiers, build metadata
/// that `Version` drops). Returns `None` for unparseable ranges
/// (workspace:/file: specs, git URLs, etc.) so callers can fall
/// back to the locked version.
pub(crate) fn max_satisfying_version(
    packument: &aube_registry::Packument,
    range_str: &str,
) -> Option<String> {
    let range = node_semver::Range::parse(range_str).ok()?;
    let mut best: Option<(&str, node_semver::Version)> = None;
    for ver_str in packument.versions.keys() {
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !v.satisfies(&range) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| v > *b) {
            best = Some((ver_str.as_str(), v));
        }
    }
    best.map(|(key, _)| key.to_string())
}

/// Type alias for the catalog map the resolver consumes — outer key is
/// the catalog name (`default` for the unnamed catalog), inner map goes
/// from package name to version range.
pub(crate) type CatalogMap =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

/// Merge `default_cat` / `named_cats` into `out`. Later calls overwrite
/// earlier entries — callers invoke this in ascending precedence order
/// so the highest-priority source lands last.
fn merge_catalog_source(
    out: &mut CatalogMap,
    default_cat: &std::collections::BTreeMap<String, String>,
    named_cats: &CatalogMap,
) {
    if !default_cat.is_empty() {
        let entry = out.entry("default".to_string()).or_default();
        for (k, v) in default_cat {
            entry.insert(k.clone(), v.clone());
        }
    }
    for (name, entries) in named_cats {
        let bucket = out.entry(name.clone()).or_default();
        for (k, v) in entries {
            bucket.insert(k.clone(), v.clone());
        }
    }
}

/// Pull the bun-style `workspaces.catalog` / `workspaces.catalogs` and
/// pnpm-style `pnpm.catalog` / `pnpm.catalogs` out of a single
/// package.json and merge them into `out`. Precedence within one
/// manifest: `pnpm.*` wins over `workspaces.*`.
fn merge_manifest_catalogs(out: &mut CatalogMap, manifest: &aube_manifest::PackageJson) {
    if let Some(ws) = &manifest.workspaces {
        merge_catalog_source(out, ws.catalog(), ws.catalogs());
    }
    merge_catalog_source(out, &manifest.pnpm_catalog(), &manifest.pnpm_catalogs());
}

/// Discover catalog entries from every supported source and merge them
/// into a single map for the resolver.
///
/// Sources, in ascending precedence (later overrides earlier on a per-
/// entry basis):
/// 1. `workspaces.catalog` / `workspaces.catalogs` in the project-root
///    `package.json` (bun style).
/// 2. `pnpm.catalog` / `pnpm.catalogs` in the project-root `package.json`.
/// 3. Same two fields from the workspace-root `package.json` when it's
///    a different file (monorepo subpackage installs). The workspace
///    root is the nearest ancestor with either a `pnpm-workspace.yaml` /
///    `aube-workspace.yaml` or a `package.json` carrying a `workspaces`
///    field — bun / npm / yarn projects use the latter and have no yaml.
/// 4. `catalog:` / `catalogs:` in the nearest `pnpm-workspace.yaml` /
///    `aube-workspace.yaml` walking up from `project_root`.
///
/// Walking up matters for monorepos where `aube install` runs from a
/// subpackage — without it, the loader only looks at `project_root`
/// and misses the root workspace's catalogs entirely.
///
/// Every command that builds a `Resolver` threads this map through
/// `Resolver::with_catalogs`; otherwise the resolver hard-fails any
/// `catalog:` dep with `UnknownCatalog(Entry)`.
pub(crate) fn discover_catalogs(project_root: &std::path::Path) -> miette::Result<CatalogMap> {
    use miette::{Context, IntoDiagnostic};

    let mut out = CatalogMap::new();

    // (1)+(2): project-root package.json catalogs.
    let project_manifest_path = project_root.join("package.json");
    let project_manifest = aube_manifest::PackageJson::from_path(&project_manifest_path).ok();
    if let Some(m) = &project_manifest {
        merge_manifest_catalogs(&mut out, m);
    }

    // (3): workspace-root package.json catalogs, if the workspace root
    // sits above the project root. We resolve the workspace root from
    // either marker — yaml first (pnpm convention), then `workspaces`
    // field (bun / npm / yarn convention) — so a subpackage install in
    // a non-pnpm monorepo still picks up the root catalog.
    let workspace_yaml_dir = crate::dirs::find_workspace_yaml_root(project_root);
    let workspace_root_dir = crate::dirs::find_workspace_root(project_root);
    if let Some(dir) = &workspace_root_dir
        && dir != project_root
        && let Ok(m) = aube_manifest::PackageJson::from_path(&dir.join("package.json"))
    {
        merge_manifest_catalogs(&mut out, &m);
    }

    // (4): workspace yaml catalogs, highest precedence. Loaded from the
    // walk-up directory when present, else from `project_root`.
    let yaml_dir = workspace_yaml_dir.as_deref().unwrap_or(project_root);
    let (ws_config, _raw) = aube_manifest::workspace::load_both(yaml_dir)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    merge_catalog_source(&mut out, &ws_config.catalog, &ws_config.catalogs);

    out.retain(|_, v| !v.is_empty());
    Ok(out)
}

/// Convenience alias preserved for existing call sites; forwards to
/// [`discover_catalogs`] so every command sees the same merged view.
pub(crate) fn load_workspace_catalogs(cwd: &std::path::Path) -> miette::Result<CatalogMap> {
    discover_catalogs(cwd)
}

/// Read and parse `package.json` at `manifest_path` with the standard
/// miette-wrapped error message used across commands.
pub(crate) fn load_manifest(manifest_path: &Path) -> miette::Result<aube_manifest::PackageJson> {
    aube_manifest::PackageJson::from_path(manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")
}

/// Load `<root>/package.json` when it exists, else return a default
/// (empty) manifest. Used by workspace-scoped commands that accept
/// yaml-only coordinator roots (`pnpm-workspace.yaml` only, no root
/// `package.json`).
pub(crate) fn load_manifest_or_default(root: &Path) -> miette::Result<aube_manifest::PackageJson> {
    let path = root.join("package.json");
    if path.is_file() {
        load_manifest(&path)
    } else {
        Ok(aube_manifest::PackageJson::default())
    }
}

/// Serialize `value` as pretty JSON with a trailing newline and
/// atomically write it to `path`. Wraps the serialize + atomic-write
/// pair used by add/remove/update/audit when mutating `package.json`.
pub(crate) fn write_manifest_json<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> miette::Result<()> {
    let json = serde_json::to_string_pretty(value)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    write_manifest_atomic(path, format!("{json}\n").as_bytes())
        .wrap_err("failed to write package.json")
}

pub(crate) fn update_manifest_json_object<F>(path: &Path, update: F) -> miette::Result<()>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> miette::Result<()>,
{
    let content = std::fs::read_to_string(path)
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;
    let mut json: serde_json::Value = serde_json::from_str(&content)
        .into_diagnostic()
        .wrap_err("failed to parse package.json")?;
    let serde_json::Value::Object(obj) = &mut json else {
        return Err(miette!("package.json must contain a JSON object"));
    };

    update(obj)?;

    let json = serde_json::to_string_pretty(&json)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    write_manifest_atomic(path, format!("{json}\n").as_bytes())
}

pub(crate) fn write_manifest_dep_sections(
    path: &Path,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<()> {
    update_manifest_json_object(path, |obj| {
        sync_manifest_dep_sections(obj, manifest);
        Ok(())
    })
}

pub(crate) fn sync_manifest_dep_sections(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    manifest: &aube_manifest::PackageJson,
) {
    sync_dep_section(obj, "dependencies", &manifest.dependencies);
    sync_dep_section(obj, "devDependencies", &manifest.dev_dependencies);
    sync_dep_section(obj, "peerDependencies", &manifest.peer_dependencies);
    sync_dep_section(obj, "optionalDependencies", &manifest.optional_dependencies);
}

fn sync_dep_section(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    deps: &std::collections::BTreeMap<String, String>,
) {
    if deps.is_empty() {
        obj.remove(key);
        return;
    }

    let section = deps
        .iter()
        .map(|(name, spec)| (name.clone(), serde_json::Value::String(spec.clone())))
        .collect();
    obj.insert(key.to_string(), serde_json::Value::Object(section));
}

/// Atomic write for `package.json` (and any sibling JSON we care
/// about): write to a tempfile in the same directory then rename.
/// The old `fs::write` truncates in place and a crash mid-write left
/// users with an empty manifest — the worst aube failure mode.
pub(crate) fn write_manifest_atomic(path: &Path, body: &[u8]) -> miette::Result<()> {
    aube_util::fs_atomic::atomic_write(path, body)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write {}", path.display()))
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
) -> miette::Result<std::path::PathBuf> {
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
pub(crate) fn find_workspace_root(start: &std::path::Path) -> miette::Result<std::path::PathBuf> {
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
    cwd: &std::path::Path,
    filter: &aube_workspace::selector::EffectiveFilter,
    command: &str,
) -> miette::Result<(
    std::path::PathBuf,
    Vec<aube_workspace::selector::SelectedPackage>,
)> {
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

/// Resolve a version spec against a full packument. Returns the concrete
/// version string to look up in the `versions` object.
///
/// Resolution order, matching npm/pnpm:
/// 1. No spec → `dist-tags.latest`
/// 2. Spec is a dist-tag → `dist-tags[spec]`
/// 3. Spec is an exact version in `versions` → that version
/// 4. Spec is a semver range → highest matching version in `versions`
///
/// Shared by `aube view` and `aube store add` so fixes land in one place.
pub(crate) fn resolve_version(packument: &serde_json::Value, spec: Option<&str>) -> Option<String> {
    let dist_tags = packument.get("dist-tags").and_then(|v| v.as_object());
    let versions = packument.get("versions").and_then(|v| v.as_object())?;

    let spec = match spec {
        None | Some("") => {
            return dist_tags?
                .get("latest")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        Some(s) => s,
    };

    if let Some(tag) = dist_tags.and_then(|t| t.get(spec)).and_then(|v| v.as_str()) {
        return Some(tag.to_string());
    }

    if versions.contains_key(spec) {
        return Some(spec.to_string());
    }

    let range: node_semver::Range = spec.parse().ok()?;
    versions
        .keys()
        .filter_map(|v| {
            v.parse::<node_semver::Version>()
                .ok()
                .filter(|parsed| parsed.satisfies(&range))
                .map(|parsed| (v.clone(), parsed))
        })
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(raw, _)| raw)
}

/// Split `name[@version]` into the package name and optional version spec.
/// Handles scoped packages (`@scope/name[@version]`) correctly — the first
/// `@` in a scoped input is the scope sigil, not a version separator.
///
/// Returns borrowed slices of the input. Callers that need owned `String`s
/// or a default like `"latest"` can adapt the result at their call site.
pub(crate) fn split_name_spec(input: &str) -> (&str, Option<&str>) {
    aube_util::pkg::split_name_spec(input)
}

/// Percent-encode a package name for npm registry path segments.
/// `@scope/name` becomes `@scope%2Fname`; the leading `@` stays literal
/// and only the scope/name slash is encoded. Plain names pass through.
///
/// Shared between `publish` and `unpublish` (both target
/// `{registry}/{name}/...` endpoints) so the two write commands can't
/// drift on URL shape — the registry routes auth on these paths, so
/// even a subtle encoding change would break one command silently
/// while leaving the other working.
pub(crate) fn encode_package_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix('@')
        && let Some((scope, pkg)) = rest.split_once('/')
    {
        return format!("@{scope}%2F{pkg}");
    }
    name.to_string()
}

#[cfg(test)]
mod encode_package_name_tests {
    use super::encode_package_name;

    #[test]
    fn scoped_name_encodes_slash() {
        assert_eq!(encode_package_name("@scope/pkg"), "@scope%2Fpkg");
    }

    #[test]
    fn plain_name_passthrough() {
        assert_eq!(encode_package_name("lodash"), "lodash");
    }

    #[test]
    fn malformed_scoped_name_passthrough() {
        // `@scope` with no slash isn't a valid package name, but we
        // shouldn't panic — return it verbatim so the registry can
        // surface the error.
        assert_eq!(encode_package_name("@scope"), "@scope");
    }
}

#[cfg(test)]
mod split_name_spec_tests {
    use super::split_name_spec;

    #[test]
    fn plain_name() {
        assert_eq!(split_name_spec("lodash"), ("lodash", None));
    }

    #[test]
    fn name_with_version() {
        assert_eq!(
            split_name_spec("lodash@4.17.21"),
            ("lodash", Some("4.17.21"))
        );
    }

    #[test]
    fn name_with_range() {
        assert_eq!(split_name_spec("lodash@^4"), ("lodash", Some("^4")));
    }

    #[test]
    fn name_with_tag() {
        assert_eq!(split_name_spec("react@next"), ("react", Some("next")));
    }

    #[test]
    fn scoped_no_version() {
        assert_eq!(split_name_spec("@babel/core"), ("@babel/core", None));
    }

    #[test]
    fn scoped_with_version() {
        assert_eq!(
            split_name_spec("@babel/core@7.0.0"),
            ("@babel/core", Some("7.0.0"))
        );
    }
}

/// Remove an existing file/dir/symlink at the given path, if present.
///
/// Windows quirk: directory junctions and directory symlinks report as
/// symlinks via `symlink_metadata`, but `std::fs::remove_file` returns
/// `Access is denied (os error 5)` for them — the Win32 `DeleteFile`
/// syscall only works on file-shaped entries. The link entry has to be
/// torn down with `RemoveDirectory` (= `std::fs::remove_dir`), which is
/// non-recursive and so leaves the junction target untouched. Falling
/// back on `remove_file` failure keeps every other platform on the
/// usual single-syscall path.
pub(crate) fn remove_existing(path: &std::path::Path) -> miette::Result<()> {
    let Ok(md) = path.symlink_metadata() else {
        return Ok(());
    };
    let file_type = md.file_type();
    if file_type.is_dir() {
        return std::fs::remove_dir_all(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display()));
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(_) if file_type.is_symlink() => std::fs::remove_dir(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display())),
        Err(e) => Err(e)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display())),
    }
}

pub(crate) fn workspace_importer_path(
    workspace_root: &std::path::Path,
    dir: &std::path::Path,
) -> miette::Result<String> {
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

/// Create a directory link (symlink on Unix, NTFS junction on
/// Windows). Thin re-export of [`aube_linker::create_dir_link`] —
/// the linker owns the platform-specific implementation so every
/// directory-link call site in the workspace behaves identically,
/// including Windows' "junctions not symlinks" choice that keeps
/// installs working without Developer Mode.
pub(crate) fn symlink_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    aube_linker::create_dir_link(src, dst)
}

/// Dep-type filter derived from `--prod` / `--dev` on list-style commands
/// (`list`, `why`). Both commands take the same two flags with the same
/// semantics — this enum is the shared derivation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DepFilter {
    /// Include every dep type.
    All,
    /// `--prod`: include `Production` and `Optional`, drop `Dev`.
    ProdOnly,
    /// `--dev`: include only `Dev`.
    DevOnly,
}

impl DepFilter {
    /// Collapse the two mutually-exclusive boolean flags into a filter.
    /// `(true, _)` wins because clap enforces `conflicts_with = "dev"`.
    pub(crate) fn from_flags(prod: bool, dev: bool) -> Self {
        match (prod, dev) {
            (true, _) => Self::ProdOnly,
            (_, true) => Self::DevOnly,
            _ => Self::All,
        }
    }

    /// Does this filter keep the given dep type?
    pub(crate) fn keeps(self, dep_type: aube_lockfile::DepType) -> bool {
        use aube_lockfile::DepType;
        matches!(
            (self, dep_type),
            (Self::All, _)
                | (Self::ProdOnly, DepType::Production | DepType::Optional)
                | (Self::DevOnly, DepType::Dev)
        )
    }
}

#[cfg(test)]
mod dep_filter_tests {
    use super::*;
    use aube_lockfile::DepType;

    #[test]
    fn all_keeps_everything() {
        let f = DepFilter::from_flags(false, false);
        assert!(f.keeps(DepType::Production));
        assert!(f.keeps(DepType::Dev));
        assert!(f.keeps(DepType::Optional));
    }

    #[test]
    fn prod_keeps_production_and_optional() {
        let f = DepFilter::from_flags(true, false);
        assert!(f.keeps(DepType::Production));
        assert!(f.keeps(DepType::Optional));
        assert!(!f.keeps(DepType::Dev));
    }

    #[test]
    fn dev_keeps_only_dev() {
        let f = DepFilter::from_flags(false, true);
        assert!(!f.keeps(DepType::Production));
        assert!(!f.keeps(DepType::Optional));
        assert!(f.keeps(DepType::Dev));
    }

    #[test]
    fn prod_wins_over_dev_when_both_set() {
        // clap should prevent this combination via conflicts_with, but we
        // still want deterministic behavior if it ever gets through.
        let f = DepFilter::from_flags(true, true);
        assert!(f.keeps(DepType::Production));
        assert!(!f.keeps(DepType::Dev));
    }

    #[test]
    fn package_manager_mismatch_skip_auto_install_defaults_off() {
        assert!(!skip_auto_install_on_package_manager_mismatch());
    }
}

#[cfg(test)]
mod manifest_write_tests {
    use super::*;

    #[test]
    fn write_manifest_dep_sections_preserves_existing_top_level_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{
  "name": "example",
  "version": "1.0.0",
  "license": "MIT",
  "scripts": {
    "test": "echo test"
  },
  "devDependencies": {
    "typescript": "^6.0.3"
  }
}
"#,
        )
        .unwrap();

        let mut manifest = aube_manifest::PackageJson::from_path(&path).unwrap();
        manifest
            .dev_dependencies
            .insert("tstyche".to_string(), "^7.1.0".to_string());

        write_manifest_dep_sections(&path, &manifest).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            root_key_order(&written),
            ["name", "version", "license", "scripts", "devDependencies"]
        );
        assert!(written.contains(r#""tstyche": "^7.1.0""#));
    }

    #[test]
    fn write_manifest_dep_sections_removes_empty_sections_without_reordering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{
  "name": "example",
  "devDependencies": {
    "typescript": "^6.0.3"
  },
  "license": "MIT"
}
"#,
        )
        .unwrap();

        let mut manifest = aube_manifest::PackageJson::from_path(&path).unwrap();
        manifest.dev_dependencies.remove("typescript");

        write_manifest_dep_sections(&path, &manifest).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(root_key_order(&written), ["name", "license"]);
        assert!(!written.contains("devDependencies"));
    }

    fn root_key_order(raw: &str) -> Vec<String> {
        let serde_json::Value::Object(obj) = serde_json::from_str(raw).unwrap() else {
            panic!("expected object");
        };
        obj.keys().cloned().collect()
    }
}

#[cfg(test)]
mod remove_existing_tests {
    use super::*;

    #[test]
    fn removes_a_symlink_pointing_at_a_populated_directory_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let canary = target.join("keep.txt");
        std::fs::write(&canary, b"keep me").unwrap();

        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        aube_linker::create_dir_link(&target, &link).unwrap();

        remove_existing(&link).unwrap();
        assert!(!link.exists());
        assert!(
            canary.exists(),
            "remove_existing must not recurse into the symlink's target"
        );
    }

    #[test]
    fn missing_path_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        remove_existing(&dir.path().join("does-not-exist")).unwrap();
    }
}
