use tracing::{debug, trace, warn};

use crate::patches::apply_multi_file_patch;
use crate::sweep::mkdirp;
use crate::{Error, LinkStats, LinkStrategy, Linker, sys};
use aube_lockfile::LockedPackage;
use aube_store::{PackageIndex, StoredFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

impl Linker {
    /// Detect the best linking strategy for the filesystem at the given path.
    ///
    /// One-arg form. Probes within one dir. Fine when store and
    /// project node_modules share the same mount. Use the two-arg
    /// form for installs where the store lives on a different
    /// filesystem than the project (USB drives, bind mounts, Docker
    /// volumes, cross-drive Windows installs). Otherwise the probe
    /// reports hardlink based on project-FS self-test, then every
    /// real link call crosses an FS boundary and hits EXDEV. Runtime
    /// falls back to `fs::copy` per file silently, thousands of
    /// wasted syscalls, user thinks they got hardlinks.
    ///
    /// Returns `Hardlink` when the probe succeeds, `Copy` otherwise.
    /// Reflink is reachable only through explicit
    /// `packageImportMethod = clone` / `clone-or-copy`; `auto` resolves
    /// to `Hardlink` because hardlink benchmarks faster across every
    /// target reflink supports (APFS clonefile, btrfs/xfs FICLONE).
    pub fn detect_strategy(path: &Path) -> LinkStrategy {
        Self::detect_strategy_cross(path, path)
    }

    /// Two-arg probe. src is the store shard (or any dir on the
    /// store FS), dst is the project modules dir (or any dir on the
    /// destination FS). Probe creates a real cross-mount src file
    /// and tries to hardlink into dst, which catches EXDEV up front.
    /// Returns `Hardlink` when the probe succeeds, `Copy` otherwise.
    pub fn detect_strategy_cross(src_dir: &Path, dst_dir: &Path) -> LinkStrategy {
        // Memoize per (src_dir, dst_dir) for the process lifetime.
        // The probe writes a real test file and tries hardlink,
        // ~2 syscalls + 2 unlinks. Multiple Linker instances within
        // one install (prewarm + final + per-workspace) all repeat
        // the probe; cache the answer.
        type ProbeKey = (std::path::PathBuf, std::path::PathBuf);
        static CACHE: std::sync::OnceLock<
            std::sync::RwLock<std::collections::HashMap<ProbeKey, LinkStrategy>>,
        > = std::sync::OnceLock::new();
        let key = (src_dir.to_path_buf(), dst_dir.to_path_buf());
        let cache = CACHE.get_or_init(Default::default);
        if let Some(hit) = cache.read().expect("probe cache poisoned").get(&key) {
            return *hit;
        }

        let test_src = src_dir.join(".aube-link-test-src");
        let test_dst = dst_dir.join(".aube-link-test-dst");

        let strategy = if std::fs::write(&test_src, b"test").is_ok() {
            let result = if std::fs::hard_link(&test_src, &test_dst).is_ok() {
                LinkStrategy::Hardlink
            } else {
                LinkStrategy::Copy
            };
            let _ = std::fs::remove_file(&test_src);
            let _ = std::fs::remove_file(&test_dst);
            result
        } else {
            LinkStrategy::Copy
        };

        // First-write-wins via `entry().or_insert`. Two concurrent
        // linker probes (prewarm + final) sharing the same
        // (src_dir, dst_dir) can race on the test files: one observes
        // hardlink-ok, the other sees the first writer's leftover and
        // falls back to Copy. `.insert()` would let the wrong Copy
        // result clobber the correct Hardlink for the rest of the
        // process; `or_insert` keeps whichever value landed first.
        *cache
            .write()
            .expect("probe cache poisoned")
            .entry(key)
            .or_insert(strategy)
    }

    /// Materialize a package in the global virtual store if not already present.
    ///
    /// Materialize `dep_path` into the shared global virtual store.
    ///
    /// Uses atomic rename to avoid TOCTOU races: materializes into a
    /// PID-stamped temp directory, then renames into place. If another
    /// process wins the race, its result is kept and the temp dir is
    /// cleaned up.
    ///
    /// Exposed so the install driver can pipeline GVS population into
    /// the fetch phase: as each tarball finishes importing into the
    /// CAS, the driver calls this to reflink the package into its
    /// `~/.cache/aube/virtual-store/<subdir>` entry. Link step 1 then
    /// hits the `pkg_nm_dir.exists()` fast path and only creates the
    /// per-project `.aube/<dep_path>` symlink.
    pub fn ensure_in_virtual_store(
        &self,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        // `link:` transitives the resolver pinned (e.g. via root
        // `pnpm.overrides`) need their on-disk target so the parent's
        // sibling symlink doesn't dangle into a non-existent
        // `.aube/<name>@link+...`. `None` means "no nested links in
        // this graph" and the materialize hot path stays unchanged.
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        let _diag =
            aube_util::diag::Span::new(aube_util::diag::Category::Linker, "ensure_in_vstore")
                .with_meta_fn(|| {
                    format!(
                        r#"{{"name":{},"files":{}}}"#,
                        aube_util::diag::jstr(&pkg.name),
                        index.len()
                    )
                });
        // Global-store paths always run through the vstore_key map —
        // when hashes are installed this folds dep-graph + engine
        // state into the leaf name, so concurrent builds of the same
        // package against different toolchains don't collide.
        let subdir = self.virtual_store_subdir(dep_path);
        let pkg_nm_dir = self
            .virtual_store
            .join(&subdir)
            .join("node_modules")
            .join(&pkg.name);

        if pkg_nm_dir.exists() {
            trace!("virtual store hit: {dep_path}");
            stats.packages_cached += 1;
            return Ok(());
        }

        // Materialize into a temp directory, then atomically rename into place
        // to avoid TOCTOU races between concurrent `aube install` processes.
        // `subdir` already comes from `dep_path_to_filename`, which
        // flattens `/` to `+` as part of its escape pass, so it's
        // already safe to splice into a single path component.
        let tmp_name = format!(".tmp-{}-{subdir}", std::process::id());
        let tmp_base = self.virtual_store.join(&tmp_name);

        let result = self.materialize_into(
            &tmp_base,
            dep_path,
            pkg,
            index,
            stats,
            true,
            nested_link_targets,
        );

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp_base);
            return result;
        }

        // Atomically move the dep_path entry from the temp dir to the final location.
        let tmp_entry = tmp_base.join(&subdir);
        let final_entry = self.virtual_store.join(&subdir);

        // Ensure the parent of the final entry exists (e.g. for scoped packages).
        if let Some(parent) = final_entry.parent() {
            mkdirp(parent)?;
        }

        match aube_util::fs_atomic::rename_with_retry(&tmp_entry, &final_entry) {
            Ok(()) => {
                trace!("atomically placed {subdir} in virtual store");
            }
            Err(e) if final_entry.exists() => {
                // Another process won the race — that's fine, use theirs.
                trace!("lost rename race for {dep_path}, using existing: {e}");
                // Undo the stats from our materialization since we're discarding it
                stats.packages_linked = stats.packages_linked.saturating_sub(1);
                stats.files_linked = stats.files_linked.saturating_sub(index.len());
                stats.packages_cached += 1;
                // Lost-race path: our `subdir` is still inside
                // `tmp_base`, so a full recursive delete is needed.
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Ok(());
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Err(Error::Io(final_entry, e));
            }
        }

        // Successful rename: `tmp_base` is now an empty wrapper directory
        // (its single child was the subdir we just renamed out). Use
        // `remove_dir` instead of `remove_dir_all` — the latter still
        // does the full `opendir`/`fdopendir`(fcntl)/`readdir`/`close`
        // walk even on an empty dir, which dtrace shows as ~6 extra
        // syscalls per package. At 227 packages that's ~1.4k wasted
        // syscalls on every cold install.
        //
        // `remove_dir` fails with `ENOTEMPTY` if a future change to
        // `materialize_into` starts dropping extra files into
        // `tmp_base`. Log at debug so the leak is observable without
        // being fatal; the worst-case outcome is a stray tmp dir, and
        // concurrent-writer races already use the full
        // `remove_dir_all` branch above.
        if let Err(e) = std::fs::remove_dir(&tmp_base) {
            debug!(
                "remove_dir({}) failed, leaving tmp in place: {e}",
                tmp_base.display()
            );
        }

        Ok(())
    }

    /// Materialize a single package directly into the per-project
    /// virtual store at `aube_dir/<dep_path>/node_modules/<name>/`.
    ///
    /// Idempotent: if the entry already exists, counts as cached and
    /// returns. Used by the install-time materializer to pipeline the
    /// link work into the fetch phase under non-GVS mode, so the
    /// dedicated link phase only has to create top-level
    /// `node_modules/<name>` symlinks.
    pub fn ensure_in_aube_dir(
        &self,
        aube_dir: &Path,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        // `materialize_into` batches `create_dir_all` for every parent
        // it needs, so callers don't have to mkdirp the entry's parent
        // (which is just `aube_dir` itself, already created by the
        // materializer driver).
        let entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
        if entry.exists() {
            stats.packages_cached += 1;
            return Ok(());
        }
        self.materialize_into(
            aube_dir,
            dep_path,
            pkg,
            index,
            stats,
            false,
            nested_link_targets,
        )
    }

    /// Materialize a package's files and transitive dep symlinks into a base directory.
    ///
    /// `apply_hashes` controls whether per-dep subdir names are run
    /// through `vstore_key` (the content-addressed name) or used as
    /// raw `dep_path` strings. Global-store callers pass `true` so
    /// the shared `~/.cache/aube/virtual-store/` can hold isolated
    /// copies for each `(deps_hash, engine)` combination;
    /// per-project `.aube/` callers pass `false` because node's
    /// runtime module walk resolves by dep_path only.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn materialize_into(
        &self,
        base_dir: &Path,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        apply_hashes: bool,
        // dep_path → absolute on-disk target for any `link:` packages
        // referenced as transitive deps. When the parent itself is a
        // `file:` Directory or `link:` Link (workspace-style locals),
        // its `package.json` may declare `link:./libs/foo` deps that
        // point inside the parent's source tree. We sidestep the
        // virtual store for those — there is no `.aube/<dep>@link+...`
        // entry — and symlink straight to the on-disk path the
        // resolver pinned. `None` means "no nested link transitives in
        // this graph", which is the common case.
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        validate_package_link_name(&pkg.name)?;
        for dep_name in pkg.dependencies.keys() {
            validate_package_link_name(dep_name)?;
        }
        let subdir = if apply_hashes {
            self.virtual_store_subdir(dep_path)
        } else {
            self.aube_dir_entry_name(dep_path)
        };
        let pkg_nm_dir = base_dir.join(&subdir).join("node_modules").join(&pkg.name);

        // Pre-compute the set of unique parent directories across
        // every file in the index AND every scoped transitive-dep
        // symlink we're about to create, then mkdir them in a single
        // pass. Previously each file looped through `mkdirp(parent)`
        // which always did an `exists()` check (= statx syscall) even
        // though the same parents were shared by dozens of siblings —
        // `materialize_into` for a typical 32-file npm package
        // resulted in ~25 redundant statx calls. Collecting the unique
        // parents first, sorting by length (so ancestors precede
        // descendants), and calling `create_dir_all` once each cuts
        // out the redundant stats entirely. `BTreeSet` sorts
        // lexicographically, which is good enough because every
        // ancestor of a directory is a prefix of it.
        let pkg_nm_parent = base_dir.join(&subdir).join("node_modules");
        // Collect into Vec + sort + dedup instead of BTreeSet. For a
        // package with thousands of files (typescript, next), the
        // BTreeSet's per-insert log-N PathBuf comparison (~50-byte
        // memcmps) was a measurable cost on top of the redundant
        // create_dir_all that the set was deduplicating in the first
        // place.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(index.len() / 4 + 4);
        parents.push(pkg_nm_dir.clone());
        // Validate every key once here. The file-linking loop below
        // walks the same immutable index, so skipping the check
        // there is safe.
        for rel_path in index.keys() {
            validate_index_key(rel_path)?;
            let target = pkg_nm_dir.join(rel_path);
            if let Some(parent) = target.parent() {
                parents.push(parent.to_path_buf());
            }
        }
        // Scoped transitive deps need `pkg_nm_parent/@scope/` to exist
        // before the symlink call; include those parents in the batch.
        for dep_name in pkg.dependencies.keys() {
            if let Some(slash) = dep_name.find('/')
                && dep_name.starts_with('@')
            {
                parents.push(pkg_nm_parent.join(&dep_name[..slash]));
            }
        }
        parents.sort_unstable();
        parents.dedup();
        for parent in &parents {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.clone(), e))?;
        }

        // `materialize_into` always writes into a fresh location
        // (either a `.tmp-<pid>-...` staging dir for the global virtual
        // store or a per-project `.aube/<dep_path>` just created by
        // the caller), so we can skip the `remove_file(dst)` that
        // `link_file` does defensively. Pass `fresh = true` to suppress
        // the unlink syscall on every file. For a 1.4k-package install
        // that's ~45k wasted `unlink` calls on the hot path.
        for (rel_path, stored) in index {
            // Key already validated in the parent-collection loop
            // above. The index is immutable between the two loops.
            let target = pkg_nm_dir.join(rel_path);

            if let Err(e) = self.link_file_fresh(stored, rel_path, &target) {
                if let Error::MissingStoreFile { .. } = &e {
                    invalidate_stale_index_for_package(&self.store, pkg);
                }
                return Err(e);
            }
            stats.files_linked += 1;

            if stored.executable {
                // `create_cas_file` writes every CAS entry as 0o644
                // unconditionally; the only place a CAS entry's
                // shared inode gets the +x bit is the very first
                // `make_executable` call against a hardlinked or
                // reflinked target — that `chmod` upgrades the
                // shared inode for every later linker that points
                // at it. Skipping the call (an earlier optimization)
                // produced 0o644 binaries on cold installs and
                // broke every CLI shipped via npm.
                #[cfg(unix)]
                xx::file::make_executable(&target).map_err(|e| Error::Xx(e.to_string()))?;
            }
        }

        // Apply any user-supplied patch for this `(name, version)`.
        // Patches are applied *after* the files have been linked into
        // the virtual store but *before* transitive symlinks, so the
        // patched bytes live alongside the unpatched ones at a
        // distinct subdir (the graph hash callback is responsible for
        // making sure that's true).
        let patch_key = pkg.spec_key();
        if let Some(patch_text) = self.patches.get(&patch_key) {
            apply_multi_file_patch(&pkg_nm_dir, patch_text)
                .map_err(|msg| Error::Patch(patch_key.clone(), msg))?;
        }

        // Create symlinks for transitive dependencies. Parents for
        // scoped packages were added to the `parents` batch above, so
        // we no longer need a per-symlink mkdirp. We also skip the
        // `symlink_metadata().is_ok()` existence check: callers
        // guarantee the target directory is freshly created (either a
        // `.tmp-<pid>-...` staging dir for the global virtual store or
        // a per-project `.aube/<dep_path>` that the caller just
        // ensured is empty), so nothing can be in the way.
        for (dep_name, dep_version) in &pkg.dependencies {
            let dep_dep_path = format!("{dep_name}@{dep_version}");
            // Skip any dep whose name matches the package being
            // materialized, regardless of version. The symlink would
            // land at `pkg_nm_parent.join(dep_name)` which is exactly
            // `pkg_nm_dir` — the directory we just populated with the
            // package's own files — and `create_dir_link` would fail
            // EEXIST. The skip used to require version-equality too,
            // but published packages occasionally declare a *different*
            // version of themselves as a dep (e.g. `react_ujs@3.3.0`
            // pins `react_ujs@^2.7.1`, an artifact of how its build
            // script generates its package.json). Treat that as a
            // self-reference: `require('<self>')` from inside the
            // package resolves to its own files, matching what npm /
            // pnpm / yarn end up with after their hoisting passes.
            if dep_name == &pkg.name {
                continue;
            }
            let symlink_path = pkg_nm_parent.join(dep_name);
            // `link:` transitive: the resolver pinned an absolute
            // on-disk target. Skip the virtual-store sibling lookup
            // (there is no `.aube/<dep>@link+...` entry for these) and
            // symlink straight at the source directory.
            //
            // Store the absolute target verbatim. A relative path
            // would have to thread two pitfalls at once: the GVS
            // tmp→final rename (link's own depth changes by one) AND
            // macOS `/tmp`→`/private/tmp` symlink expansion (the dir
            // the OS resolves the link from is one level deeper than
            // `self.virtual_store` lexically suggests). Either alone
            // is fixable; together every `pathdiff` variant lands one
            // component off and the link dangles. Sibling symlinks
            // get away with relative paths because both endpoints
            // live inside `base_dir` and move together; nested-link
            // targets are *external* (under `project_dir`) so the
            // tricks that work for siblings don't apply. Windows
            // already uses absolute targets for the same reason (see
            // the `#[cfg(windows)]` block below).
            if let Some(map) = nested_link_targets
                && let Some(abs_target) = map.get(&dep_dep_path)
            {
                sys::create_dir_link(abs_target, &symlink_path)
                    .map_err(|e| Error::Io(symlink_path.clone(), e))?;
                continue;
            }
            // Match the parent's convention: global-store materialization
            // walks sibling subdirs under their hashed names, while the
            // per-project `.aube/` layout uses raw dep_paths.
            let sibling_subdir = if apply_hashes {
                self.virtual_store_subdir(&dep_dep_path)
            } else {
                self.aube_dir_entry_name(&dep_dep_path)
            };
            // Compute the relative path from the symlink's parent to
            // the sibling dep directory. The symlink's parent is
            // `pkg_nm_parent/` for a bare name but
            // `pkg_nm_parent/@scope/` for a scoped one, so we can't
            // hard-code `../..` — doing so would undercount by one
            // level for every scoped transitive dep and produce a
            // dangling link. `pathdiff::diff_paths` walks the
            // difference for us, yielding `../..` for `foo` and
            // `../../..` for `@vue/shared`, both relative to whatever
            // parent `symlink_path` ends up with.
            // `pkg_nm_parent` is `<base_dir>/<subdir>/node_modules/`, so
            // two parents deep brings us to `<base_dir>/` where all
            // sibling subdirs live side-by-side.
            let virtual_root = pkg_nm_parent
                .parent()
                .and_then(Path::parent)
                .unwrap_or(&pkg_nm_parent);
            let sibling_abs = virtual_root
                .join(&sibling_subdir)
                .join("node_modules")
                .join(dep_name);
            let link_parent = symlink_path.parent().unwrap_or(&pkg_nm_parent);
            let target = pathdiff::diff_paths(&sibling_abs, link_parent)
                .unwrap_or_else(|| sibling_abs.clone());

            // GVS materialize writes into `.tmp-<pid>-<subdir>/`, then
            // atomic-renames into `self.virtual_store/<subdir>/`. POSIX
            // symlinks store the relative offset verbatim. Offset stays
            // invariant under the wrapper rename, so the link resolves
            // correctly after the move. Windows junctions resolve the
            // target against `link.parent()` at create time and persist
            // an absolute path, which binds the junction to the tmp
            // wrapper. After rename every sibling link dangles into a
            // gone `.tmp-<pid>-...` path. Fix: on Windows GVS path
            // (`apply_hashes = true`) rewrite the target to point at
            // the final virtual store root so the stored absolute path
            // survives the rename.
            #[cfg(windows)]
            let target = if apply_hashes {
                self.virtual_store
                    .join(&sibling_subdir)
                    .join("node_modules")
                    .join(dep_name)
            } else {
                target
            };

            sys::create_dir_link(&target, &symlink_path)
                .map_err(|e| Error::Io(symlink_path.clone(), e))?;
        }

        stats.packages_linked += 1;
        trace!("materialized {dep_path} ({} files)", index.len());
        Ok(())
    }

    /// Hardlink-or-copy a file into a freshly-created destination.
    /// Assumes `dst` does not exist — callers (`materialize_into`)
    /// always write into a `.tmp-<pid>-...` staging dir or a
    /// just-wiped per-project `.aube/<dep_path>`, so the defensive
    /// `remove_file(dst)` an idempotent variant would need is skipped.
    /// Eliminates one syscall per linked file (~45k on the medium
    /// benchmark fixture).
    pub(crate) fn link_file_fresh(
        &self,
        stored: &StoredFile,
        rel_path: &str,
        dst: &Path,
    ) -> Result<(), Error> {
        #[cfg(target_os = "macos")]
        const SMALL_FILE_COPY_MAX: u64 = 16 * 1024;
        let map_io = |e: std::io::Error| classify_link_error(stored, rel_path, dst, e);
        let missing_source = || Error::MissingStoreFile {
            store_path: stored.store_path.clone(),
            rel_path: rel_path.to_string(),
        };
        // Track the realized strategy (may differ from `self.strategy` when
        // a reflink or hardlink falls back to copy) for diagnostic
        // attribution. Diag emits a `linker.link_<strategy>` event with
        // the per-file duration so the analyzer can break down link cost
        // by realized path: reflink (zero-copy CoW), hardlink (zero-cost
        // metadata link), copy (full byte transfer), or the
        // small-file-copy short circuit on macOS.
        let diag_t0 = aube_util::diag::enabled().then(std::time::Instant::now);
        let realized: &'static str;
        match self.strategy {
            LinkStrategy::Reflink => {
                #[cfg(target_os = "macos")]
                if matches!(stored.size, Some(size) if size <= SMALL_FILE_COPY_MAX) {
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                    if let Some(t0) = diag_t0 {
                        aube_util::diag::event(
                            aube_util::diag::Category::Linker,
                            "link_macos_small_copy",
                            t0.elapsed(),
                            None,
                        );
                    }
                    return Ok(());
                }
                if let Err(e) = reflink_copy::reflink(&stored.store_path, dst) {
                    // Source-missing short-circuit avoids the misleading
                    // "fell back to copy" trace and the redundant copy
                    // attempt that would just ENOENT for the same reason.
                    if !stored.store_path.exists() {
                        return Err(missing_source());
                    }
                    // Fall back to copy on cross-filesystem errors
                    trace!("reflink failed, falling back to copy: {e}");
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                    realized = "reflink_fallback_copy";
                } else {
                    realized = "reflink";
                }
            }
            LinkStrategy::Hardlink => {
                if let Err(e) = std::fs::hard_link(&stored.store_path, dst) {
                    if !stored.store_path.exists() {
                        return Err(missing_source());
                    }
                    // Fall back to copy on cross-filesystem errors (EXDEV)
                    trace!("hardlink failed, falling back to copy: {e}");
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                    realized = "hardlink_fallback_copy";
                } else {
                    realized = "hardlink";
                }
            }
            LinkStrategy::Copy => {
                std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                realized = "copy";
            }
        }

        if let Some(t0) = diag_t0 {
            // `realized` is one of seven static strings; matching is
            // O(1) and the static `&str` keeps the JSONL category compact.
            let name = match realized {
                "reflink" => "link_reflink",
                "reflink_fallback_copy" => "link_reflink_fallback",
                "hardlink" => "link_hardlink",
                "hardlink_fallback_copy" => "link_hardlink_fallback",
                "copy" => "link_copy",
                "macos_small_copy" => "link_macos_small_copy",
                _ => "link_unknown",
            };
            aube_util::diag::event(aube_util::diag::Category::Linker, name, t0.elapsed(), None);
        }
        Ok(())
    }
}

