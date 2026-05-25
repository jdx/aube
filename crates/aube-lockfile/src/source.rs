use std::path::{Path, PathBuf};

/// Non-registry source for a locked package.
///
/// When a package comes from a local path (via `file:` or `link:` in
/// `package.json`) it doesn't have a tarball URL or integrity hash, so we
/// record the source separately and let the linker materialize it
/// on-the-fly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSource {
    /// `file:<dir>` — a directory on disk whose contents should be
    /// hardlink-copied into the virtual store like a normal package.
    /// Path is stored relative to the project root.
    Directory(PathBuf),
    /// `file:<tarball>` — a `.tgz` on disk, extracted into the virtual
    /// store the same way we extract registry tarballs.
    Tarball(PathBuf),
    /// `link:<dir>` — a plain symlink into `node_modules/<name>`, never
    /// materialized into the virtual store. Transitive deps are the
    /// target's responsibility.
    Link(PathBuf),
    /// `portal:<dir>` — a Yarn Berry package portal. The target is a
    /// package on disk, but unlike `link:` its dependencies are still
    /// modeled in the lockfile graph.
    Portal(PathBuf),
    /// `exec:<script>` — a Yarn Berry generator script. The script is
    /// executed at fetch time and writes the package files into a
    /// generated build directory.
    Exec(PathBuf),
    /// `git+https://`, `git+ssh://`, `github:user/repo`, etc. — a
    /// remote git repo. Cloned at fetch time and imported like a
    /// `file:` directory. `url` is the normalized clone URL (what
    /// gets passed to `git clone`). `committish` is the user-written
    /// ref after `#` (branch, tag, or commit; `None` means HEAD).
    /// `resolved` is the 40-char commit SHA that `git ls-remote`
    /// pinned the ref to — the lockfile records this so repeat
    /// installs reproduce bit-for-bit.
    Git(GitSource),
    /// `https://example.com/pkg.tgz` — a remote tarball URL. Fetched
    /// once at resolve time so the resolver can read the enclosed
    /// `package.json` for version + transitive deps and pin the
    /// sha512 integrity. `integrity` stays empty on freshly-parsed
    /// specifiers and is filled in by the resolver after download.
    RemoteTarball(RemoteTarballSource),
}

/// A remote tarball dependency spec. See [`LocalSource::RemoteTarball`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarballSource {
    pub url: String,
    pub integrity: String,
}

/// A git dependency spec. See [`LocalSource::Git`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub url: String,
    pub committish: Option<String>,
    pub resolved: String,
    /// SHA-512 SRI of the hosted tarball bytes when the git source was
    /// fetched through a codeload-style archive. Plain git-clone sources
    /// leave this unset because git object IDs verify the checkout.
    pub integrity: Option<String>,
    /// pnpm `&path:/sub/dir` selector — when set, only this
    /// subdirectory of the cloned repo is treated as the package
    /// root. Stored without leading slash so dep_path hashes are
    /// stable regardless of whether the user wrote `path:/x` or
    /// `path:x`.
    pub subpath: Option<String>,
}

impl LocalSource {
    /// The original path (relative to the project root) the user wrote
    /// in `package.json`. `None` for non-path sources like git.
    pub fn path(&self) -> Option<&Path> {
        match self {
            LocalSource::Directory(p)
            | LocalSource::Tarball(p)
            | LocalSource::Link(p)
            | LocalSource::Portal(p)
            | LocalSource::Exec(p) => Some(p),
            LocalSource::Git(_) | LocalSource::RemoteTarball(_) => None,
        }
    }

