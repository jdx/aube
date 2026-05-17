mod argv;
mod cli_args;
mod commands;
mod dep_chain;
mod deprecations;
mod dirs;
mod engines;
mod patches;
mod pnpmfile;
mod progress;
mod state;
mod update_check;
mod version;

// mimalloc as global allocator on release builds. Cuts linker-phase
// wall time and peak RSS on large installs. Per-thread heaps suit
// rayon work-stealing and tokio's blocking pool. Gated on
// `not(debug_assertions)` so `cargo run` and `cargo test` keep the
// system allocator, which keeps Valgrind, ASAN, and Miri happy.
// `secure` feature skipped. aube's hot path is tarball extraction
// with bounded input, not a sandbox boundary.
#[cfg(all(feature = "mimalloc", not(debug_assertions)))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use argv::{extract_config_overrides, lift_per_subcommand_flags, rewrite_multicall_argv};
use clap::{Parser, Subcommand, ValueEnum};
use miette::{Context, IntoDiagnostic, miette};
use std::ffi::OsString;
use std::path::PathBuf;
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[command(
    name = "aube",
    about = "A fast Node.js package manager",
    version = version::VERSION_LONG.as_str(),
    disable_version_flag = true
)]
pub(crate) struct Cli {
    /// Change to directory before running (like `make -C` or `mise --cd`)
    #[arg(short = 'C', long = "dir", visible_aliases = ["cd", "prefix"], global = true, value_name = "DIR")]
    dir: Option<std::path::PathBuf>,

    /// Scope command execution to workspace packages matching PATTERN.
    ///
    /// Supports exact names (`my-pkg`), globs (`@scope/*`, `*-plugin`),
    /// paths (`./packages/api`), graph selectors (`pkg...`, `...pkg`),
    /// git-ref selectors (`[origin/main]`), and exclusions (`!pkg`).
    /// Repeatable; matches are OR-ed.
    ///
    /// Currently honored by `run`, `test`, `start`, `stop`, `restart`,
    /// `install`, `exec`, `list`, `publish`, `deploy`, `add`, `remove`,
    /// `update`, `why`, and implicit-script invocations.
    #[arg(short = 'F', long, global = true, value_name = "PATTERN")]
    filter: Vec<String>,

    /// Run the command across every workspace package.
    ///
    /// Equivalent to `--filter=*`; if `--filter` is also given,
    /// `--recursive` is a no-op and the explicit filter wins. Honored
    /// by the same commands as `--filter`.
    #[arg(short = 'r', long, global = true)]
    recursive: bool,

    /// Enable verbose/debug logging (shortcut for `--loglevel debug`)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Print version and check for updates.
    ///
    /// Manual flag so we can run the async update notifier alongside
    /// the version print — clap's auto `Action::Version` exits inside
    /// `parse_from`, before the tokio runtime is built.
    #[arg(short = 'V', long = "version", global = true)]
    version: bool,

    /// Group workspace command output after each package finishes.
    ///
    /// Accepted for pnpm compatibility; aube's workspace fanout is
    /// currently sequential, so output is already grouped.
    #[arg(long, global = true, conflicts_with = "stream", hide = true)]
    aggregate_output: bool,

    /// Force colored output even when stderr is not a TTY.
    ///
    /// Overrides `NO_COLOR` / `CLICOLOR=0`. Mutually exclusive with
    /// `--no-color`.
    #[arg(long, global = true, conflicts_with = "no_color")]
    color: bool,

    /// Enable cold-install deep diagnostics. Modes:
    ///   summary  — sum_ms / mean / max / %wall table at end
    ///   trace    — summary + critical path + starvation + what-if + lifecycle
    ///   live     — like trace, plus print every span >= 100ms to stderr live
    ///   full     — like trace, plus write JSONL trace to a file (defaults to ./aube-diag.jsonl)
    ///
    /// Quick form: `--diag` with no value defaults to `trace`.
    /// Output file path can be set via `--diag-file`. Threshold for live
    /// mode via `--diag-threshold-ms`.
    #[arg(long, global = true, value_name = "MODE", num_args = 0..=1, default_missing_value = "trace")]
    diag: Option<String>,

    /// Path for `--diag full` JSONL trace (default: ./aube-diag.jsonl)
    #[arg(long, global = true, value_name = "PATH")]
    diag_file: Option<PathBuf>,

    /// Live-mode threshold: only print spans whose duration is >= N ms (default 100).
    #[arg(long, global = true, value_name = "MS")]
    diag_threshold_ms: Option<u64>,

    /// Error when a workspace selector matches no packages.
    ///
    /// Accepted globally; selected commands already fail on empty matches.
    #[arg(long, global = true)]
    fail_if_no_match: bool,

    /// Production-only variant of `--filter`.
    ///
    /// Same selector grammar as `--filter`, but graph walks (`pkg...`,
    /// `...pkg`) only follow `dependencies` / `optionalDependencies` /
    /// `peerDependencies` edges — `devDependencies` (and packages
    /// reachable solely through them) are skipped. Non-graph forms
    /// (exact name, glob, path, `[git-ref]`) behave identically to
    /// `--filter`. Repeatable; can be combined with `--filter`.
    #[arg(long, global = true, value_name = "PATTERN")]
    filter_prod: Vec<String>,

    /// Ignore workspace discovery for commands that support workspace fanout.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, global = true, hide = true)]
    ignore_workspace: bool,

    /// Include the workspace root in recursive workspace operations.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, global = true, hide = true)]
    include_workspace_root: bool,

    /// Set the log level. Logs at or above this level are shown.
    #[arg(long, global = true, value_name = "LEVEL", value_enum)]
    loglevel: Option<LogLevel>,

    /// Disable colored output.
    ///
    /// Overrides `FORCE_COLOR` / `CLICOLOR_FORCE` and sets `NO_COLOR=1`
    /// so downstream libraries (miette, clx, child processes) all see
    /// the same choice.
    #[arg(long, global = true)]
    no_color: bool,

    /// Output format: default, append-only, ndjson, silent.
    ///
    /// `default` renders the progress UI when stderr is a TTY;
    /// `append-only` disables the progress UI in favor of plain
    /// line-at-a-time logs; `ndjson` swaps the tracing fmt layer for
    /// the JSON formatter (one JSON object per log event on stderr)
    /// and is what tooling wrappers should consume; `silent`
    /// suppresses all non-error output (alias for `--loglevel silent`).
    #[arg(long, global = true, value_name = "NAME", value_enum)]
    reporter: Option<ReporterType>,

    /// Suppress all non-error output (alias for `--loglevel silent`)
    #[arg(long, global = true)]
    silent: bool,

    /// Stream workspace command output as each child process writes it.
    ///
    /// Accepted for pnpm compatibility; aube's workspace fanout is
    /// currently sequential.
    #[arg(long, global = true, conflicts_with = "aggregate_output", hide = true)]
    stream: bool,

    /// Route lifecycle and workspace command output through stderr.
    ///
    /// Accepted for pnpm compatibility.
    #[arg(long, global = true, hide = true)]
    use_stderr: bool,

    /// Prefer workspace packages when resolving dependencies.
    ///
    /// Parsed for pnpm compatibility; aube already resolves workspace
    /// packages when a workspace is present.
    #[arg(long, global = true, hide = true)]
    workspace_packages: bool,

    /// Run from the workspace root regardless of the current package.
    #[arg(long, global = true)]
    workspace_root: bool,

    /// Automatically answer yes to prompts.
    ///
    /// Parsed for pnpm compatibility; aube does not currently prompt
    /// on these paths.
    #[arg(short = 'y', long, global = true, hide = true)]
    yes: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Silent,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub(crate) enum ReporterType {
    Default,
    AppendOnly,
    Ndjson,
    Silent,
}

impl LogLevel {
    fn filter(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::Silent => "off",
        }
    }
}

/// Redirects stderr (fd 2) to `/dev/null` for its lifetime, restoring the
/// original on drop. Used by `--silent` to suppress the ~230 direct
/// `eprintln!` calls scattered across command implementations without
/// rewriting them all. The guard must be dropped *before* `main` returns
/// so that any `miette` error report bubbled up through `?` is printed to
/// the real stderr. Stdout is left alone — `aube --silent config get foo`
/// should still emit data to a pipe.
struct SilentStderrGuard {
    saved: libc::c_int,
}

impl SilentStderrGuard {
    fn install() -> Option<Self> {
        unsafe {
            let saved = libc::dup(2);
            if saved < 0 {
                return None;
            }
            let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if devnull < 0 {
                libc::close(saved);
                return None;
            }
            if libc::dup2(devnull, 2) < 0 {
                libc::close(devnull);
                libc::close(saved);
                return None;
            }
            libc::close(devnull);
            Some(Self { saved })
        }
    }
}

impl Drop for SilentStderrGuard {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved, 2);
            libc::close(self.saved);
        }
    }
}