/// Translate a copy failure into the most informative linker error.
/// ENOENT can mean either side of the operation is missing — stat the
/// source CAS shard to attribute it. A missing shard means the cached
/// package index is out of sync with the on-disk store, which the
/// caller can recover from by invalidating the cached index and
/// re-importing the tarball.
fn classify_link_error(
    stored: &StoredFile,
    rel_path: &str,
    dst: &Path,
    err: std::io::Error,
) -> Error {
    if err.kind() == std::io::ErrorKind::NotFound && !stored.store_path.exists() {
        return Error::MissingStoreFile {
            store_path: stored.store_path.clone(),
            rel_path: rel_path.to_string(),
        };
    }
    Error::Io(dst.to_path_buf(), err)
}

/// Best-effort drop the cached package index when materialize discovers
/// its referenced CAS shard is gone. Callers always surface the original
/// `MissingStoreFile` error first; this side effect just makes sure the
/// next install miss `load_index` instead of looping on the same dead
/// reference. If the cache write fails (e.g. permission error), warn
/// loudly so the user knows the auto-recovery didn't take and they need
/// to wipe the index dir by hand (run `aube store path` to find it).
pub(crate) fn invalidate_stale_index_for_package(store: &aube_store::Store, pkg: &LockedPackage) {
    match store.invalidate_cached_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())
    {
        Ok(true) => debug!("invalidated stale index for {}", pkg.spec_key()),
        Ok(false) => {}
        Err(e) => warn!(
            "failed to invalidate stale index for {}: {e}; manual recovery: rm -rf \"$(aube store path)/index\"",
            pkg.spec_key()
        ),
    }
}

