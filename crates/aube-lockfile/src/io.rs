use crate::{LockedPackage, LockfileGraph, bun, npm, pnpm, yarn};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which source lockfile format was parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileKind {
    /// `aube-lock.yaml` — aube's default lockfile when no existing
    /// lockfile is present. Same on-disk format as pnpm v9 for now
    /// (we piggyback on pnpm::read/write).
    Aube,
    /// `pnpm-lock.yaml` — pnpm v9 format. If this is the existing
    /// project lockfile, aube reads and writes it in place.
    Pnpm,
    Npm,
    /// `yarn.lock` v1 (classic yarn). Line-based text format with
    /// 2-space indented fields.
    Yarn,
    /// `yarn.lock` v2+ (yarn berry). YAML format with `__metadata:`
    /// header, `resolution:` / `checksum:` fields, and
    /// `languageName` / `linkType`. Same filename as `Yarn`; detection
    /// peeks at the content for the `__metadata:` marker to pick
    /// between the two.
    YarnBerry,
    NpmShrinkwrap,
    Bun,
}

impl LockfileKind {
    pub fn filename(self) -> &'static str {
        match self {
            LockfileKind::Aube => "aube-lock.yaml",
            LockfileKind::Pnpm => "pnpm-lock.yaml",
            LockfileKind::Npm => "package-lock.json",
            LockfileKind::Yarn | LockfileKind::YarnBerry => "yarn.lock",
            LockfileKind::NpmShrinkwrap => "npm-shrinkwrap.json",
            LockfileKind::Bun => "bun.lock",
        }
    }
}