    /// The protocol kind (`"file"` / `"link"` / `"git"` / `"url"`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            LocalSource::Directory(_) | LocalSource::Tarball(_) => "file",
            LocalSource::Link(_) => "link",
            LocalSource::Portal(_) => "portal",
            LocalSource::Exec(_) => "exec",
            LocalSource::Git(_) => "git",
            LocalSource::RemoteTarball(_) => "url",
        }
    }

    /// The path as a POSIX-style string with forward-slash separators.
    /// `Path::display()` and `to_string_lossy()` honor the host's
    /// separator (backslash on Windows), which would make `dep_path`
    /// hashes and lockfile `specifier:` strings non-portable: the
    /// same `file:./some/dir` would render as `some\dir` on Windows
    /// and `some/dir` on Unix, producing two different hashes for
    /// the same logical target. Always rendering with `/` keeps
    /// lockfiles cross-platform identical.
    pub fn path_posix(&self) -> String {
        self.path()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    }

    /// Canonical specifier string as pnpm writes it in the `packages:`
    /// and `snapshots:` keys (post-`<name>@` part). For `file:` /
    /// `link:` this is `file:./vendor/foo` / `link:../sibling`. For
    /// `git`, pnpm uses the resolved form `<url>#<commit>` (no
    /// `git+` prefix) because the lockfile pins to the exact commit
    /// regardless of what the user wrote. Always emits POSIX
    /// separators so the resulting lockfile is portable.
    pub fn specifier(&self) -> String {
        match self {
            LocalSource::Git(g) => match &g.subpath {
                Some(sub) => format!("{}#{}&path:/{}", g.url, g.resolved, sub),
                None => format!("{}#{}", g.url, g.resolved),
            },
            LocalSource::RemoteTarball(t) => t.url.clone(),
            _ => format!("{}:{}", self.kind_str(), self.path_posix()),
        }
    }

    /// Internal FS-safe dep_path used as the key in
    /// `LockfileGraph.packages` and as the `.aube/` subdir name.
    ///
    /// Distinct paths must map to distinct keys (otherwise the
    /// linker would silently mix files between two local packages),
    /// and the result must be a single filesystem component — no
    /// `/`, `\`, `:`, or `..`. Ad-hoc character substitution trips
    /// over cases like `../vendor` vs `__/vendor` or `a.b` vs `a_b`
    /// collapsing to the same string, so we hash the raw path bytes
    /// and suffix the first 16 hex chars (64 bits — more than enough
    /// to avoid collisions inside a single project).
    ///
    /// The hash input is the POSIX-form path string so a checked-in
    /// lockfile resolves to the same key regardless of which
    /// platform ran `aube install`.
    pub fn dep_path(&self, name: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        match self {
            LocalSource::Git(g) => {
                hasher.update(g.url.as_bytes());
                hasher.update(b"#");
                hasher.update(g.resolved.as_bytes());
                if let Some(sub) = &g.subpath {
                    hasher.update(b"&path:/");
                    hasher.update(sub.as_bytes());
                }
            }
            LocalSource::RemoteTarball(t) => {
                hasher.update(t.url.as_bytes());
            }
            _ => hasher.update(self.path_posix().as_bytes()),
        }
        let digest = hasher.finalize();
        let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("{name}@{}+{short}", self.kind_str())
    }

    /// Classify a user-written `file:` / `link:` specifier against the
    /// project root. Returns `None` if `spec` isn't a local specifier.
    /// Resolves the target path relative to `project_root`; a `file:`
    /// target that resolves to a `.tgz` / `.tar.gz` on disk is treated
    /// as a tarball, anything else as a directory.
    pub fn parse(spec: &str, project_root: &Path) -> Option<Self> {
        // Check git first so URLs like `https://host/user/repo.git`
        // aren't swallowed by the broader bare-http tarball check
        // below.
        if let Some((url, committish, subpath)) = parse_git_spec(spec) {
            // `resolved` is filled in by the resolver after running
            // `git ls-remote`. A lockfile round-trip that never
            // re-resolves will leave this empty, which is the sentinel
            // the resolver checks for before calling ls-remote.
            return Some(LocalSource::Git(GitSource {
                url,
                committish,
                resolved: String::new(),
                integrity: None,
                subpath,
            }));
        }
        // Any remaining bare `http(s)://` URL is a remote tarball.
        // npm semantics treat *all* non-git HTTP URLs in a dependency
        // value as tarball URLs, so services that serve tarballs from
        // URLs without a `.tgz` extension (pkg.pr.new, GitHub
        // codeload, etc.) classify correctly here.
        if Self::looks_like_remote_tarball_url(spec) {
            return Some(LocalSource::RemoteTarball(RemoteTarballSource {
                url: spec.to_string(),
                integrity: String::new(),
            }));
        }
        let (kind, rest) = if let Some(r) = spec.strip_prefix("file:") {
            ("file", r)
        } else if let Some(r) = spec.strip_prefix("link:") {
            ("link", r)
        } else if let Some(r) = spec.strip_prefix("portal:") {
            ("portal", r)
        } else if let Some(r) = spec.strip_prefix("exec:") {
            return Some(LocalSource::Exec(PathBuf::from(r)));
        } else {
            return None;
        };
        let rel = PathBuf::from(rest);
        let abs = project_root.join(&rel);
        if kind == "link" {
            return Some(LocalSource::Link(rel));
        }
        if kind == "portal" {
            return Some(LocalSource::Portal(rel));
        }
        if abs.is_file() && Self::path_looks_like_tarball(&rel) {
            return Some(LocalSource::Tarball(rel));
        }
        Some(LocalSource::Directory(rel))
    }

    /// Whether a specifier looks like a direct HTTP(S) URL that should
    /// be fetched as a tarball. Per npm semantics, *any* `http://` or
    /// `https://` URL in a dependency value is a tarball URL — services
    /// like pkg.pr.new, GitHub codeload, and private registries with
    /// auth-token query strings serve tarballs from URLs that don't
    /// carry a `.tgz` extension. Git URLs must already have been
    /// ruled out by the caller (see [`parse_git_spec`]) so a
    /// `.git`-suffixed URL doesn't get misclassified here.
    pub fn looks_like_remote_tarball_url(spec: &str) -> bool {
        spec.starts_with("https://") || spec.starts_with("http://")
    }

    pub fn path_looks_like_tarball(path: &Path) -> bool {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return false,
        };
        let lower = name.to_ascii_lowercase();
        lower.ends_with(".tgz") || lower.ends_with(".tar.gz")
    }
}

