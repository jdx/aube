use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_STATE_DIR: &str = "node_modules";
const INSTALL_STATE_FILE_NAME: &str = "state.json";
const FRESH_STATE_FILE_NAME: &str = "fresh.json";
const LOCKFILE_SNAPSHOT_FILE_NAME: &str = "lockfile";

/// The install-state directory name, `.<name>-state`. Standalone aube:
/// `.aube-state`.
fn state_dir_name() -> String {
    format!(".{}-state", aube_util::embedder().name)
}

/// Resolve the modules dir and state directory path for `project_dir` in a
/// single settings-context load. `check_needs_install` and `write_state`
/// both need both values, and this is on the hot path for every
/// `aube run` / `exec` / `test` / `start` / `restart`.
///
/// The default `stateDir` falls back to the resolved `modulesDir` so the
/// state directory lives alongside the install tree — otherwise a
/// `modulesDir` override would create a phantom `node_modules/`
/// directory just to hold the state directory.
fn resolve_paths(project_dir: &Path) -> (PathBuf, PathBuf) {
    crate::commands::with_settings_ctx(project_dir, |ctx| {
        let modules_dir = project_dir.join(aube_settings::resolved::modules_dir(ctx));
        let raw_state = aube_settings::resolved::state_dir(ctx);
        let state_parent = if raw_state == DEFAULT_STATE_DIR {
            modules_dir.clone()
        } else {
            crate::commands::expand_setting_path(&raw_state, project_dir)
                .unwrap_or_else(|| modules_dir.clone())
        };
        let state_dir = state_parent.join(state_dir_name());
        (modules_dir, state_dir)
    })
}

fn state_dir(project_dir: &Path) -> PathBuf {
    resolve_paths(project_dir).1
}

fn relative_path_or_original(path: &Path, base: &Path) -> String {
    pathdiff::diff_paths(path, base)
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallState {
    pub lockfile_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_snapshot_name: Option<String>,
    pub package_json_hashes: BTreeMap<String, String>,
    /// Mirrors `FreshnessState::package_json_meta`. See R1 docstring
    /// there for the freshness-check fast-path semantics.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_json_meta: BTreeMap<String, FileMeta>,
    pub aube_version: String,
    #[serde(default, rename = "prod")]
    pub section_filtered: bool,
    #[serde(default)]
    pub settings_hash: String,
    /// Per-package content fingerprints from the last install,
    /// keyed by dep_path. Drives delta installs. Next install diffs
    /// these against the new lockfile's hashes and only re-fetches
    /// and re-links the entries that moved. Missing or stale values
    /// cascade to a full install. Purely additive, never
    /// load-bearing. Empty on fresh state or pre-delta aube.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_content_hashes: BTreeMap<String, String>,
    /// LtHash accumulator digest (hex) over every package in the
    /// installed graph. Wide-add multiset hash from
    /// `commands::install::delta::LtHash`. Match on this digest
    /// proves graph equivalence in a 32-byte compare and skips the
    /// O(N) map walk. Missing field cascades to the full diff.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub graph_lthash: String,
    /// Per-package Merkle subtree fingerprints, keyed by dep_path.
    /// Lets the delta path skip packages whose subtree matches the
    /// stored value even when their leaf changed. Peer-dep rewrites
    /// shuffle metadata without moving installed content, that is
    /// the case this catches. Missing field cascades to the
    /// leaf-only diff.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_subtree_hashes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_json_shape_digests: BTreeMap<String, String>,
    #[serde(default)]
    pub layout: Option<InstallLayoutState>,
    /// Spec keys (`name@version`) of registry deps whose build
    /// scripts were skipped on the last install because they are not
    /// on the `allowBuilds` allowlist. Persisted so the warm-path
    /// short-circuit can re-emit the same warning the full pipeline
    /// emits — without it, repeat installs go silent and users
    /// forget pending approvals. Empty on installs where the warning
    /// did not fire (no registry deps with lifecycle scripts, or
    /// `--ignore-scripts` / `strictDepBuilds=true` / `virtualStoreOnly`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreviewed_builds: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FreshnessState {
    lockfile_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lockfile_snapshot_name: Option<String>,
    package_json_hashes: BTreeMap<String, String>,
    /// Mtime + size per `package.json` keyed identically to
    /// `package_json_hashes`. Lets `package_jsons_stale` skip the
    /// BLAKE3 hash on the fast path: stat once, compare both fields,
    /// only re-hash when mtime or size changed. On a typical
    /// monorepo with 30 direct deps that's 30 BLAKE3 hashes per
    /// `aube run` startup collapsed to 30 stat calls.
    /// Missing field defaults to empty → falls through to the
    /// existing hash path, so older state files stay valid.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    package_json_meta: BTreeMap<String, FileMeta>,
    #[serde(default, rename = "prod")]
    section_filtered: bool,
    #[serde(default)]
    settings_hash: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    package_json_shape_digests: BTreeMap<String, String>,
    #[serde(default)]
    layout: Option<InstallLayoutState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unreviewed_builds: Vec<String>,
}

/// `(size, mtime)` snapshot used by `R1` mtime fast path. mtime is
/// stored as (secs, nanos) since UNIX epoch so the comparison
/// preserves the resolution the underlying filesystem reports.
///
/// Linux ext4/btrfs/XFS and macOS APFS report nanosecond mtimes;
/// Windows NTFS reports 100-nanosecond ticks. Truncating to whole
/// seconds would let an in-place edit within the same second as the
/// previous install slip past the freshness check (very plausible in
/// CI where edits + installs happen within milliseconds). FAT32 and
/// other coarse-resolution filesystems still get correct behavior:
/// a same-second overwrite there has nanos == 0 on both samples, so
/// the fast path matches and we skip — but FAT32 does not promise
/// mtime granularity below 2 seconds anyway, so callers running on
/// it should not rely on the fast path. The size comparison still
/// catches any change that grows or shrinks the file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub mtime_secs: i64,
    #[serde(default)]
    pub mtime_nanos: u32,
}