/// Atomic lockfile write. Tempfile in the same dir, fsync, rename
/// over the target. Every format writer goes through this so a
/// crash or Ctrl+C mid-write cannot leave a truncated lockfile on
/// disk. Rename is atomic on POSIX, on Windows MoveFileEx gives
/// the same guarantee post Win10. Caller passes the serialized
/// bytes already formatted, this just handles the IO layer.
pub(crate) fn atomic_write_lockfile(path: &Path, body: &[u8]) -> Result<(), Error> {
    aube_util::fs_atomic::atomic_write(path, body).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// Write a lockfile to the given project directory using aube's default
/// filename (`aube-lock.yaml`, or `aube-lock.<branch>.yaml` when branch
/// lockfiles are enabled).
pub fn write_lockfile(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    write_lockfile_as(project_dir, graph, manifest, LockfileKind::Aube)?;
    Ok(())
}

/// Collapse peer-context variants from `graph` into a single map keyed
/// by `"name@version"`, pointing at the first-seen package. Several
/// writers (npm, yarn, …) share this shape: one canonical entry per
/// `(name, version)` pair regardless of how many peer suffixes the
/// full graph emits.
pub fn build_canonical_map(graph: &LockfileGraph) -> BTreeMap<String, &LockedPackage> {
    let mut canonical: BTreeMap<String, &LockedPackage> = BTreeMap::new();
    for pkg in graph.packages.values() {
        canonical.entry(pkg.spec_key()).or_insert(pkg);
    }
    canonical
}

/// Write a lockfile using the existing project lockfile kind, or
/// `aube-lock.yaml` when the project does not have one yet.
///
/// This is the default write path for commands that mutate the active
/// project graph (`install`, `add`, `remove`, `update`, `dedupe`, ...).
pub fn write_lockfile_preserving_existing(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<PathBuf, Error> {
    let kind = detect_existing_lockfile_kind(project_dir).unwrap_or(LockfileKind::Aube);
    write_lockfile_as(project_dir, graph, manifest, kind)
}

/// Write `graph` in the requested lockfile format into `project_dir`.
///
/// Returns the path that was actually written (useful for logging
/// since `Aube` may resolve to a branch-specific filename). Callers
/// that want to preserve whatever format was already on disk should
/// pair this with [`detect_existing_lockfile_kind`].
///
/// All supported formats: `Aube`, `Pnpm`, `Npm`, `NpmShrinkwrap`,
/// `Yarn`, and `Bun`. This preserves the lockfile kind that already
/// exists in the project; callers should pass `Aube` only when no
/// lockfile exists yet. See each writer module's doc comment for
/// per-format lossy areas (peer contexts, `resolved` URLs, etc.).
pub fn write_lockfile_as(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    kind: LockfileKind,
) -> Result<PathBuf, Error> {
    let _diag = aube_util::diag::Span::new(aube_util::diag::Category::Lockfile, "write")
        .with_meta_fn(|| {
            format!(
                r#"{{"kind":{},"packages":{}}}"#,
                aube_util::diag::jstr(&format!("{:?}", kind)),
                graph.packages.len()
            )
        });
    let filename = match kind {
        LockfileKind::Aube => aube_lock_filename(project_dir),
        LockfileKind::Pnpm => pnpm_lock_filename(project_dir),
        other => other.filename().to_string(),
    };
    let path = project_dir.join(&filename);
    match kind {
        LockfileKind::Aube | LockfileKind::Pnpm => pnpm::write(&path, graph, manifest)?,
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => npm::write(&path, graph, manifest)?,
        LockfileKind::Yarn => yarn::write_classic(&path, graph, manifest)?,
        LockfileKind::YarnBerry => yarn::write_berry(&path, graph, manifest)?,
        LockfileKind::Bun => bun::write(&path, graph, manifest)?,
    }
    Ok(path)
}

/// Return the [`LockfileKind`] of the lockfile already on disk in
/// `project_dir`, if any. Follows the same precedence as
/// [`parse_lockfile_with_kind`] (aube > pnpm > bun > yarn >
/// npm-shrinkwrap > npm). Used by install to preserve a project's
/// existing lockfile format when rewriting after a re-resolve — a
/// user with only `pnpm-lock.yaml`, `package-lock.json`, or another
/// supported lockfile gets that file written back, not a surprise
/// `aube-lock.yaml` alongside it.
pub fn detect_existing_lockfile_kind(project_dir: &Path) -> Option<LockfileKind> {
    for (path, kind) in lockfile_candidates(project_dir, /*include_aube=*/ true) {
        if path.exists() {
            return Some(refine_yarn_kind(&path, kind));
        }
    }
    None
}

/// Return true when the active lockfile contains Git conflict markers.
///
/// Used by install's prefer-frozen path to distinguish a merge/rebase
/// artifact from an arbitrary parse error: conflict markers can be
/// repaired by regenerating from the already-resolved `package.json`,
/// while other parse failures should stay loud.
pub fn active_lockfile_has_conflict_markers(project_dir: &Path) -> bool {
    for (path, _) in lockfile_candidates(project_dir, /*include_aube=*/ true) {
        if !path.exists() {
            continue;
        }
        return read_lockfile(&path)
            .map(|content| has_conflict_markers(&content))
            .unwrap_or(false);
    }
    false
}

fn has_conflict_markers(content: &str) -> bool {
    content.lines().any(|line| {
        line.starts_with("<<<<<<< ")
            || line.trim_end_matches('\r') == "======="
            || line.starts_with(">>>>>>> ")
    })
}

/// Resolve the canonical lockfile filename for `project_dir` (aube's own).
///
/// Returns `aube-lock.<branch>.yaml` when `gitBranchLockfile: true` is
/// set in `pnpm-workspace.yaml` (or `aube-workspace.yaml`) and the
/// project is inside a git checkout with a current branch. Forward
/// slashes in the branch name are encoded as `!`, matching pnpm. Falls
/// back to plain `aube-lock.yaml` in every other case.
///
/// Memoized per `project_dir` for the lifetime of the process: a
/// single install resolves this 3–5 times (lockfile_candidates,
/// write_lockfile, debug log, state read/write), and
/// `check_needs_install` runs on every `aube run`/`aube exec` via
/// `ensure_installed`. Without caching, every command would pay for a
/// YAML parse + a `git branch --show-current` subprocess just to
/// recompute a value that can't change mid-process.
pub fn aube_lock_filename(project_dir: &Path) -> String {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(project_dir)
    {
        return hit.clone();
    }
    let resolved = if !git_branch_lockfile_enabled(project_dir) {
        "aube-lock.yaml".to_string()
    } else {
        match current_git_branch(project_dir) {
            Some(branch) => format!("aube-lock.{}.yaml", branch.replace('/', "!")),
            None => "aube-lock.yaml".to_string(),
        }
    };
    if let Ok(mut map) = cache.lock() {
        map.insert(project_dir.to_path_buf(), resolved.clone());
    }
    resolved
}

/// Resolve the pnpm lockfile filename for `project_dir`.
///
/// Mirrors [`aube_lock_filename`] for branch lockfiles, but keeps the
/// pnpm filename prefix so projects with an existing `pnpm-lock.yaml`
/// keep writing to pnpm's file.
pub fn pnpm_lock_filename(project_dir: &Path) -> String {
    let aube_name = aube_lock_filename(project_dir);
    // `aube_lock_filename` always returns "aube-lock.<rest>", so strip_prefix
    // always succeeds. The fallback is purely defensive.
    aube_name
        .strip_prefix("aube-lock.")
        .map(|rest| format!("pnpm-lock.{rest}"))
        .unwrap_or_else(|| "pnpm-lock.yaml".to_string())
}

fn git_branch_lockfile_enabled(project_dir: &Path) -> bool {
    // Goes through the build-time-generated typed accessor in
    // `aube_settings::resolved` so the alias list is driven off
    // `settings.toml` — no hand-maintained typed field. This path
    // reads only `pnpm-workspace.yaml`; `.npmrc` values are out of
    // scope here because aube-lockfile doesn't want a dependency on
    // aube-registry just to load npmrc (and the historical behavior
    // never read `.npmrc` either).
    let Ok(raw) = aube_manifest::workspace::load_raw(project_dir) else {
        return false;
    };
    let npmrc: Vec<(String, String)> = Vec::new();
    let ctx = aube_settings::ResolveCtx::files_only(&npmrc, &raw);
    aube_settings::resolved::git_branch_lockfile(&ctx)
}

pub(crate) fn current_git_branch(project_dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(project_dir)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// Detect and parse the lockfile in the given project directory.
///
/// Priority: `aube-lock.yaml` → `pnpm-lock.yaml` → `bun.lock` →
/// `yarn.lock` → `npm-shrinkwrap.json` → `package-lock.json`.
/// (Shrinkwrap takes priority over package-lock.json when both exist, matching npm's behavior.)
///
/// `manifest` is needed to classify direct vs transitive deps when
/// reading yarn.lock (which has no notion of that distinction).
pub fn parse_lockfile(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let (graph, _kind) = parse_lockfile_with_kind(project_dir, manifest)?;
    Ok(graph)
}

/// Like [`parse_lockfile`] but also returns which format was read.
pub fn parse_lockfile_with_kind(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<(LockfileGraph, LockfileKind), Error> {
    reject_bun_binary(project_dir)?;
    for (path, kind) in lockfile_candidates(project_dir, /*include_aube=*/ true) {
        if !path.exists() {
            continue;
        }
        let kind = refine_yarn_kind(&path, kind);
        let graph = parse_one(&path, kind, manifest)?;
        return Ok((graph, kind));
    }
    Err(Error::NotFound(project_dir.to_path_buf()))
}

/// Variant of [`parse_lockfile_with_kind`] used by `aube import`.
///
/// Skips `aube-lock.yaml` — if the project already has one, there's
/// nothing to import. `pnpm-lock.yaml` *is* included because the whole
/// point of `aube import` is to convert a foreign lockfile (including
/// pnpm's) into `aube-lock.yaml`.
pub fn parse_for_import(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<(LockfileGraph, LockfileKind), Error> {
    reject_bun_binary(project_dir)?;
    for (path, kind) in lockfile_candidates(project_dir, /*include_aube=*/ false) {
        if !path.exists() {
            continue;
        }
        let kind = refine_yarn_kind(&path, kind);
        let graph = parse_one(&path, kind, manifest)?;
        return Ok((graph, kind));
    }
    Err(Error::NotFound(project_dir.to_path_buf()))
}

/// If only `bun.lockb` is present (without a text `bun.lock`), surface an
/// actionable error instead of silently falling through to another format.
fn reject_bun_binary(project_dir: &Path) -> Result<(), Error> {
    let lockb = project_dir.join("bun.lockb");
    let text = project_dir.join("bun.lock");
    if lockb.exists() && !text.exists() {
        return Err(Error::parse(
            &lockb,
            "bun.lockb (binary format) is not supported — run `bun install --save-text-lockfile` to generate a bun.lock text file first, or upgrade to bun 1.2+ where text is the default",
        ));
    }
    Ok(())
}

fn lockfile_candidates(project_dir: &Path, include_aube: bool) -> Vec<(PathBuf, LockfileKind)> {
    let mut out = Vec::new();
    if include_aube {
        // Prefer the branch-specific lockfile (if `gitBranchLockfile` is on
        // and we resolve a branch); fall through to plain `aube-lock.yaml`
        // so a freshly-enabled branch still picks up the base lockfile.
        let branch_name = aube_lock_filename(project_dir);
        if branch_name != "aube-lock.yaml" {
            out.push((project_dir.join(&branch_name), LockfileKind::Aube));
        }
        out.push((project_dir.join("aube-lock.yaml"), LockfileKind::Aube));
    }
    // Preserve pnpm lockfiles in place. Branch-specific
    // `pnpm-lock.<branch>.yaml` mirrors the aube branch lockfile naming
    // logic, so a project that already uses pnpm branch lockfiles keeps
    // writing through that file.
    let pnpm_branch = {
        let mut s = aube_lock_filename(project_dir);
        if let Some(rest) = s.strip_prefix("aube-lock.") {
            s = format!("pnpm-lock.{rest}");
        }
        s
    };
    if pnpm_branch != "pnpm-lock.yaml" {
        out.push((project_dir.join(&pnpm_branch), LockfileKind::Pnpm));
    }
    out.push((project_dir.join("pnpm-lock.yaml"), LockfileKind::Pnpm));
    out.push((project_dir.join("bun.lock"), LockfileKind::Bun));
    out.push((project_dir.join("yarn.lock"), LockfileKind::Yarn));
    out.push((
        project_dir.join("npm-shrinkwrap.json"),
        LockfileKind::NpmShrinkwrap,
    ));
    out.push((project_dir.join("package-lock.json"), LockfileKind::Npm));
    out
}

fn parse_one(
    path: &Path,
    kind: LockfileKind,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let _diag = aube_util::diag::Span::new(aube_util::diag::Category::Lockfile, "parse_one")
        .with_meta_fn(|| {
            // Emit only the file name (e.g. `aube-lock.yaml`) so traces
            // do not leak absolute project paths.
            let display = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!(
                r#"{{"kind":{},"path":{}}}"#,
                aube_util::diag::jstr(&format!("{:?}", kind)),
                aube_util::diag::jstr(&display)
            )
        });
    let graph = match kind {
        // `aube-lock.yaml` uses the same on-disk format as pnpm v9 for
        // now — same parser, same writer — so we piggyback on the pnpm
        // module. Keeping the variant distinct lets detection/import
        // treat the two differently even though the bytes are the same.
        LockfileKind::Aube | LockfileKind::Pnpm => pnpm::parse(path),
        // yarn.rs::parse peeks the file for `__metadata:` and
        // dispatches between classic (v1) and berry (v2+) internally,
        // so we can hand both kinds to the same entry point. The
        // caller keeps the kind label it resolved from
        // `refine_yarn_kind` for downstream write-back.
        LockfileKind::Yarn | LockfileKind::YarnBerry => yarn::parse(path, manifest),
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => npm::parse(path),
        LockfileKind::Bun => bun::parse(path),
    }?;
    validate_resolution_shapes(path, &graph)?;
    Ok(graph)
}

fn validate_resolution_shapes(path: &Path, graph: &LockfileGraph) -> Result<(), Error> {
    for (dep_path, pkg) in &graph.packages {
        if pkg.local_source.is_some() && dep_path_has_registry_version(dep_path, &pkg.name) {
            return Err(Error::ResolutionShapeMismatch {
                path: path.to_path_buf(),
                dep_path: dep_path.clone(),
                source_kind: pkg
                    .local_source
                    .as_ref()
                    .map(|source| source.kind_str())
                    .unwrap_or("unknown"),
            });
        }
    }
    Ok(())
}

fn dep_path_has_registry_version(dep_path: &str, name: &str) -> bool {
    let Some(tail) = dep_path
        .strip_prefix('/')
        .unwrap_or(dep_path)
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('@'))
    else {
        return false;
    };
    let version = tail.split('(').next().unwrap_or(tail);
    node_semver::Version::parse(version).is_ok()
}

/// Replace `LockfileKind::Yarn` with `LockfileKind::YarnBerry` when
/// the yarn.lock at `path` is actually a yarn 2+ lockfile. Other
/// kinds pass through unchanged.
///
/// `lockfile_candidates` only knows filenames, not content, so the
/// yarn entry is always tagged `Yarn`. Callers that need the precise
/// variant (install write-back, import conversions, drift logging)
/// funnel through this helper after confirming the candidate exists.
fn refine_yarn_kind(path: &Path, kind: LockfileKind) -> LockfileKind {
    if kind == LockfileKind::Yarn && yarn::is_berry_path(path) {
        LockfileKind::YarnBerry
    } else {
        kind
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("no lockfile found in {0}")]
    #[diagnostic(code(ERR_AUBE_NO_LOCKFILE))]
    NotFound(std::path::PathBuf),
    #[error("unsupported lockfile format: {0}")]
    #[diagnostic(code(ERR_AUBE_LOCKFILE_UNSUPPORTED_FORMAT))]
    UnsupportedFormat(String),
    #[error("failed to read lockfile {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),
    /// Structural/serialization lockfile errors that have no source
    /// location — shape checks (`must be a mapping`), version guards
    /// (`lockfileVersion N unsupported`), and `yaml_serde::to_string`
    /// failures during write.
    #[error("failed to parse lockfile {0}: {1}")]
    #[diagnostic(code(ERR_AUBE_LOCKFILE_PARSE))]
    Parse(std::path::PathBuf, String),
    #[error(
        "lockfile {path} has registry-style dependency path `{dep_path}` backed by {source_kind} resolution"
    )]
    #[diagnostic(
        code(ERR_AUBE_RESOLUTION_SHAPE_MISMATCH),
        help(
            "run `aube install --no-frozen-lockfile` from a trusted manifest to regenerate the lockfile"
        )
    )]
    ResolutionShapeMismatch {
        path: std::path::PathBuf,
        dep_path: String,
        source_kind: &'static str,
    },
    /// Deserialization failure with a byte offset into the source
    /// content, so miette's `fancy` handler can draw a pointer at the
    /// offending byte of the lockfile. Reuses `aube_manifest`'s
    /// `ParseError` — identical shape, identical rendering — via the
    /// same `ParseDiag` pattern `aube-workspace` uses.
    #[error(transparent)]
    #[diagnostic(transparent)]
    ParseDiag(Box<aube_manifest::ParseError>),
}