// Commands are listed in alphabetical order; validated by
// `cli_ordering_tests::test_cli_ordering`. Per-command arg fields are
// similarly sorted: positional first, then short flags by short option,
// then long-only flags alphabetically. The `External` catch-all is last
// because clap's external_subcommand must come after named variants; it
// has no fixed name so the sort check skips it.
#[derive(Subcommand)]
enum Commands {
    /// Bootstrap aube's cached node-gyp and print the executable path.
    #[command(name = "__node-gyp-bootstrap", hide = true)]
    NodeGypBootstrap { project_dir: PathBuf },
    /// Add a dependency
    #[command(visible_alias = "a")]
    Add(commands::add::AddArgs),
    /// Approve ignored dependency build scripts.
    ///
    /// Writes entries under `allowBuilds` in `aube-workspace.yaml` (or
    /// `pnpm-workspace.yaml` if present).
    ApproveBuilds(commands::approve_builds::ApproveBuildsArgs),
    /// Check installed packages against the registry advisory DB
    #[command(after_long_help = commands::audit::AFTER_LONG_HELP)]
    Audit(commands::audit::AuditArgs),
    /// Print the path to `node_modules/.bin`
    #[command(after_long_help = commands::bin::AFTER_LONG_HELP)]
    Bin(commands::bin::BinArgs),
    /// Inspect and manage the packument metadata cache
    Cache(commands::cache::CacheArgs),
    /// Print a file from the global store by integrity or hex hash
    CatFile(commands::cat_file::CatFileArgs),
    /// Print the cached package index JSON for `<name>@<version>`
    CatIndex(commands::cat_index::CatIndexArgs),
    /// Verify installed packages can resolve their declared deps.
    ///
    /// Walks the `node_modules/` symlink tree and confirms every
    /// dependency in each `package.json` resolves to a real entry.
    #[command(after_long_help = commands::check::AFTER_LONG_HELP)]
    Check(commands::check::CheckArgs),
    /// Clean install: delete node_modules, then install with frozen lockfile.
    ///
    /// Use in CI to guarantee a reproducible install from the committed lockfile.
    #[command(visible_alias = "clean-install", aliases = ["ic", "install-clean"])]
    Ci(commands::ci::CiArgs),
    /// Remove `node_modules` across every workspace project.
    ///
    /// `--lockfile` / `-l` also deletes lockfiles. A `clean` script in
    /// the root `package.json` overrides the built-in.
    Clean(commands::clean::CleanArgs),
    /// Generate shell completions (bash, zsh, fish)
    Completion(commands::completion::CompletionArgs),
    /// Read and write settings in `.npmrc`
    #[command(alias = "c")]
    Config(commands::config::ConfigArgs),
    /// Scaffold a project from a `create-*` starter kit (via dlx)
    Create(commands::create::CreateArgs),
    /// Re-resolve the lockfile to collapse duplicate versions
    Dedupe(commands::dedupe::DedupeArgs),
    /// Deploy a workspace package into a target directory with deps inlined
    Deploy(commands::deploy::DeployArgs),
    /// Mark published versions of a package as deprecated on the registry
    Deprecate(commands::deprecate::DeprecateArgs),
    /// Report deprecated packages in the resolved dependency graph
    Deprecations(commands::deprecations::DeprecationsArgs),
    /// Diagnostic trace analysis (compare/analyze JSONL traces)
    Diag(commands::diag::DiagArgs),
    /// Manage package distribution tags on the registry
    #[command(visible_alias = "dist-tags")]
    DistTag(commands::dist_tag::DistTagArgs),
    /// Fetch a package into a throwaway environment and run its binary
    Dlx(commands::dlx::DlxArgs),
    /// Run broad install-health diagnostics
    #[command(after_long_help = commands::doctor::AFTER_LONG_HELP)]
    Doctor(commands::doctor::DoctorArgs),
    /// Execute a locally installed binary
    #[command(visible_alias = "x")]
    Exec(commands::exec::ExecArgs),
    /// Download lockfile dependencies into the store without linking node_modules
    Fetch(commands::fetch::FetchArgs),
    /// List packages whose cached index references a given file hash
    #[command(after_long_help = commands::find_hash::AFTER_LONG_HELP)]
    FindHash(commands::find_hash::FindHashArgs),
    /// Alias for `config get` (hidden; prefer `config get`)
    #[command(hide = true)]
    Get(commands::config::GetArgs),
    /// Print packages whose install scripts were skipped by `pnpm.allowBuilds`
    #[command(after_long_help = commands::ignored_builds::AFTER_LONG_HELP)]
    IgnoredBuilds(commands::ignored_builds::IgnoredBuildsArgs),
    /// Convert a supported lockfile into aube-lock.yaml
    Import(commands::import::ImportArgs),
    /// Create a `package.json` in the current directory
    Init(commands::init::InitArgs),
    /// Install all dependencies
    #[command(alias = "i")]
    Install(commands::install::InstallArgs),
    /// Install dependencies, then run the `test` script (pnpm compat alias).
    ///
    /// Hidden from help because `aube test` already auto-installs.
    #[command(alias = "it", hide = true)]
    InstallTest(commands::run::ScriptArgs),
    /// Alias for `list --long` (hidden; prefer `list --long`)
    #[command(hide = true)]
    La(commands::list::ListArgs),
    /// Report the licenses of installed dependencies
    #[command(after_long_help = commands::licenses::AFTER_LONG_HELP)]
    Licenses(commands::licenses::LicensesArgs),
    /// Link a local package globally, or into the current project
    #[command(visible_alias = "ln")]
    Link(commands::link::LinkArgs),
    /// Print the installed dependency tree
    #[command(visible_alias = "ls", after_long_help = commands::list::AFTER_LONG_HELP)]
    List(commands::list::ListArgs),
    /// Alias for `list --long` (hidden; prefer `list --long`)
    #[command(hide = true)]
    Ll(commands::list::ListArgs),
    /// Store a registry auth token in the user's ~/.npmrc
    #[command(alias = "adduser")]
    Login(commands::login::LoginArgs),
    /// Remove a registry auth token from the user's ~/.npmrc
    Logout(commands::logout::LogoutArgs),
    /// Report dependencies whose installed version lags behind the registry
    #[command(after_long_help = commands::outdated::AFTER_LONG_HELP)]
    Outdated(commands::outdated::OutdatedArgs),
    /// Manage package owners (not implemented — use `npm owner`)
    #[command(hide = true)]
    Owner(commands::npm_fallback::FallbackArgs),
    /// Create a publishable `.tgz` tarball from the current project
    Pack(commands::pack::PackArgs),
    /// Extract a package into an edit directory so it can be patched
    Patch(commands::patch::PatchArgs),
    /// Generate a `.patch` file from a `aube patch` edit directory
    PatchCommit(commands::patch_commit::PatchCommitArgs),
    /// Remove patch entries from `pnpm.patchedDependencies`
    PatchRemove(commands::patch_remove::PatchRemoveArgs),
    /// Inspect peer-dependency resolution from the lockfile
    Peers(commands::peers::PeersArgs),
    /// Manage package.json entries (not implemented — use `npm pkg`)
    #[command(hide = true)]
    Pkg(commands::npm_fallback::FallbackArgs),
    /// Remove extraneous packages from node_modules
    Prune(commands::prune::PruneArgs),
    /// Publish the current package to the registry
    Publish(commands::publish::PublishArgs),
    /// Alias for `clean` — remove `node_modules` across every workspace project.
    ///
    /// A `purge` script in the root `package.json` overrides the built-in.
    Purge(commands::clean::CleanArgs),
    /// Query packages in the resolved dependency graph
    #[command(after_long_help = commands::query::AFTER_LONG_HELP)]
    Query(commands::query::QueryArgs),
    /// Re-run root lifecycle scripts and allowlisted dependency builds
    #[command(visible_alias = "rb")]
    Rebuild(commands::rebuild::RebuildArgs),
    /// Run a supported command across workspace packages
    #[command(visible_aliases = ["multi", "m"])]
    Recursive(commands::recursive::RecursiveArgs),
    /// Remove a dependency
    #[command(visible_alias = "rm", aliases = ["uninstall", "un", "uni"])]
    Remove(commands::remove::RemoveArgs),
    /// Restart a package (shortcut for `run restart`; falls back to `stop` + `start`)
    Restart(commands::run::ScriptArgs),
    /// Print the path to `node_modules`
    #[command(after_long_help = commands::root::AFTER_LONG_HELP)]
    Root(commands::root::RootArgs),
    /// Run a script defined in package.json
    #[command(alias = "run-script")]
    Run(commands::run::RunArgs),
    /// Generate a Software Bill of Materials (CycloneDX or SPDX)
    Sbom(commands::sbom::SbomArgs),
    /// Search the registry for packages (not implemented — use `npm search`)
    #[command(hide = true)]
    Search(commands::npm_fallback::FallbackArgs),
    /// Alias for `config set` (hidden; prefer `config set`)
    #[command(hide = true)]
    Set(commands::config::SetArgs),
    /// Set a `package.json` script (not implemented — use `npm set-script`)
    #[command(hide = true, name = "set-script")]
    SetScript(commands::npm_fallback::FallbackArgs),
    /// Start a package (shortcut for `run start`)
    Start(commands::run::ScriptArgs),
    /// Stop a package (shortcut for `run stop`)
    Stop(commands::run::ScriptArgs),
    /// Manage the global store
    Store(commands::store::StoreArgs),
    /// Run the `test` script (shortcut for `run test`)
    #[command(visible_alias = "t")]
    Test(commands::run::ScriptArgs),
    /// Manage registry auth tokens (not implemented — use `npm token`)
    #[command(hide = true)]
    Token(commands::npm_fallback::FallbackArgs),
    /// Clear an existing deprecation on the registry
    Undeprecate(commands::undeprecate::UndeprecateArgs),
    /// Unlink a package (remove linked entries from node_modules)
    #[command(alias = "dislink")]
    Unlink(commands::unlink::UnlinkArgs),
    /// Remove a package (or a single version) from the registry
    Unpublish(commands::unpublish::UnpublishArgs),
    /// Update dependencies
    #[command(aliases = ["up", "upgrade"])]
    Update(commands::update::UpdateArgs),
    /// Print a usage.jdx.dev KDL spec for the CLI (internal)
    #[command(hide = true)]
    Usage,
    /// Bump the version in package.json (and optionally create a git commit + tag)
    Version(commands::version::VersionArgs),
    /// Print package metadata from the registry
    #[command(visible_aliases = ["info", "show"], alias = "v", after_long_help = commands::view::AFTER_LONG_HELP)]
    View(commands::view::ViewArgs),
    /// Report the current registry user (not implemented — use `npm whoami`)
    #[command(hide = true)]
    Whoami(commands::npm_fallback::FallbackArgs),
    /// Print reverse dependency chains explaining why a package is installed
    #[command(visible_alias = "w", after_long_help = commands::why::AFTER_LONG_HELP)]
    Why(commands::why::WhyArgs),
    /// Catch-all for implicit script execution (e.g., `aube dev` = `aube run dev`)
    #[command(external_subcommand)]
    External(Vec<String>),
}