impl FileMeta {
    pub fn capture(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let dur = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        let (secs, nanos) = match dur {
            Some(d) => (d.as_secs() as i64, d.subsec_nanos()),
            None => (0, 0),
        };
        Some(Self {
            size: meta.len(),
            mtime_secs: secs,
            mtime_nanos: nanos,
        })
    }
}

impl From<&InstallState> for FreshnessState {
    fn from(state: &InstallState) -> Self {
        Self {
            lockfile_hash: state.lockfile_hash.clone(),
            lockfile_snapshot_name: state.lockfile_snapshot_name.clone(),
            package_json_hashes: state.package_json_hashes.clone(),
            package_json_meta: state.package_json_meta.clone(),
            section_filtered: state.section_filtered,
            settings_hash: state.settings_hash.clone(),
            package_json_shape_digests: state.package_json_shape_digests.clone(),
            layout: state.layout.clone(),
            unreviewed_builds: state.unreviewed_builds.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallLayoutState {
    pub linker: InstallLayoutMode,
    pub direct_entries: BTreeMap<String, Vec<String>>,
    pub packages: BTreeMap<String, InstalledPackageState>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallLayoutMode {
    Isolated,
    Hoisted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackageState {
    pub name: String,
    pub version: String,
    pub package_json_path: String,
    #[serde(default)]
    pub package_json_hash: String,
}

/// Check if install is needed. Returns None if up-to-date, or Some(reason) if stale.
pub fn check_needs_install(project_dir: &Path) -> Option<String> {
    check_needs_install_inner(project_dir, None)
}

/// Variant of [`check_needs_install`] that also checks `settings_hash`
/// with the caller's `cli_flags` bag. Use from `install::run`'s warm
/// path short circuit so `--node-linker=hoisted` and friends also feed
/// the hash. `ensure_installed` (from `aube run`) uses the plain
/// [`check_needs_install`] on purpose, see the note there.
pub fn check_needs_install_with_flags(
    project_dir: &Path,
    cli_flags: &[(String, String)],
) -> Option<String> {
    check_needs_install_inner(project_dir, Some(cli_flags))
}

fn check_needs_install_inner(
    project_dir: &Path,
    cli_flags: Option<&[(String, String)]>,
) -> Option<String> {
    let _diag =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "check_needs_install");
    let (modules_dir, state_path) = resolve_paths(project_dir);

    // No state directory = never installed (or `rm -rf <modulesDir>` wiped it).
    let _diag_read =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "read_state_file");
    let state = match read_or_migrate_fresh_state(&state_path) {
        Some(s) => s,
        None => return Some("install state not found".into()),
    };
    drop(_diag_read);

    // In the default config the state file lives inside `modulesDir` so
    // `rm -rf <modules>` wipes it. But `stateDir` can point elsewhere,
    // in which case the state survives a manual modules-dir nuke and
    // the hashes below would falsely report "up to date". Guard against
    // that explicitly — zero-dep projects still get a modules directory
    // (with `.bin/`) from install, so the directory check covers them.
    if !modules_dir.exists() {
        let name = modules_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("node_modules");
        return Some(format!("{name} is missing"));
    }

    // Check lockfile hash. Honor `gitBranchLockfile` so a branch-specific
    // lockfile is the freshness anchor when present, but fall back to the
    // base lockfile names so a freshly-enabled branch doesn't loop on
    // "no lockfile found" — see `active_lockfile` for the full resolution
    // order.
    let _diag_lock = aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "lockfile_hash");
    let (lockfile_name, lockfile_path) = active_lockfile(project_dir);
    let mut lockfile_missing = false;
    if let Some(path) = lockfile_path {
        let current_hash = hash_file(&path);
        if current_hash != state.lockfile_hash {
            return Some(format!("{lockfile_name} has changed"));
        }
    } else {
        lockfile_missing = true;
    }
    drop(_diag_lock);

    let _diag_pjs =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "package_jsons_stale");
    if let Some(reason) = package_jsons_stale(project_dir, &state) {
        return Some(reason);
    }
    drop(_diag_pjs);

    if state.section_filtered {
        return Some(
            "previous install omitted dependency sections; auto-installing full graph".into(),
        );
    }

    let _diag_layout =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "verify_install_layout");
    if let Some(reason) = verify_install_layout(project_dir, state.layout.as_ref()) {
        return Some(reason);
    }
    drop(_diag_layout);

    if let Some(cli_flags) = cli_flags {
        let _diag_settings =
            aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "settings_hash");
        let current_settings_hash = hash_settings(project_dir, cli_flags);
        if current_settings_hash != state.settings_hash {
            return Some(".npmrc or workspace config has changed".into());
        }
    }

    // No settings_hash check when cli_flags is None. That path feeds
    // ensure_installed (aube run / exec / test). Those commands do not
    // care about install-shape settings changing because the tree is
    // still the tree built by the last install. Skipping this check
    // also avoids the asymmetry bug where `aube install
    // --node-linker=hoisted` writes a hash with cli_flags set, then
    // bare `aube run` reads without the flag, mismatches, and triggers
    // a spurious auto-install.
    if lockfile_missing
        && restore_lockfile_snapshot(project_dir, &state_path, &state, &lockfile_name).is_none()
    {
        return Some("no lockfile found".into());
    }
    None
}

pub fn restore_missing_lockfile_if_fresh(
    project_dir: &Path,
    cli_flags: &[(String, String)],
) -> bool {
    let (modules_dir, state_path) = resolve_paths(project_dir);
    let (lockfile_name, lockfile_path) = active_lockfile(project_dir);
    if lockfile_path.is_some() || !modules_dir.exists() {
        return false;
    }
    let Some(state) = read_or_migrate_fresh_state(&state_path) else {
        return false;
    };
    if package_jsons_stale(project_dir, &state).is_some()
        || state.section_filtered
        || verify_install_layout(project_dir, state.layout.as_ref()).is_some()
        || hash_settings(project_dir, cli_flags) != state.settings_hash
    {
        return false;
    }
    restore_lockfile_snapshot(project_dir, &state_path, &state, &lockfile_name).is_some()
}