/// Read a lockfile from disk, mapping I/O errors to `Error::Io`.
pub fn read_lockfile(path: &std::path::Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// Parse a JSON lockfile document, attaching a miette source span on
/// failure so the fancy handler can point at the offending byte.
pub fn parse_json<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    content: String,
) -> Result<T, Error> {
    // sonic-rs takes an immutable &[u8], so the original `content`
    // bytes stay intact for the serde_json fallback's diagnostic.
    match sonic_rs::from_slice(content.as_bytes()) {
        Ok(v) => Ok(v),
        Err(_) => match serde_json::from_str(&content) {
            Ok(v) => Ok(v),
            Err(e) => Err(Error::parse_json_err(path, content, &e)),
        },
    }
}

impl Error {
    pub fn parse(path: &std::path::Path, msg: impl Into<String>) -> Self {
        Error::Parse(path.to_path_buf(), msg.into())
    }

    pub fn parse_json_err(
        path: &std::path::Path,
        content: String,
        err: &serde_json::Error,
    ) -> Self {
        Error::ParseDiag(Box::new(aube_manifest::ParseError::from_json_err(
            path, content, err,
        )))
    }

    pub fn parse_yaml_err(
        path: &std::path::Path,
        content: String,
        err: &yaml_serde::Error,
    ) -> Self {
        Error::ParseDiag(Box::new(aube_manifest::ParseError::from_yaml_err(
            path, content, err,
        )))
    }
}