fn main() {
    // Two-phase wrapper: `inner_main` runs the real CLI and returns
    // `Result<(), miette::Report>`. On Err we render via miette's
    // fancy handler (matching the previous `Termination` behavior),
    // then look up the diagnostic's `code()` against
    // `aube_codes::exit::EXIT_TABLE` to pick a bespoke exit code.
    // Codes outside the table fall through to `EXIT_GENERIC` (1).
    //
    // Chain a panic hook that flushes the diag buffer before the
    // default hook prints the panic. Without this, a debug-build panic
    // (release uses `panic = "abort"` so the hook would not run anyway)
    // would lose the BufWriter's 64 KiB tail and any unflushed events.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        aube_util::diag::flush();
        prev_hook(info);
    }));
    let result = inner_main();
    aube_util::diag::flush();
    // Drain any in-flight slow-metadata group whose debounce window
    // hasn't fired yet. install pipelines also flush at end-of-resolve
    // (for in-progress UX), but non-install commands — `aube add`,
    // `aube audit`, `aube deprecate`, `aube deprecations`, `aube view`,
    // etc. — never hit that path and would otherwise silently lose
    // their slow-fetch warnings to the accumulator.
    aube_registry::slow_metadata::flush_summary();
    if let Err(report) = result {
        eprintln!("{report:?}");
        std::process::exit(report_exit_code(&report));
    }
}

/// Resolve a diagnostic's exit code by walking its `code()` chain.
/// Falls back to `EXIT_GENERIC` (1) when no `code` is set or the
/// reported code has no entry in `aube_codes::exit::EXIT_TABLE`.
fn report_exit_code(report: &miette::Report) -> i32 {
    if let Some(code) = report.code() {
        let code = code.to_string();
        if let Some(exit) = aube_codes::exit::exit_code_for(&code) {
            return exit;
        }
    }
    aube_codes::exit::EXIT_GENERIC
}

