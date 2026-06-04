use aube_manifest::BundledDependencies;
use aube_registry::{Attestations, Dist, NpmUser, Packument, PeerDepMeta, VersionMetadata};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

#[path = "primer_schema.rs"]
mod primer_schema;

pub(crate) use primer_schema::Seed;
use primer_schema::{
    PrimerBundledDependencies, PrimerDist, PrimerPackument, PrimerPeerDepMeta,
    PrimerVersionMetadata,
};

const PRIMER_FORMAT: &str = "rkyv-v1";
const PRUNE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const AUTO_PRUNE_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
const AUTO_PRUNE_DENOMINATOR: u8 = 100;

include!(concat!(env!("OUT_DIR"), "/primer_index.rs"));

#[derive(Default)]
pub struct PruneStats {
    pub files: u64,
    pub bytes: u64,
}

impl Seed {
    pub(crate) fn packument(&self) -> Packument {
        self.packument.to_packument()
    }
}

impl PrimerPackument {
    fn to_packument(&self) -> Packument {
        let mut time = BTreeMap::new();
        let versions = self
            .versions
            .iter()
            .map(|v| {
                if let Some(published_at) = v.published_at.as_ref() {
                    time.insert(v.version.clone(), published_at.clone());
                }
                (
                    v.version.clone(),
                    v.metadata.to_version_metadata(&self.name, &v.version),
                )
            })
            .collect();
        Packument {
            name: self.name.clone(),
            modified: self.modified.clone(),
            versions,
            dist_tags: self.dist_tags.clone(),
            time,
        }
    }
}

impl PrimerVersionMetadata {
    fn to_version_metadata(&self, name: &str, version: &str) -> VersionMetadata {
        VersionMetadata {
            name: name.to_owned(),
            version: version.to_owned(),
            dependencies: self.dependencies.clone(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: self.peer_dependencies.clone(),
            peer_dependencies_meta: self
                .peer_dependencies_meta
                .iter()
                .map(|(name, meta)| (name.clone(), meta.to_peer_dep_meta()))
                .collect(),
            optional_dependencies: self.optional_dependencies.clone(),
            bundled_dependencies: self
                .bundled_dependencies
                .as_ref()
                .map(PrimerBundledDependencies::to_bundled_dependencies),
            dist: self.dist.as_ref().map(|d| d.to_dist(name, version)),
            os: self.os.clone(),
            cpu: self.cpu.clone(),
            libc: self.libc.clone(),
            engines: self.engines.clone(),
            license: self.license.clone(),
            funding_url: self.funding_url.clone(),
            bin: self.bin.clone(),
            has_install_script: self.has_install_script,
            deprecated: self.deprecated.clone(),
            approver: None,
            npm_user: self.trusted_publisher.then(|| NpmUser {
                trusted_publisher: Some(serde_json::json!({"id": "npm-primer"})),
            }),
        }
    }
}

impl PrimerPeerDepMeta {
    fn to_peer_dep_meta(&self) -> PeerDepMeta {
        PeerDepMeta {
            optional: self.optional,
        }
    }
}

impl PrimerBundledDependencies {
    fn to_bundled_dependencies(&self) -> BundledDependencies {
        match self {
            Self::List(v) => BundledDependencies::List(v.clone()),
            Self::All(v) => BundledDependencies::All(*v),
        }
    }
}

impl PrimerDist {
    fn to_dist(&self, name: &str, version: &str) -> Dist {
        Dist {
            tarball: self
                .tarball
                .clone()
                .unwrap_or_else(|| deterministic_tarball_url(name, version)),
            integrity: self.integrity.clone(),
            shasum: None,
            unpacked_size: None,
            attestations: self.provenance.then(|| Attestations {
                provenance: Some(serde_json::json!({
                    "predicateType": "https://slsa.dev/provenance/v1"
                })),
            }),
        }
    }
}

/// Reconstruct the npmjs tarball URL when the primer omitted it
/// (the common case — see PrimerDist::tarball docs). Mirrors
/// `RegistryClient::tarball_url`'s format for `registry.npmjs.org`.
/// In force-metadata-primer mode the URL is rewritten to the active
/// registry by the resolver, so this default is only consulted on
/// the default-registry path.
fn deterministic_tarball_url(name: &str, version: &str) -> String {
    let unscoped = name
        .strip_prefix('@')
        .and_then(|rest| rest.split('/').nth(1))
        .unwrap_or(name);
    format!("https://registry.npmjs.org/{name}/-/{unscoped}-{version}.tgz")
}

static GENERATED_AT: OnceLock<Option<String>> = OnceLock::new();
static AUTO_PRUNED: OnceLock<()> = OnceLock::new();

pub(crate) fn get(name: &str) -> Option<Seed> {
    let (_, offset, len) = PRIMER_INDEX
        .binary_search_by(|(candidate, _, _)| candidate.cmp(&name))
        .ok()
        .and_then(|idx| PRIMER_INDEX.get(idx))?;
    auto_prune_once();
    let end = offset.checked_add(*len)?;
    let compressed = PRIMER_BLOB.get(*offset..end)?;
    let archived = zstd::stream::decode_all(Cursor::new(compressed)).ok()?;
    rkyv::from_bytes::<Seed, rkyv::rancor::Error>(&archived).ok()
}

pub(crate) fn covers_cutoff(cutoff: &str) -> bool {
    generated_at().is_some_and(|generated_at| generated_at.as_str() >= cutoff)
}

fn generated_at() -> Option<&'static String> {
    GENERATED_AT
        .get_or_init(|| {
            let secs = option_env!("AUBE_PRIMER_GENERATED_AT")?.parse().ok()?;
            Some(crate::types::format_iso8601_utc(secs))
        })
        .as_ref()
}

