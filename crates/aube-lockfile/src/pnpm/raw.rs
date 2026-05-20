use crate::{GitSource, LocalSource, RemoteTarballSource};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Parse `pnpm-lock.yaml` content, tolerating pnpm v11's multi-document
/// layout.
///
/// pnpm v11 splits the lockfile into two YAML documents: a bootstrap
/// document that tracks pnpm's own `packageManagerDependencies` /
/// `configDependencies`, and the "real" project lockfile (with the
/// workspace's `dependencies` / `devDependencies`, `settings`,
/// `catalogs`, `overrides`, `patchedDependencies`, etc.). We want the
/// second one. Heuristic: score every parseable document by
/// project-lockfile signal (real importer deps + settings/catalogs/
/// overrides + packages/snapshots count) and take the highest. If only
/// one document is present (pnpm v9/v10 and older) this reduces to the
/// previous single-document parse.
pub(super) fn parse_raw_lockfile(content: &str) -> Result<RawPnpmLockfile, yaml_serde::Error> {
    // Hard cap on documents inspected. pnpm v11 emits exactly two;
    // anything beyond a handful is pathological. This also guards
    // against malformed YAML that puts
    // `yaml_serde::Deserializer::from_str`'s iterator into an
    // infinite-yield state — `test_parse_invalid_yaml` tripped that
    // mode on Windows CI with an unbounded loop.
    const MAX_DOCUMENTS: usize = 16;

    let mut best: Option<(u64, RawPnpmLockfile)> = None;
    let mut first_err: Option<yaml_serde::Error> = None;
    for (idx, doc) in yaml_serde::Deserializer::from_str(content)
        .enumerate()
        .take(MAX_DOCUMENTS)
    {
        match RawPnpmLockfile::deserialize(doc) {
            Ok(raw) => {
                let score = project_lockfile_score(&raw);
                best = match best {
                    Some((prev, _)) if prev >= score => best,
                    _ => Some((score, raw)),
                };
            }
            Err(e) => {
                // Log the first per-document failure and stop. A malformed document
                // typically puts yaml_serde's iterator into a state
                // where further iteration is either more garbage or an
                // infinite loop (see `test_parse_invalid_yaml`). The
                // returned error is the first failure, which is both
                // most explanatory and the only one we actually
                // observed.
                tracing::debug!("pnpm-lock.yaml document {idx} failed to parse: {e}");
                first_err = Some(e);
                break;
            }
        }
    }
    match (best, first_err) {
        (Some((_, raw)), _) => Ok(raw),
        (None, Some(e)) => Err(e),
        // No documents at all — defer to the single-doc parser so the
        // error surface matches what callers saw before.
        (None, None) => yaml_serde::from_str(content),
    }
}

/// Score for picking the "main" document out of a multi-document
/// `pnpm-lock.yaml`. Weighted so a document with real importer
/// dependencies beats one with only `packageManagerDependencies`
/// (pnpm v11's bootstrap doc has the latter but no regular deps).
pub(super) fn project_lockfile_score(raw: &RawPnpmLockfile) -> u64 {
    let importer_dep_count: usize = raw
        .importers
        .values()
        .map(|i| {
            i.dependencies.as_ref().map(|m| m.len()).unwrap_or(0)
                + i.dev_dependencies.as_ref().map(|m| m.len()).unwrap_or(0)
                + i.optional_dependencies
                    .as_ref()
                    .map(|m| m.len())
                    .unwrap_or(0)
        })
        .sum();
    let mut score = importer_dep_count as u64 * 1000;
    if raw.settings.is_some() {
        score += 100;
    }
    if raw.catalogs.as_ref().is_some_and(|c| !c.is_empty()) {
        score += 100;
    }
    if raw.overrides.as_ref().is_some_and(|o| !o.is_empty()) {
        score += 100;
    }
    score += raw.packages.len() as u64;
    score += raw.snapshots.len() as u64;
    score
}

// -- Raw serde types for pnpm-lock.yaml v9 (deserialization) --

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawPnpmLockfile {
    #[allow(dead_code)]
    pub(super) lockfile_version: yaml_serde::Value,
    #[serde(default)]
    pub(super) settings: Option<RawSettings>,
    #[serde(default)]
    pub(super) overrides: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub(super) catalogs: Option<BTreeMap<String, BTreeMap<String, RawCatalogEntry>>>,
    /// pnpm v9+ top-level `patchedDependencies:` block. Map of
    /// `pkg@version` selector → patch entry (pnpm uses a nested
    /// `{ path, hash }` object, but we only model the path string
    /// on the shared graph). Round-tripped verbatim so a parse/
    /// write cycle doesn't drop user patches.
    #[serde(default)]
    pub(super) patched_dependencies: Option<BTreeMap<String, RawPatchedDependency>>,
    #[serde(default)]
    pub(super) ignored_optional_dependencies: Option<Vec<String>>,
    #[serde(default)]
    pub(super) importers: BTreeMap<String, RawImporter>,
    #[serde(default)]
    pub(super) packages: BTreeMap<String, RawPackageInfo>,
    #[serde(default)]
    pub(super) snapshots: BTreeMap<String, RawSnapshot>,
    #[serde(default)]
    pub(super) time: Option<BTreeMap<String, String>>,
}

