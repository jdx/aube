//! aube self-version management: discovering, downloading, and
//! installing *aube* binaries so a project's `packageManager` /
//! `devEngines.packageManager` pin can re-exec the right version
//! (corepack semantics; pnpm's `managePackageManagerVersions`).
//!
//! Sources, same shape as Node runtimes: mise installs
//! (`installs/aube/<v>/`, binaries at the version root) are reused
//! read-only, and self-downloads come from GitHub release archives
//! (`aube-v{V}-{target-triple}.tar.gz` / `.zip`, binaries at the
//! archive root) into `$XDG_DATA_HOME/aube/self/<v>/`.

use crate::discover::{self, InstallOrigin};
use crate::error::Error;
use crate::http::Http;
use crate::installer::stream_to_file;
use crate::mise;
use crate::progress::{DownloadProgress, InstallPhase, NoopProgress};
use crate::{InstallerMode, RuntimeConfig};
use std::path::{Path, PathBuf};

/// Default base for release archives. `AUBE_SELF_DOWNLOAD_BASE`
/// overrides for tests and mirrors; archives live at
/// `{base}/v{V}/aube-v{V}-{triple}.{ext}` with a sibling
/// `{archive}.sha256`.
const RELEASE_BASE: &str = "https://github.com/jdx/aube/releases/download";

/// Endpoint announcing the newest release (one line, bare version).
/// Shared with the update notifier. `AUBE_SELF_VERSION_URL` overrides.
const VERSION_URL: &str = "https://aube.jdx.dev/VERSION";

/// A validated on-disk aube install.
#[derive(Debug, Clone)]
pub struct InstalledAube {
    pub version: node_semver::Version,
    pub install_dir: PathBuf,
    /// The `aube` executable. `aubr` / `aubx` siblings live next to it.
    pub exe: PathBuf,
    pub origin: InstallOrigin,
}

/// aube's own versions dir (`$XDG_DATA_HOME/aube/self`).
/// `AUBE_SELF_DIR` overrides for tests.
pub fn self_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AUBE_SELF_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join("aube/self"));
    }
    let data_home = aube_util::env::xdg_data_home()
        .or_else(|| aube_util::env::home_dir().map(|h| h.join(".local/share")))?;
    Some(data_home.join("aube/self"))
}

/// Every valid installed aube across mise's installs dir and aube's
/// self dir. Same collision rule as Node: aube's own copy of a
/// version wins over mise's.
pub fn list_installed_aube() -> Vec<InstalledAube> {
    let mut by_version: std::collections::BTreeMap<node_semver::Version, InstalledAube> =
        Default::default();
    if let Some(dir) = discover::mise_tool_installs_dir("aube") {
        for install in scan_aube_dir(&dir, InstallOrigin::Mise) {
            by_version.insert(install.version.clone(), install);
        }
    }
    if let Some(dir) = self_dir() {
        for install in scan_aube_dir(&dir, InstallOrigin::Aube) {
            by_version.insert(install.version.clone(), install);
        }
    }
    by_version.into_values().collect()
}

/// Look up one exact installed version (mise first, then self dir —
/// the self-dir copy wins, mirroring `list_installed_aube`).
pub fn find_installed_aube(version: &node_semver::Version) -> Option<InstalledAube> {
    let from_self = self_dir()
        .map(|d| d.join(version.to_string()))
        .and_then(|d| validate_aube_install(&d, version.clone(), InstallOrigin::Aube));
    from_self.or_else(|| {
        discover::mise_tool_installs_dir("aube")
            .map(|d| d.join(version.to_string()))
            .and_then(|d| validate_aube_install(&d, version.clone(), InstallOrigin::Mise))
    })
}

fn scan_aube_dir(root: &Path, origin: InstallOrigin) -> Vec<InstalledAube> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            // Skips mise's alias symlinks (`1`, `1.18`, `latest`).
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(version) = node_semver::Version::parse(name.trim_start_matches('v')) else {
            continue;
        };
        if let Some(install) = validate_aube_install(&path, version, origin) {
            out.push(install);
        }
    }
    out
}