fn auto_prune_once() {
    AUTO_PRUNED.get_or_init(|| {
        if let Some(dir) = primer_cache_dir() {
            auto_prune(&dir);
        }
    });
}

fn auto_prune(dir: &Path) {
    if !random_byte().is_multiple_of(AUTO_PRUNE_DENOMINATOR) {
        return;
    }
    if let Err(e) = prune_old(dir, PRUNE_AGE, false, Some(AUTO_PRUNE_COOLDOWN)) {
        tracing::debug!("failed to prune old primer cache files: {e}");
    }
}

pub fn prune_cache(dry_run: bool, age: Duration) -> std::io::Result<PruneStats> {
    let Some(dir) = primer_cache_dir() else {
        return Ok(PruneStats::default());
    };
    prune_old(&dir, age, dry_run, None)
}

fn prune_old(
    dir: &Path,
    age: Duration,
    dry_run: bool,
    sentinel_cooldown: Option<Duration>,
) -> std::io::Result<PruneStats> {
    let mut stats = PruneStats::default();
    std::fs::create_dir_all(dir)?;
    let sentinel = dir.join(".auto_prune");
    if let Some(cooldown) = sentinel_cooldown
        && let Ok(modified) = sentinel.metadata().and_then(|m| m.modified())
        && modified.elapsed().unwrap_or_default() < cooldown
    {
        return Ok(stats);
    }
    if sentinel_cooldown.is_some() {
        touch(&sentinel)?;
    }
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_primer_cache_file(name) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.modified()?.elapsed().unwrap_or_default() > age {
            stats.files += 1;
            stats.bytes += metadata.len();
            if !dry_run {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(stats)
}

fn touch(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(b"\n")
}

fn is_primer_cache_file(name: &str) -> bool {
    name.starts_with(&format!("{PRIMER_FORMAT}-")) && name.ends_with(".rkyv")
}

fn random_byte() -> u8 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    (nanos as u8) ^ (std::process::id() as u8)
}

fn primer_cache_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("AUBE_CACHE_DIR") {
        return Some(PathBuf::from(base).join("primer"));
    }
    cache_base_dir().map(|p| p.join("aube").join("primer"))
}

#[cfg(unix)]
fn cache_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
}

#[cfg(windows)]
fn cache_base_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_primer_loads() {
        let Some((name, _, _)) = PRIMER_INDEX.first() else {
            return;
        };
        assert!(super::get(name).is_some());
    }

    #[test]
    fn bundled_primer_synthesizes_tarball_urls() {
        // The generator omits the tarball URL when it matches the
        // deterministic `{registry}/{name}/-/{unscoped}-{version}.tgz`
        // pattern. Verify the runtime fills it in correctly: every
        // dist must surface a tarball URL whose path segments match
        // the package name + version we asked for, so a synthesis bug
        // that drops or swaps either field can't pass silently.
        let Some((name, _, _)) = PRIMER_INDEX.first() else {
            return;
        };
        let packument = super::get(name).expect("primer hit").packument();
        let (version, meta) = packument
            .versions
            .iter()
            .find(|(_, v)| v.dist.is_some())
            .expect("packument has at least one version with dist metadata");
        let dist = meta.dist.as_ref().unwrap();
        assert!(
            dist.tarball.starts_with("https://"),
            "tarball: {}",
            dist.tarball
        );
        assert!(dist.tarball.ends_with(".tgz"), "tarball: {}", dist.tarball);
        assert!(
            dist.tarball.contains(*name),
            "tarball {} missing package name {name}",
            dist.tarball,
        );
        assert!(
            dist.tarball.contains(version),
            "tarball {} missing version {version}",
            dist.tarball,
        );
    }

    #[test]
    fn deterministic_tarball_url_handles_scoped_names() {
        assert_eq!(
            deterministic_tarball_url("react", "18.2.0"),
            "https://registry.npmjs.org/react/-/react-18.2.0.tgz"
        );
        assert_eq!(
            deterministic_tarball_url("@types/node", "20.10.0"),
            "https://registry.npmjs.org/@types/node/-/node-20.10.0.tgz"
        );
    }

    #[test]
    fn primer_cache_file_match_is_narrow() {
        assert!(is_primer_cache_file("rkyv-v1-abc.rkyv"));
        assert!(!is_primer_cache_file(".auto_prune"));
        assert!(!is_primer_cache_file("rkyv-v1-abc.tmp"));
        assert!(!is_primer_cache_file("other-v1-abc.rkyv"));
    }

    #[test]
    fn prune_removes_old_extracted_primer_files() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("rkyv-v1-old-0-old.rkyv"), "{}").unwrap();
        std::fs::write(dir.join("packument.json"), "{}").unwrap();
        let stats = prune_old(dir, Duration::from_secs(0), false, None).unwrap();
        assert_eq!(stats.files, 1);
        assert!(!dir.join("rkyv-v1-old-0-old.rkyv").exists());
        assert!(dir.join("packument.json").exists());
    }

    #[test]
    fn prune_sentinel_uses_own_cooldown() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let primer_file = dir.join("rkyv-v1-old-0-old.rkyv");
        std::fs::write(&primer_file, "{}").unwrap();
        touch(&dir.join(".auto_prune")).unwrap();

        let stats = prune_old(
            dir,
            Duration::from_secs(0),
            false,
            Some(Duration::from_secs(60)),
        )
        .unwrap();

        assert_eq!(stats.files, 0);
        assert!(primer_file.exists());
    }
}