fn package_jsons_stale(project_dir: &Path, state: &FreshnessState) -> Option<String> {
    for (rel, stored_hash) in &state.package_json_hashes {
        let path = if rel == "." {
            project_dir.join("package.json")
        } else {
            project_dir.join(rel)
        };
        if !path.exists() {
            return Some(format!("{rel} is missing"));
        }
        // Fast path: if a `(size, mtime)` snapshot was recorded last
        // install AND it still matches, the file is byte-identical
        // (mtime + size pair is sufficient evidence that nothing was
        // overwritten in place). Skip the BLAKE3 hash entirely. Falls
        // through on schema upgrades where `package_json_meta` is
        // empty.
        if let Some(stored_meta) = state.package_json_meta.get(rel)
            && let Some(current_meta) = FileMeta::capture(&path)
            && current_meta == *stored_meta
        {
            continue;
        }
        if hash_file(&path) == *stored_hash {
            continue;
        }
        let stale_reason = || {
            if rel == "." {
                "package.json has changed".into()
            } else {
                format!("{rel} has changed")
            }
        };
        let Some(stored_shape) = state.package_json_shape_digests.get(rel) else {
            return Some(stale_reason());
        };
        let Ok(content) = std::fs::read(&path) else {
            return Some(stale_reason());
        };
        let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&content);
        let Ok(parsed) = parsed else {
            return Some(stale_reason());
        };
        let current_shape = hex::encode(aube_util::hash::manifest_install_shape_digest(&parsed));
        if current_shape != *stored_shape {
            return Some(stale_reason());
        }
    }
    None
}

/// Write state file after a successful install. `section_filtered` should be
/// `true` when the install omitted dependency sections, so that
/// `check_needs_install` knows to trigger a full re-install before commands
/// that expect the whole graph. `cli_flags` is the install's `opts.cli_flags`
/// bag — threaded through so the stored `settings_hash` reflects CLI overrides
/// (e.g. `--node-linker=hoisted`) that shaped the tree on disk.
pub struct WriteStateLayout<'a> {
    pub graph: &'a aube_lockfile::LockfileGraph,
    pub node_linker: aube_linker::NodeLinker,
    pub modules_dir_name: &'a str,
    pub aube_dir: &'a Path,
    pub virtual_store_dir_max_length: usize,
    pub placements: Option<&'a aube_linker::HoistedPlacements>,
}

pub struct WriteStateInput<'a> {
    pub section_filtered: bool,
    pub package_json_hashes: BTreeMap<String, String>,
    pub cli_flags: &'a [(String, String)],
    pub package_content_hashes: BTreeMap<String, String>,
    pub graph_lthash: String,
    pub package_subtree_hashes: BTreeMap<String, String>,
    pub layout: WriteStateLayout<'a>,
    pub unreviewed_builds: Vec<String>,
}

pub fn write_state(project_dir: &Path, input: WriteStateInput<'_>) -> Result<(), std::io::Error> {
    let WriteStateInput {
        section_filtered,
        package_json_hashes,
        cli_flags,
        package_content_hashes,
        graph_lthash,
        package_subtree_hashes,
        layout,
        unreviewed_builds,
    } = input;

    let state_path = state_dir(project_dir);
    remove_legacy_state_file(&state_path)?;
    let (lockfile_hash, lockfile_snapshot_name) =
        snapshot_active_lockfile(project_dir, &state_path)?;
    let settings_hash = hash_settings(project_dir, cli_flags);
    let install_layout = InstallLayoutState::from_graph(
        project_dir,
        layout.graph,
        layout.node_linker,
        layout.modules_dir_name,
        layout.aube_dir,
        layout.virtual_store_dir_max_length,
        layout.placements,
    );

    let package_json_shape_digests: BTreeMap<String, String> = package_json_hashes
        .keys()
        .filter_map(|rel| {
            let path = if rel == "." {
                project_dir.join("package.json")
            } else {
                project_dir.join(rel)
            };
            let bytes = std::fs::read(&path).ok()?;
            let parsed: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            Some((
                rel.clone(),
                hex::encode(aube_util::hash::manifest_install_shape_digest(&parsed)),
            ))
        })
        .collect();

    // Capture (size, mtime) per manifest so the next freshness check
    // can skip the BLAKE3 hash on the warm path. See R1 docstring on
    // FreshnessState.package_json_meta.
    let package_json_meta: BTreeMap<String, FileMeta> = package_json_hashes
        .keys()
        .filter_map(|rel| {
            let path = if rel == "." {
                project_dir.join("package.json")
            } else {
                project_dir.join(rel)
            };
            FileMeta::capture(&path).map(|m| (rel.clone(), m))
        })
        .collect();

    let state = InstallState {
        lockfile_hash,
        lockfile_snapshot_name,
        package_json_hashes,
        package_json_meta,
        aube_version: env!("CARGO_PKG_VERSION").to_string(),
        section_filtered,
        settings_hash,
        package_content_hashes,
        graph_lthash,
        package_subtree_hashes,
        package_json_shape_digests,
        layout: Some(install_layout),
        unreviewed_builds,
    };

    let fresh_state = FreshnessState::from(&state);
    let json = serde_json::to_string_pretty(&state)?;
    aube_util::fs_atomic::atomic_write(&install_state_file(&state_path), json.as_bytes())?;
    write_fresh_state(&state_path, &fresh_state)?;

    Ok(())
}

fn snapshot_active_lockfile(
    project_dir: &Path,
    state_path: &Path,
) -> Result<(String, Option<String>), std::io::Error> {
    let (name, path) = active_lockfile(project_dir);
    let Some(path) = path else {
        let _ = std::fs::remove_file(lockfile_snapshot_file(state_path));
        return Ok((String::new(), None));
    };
    let Ok(content) = std::fs::read(&path) else {
        let _ = std::fs::remove_file(lockfile_snapshot_file(state_path));
        return Ok((String::new(), None));
    };
    aube_util::fs_atomic::atomic_write(&lockfile_snapshot_file(state_path), &content)?;
    Ok((hash_bytes(&content), Some(name)))
}