/// Validate a version dir: no `incomplete` marker (mise's in-progress
/// signal), and the `aube` executable present at the root or under
/// `bin/` (mise and the release archives use the root; `bin/` covers
/// alternative packagings).
fn validate_aube_install(
    dir: &Path,
    version: node_semver::Version,
    origin: InstallOrigin,
) -> Option<InstalledAube> {
    if dir.join("incomplete").exists() {
        return None;
    }
    let exe_name = if cfg!(windows) { "aube.exe" } else { "aube" };
    let exe = [dir.join(exe_name), dir.join("bin").join(exe_name)]
        .into_iter()
        .find(|p| p.is_file())?;
    Some(InstalledAube {
        version,
        install_dir: dir.to_path_buf(),
        exe,
        origin,
    })
}

/// The release-archive target triple for the host. aube publishes:
/// `aarch64-apple-darwin`, `{x86_64,aarch64}-unknown-linux-{gnu,musl}`,
/// `{x86_64,aarch64}-pc-windows-msvc`. Hosts without a published
/// build (e.g. Intel macOS) get an [`Error::UnsupportedPlatform`]
/// pointing at mise.
pub fn release_target_triple() -> Result<String, Error> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            return Err(Error::UnsupportedPlatform {
                platform: format!("{}-{other}", std::env::consts::OS),
            });
        }
    };
    let triple = match std::env::consts::OS {
        "macos" => {
            if arch != "aarch64" {
                return Err(Error::UnsupportedPlatform {
                    platform: "macos-x86_64 (no published aube build; install via mise)"
                        .to_string(),
                });
            }
            format!("{arch}-apple-darwin")
        }
        "linux" => {
            let libc = if crate::Platform::current()?.libc.as_deref() == Some("musl") {
                "musl"
            } else {
                "gnu"
            };
            format!("{arch}-unknown-linux-{libc}")
        }
        "windows" => format!("{arch}-pc-windows-msvc"),
        other => {
            return Err(Error::UnsupportedPlatform {
                platform: format!("{other}-{arch}"),
            });
        }
    };
    Ok(triple)
}