/// Defence in depth for the tarball path-traversal class. The
/// primary guard lives in `aube_store::import_tarball`, which
/// refuses malformed entries before they enter the `PackageIndex`.
/// This helper is the last check before `base.join(key)` is
/// written through the linker, so an index loaded from a cache
/// file that predates the store-side validation (or a bug that
/// lets a traversing key slip past it) still cannot produce a
/// file outside the package root.
pub(crate) fn validate_index_key(key: &str) -> Result<(), Error> {
    if key.is_empty()
        || key.starts_with('/')
        || key.starts_with('\\')
        || key.contains('\0')
        || key.contains('\\')
    {
        return Err(Error::UnsafeIndexKey(key.to_string()));
    }
    // Reject any `..` component or Windows drive prefix like `C:`
    // that would make `Path::join` escape the base.
    for component in std::path::Path::new(key).components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(Error::UnsafeIndexKey(key.to_string()));
            }
            std::path::Component::Normal(os) => {
                #[cfg(windows)]
                {
                    if let Some(s) = os.to_str()
                        && s.contains(':')
                    {
                        return Err(Error::UnsafeIndexKey(key.to_string()));
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = os;
                }
            }
            std::path::Component::CurDir => {}
        }
    }
    Ok(())
}

/// Validate a package/dependency alias before it becomes a path below
/// `node_modules`. npm names allow either `name` or `@scope/name`; every
/// other slash shape is a filesystem path, not a package slot.
pub(crate) fn validate_package_link_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name.contains('\0')
        || name.contains('\\')
        || name.starts_with('/')
        || name.starts_with('\\')
    {
        return Err(Error::UnsafePackageName(name.to_string()));
    }
    let parts: Vec<&str> = name.split('/').collect();
    let ok = match parts.as_slice() {
        [bare] => is_safe_package_component(bare),
        [scope, bare] => {
            scope.starts_with('@')
                && scope.len() > 1
                && is_safe_package_component(scope)
                && is_safe_package_component(bare)
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::UnsafePackageName(name.to_string()))
    }
}

#[cfg(test)]
mod package_name_tests {
    use super::*;

    #[test]
    fn validate_package_link_name_accepts_npm_slots() {
        validate_package_link_name("react").unwrap();
        validate_package_link_name("@scope/pkg").unwrap();
    }

    #[test]
    fn validate_package_link_name_rejects_path_shapes() {
        for name in [
            "",
            ".",
            "..",
            "../evil",
            "@scope/../evil",
            "@scope/pkg/extra",
            "/abs",
            "C:evil",
            "pkg\\evil",
            "pkg\0evil",
        ] {
            assert!(
                matches!(
                    validate_package_link_name(name),
                    Err(Error::UnsafePackageName(_))
                ),
                "{name:?} should be rejected"
            );
        }
    }
}

fn is_safe_package_component(component: &str) -> bool {
    if component.is_empty() || matches!(component, "." | "..") {
        return false;
    }
    if component.len() >= 2 && component.as_bytes()[1] == b':' {
        return false;
    }
    !std::path::Path::new(component).components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    })
}