/// Read per-package fingerprints from a project's state directory.
/// Returns `None` on any failure path (file missing, malformed
/// JSON, pre-delta aube). Caller treats that as "no prior
/// fingerprints, full install". Never surfaces an error because
/// delta is additive. A miss just lands on the full-install path.
pub fn read_state_package_content_hashes(project_dir: &Path) -> Option<BTreeMap<String, String>> {
    let state = read_state(&state_dir(project_dir))?;
    if state.package_content_hashes.is_empty() {
        return None;
    }
    Some(state.package_content_hashes)
}

/// Read the LtHash accumulator digest the last install wrote, if
/// any. Empty string on fresh state or pre-lthash aube versions.
pub fn read_state_graph_lthash(project_dir: &Path) -> Option<String> {
    let state = read_state(&state_dir(project_dir))?;
    if state.graph_lthash.is_empty() {
        return None;
    }
    Some(state.graph_lthash)
}

/// Read stored subtree hashes for delta installs that want to
/// prune at the subtree granularity rather than the leaf
/// granularity. Absent field cascades to the leaf diff path.
pub fn read_state_subtree_hashes(project_dir: &Path) -> Option<BTreeMap<String, String>> {
    let state = read_state(&state_dir(project_dir))?;
    if state.package_subtree_hashes.is_empty() {
        return None;
    }
    Some(state.package_subtree_hashes)
}

/// Read the unreviewed-builds spec keys recorded by the last
/// install. Powers warm-path warning re-emission so repeat
/// installs keep nudging users about pending build approvals.
/// Returns an empty vec when state is missing or pre-feature.
pub fn read_state_unreviewed_builds(project_dir: &Path) -> Vec<String> {
    read_or_migrate_fresh_state(&state_dir(project_dir))
        .map(|s| s.unreviewed_builds)
        .unwrap_or_default()
}

