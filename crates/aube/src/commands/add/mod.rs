mod global;
mod manifest;
mod spec;

use super::install;
use clap::Args;
use manifest::{
    AddManifestOptions, collect_workspace_versions, update_manifest_for_add,
    workspace_protocol_override_from_flags,
};
use miette::{Context, IntoDiagnostic, miette};
use spec::parse_pkg_spec;
use std::path::Path;

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    /// Package(s) to add
    pub packages: Vec<String>,
    /// Add as dev dependency
    #[arg(short = 'D', long)]
    pub save_dev: bool,
    /// Pin the exact resolved version (no `^` prefix)
    #[arg(short = 'E', long)]
    pub save_exact: bool,
    /// Install the package globally.
    ///
    /// Installs into the aube/pnpm global directory and links its
    /// binaries into the global bin directory. Mirrors `pnpm add -g`.
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Add as optional dependency
    #[arg(short = 'O', long)]
    pub save_optional: bool,
    /// Pre-approve a dependency's lifecycle scripts as part of the add.
    ///
    /// Writes `allowBuilds: { <pkg>: true }` into the workspace yaml
    /// (or `package.json#aube.allowBuilds`) before the install runs,
    /// so the named package's `preinstall` / `install` / `postinstall`
    /// scripts execute on this invocation. Repeatable — pass the flag
    /// once per package. Mirrors `pnpm add --allow-build=<pkg>`.
    ///
    /// Conflicts with `--no-save`, which only snapshots `package.json`
    /// and the lockfile and would leave an orphaned approval in the
    /// workspace yaml on restore. Also conflicts with `--deny-build` for
    /// the same package name.
    #[arg(
        long = "allow-build",
        value_name = "PKG",
        conflicts_with = "no_save",
        require_equals = true,
        value_parser = parse_allow_build_value,
    )]
    pub allow_build: Vec<String>,
    /// Bypass the [`lowDownloadThreshold`] confirm prompt / refusal for
    /// this invocation.
    ///
    /// `aube add` looks up each candidate's weekly download count and
    /// prompts (interactive) or fails (CI) when the count is below
    /// [`lowDownloadThreshold`]. The flag is intended for the cases
    /// where you've already verified the package out-of-band — adding
    /// a brand-new niche tool, a fresh fork, an internal scratch
    /// package — and don't want the prompt to interrupt scripted
    /// workflows. Does not affect the OSV malicious-package check,
    /// which remains a hard block.
    #[arg(long)]
    pub allow_low_downloads: bool,
    /// Mark a dependency's lifecycle scripts as reviewed and denied.
    ///
    /// Writes `allowBuilds: { <pkg>: false }` into the workspace yaml
    /// (or `package.json#aube.allowBuilds`) before the install runs,
    /// so the named package's lifecycle scripts stay skipped without
    /// tripping `strictDepBuilds=true`. Repeatable — pass the flag
    /// once per package.
    ///
    /// Conflicts with `--no-save`, which only snapshots `package.json`
    /// and the lockfile and would leave an orphaned denial in the
    /// workspace yaml on restore. Also conflicts with `--allow-build` for
    /// the same package name.
    #[arg(
        long = "deny-build",
        value_name = "PKG",
        conflicts_with = "no_save",
        require_equals = true,
        value_parser = parse_deny_build_value,
    )]
    pub deny_build: Vec<String>,
    /// Skip lifecycle scripts (no-op; aube already skips by default).
    #[arg(long, hide = true)]
    pub ignore_scripts: bool,
    /// Install without persisting the dependency to `package.json`.
    ///
    /// Snapshots `package.json` and the lockfile, links the named
    /// packages into `node_modules`, and then restores both files —
    /// so the dependency is usable for the current process but the
    /// project's committed state is untouched.
    ///
    /// Handy for one-off experiments and for scripts that install a
    /// tool transiently. Mirrors `pnpm add --no-save`. Conflicts with
    /// `-g`/`--global`, which has to persist the install to its global
    /// manifest.
    #[arg(long, conflicts_with = "global")]
    pub no_save: bool,
    /// Inverse of `--save-workspace-protocol`.
    ///
    /// Forces the manifest specifier into a registry-style spec
    /// (`^<version>`) for this invocation, even when
    /// `linkWorkspacePackages` matched a local sibling. The install
    /// pipeline still prefers the local workspace copy at resolve
    /// time — this flag only controls what's written to
    /// `package.json`. Mirrors `pnpm add --no-save-workspace-protocol`.
    #[arg(long, overrides_with = "save_workspace_protocol")]
    pub no_save_workspace_protocol: bool,
    /// Save the new dependency into the workspace's default catalog.
    ///
    /// Writes `catalog:` into `package.json` and seeds/upserts the
    /// resolved range under `catalog:` in the workspace yaml. Mirrors
    /// `pnpm add --save-catalog`.
    ///
    /// Workspace and aliased specs (`workspace:*`, `npm:`, `jsr:`) are
    /// never catalogized — the manifest gets the original spec and
    /// the catalog yaml is left alone. If the package is already in
    /// the target catalog, the existing entry is preserved (never
    /// overwritten); the manifest then gets `catalog:` only when the
    /// existing entry is compatible with the user's range.
    ///
    /// Conflicts with `--no-save`: catalog mutations write to the
    /// workspace yaml, which the `--no-save` restore path doesn't
    /// snapshot — combining the two would silently leave an orphaned
    /// catalog entry behind.
    #[arg(long, conflicts_with_all = ["save_catalog_name", "no_save"])]
    pub save_catalog: bool,
    /// Save the new dependency into a *named* catalog.
    ///
    /// Writes the entry to `catalogs.<name>` in the workspace yaml and
    /// `catalog:<name>` into `package.json`. Same workspace/alias
    /// exclusions and `--no-save` conflict as `--save-catalog`. Mirrors
    /// `pnpm add --save-catalog-name=<name>`.
    #[arg(long, value_name = "NAME", conflicts_with = "no_save")]
    pub save_catalog_name: Option<String>,
    /// Add as a peer dependency (written to `peerDependencies` in
    /// package.json).
    ///
    /// By convention you usually pair this with `--save-dev` so the
    /// peer is also installed for local development; that's what pnpm
    /// does.
    #[arg(long, conflicts_with = "save_optional")]
    pub save_peer: bool,
    /// Force the manifest specifier into `workspace:` form for this
    /// invocation, overriding `saveWorkspaceProtocol` from the
    /// workspace yaml / `.npmrc` / env.
    ///
    /// Only meaningful when `linkWorkspacePackages` (or a workspace
    /// sibling already exists for the named package). With this flag
    /// the entry written to `package.json` is `workspace:^` (rolling)
    /// or `workspace:^<version>` (pinned), depending on the resolved
    /// `saveWorkspaceProtocol` value.
    #[arg(long, overrides_with = "no_save_workspace_protocol")]
    pub save_workspace_protocol: bool,
    /// Add the dependency to the workspace root's `package.json`.
    ///
    /// Applies regardless of the current working directory: walks up
    /// from cwd looking for `aube-workspace.yaml`, `pnpm-workspace.yaml`,
    /// or a `package.json` with a `workspaces` field and runs the add
    /// against that directory.
    #[arg(short = 'w', long, conflicts_with = "global")]
    pub workspace: bool,
    /// Allow `add` to run in a workspace root.
    ///
    /// By default aube refuses to add dependencies to the root
    /// `package.json` of a workspace (a directory containing
    /// `aube-workspace.yaml`, `pnpm-workspace.yaml`, or a `package.json`
    /// with a `workspaces` field) because deps added there end up
    /// shared by every package and usually reflect a mistake. Pass
    /// this flag to opt in. Mirrors `pnpm add -W`.
    #[arg(short = 'W', long)]
    pub ignore_workspace_root_check: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    args: AddArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    if !filter.is_empty() && !args.global && !args.workspace {
        return run_filtered(args, &filter).await;
    }

    let AddArgs {
        packages,
        global,
        save_dev,
        save_optional,
        save_exact,
        save_peer,
        save_workspace_protocol,
        no_save_workspace_protocol,
        workspace,
        ignore_scripts: _,
        no_save,
        ignore_workspace_root_check,
        save_catalog,
        save_catalog_name,
        allow_build,
        deny_build,
        allow_low_downloads,
        lockfile,
        network,
        virtual_store,
    } = args;
    let save_catalog_target = save_catalog_name.or_else(|| {
        if save_catalog {
            Some("default".to_string())
        } else {
            None
        }
    });
    let packages = &packages[..];
    if packages.is_empty() {
        return Err(miette!("no packages specified"));
    }
    reject_conflicting_build_flags(&allow_build, &deny_build)?;

    if global {
        return global::run_global(
            packages,
            allow_build,
            deny_build,
            allow_low_downloads,
            lockfile,
            network,
            virtual_store,
        )
        .await;
    }

    // `--workspace` / `-w`: redirect the add at the workspace root
    // (directory containing `aube-workspace.yaml` / `pnpm-workspace.yaml`)
    // before anything reads `dirs::cwd()`. We chdir into it so the
    // downstream install pipeline treats the root as the project.
    if workspace {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = super::find_workspace_root(&start).wrap_err("--workspace")?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to chdir into {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    // pnpm `install <pkg>` (= aube `add <pkg>`) creates an empty
    // package.json when run in a directory with no manifest, so users
    // can bootstrap a project with a single command. Match that: if no
    // ancestor has a package.json (within the home boundary), write a
    // minimal `{}` in cwd before resolving the project root. The
    // `--global`/`-g` path returned earlier; `--workspace` already
    // redirected to a known root above.
    let initial_cwd = crate::dirs::cwd()?;
    if crate::dirs::find_project_root(&initial_cwd).is_none() {
        std::fs::write(initial_cwd.join("package.json"), "{}\n")
            .into_diagnostic()
            .wrap_err("failed to create package.json")?;
    }
    let cwd = crate::dirs::project_root()?;

    // Refuse to add into a workspace root unless the caller opts out.
    // Matches pnpm: deps added here are shared by every workspace
    // package and usually reflect a mistake. `-W` /
    // `--ignore-workspace-root-check` bypasses the check, and `-w` /
    // `--workspace` implies the bypass since the user explicitly
    // targeted the root. We trip on a *declared* package-pattern list,
    // not on the materialized glob — an empty `packages/*` directory
    // is still a workspace root the user should opt into. Bare
    // catalog-only yaml is not a workspace root, and a `package.json`
    // without a `workspaces` field isn't either.
    if !ignore_workspace_root_check && !workspace {
        // `WorkspaceConfig::load` already returns an empty `packages`
        // list when no yaml exists, so propagating errors here only
        // surfaces genuine yaml problems (permission denied, malformed
        // YAML) instead of silently letting `add` proceed against what
        // might actually be a workspace root.
        let ws = aube_manifest::WorkspaceConfig::load(&cwd)
            .into_diagnostic()
            .wrap_err("failed to read workspace config")?;
        let yaml_has_packages = !ws.packages.is_empty();
        // `package.json` read errors fall through intentionally: the
        // install pipeline below re-reads and parses the same file and
        // surfaces a richer miette diagnostic pointing at the offending
        // byte. Duplicating that error here would double-report.
        let pkg_json_has_workspaces =
            aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
                .ok()
                .and_then(|m| m.workspaces)
                .is_some_and(|w| !w.patterns().is_empty());
        if yaml_has_packages || pkg_json_has_workspaces {
            return Err(miette!(
                "refusing to add dependencies to the workspace root. \
                 If this is intentional, pass --ignore-workspace-root-check (-W)."
            ));
        }
    }

    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    // 1. Read existing package.json. Snapshot the raw bytes when
    // `--no-save` is in effect so we can restore both the manifest
    // *and* the lockfile after the resolver/install pipeline (both
    // re-read from disk) has done its work — the user gets the new
    // package linked into `node_modules` while their committed
    // project state stays exactly as they wrote it.
    //
    // The lockfile path matches whatever
    // `write_lockfile_preserving_existing` will write to: detect the
    // existing lockfile kind on disk (pnpm, npm, yarn, bun, …) so a
    // project using `pnpm-lock.yaml` doesn't end up with both a
    // restored aube-lock.yaml *and* a leftover modified pnpm-lock.yaml.
    // When no lockfile exists yet the resolver falls back to aube's
    // own format, so we target that path and the restore step deletes
    // it (since `lockfile_bytes` is `None`).
    let lockfile_path = lockfile_path_for_project(&cwd);
    let no_save_snapshot = if no_save {
        let manifest_bytes = std::fs::read(&manifest_path)
            .into_diagnostic()
            .wrap_err("failed to snapshot package.json for --no-save")?;
        let lockfile_bytes = match std::fs::read(&lockfile_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot lockfile for --no-save");
            }
        };
        Some(NoSaveSnapshot {
            manifest_bytes,
            lockfile_bytes,
        })
    } else {
        None
    };
    // `--allow-build=<pkg>` / `--deny-build=<pkg>` pre-review dep
    // lifecycle scripts as part of the add. The install pipeline
    // re-reads the map from disk, so writing before manifest mutation
    // keeps failure-mode reasoning local.
    if !allow_build.is_empty() {
        apply_allow_build_flags(&cwd, &allow_build)?;
    }
    if !deny_build.is_empty() {
        apply_deny_build_flags(&cwd, &deny_build)?;
    }

    // OSV / downloads gates fire pre-manifest-mutation — they're
    // human-intent signals that key off the typed package names,
    // so a refusal here leaves `package.json` untouched. The
    // Bun-style `securityScanner` is intentionally NOT called
    // here: it runs post-resolve from `install::run` against the
    // full resolved graph (matching Bun's contract), with
    // concrete versions + transitives the OSV/downloads probes
    // wouldn't see at this stage.
    let registry_names = registry_bound_names_for_supply_chain(&cwd, packages);
    let (advisory_check, low_download_threshold, allowed_unpopular) =
        super::with_settings_ctx(&cwd, |ctx| {
            let policy = if aube_settings::resolved::paranoid(ctx) {
                aube_settings::resolved::AdvisoryCheck::Required
            } else {
                aube_settings::resolved::advisory_check(ctx)
            };
            (
                policy,
                aube_settings::resolved::low_download_threshold(ctx),
                aube_settings::resolved::allowed_unpopular_packages(ctx).unwrap_or_default(),
            )
        });
    super::add_supply_chain::run_gates(
        &registry_names,
        advisory_check,
        low_download_threshold,
        allow_low_downloads,
        &allowed_unpopular,
    )
    .await?;

    update_manifest_for_add(
        &cwd,
        packages,
        AddManifestOptions {
            save_dev,
            save_exact,
            save_optional,
            save_peer,
            save_catalog: save_catalog_target,
            workspace_protocol_override: workspace_protocol_override_from_flags(
                save_workspace_protocol,
                no_save_workspace_protocol,
            ),
        },
        !no_save,
    )
    .await?;

    // 4. Run install. It re-reads the mutated package.json, runs the
    // resolver (reusing locked entries for unchanged specs), writes the
    // lockfile, and links node_modules in one pipeline. `Fix` mode is
    // the right semantic here: package.json just gained a new spec,
    // so the lockfile is by definition stale on that one entry — Prefer
    // would risk taking the from-lockfile fast path and missing the
    // new dep. Wrapping in a `Result` so the restore step below runs
    // even on failure — a network error mid-resolve would otherwise
    // leave the mutated `package.json` on disk, breaking `--no-save`.
    // `with_mode()` already skips root lifecycle hooks (chained-call
    // contract) so `aube add` doesn't re-run the root postinstall /
    // prepare on every invocation.
    // `osv_transitive_check = true` routes the resolved transitive
    // set through OSV's `MAL-*` batch query post-resolve, so a
    // malicious dep-of-dep fails the install with the same
    // `ERR_AUBE_MALICIOUS_PACKAGE` as the CLI-name gate above.
    let mut install_opts =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Fix));
    install_opts.osv_transitive_check = true;
    let pipeline_result: miette::Result<()> = install::run(install_opts).await;

    // 5. Under `--no-save`, restore the snapshotted `package.json` and
    // lockfile so neither shows up in `git status`. The user's
    // `node_modules` keeps the freshly linked package — matching
    // pnpm's `--no-save` semantics. We do this regardless of whether
    // the install succeeded so failures still leave the project
    // pristine. If the lockfile didn't exist before, delete the one
    // we just wrote.
    //
    // Both restores are attempted independently — if the manifest
    // write fails, we still try the lockfile restore so the project
    // doesn't get stuck in a half-mutated state. Any errors from this
    // step (and the captured `pipeline_result`) are folded together
    // before returning, so the caller sees the *first* relevant
    // failure rather than silently dropping later ones.
    let restore_errors = if let Some(snapshot) = no_save_snapshot {
        let mut errors: Vec<miette::Report> = Vec::new();
        if let Err(e) = aube_util::fs_atomic::atomic_write(&manifest_path, &snapshot.manifest_bytes)
        {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original package.json after --no-save")
                    .unwrap_err(),
            );
        }
        let lockfile_restore = match &snapshot.lockfile_bytes {
            Some(bytes) => aube_util::fs_atomic::atomic_write(&lockfile_path, bytes),
            None => match std::fs::remove_file(&lockfile_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = lockfile_restore {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original lockfile after --no-save")
                    .unwrap_err(),
            );
        }
        if errors.is_empty() {
            eprintln!("Restored package.json and lockfile (--no-save)");
        }
        errors
    } else {
        Vec::new()
    };

    // Order matters: surface the pipeline error first when present —
    // it's the root cause and the restore errors are downstream
    // fallout. With no pipeline error, surface the first restore
    // failure (subsequent ones are usually variants of the same
    // filesystem problem).
    pipeline_result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}