fn inner_main() -> miette::Result<()> {
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    // pnpm-compat: pull `--config.<key>[=<value>]` out of argv before
    // clap parses it. Stripping here means the rest of the binary sees
    // a clean argv, and the parsed pairs feed every `ResolveCtx::cli`
    // through the process-global slot in `aube_settings`.
    let config_overrides = extract_config_overrides(&mut argv);
    aube_settings::set_global_cli_overrides(config_overrides);
    let cli = Cli::parse_from(lift_per_subcommand_flags(rewrite_multicall_argv(argv)));

    // `--color` / `--no-color` take effect before anything else touches
    // color state: we translate the flags into the env vars that miette,
    // clx, `supports-color`, and spawned child processes all already
    // consult, so the choice is consistent across every output path and
    // inherits into `run` / `exec` / lifecycle scripts. The explicit flag
    // wins over whatever was in the environment — that's what pnpm does.
    //
    // This has to happen *before* we build the Tokio runtime: the Rust
    // 2024 contract on `std::env::set_var` requires that no other
    // threads exist, and a multi-threaded runtime spawns its worker
    // pool during `build()`. So we keep `main` synchronous, mutate env
    // here, and only then enter the async body.
    let color_mode = resolve_color_mode(&cli);
    if matches!(color_mode, ColorMode::Never) {
        // SAFETY: single-threaded `main` — no other threads exist yet.
        unsafe {
            std::env::set_var("NO_COLOR", "1");
            std::env::remove_var("FORCE_COLOR");
            std::env::remove_var("CLICOLOR_FORCE");
        }
    } else if matches!(color_mode, ColorMode::Always) {
        // SAFETY: single-threaded `main` — no other threads exist yet.
        unsafe {
            std::env::set_var("FORCE_COLOR", "1");
            std::env::set_var("CLICOLOR_FORCE", "1");
            std::env::remove_var("NO_COLOR");
        }
    } else if ci_renders_ansi() && !env_disables_color() {
        // Auto + a CI runner whose log viewer renders ANSI, and the
        // user hasn't opted out via NO_COLOR / CLICOLOR=0: stderr isn't
        // a TTY so console/clx would default to plain text. Flip color
        // on for stderr only via console's per-stream override — that's
        // the stream the install progress heartbeat writes to.
        // Deliberately *not* setting FORCE_COLOR / CLICOLOR_FORCE:
        // those are process-wide and would also colorize stdout (e.g.
        // `aube view --json > out.json` baking escapes into the file)
        // and propagate into lifecycle scripts.
        console::set_colors_enabled_stderr(true);
    }

    // `--use-stderr` / `.npmrc` `useStderr=true`: redirect stdout to stderr
    // so all output goes through a single fd. Resolved here (single-threaded)
    // before the tokio runtime spawns workers.
    //
    // Skip when `--silent` is active: the SilentStderrGuard later redirects
    // fd 2 to /dev/null, and if we dup2 first, fd 1 would capture the real
    // stderr and escape silencing.
    let is_silent = cli.silent || matches!(cli.reporter, Some(ReporterType::Silent));
    if !is_silent {
        let use_stderr_active = cli.use_stderr
            || startup_cwd(&cli).ok().is_some_and(|cwd| {
                let files = commands::FileSources::load(&cwd);
                let ws = std::collections::BTreeMap::new();
                let env_snap = aube_settings::values::capture_env();
                aube_settings::resolved::use_stderr(&files.ctx(&ws, &env_snap, &[]))
            });
        if use_stderr_active {
            // SAFETY: single-threaded `main` — no other threads exist yet.
            // `dup2(stderr, stdout)` makes fd 1 point at the same file as fd 2.
            unsafe {
                libc::dup2(2, 1);
            }
        }
    }

    /*
     * High core boxes don't need 64-128 worker threads for an I/O
     * pipeline. Default worker_threads = num_cpus and
     * max_blocking_threads = 512 are both wasteful. Cap workers at
     * 8 (install semaphore already gates network).
     *
     * Blocking pool sits at 128, raised from 64 after diag traces
     * showed AdaptiveLimit running 100+ concurrent tarball imports
     * (each holding a blocking slot for gzip + tar + CAS write)
     * while the linker is also fanning out hardlinks on the same
     * pool. 64 was saturating, queueing late tarballs behind
     * earlier finishers. 128 covers worst case fat tarball
     * pipeline plus linker plus side effects.
     *
     * AUBE_TOKIO_WORKERS / AUBE_TOKIO_BLOCKING for benchmarking.
     */
    let parse_env = |key: &str, default: usize| -> usize {
        std::env::var(key)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    };
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let workers = parse_env("AUBE_TOKIO_WORKERS", cpu_count.min(8));
    let blocking = parse_env("AUBE_TOKIO_BLOCKING", 128);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(blocking)
        .enable_all()
        .build()
        .into_diagnostic()
        .wrap_err("failed to build tokio runtime")?;
    let exit_code = runtime.block_on(async_main(cli))?;
    drop(runtime);
    if let Some(exit_code) = exit_code {
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn async_main(cli: Cli) -> miette::Result<Option<i32>> {
    // Default log level is `warn` so routine install output doesn't collide
    // with the clx progress display. `-v` / `--verbose` and `--loglevel debug`
    // turn on debug logging, and in that mode we also force clx into Text
    // output so the progress UI never renders over the log lines. `--silent`
    // (and `--loglevel silent`) turn logging off entirely and disable the
    // progress UI.
    // `--reporter=silent` is equivalent to `--silent`; all other reporter
    // values leave the log level alone and only affect output routing.
    if let Some(dir) = &cli.dir {
        std::env::set_current_dir(dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to change directory to {}", dir.display()))?;
    }

    if cli.version {
        println!("{}", crate::version::VERSION_LONG.as_str());
        let cwd =
            crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
        update_check::check_and_notify(&cwd).await;
        return Ok(None);
    }

    if cli.workspace_root {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = commands::find_workspace_root(&start)?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to change directory to {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    let settings = load_startup_settings()?;
    let effective_level = resolve_loglevel(&cli, settings.loglevel.as_deref());
    init_logging(&cli, effective_level);
    // Skip diag init for the `diag` subcommand itself — the analyzer
    // would otherwise truncate the JSONL file it's about to read.
    if !matches!(cli.command, Some(Commands::Diag(_))) {
        match diag_config_from_flag(&cli) {
            Some(cfg_opt) => aube_util::diag::init_with_config(cfg_opt),
            None => aube_util::diag::init(),
        }
    }
    raise_nofile_limit();

    // `--silent` suppresses non-error stderr output from every command,
    // including the ~230 direct `eprintln!` calls in command bodies. The
    // guard restores fd 2 on drop (before main returns), so miette still
    // prints error reports to the real stderr. We also register the
    // saved fd with aube-scripts so child processes spawned via
    // `aube_scripts::child_stderr()` (lifecycle scripts, `aube run`,
    // `aube exec`, `aube dlx`) keep writing to the real terminal — only
    // aube's own output is silenced, matching `pnpm --loglevel silent`.
    let _silent_guard = matches!(effective_level, LogLevel::Silent)
        .then(SilentStderrGuard::install)
        .flatten();
    if let Some(ref guard) = _silent_guard {
        aube_scripts::set_saved_stderr_fd(guard.saved);
    }

    commands::set_skip_auto_install_on_package_manager_mismatch(false);
    if command_needs_package_manager_guard(cli.command.as_ref()) {
        let guard = enforce_package_manager_guardrails(&settings, cli.command.as_ref())?;
        commands::set_skip_auto_install_on_package_manager_mismatch(
            guard == PackageManagerGuard::WarnRunOnly,
        );
    }

    // `--recursive` / `-r` is sugar for `--filter=*`. When a filter is
    // already set, `-r` is a no-op — the explicit scope wins.
    let effective_filter = compute_effective_filter(&cli);

    commands::set_global_output_flags(commands::GlobalOutputFlags {
        ndjson: matches!(cli.reporter, Some(ReporterType::Ndjson)),
        silent: matches!(effective_level, LogLevel::Silent),
    });

    match cli.command {
        Some(Commands::NodeGypBootstrap { project_dir }) => {
            commands::install::node_gyp_bootstrap::print_bootstrapped_binary(&project_dir).await?
        }
        Some(Commands::Add(args)) => {
            commands::add::run(args, effective_filter.clone()).await?;
        }
        Some(Commands::ApproveBuilds(args)) => commands::approve_builds::run(args).await?,
        Some(Commands::Audit(args)) => commands::audit::run(args).await?,
        Some(Commands::Bin(args)) => commands::bin::run(args).await?,
        Some(Commands::Cache(args)) => commands::cache::run(args).await?,
        Some(Commands::CatFile(args)) => commands::cat_file::run(args).await?,
        Some(Commands::CatIndex(args)) => commands::cat_index::run(args).await?,
        Some(Commands::Check(args)) => commands::check::run(args).await?,
        Some(Commands::Ci(args)) => commands::ci::run(args).await?,
        Some(Commands::Clean(args)) => commands::clean::run(args).await?,
        Some(Commands::Completion(args)) => commands::completion::run(args).await?,
        Some(Commands::Config(args)) => commands::config::run(args).await?,
        Some(Commands::Create(args)) => commands::create::run(args).await?,
        Some(Commands::Dedupe(args)) => commands::dedupe::run(args).await?,
        Some(Commands::Deploy(args)) => {
            commands::deploy::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Deprecate(args)) => commands::deprecate::run(args).await?,
        Some(Commands::Deprecations(args)) => {
            if let Some(code) = commands::deprecations::run(args).await? {
                return Ok(Some(code));
            }
        }
        Some(Commands::Diag(args)) => commands::diag::run(args).await?,
        Some(Commands::DistTag(args)) => commands::dist_tag::run(args).await?,
        Some(Commands::Dlx(args)) => commands::dlx::run(args).await?,
        Some(Commands::Doctor(args)) => commands::doctor::run(args).await?,
        Some(Commands::Exec(args)) => commands::exec::run(args, effective_filter.clone()).await?,
        Some(Commands::Fetch(args)) => commands::fetch::run(args).await?,
        Some(Commands::FindHash(args)) => commands::find_hash::run(args).await?,
        Some(Commands::Get(args)) => commands::config::get(args)?,
        Some(Commands::IgnoredBuilds(args)) => commands::ignored_builds::run(args).await?,
        Some(Commands::Import(args)) => commands::import::run(args).await?,
        Some(Commands::Init(args)) => commands::init::run(args).await?,
        Some(Commands::Install(args)) => {
            run_install_command(args, effective_filter.clone(), cli.workspace_root).await?;
        }
        Some(Commands::InstallTest(args)) => commands::install_test::run(args).await?,
        Some(Commands::La(mut args)) | Some(Commands::Ll(mut args)) => {
            args.long = true;
            commands::list::run(args, effective_filter.clone()).await?;
        }
        Some(Commands::Licenses(args)) => commands::licenses::run(args).await?,
        Some(Commands::Link(args)) => commands::link::run(args).await?,
        Some(Commands::List(args)) => commands::list::run(args, effective_filter.clone()).await?,
        Some(Commands::Login(args)) => commands::login::run(args).await?,
        Some(Commands::Logout(args)) => commands::logout::run(args).await?,
        Some(Commands::Outdated(args)) => {
            commands::outdated::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Owner(args)) => {
            return Ok(Some(commands::npm_fallback::run("owner", &args)?));
        }
        Some(Commands::Pack(args)) => commands::pack::run(args).await?,
        Some(Commands::Patch(args)) => commands::patch::run(args).await?,
        Some(Commands::PatchCommit(args)) => commands::patch_commit::run(args).await?,
        Some(Commands::PatchRemove(args)) => commands::patch_remove::run(args).await?,
        Some(Commands::Peers(args)) => commands::peers::run(args).await?,
        Some(Commands::Pkg(args)) => {
            return Ok(Some(commands::npm_fallback::run("pkg", &args)?));
        }
        Some(Commands::Prune(args)) => commands::prune::run(args).await?,
        Some(Commands::Publish(args)) => {
            commands::publish::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Purge(args)) => commands::clean::run_purge(args).await?,
        Some(Commands::Query(args)) => commands::query::run(args, effective_filter.clone()).await?,
        Some(Commands::Rebuild(args)) => {
            commands::rebuild::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Remove(args)) => {
            commands::remove::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Recursive(args)) => {
            let argv = commands::recursive::argv(
                args,
                commands::recursive::RecursiveGlobals {
                    filters: effective_filter.clone(),
                    color: cli.color,
                    no_color: cli.no_color,
                },
            )?;
            // The reconstructed argv may carry pre-subcommand-positioned
            // flags that moved off `global = true` (e.g. `--registry`,
            // `--frozen-lockfile`). Run the same lift-pass we use on the
            // outer argv so the nested clap parse sees them after the
            // subcommand.
            let nested_argv: Vec<OsString> =
                lift_per_subcommand_flags(argv.into_iter().map(OsString::from).collect());
            let nested = Cli::try_parse_from(nested_argv).into_diagnostic()?;
            let nested_filter = compute_effective_filter(&nested);
            match nested.command {
                Some(Commands::Add(args)) => {
                    commands::add::run(args, nested_filter).await?;
                }
                Some(Commands::Deploy(args)) => commands::deploy::run(args, nested_filter).await?,
                Some(Commands::Exec(args)) => commands::exec::run(args, nested_filter).await?,
                Some(Commands::Install(args)) => {
                    run_install_command(args, nested_filter, nested.workspace_root).await?;
                }
                Some(Commands::List(args)) => commands::list::run(args, nested_filter).await?,
                Some(Commands::La(mut args)) | Some(Commands::Ll(mut args)) => {
                    args.long = true;
                    commands::list::run(args, nested_filter).await?;
                }
                Some(Commands::Outdated(args)) => {
                    commands::outdated::run(args, nested_filter).await?
                }
                Some(Commands::Publish(args)) => {
                    commands::publish::run(args, nested_filter).await?
                }
                Some(Commands::Rebuild(args)) => {
                    commands::rebuild::run(args, nested_filter).await?
                }
                Some(Commands::Remove(args)) => commands::remove::run(args, nested_filter).await?,
                Some(Commands::Restart(args)) => {
                    commands::restart::run(args, nested_filter).await?
                }
                Some(Commands::Run(args)) => commands::run::run(args, nested_filter).await?,
                Some(Commands::Start(args)) => {
                    run_script_lifecycle("start", args, &nested_filter).await?;
                }
                Some(Commands::Stop(args)) => {
                    run_script_lifecycle("stop", args, &nested_filter).await?;
                }
                Some(Commands::Test(args)) => {
                    run_script_lifecycle("test", args, &nested_filter).await?;
                }
                Some(Commands::Update(args)) => {
                    commands::update::run(args, nested_filter).await?;
                }
                Some(Commands::Why(args)) => commands::why::run(args, nested_filter).await?,
                Some(Commands::External(args)) => {
                    let script = &args[0];
                    let script_args: Vec<String> = args[1..].to_vec();
                    commands::run::run_script(script, &script_args, false, false, &nested_filter)
                        .await?;
                }
                Some(_) | None => {
                    return Err(miette::miette!(
                        code = aube_codes::errors::ERR_AUBE_RECURSIVE_NOT_SUPPORTED,
                        "aube recursive: command does not support recursive execution"
                    ));
                }
            }
        }
        Some(Commands::Restart(args)) => {
            commands::restart::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Root(args)) => commands::root::run(args).await?,
        Some(Commands::Run(args)) => commands::run::run(args, effective_filter.clone()).await?,
        Some(Commands::Sbom(args)) => commands::sbom::run(args).await?,
        Some(Commands::Search(args)) => {
            return Ok(Some(commands::npm_fallback::run("search", &args)?));
        }
        Some(Commands::Set(args)) => commands::config::set(args)?,
        Some(Commands::SetScript(args)) => {
            return Ok(Some(commands::npm_fallback::run("set-script", &args)?));
        }
        Some(Commands::Start(args)) => {
            run_script_lifecycle("start", args, &effective_filter).await?;
        }
        Some(Commands::Stop(args)) => {
            run_script_lifecycle("stop", args, &effective_filter).await?;
        }
        Some(Commands::Store(args)) => commands::store::run(args).await?,
        Some(Commands::Test(args)) => {
            run_script_lifecycle("test", args, &effective_filter).await?;
        }
        Some(Commands::Token(args)) => {
            return Ok(Some(commands::npm_fallback::run("token", &args)?));
        }
        Some(Commands::Undeprecate(args)) => commands::undeprecate::run(args).await?,
        Some(Commands::Unlink(args)) => commands::unlink::run(args).await?,
        Some(Commands::Unpublish(args)) => commands::unpublish::run(args).await?,
        Some(Commands::Update(args)) => {
            commands::update::run(args, effective_filter.clone()).await?;
        }
        Some(Commands::Version(args)) => commands::version::run(args).await?,
        Some(Commands::View(args)) => commands::view::run(args).await?,
        Some(Commands::Whoami(args)) => {
            return Ok(Some(commands::npm_fallback::run("whoami", &args)?));
        }
        Some(Commands::Why(args)) => commands::why::run(args, effective_filter.clone()).await?,
        Some(Commands::Usage) => {
            use clap::CommandFactory;
            // Reset the `-DEBUG`-suffixed runtime version back to the plain
            // package version so the emitted KDL (consumed by `mise render`
            // and the CLI docs build) stays byte-stable across profiles.
            let mut cmd = Cli::command().version(env!("CARGO_PKG_VERSION"));
            clap_usage::generate(&mut cmd, "aube", &mut std::io::stdout());
        }
        Some(Commands::External(args)) => {
            // Implicit run: `aube dev` = `aube run dev`.
            //
            // External is clap's catch-all, so a typo like `aube fooefjwol`
            // lands here too. If the name isn't an actual script in the
            // local `package.json` (or there's no `package.json` at all),
            // print `aube --help` and bail instead of routing it into the
            // script runner and surfacing a confusing "script not found"
            // or "failed to read package.json" — the user typed something
            // we don't recognize and help is the most useful reply.
            //
            // The pre-check only fires when *no* workspace filter is
            // active: `-r` / `-F` fan implicit scripts out across
            // sub-packages, and the script may live in one of the
            // matched workspaces while the root `package.json` has no
            // `scripts` entry at all. In that mode we hand off to
            // `run_script` unchanged and let the filtered runner
            // produce its own per-package diagnostics.
            let script = &args[0];
            let script_args: Vec<String> = args[1..].to_vec();
            if effective_filter.is_empty() {
                let initial_cwd = crate::dirs::cwd()?;
                let script_exists = crate::dirs::find_project_root(&initial_cwd)
                    .and_then(|cwd| {
                        aube_manifest::PackageJson::from_path(&cwd.join("package.json")).ok()
                    })
                    .map(|m| m.scripts.contains_key(script))
                    .unwrap_or(false);
                if !script_exists {
                    use clap::CommandFactory;
                    let mut cmd = Cli::command();
                    cmd.print_help().ok();
                    eprintln!();
                    return Err(miette::miette!(
                        code = aube_codes::errors::ERR_AUBE_UNKNOWN_COMMAND,
                        "unknown command: {script}"
                    ));
                }
            }
            commands::run::run_script(script, &script_args, false, false, &effective_filter)
                .await?;
        }
        None => {
            // Bare `aube` prints `--help` and exits 0, matching pnpm.
            // pnpm's bare invocation does not run an install; users who
            // want that behavior should type `aube install` explicitly.
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            cmd.print_help().ok();
            println!();
        }
    }

    Ok(None)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug)]
struct StartupSettings {
    loglevel: Option<String>,
    package_manager_strict: PackageManagerStrictMode,
    package_manager_strict_version: bool,
}

/// Tri-state for the `packageManagerStrict` setting.
///
/// `Off` skips the check entirely. `Warn` (the default) prints a
/// warning for unsupported `packageManager` names but lets every
/// command continue; install-class commands also disable the implicit
/// auto-install probe so aube does not write into another package
/// manager's `node_modules` layout. `Error` fails install-class
/// commands hard while still degrading to a warning for run-class
/// commands (matching the prior `true` behavior).
///
/// Accepts the bool spellings (`true` → `Error`, `false` → `Off`) for
/// back-compat with older `.npmrc` files that pre-date the tri-state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
enum PackageManagerStrictMode {
    Off,
    #[default]
    Warn,
    Error,
}

impl PackageManagerStrictMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Some(Self::Off),
            "warn" => Some(Self::Warn),
            "error" | "true" | "1" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Resolve `packageManagerStrict`, surfacing a warning when the user
/// configured an unrecognized value (e.g. `errror` typo) instead of
/// silently falling back to the default. Tracing isn't initialized
/// yet at startup, so the warning goes straight to stderr.
fn resolve_package_manager_strict(ctx: &aube_settings::ResolveCtx<'_>) -> PackageManagerStrictMode {
    let raw = aube_settings::resolved::package_manager_strict(ctx);
    if let Some(mode) = PackageManagerStrictMode::parse(&raw) {
        return mode;
    }
    eprintln!(
        "warning: packageManagerStrict={raw:?} is not a recognized value (expected `off`, `warn`, `error`, or back-compat bool `true`/`false`); falling back to `warn`."
    );
    PackageManagerStrictMode::default()
}

fn resolve_color_mode(cli: &Cli) -> ColorMode {
    if cli.no_color {
        return ColorMode::Never;
    }
    if cli.color {
        return ColorMode::Always;
    }
    let env = aube_settings::values::capture_env();
    if let Some(mode) =
        aube_settings::values::string_from_env("color", &env).and_then(|raw| parse_color_mode(&raw))
    {
        return mode;
    }
    let Ok(cwd) = startup_cwd(cli) else {
        return ColorMode::Auto;
    };
    let npmrc = aube_registry::config::load_npmrc_entries(&cwd);
    aube_settings::values::string_from_npmrc("color", &npmrc)
        .and_then(|raw| parse_color_mode(&raw))
        .unwrap_or(ColorMode::Auto)
}

/// Conservative allowlist of CI vendors whose log viewers are known to
/// render ANSI escape sequences. We force color on stderr for these so
/// the install progress UI keeps its styling under CI; everything not
/// on the list (Heroku build, Netlify, AWS CodeBuild, generic `CI=true`
/// from a script that captures stderr to a log file, …) keeps the
/// default no-color behavior to avoid baking escapes into log artifacts.
///
/// Deliberately narrower than `is_ci::cached()` (used elsewhere to pick
/// the CI heartbeat over the animated TTY bar): "are we in CI?" is a
/// broader question than "does this CI render ANSI?", and forcing
/// color on a runner that captures stderr to a plain log file is worse
/// than leaving it off. New entries here should be vendors whose web
/// log viewer is documented to render ANSI; expand as confirmed.
fn ci_renders_ansi() -> bool {
    use ci_info::types::Vendor;
    matches!(
        ci_info::get().vendor,
        Some(
            Vendor::GitHubActions
                | Vendor::GitLabCI
                | Vendor::Buildkite
                | Vendor::CircleCI
                | Vendor::TravisCI
                | Vendor::Drone
                | Vendor::AppVeyor
                | Vendor::AzurePipelines
                | Vendor::BitbucketPipelines
                | Vendor::TeamCity
                | Vendor::WoodpeckerCI
        )
    )
}

/// True when the user has asked for no color via the cross-tool
/// conventions (https://no-color.org/, the CLICOLOR convention). Lets
/// the CI auto-color branch back off without disturbing the explicit
/// `--color` / `color=always` paths, which already win earlier in
/// `resolve_color_mode`.
fn env_disables_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || std::env::var_os("CLICOLOR").is_some_and(|v| v == "0")
}

fn startup_cwd(cli: &Cli) -> miette::Result<PathBuf> {
    let cwd = match &cli.dir {
        Some(dir) if dir.is_absolute() => Ok(dir.clone()),
        Some(dir) => std::env::current_dir()
            .into_diagnostic()
            .map(|cwd| cwd.join(dir)),
        None => std::env::current_dir().into_diagnostic(),
    }?;
    if cli.workspace_root {
        commands::find_workspace_root(&cwd)
    } else {
        Ok(cwd)
    }
}

fn load_startup_settings() -> miette::Result<StartupSettings> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    let files = commands::FileSources::load(&cwd);
    let empty_ws = std::collections::BTreeMap::new();
    let env = aube_settings::values::capture_env();
    let ctx = files.ctx(&empty_ws, &env, &[]);
    Ok(StartupSettings {
        loglevel: aube_settings::values::string_from_env("loglevel", &env)
            .or_else(|| {
                aube_settings::values::string_from_npmrc("loglevel", &files.project_aube_config)
            })
            .or_else(|| aube_settings::values::string_from_npmrc("loglevel", &files.project_npmrc))
            .or_else(|| {
                aube_settings::values::string_from_npmrc("loglevel", &files.user_aube_config)
            })
            .or_else(|| aube_settings::values::string_from_npmrc("loglevel", &files.user_npmrc)),
        package_manager_strict: resolve_package_manager_strict(&ctx),
        package_manager_strict_version: aube_settings::resolved::package_manager_strict_version(
            &ctx,
        ),
    })
}

fn resolve_loglevel(cli: &Cli, configured: Option<&str>) -> LogLevel {
    let reporter_silent = matches!(cli.reporter, Some(ReporterType::Silent));
    if cli.silent || reporter_silent {
        return LogLevel::Silent;
    }
    if let Some(level) = cli.loglevel {
        return level;
    }
    if env_is_truthy("AUBE_TRACE") {
        return LogLevel::Trace;
    }
    if cli.verbose || env_is_truthy("AUBE_DEBUG") {
        return LogLevel::Debug;
    }
    configured
        .and_then(parse_loglevel)
        .unwrap_or(LogLevel::Warn)
}

fn env_is_truthy(name: &str) -> bool {
    let Ok(raw) = std::env::var(name) else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y"
    )
}

fn parse_loglevel(raw: &str) -> Option<LogLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(LogLevel::Trace),
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        "silent" => Some(LogLevel::Silent),
        _ => None,
    }
}