/// Parse a git dependency specifier into `(clone_url, committish)`.
///
/// Recognized forms:
/// - `git+https://host/user/repo.git[#ref]`
/// - `git+ssh://git@host/user/repo.git[#ref]`
/// - `git://host/user/repo.git[#ref]`
/// - `https://host/user/repo.git[#ref]` (only when ending in `.git`)
/// - `user@host:path[.git][#ref]` (scp-form, only for github.com / gitlab.com /
///   bitbucket.org — matches pnpm 11 behavior, where unknown SCP hosts are
///   treated as local paths) → `ssh://user@host/path[.git]`
/// - `github:user/repo[#ref]` → `https://github.com/user/repo.git`
/// - `gitlab:user/repo[#ref]` → `https://gitlab.com/user/repo.git`
/// - `bitbucket:user/repo[#ref]` → `https://bitbucket.org/user/repo.git`
/// - `user/repo[#ref]` (bare GitHub shorthand, npm/pnpm compat)
///   → `https://github.com/user/repo.git`
///
/// Returns `None` for any specifier that doesn't look like a git URL,
/// so the caller can fall through to other protocol parsers.
pub fn parse_git_spec(spec: &str) -> Option<(String, Option<String>, Option<String>)> {
    let (body, committish, subpath) = match spec.find('#') {
        Some(idx) => {
            let (c, s) = parse_git_fragment(&spec[idx + 1..]);
            (&spec[..idx], c, s)
        }
        None => (spec, None, None),
    };
    let is_bare_transport = body.starts_with("https://")
        || body.starts_with("http://")
        || body.starts_with("ssh://")
        || body.starts_with("file://");
    let url = if let Some(rest) = body.strip_prefix("git+") {
        // `git+` explicitly tags the URL as git, so the `.git`
        // suffix is optional (GitHub/GitLab accept both forms).
        rest.to_string()
    } else if body.starts_with("git://") {
        body.to_string()
    } else if let Some(scp) = parse_scp_url(body) {
        scp
    } else if let Some(path) = body.strip_prefix("github:") {
        format!("https://github.com/{path}.git")
    } else if let Some(path) = body.strip_prefix("gitlab:") {
        format!("https://gitlab.com/{path}.git")
    } else if let Some(path) = body.strip_prefix("bitbucket:") {
        format!("https://bitbucket.org/{path}.git")
    } else if is_bare_transport && body.ends_with(".git") {
        body.to_string()
    } else if is_bare_transport
        && committish
            .as_deref()
            .is_some_and(|c| c.len() == 40 && c.chars().all(|ch| ch.is_ascii_hexdigit()))
    {
        // Lockfile round-trip form: `specifier()` writes the stored
        // URL verbatim plus `#<sha>`. URLs that dropped the `git+`
        // prefix (and happen to lack `.git`) are disambiguated from
        // plain tarball URLs by the 40-hex committish suffix.
        body.to_string()
    } else if is_bare_github_shorthand(body) {
        // npm/pnpm bare GitHub shorthand: `user/repo` expands to
        // `github:user/repo`. Placed last so all explicit URL/scheme
        // forms above shadow it.
        format!("https://github.com/{body}.git")
    } else {
        return None;
    };
    Some((url, committish, subpath))
}