/// Bytes captured from disk before `aube add --no-save` mutated the
/// manifest and lockfile, used to put both back exactly as the user had
/// them once the install pipeline (which insists on reading from disk)
/// has finished linking `node_modules`.
struct NoSaveSnapshot {
    manifest_bytes: Vec<u8>,
    /// `None` means the lockfile didn't exist before the add — in that
    /// case the restore step deletes whatever the resolver wrote.
    lockfile_bytes: Option<Vec<u8>>,
}

/// Reject empty values for the allow-build flag with pnpm's
/// verbatim error message.
///
/// Catches the explicit empty form `--allow-build=`. The bare form
/// `--allow-build` is rejected upstream by clap (because the arg
/// has no `default_missing_value` and `require_equals = true`), so
/// it never reaches this validator.
///
/// Wording must stay byte-identical to pnpm's: scripts that grep
/// pnpm's stderr for this exact line continue to work after a swap
/// to aube.
fn parse_allow_build_value(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("The --allow-build flag is missing a package name. \
             Please specify the package name(s) that are allowed to run installation scripts."
            .to_string())
    } else {
        Ok(s.to_string())
    }
}

fn parse_deny_build_value(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("The --deny-build flag is missing a package name. \
             Please specify the package name(s) that are denied from running installation scripts."
            .to_string())
    } else {
        Ok(s.to_string())
    }
}