fn parse_color_mode(raw: &str) -> Option<ColorMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always" | "true" | "1" => Some(ColorMode::Always),
        "never" | "false" | "0" => Some(ColorMode::Never),
        "auto" => Some(ColorMode::Auto),
        _ => None,
    }
}

/// Raise the `RLIMIT_NOFILE` soft limit toward the hard limit on Unix.
/// macOS defaults this to ~256 on some setups, which installs blow past
/// during concurrent fetch + tarball-extract + materialize (one FD per
/// CAS file open, multiplied across the tokio blocking pool and rayon
/// linker threads). Silent no-op on Windows.
///
/// First try `soft = hard`. If that fails (macOS reports `rlim_max` as
/// `RLIM_INFINITY` but the kernel still caps at `kern.maxfilesperproc`,
/// usually 24576), retry with `OPEN_MAX = 10240` which is accepted on
/// every stock macOS.
#[cfg(unix)]
fn raise_nofile_limit() {
    // SAFETY: get/setrlimit are sync syscalls that read/write our own
    // process's resource table. No aliasing. Failure is reported as a
    // non-zero return and handled by the caller.
    unsafe {
        let mut rlim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            tracing::trace!("getrlimit(RLIMIT_NOFILE) failed; keeping default FD limit");
            return;
        }
        let before = rlim.rlim_cur;
        if before >= rlim.rlim_max {
            tracing::trace!("RLIMIT_NOFILE soft={before} already at hard limit");
            return;
        }
        let hard = rlim.rlim_max;
        rlim.rlim_cur = hard;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            tracing::trace!("raised RLIMIT_NOFILE soft {before} -> {hard}");
            return;
        }
        rlim.rlim_cur = before.max(10240).min(hard);
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            tracing::trace!(
                "raised RLIMIT_NOFILE soft {before} -> {} (hard={hard}, fallback cap)",
                rlim.rlim_cur
            );
        } else {
            tracing::trace!("setrlimit(RLIMIT_NOFILE) failed; keeping soft={before}");
        }
    }
}

