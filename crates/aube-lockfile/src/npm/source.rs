use crate::{GitSource, LocalSource, LockedPackage};
use std::path::PathBuf;

pub(super) fn local_git_source_from_resolved(resolved: &str) -> Option<LocalSource> {
    let (url, committish, subpath) = crate::parse_git_spec(resolved)?;
    let resolved = committish.clone()?;
    Some(LocalSource::Git(GitSource {
        url,
        committish,
        resolved,
        integrity: None,
        subpath,
    }))
}

/// Convert a `file:<path>` value in a non-`link:true` entry's
/// `resolved` field to the matching local source. npm writes this
/// shape for `npm install file:../foo-1.0.0.tgz` (local tarballs)
/// and for some directory deps that pre-date the modern `link: true`
/// emission. Without recognizing it, the entry parses as a plain
/// registry package; lockfile-reuse then matches by name+version and
/// the fetcher 404s on the literal package name.
///
/// Tarball vs. Directory is decided purely by the `.tgz`/`.tar.gz`
/// suffix: the lockfile path is authoritative, and we don't have the
/// project root here to stat the target. False classification is
/// recoverable on the next install — `LocalSource::parse` from the
/// manifest re-runs the FS-aware check.
pub(super) fn local_file_source_from_resolved(resolved: &str) -> Option<LocalSource> {
    let rest = resolved.strip_prefix("file:")?;
    let path = PathBuf::from(rest);
    if LocalSource::path_looks_like_tarball(&path) {
        Some(LocalSource::Tarball(path))
    } else {
        Some(LocalSource::Directory(path))
    }
}

pub(super) fn npm_resolved_field(pkg: &LockedPackage) -> Option<String> {
    pkg.tarball_url.clone().or_else(|| match &pkg.local_source {
        Some(LocalSource::Git(git)) => {
            let url = if git.url.starts_with("git://") || git.url.starts_with("git+") {
                git.url.clone()
            } else {
                format!("git+{}", git.url)
            };
            match &git.subpath {
                Some(subpath) => Some(format!("{url}#{}&path:/{subpath}", git.resolved)),
                None => Some(format!("{url}#{}", git.resolved)),
            }
        }
        _ => None,
    })
}
