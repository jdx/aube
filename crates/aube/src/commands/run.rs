use super::ensure_installed;
use aube_manifest::PackageJson;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::io::IsTerminal;
use std::path::Path;

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Script or local binary name.
    ///
    /// Omit on an interactive TTY to pick from `package.json`
    /// scripts. If no script matches, aube falls back to
    /// `node_modules/.bin/<name>`.
    pub script: Option<String>,
    /// Arguments to pass to the script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Don't error if the script is missing from package.json
    #[arg(long)]
    pub if_present: bool,
    /// Forward `--inspect` to a Node-backed script or local binary.
    #[arg(long, value_name = "[[HOST:]PORT]", num_args = 0..=1, require_equals = true, default_missing_value = "")]
    pub inspect: Option<String>,
    /// Forward `--inspect-brk` to a Node-backed script or local binary.
    #[arg(long, value_name = "[[HOST:]PORT]", num_args = 0..=1, require_equals = true, default_missing_value = "")]
    pub inspect_brk: Option<String>,
    /// Continue recursive execution after a script fails.
    ///
    /// Parsed for pnpm compatibility; aube's sequential fanout still
    /// stops on first failure.
    #[arg(long)]
    pub no_bail: bool,
    /// Skip auto-install check
    #[arg(long)]
    pub no_install: bool,
    /// Disable topological sorting (default is on).
    ///
    /// Without this, recursive runs visit packages in a deps-first
    /// order so a `build` script in a shared library finishes before a
    /// dependent app's `build` starts. Pass this to fall back to the
    /// raw workspace-listing order.
    #[arg(long, overrides_with = "sort")]
    pub no_sort: bool,
    /// Run the script in every matched workspace package concurrently.
    ///
    /// Unbounded parallelism. Pair with `--workspace-concurrency=N` to
    /// cap the worker count. Single-package runs ignore this flag.
    /// First non-zero exit fails the whole run, but siblings are
    /// allowed to finish so their output isn't truncated. Child
    /// stdio is piped and lines are emitted with a `<package>: `
    /// prefix; pass `--reporter-hide-prefix` to drop the labels.
    #[arg(long)]
    pub parallel: bool,
    /// Write a recursive run summary file.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long)]
    pub report_summary: bool,
    /// Hide the `<package>: ` label on parallel-run output lines.
    ///
    /// Lines are still piped through aube (so the line breaks are
    /// clean even when many packages run at once), but the source
    /// package isn't named on each line. Sequential runs ignore this
    /// flag.
    #[arg(long)]
    pub reporter_hide_prefix: bool,
    /// Resume recursive execution starting at this package name.
    ///
    /// After the topo sort and `--reverse` are applied, packages
    /// before the named one in the resulting order are skipped. Errors
    /// if the name isn't in the matched workspace set.
    #[arg(long, value_name = "PACKAGE")]
    pub resume_from: Option<String>,
    /// Reverse the recursive execution order (after topo sort).
    ///
    /// Useful for teardown-style scripts where dependents must shut
    /// down before their deps.
    #[arg(long)]
    pub reverse: bool,
    /// Sort recursive packages topologically (this is the default).
    ///
    /// Pass to override an earlier `--no-sort` on the same invocation.
    #[arg(long, overrides_with = "no_sort")]
    pub sort: bool,
    /// Suppress aube's wrapper output while still showing script
    /// stdout/stderr.
    ///
    /// Short alias for the global `--silent` flag; long form is
    /// intentionally omitted to avoid shadowing the global `--silent`
    /// in clap's dispatch.
    #[arg(short = 's')]
    pub silent: bool,
    /// Cap the number of recursive packages running at once.
    ///
    /// Setting this implicitly enables parallel mode at width `N`.
    /// `0` means "use the available CPU count". Without this flag,
    /// `--parallel` stays unbounded.
    #[arg(long, value_name = "N")]
    pub workspace_concurrency: Option<usize>,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

/// Shared args for the lifecycle shortcut commands: `start`, `stop`, `test`,
/// `restart`.
///
/// These forward to a fixed script name so the script name itself
/// is implicit — only the trailing args and `--no-install` are configurable.
#[derive(Debug, Args)]
pub struct ScriptArgs {
    /// Arguments to pass to the script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Skip auto-install check
    #[arg(long)]
    pub no_install: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    run_args: RunArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    run_args.network.install_overrides();
    run_args.lockfile.install_overrides();
    run_args.virtual_store.install_overrides();
    let RunArgs {
        script,
        args,
        no_install,
        no_sort,
        if_present,
        inspect,
        inspect_brk,
        parallel,
        no_bail: _,
        report_summary: _,
        reporter_hide_prefix,
        resume_from,
        reverse,
        silent,
        sort: _,
        workspace_concurrency,
        lockfile: _,
        network: _,
        virtual_store: _,
    } = run_args;
    let silent = silent || super::global_output_flags().silent;
    let script = match script {
        Some(s) => s,
        None => prompt_for_script()?,
    };
    let node_args = node_args_from_run_flags(inspect, inspect_brk);
    // Resolve the project's Node runtime before anything spawns —
    // covers the warm path where no install runs (the install path
    // re-enters ensure() with the lockfile pin, but the OnceCell makes
    // that a no-op if we already resolved here; both read the same
    // version sources so they agree).
    crate::runtime::ensure_for_cwd(&crate::dirs::cwd()?).await?;
    let recursive = RecursiveOpts {
        // pnpm parity: topo sort is on by default. `--sort` and
        // `--no-sort` use clap `overrides_with`, so only one can land
        // as `true`; default-false on both means "use the default",
        // which is sort=on. We invert `no_sort` rather than reading
        // `sort` so the absence of both flags maps to the default.
        sort: !no_sort,
        reverse,
        resume_from,
        workspace_concurrency,
        reporter_hide_prefix,
    };
    run_script_with(
        &script, &args, &node_args, no_install, if_present, parallel, silent, &filter, recursive,
    )
    .await
}