#[cfg(not(unix))]
fn raise_nofile_limit() {}

/// Build a [`aube_util::diag::DiagConfig`] from the `--diag` flag set, or
/// `None` to defer to env-var driven init. Returns `Some(None)` when the user
/// passed an invalid mode (caller still inits, just without diag).
fn diag_config_from_flag(cli: &Cli) -> Option<Option<aube_util::diag::DiagConfig>> {
    let mode = cli.diag.as_deref()?;
    let mode = mode.trim().to_ascii_lowercase();
    let valid = ["summary", "trace", "live", "full"];
    if !valid.contains(&mode.as_str()) {
        eprintln!(
            "[diag] unknown --diag mode {:?}. Valid: summary | trace | live | full",
            mode
        );
        return Some(None);
    }
    let track_events = mode != "summary";
    let print_stderr = mode == "live";
    let threshold_ms = if print_stderr {
        cli.diag_threshold_ms.unwrap_or(100)
    } else {
        0
    };
    let file = if mode == "full" {
        Some(
            cli.diag_file
                .clone()
                .unwrap_or_else(|| PathBuf::from("aube-diag.jsonl")),
        )
    } else {
        cli.diag_file.clone()
    };
    eprintln!(
        "[diag] mode={} (summary{}{}{})",
        mode,
        if track_events { " + critpath" } else { "" },
        if print_stderr { " + live" } else { "" },
        if file.is_some() { " + jsonl" } else { "" }
    );
    Some(Some(aube_util::diag::DiagConfig {
        file,
        print_stderr,
        summary: true,
        track_events,
        threshold_ms,
    }))
}