/// Remove the install state directory. Missing state is not an error.
pub fn remove_state(project_dir: &Path) -> Result<(), std::io::Error> {
    let state_path = state_dir(project_dir);
    let result = if state_path.is_dir() {
        std::fs::remove_dir_all(state_path)
    } else {
        std::fs::remove_file(state_path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Pick the lockfile path that an install in `project_dir` will actually
/// read or write through, mirroring `aube_lockfile::lockfile_candidates`.
///
/// Order:
///   1. `aube-lock.<branch>.yaml` (only if `gitBranchLockfile` is on
///      and we resolve a branch — the preferred value).
///   2. `aube-lock.yaml` — the default base file. Critical for the
///      freshly-enabled-branch case: the branch file hasn't been
///      written yet, but the base file exists, and without this step
///      `check_needs_install` would fall through to pnpm lockfiles
///      (or to `None` on aube-lock projects) and loop on
///      every `aube run` / `aube exec`.
///   3. `pnpm-lock.<branch>.yaml` / `pnpm-lock.yaml`.
///
/// Returns the display name (for messages) plus the resolved path, if
/// any exists.
fn active_lockfile(project_dir: &Path) -> (String, Option<PathBuf>) {
    let basename = aube_util::embedder().lockfile_basename;
    let stem = basename.rsplit_once('.').map_or(basename, |(s, _)| s);
    let preferred = aube_lockfile::aube_lock_filename(project_dir);
    let preferred_path = project_dir.join(&preferred);
    if preferred_path.exists() {
        return (preferred, Some(preferred_path));
    }
    // Freshly-enabled `gitBranchLockfile`: base file exists, branch
    // file does not. Pick up the base so we don't loop on every run.
    if preferred != basename {
        let base = project_dir.join(basename);
        if base.exists() {
            return (basename.to_string(), Some(base));
        }
    }
    // Preserve pnpm-lock.yaml (and its branch variant) as an active
    // lockfile when the project already uses it.
    let pnpm_preferred = preferred.replacen(&format!("{stem}."), "pnpm-lock.", 1);
    if pnpm_preferred != preferred {
        let pnpm_branch = project_dir.join(&pnpm_preferred);
        if pnpm_branch.exists() {
            return (pnpm_preferred, Some(pnpm_branch));
        }
    }
    let pnpm_base = project_dir.join("pnpm-lock.yaml");
    if pnpm_base.exists() {
        return ("pnpm-lock.yaml".to_string(), Some(pnpm_base));
    }
    // Also track npm/yarn/bun lockfiles written by the format-preserving
    // install path, so `check_needs_install` doesn't loop on "no lockfile
    // found" for projects that use these formats.
    for name in [
        "bun.lock",
        "yarn.lock",
        "npm-shrinkwrap.json",
        "package-lock.json",
    ] {
        let path = project_dir.join(name);
        if path.exists() {
            return (name.to_string(), Some(path));
        }
    }
    (preferred, None)
}

fn read_state(state_path: &Path) -> Option<InstallState> {
    if state_path.is_file() {
        let _ = std::fs::remove_file(state_path);
        return None;
    }
    let content = std::fs::read_to_string(install_state_file(state_path)).ok()?;
    serde_json::from_str(&content).ok()
}

fn install_state_file(state_path: &Path) -> PathBuf {
    state_path.join(INSTALL_STATE_FILE_NAME)
}

fn fresh_state_file(state_path: &Path) -> PathBuf {
    state_path.join(FRESH_STATE_FILE_NAME)
}

fn lockfile_snapshot_file(state_path: &Path) -> PathBuf {
    state_path.join(LOCKFILE_SNAPSHOT_FILE_NAME)
}

fn read_fresh_state(state_path: &Path) -> Option<FreshnessState> {
    if state_path.is_file() {
        let _ = std::fs::remove_file(state_path);
        return None;
    }
    let content = std::fs::read_to_string(fresh_state_file(state_path)).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_or_migrate_fresh_state(state_path: &Path) -> Option<FreshnessState> {
    if let Some(state) = read_fresh_state(state_path) {
        return Some(state);
    }
    let state = FreshnessState::from(&read_state(state_path)?);
    let _ = write_fresh_state(state_path, &state);
    Some(state)
}

fn write_fresh_state(state_path: &Path, state: &FreshnessState) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(state)?;
    aube_util::fs_atomic::atomic_write(&fresh_state_file(state_path), json.as_bytes())
}

fn restore_lockfile_snapshot(
    project_dir: &Path,
    state_path: &Path,
    state: &FreshnessState,
    expected_name: &str,
) -> Option<PathBuf> {
    let name = state.lockfile_snapshot_name.as_ref()?;
    if !is_restorable_lockfile_name(name) {
        return None;
    }
    if is_branch_lockfile_name(name) && name != expected_name {
        return None;
    }
    let content = std::fs::read(lockfile_snapshot_file(state_path)).ok()?;
    if hash_bytes(&content) != state.lockfile_hash {
        return None;
    }
    let path = project_dir.join(name);
    aube_util::fs_atomic::atomic_write(&path, &content).ok()?;
    Some(path)
}

fn is_restorable_lockfile_name(name: &str) -> bool {
    let basename = aube_util::embedder().lockfile_basename;
    matches!(
        name,
        "pnpm-lock.yaml" | "bun.lock" | "yarn.lock" | "npm-shrinkwrap.json" | "package-lock.json"
    ) || name == basename
        || is_branch_lockfile_name(name)
}

fn is_branch_lockfile_name(name: &str) -> bool {
    let basename = aube_util::embedder().lockfile_basename;
    let stem = basename.rsplit_once('.').map_or(basename, |(s, _)| s);
    (name.starts_with(&format!("{stem}.")) || name.starts_with("pnpm-lock."))
        && name.ends_with(".yaml")
        && name != basename
        && name != "pnpm-lock.yaml"
}

fn remove_legacy_state_file(state_path: &Path) -> Result<(), std::io::Error> {
    if state_path.is_file() {
        std::fs::remove_file(state_path)?;
    }
    Ok(())
}

impl InstallLayoutState {
    fn from_graph(
        project_dir: &Path,
        graph: &aube_lockfile::LockfileGraph,
        node_linker: aube_linker::NodeLinker,
        modules_dir_name: &str,
        aube_dir: &Path,
        virtual_store_dir_max_length: usize,
        placements: Option<&aube_linker::HoistedPlacements>,
    ) -> Self {
        let linker = match node_linker {
            aube_linker::NodeLinker::Isolated => InstallLayoutMode::Isolated,
            aube_linker::NodeLinker::Hoisted => InstallLayoutMode::Hoisted,
        };
        let mut direct_entries = BTreeMap::new();
        if let Some(deps) = graph.importers.get(".") {
            let mut entries = Vec::with_capacity(deps.len());
            for dep in deps {
                entries.push(project_dir.join(modules_dir_name).join(&dep.name));
            }
            direct_entries.insert(
                ".".to_string(),
                entries
                    .into_iter()
                    .map(|p| relative_path_or_original(&p, project_dir))
                    .collect(),
            );
        }

        let mut packages = BTreeMap::new();
        let direct_dep_paths: std::collections::BTreeSet<String> = graph
            .importers
            .get(".")
            .into_iter()
            .flat_map(|deps| deps.iter().map(|dep| dep.dep_path.clone()))
            .collect();
        for dep_path in direct_dep_paths {
            let Some(pkg) = graph.packages.get(&dep_path) else {
                continue;
            };
            let package_json_path = match pkg.local_source.as_ref() {
                Some(aube_lockfile::LocalSource::Link(path)) => {
                    project_dir.join(path).join("package.json")
                }
                _ => crate::commands::install::materialized_pkg_dir(
                    aube_dir,
                    &dep_path,
                    &pkg.name,
                    virtual_store_dir_max_length,
                    placements,
                )
                .join("package.json"),
            };
            packages.insert(
                dep_path,
                InstalledPackageState {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    package_json_path: relative_path_or_original(&package_json_path, project_dir),
                    package_json_hash: hash_file_if_exists(&package_json_path).unwrap_or_default(),
                },
            );
        }

        Self {
            linker,
            direct_entries,
            packages,
        }
    }
}

fn verify_install_layout(
    project_dir: &Path,
    layout: Option<&InstallLayoutState>,
) -> Option<String> {
    let layout = layout?;
    for entries in layout.direct_entries.values() {
        for rel in entries {
            let path = project_dir.join(rel);
            if !path.exists() {
                return Some(format!("installed entry missing: {rel}"));
            }
        }
    }

    for pkg in layout.packages.values() {
        let pkg_json_path = project_dir.join(&pkg.package_json_path);
        let current_hash = hash_file_if_exists(&pkg_json_path);
        if let Some(current_hash) = current_hash
            && !pkg.package_json_hash.is_empty()
            && pkg.package_json_hash != empty_blake3_hash()
            && current_hash == pkg.package_json_hash
        {
            continue;
        }
        let manifest = match read_installed_package_manifest(&pkg_json_path) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                return Some(format!(
                    "installed package metadata missing: {}",
                    pkg.package_json_path
                ));
            }
            Err(_) => {
                return Some(format!(
                    "installed package metadata unreadable: {}",
                    pkg.package_json_path
                ));
            }
        };
        if manifest.name != pkg.name || manifest.version != pkg.version {
            return Some(format!(
                "installed package metadata changed: {}",
                pkg.package_json_path
            ));
        }
    }

    None
}

#[derive(Deserialize)]
struct InstalledManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

fn read_installed_package_manifest(
    path: &Path,
) -> Result<Option<InstalledManifest>, std::io::Error> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let parsed = serde_json::from_str(&content)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    Ok(Some(parsed))
}

pub fn collect_package_json_hashes_from_manifests(
    project_dir: &Path,
    manifests: &[(String, aube_manifest::PackageJson)],
) -> BTreeMap<String, String> {
    manifests
        .par_iter()
        .filter_map(|(rel, _)| {
            let pkg_json = if rel == "." {
                project_dir.join("package.json")
            } else {
                project_dir.join(rel).join("package.json")
            };
            if !pkg_json.is_file() {
                return None;
            }
            let key = if rel == "." {
                ".".to_string()
            } else {
                relative_path_or_original(&pkg_json, project_dir)
            };
            Some((key, hash_file(&pkg_json)))
        })
        .collect()
}