/// `user/repo` — a single `/`, both segments non-empty, ASCII
/// alphanumeric + `_.-` only, owner doesn't start with `.` so
/// single-component relative paths (`./repo`, `../repo`) are rejected.
/// Excludes scoped npm names (`@scope/pkg`) and file paths. Other
/// URL/SCP forms are ruled out by placement order in `parse_git_spec`.
fn is_bare_github_shorthand(body: &str) -> bool {
    let Some((owner, repo)) = body.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !owner.starts_with('.')
        && !repo.is_empty()
        && !repo.contains('/')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A git URL that maps to one of the three "hosted" providers npm /
/// pnpm both special-case (github / gitlab / bitbucket). For these
/// hosts a public read can be served as a flat HTTPS tarball over
/// `codeload.github.com` (or each host's equivalent), bypassing `git`
/// entirely. The lockfile's stored URL is canonical-identity only —
/// pnpm and npm both re-derive the fetch URL from `(host, owner,
/// repo)` on every install rather than dialing whatever scheme
/// happens to be in `resolved:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedGit {
    pub host: HostedGitHost,
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedGitHost {
    GitHub,
    GitLab,
    Bitbucket,
}

impl HostedGit {
    /// `https://github.com/<owner>/<repo>.git` — the form `git fetch`
    /// can dial without an SSH key. Used as the runtime fetch URL when
    /// the lockfile's stored URL is `git+ssh://git@…` (npm canonical
    /// identity) but the actual install host has no SSH configured.
    pub fn https_url(&self) -> String {
        let host = self.host.host_domain();
        format!("https://{host}/{}/{}.git", self.owner, self.repo)
    }

    /// `https://codeload.github.com/<owner>/<repo>/tar.gz/<sha>` (or
    /// each host's equivalent) — a flat HTTPS tarball at the given
    /// commit. Returns `None` unless `committish` is a 40-char hex
    /// SHA, since the codeload path can't be verified after extraction
    /// without `.git/` metadata. Branch / tag names round-trip through
    /// `git ls-remote` to get pinned to a SHA first.
    pub fn tarball_url(&self, committish: &str) -> Option<String> {
        if committish.len() != 40 || !committish.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let sha = committish.to_ascii_lowercase();
        Some(match self.host {
            HostedGitHost::GitHub => format!(
                "https://codeload.github.com/{}/{}/tar.gz/{sha}",
                self.owner, self.repo
            ),
            HostedGitHost::GitLab => format!(
                "https://gitlab.com/{}/{}/-/archive/{sha}/{}-{sha}.tar.gz",
                self.owner, self.repo, self.repo
            ),
            HostedGitHost::Bitbucket => format!(
                "https://bitbucket.org/{}/{}/get/{sha}.tar.gz",
                self.owner, self.repo
            ),
        })
    }
}

impl HostedGitHost {
    fn from_domain(domain: &str) -> Option<Self> {
        match domain {
            "github.com" => Some(HostedGitHost::GitHub),
            "gitlab.com" => Some(HostedGitHost::GitLab),
            "bitbucket.org" => Some(HostedGitHost::Bitbucket),
            _ => None,
        }
    }

    pub fn host_domain(self) -> &'static str {
        match self {
            HostedGitHost::GitHub => "github.com",
            HostedGitHost::GitLab => "gitlab.com",
            HostedGitHost::Bitbucket => "bitbucket.org",
        }
    }
}

