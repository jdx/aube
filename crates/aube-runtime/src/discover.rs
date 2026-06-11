//! Discovery of already-installed Node versions: aube's own runtime
//! dir, mise's installs dir (read-only), and the `node` on PATH.

use crate::paths;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where an installed Node came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOrigin {
    Aube,
    Mise,
}

impl InstallOrigin {
    pub fn label(self) -> &'static str {
        match self {
            InstallOrigin::Aube => "aube",
            InstallOrigin::Mise => "mise",
        }
    }
}

/// A validated on-disk Node install.
#[derive(Debug, Clone)]
pub struct InstalledNode {
    pub version: node_semver::Version,
    pub install_dir: PathBuf,
    /// The directory to prepend to PATH: `<dir>/bin` on unix, the
    /// install dir itself on Windows (node.exe sits at the root).
    pub bin_dir: PathBuf,
    pub node_bin: PathBuf,
    pub origin: InstallOrigin,
}

/// List every valid installed Node version across aube's runtime dir
/// and mise's installs dir. When both have the same version, aube's
/// copy wins (deterministic, and it's the copy aube can manage).
pub fn list_installed() -> Vec<InstalledNode> {
    let mut by_version: BTreeMap<node_semver::Version, InstalledNode> = BTreeMap::new();
    // Insert mise first so aube entries overwrite on version collision.
    if let Some(dir) = mise_node_installs_dir() {
        for node in scan_install_dir(&dir, InstallOrigin::Mise) {
            by_version.insert(node.version.clone(), node);
        }
    }
    if let Some(dir) = paths::runtime_dir() {
        for node in scan_install_dir(&dir, InstallOrigin::Aube) {
            by_version.insert(node.version.clone(), node);
        }
    }
    by_version.into_values().collect()
}

/// mise's node installs directory.
pub fn mise_node_installs_dir() -> Option<PathBuf> {
    mise_tool_installs_dir("node")
}

/// mise's installs directory for one tool:
/// `$MISE_INSTALLS_DIR || $MISE_DATA_DIR/installs || ~/.local/share/mise/installs`,
/// plus the tool segment. mise uses `~/.local/share` on every OS.
pub fn mise_tool_installs_dir(tool: &str) -> Option<PathBuf> {
    let installs = if let Some(dir) = std::env::var_os("MISE_INSTALLS_DIR") {
        PathBuf::from(dir)
    } else if let Some(dir) = std::env::var_os("MISE_DATA_DIR") {
        PathBuf::from(dir).join("installs")
    } else {
        let data_home = aube_util::env::xdg_data_home()
            .or_else(|| aube_util::env::home_dir().map(|h| h.join(".local/share")))?;
        data_home.join("mise/installs")
    };
    Some(installs.join(tool))
}

/// Scan one installs root (dir-per-version) and validate each entry:
/// the dir name must parse as a version, symlinks are skipped (mise's
/// `latest` / `lts` / `20` aliases are symlinks — including them would
/// double-count), an in-progress install (mise's `incomplete` marker
/// file) is skipped, and the node binary must exist.
fn scan_install_dir(root: &Path, origin: InstallOrigin) -> Vec<InstalledNode> {
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
            // Skips both files and symlinked alias dirs:
            // `DirEntry::file_type` does not follow symlinks.
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(version) = node_semver::Version::parse(name.trim_start_matches('v')) else {
            continue;
        };
        if let Some(node) = validate_install(&path, version, origin) {
            out.push(node);
        }
    }
    out
}

/// Validate a single version dir and compute its bin paths. Public to
/// the crate so the installer can re-check a freshly-published dir
/// (or one mise just created) through the exact same rules.
pub(crate) fn validate_install(
    dir: &Path,
    version: node_semver::Version,
    origin: InstallOrigin,
) -> Option<InstalledNode> {
    if dir.join("incomplete").exists() {
        return None;
    }
    let (bin_dir, node_bin) = node_paths_in(dir);
    if !node_bin.is_file() {
        return None;
    }
    Some(InstalledNode {
        version,
        install_dir: dir.to_path_buf(),
        bin_dir,
        node_bin,
        origin,
    })
}

/// Per-OS layout of a native Node install: unix puts `node` under
/// `bin/`, Windows puts `node.exe` at the root (mise mirrors both).
pub(crate) fn node_paths_in(dir: &Path) -> (PathBuf, PathBuf) {
    if cfg!(windows) {
        // Windows zips have node.exe at the archive root, but mise
        // (and some mirrors' layouts) use bin\node.exe — accept both.
        let root_exe = dir.join("node.exe");
        if root_exe.is_file() {
            return (dir.to_path_buf(), root_exe);
        }
        let bin = dir.join("bin");
        let exe = bin.join("node.exe");
        (bin, exe)
    } else {
        let bin = dir.join("bin");
        let exe = bin.join("node");
        (bin, exe)
    }
}

/// Find `node` on PATH and probe its version (`node --version`).
/// Memoized for the process: one spawn no matter how many resolution
/// calls happen.
pub fn probe_path_node() -> Option<(node_semver::Version, PathBuf)> {
    static PROBED: std::sync::OnceLock<Option<(node_semver::Version, PathBuf)>> =
        std::sync::OnceLock::new();
    PROBED.get_or_init(probe_path_node_uncached).clone()
}

fn probe_path_node_uncached() -> Option<(node_semver::Version, PathBuf)> {
    let exe = find_on_path(if cfg!(windows) { "node.exe" } else { "node" })?;
    let output = std::process::Command::new(&exe)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let version = node_semver::Version::parse(raw.trim().trim_start_matches('v')).ok()?;
    Some((version, exe))
}

/// Minimal PATH walk (std-only). Returns the first existing,
/// file-typed match.
pub(crate) fn find_on_path(bin_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            // PATHEXT resolution for the bare name (mise is usually
            // mise.exe; node is node.exe — callers pass the .exe name
            // already, this is a fallback).
            let with_exe = dir.join(format!("{bin_name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fab_install(root: &Path, version: &str) {
        let dir = root.join(version);
        let bin = if cfg!(windows) {
            dir.clone()
        } else {
            dir.join("bin")
        };
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(if cfg!(windows) { "node.exe" } else { "node" });
        std::fs::write(&exe, "#!/bin/sh\necho v0.0.0\n").unwrap();
    }

    #[test]
    fn scans_and_validates() {
        let tmp = tempfile::tempdir().unwrap();
        fab_install(tmp.path(), "22.1.0");
        fab_install(tmp.path(), "24.4.1");
        // Incomplete install: skipped.
        fab_install(tmp.path(), "26.0.0");
        std::fs::write(tmp.path().join("26.0.0/incomplete"), "").unwrap();
        // Missing binary: skipped.
        std::fs::create_dir_all(tmp.path().join("20.0.0")).unwrap();
        // Non-version dir: skipped.
        std::fs::create_dir_all(tmp.path().join(".downloads")).unwrap();

        let found = scan_install_dir(tmp.path(), InstallOrigin::Aube);
        let mut versions: Vec<String> = found.iter().map(|n| n.version.to_string()).collect();
        versions.sort();
        assert_eq!(versions, vec!["22.1.0", "24.4.1"]);
    }

    #[cfg(unix)]
    #[test]
    fn alias_symlinks_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        fab_install(tmp.path(), "22.1.0");
        std::os::unix::fs::symlink(tmp.path().join("22.1.0"), tmp.path().join("22")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("22.1.0"), tmp.path().join("latest")).unwrap();
        let found = scan_install_dir(tmp.path(), InstallOrigin::Aube);
        assert_eq!(found.len(), 1, "{found:?}");
    }
}