fn hash_settings(project_dir: &Path, cli_flags: &[(String, String)]) -> String {
    // hash resolved settings not raw file bytes. old byte hash tripped on
    // noop edits like `optimisticRepeatInstall=true` (same as default).
    // resolved values collapse defaults to identical hash. cli flags feed
    // through ctx so `--node-linker=hoisted` also shows up here.
    // workspace yaml bytes still hashed on top, covers map shaped settings
    // like catalog, overrides, packageExtensions, onlyBuiltDependencies
    // where any change means a real re-resolve.
    let files = crate::commands::FileSources::load(project_dir);
    let raw_workspace = aube_manifest::workspace::load_raw(project_dir).unwrap_or_default();
    let env = aube_settings::values::capture_env();
    let ctx = files.ctx(&raw_workspace, &env, cli_flags);
    let mut hasher = blake3::Hasher::new();
    // node_linker, hoist family, modules_dir, import method. these shape
    // the tree on disk. flip any of them, linker needs to rebuild.
    let node_linker = aube_settings::resolved::node_linker(&ctx);
    hasher.update(b"node_linker=");
    hasher.update(format!("{node_linker:?}").as_bytes());
    hasher.update(b"\0");
    let hoist = aube_settings::resolved::hoist(&ctx);
    hasher.update(format!("hoist={hoist}\0").as_bytes());
    let shamefully_hoist = aube_settings::resolved::shamefully_hoist(&ctx);
    hasher.update(format!("shamefully_hoist={shamefully_hoist}\0").as_bytes());
    let hoist_pattern = aube_settings::resolved::hoist_pattern(&ctx);
    hasher.update(b"hoist_pattern=");
    for p in &hoist_pattern {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    let public_hoist_pattern = aube_settings::resolved::public_hoist_pattern(&ctx);
    hasher.update(b"public_hoist_pattern=");
    for p in &public_hoist_pattern {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    let modules_dir = aube_settings::resolved::modules_dir(&ctx);
    hasher.update(format!("modules_dir={modules_dir}\0").as_bytes());
    let package_import_method = aube_settings::resolved::package_import_method(&ctx);
    hasher.update(b"package_import_method=");
    hasher.update(format!("{package_import_method:?}").as_bytes());
    hasher.update(b"\0");
    // enable_global_virtual_store is Option<bool>. Debug format keeps
    // None/Some(true)/Some(false) distinct which matters because Some(false)
    // is user opt out while None is "follow default".
    let enable_gvs = aube_settings::resolved::enable_global_virtual_store(&ctx);
    hasher.update(b"enable_gvs=");
    hasher.update(format!("{enable_gvs:?}").as_bytes());
    hasher.update(b"\0");
    let lockfile_enabled = aube_settings::resolved::lockfile(&ctx);
    hasher.update(format!("lockfile={lockfile_enabled}\0").as_bytes());
    // additional tree shape settings. cover enable_modules_dir flip
    // (pnpm equivalent of --lockfile-only persistent), virtual_store_only,
    // hoist_workspace_packages, dedupe_direct_deps, symlink,
    // disable_global_virtual_store_for_packages. any of these flipping
    // means the tree shape needs rebuild.
    let enable_modules_dir = aube_settings::resolved::enable_modules_dir(&ctx);
    hasher.update(format!("enable_modules_dir={enable_modules_dir}\0").as_bytes());
    let virtual_store_only = aube_settings::resolved::virtual_store_only(&ctx);
    hasher.update(format!("virtual_store_only={virtual_store_only}\0").as_bytes());
    let hoist_workspace_packages = aube_settings::resolved::hoist_workspace_packages(&ctx);
    hasher.update(format!("hoist_workspace_packages={hoist_workspace_packages}\0").as_bytes());
    let hoisting_limits = aube_settings::resolved::hoisting_limits(&ctx);
    hasher.update(b"hoisting_limits=");
    hasher.update(format!("{hoisting_limits:?}").as_bytes());
    hasher.update(b"\0");
    let dedupe_direct_deps = aube_settings::resolved::dedupe_direct_deps(&ctx);
    hasher.update(format!("dedupe_direct_deps={dedupe_direct_deps}\0").as_bytes());
    let symlink = aube_settings::resolved::symlink(&ctx);
    hasher.update(format!("symlink={symlink}\0").as_bytes());
    let disable_gvs_for_packages =
        aube_settings::resolved::disable_global_virtual_store_for_packages(&ctx);
    hasher.update(b"disable_gvs_for_packages=");
    for p in &disable_gvs_for_packages {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    // map shaped workspace settings live in yaml. raw byte hash catches
    // catalog edits, overrides bumps, packageExtensions, allowBuilds list.
    // any of those mean re-resolve is needed, yaml bytes are the source.
    hasher.update(b"workspace_yaml=");
    // The shared `pnpm-workspace.yaml` compatibility surface first, then
    // this tool's own branded YAML (if it has one). Standalone aube:
    // `["pnpm-workspace.yaml", "aube-workspace.yaml"]`.
    let workspace_yaml_files: Vec<&str> = std::iter::once("pnpm-workspace.yaml")
        .chain(aube_util::embedder().workspace_yaml)
        .collect();
    for name in workspace_yaml_files {
        let path = project_dir.join(name);
        hasher.update(name.as_bytes());
        hasher.update(b"\x1f");
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
        hasher.update(b"\x1e");
    }
    hasher.update(b"\0");
    // Raw `.npmrc` bytes. Resolved settings above only cover the
    // install-shape keys we read. A user swapping `registry=` or
    // `//host/:_authToken=` changes what tarballs we would fetch
    // but the resolved-values hash never noticed, so fast path
    // stayed green while the actual source of truth for deps
    // changed. Hashing raw bytes is coarse (comment edits
    // invalidate too) but correct.
    hasher.update(b"npmrc=");
    {
        let mut paths: Vec<PathBuf> = vec![project_dir.join(".npmrc")];
        // User-level `~/.npmrc` also drives `registry=` and `_authToken`
        // (see `aube_registry::config::load_npmrc_entries`). Hash it so
        // a token swap or registry change invalidates the fast-path
        // verdict the same way a project-level edit does.
        if let Some(home) = aube_util::env::home_dir() {
            paths.push(home.join(".npmrc"));
        }
        for path in &paths {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update(b"\x1f");
            if let Ok(bytes) = std::fs::read(path) {
                hasher.update(&bytes);
            }
            hasher.update(b"\x1e");
        }
    }
    hasher.update(b"\0");
    // OS + arch + libc. Optional deps filter by these. Swap host
    // between runs (committed node_modules across machines, shared
    // CI cache volume, Rosetta switch) and the correct prebuilts
    // change. Old fast path did not notice and skipped the install,
    // node_modules had the wrong variant for the active host.
    hasher.update(b"host=");
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(b"\x1f");
    // Piggyback on resolver's runtime libc probe. OS != linux
    // returns empty string, harmless but stable.
    hasher.update(aube_resolver::platform::host_triple().2.as_bytes());
    hasher.update(b"\0");
    // Patches dir. patch-commit and patch-remove touch patches in
    // `<project>/patches/` and `.aube-patches.json`. Old fast path
    // did not hash either. User edits a patch file, next install
    // says up-to-date, node_modules still has old patched content.
    hasher.update(b"patches=");
    let patches_sidecar_name = format!(".{}-patches.json", aube_util::embedder().name);
    let patches_sidecar = project_dir.join(&patches_sidecar_name);
    if let Ok(bytes) = std::fs::read(&patches_sidecar) {
        hasher.update(patches_sidecar_name.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(&bytes);
        hasher.update(b"\x1e");
    }
    let patches_dir = project_dir.join("patches");
    if let Ok(entries) = std::fs::read_dir(&patches_dir) {
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        // Sort so hash is deterministic across filesystems that
        // return dir entries in different order (ext4 vs tmpfs vs
        // NTFS).
        paths.sort();
        for p in paths {
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            hasher.update(name.as_bytes());
            hasher.update(b"\x1f");
            if let Ok(bytes) = std::fs::read(&p) {
                hasher.update(&bytes);
            }
            hasher.update(b"\x1e");
        }
    }
    hasher.update(b"\0");
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn hash_file(path: &Path) -> String {
    // BLAKE3 is 3–5× faster than SHA-256 on the state-check hot path.
    // The `"blake3:"` prefix makes old `"sha256:"` state mismatch on
    // first run after upgrade, which correctly triggers a rebuild.
    let content = std::fs::read(path).unwrap_or_default();
    let hash = blake3::hash(&content);
    format!("blake3:{}", hash.to_hex())
}

fn hash_bytes(content: &[u8]) -> String {
    let hash = blake3::hash(content);
    format!("blake3:{}", hash.to_hex())
}

fn hash_file_if_exists(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|content| {
        let hash = blake3::hash(&content);
        format!("blake3:{}", hash.to_hex())
    })
}

fn empty_blake3_hash() -> &'static str {
    "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
}

#[cfg(test)]
mod tests {
    use super::{
        InstallLayoutMode, InstallLayoutState, InstallState, InstalledPackageState,
        collect_package_json_hashes_from_manifests, empty_blake3_hash, fresh_state_file, hash_file,
        install_state_file, read_or_migrate_fresh_state, relative_path_or_original, remove_state,
        verify_install_layout,
    };
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[test]
    fn relative_path_helper_keeps_original_path_when_diff_fails() {
        let original = Path::new("/tmp/aube-test/package.json");
        let base = Path::new("project/../project");

        assert_eq!(
            relative_path_or_original(original, base),
            original.to_string_lossy()
        );
    }

    #[test]
    fn verify_install_layout_treats_legacy_empty_hash_as_cache_miss() {
        let project_dir = temp_project_dir("legacy-empty-hash");
        let state = InstallState {
            lockfile_hash: String::new(),
            lockfile_snapshot_name: None,
            package_json_hashes: BTreeMap::new(),
            package_json_meta: BTreeMap::new(),
            aube_version: String::new(),
            section_filtered: false,
            settings_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: Some(InstallLayoutState {
                linker: InstallLayoutMode::Isolated,
                direct_entries: BTreeMap::new(),
                packages: BTreeMap::from([(
                    "is-odd@3.0.1".to_string(),
                    InstalledPackageState {
                        name: "is-odd".to_string(),
                        version: "3.0.1".to_string(),
                        package_json_path:
                            "node_modules/.aube/missing/node_modules/is-odd/package.json"
                                .to_string(),
                        package_json_hash: empty_blake3_hash().to_string(),
                    },
                )]),
            }),
            unreviewed_builds: Vec::new(),
        };

        assert_eq!(
            verify_install_layout(&project_dir, state.layout.as_ref()),
            Some(
                "installed package metadata missing: node_modules/.aube/missing/node_modules/is-odd/package.json"
                    .to_string()
            )
        );
    }

    #[test]
    fn collect_package_json_hashes_from_manifests_uses_file_paths_for_workspaces() {
        let project_dir = temp_project_dir("manifest-hash-keys");
        let root_pkg = project_dir.join("package.json");
        let ws_pkg = project_dir.join("packages/foo/package.json");
        std::fs::create_dir_all(ws_pkg.parent().expect("workspace dir"))
            .expect("workspace dir should be creatable");
        std::fs::write(&root_pkg, "{\"name\":\"root\"}").expect("root package.json should write");
        std::fs::write(&ws_pkg, "{\"name\":\"foo\"}").expect("workspace package.json should write");

        let manifests = vec![
            (".".to_string(), aube_manifest::PackageJson::default()),
            (
                "packages/foo".to_string(),
                aube_manifest::PackageJson::default(),
            ),
        ];

        let hashes = collect_package_json_hashes_from_manifests(&project_dir, &manifests);

        assert_eq!(hashes.get("."), Some(&hash_file(&root_pkg)));
        assert_eq!(
            hashes.get("packages/foo/package.json"),
            Some(&hash_file(&ws_pkg))
        );
    }

    #[test]
    fn state_json_migrates_fresh_state_without_delta_maps() {
        let project_dir = temp_project_dir("fresh-migration");
        let state_path = project_dir.join(".aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        let state = InstallState {
            lockfile_hash: "blake3:lock".to_string(),
            lockfile_snapshot_name: None,
            package_json_hashes: BTreeMap::from([(".".to_string(), "blake3:pkg".to_string())]),
            package_json_meta: BTreeMap::new(),
            aube_version: env!("CARGO_PKG_VERSION").to_string(),
            section_filtered: false,
            settings_hash: "blake3:settings".to_string(),
            package_content_hashes: BTreeMap::from([(
                "is-odd@3.0.1".to_string(),
                "blake3:content".to_string(),
            )]),
            graph_lthash: "abcdef".to_string(),
            package_subtree_hashes: BTreeMap::from([(
                "is-odd@3.0.1".to_string(),
                "blake3:subtree".to_string(),
            )]),
            package_json_shape_digests: BTreeMap::from([(".".to_string(), "shape".to_string())]),
            layout: Some(InstallLayoutState {
                linker: InstallLayoutMode::Isolated,
                direct_entries: BTreeMap::new(),
                packages: BTreeMap::new(),
            }),
            unreviewed_builds: Vec::new(),
        };
        let json = serde_json::to_string(&state).expect("state should serialize");
        std::fs::write(install_state_file(&state_path), json).expect("state should write");

        let migrated = read_or_migrate_fresh_state(&state_path).expect("fresh state should load");
        assert_eq!(migrated.lockfile_hash, "blake3:lock");
        let fresh_json = std::fs::read_to_string(fresh_state_file(&state_path))
            .expect("fresh state should write");
        assert!(fresh_json.contains("package_json_hashes"));
        assert!(!fresh_json.contains("package_content_hashes"));
        assert!(!fresh_json.contains("package_subtree_hashes"));
    }

    #[test]
    fn legacy_state_file_is_deleted_instead_of_migrated() {
        let project_dir = temp_project_dir("legacy-file-delete");
        let state_path = project_dir.join(".aube-state");
        std::fs::write(&state_path, "{}").expect("legacy state file should write");

        assert!(read_or_migrate_fresh_state(&state_path).is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn unreviewed_builds_roundtrip_persists_into_fresh_state() {
        use super::read_state_unreviewed_builds;
        let project_dir = temp_project_dir("unreviewed-builds-rt");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        let state = InstallState {
            lockfile_hash: "blake3:lock".to_string(),
            lockfile_snapshot_name: None,
            package_json_hashes: BTreeMap::new(),
            package_json_meta: BTreeMap::new(),
            aube_version: env!("CARGO_PKG_VERSION").to_string(),
            section_filtered: false,
            settings_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: None,
            unreviewed_builds: vec![
                "esbuild@0.21.5".to_string(),
                "better-sqlite3@11.5.0".to_string(),
            ],
        };
        let json = serde_json::to_string(&state).expect("state should serialize");
        std::fs::write(install_state_file(&state_path), json).expect("state should write");
        // First read migrates the fresh sidecar.
        let _ = read_state_unreviewed_builds(&project_dir);
        let unreviewed = read_state_unreviewed_builds(&project_dir);
        assert_eq!(
            unreviewed,
            vec![
                "esbuild@0.21.5".to_string(),
                "better-sqlite3@11.5.0".to_string()
            ]
        );
    }

    #[test]
    fn unreviewed_builds_default_when_field_missing_in_state() {
        use super::read_state_unreviewed_builds;
        let project_dir = temp_project_dir("unreviewed-builds-default");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        // Pre-feature state file with no unreviewed_builds key — the
        // serde default keeps the read path working.
        let legacy_json = r#"{
            "lockfile_hash": "blake3:lock",
            "package_json_hashes": {},
            "aube_version": "0.0.0"
        }"#;
        std::fs::write(install_state_file(&state_path), legacy_json)
            .expect("legacy state should write");
        let unreviewed = read_state_unreviewed_builds(&project_dir);
        assert!(unreviewed.is_empty());
    }

    #[test]
    fn remove_state_deletes_directory_and_legacy_file() {
        let project_dir = temp_project_dir("remove-state");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        std::fs::write(install_state_file(&state_path), "{}").expect("state json should write");

        remove_state(&project_dir).expect("state directory should remove");
        assert!(!state_path.exists());

        std::fs::create_dir_all(state_path.parent().expect("state parent"))
            .expect("state parent should write");
        std::fs::write(&state_path, "{}").expect("legacy state file should write");

        remove_state(&project_dir).expect("legacy state file should remove");
        assert!(!state_path.exists());
    }

    fn temp_project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aube-state-tests-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    #[test]
    fn shape_digest_keeps_fast_path_on_cosmetic_edit() {
        use std::collections::BTreeMap;
        let dir = temp_project_dir("shape-cosmetic");
        let original = r#"{
  "name": "x",
  "dependencies": { "react": "19.0.0" },
  "scripts": { "test": "vitest" }
}"#;
        let pkg_path = dir.join("package.json");
        std::fs::write(&pkg_path, original).unwrap();

        let orig_bytes = std::fs::read(&pkg_path).unwrap();
        let orig_parsed: serde_json::Value = serde_json::from_slice(&orig_bytes).unwrap();
        let orig_shape = hex::encode(aube_util::hash::manifest_install_shape_digest(&orig_parsed));

        let mut pjh = BTreeMap::new();
        pjh.insert(".".to_string(), hash_file(&pkg_path));
        let mut shapes = BTreeMap::new();
        shapes.insert(".".to_string(), orig_shape);
        let state = InstallState {
            lockfile_hash: String::new(),
            lockfile_snapshot_name: None,
            package_json_hashes: pjh,
            package_json_meta: BTreeMap::new(),
            aube_version: env!("CARGO_PKG_VERSION").to_string(),
            section_filtered: false,
            settings_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            package_json_shape_digests: shapes,
            layout: None,
            unreviewed_builds: Vec::new(),
        };
        let reformatted = r#"{
  "name": "x",
  "dependencies": { "react": "19.0.0" },
  "scripts": { "test": "jest" }
}
"#;
        std::fs::write(&pkg_path, reformatted).unwrap();

        let new_bytes = std::fs::read(&pkg_path).unwrap();
        let new_parsed: serde_json::Value = serde_json::from_slice(&new_bytes).unwrap();
        let new_shape = hex::encode(aube_util::hash::manifest_install_shape_digest(&new_parsed));
        assert_eq!(
            new_shape, state.package_json_shape_digests["."],
            "shape digest should ignore scripts + whitespace"
        );
    }
}