fn init_logging(cli: &Cli, effective_level: LogLevel) {
    let log_level = effective_level.filter();
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("AUBE_LOG").unwrap_or_else(|_| {
        format!(
            "aube={log_level},aube_cli={log_level},aube_registry={log_level},\
             aube_resolver={log_level},aube_lockfile={log_level},aube_store={log_level},\
             aube_linker={log_level},aube_manifest={log_level},aube_scripts={log_level},\
             aube_workspace={log_level},aube_settings={log_level},aube_util={log_level}"
        )
        .into()
    });

    // ndjson swaps the fmt layer for the JSON formatter so every tracing
    // event is serialized as a single line of JSON on stderr. The filter
    // itself is the same as every other mode — `--loglevel` / `--verbose`
    // / `AUBE_DEBUG` / `AUBE_LOG` pick the verbosity, ndjson just changes
    // the encoding.
    //
    // Every mode routes writes through `progress::PausingWriter`, which
    // pauses the clx progress display and holds the terminal lock across
    // each event so a `warn!` emitted mid-render doesn't get smeared
    // through the animated bar. In text mode (append-only / ndjson /
    // silent / verbose) the progress display never starts, so the writer
    // degrades to a plain stderr flush.
    //
    // Timestamps are dropped unless the user asked for `debug` verbosity
    // (via `-v` / `--loglevel=debug` / `AUBE_DEBUG=1` / `AUBE_LOG`). A
    // default-verbosity install that emits deprecated-package warnings
    // reads more like pnpm's `WARN: mathjax-full@3.2.2 is deprecated…`
    // than a server log line; keeping the RFC3339 prefix just pushes the
    // package name off the visible width for no gain. ndjson always
    // keeps the timestamp — its whole point is machine-parseable records.
    let drop_timestamp = !matches!(effective_level, LogLevel::Debug | LogLevel::Trace);
    let registry = tracing_subscriber::registry().with(env_filter);
    if matches!(cli.reporter, Some(ReporterType::Ndjson)) {
        crate::pnpmfile::set_ndjson_reporter(true);
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_writer(crate::progress::PausingWriter),
            )
            .init();
    } else if drop_timestamp {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_writer(crate::progress::PausingWriter),
            )
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(crate::progress::PausingWriter))
            .init();
    }

    // Force clx into plain text mode whenever the progress UI would collide
    // with the reporter: `append-only` and `ndjson` both want line-at-a-time
    // output, and `debug`/`silent` already disabled the UI for their own
    // reasons.
    let force_text = matches!(
        effective_level,
        LogLevel::Trace | LogLevel::Debug | LogLevel::Silent
    ) || matches!(
        cli.reporter,
        Some(ReporterType::AppendOnly) | Some(ReporterType::Ndjson)
    );
    if force_text {
        clx::progress::set_output(clx::progress::ProgressOutput::Text);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PackageManagerGuard {
    Ok,
    WarnRunOnly,
}

fn enforce_package_manager_guardrails(
    settings: &StartupSettings,
    command: Option<&Commands>,
) -> miette::Result<PackageManagerGuard> {
    if settings.package_manager_strict == PackageManagerStrictMode::Off {
        return Ok(PackageManagerGuard::Ok);
    }

    let cwd = std::env::current_dir().into_diagnostic()?;
    let Some(root) = crate::dirs::find_workspace_root(&cwd)
        .filter(|root| root.join("package.json").is_file())
        .or_else(|| crate::dirs::find_project_root(&cwd))
    else {
        return Ok(PackageManagerGuard::Ok);
    };
    let path = root.join("package.json");
    let raw = std::fs::read_to_string(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
    let Some(package_manager) = json.get("packageManager").and_then(|v| v.as_str()) else {
        return Ok(PackageManagerGuard::Ok);
    };
    let Some((name, version)) = parse_package_manager(package_manager) else {
        return Err(miette!(
            "invalid packageManager field `{package_manager}` in {}",
            path.display()
        ));
    };

    // Accept either the clean CARGO_PKG_VERSION or the `-DEBUG`-suffixed
    // runtime string: users may copy `aube --version` (which appends `-DEBUG`
    // on non-release builds) into `packageManager`, and `aube init` writes
    // the clean version. Both should pass the strict check on the same
    // binary.
    let normalized = version.strip_suffix("-DEBUG").unwrap_or(version);
    match name {
        "aube" => {
            if settings.package_manager_strict_version && normalized != env!("CARGO_PKG_VERSION") {
                return Err(miette!(
                    "packageManager requires aube@{version}, but this is aube@{}",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            Ok(PackageManagerGuard::Ok)
        }
        "pnpm" => {
            if settings.package_manager_strict_version {
                return Err(miette!(
                    "packageManager requires exact pnpm@{version}, but aube cannot download or re-exec a specific pnpm version. Use pnpm directly, set packageManagerStrictVersion=false, or pin packageManager to aube@{}.",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            Ok(PackageManagerGuard::Ok)
        }
        other => {
            // `error` mode: install-class commands fail hard, run-class
            // commands warn-and-skip-auto-install (matches the prior
            // `true` behavior). `warn` mode: every command warns; for
            // install-class we still suppress auto-install so aube
            // does not write into another PM's node_modules layout.
            let mode = match settings.package_manager_strict {
                PackageManagerStrictMode::Error => package_manager_guard_mode(command),
                _ => PackageManagerGuardMode::WarnAndSkipAutoInstall,
            };
            match mode {
                PackageManagerGuardMode::Error => Err(miette!(
                    "packageManager in {} uses unsupported package manager `{other}`. aube's packageManagerStrict=error guard only accepts `aube` and `pnpm`; remove or change the `packageManager` field, or set `package-manager-strict=warn` (the default) or `=off` in .npmrc to soften this guard.",
                    path.display()
                )),
                PackageManagerGuardMode::WarnAndSkipAutoInstall => {
                    eprintln!(
                        "warning: packageManager in {} uses unsupported package manager `{other}`; continuing but auto-install is disabled. Switch packageManager to `aube`/`pnpm`, set packageManagerStrict=off, or pass `--no-install` to skip the install probe explicitly.",
                        path.display()
                    );
                    Ok(PackageManagerGuard::WarnRunOnly)
                }
            }
        }
    }
}

fn parse_package_manager(raw: &str) -> Option<(&str, &str)> {
    let (name, rest) = raw.rsplit_once('@')?;
    if name.is_empty() || rest.is_empty() {
        return None;
    }
    let version = rest.split_once('+').map_or(rest, |(version, _)| version);
    if version.is_empty() {
        return None;
    }
    Some((name, version))
}

fn command_needs_package_manager_guard(command: Option<&Commands>) -> bool {
    !matches!(
        command,
        None | Some(Commands::Config(_))
            | Some(Commands::Get(_))
            | Some(Commands::Set(_))
            | Some(Commands::Completion(_))
            | Some(Commands::Usage)
    )
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PackageManagerGuardMode {
    Error,
    WarnAndSkipAutoInstall,
}

fn package_manager_guard_mode(command: Option<&Commands>) -> PackageManagerGuardMode {
    if matches!(
        command,
        Some(Commands::Run(_))
            | Some(Commands::Test(_))
            | Some(Commands::Start(_))
            | Some(Commands::Stop(_))
            | Some(Commands::Restart(_))
            | Some(Commands::External(_))
    ) {
        PackageManagerGuardMode::WarnAndSkipAutoInstall
    } else {
        PackageManagerGuardMode::Error
    }
}

fn compute_effective_filter(cli: &Cli) -> aube_workspace::selector::EffectiveFilter {
    // `--recursive` / `-r` is sugar for `--filter=*`, so the wildcard
    // only joins the regular filter list — never `--filter-prod`. When
    // the user supplies either flag explicitly, `-r` is a no-op.
    let mut filters = cli.filter.clone();
    if cli.recursive && filters.is_empty() && cli.filter_prod.is_empty() {
        filters.push("*".to_string());
    }
    aube_workspace::selector::EffectiveFilter {
        filters,
        filter_prods: cli.filter_prod.clone(),
        fail_if_no_match: cli.fail_if_no_match,
        include_workspace_root: cli.include_workspace_root || cli.workspace_root,
    }
}

/// Run a lifecycle script (`start` / `stop` / `test` / `restart`).
///
/// `ScriptArgs` carries the moved-off-global `LockfileArgs` /
/// `NetworkArgs` / `VirtualStoreArgs` flattens for these commands, so we
/// drain them into the process-global slots before delegating to the
/// shared `run_script` helper. Auto-install (triggered by `run_script`
/// when the named script doesn't exist locally) reads the slots through
/// `ensure_installed`.
async fn run_script_lifecycle(
    name: &str,
    args: commands::run::ScriptArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    commands::run::run_script(name, &args.args, args.no_install, false, filter).await
}

async fn run_install_command(
    args: commands::install::InstallArgs,
    filter: aube_workspace::selector::EffectiveFilter,
    workspace_root_already: bool,
) -> miette::Result<()> {
    // `-w` on install is a short alias for the global
    // `--workspace-root` flag. Handle the chdir here when the global
    // flag wasn't already set.
    if args.workspace_root_short && !workspace_root_already {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = commands::find_workspace_root(&start)?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to change directory to {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let global_frozen = args.lockfile.frozen_override();
    let global_gvs = args.virtual_store.flags();
    // Match `install::run`'s precedence so settings here resolve from
    // the same root the install will operate against. Workspace-first
    // means `aube install` from inside a member loads `.npmrc` /
    // workspace yaml from the workspace root, not the member; without
    // this the two diverged when both roots existed.
    let cwd = crate::dirs::workspace_or_project_root()?;
    let files = commands::FileSources::load(&cwd);
    let raw_ws = aube_manifest::workspace::load_raw(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env = aube_settings::values::capture_env();
    let cli_flags = args.to_cli_flag_bag(global_frozen, global_gvs);
    let ctx = files.ctx(&raw_ws, &env, &cli_flags);
    let yaml_prefer_frozen = aube_settings::resolved::prefer_frozen_lockfile(&ctx);
    let mut opts = args.into_options(global_frozen, yaml_prefer_frozen, cli_flags, env);
    opts.workspace_filter = filter;
    commands::install::run(opts).await?;
    Ok(())
}

#[cfg(test)]
mod cli_spec_tests {
    use super::*;

    #[test]
    fn install_accepts_subcommand_registry_flag() {
        let cli = Cli::try_parse_from([
            "aube",
            "install",
            "--registry",
            "https://registry.example.com/",
        ])
        .expect("install --registry should parse");

        let Some(Commands::Install(install_args)) = cli.command else {
            panic!("expected install subcommand");
        };
        assert_eq!(
            install_args.network.registry.as_deref(),
            Some("https://registry.example.com/")
        );
    }

    #[test]
    fn pre_subcommand_registry_lifts_to_install() {
        // pnpm-compat: `--registry=URL install` continues to parse via
        // `lift_per_subcommand_flags`, which shifts the flag past the
        // subcommand before clap sees argv.
        let argv = lift_per_subcommand_flags(
            [
                "aube",
                "--registry",
                "https://registry.example.com/",
                "install",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        );
        let cli = Cli::try_parse_from(argv)
            .expect("pre-subcommand --registry should still parse via the rewriter");
        let Some(Commands::Install(install_args)) = cli.command else {
            panic!("expected install subcommand");
        };
        assert_eq!(
            install_args.network.registry.as_deref(),
            Some("https://registry.example.com/")
        );
    }

    #[test]
    fn lifter_does_not_eat_lifted_flag_as_kept_flag_value() {
        // Regression: `aube --dir /tmp --frozen-lockfile install` would
        // previously lose `--frozen-lockfile` if `--dir`'s value was
        // omitted because the rewriter unconditionally consumed the next
        // token as the kept flag's value.
        let argv = lift_per_subcommand_flags(
            ["aube", "--dir", "--frozen-lockfile", "install"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        );
        // After the lift, `--frozen-lockfile` should sit after `install`,
        // NOT have been consumed as `--dir`'s value.
        let strs: Vec<&str> = argv.iter().filter_map(|t| t.to_str()).collect();
        let install_idx = strs
            .iter()
            .position(|s| *s == "install")
            .expect("install subcommand should survive the lift");
        assert!(
            strs[install_idx + 1..].contains(&"--frozen-lockfile"),
            "--frozen-lockfile should land after the subcommand: {strs:?}"
        );
    }

    #[test]
    fn short_command_aliases_parse() {
        let cli = Cli::try_parse_from(["aube", "a", "react"]).expect("a should parse as add");
        assert!(matches!(cli.command, Some(Commands::Add(_))));

        let cli =
            Cli::try_parse_from(["aube", "x", "vitest", "--run"]).expect("x should parse as exec");
        let Some(Commands::Exec(args)) = cli.command else {
            panic!("x should dispatch to exec");
        };
        assert_eq!(args.bin, "vitest");
        assert_eq!(args.args, vec!["--run"]);

        let cli = Cli::try_parse_from(["aube", "w", "react"]).expect("w should parse as why");
        assert!(matches!(cli.command, Some(Commands::Why(_))));
    }
}

#[cfg(test)]
mod multicall_tests {
    use super::*;

    fn os(strs: &[&str]) -> Vec<OsString> {
        strs.iter().map(OsString::from).collect()
    }

    fn temp_shim(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        std::fs::write(dir.path().join(name), "#!/tmp/aube.exe\n").expect("shim should be written");
        dir
    }

    #[test]
    fn aube_passes_through_unchanged() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aube", "install"])),
            os(&["aube", "install"])
        );
    }

    #[test]
    fn aubr_rewrites_to_run() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aubr", "build"])),
            os(&["aube", "run", "build"])
        );
    }

    #[test]
    fn aubx_rewrites_to_dlx() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx", "cowsay", "hi"])),
            os(&["aube", "dlx", "cowsay", "hi"])
        );
    }

    #[test]
    fn absolute_path_and_exe_suffix_are_handled() {
        // argv[0] can be an absolute path (exec-style invocation) or carry
        // a `.exe` suffix on Windows. `Path::file_stem` takes care of both
        // so dispatch stays purely basename-driven.
        assert_eq!(
            rewrite_multicall_argv(os(&["/usr/local/bin/aubr", "test"])),
            os(&["aube", "run", "test"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx.exe", "pkg"])),
            os(&["aube", "dlx", "pkg"])
        );
    }

    #[test]
    fn bare_shim_invocation_passes_through_to_subcommand() {
        // `aubr` with no further args becomes `aube run`, which clap
        // parses as the `run` subcommand with no positional — same as
        // the user typing `aube run` directly.
        assert_eq!(rewrite_multicall_argv(os(&["aubr"])), os(&["aube", "run"]));
    }

    #[test]
    fn version_flag_short_circuits_to_top_level() {
        // `aubr --version` / `aubx --version` should print the aube
        // version, not trip the `run` / `dlx` parsers.
        assert_eq!(
            rewrite_multicall_argv(os(&["aubr", "--version"])),
            os(&["aube", "--version"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx", "--version"])),
            os(&["aube", "--version"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubr", "-V"])),
            os(&["aube", "-V"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx.exe", "-V"])),
            os(&["aube", "-V"])
        );
    }

    #[test]
    fn npm_interpreter_shim_path_is_dropped() {
        let dir = temp_shim("aube");
        let shim = dir.path().join("aube");
        let shim_os = shim.clone().into_os_string();
        assert_eq!(
            rewrite_multicall_argv(vec![
                OsString::from("aube.exe"),
                shim.into_os_string(),
                OsString::from("--version"),
            ]),
            vec![shim_os, OsString::from("--version")]
        );
    }

    #[test]
    fn npm_interpreter_shim_preserves_multicall_dispatch() {
        let dir = temp_shim("aubr");
        let shim = dir.path().join("aubr");
        assert_eq!(
            rewrite_multicall_argv(vec![
                OsString::from("aubr.exe"),
                shim.into_os_string(),
                OsString::from("build"),
            ]),
            os(&["aube", "run", "build"])
        );
    }

    #[test]
    fn extract_config_overrides_strips_equals_form() {
        let mut argv = os(&["aube", "install", "--config.strict-dep-builds=true"]);
        let parsed = extract_config_overrides(&mut argv);
        assert_eq!(argv, os(&["aube", "install"]));
        assert_eq!(
            parsed,
            vec![("strict-dep-builds".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn extract_config_overrides_strips_bool_form() {
        let mut argv = os(&["aube", "--config.strictDepBuilds", "install"]);
        let parsed = extract_config_overrides(&mut argv);
        assert_eq!(argv, os(&["aube", "install"]));
        assert_eq!(
            parsed,
            vec![("strictDepBuilds".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn extract_config_overrides_handles_multiple_and_preserves_order() {
        let mut argv = os(&[
            "aube",
            "--config.foo=1",
            "install",
            "--config.bar=two",
            "--config.foo=3",
        ]);
        let parsed = extract_config_overrides(&mut argv);
        assert_eq!(argv, os(&["aube", "install"]));
        assert_eq!(
            parsed,
            vec![
                ("foo".to_string(), "1".to_string()),
                ("bar".to_string(), "two".to_string()),
                ("foo".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn extract_config_overrides_stops_at_double_dash() {
        let mut argv = os(&["aube", "exec", "--", "node", "--config.foo=should-stay"]);
        let parsed = extract_config_overrides(&mut argv);
        assert!(parsed.is_empty());
        assert_eq!(
            argv,
            os(&["aube", "exec", "--", "node", "--config.foo=should-stay"])
        );
    }

    #[test]
    fn extract_config_overrides_preserves_argv_when_absent() {
        let mut argv = os(&["aube", "install", "--frozen-lockfile"]);
        let parsed = extract_config_overrides(&mut argv);
        assert!(parsed.is_empty());
        assert_eq!(argv, os(&["aube", "install", "--frozen-lockfile"]));
    }
}

#[cfg(test)]
mod package_manager_guard_tests {
    use super::*;

    #[test]
    fn run_like_commands_warn_instead_of_erroring() {
        let run = Cli::try_parse_from(["aube", "run", "test"]).expect("run should parse");
        let test = Cli::try_parse_from(["aube", "test"]).expect("test should parse");

        assert_eq!(
            package_manager_guard_mode(run.command.as_ref()),
            PackageManagerGuardMode::WarnAndSkipAutoInstall
        );
        assert_eq!(
            package_manager_guard_mode(test.command.as_ref()),
            PackageManagerGuardMode::WarnAndSkipAutoInstall
        );
    }

    #[test]
    fn install_still_errors_on_mismatch() {
        let cli = Cli::try_parse_from(["aube", "install"]).expect("install should parse");
        assert_eq!(
            package_manager_guard_mode(cli.command.as_ref()),
            PackageManagerGuardMode::Error
        );
    }

    #[test]
    fn install_test_still_errors_on_mismatch() {
        let cli = Cli::try_parse_from(["aube", "install-test"]).expect("install-test should parse");
        assert_eq!(
            package_manager_guard_mode(cli.command.as_ref()),
            PackageManagerGuardMode::Error
        );
    }

    #[test]
    fn package_manager_strict_mode_parses_canonical_spellings() {
        for (input, expected) in [
            ("off", PackageManagerStrictMode::Off),
            ("warn", PackageManagerStrictMode::Warn),
            ("error", PackageManagerStrictMode::Error),
            ("  ERROR\n", PackageManagerStrictMode::Error),
        ] {
            assert_eq!(PackageManagerStrictMode::parse(input), Some(expected));
        }
    }

    #[test]
    fn package_manager_strict_mode_parses_bool_back_compat() {
        // `true`/`false` (and the shell-style `1`/`0` admitted by the
        // generic bool parser) need to keep working so projects on the
        // pre-tri-state default don't break.
        for (input, expected) in [
            ("true", PackageManagerStrictMode::Error),
            ("false", PackageManagerStrictMode::Off),
            ("1", PackageManagerStrictMode::Error),
            ("0", PackageManagerStrictMode::Off),
        ] {
            assert_eq!(PackageManagerStrictMode::parse(input), Some(expected));
        }
    }

    #[test]
    fn package_manager_strict_mode_returns_none_for_typos() {
        // Caller turns `None` into a startup warning + default. The
        // unit test pins the precondition: parse must NOT silently
        // coerce a typo to the default.
        assert!(PackageManagerStrictMode::parse("errror").is_none());
        assert!(PackageManagerStrictMode::parse("warning").is_none());
        assert!(PackageManagerStrictMode::parse("").is_none());
    }
}

#[cfg(test)]
mod cli_ordering_tests {
    use super::*;
    use clap::CommandFactory;
    use std::collections::BTreeMap;

    /// Validate that aube's CLI commands and arguments are ordered:
    /// - Subcommands alphabetical by name
    /// - Short flags alphabetical by short option
    /// - Long-only flags alphabetical by long name *within each help-heading
    ///   bucket* (the unheaded default counts as one bucket)
    ///
    /// We can't use `clap_sort::assert_sorted` directly because flags from
    /// flattened `cli_args::*Args` groups carry their own `help_heading`
    /// (e.g. "Lockfile", "Network", "Virtual store") and clap-sort enforces
    /// strict alphabetical across the full long-only set, which would
    /// require interleaving group flags between per-command flags. The
    /// help-grouped layout is the whole point of the move, so we sort
    /// within heading buckets instead.
    #[test]
    fn test_cli_ordering() {
        check_command_sorted(&Cli::command(), &[]);
    }

    fn check_command_sorted(cmd: &clap::Command, path: &[&str]) {
        let mut current_path: Vec<&str> = path.to_vec();
        current_path.push(cmd.get_name());

        // Subcommands alphabetical
        let names: Vec<_> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert!(
            names == sorted,
            "Subcommands in '{}' are not sorted alphabetically!\nActual: {:?}\nExpected: {:?}",
            current_path.join(" "),
            names,
            sorted,
        );

        // Short flags alphabetical, long-only alphabetical within heading.
        let mut shorts: Vec<char> = Vec::new();
        let mut by_heading: BTreeMap<Option<&str>, Vec<&str>> = BTreeMap::new();
        for arg in cmd.get_arguments() {
            if let Some(s) = arg.get_short() {
                shorts.push(s);
            } else if let Some(l) = arg.get_long() {
                by_heading
                    .entry(arg.get_help_heading())
                    .or_default()
                    .push(l);
            }
        }
        let mut sorted_shorts = shorts.clone();
        sorted_shorts.sort_by_key(|c| (c.to_ascii_lowercase(), c.is_uppercase()));
        assert!(
            shorts == sorted_shorts,
            "Short flags in '{}' are not sorted!\nActual: {:?}\nExpected: {:?}",
            current_path.join(" "),
            shorts,
            sorted_shorts,
        );
        for (heading, longs) in &by_heading {
            let mut sorted_longs = longs.clone();
            sorted_longs.sort();
            assert!(
                longs == &sorted_longs,
                "Long-only flags under heading {:?} in '{}' are not sorted!\nActual: {:?}\nExpected: {:?}",
                heading,
                current_path.join(" "),
                longs,
                sorted_longs,
            );
        }

        for sub in cmd.get_subcommands() {
            check_command_sorted(sub, &current_path);
        }
    }
}
