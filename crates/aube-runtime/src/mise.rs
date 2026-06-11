//! mise integration: read-only discovery lives in [`crate::discover`];
//! this module handles *delegation* — asking mise to install a version
//! so mise users keep a single Node store on disk.

use crate::discover::{self, InstallOrigin, InstalledNode};
use crate::error::Error;
use std::path::PathBuf;

/// The `mise` executable on PATH, if any. Memoized — PATH walks are
/// cheap but this gets asked on every resolution that reaches the
/// install stage.
pub fn mise_on_path() -> Option<PathBuf> {
    static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| discover::find_on_path(if cfg!(windows) { "mise.exe" } else { "mise" }))
        .clone()
}

/// Run `mise install node@<version>` and return the resulting install
/// from mise's installs dir.
///
/// Output is captured, never inherited — a clx progress bar may be
/// live on stderr, and mise's own progress output would corrupt it.
/// Captured stderr is surfaced through `tracing::debug!` and, on
/// failure, the tail rides along in the error message. Failure is
/// exit-status only; mise's stderr is human prose and not a stable
/// interface.
pub(crate) async fn install_via_mise(
    mise_bin: &std::path::Path,
    version: &node_semver::Version,
) -> Result<InstalledNode, Error> {
    install_tool_via_mise(mise_bin, "node", version).await?;

    // Rescan for exactly that version. Exit 0 but no discoverable
    // install usually means aube's view of the installs dir differs
    // from mise's config (custom MISE_DATA_DIR in mise's own env,
    // shared install dirs, etc.).
    find_mise_install(version).ok_or_else(|| Error::MiseInstallFailed {
        version: format!("node@{version}"),
        reason: format!(
            "mise reported success but node@{} was not found under {} — \
             if mise uses a custom data dir, export MISE_DATA_DIR so aube sees the same path",
            version,
            discover::mise_node_installs_dir()
                .unwrap_or_default()
                .display()
        ),
    })
}

/// Run `mise install <tool>@<version>` for any tool. Callers rescan
/// the installs dir themselves — layouts differ per tool.
pub(crate) async fn install_tool_via_mise(
    mise_bin: &std::path::Path,
    tool: &str,
    version: &node_semver::Version,
) -> Result<(), Error> {
    let spec = format!("{tool}@{version}");
    tracing::debug!(mise = %mise_bin.display(), %spec, "delegating install to mise");
    let output = tokio::process::Command::new(mise_bin)
        .args(["install", &spec])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| Error::MiseInstallFailed {
            version: spec.clone(),
            reason: format!("failed to spawn mise: {e}"),
        })?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        tracing::debug!(target: "mise", "{line}");
    }

    if !output.status.success() {
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(Error::MiseInstallFailed {
            version: spec,
            reason: format!(
                "exit status {}{}{}",
                output.status.code().unwrap_or(-1),
                if tail.is_empty() { "" } else { "\n" },
                tail
            ),
        });
    }
    Ok(())
}

/// Look up one exact version in mise's installs dir.
pub(crate) fn find_mise_install(version: &node_semver::Version) -> Option<InstalledNode> {
    let dir = discover::mise_node_installs_dir()?.join(version.to_string());
    discover::validate_install(&dir, version.clone(), InstallOrigin::Mise)
}