/// Recursive-run knobs surfaced by `RunArgs` / `ExecArgs`. Only relevant
/// when a workspace filter is non-empty (sequential/single-package runs
/// ignore them entirely). Bundled into one struct so `run_script_with`
/// doesn't grow yet another arm of positional args.
#[derive(Debug, Clone)]
pub(crate) struct RecursiveOpts {
    /// Topologically sort matched packages so deps run before dependents.
    /// Default `true` to match pnpm.
    pub sort: bool,
    /// Reverse the (post-sort) execution order. Useful for teardown
    /// scripts where dependents must shut down before their deps.
    pub reverse: bool,
    /// Skip ordered packages until this package name appears, then run
    /// from there onward. Errors if the name isn't in the matched set.
    pub resume_from: Option<String>,
    /// Cap on concurrent package executions. `None` means
    /// "unbounded under `--parallel`, sequential otherwise"; `Some(n)`
    /// implicitly enables parallel mode at width `n` (matching pnpm,
    /// where setting workspace-concurrency parallelizes by itself).
    /// `Some(0)` means "use available CPU count".
    pub workspace_concurrency: Option<usize>,
    /// When set, parallel runs pipe child stdio but emit lines without
    /// a `<package>: ` prefix. Matches pnpm's `--reporter-hide-prefix`.
    /// Sequential runs ignore this — they always inherit stdio.
    pub reporter_hide_prefix: bool,
}

impl Default for RecursiveOpts {
    fn default() -> Self {
        Self {
            sort: true,
            reverse: false,
            resume_from: None,
            workspace_concurrency: None,
            reporter_hide_prefix: false,
        }
    }
}

fn node_args_from_run_flags(inspect: Option<String>, inspect_brk: Option<String>) -> Vec<String> {
    let mut args = Vec::with_capacity(2);
    if let Some(value) = inspect {
        args.push(node_arg("--inspect", &value));
    }
    if let Some(value) = inspect_brk {
        args.push(node_arg("--inspect-brk", &value));
    }
    args
}

fn node_arg(flag: &str, value: &str) -> String {
    if value.is_empty() {
        flag.to_string()
    } else {
        format!("{flag}={value}")
    }
}

/// Prompt the user to pick a script from the project root's
/// `package.json`. Called by `aube run` when no script name is passed.
/// Errors without prompting if stdin is not a TTY — automation should
/// always pass a script name explicitly.
///
/// Reads the manifest as raw JSON so scripts appear in
/// `package.json` definition order (pnpm parity); `PackageJson.scripts`
/// is a `BTreeMap`, which would sort them alphabetically. We then hand
/// off to `run_script_with`, which re-reads the manifest through the
/// typed path — the extra read is one `fs::read` and only happens on
/// the interactive prompt path, so the simpler call graph is worth it.
fn prompt_for_script() -> miette::Result<String> {
    let initial_cwd = crate::dirs::cwd()?;
    let cwd = crate::dirs::find_project_root(&initial_cwd).ok_or_else(|| {
        miette!(
            "no package.json found in {} or any parent directory",
            initial_cwd.display()
        )
    })?;
    let scripts = read_scripts_in_order(&cwd)?;
    if scripts.is_empty() {
        return Err(miette!(
            "no scripts defined in {}",
            cwd.join("package.json").display()
        ));
    }
    if !std::io::stdin().is_terminal() {
        let names: Vec<&str> = scripts.iter().map(|(n, _)| n.as_str()).collect();
        return Err(miette!(
            "aube run: script name required when stdin is not a TTY. Available scripts: {}",
            names.join(", ")
        ));
    }
    let mut picker = demand::Select::new("Select a script to run")
        .description("package.json scripts")
        .filterable(true);
    for (name, cmd) in &scripts {
        let label = format!("{name}: {cmd}");
        picker = picker.option(demand::DemandOption::new(name.clone()).label(&label));
    }
    match picker.run() {
        Ok(name) => Ok(name),
        // Ctrl-C / Esc cancels the prompt — exit silently with the
        // conventional SIGINT code rather than printing a miette error
        // for what was a deliberate user action.
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => std::process::exit(130),
        Err(e) => Err(e)
            .into_diagnostic()
            .wrap_err("failed to read script selection"),
    }
}