fn reject_conflicting_build_flags(
    allow_build: &[String],
    deny_build: &[String],
) -> miette::Result<()> {
    if allow_build.is_empty() || deny_build.is_empty() {
        return Ok(());
    }

    let mut overlap: Vec<&str> = allow_build
        .iter()
        .filter(|name| deny_build.contains(name))
        .map(String::as_str)
        .collect();
    overlap.sort_unstable();
    overlap.dedup();
    if overlap.is_empty() {
        return Ok(());
    }

    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_CONFLICTING_BUILD_FLAGS,
        "--allow-build and --deny-build both name the same package(s): {}. \
         Each package may only appear in one flag.",
        overlap.join(", ")
    ))
}

/// Apply `--allow-build=<pkg>` flags by writing each package as `true`
/// to the project's `allowBuilds` map (workspace yaml or
/// `package.json#aube.allowBuilds`), overwriting any prior value. An
/// explicit `false` is treated as something the user is now flipping
/// on purpose, not a conflict.
fn apply_allow_build_flags(cwd: &std::path::Path, names: &[String]) -> miette::Result<()> {
    aube_manifest::workspace::add_to_allow_builds(cwd, names)
        .into_diagnostic()
        .wrap_err("failed to write --allow-build entries")?;
    Ok(())
}