#[cfg(test)]
mod parse_diag_tests {
    use super::*;
    use crate::{LocalSource, LockedPackage};
    use std::path::Path;

    /// Trailing `,` in an otherwise fine JSON lockfile — confirm the
    /// helper attaches a `NamedSource` pointed at the lockfile path and
    /// the span stays in bounds so miette can render a pointer.
    #[test]
    fn parse_json_attaches_span_for_bad_input() {
        let path = Path::new("package-lock.json");
        let content = r#"{"name":"x","#.to_string();
        let Err(Error::ParseDiag(pe)) = parse_json::<serde_json::Value>(path, content.clone())
        else {
            panic!("parse_json must produce ParseDiag on malformed input");
        };
        let offset: usize = pe.span.offset();
        let len: usize = pe.span.len();
        assert!(offset + len <= content.len());
        assert_eq!(pe.path, path);
    }

    #[test]
    fn validate_resolution_shapes_rejects_local_source_with_registry_dep_path() {
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "left-pad@1.3.0".to_string(),
            LockedPackage {
                name: "left-pad".to_string(),
                version: "1.3.0".to_string(),
                dep_path: "left-pad@1.3.0".to_string(),
                local_source: Some(LocalSource::Directory("vendor/left-pad".into())),
                ..Default::default()
            },
        );