fn release_base() -> String {
    std::env::var("AUBE_SELF_DOWNLOAD_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| RELEASE_BASE.to_string())
}

/// Resolve the newest published aube version (for range pins).
pub async fn latest_aube_version(retries: u32) -> Result<node_semver::Version, Error> {
    let url = std::env::var("AUBE_SELF_VERSION_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| VERSION_URL.to_string());
    let http = Http::new(retries);
    let resp = http.get(&url, None, None, false).await?;
    let body = resp.body.ok_or_else(|| Error::DownloadFailed {
        url: url.clone(),
        reason: "unexpected empty response".to_string(),
    })?;
    let text = body.text().await.map_err(|e| Error::DownloadFailed {
        url: url.clone(),
        reason: e.to_string(),
    })?;
    node_semver::Version::parse(text.trim().trim_start_matches('v')).map_err(|e| {
        Error::DownloadFailed {
            url,
            reason: format!("unparseable version announcement: {e}"),
        }
    })
}

/// Install aube `version`, honoring the installer mode: mise
/// delegation first under `auto`/`mise` (one tool store for mise
/// users), self-download from GitHub releases otherwise.
pub async fn install_aube(
    cfg: &RuntimeConfig,
    version: &node_semver::Version,
) -> Result<InstalledAube, Error> {
    if let Some(existing) = find_installed_aube(version) {
        return Ok(existing);
    }
    match cfg.installer {
        InstallerMode::Aube => self_download(cfg, version).await,
        InstallerMode::Mise => {
            let Some(mise_bin) = mise::mise_on_path() else {
                return Err(Error::MiseInstallFailed {
                    version: format!("aube@{version}"),
                    reason: "runtimeInstaller=mise but mise is not on PATH".to_string(),
                });
            };
            delegate_to_mise(&mise_bin, version).await
        }
        InstallerMode::Auto => match mise::mise_on_path() {
            Some(mise_bin) => match delegate_to_mise(&mise_bin, version).await {
                Ok(install) => Ok(install),
                Err(e) => {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_RUNTIME_MISE_FALLBACK,
                        error = %e,
                        "mise failed to install aube; falling back to a release download"
                    );
                    self_download(cfg, version).await
                }
            },
            None => self_download(cfg, version).await,
        },
    }
}

async fn delegate_to_mise(
    mise_bin: &Path,
    version: &node_semver::Version,
) -> Result<InstalledAube, Error> {
    mise::install_tool_via_mise(mise_bin, "aube", version).await?;
    discover::mise_tool_installs_dir("aube")
        .map(|d| d.join(version.to_string()))
        .and_then(|d| validate_aube_install(&d, version.clone(), InstallOrigin::Mise))
        .ok_or_else(|| Error::MiseInstallFailed {
            version: format!("aube@{version}"),
            reason: "mise reported success but the install was not found — \
                     if mise uses a custom data dir, export MISE_DATA_DIR so aube sees the same path"
                .to_string(),
        })
}

/// Download a release archive, verify its published `.sha256` when
/// available (older releases predate checksum publishing; those fall
/// back to TLS-only with a debug note), extract — binaries sit at the
/// archive root — and atomically publish.
async fn self_download(
    cfg: &RuntimeConfig,
    version: &node_semver::Version,
) -> Result<InstalledAube, Error> {
    let root = self_dir().ok_or_else(|| {
        Error::io(
            "locate the aube self dir",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"),
        )
    })?;
    let dest = root.join(version.to_string());
    let locks = root.join(".locks");
    std::fs::create_dir_all(&locks)
        .map_err(|e| Error::io(format!("create {}", locks.display()), e))?;
    let lock_path = locks.join(format!("{version}.lock"));
    let lock = tokio::task::spawn_blocking(move || xx::fslock::FSLock::new(&lock_path).lock())
        .await
        .map_err(|e| {
            Error::io(
                "acquire self-install lock",
                std::io::Error::other(e.to_string()),
            )
        })?
        .map_err(|e| {
            Error::io(
                "acquire self-install lock",
                std::io::Error::other(e.to_string()),
            )
        })?;
    if let Some(existing) = validate_aube_install(&dest, version.clone(), InstallOrigin::Aube) {
        drop(lock);
        return Ok(existing);
    }

    let triple = release_target_triple()?;
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let archive_name = format!("aube-v{version}-{triple}.{ext}");
    let url = format!("{}/v{version}/{archive_name}", release_base());
    let http = Http::new(cfg.retries);
    let progress = NoopProgress;
    progress.on_phase(version, InstallPhase::Downloading);

    let downloads = root.join(".downloads");
    let staging_root = root.join(".tmp");
    std::fs::create_dir_all(&downloads)
        .map_err(|e| Error::io(format!("create {}", downloads.display()), e))?;
    std::fs::create_dir_all(&staging_root)
        .map_err(|e| Error::io(format!("create {}", staging_root.display()), e))?;
    let archive_path = downloads.join(format!("{archive_name}.{}", std::process::id()));
    let actual = stream_to_file(&http, &url, &archive_path, &progress).await?;

    // Published checksum, when the release ships one.
    match fetch_published_sha256(&http, &url).await {
        Some(expected) if expected != actual => {
            let _ = std::fs::remove_file(&archive_path);
            drop(lock);
            return Err(Error::ChecksumMismatch {
                url,
                expected: hex::encode(expected),
                actual: hex::encode(actual),
            });
        }
        Some(_) => {}
        None => {
            tracing::debug!(
                %url,
                "release publishes no .sha256 (pre-checksum release); trusting TLS"
            );
        }
    }

    let staging = staging_root.join(format!("{version}.{}", std::process::id()));
    std::fs::create_dir_all(&staging)
        .map_err(|e| Error::io(format!("create {}", staging.display()), e))?;
    let extract_from = archive_path.clone();
    let extract_to = staging.clone();
    let zip = ext == "zip";
    let extract_result = tokio::task::spawn_blocking(move || {
        crate::extract::extract_archive(&extract_from, &extract_to, zip, false)
    })
    .await
    .map_err(|e| Error::ExtractFailed {
        reason: e.to_string(),
    })?;
    let _ = std::fs::remove_file(&archive_path);
    if let Err(e) = extract_result {
        let _ = std::fs::remove_dir_all(&staging);
        drop(lock);
        return Err(e);
    }

    if let Err(rename_err) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_dir_all(&staging);
        if validate_aube_install(&dest, version.clone(), InstallOrigin::Aube).is_none() {
            drop(lock);
            return Err(Error::io(
                format!("publish aube {} into {}", version, dest.display()),
                rename_err,
            ));
        }
    }
    drop(lock);

    validate_aube_install(&dest, version.clone(), InstallOrigin::Aube).ok_or_else(|| {
        Error::ExtractFailed {
            reason: format!(
                "release archive did not produce a usable aube at {}",
                dest.display()
            ),
        }
    })
}