/// Apply `--deny-build=<pkg>` flags by writing each package as `false`
/// to the project's `allowBuilds` map, overwriting any prior value.
fn apply_deny_build_flags(cwd: &std::path::Path, names: &[String]) -> miette::Result<()> {
    aube_manifest::workspace::set_allow_builds(cwd, names, false)
        .into_diagnostic()
        .wrap_err("failed to write --deny-build entries")?;
    Ok(())
}

fn registry_bound_names_for_supply_chain(cwd: &Path, packages: &[String]) -> Vec<String> {
    let mut names = Vec::with_capacity(packages.len());
    let workspace_versions = collect_workspace_versions(cwd);
    // Scope→registry overrides + the default registry tell us which
    // names route through public npmjs. Anything else (a swapped-out
    // default registry, an `@myorg:registry=https://internal/`
    // override) has no signal in the OSV `MAL-*` database or the
    // npmjs weekly-downloads API — skip those names so private
    // packages don't trip the gates on a public-registry collision.
    let npm_config = aube_registry::config::NpmConfig::load(cwd);
    for raw in packages {
        let Ok(spec) = parse_pkg_spec(raw) else {
            // Parse failures get a richer diagnostic from
            // `update_manifest_for_add` later — we don't want to
            // double-report or block the gate on something that
            // would already fail.
            continue;
        };
        if spec.git_spec.is_some()
            || spec.local_spec.is_some()
            || spec.jsr_name.is_some()
            || aube_util::pkg::is_workspace_spec(&spec.range)
            || aube_util::pkg::is_catalog_spec(&spec.range)
        {
            continue;
        }
        // A bare `aube add my-pkg` against a local workspace sibling
        // resolves locally — no public registry round-trip happens,
        // so the OSV / downloads probes have nothing to say.
        if workspace_versions.contains_key(&spec.name) {
            continue;
        }
        if !npm_config.is_public_npmjs(&spec.name) {
            // `redact_url` strips any embedded userinfo (`https://tok@host/`
            // — uncommon but a registry URL can legally carry it) so a
            // token doesn't slip into observability pipelines that ingest
            // debug-level structured logs.
            tracing::debug!(
                "skipping supply-chain gates for {}: routes through non-public registry {}",
                spec.name,
                aube_util::url::redact_url(npm_config.registry_for(&spec.name))
            );
            continue;
        }
        // Scoped names (`@scope/name`) stay in the list. OSV's batch
        // API supports scoped queries — skipping them here would let
        // a `MAL-*` advisory against `@scope/evil` slip past the
        // hard block. The downloads probe already folds scoped
        // packages into `DownloadCount::Unknown` (npm's downloads
        // API doesn't index them), so the prompt naturally skips
        // them — no per-name special case needed in the gate.
        names.push(spec.name);
    }
    names.sort();
    names.dedup();
    names
}