/// Read `package.json` and return its `scripts` entries in the order
/// they appear in the file. Relies on the workspace's
/// `serde_json/preserve_order` feature — `serde_json::Value`'s object
/// variant is an `IndexMap` there, so object iteration preserves
/// insertion order. Entries with non-string values (invalid per npm
/// but we don't want to choke on them here) are skipped.
fn read_scripts_in_order(cwd: &Path) -> miette::Result<Vec<(String, String)>> {
    let path = cwd.join("package.json");
    let bytes = std::fs::read(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
    let Some(serde_json::Value::Object(obj)) = value.get("scripts") else {
        return Ok(Vec::new());
    };
    Ok(obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect())
}

pub(crate) async fn run_script(
    script: &str,
    args: &[String],
    no_install: bool,
    if_present: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let silent = super::global_output_flags().silent;
    run_script_with(
        script,
        args,
        &[],
        no_install,
        if_present,
        false,
        silent,
        filter,
        RecursiveOpts::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_script_with(
    script: &str,
    args: &[String],
    node_args: &[String],
    no_install: bool,
    if_present: bool,
    parallel: bool,
    silent: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
    recursive: RecursiveOpts,
) -> miette::Result<()> {
    let initial_cwd = crate::dirs::cwd()?;
    // Walk upward to the nearest `package.json` so `aube run` from a
    // subdirectory picks up the project root's scripts, matching pnpm.
    // Filtered/recursive runs accept a yaml-only workspace root —
    // `run_script_filtered` resolves its own root via
    // `select_workspace_packages` and only needs each member's manifest.
    let cwd = match crate::dirs::find_project_root(&initial_cwd) {
        Some(p) => p,
        None if !filter.is_empty() => {
            crate::dirs::find_workspace_root(&initial_cwd).ok_or_else(|| {
                miette!(
                    "no project (package.json) or workspace root \
                     (aube-workspace.yaml / pnpm-workspace.yaml) found in {} \
                     or any parent directory",
                    initial_cwd.display()
                )
            })?
        }
        None => {
            return Err(miette!(
                "no package.json found in {} or any parent directory",
                initial_cwd.display()
            ));
        }
    };
    let enable_pre_post_scripts = configure_script_settings_for_project(&cwd)?;

    if !filter.is_empty() {
        return run_script_filtered(
            &cwd,
            script,
            args,
            node_args,
            no_install,
            if_present,
            parallel,
            silent,
            filter,
            enable_pre_post_scripts,
            recursive,
        )
        .await;
    }

    let manifest = load_manifest(&cwd)?;
    if !manifest.scripts.contains_key(script) {
        ensure_installed(no_install).await?;
        let bin_path = super::project_modules_dir(&cwd).join(".bin").join(script);
        if bin_path.exists() {
            return super::exec::exec_bin_with_node_args(
                &cwd, &bin_path, script, args, node_args, false,
            )
            .await;
        }
        if if_present {
            return Ok(());
        }
        // Old error was "script not found: foo" with no list. User
        // has to cat package.json to figure out what to type. npm
        // and pnpm both list available scripts here. Do the same.
        // Keep the list on one line when short, separate lines when
        // long, since users often have 20+ scripts in a real project.
        let mut names: Vec<&str> = manifest.scripts.keys().map(String::as_str).collect();
        names.sort_unstable();
        let hint = if names.is_empty() {
            "no scripts defined in package.json".to_string()
        } else {
            format!("available scripts: {}", names.join(", "))
        };
        return Err(miette!("script not found: {script}\n  {hint}"));
    }

    ensure_installed(no_install).await?;
    exec_script_chain(
        &cwd,
        &manifest,
        script,
        args,
        node_args,
        enable_pre_post_scripts,
    )
    .await
}

/// Fan out a script over workspace packages matched by `filter`.
///
/// Ordering: matched packages are sorted topologically by intra-workspace
/// deps (deps before dependents) when `recursive.sort` is true (default).
/// `--reverse` flips that order; `--resume-from <name>` skips ahead to
/// the named package in the post-sort, post-reverse list.
///
/// Concurrency: sequential by default (bail on first non-zero exit).
/// `--parallel` spawns all packages at once and lets siblings finish
/// before reporting the first failure (preserves output integrity).
/// `--workspace-concurrency=N` caps that fanout to N workers and
/// implicitly enables parallel mode if `--parallel` wasn't passed.
/// `N = 0` means "use available CPU count".
#[allow(clippy::too_many_arguments)]
async fn run_script_filtered(
    cwd: &Path,
    script: &str,
    args: &[String],
    node_args: &[String],
    no_install: bool,
    if_present: bool,
    parallel: bool,
    silent: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
    enable_pre_post_scripts: bool,
    recursive: RecursiveOpts,
) -> miette::Result<()> {
    // `cwd` is the nearest ancestor with a `package.json`, which in a
    // monorepo subpackage is the child — not the workspace root. The
    // shared helper walks up to the real workspace root before
    // enumerating packages, so yarn / npm / bun monorepos work from a
    // subpackage.
    let (_root, matched) = super::select_workspace_packages(cwd, filter, "run")?;

    let matched = order_matched_packages(matched, &recursive)?;

    // Install once at the workspace root before fanning out — the
    // isolated linker already materializes every workspace package's
    // deps in a single pass, so per-package reinstalls would just
    // re-check the same lockfile N times.
    ensure_installed(no_install).await?;

    if let Some(concurrency) = effective_concurrency(parallel, recursive.workspace_concurrency) {
        return run_filtered_parallel(
            script,
            args,
            node_args,
            if_present,
            silent,
            enable_pre_post_scripts,
            matched,
            concurrency,
            recursive.reporter_hide_prefix,
            recursive.reverse,
        )
        .await;
    }

    for pkg in &matched {
        let name = pkg
            .name
            .as_deref()
            .unwrap_or_else(|| pkg.dir.to_str().unwrap_or("(unnamed)"));
        if !pkg.manifest.scripts.contains_key(script) {
            let bin_path = super::project_modules_dir(&pkg.dir)
                .join(".bin")
                .join(script);
            if bin_path.exists() {
                if !silent {
                    tracing::info!("aube run: {name} -> {script}");
                }
                super::exec::exec_bin_with_node_args(
                    &pkg.dir, &bin_path, script, args, node_args, false,
                )
                .await?;
                continue;
            }
            if if_present {
                continue;
            }
            return Err(miette!("aube run: package {name} has no `{script}` script"));
        }
        if !silent {
            tracing::info!("aube run: {name} -> {script}");
        }
        exec_script_chain(
            &pkg.dir,
            &pkg.manifest,
            script,
            args,
            node_args,
            enable_pre_post_scripts,
        )
        .await?;
    }
    Ok(())
}

/// Apply topo sort, reverse, and resume-from to a matched-package list.
/// Pulled out so `aube exec -r` can share the exact same ordering rules
/// — divergence between run and exec would surprise pnpm-muscle-memory
/// users on the same monorepo.
pub(crate) fn order_matched_packages(
    mut matched: Vec<aube_workspace::selector::SelectedPackage>,
    recursive: &RecursiveOpts,
) -> miette::Result<Vec<aube_workspace::selector::SelectedPackage>> {
    if recursive.sort {
        matched = aube_workspace::topo::topological_sort(matched);
    }
    if recursive.reverse {
        matched.reverse();
    }
    if let Some(name) = recursive.resume_from.as_deref() {
        // Used by both `aube run -r` and `aube exec -r`, so the
        // message intentionally doesn't name a specific subcommand.
        // Miette already prefixes the diagnostic chain with the
        // command context.
        let idx = matched
            .iter()
            .position(|p| p.name.as_deref() == Some(name))
            .ok_or_else(|| {
                miette!(
                    "--resume-from package `{name}` is not in the matched \
                     workspace set"
                )
            })?;
        matched.drain(..idx);
    }
    Ok(matched)
}

/// Map `--parallel` + `--workspace-concurrency` to a parallel-mode
/// decision: `None` is the default sequential path (no flags, inherit
/// stdio, bail on first failure); `Some(n)` takes the bounded-parallel
/// path (piped/prefixed output, all-tasks-finish, semaphore-capped
/// fanout) at width `n`. `Some(1)` is intentional pnpm parity — an
/// explicit `--workspace-concurrency=1` opts into parallel-path
/// semantics with width 1, even though no two tasks ever run at once.
///
/// `--parallel` alone resolves to [`tokio::sync::Semaphore::MAX_PERMITS`]
/// (= `usize::MAX >> 3`), which is effectively unbounded for any real
/// workspace and avoids the runtime panic `Semaphore::new(usize::MAX)`
/// triggers on its internal `MAX_PERMITS` invariant. Any user-supplied
/// cap is also clamped to that ceiling.
pub(crate) fn effective_concurrency(
    parallel: bool,
    workspace_concurrency: Option<usize>,
) -> Option<usize> {
    match (parallel, workspace_concurrency) {
        // pnpm: `workspace-concurrency=0` means "use available CPUs".
        // `available_parallelism` is documented to never return 0 — but
        // saturate to `1` defensively so a future change to that contract
        // can't silently turn into "sequential" via underflow.
        (_, Some(0)) => Some(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .max(1),
        ),
        (_, Some(n)) => Some(n.min(tokio::sync::Semaphore::MAX_PERMITS)),
        // `--parallel` with no explicit cap preserves the historical
        // unbounded fanout. Power users on a 200-package workspace who
        // upgrade to a version with default-bounded parallel would be
        // surprised by an apparent slowdown; keep the explicit
        // unbounded request honored.
        (true, None) => Some(tokio::sync::Semaphore::MAX_PERMITS),
        (false, None) => None,
    }
}

/// Bounded parallel fanout with topo-aware ordering.
///
/// Each task waits for every intra-workspace dep to finish (success or
/// failure) before claiming a [`tokio::sync::Semaphore`] permit. With
/// `concurrency = N`, that gives at most `N` running at once *and* the
/// dep-before-dependent invariant — without it, a chain `core → lib → app`
/// at `--workspace-concurrency=2` would let `app` start while `lib` is
/// still running once `core` released its slot.
///
/// Other guarantees preserved from the previous implementation:
/// validate every package up front (no orphaned children if a
/// script-name lookup errors mid-spawn), let every started task finish
/// so output isn't truncated by an early bail, and report the first
/// non-zero exit's code through `std::process::exit`. Output mode per
/// task: prefixed (`<pkg>: <line>`, color-rotated) by default;
/// unprefixed but still piped under `--reporter-hide-prefix`.
#[allow(clippy::too_many_arguments)]
async fn run_filtered_parallel(
    script: &str,
    args: &[String],
    node_args: &[String],
    if_present: bool,
    silent: bool,
    enable_pre_post_scripts: bool,
    matched: Vec<aube_workspace::selector::SelectedPackage>,
    concurrency: usize,
    reporter_hide_prefix: bool,
    reverse: bool,
) -> miette::Result<()> {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    let runnable: Vec<_> = matched
        .into_iter()
        .filter_map(|pkg| {
            if pkg.manifest.scripts.contains_key(script) {
                Some(Ok((pkg, None)))
            } else {
                let bin_path = super::project_modules_dir(&pkg.dir)
                    .join(".bin")
                    .join(script);
                if bin_path.exists() {
                    return Some(Ok((pkg, Some(bin_path))));
                }
                if if_present {
                    return None;
                }
                let name = pkg
                    .name
                    .clone()
                    .unwrap_or_else(|| pkg.dir.display().to_string());
                Some(Err(miette!(
                    "aube run: package {name} has no `{script}` script"
                )))
            }
        })
        .collect::<miette::Result<Vec<_>>>()?;

    // Compute prereqs against the post-`if_present` filtered set so a
    // dependent doesn't deadlock waiting on a sibling that was skipped
    // because it had no matching script. Transpose under `--reverse`
    // so dependents wait for their dependents (teardown order); just
    // reversing the slice without flipping the edges is a no-op for
    // any dep-linked workspace.
    let runnable_pkgs: Vec<aube_workspace::selector::SelectedPackage> =
        runnable.iter().map(|(p, _)| p.clone()).collect();
    let prereqs = aube_workspace::topo::compute_prereq_indices(&runnable_pkgs);
    let prereqs = if reverse {
        aube_workspace::topo::transpose_prereqs(&prereqs)
    } else {
        prereqs
    };
    // One sender per package. Dependents subscribe before we move the
    // sender into its task. Sender is moved (not cloned) so that when
    // a task panics before signaling, its sender is the only one and
    // the channel closes — `rx.changed()` returns Err and dependents
    // unblock instead of hanging. Cloning would keep the channel alive
    // via the original copy and make the panic-recovery path unreachable.
    let senders: Vec<tokio::sync::watch::Sender<bool>> = (0..runnable.len())
        .map(|_| tokio::sync::watch::channel(false).0)
        .collect();
    let prereq_rxs_per_task: Vec<Vec<tokio::sync::watch::Receiver<bool>>> = (0..runnable.len())
        .map(|i| prereqs[i].iter().map(|&j| senders[j].subscribe()).collect())
        .collect();

    let sem = Arc::new(Semaphore::new(concurrency));
    let mut tasks: Vec<tokio::task::JoinHandle<miette::Result<std::process::ExitStatus>>> =
        Vec::with_capacity(runnable.len());
    let mut task_names: Vec<String> = Vec::with_capacity(runnable.len());
    let mut senders_iter = senders.into_iter();
    let mut prereq_rxs_iter = prereq_rxs_per_task.into_iter();
    for (index, (pkg, bin_path)) in runnable.into_iter().enumerate() {
        let name = pkg
            .name
            .clone()
            .unwrap_or_else(|| pkg.dir.display().to_string());
        if !silent {
            tracing::info!("aube run: {name} -> {script} (parallel)");
        }
        let output_mode = if reporter_hide_prefix {
            super::run_output::OutputMode::NoPrefix
        } else {
            super::run_output::OutputMode::prefix(pkg.name.as_deref(), index)
        };
        let prereq_rxs = prereq_rxs_iter.next().expect("one rx vec per package");
        let done_tx = senders_iter.next().expect("one sender per package");
        let script = script.to_string();
        let args = args.to_vec();
        let node_args = node_args.to_vec();
        let dir = pkg.dir.clone();
        let manifest = pkg.manifest.clone();
        let sem = Arc::clone(&sem);
        task_names.push(name);
        tasks.push(tokio::spawn(async move {
            // Topo barrier: hold off until every workspace dep has
            // signaled `true` (= finished, regardless of outcome).
            // pnpm's default `--no-bail=false` aborts the whole run on
            // first failure anyway, so the "wait for failed dep too"
            // case only fires under `--no-bail` (parsed but not yet
            // wired) and degrades to "dependent runs against possibly
            // stale dep state" — same as pnpm. A `changed()` Err means
            // the prereq's sender was dropped without sending (panic
            // or aborted JoinHandle); break so dependents unblock
            // instead of hanging — only reachable because each task
            // *owns* (not clones) its sender.
            for mut rx in prereq_rxs {
                while !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            }
            // The semaphore close path is unreachable — `sem` is held
            // here via `Arc` and never closed — so `acquire_owned`
            // failing would mean a logic bug. Surface as a miette
            // error rather than panic so a future refactor that
            // adds explicit closing surfaces gracefully.
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|e| miette!("workspace concurrency semaphore closed: {e}"))?;
            let result = if let Some(bin_path) = bin_path {
                super::exec::exec_bin_status_with_node_args(
                    &dir,
                    &bin_path,
                    &script,
                    &args,
                    &node_args,
                    false,
                    &output_mode,
                )
                .await
            } else {
                exec_script_status_chain(
                    &dir,
                    &manifest,
                    &script,
                    &args,
                    &node_args,
                    enable_pre_post_scripts,
                    &output_mode,
                )
                .await
            };
            // Always signal completion so dependents don't hang on a
            // `changed()` that never fires. `send` on a watch with no
            // subscribers returns Err, which we deliberately ignore.
            let _ = done_tx.send(true);
            result
        }));
    }
    let mut first_err: Option<miette::Report> = None;
    let mut first_exit: Option<i32> = None;
    for (t, name) in tasks.into_iter().zip(task_names) {
        match t.await {
            Ok(Ok(status)) => {
                if !status.success() && first_exit.is_none() {
                    let code = aube_scripts::exit_code_from_status(status);
                    first_exit = Some(code);
                    first_err = Some(miette!(
                        "aube run: `{script}` failed in {name} (exit {code})"
                    ));
                }
            }
            Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
            Ok(Err(_)) => {}
            Err(e) if first_err.is_none() => first_err = Some(miette!("task panicked: {e}")),
            Err(_) => {}
        }
    }
    if let Some(code) = first_exit {
        std::process::exit(code);
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(())
}

pub(crate) fn load_manifest(cwd: &Path) -> miette::Result<PackageJson> {
    PackageJson::from_path(&cwd.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")
}

fn configure_script_settings_for_project(cwd: &Path) -> miette::Result<bool> {
    let files = crate::commands::FileSources::load(cwd);
    let (_, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let ctx = files.ctx(&raw_workspace, &env_snapshot, &[]);
    let enable_pre_post_scripts = aube_settings::resolved::enable_pre_post_scripts(&ctx);
    super::configure_script_settings(&ctx);
    Ok(enable_pre_post_scripts)
}

/// Run `script` if it exists in `manifest.scripts`. Returns `true` if the
/// script was found and executed, `false` if it was missing. Errors only
/// when the script ran but exited non-zero (via `exit`).
pub(crate) async fn exec_optional(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
) -> miette::Result<bool> {
    if !manifest.scripts.contains_key(script) {
        return Ok(false);
    }
    exec_script(cwd, manifest, script, args).await?;
    Ok(true)
}

pub(crate) async fn exec_script(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
) -> miette::Result<()> {
    exec_script_with_node_args(cwd, manifest, script, args, &[]).await
}

/// Build a fully-configured `tokio::process::Command` for running a
/// package.json script. Handles arg quoting, PATH prepend, npm-compat
/// env vars, and INIT_CWD. Shared between `exec_script` (which exits
/// on non-zero) and `exec_script_status` (which returns the status
/// so the parallel path can collect all outcomes). Keeping one place
/// to configure these means future security fixes land once, not
/// twice.
async fn build_script_command(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    cmd: &str,
    args: &[String],
    node_args: &[String],
) -> miette::Result<tokio::process::Command> {
    let cmd = inject_node_args(cmd, node_args);
    let shell_cmd = if args.is_empty() {
        cmd
    } else {
        // Quote each forwarded arg. Args land inside `sh -c "..."` or
        // cmd `/c "..."` which reparses. Unquoted $, backticks, ;, |,
        // () all get interpreted. `aube run echo '$(rm -rf ~)'` would
        // run the subshell. Same npm/pnpm bug class from years ago.
        // shell_quote_arg wraps per-platform. See aube-scripts.
        let mut buf =
            String::with_capacity(cmd.len() + args.iter().map(|a| a.len() + 3).sum::<usize>());
        buf.push_str(&cmd);
        for a in args {
            buf.push(' ');
            buf.push_str(&aube_scripts::shell_quote_arg(a));
        }
        buf
    };

    let bin_dir = super::project_modules_dir(cwd).join(".bin");
    let node_gyp_bin_dir = super::install::node_gyp_bootstrap::lazy_shim_bin_dir(&bin_dir)?;
    let runtime_bin_dirs = crate::runtime::path_entries();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut entries =
        Vec::with_capacity(2 + usize::from(node_gyp_bin_dir.is_some()) + runtime_bin_dirs.len());
    entries.push(bin_dir);
    if let Some(dir) = node_gyp_bin_dir {
        entries.push(dir);
    }
    // The switched Node runtime beats the inherited PATH but loses to
    // project-local bins — scripts running `node` get the pinned
    // version.
    entries.extend(runtime_bin_dirs);
    entries.extend(std::env::split_paths(&path));
    let new_path = std::env::join_paths(entries).unwrap_or(path);
    let script_dir = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        crate::dirs::cwd()?.join(cwd)
    };
    let node_gyp_project_dir =
        crate::dirs::find_workspace_root(&script_dir).unwrap_or_else(|| script_dir.clone());

    // npm-compat env vars. Lifecycle path sets these in
    // aube-scripts::run_root_hook, `aube run` was bare env before.
    // Build scripts that stamp `$npm_package_version` or branch on
    // `$npm_lifecycle_event` got empty strings under `aube run`
    // while working fine under `aube install` postinstall. npm
    // and pnpm set these on every script exec.
    let mut command = aube_scripts::spawn_shell(&shell_cmd);
    crate::runtime::apply_child_env(&mut command);
    command
        .env("PATH", &new_path)
        .current_dir(cwd)
        .env(
            "AUBE_NODE_GYP_EXE",
            std::env::current_exe().into_diagnostic()?,
        )
        .env("AUBE_NODE_GYP_PROJECT_DIR", node_gyp_project_dir)
        .env("npm_lifecycle_event", script)
        .stderr(aube_scripts::child_stderr());
    if let Some(ref name) = manifest.name {
        command.env("npm_package_name", name);
    }
    if let Some(ref version) = manifest.version {
        command.env("npm_package_version", version);
    }
    // INIT_CWD is the dir the user invoked aube from, NOT the
    // project root. node-gyp and prebuild-install key their
    // caches by INIT_CWD to locate the invoking project. Pulling
    // the invocation cwd from dirs::cwd() matches npm and pnpm
    // semantics when run from a subdirectory. Preserve a
    // parent-set value so nested aube calls see the outermost
    // invocation cwd.
    if std::env::var_os("INIT_CWD").is_none() {
        let init_cwd = crate::dirs::cwd().ok().unwrap_or_else(|| cwd.to_path_buf());
        command.env("INIT_CWD", init_cwd);
    }
    Ok(command)
}

fn inject_node_args(cmd: &str, node_args: &[String]) -> String {
    if node_args.is_empty() {
        return cmd.to_string();
    }
    let trimmed = cmd.trim_start();
    let leading_len = cmd.len() - trimmed.len();
    let Some(rest) = trimmed.strip_prefix("node") else {
        return cmd.to_string();
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return cmd.to_string();
    }
    let mut out =
        String::with_capacity(cmd.len() + node_args.iter().map(|arg| arg.len() + 1).sum::<usize>());
    out.push_str(&cmd[..leading_len + 4]);
    for arg in node_args {
        out.push(' ');
        out.push_str(&aube_scripts::shell_quote_arg(arg));
    }
    out.push_str(rest);
    out
}

pub(crate) async fn exec_script_chain(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
    node_args: &[String],
    enable_pre_post_scripts: bool,
) -> miette::Result<()> {
    if enable_pre_post_scripts {
        let pre = format!("pre{script}");
        exec_optional(cwd, manifest, &pre, &[]).await?;
    }
    exec_script_with_node_args(cwd, manifest, script, args, node_args).await?;
    if enable_pre_post_scripts {
        let post = format!("post{script}");
        exec_optional(cwd, manifest, &post, &[]).await?;
    }
    Ok(())
}

async fn exec_script_with_node_args(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
    node_args: &[String],
) -> miette::Result<()> {
    let cmd = manifest
        .scripts
        .get(script)
        .ok_or_else(|| miette!("script not found: {script}"))?;

    let mut command = build_script_command(cwd, manifest, script, cmd, args, node_args).await?;

    let status = command
        .status()
        .await
        .into_diagnostic()
        .wrap_err("failed to execute script")?;

    if !status.success() {
        std::process::exit(aube_scripts::exit_code_from_status(status));
    }

    Ok(())
}

/// Same shell as `exec_script` but returns the `ExitStatus` instead of
/// `std::process::exit`-ing on failure. Used by the `--parallel` path,
/// which needs to collect every task's outcome before deciding how to
/// terminate.
pub(crate) async fn exec_script_status(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
    output_mode: &super::run_output::OutputMode,
) -> miette::Result<std::process::ExitStatus> {
    exec_script_status_with_node_args(cwd, manifest, script, args, &[], output_mode).await
}

pub(crate) async fn exec_script_status_chain(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
    node_args: &[String],
    enable_pre_post_scripts: bool,
    output_mode: &super::run_output::OutputMode,
) -> miette::Result<std::process::ExitStatus> {
    if enable_pre_post_scripts {
        let pre = format!("pre{script}");
        if manifest.scripts.contains_key(&pre) {
            let status = exec_script_status(cwd, manifest, &pre, &[], output_mode).await?;
            if !status.success() {
                return Ok(status);
            }
        }
    }
    let status =
        exec_script_status_with_node_args(cwd, manifest, script, args, node_args, output_mode)
            .await?;
    if !status.success() {
        return Ok(status);
    }
    if enable_pre_post_scripts {
        let post = format!("post{script}");
        if manifest.scripts.contains_key(&post) {
            return exec_script_status(cwd, manifest, &post, &[], output_mode).await;
        }
    }
    Ok(status)
}

async fn exec_script_status_with_node_args(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
    node_args: &[String],
    output_mode: &super::run_output::OutputMode,
) -> miette::Result<std::process::ExitStatus> {
    let cmd = manifest
        .scripts
        .get(script)
        .ok_or_else(|| miette!("script not found: {script}"))?;
    let command = build_script_command(cwd, manifest, script, cmd, args, node_args).await?;
    super::run_output::run_command(command, output_mode).await
}

#[cfg(test)]
mod tests {
    use super::{
        RecursiveOpts, effective_concurrency, inject_node_args, node_args_from_run_flags,
        order_matched_packages,
    };
    use aube_manifest::PackageJson;
    use aube_workspace::selector::SelectedPackage;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn pkg(name: &str, deps: &[&str]) -> SelectedPackage {
        let manifest = PackageJson {
            name: Some(name.to_string()),
            dependencies: deps
                .iter()
                .map(|d| ((*d).to_string(), "*".to_string()))
                .collect::<BTreeMap<_, _>>(),
            ..PackageJson::default()
        };
        SelectedPackage {
            name: Some(name.to_string()),
            version: None,
            private: false,
            dir: PathBuf::from(name),
            manifest,
        }
    }

    fn order_names(out: &[SelectedPackage]) -> Vec<&str> {
        out.iter().map(|p| p.name.as_deref().unwrap()).collect()
    }

    #[test]
    fn no_parallel_no_concurrency_is_sequential() {
        assert_eq!(effective_concurrency(false, None), None);
    }

    #[test]
    fn parallel_alone_uses_semaphore_max_permits() {
        // Backward compat: existing scripts that pass `--parallel`
        // expect effectively unbounded fanout. Use tokio's own
        // `MAX_PERMITS` constant — `Semaphore::new(usize::MAX)` panics
        // because tokio reserves the high bits internally, so this is
        // the largest value we can hand to `Semaphore::new` without
        // crashing on construction.
        assert_eq!(
            effective_concurrency(true, None),
            Some(tokio::sync::Semaphore::MAX_PERMITS)
        );
    }

    #[test]
    fn explicit_concurrency_above_max_permits_is_clamped() {
        // A user passing `--workspace-concurrency=usize::MAX` would
        // otherwise hit the same Semaphore panic. The cap clamps it.
        assert_eq!(
            effective_concurrency(true, Some(usize::MAX)),
            Some(tokio::sync::Semaphore::MAX_PERMITS)
        );
    }

    #[test]
    fn concurrency_overrides_unbounded_parallel() {
        assert_eq!(effective_concurrency(true, Some(4)), Some(4));
    }

    #[test]
    fn concurrency_alone_implies_parallel() {
        // pnpm parity: setting a concurrency cap parallelizes by
        // itself, no separate `--parallel` needed.
        assert_eq!(effective_concurrency(false, Some(3)), Some(3));
    }

    #[test]
    fn concurrency_one_explicit_takes_parallel_path() {
        // pnpm parity: `--workspace-concurrency=1` is documented as
        // "implicitly enables parallel mode at width N". Width 1 means
        // tasks serialize, but the user still gets piped/prefixed
        // output and all-tasks-finish semantics rather than the
        // sequential default's inherited stdio + bail-on-first.
        assert_eq!(effective_concurrency(false, Some(1)), Some(1));
        assert_eq!(effective_concurrency(true, Some(1)), Some(1));
    }

    #[test]
    fn concurrency_zero_picks_cpu_count() {
        let n = effective_concurrency(false, Some(0)).expect("Some on explicit cap");
        assert!(n >= 1, "available_parallelism floor is 1");
    }

    #[test]
    fn order_matched_applies_topo_then_reverse_then_resume() {
        // app -> lib -> core. With sort+reverse, the natural order is
        // [app, lib, core]; --resume-from=lib drops `app`.
        let opts = RecursiveOpts {
            sort: true,
            reverse: true,
            resume_from: Some("lib".to_string()),
            ..RecursiveOpts::default()
        };
        let out = order_matched_packages(
            vec![
                pkg("app", &["lib"]),
                pkg("lib", &["core"]),
                pkg("core", &[]),
            ],
            &opts,
        )
        .unwrap();
        assert_eq!(order_names(&out), vec!["lib", "core"]);
    }

    #[test]
    fn order_matched_resume_from_unknown_package_errors() {
        let opts = RecursiveOpts {
            sort: false,
            resume_from: Some("nonexistent".to_string()),
            ..RecursiveOpts::default()
        };
        let err = order_matched_packages(vec![pkg("a", &[])], &opts).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn order_matched_no_sort_preserves_input_order() {
        let opts = RecursiveOpts {
            sort: false,
            ..RecursiveOpts::default()
        };
        // Without sort, an app-before-lib input stays that way even
        // though app depends on lib.
        let out =
            order_matched_packages(vec![pkg("app", &["lib"]), pkg("lib", &[])], &opts).unwrap();
        assert_eq!(order_names(&out), vec!["app", "lib"]);
    }

    #[test]
    fn node_args_from_flags_supports_optional_values() {
        assert_eq!(
            node_args_from_run_flags(Some(String::new()), Some("0.0.0.0:9230".to_string())),
            vec![
                "--inspect".to_string(),
                "--inspect-brk=0.0.0.0:9230".to_string()
            ]
        );
    }

    #[test]
    fn inject_node_args_only_touches_direct_node_commands() {
        let args = vec!["--inspect".to_string()];
        assert_eq!(
            inject_node_args("node test.js", &args),
            format!(
                "node {} test.js",
                aube_scripts::shell_quote_arg("--inspect")
            )
        );
        assert_eq!(inject_node_args("tsx test.ts", &args), "tsx test.ts");
        assert_eq!(
            inject_node_args("node-gyp rebuild", &args),
            "node-gyp rebuild"
        );
    }
}