/// Fetch `{archive_url}.sha256` and parse the leading hex digest
/// (taiki-e's checksum files are `<hex> *<filename>`). `None` when the
/// asset doesn't exist or doesn't parse — caller decides the policy.
async fn fetch_published_sha256(http: &Http, archive_url: &str) -> Option<[u8; 32]> {
    let url = format!("{archive_url}.sha256");
    let resp = http.get(&url, None, None, false).await.ok()?;
    let text = resp.body?.text().await.ok()?;
    let hex_token = text.split_whitespace().next()?;
    let bytes = hex::decode(hex_token).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fab_aube(root: &Path, version: &str) {
        let dir = root.join(version);
        std::fs::create_dir_all(&dir).unwrap();
        for bin in ["aube", "aubr", "aubx"] {
            let path = dir.join(if cfg!(windows) {
                format!("{bin}.exe")
            } else {
                bin.to_string()
            });
            std::fs::write(&path, "#!/bin/sh\necho fake\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }

    #[test]
    fn scans_and_validates_aube_installs() {
        let tmp = tempfile::tempdir().unwrap();
        fab_aube(tmp.path(), "1.17.0");
        fab_aube(tmp.path(), "1.18.2");
        fab_aube(tmp.path(), "1.19.0");
        std::fs::write(tmp.path().join("1.19.0/incomplete"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("not-a-version")).unwrap();

        let mut versions: Vec<String> = scan_aube_dir(tmp.path(), InstallOrigin::Mise)
            .into_iter()
            .map(|i| i.version.to_string())
            .collect();
        versions.sort();
        assert_eq!(versions, vec!["1.17.0", "1.18.2"]);
    }

    #[test]
    fn validate_accepts_bin_subdir_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("2.0.0/bin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(if cfg!(windows) { "aube.exe" } else { "aube" }),
            "x",
        )
        .unwrap();
        let install = validate_aube_install(
            &tmp.path().join("2.0.0"),
            "2.0.0".parse().unwrap(),
            InstallOrigin::Aube,
        )
        .unwrap();
        assert!(install.exe.parent().unwrap().ends_with("bin"));
    }

    #[test]
    fn target_triple_is_publishable() {
        // On every platform CI runs, the host triple must map to a
        // name aube actually publishes (Intel macOS is the documented
        // exception).
        match release_target_triple() {
            Ok(t) => {
                assert!(
                    t.contains("apple-darwin")
                        || t.contains("unknown-linux")
                        || t.contains("pc-windows"),
                    "{t}"
                );
            }
            Err(Error::UnsupportedPlatform { .. }) => {
                assert_eq!(std::env::consts::OS, "macos");
                assert_eq!(std::env::consts::ARCH, "x86_64");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}