/// Resolve the on-disk lockfile path that a normal `add` would write
/// to in `project_dir`. Mirrors the `LockfileKind` -> filename mapping
/// inside `aube_lockfile::write_lockfile_as` so the snapshot/restore
/// path under `--no-save` lines up byte-for-byte with whatever
/// `write_lockfile_preserving_existing` produces, including non-aube
/// lockfiles (`pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`,
/// `bun.lock`, `npm-shrinkwrap.json`). When no lockfile exists yet the
/// resolver falls back to aube's own format.
fn lockfile_path_for_project(project_dir: &std::path::Path) -> std::path::PathBuf {
    use aube_lockfile::LockfileKind;
    let kind =
        aube_lockfile::detect_existing_lockfile_kind(project_dir).unwrap_or(LockfileKind::Aube);
    let filename = match kind {
        LockfileKind::Aube => aube_lockfile::aube_lock_filename(project_dir),
        LockfileKind::Pnpm => aube_lockfile::pnpm_lock_filename(project_dir),
        other => other.filename().to_string(),
    };
    project_dir.join(filename)
}

async fn run_filtered(
    args: AddArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if args.packages.is_empty() {
        return Err(miette!("no packages specified"));
    }
    reject_conflicting_build_flags(&args.allow_build, &args.deny_build)?;
    let cwd = crate::dirs::cwd()?;
    // The workspace root — not the child `cwd` — is what owns the
    // lockfile and the project lock in yarn / npm / bun monorepos.
    // Taking the lock or snapshotting the lockfile against `cwd` would
    // target a stale subpackage path, letting `install::run` (which
    // walks up) mutate the real root lockfile and then silently skip
    // the restore under `--no-save`.
    let (root, matched) = super::select_workspace_packages(&cwd, filter, "add")?;
    let _lock = super::take_project_lock(&root)?;

    // CLI build review flags write against the workspace root (where
    // `allowBuilds` lives) — same as the non-filtered path. Run before
    // any per-package manifest mutation so a failure can't leave the
    // child manifests half-mutated.
    if !args.allow_build.is_empty() {
        apply_allow_build_flags(&root, &args.allow_build)?;
    }
    if !args.deny_build.is_empty() {
        apply_deny_build_flags(&root, &args.deny_build)?;
    }

    // OSV / downloads gates fire once against the workspace root
    // — every filter-matched importer shares the same
    // `args.packages` list. The Bun-style `securityScanner` is
    // NOT called here: it runs post-resolve from `install::run`
    // against the full resolved graph.
    let registry_names = registry_bound_names_for_supply_chain(&root, &args.packages);
    let (advisory_check, low_download_threshold, allowed_unpopular) =
        super::with_settings_ctx(&root, |ctx| {
            let policy = if aube_settings::resolved::paranoid(ctx) {
                aube_settings::resolved::AdvisoryCheck::Required
            } else {
                aube_settings::resolved::advisory_check(ctx)
            };
            (
                policy,
                aube_settings::resolved::low_download_threshold(ctx),
                aube_settings::resolved::allowed_unpopular_packages(ctx).unwrap_or_default(),
            )
        });
    super::add_supply_chain::run_gates(
        &registry_names,
        advisory_check,
        low_download_threshold,
        args.allow_low_downloads,
        &allowed_unpopular,
    )
    .await?;

    let mut snapshots = Vec::new();
    let lockfile_path = lockfile_path_for_project(&root);
    let root_lockfile_snapshot = if args.no_save {
        match std::fs::read(&lockfile_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot lockfile for --no-save");
            }
        }
    } else {
        None
    };

    let result: miette::Result<()> = async {
        for pkg in &matched {
            let manifest_path = pkg.dir.join("package.json");
            if args.no_save {
                let manifest_bytes = std::fs::read(&manifest_path)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot package.json for --no-save")?;
                snapshots.push((manifest_path.clone(), manifest_bytes));
            }
            update_manifest_for_add(
                &pkg.dir,
                &args.packages,
                AddManifestOptions::from_args(&args),
                !args.no_save,
            )
            .await?;
        }

        let mut install_opts = install::InstallOptions::with_mode(super::chained_frozen_mode(
            install::FrozenMode::Fix,
        ));
        install_opts.workspace_filter = filter.clone();
        // See the sibling `aube add` codepath above for why this
        // flag is set — live OSV API on the resolved transitives.
        install_opts.osv_transitive_check = true;
        install::run(install_opts).await?;
        Ok(())
    }
    .await;

    let restore_errors = if args.no_save {
        let mut errors: Vec<miette::Report> = Vec::new();
        let restored = snapshots.len();
        for (manifest_path, manifest_bytes) in snapshots {
            if let Err(e) = aube_util::fs_atomic::atomic_write(&manifest_path, &manifest_bytes) {
                errors.push(
                    Result::<(), _>::Err(e)
                        .into_diagnostic()
                        .wrap_err_with(|| {
                            format!(
                                "failed to restore original package.json after --no-save at {}",
                                manifest_path.display()
                            )
                        })
                        .unwrap_err(),
                );
            }
        }
        let lockfile_restore = match &root_lockfile_snapshot {
            Some(bytes) => aube_util::fs_atomic::atomic_write(&lockfile_path, bytes),
            None => match std::fs::remove_file(&lockfile_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = lockfile_restore {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original lockfile after --no-save")
                    .unwrap_err(),
            );
        }
        if errors.is_empty() {
            eprintln!(
                "Restored {} and lockfile (--no-save)",
                pluralizer::pluralize("package.json file", restored as isize, true)
            );
        }
        errors
    } else {
        Vec::new()
    };

    result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}