/// pnpm writes `patchedDependencies` as either a bare path string
/// (v8 style) or a nested `{ path, hash }` object (v9+). We accept
/// both via an untagged enum and collapse to the path string on the
/// shared graph.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum RawPatchedDependency {
    Path(String),
    Object {
        path: String,
        #[serde(default)]
        #[allow(dead_code)]
        hash: Option<String>,
    },
}

impl RawPatchedDependency {
    pub(super) fn into_path(self) -> String {
        match self {
            RawPatchedDependency::Path(p) => p,
            RawPatchedDependency::Object { path, .. } => path,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawSettings {
    #[serde(default)]
    pub(super) auto_install_peers: Option<bool>,
    #[serde(default)]
    pub(super) exclude_links_from_lockfile: Option<bool>,
    #[serde(default)]
    pub(super) lockfile_include_tarball_url: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawImporter {
    pub(super) dependencies: Option<BTreeMap<String, RawDepSpec>>,
    pub(super) dev_dependencies: Option<BTreeMap<String, RawDepSpec>>,
    pub(super) optional_dependencies: Option<BTreeMap<String, RawDepSpec>>,
    pub(super) skipped_optional_dependencies: Option<BTreeMap<String, RawDepSpec>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RawDepSpec {
    pub(super) specifier: String,
    pub(super) version: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct RawCatalogEntry {
    pub(super) specifier: String,
    pub(super) version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawPackageInfo {
    pub(super) resolution: Option<Resolution>,
    #[serde(default)]
    pub(super) engines: BTreeMap<String, String>,
    pub(super) peer_dependencies: Option<BTreeMap<String, String>>,
    pub(super) peer_dependencies_meta: Option<BTreeMap<String, RawPeerDepMeta>>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    pub(super) libc: Vec<String>,
    #[serde(default)]
    pub(super) has_bin: bool,
    /// Paired writer field. See `WritablePackageInfo::alias_of`. `None`
    /// for ordinary (non-aliased) packages.
    #[serde(default)]
    pub(super) alias_of: Option<String>,
    /// pnpm emits `version: <semver>` on `packages:` entries whose dep-path
    /// key is a URL (remote tarball, git) rather than a bare semver —
    /// that way the key stays unique (one URL, one entry) while the real
    /// semver is still recorded for tooling. None for ordinary registry
    /// entries, where the version lives in the dep-path key itself.
    #[serde(default)]
    pub(super) version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct RawPeerDepMeta {
    #[serde(default)]
    pub(super) optional: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct Resolution {
    pub(super) integrity: Option<String>,
    #[serde(default, rename = "gitHosted")]
    pub(super) git_hosted: bool,
    #[serde(default)]
    pub(super) directory: Option<String>,
    #[serde(default)]
    pub(super) tarball: Option<String>,
    #[serde(default)]
    pub(super) commit: Option<String>,
    #[serde(default)]
    pub(super) repo: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    pub(super) type_: Option<String>,
    /// pnpm `&path:/<sub>` selector for git deps. Newer pnpm
    /// (>= v9.x) emits this on the resolution block in addition to
    /// encoding it in the snapshot key.
    #[serde(default, deserialize_with = "deserialize_subpath")]
    pub(super) path: Option<String>,
}

/// Strip the leading `/` from pnpm's `path:` field so the value lines
/// up with how `parse_git_fragment` stores it. Mirror the same
/// `..`/`.`/empty-component guard as the in-URL parser so a crafted
/// lockfile cannot direct the resolver to read a `package.json`
/// outside the clone dir.
pub(super) fn deserialize_subpath<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = serde::Deserialize::deserialize(de)?;
    Ok(raw.and_then(|s| {
        let trimmed = s.trim_start_matches('/');
        if trimmed.is_empty()
            || trimmed
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            None
        } else {
            Some(trimmed.to_string())
        }
    }))
}

/// Convert a pnpm `resolution:` block into a `LocalSource` classification.
/// Returns `None` for registry-sourced packages (plain integrity with no
/// tarball/directory/repo fields). Shared by the direct-dep and
/// transitive-dep reader paths so both stay in lockstep when new
/// resolution shapes are added.
pub(super) fn local_source_from_resolution(res: &Resolution) -> Option<LocalSource> {
    if let Some(ref tb) = res.tarball {
        let _git_hosted = res.git_hosted;
        if let Some(rel) = tb.strip_prefix("file:") {
            return Some(LocalSource::Tarball(std::path::PathBuf::from(rel)));
        }
        if tb.starts_with("http://") || tb.starts_with("https://") {
            return Some(LocalSource::RemoteTarball(RemoteTarballSource {
                url: tb.clone(),
                integrity: res.integrity.clone().unwrap_or_default(),
            }));
        }
        return None;
    }
    if let Some(ref dir) = res.directory {
        return Some(LocalSource::Directory(std::path::PathBuf::from(dir)));
    }
    if let (Some(repo), Some(commit)) = (res.repo.as_ref(), res.commit.as_ref()) {
        return Some(LocalSource::Git(GitSource {
            url: repo.clone(),
            committish: None,
            resolved: commit.clone(),
            integrity: res.integrity.clone(),
            subpath: res.path.clone(),
        }));
    }
    None
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawSnapshot {
    #[serde(default)]
    pub(super) dependencies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub(super) optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub(super) bundled_dependencies: Option<Vec<String>>,
    #[serde(default)]
    pub(super) optional: Option<bool>,
    #[serde(default)]
    pub(super) transitive_peer_dependencies: Option<Vec<String>>,
}