        let err = validate_resolution_shapes(Path::new("pnpm-lock.yaml"), &graph).unwrap_err();
        assert!(matches!(
            err,
            Error::ResolutionShapeMismatch {
                dep_path,
                source_kind: "file",
                ..
            } if dep_path == "left-pad@1.3.0"
        ));
    }

    #[test]
    fn validate_resolution_shapes_rejects_peer_suffixed_registry_dep_path() {
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "plugin@1.0.0(react@19.0.0)".to_string(),
            LockedPackage {
                name: "plugin".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "plugin@1.0.0(react@19.0.0)".to_string(),
                local_source: Some(LocalSource::RemoteTarball(crate::RemoteTarballSource {
                    url: "https://example.com/plugin.tgz".to_string(),
                    integrity: "sha512-test".to_string(),
                    git_hosted: false,
                })),
                ..Default::default()
            },
        );

        let err = validate_resolution_shapes(Path::new("pnpm-lock.yaml"), &graph).unwrap_err();
        assert!(matches!(
            err,
            Error::ResolutionShapeMismatch {
                dep_path,
                source_kind: "url",
                ..
            } if dep_path == "plugin@1.0.0(react@19.0.0)"
        ));
    }

    #[test]
    fn validate_resolution_shapes_allows_local_source_dep_path() {
        let source = LocalSource::Directory("vendor/left-pad".into());
        let dep_path = source.dep_path("left-pad");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: "left-pad".to_string(),
                version: "1.3.0".to_string(),
                dep_path,
                local_source: Some(source),
                ..Default::default()
            },
        );

        validate_resolution_shapes(Path::new("pnpm-lock.yaml"), &graph).unwrap();
    }

    /// Same story for YAML — yaml_serde reports a `Location` with a
    /// byte index directly, so no line/col conversion is exercised
    /// here. Both production sites (`pnpm.rs`, `yarn.rs`) call
    /// `Error::parse_yaml_err` directly (one iterates multiple YAML
    /// documents, the other has only borrowed content), so that's the
    /// entry point this test locks down.
    #[test]
    fn parse_yaml_err_attaches_span_for_bad_input() {
        let path = Path::new("yarn.lock");
        let content = "packages:\n\t- pkg\n".to_string();
        let yaml_err: yaml_serde::Error = yaml_serde::from_str::<yaml_serde::Value>(&content)
            .expect_err("tab-indented YAML must fail");
        let Error::ParseDiag(pe) = Error::parse_yaml_err(path, content.clone(), &yaml_err) else {
            panic!("parse_yaml_err must produce ParseDiag");
        };
        let offset: usize = pe.span.offset();
        let len: usize = pe.span.len();
        assert!(offset + len <= content.len());
        assert_eq!(pe.path, path);
    }
}

#[cfg(test)]
mod filename_tests {
    use super::*;

    #[test]
    fn defaults_to_plain_lockfile_when_setting_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(aube_lock_filename(dir.path()), "aube-lock.yaml");
        assert_eq!(pnpm_lock_filename(dir.path()), "pnpm-lock.yaml");
    }

    #[test]
    fn defaults_to_plain_lockfile_when_setting_explicit_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "gitBranchLockfile: false\n",
        )
        .unwrap();
        assert_eq!(aube_lock_filename(dir.path()), "aube-lock.yaml");
    }

    #[test]
    fn uses_branch_filename_when_enabled_inside_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "gitBranchLockfile: true\n",
        )
        .unwrap();
        // git init + checkout a branch with a `/` so we exercise the
        // pnpm-style `!` encoding.
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C"])
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
        };
        if run(&["init", "-q"]).status.success() {
            run(&["checkout", "-q", "-b", "feature/x"]);
            assert_eq!(aube_lock_filename(dir.path()), "aube-lock.feature!x.yaml");
            assert_eq!(pnpm_lock_filename(dir.path()), "pnpm-lock.feature!x.yaml");
        }
    }
}