/// Parse a clone URL — in any form `parse_git_spec` accepts as input
/// or produces as output — into its `(host, owner, repo)` components,
/// when the host is one of the three providers npm / pnpm route
/// through HTTPS tarballs. Returns `None` for any other host (including
/// self-hosted GitLab / Gitea / Bitbucket Data Center): those still
/// need a real `git clone` because no codeload-style HTTP archive is
/// available.
///
/// Accepts:
/// - `https://github.com/owner/repo[.git]`
/// - `git+https://github.com/owner/repo[.git]`
/// - `git://github.com/owner/repo[.git]`
/// - `ssh://git@github.com/owner/repo[.git]`
/// - `git+ssh://git@github.com/owner/repo[.git]` (npm canonical lockfile form)
/// - `git@github.com:owner/repo[.git]` (scp shorthand, in case a caller
///   parses raw lockfile fields without going through `parse_git_spec`)
pub fn parse_hosted_git(url: &str) -> Option<HostedGit> {
    let body = url.strip_prefix("git+").unwrap_or(url);
    let after_scheme = if let Some(rest) = body.strip_prefix("https://") {
        rest
    } else if let Some(rest) = body.strip_prefix("http://") {
        rest
    } else if let Some(rest) = body.strip_prefix("ssh://") {
        rest
    } else if let Some(rest) = body.strip_prefix("git://") {
        rest
    } else {
        // scp shorthand `user@host:path` — not produced by parse_git_spec
        // but accepted defensively in case a raw lockfile string ever
        // bypasses it.
        let scp_path = parse_scp_url(body)?;
        return parse_hosted_git(&scp_path);
    };
    // Strip optional `user@` (always `git@` for hosted forms).
    let host_and_path = match after_scheme.split_once('@') {
        Some((_, rest)) => rest,
        None => after_scheme,
    };
    let (host, path) = host_and_path.split_once('/')?;
    let host = HostedGitHost::from_domain(host)?;
    // Take exactly two path segments: owner and repo. Anything beyond
    // (subgroup-style GitLab paths) doesn't have a stable HTTPS tarball
    // form on the three providers we care about, so refuse and let the
    // caller fall back to clone.
    let mut segs = path.splitn(3, '/');
    let owner = segs.next()?;
    let repo = segs.next()?;
    if owner.is_empty() || repo.is_empty() || segs.next().is_some() {
        return None;
    }
    let repo = repo
        .strip_suffix(".git")
        .unwrap_or(repo)
        .trim_end_matches('/');
    if repo.is_empty() {
        return None;
    }
    Some(HostedGit {
        host,
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn parse_scp_url(body: &str) -> Option<String> {
    if body.contains("://") {
        return None;
    }
    let colon = body.find(':')?;
    let before = &body[..colon];
    let path = &body[colon + 1..];
    if before.is_empty() || path.is_empty() {
        return None;
    }
    if path.starts_with('/') {
        return None;
    }
    let at = before.find('@')?;
    let user = &before[..at];
    let host = &before[at + 1..];
    if user.is_empty() || host.is_empty() || host.contains('/') || host.contains('@') {
        return None;
    }
    // pnpm 11 only resolves SCP-form as hosted Git for the three known
    // providers; other hosts (e.g. `git@example.com:foo/bar.git`) are
    // treated as local paths, and `host:path` without a user errors.
    if !matches!(host, "github.com" | "gitlab.com" | "bitbucket.org") {
        return None;
    }
    Some(format!("ssh://{user}@{host}/{path}"))
}

/// Normalize git URL fragments used by npm-compatible lockfiles.
///
/// Plain git accepts `#<ref>`, while npm and Yarn Berry also write
/// key/value fragments such as `#commit=<sha>` for pinned git deps.
/// Downstream code passes this value directly to `git ls-remote` and
/// `git checkout`, so strip the selector key here and keep only the
/// actual ref name or SHA.
pub(crate) fn normalize_git_fragment(fragment: &str) -> Option<String> {
    parse_git_fragment(fragment).0
}

/// Parse a git URL fragment into `(committish, subpath)`. Handles the
/// pnpm/hosted-git-info form `<ref>&path:/sub/dir` (the `path:` key
/// uses a colon, not `=`, by historical convention) as well as the
/// `key=value` form npm/Yarn Berry write. Unknown selectors are
/// ignored. Subpath is returned without leading slash so the caller
/// can join it with a clone dir without tripping the absolute-path
/// branch of `Path::join`.
pub(crate) fn parse_git_fragment(fragment: &str) -> (Option<String>, Option<String>) {
    if fragment.is_empty() {
        return (None, None);
    }

    let mut fallback: Option<&str> = None;
    let mut preferred: Option<&str> = None;
    let mut subpath: Option<String> = None;
    for part in fragment.split('&') {
        if part.is_empty() {
            continue;
        }
        // Try `key=value` first; fall back to `key:value` only for
        // the small set of selectors we actually handle below. A tag
        // name with a colon (e.g. `release:2026-01`) is left alone —
        // and `semver:^1.0.0` stays as a literal ref so `ls-remote`
        // surfaces an explicit error rather than silently HEAD-ing.
        let split = part.split_once('=').or_else(|| {
            part.split_once(':')
                .filter(|(k, _)| matches!(*k, "commit" | "tag" | "head" | "branch" | "path"))
        });
        let (key, value) = split.unwrap_or(("", part));
        if value.is_empty() {
            continue;
        }
        match key {
            "commit" => {
                preferred.get_or_insert(value);
            }
            "tag" | "head" | "branch" => {
                fallback.get_or_insert(value);
            }
            "path" => {
                // Strip leading slashes (pnpm writes `path:/sub`) and
                // reject any `..` / `.` component. Without this, a
                // crafted spec like `&path:/../../etc` would let the
                // resolver and installer escape the clone dir and
                // import an arbitrary host directory into the store.
                if subpath.is_some() {
                    // First-wins, matching the other selectors above.
                    continue;
                }
                let trimmed = value.trim_start_matches('/');
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
                {
                    continue;
                }
                subpath = Some(trimmed.to_string());
            }
            "" => {
                fallback.get_or_insert(value);
            }
            _ => {}
        }
    }

    (preferred.or(fallback).map(ToString::to_string), subpath)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_https_tgz() {
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://example.com/pkg-1.0.0.tgz"
        ));
    }

    #[test]
    fn matches_http_tar_gz() {
        assert!(LocalSource::looks_like_remote_tarball_url(
            "http://example.com/pkg-1.0.0.tar.gz"
        ));
    }

    #[test]
    fn strips_fragment_before_suffix_check() {
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://example.com/pkg-1.0.0.tgz#sha512-abc"
        ));
    }

    #[test]
    fn strips_query_string_before_suffix_check() {
        // Auth-token URLs from private registries (JFrog, Nexus,
        // CodeArtifact, …) routinely trail `?token=…` after the
        // filename. Must still classify as a tarball URL.
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://registry.example.com/pkg/-/pkg-1.0.0.tgz?token=abc"
        ));
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://example.com/pkg-1.0.0.tar.gz?v=2&signed=1"
        ));
    }

    #[test]
    fn matches_bare_http_url_without_tarball_suffix() {
        // pkg.pr.new serves tarballs from URLs without a `.tgz`
        // extension; npm treats all non-git http(s) URLs as tarball
        // URLs, so these must classify as remote tarballs.
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://pkg.pr.new/lunariajs/lunaria/@lunariajs/core@904b935"
        ));
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://codeload.github.com/user/repo/tar.gz/main"
        ));
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(!LocalSource::looks_like_remote_tarball_url(
            "ftp://example.com/pkg.tgz"
        ));
        assert!(!LocalSource::looks_like_remote_tarball_url(
            "git://example.com/repo.git"
        ));
    }

    #[test]
    fn parse_classifies_bare_http_url_as_remote_tarball() {
        use std::path::Path;
        let parsed = LocalSource::parse(
            "https://pkg.pr.new/lunariajs/lunaria/@lunariajs/core@904b935",
            Path::new(""),
        );
        assert!(matches!(parsed, Some(LocalSource::RemoteTarball(_))));
    }

    #[test]
    fn parse_prefers_git_over_tarball_for_dot_git_url() {
        use std::path::Path;
        let parsed = LocalSource::parse("https://github.com/user/repo.git", Path::new(""));
        assert!(matches!(parsed, Some(LocalSource::Git(_))));
    }

    #[test]
    fn parse_classifies_exec_as_local_source() {
        let parsed = LocalSource::parse("exec:./scripts/generate.js", Path::new(""));
        assert_eq!(
            parsed,
            Some(LocalSource::Exec(PathBuf::from("./scripts/generate.js")))
        );
    }

    #[test]
    fn git_plus_https_without_dot_git_roundtrips_via_lockfile_form() {
        // Initial parse: `git+https://…/repo` (no `.git`).
        let (url, committish, subpath) = parse_git_spec("git+https://host/user/repo").unwrap();
        assert_eq!(url, "https://host/user/repo");
        assert_eq!(committish, None);
        assert_eq!(subpath, None);

        // After resolving, the serializer writes `<url>#<sha>` into
        // the lockfile's importer `version:` field.
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let source = LocalSource::Git(GitSource {
            url: url.clone(),
            committish: None,
            resolved: sha.to_string(),
            integrity: None,
            subpath: None,
        });
        let lockfile_version = source.specifier();
        assert_eq!(lockfile_version, format!("https://host/user/repo#{sha}"));

        // Re-parse must recognize the bare URL because the 40-hex
        // committish suffix unambiguously tags it as git.
        let (round_url, round_committish, round_subpath) =
            parse_git_spec(&lockfile_version).unwrap();
        assert_eq!(round_url, "https://host/user/repo");
        assert_eq!(round_committish.as_deref(), Some(sha));
        assert_eq!(round_subpath, None);
    }

    #[test]
    fn bare_https_without_dot_git_and_no_committish_is_not_git() {
        // A plain `https://…` URL with no `.git` and no SHA could be
        // anything (including a tarball); don't claim it.
        assert!(parse_git_spec("https://example.com/pkg").is_none());
    }

    #[test]
    fn github_shorthand_expands_and_roundtrips() {
        let (url, _, _) = parse_git_spec("github:user/repo").unwrap();
        assert_eq!(url, "https://github.com/user/repo.git");
    }

    #[test]
    fn bare_user_repo_expands_to_github() {
        let (url, committish, subpath) = parse_git_spec("kevva/is-negative").unwrap();
        assert_eq!(url, "https://github.com/kevva/is-negative.git");
        assert!(committish.is_none());
        assert!(subpath.is_none());
    }

    #[test]
    fn bare_user_repo_with_committish_preserved() {
        let (url, committish, _) = parse_git_spec("kevva/is-negative#v1.0.0").unwrap();
        assert_eq!(url, "https://github.com/kevva/is-negative.git");
        assert_eq!(committish.as_deref(), Some("v1.0.0"));
    }

    #[test]
    fn bare_scope_pkg_is_not_git_shorthand() {
        // npm-style `@scope/pkg` is a registry name, not a GitHub shorthand.
        assert!(parse_git_spec("@types/node").is_none());
    }

    #[test]
    fn bare_relative_path_is_not_git_shorthand() {
        // Single-component relative paths split as owner=".", owner="..",
        // so owner-starts-with-`.` is the load-bearing guard here.
        assert!(parse_git_spec("./repo").is_none());
        assert!(parse_git_spec("../repo").is_none());
        // Multi-component relative paths additionally fail the
        // single-`/`-only guard.
        assert!(parse_git_spec("./local/path").is_none());
        assert!(parse_git_spec("../local/path").is_none());
    }

    #[test]
    fn bare_path_with_extra_slashes_is_not_git_shorthand() {
        // Real GitHub shorthand is exactly `user/repo` — anything with a
        // second `/` is a path, not a shorthand.
        assert!(parse_git_spec("path/with/slashes/extra").is_none());
    }

    #[test]
    fn bare_scp_form_unknown_host_is_not_github_shorthand() {
        // `user@host:repo.git` is scp form (handled or rejected above);
        // the bare-shorthand branch must not pick it up.
        assert!(parse_git_spec("user@host:repo.git").is_none());
    }

    #[test]
    fn scp_form_recognized() {
        let (url, committish, _) =
            parse_git_spec("git@github.com:EthanHenrickson/math-mcp.git").unwrap();
        assert_eq!(url, "ssh://git@github.com/EthanHenrickson/math-mcp.git");
        assert!(committish.is_none());
    }

    #[test]
    fn scp_form_with_ref_recognized() {
        let (url, committish, _) =
            parse_git_spec("git@github.com:EthanHenrickson/math-mcp.git#0.1.5").unwrap();
        assert_eq!(url, "ssh://git@github.com/EthanHenrickson/math-mcp.git");
        assert_eq!(committish.as_deref(), Some("0.1.5"));
    }

    #[test]
    fn scp_form_bitbucket_recognized() {
        let (url, _, _) = parse_git_spec("git@bitbucket.org:pnpmjs/git-resolver.git").unwrap();
        assert_eq!(url, "ssh://git@bitbucket.org/pnpmjs/git-resolver.git");
    }

    #[test]
    fn scp_form_unknown_host_rejected() {
        // pnpm 11 treats `user@unknown-host:path` as a local path, not Git.
        assert!(parse_git_spec("git@example.com:org/repo.git").is_none());
        assert!(parse_git_spec("alice@host.example.com:org/repo.git").is_none());
    }

    #[test]
    fn scp_form_without_user_rejected() {
        // pnpm 11 errors on bare `host:path` as unsupported.
        assert!(parse_git_spec("github.com:user/repo.git").is_none());
    }

    #[test]
    fn commit_selector_fragment_normalizes_to_sha() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let (url, committish, _) =
            parse_git_spec(&format!("https://host/user/repo.git#commit={sha}")).unwrap();
        assert_eq!(url, "https://host/user/repo.git");
        assert_eq!(committish.as_deref(), Some(sha));
    }

    #[test]
    fn named_selector_fragment_normalizes_to_ref() {
        let (url, committish, _) = parse_git_spec("git+https://host/user/repo#tag=v1.2.3").unwrap();
        assert_eq!(url, "https://host/user/repo");
        assert_eq!(committish.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn pnpm_path_subpath_extracted_from_fragment() {
        // pnpm syntax: `<url>#<ref>&path:/<subdir>` selects a
        // subdirectory of the cloned repo as the package root.
        let (url, committish, subpath) =
            parse_git_spec("github:org/dep#v0.1.4&path:/packages/special").unwrap();
        assert_eq!(url, "https://github.com/org/dep.git");
        assert_eq!(committish.as_deref(), Some("v0.1.4"));
        assert_eq!(subpath.as_deref(), Some("packages/special"));
    }

    #[test]
    fn path_subpath_roundtrips_via_specifier() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let source = LocalSource::Git(GitSource {
            url: "https://github.com/org/dep.git".to_string(),
            committish: None,
            resolved: sha.to_string(),
            integrity: None,
            subpath: Some("packages/special".to_string()),
        });
        let spec = source.specifier();
        assert_eq!(
            spec,
            format!("https://github.com/org/dep.git#{sha}&path:/packages/special")
        );
        let (url, committish, subpath) = parse_git_spec(&spec).unwrap();
        assert_eq!(url, "https://github.com/org/dep.git");
        assert_eq!(committish.as_deref(), Some(sha));
        assert_eq!(subpath.as_deref(), Some("packages/special"));
    }

    #[test]
    fn parse_hosted_git_recognizes_canonical_forms() {
        // All these point at the same (github.com, owner, repo) tuple
        // and must map to the same HostedGit so the runtime fetch URL
        // doesn't depend on which scheme the lockfile happens to record.
        let canonical = HostedGit {
            host: HostedGitHost::GitHub,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
        };
        for spec in [
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo",
            "http://github.com/owner/repo.git",
            "git+https://github.com/owner/repo.git",
            "git+https://github.com/owner/repo",
            "git://github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
            "git+ssh://git@github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
        ] {
            assert_eq!(
                parse_hosted_git(spec).as_ref(),
                Some(&canonical),
                "spec {spec} should map to canonical HostedGit",
            );
        }
    }

    #[test]
    fn parse_hosted_git_returns_none_for_non_hosted() {
        // Self-hosted GitLab / Gitea / arbitrary hosts: no codeload
        // template, so the codeload fast path doesn't apply.
        for spec in [
            "https://example.com/owner/repo.git",
            "ssh://git@gitea.internal/owner/repo.git",
            "git+ssh://git@gitlab.example.com/group/sub/repo.git",
            "https://github.com/owner/repo/sub",
            "https://github.com/owner",
        ] {
            assert!(
                parse_hosted_git(spec).is_none(),
                "spec {spec} must not match a hosted provider",
            );
        }
    }

    #[test]
    fn hosted_tarball_url_only_for_full_sha() {
        let g = HostedGit {
            host: HostedGitHost::GitHub,
            owner: "o".to_string(),
            repo: "r".to_string(),
        };
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        assert_eq!(
            g.tarball_url(sha).as_deref(),
            Some("https://codeload.github.com/o/r/tar.gz/abcdef0123456789abcdef0123456789abcdef01"),
        );
        // Branch / tag / abbreviated SHA don't take the fast path —
        // codeload accepts them but the wrapper-dir name varies and
        // we can't verify a non-SHA committish post-extraction.
        assert!(g.tarball_url("main").is_none());
        assert!(g.tarball_url("v1.2.3").is_none());
        assert!(g.tarball_url("abcdef0").is_none());
    }

    #[test]
    fn hosted_tarball_url_per_provider() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let gitlab = HostedGit {
            host: HostedGitHost::GitLab,
            owner: "g".to_string(),
            repo: "r".to_string(),
        }
        .tarball_url(sha)
        .unwrap();
        assert!(gitlab.starts_with("https://gitlab.com/g/r/-/archive/"));
        assert!(gitlab.ends_with("/r-abcdef0123456789abcdef0123456789abcdef01.tar.gz"));
        let bitbucket = HostedGit {
            host: HostedGitHost::Bitbucket,
            owner: "g".to_string(),
            repo: "r".to_string(),
        }
        .tarball_url(sha)
        .unwrap();
        assert_eq!(
            bitbucket,
            "https://bitbucket.org/g/r/get/abcdef0123456789abcdef0123456789abcdef01.tar.gz",
        );
    }

    #[test]
    fn hosted_https_url_normalizes() {
        let g = parse_hosted_git("git+ssh://git@github.com/owner/repo.git").unwrap();
        assert_eq!(g.https_url(), "https://github.com/owner/repo.git");
    }

    #[test]
    fn path_traversal_components_in_subpath_are_rejected() {
        // `..` and `.` components would let a crafted spec escape the
        // clone dir at install time. The parser drops them so the
        // resolver/installer never see a traversal-laden subpath.
        let cases = [
            "github:org/dep#main&path:/../../etc",
            "github:org/dep#main&path:/packages/../../../etc",
            "github:org/dep#main&path:/./packages/foo",
            "github:org/dep#main&path:/packages//foo",
        ];
        for spec in cases {
            let (_, _, subpath) = parse_git_spec(spec).unwrap();
            assert_eq!(subpath, None, "spec should drop subpath: {spec}");
        }
    }

    #[test]
    fn dep_path_distinguishes_subpaths_under_same_commit() {
        // Two packages from the same repo+commit but different
        // subdirs must hash to distinct dep_paths so the linker
        // doesn't collapse them.
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let a = LocalSource::Git(GitSource {
            url: "https://example.com/r.git".to_string(),
            committish: None,
            resolved: sha.to_string(),
            integrity: None,
            subpath: Some("packages/a".to_string()),
        });
        let b = LocalSource::Git(GitSource {
            url: "https://example.com/r.git".to_string(),
            committish: None,
            resolved: sha.to_string(),
            integrity: None,
            subpath: Some("packages/b".to_string()),
        });
        assert_ne!(a.dep_path("dep"), b.dep_path("dep"));
    }
}
