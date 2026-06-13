use std::path::PathBuf;

use crate::identity::embedder;

/// Whether a *branded* settings env-var alias (the tool-prefixed form like
/// `AUBE_NODE_LINKER`) should be read, given the active embedder's
/// [`env_prefix`](crate::identity::Embedder::env_prefix).
///
/// aube's settings table declares each branded env alias as `{PREFIX}_<NAME>`
/// alongside the neutral `npm_config_*` / `NPM_CONFIG_*` forms and a handful of
/// bare external vars (`CI`, `HTTP_PROXY`, `NODE_OPTIONS`, …). `env_prefix` is
/// the single on/off switch for the *branded* surface only:
///
/// - `Some(prefix)` — a branded alias is read only when it is `{prefix}_…`.
///   Standalone aube (`Some("AUBE")`) thus reads every `AUBE_*` settings var
///   exactly as before, and nothing else changes.
/// - `None` — the embedder reads *no* branded settings env vars; every
///   tool-branded alias is skipped.
///
/// The neutral `npm_config_*` / `NPM_CONFIG_*` aliases and the bare external
/// vars are never the tool's brand and are always honored. Standalone aube's
/// settings table only ever emits its own `env_prefix` as the branded prefix,
/// so the brand family is exactly the `{prefix}_*` set.
pub fn branded_env_alias_enabled(alias: &str) -> bool {
    // npm-compat family — never the tool's brand, always honored.
    if alias.starts_with("npm_config_") || alias.starts_with("NPM_CONFIG_") {
        return true;
    }
    // Bare external/neutral vars — not part of any tool's brand family.
    if !looks_branded(alias) {
        return true;
    }
    // A branded-shaped alias: read it only when it matches the active prefix.
    match embedder().env_prefix {
        Some(prefix) => alias
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('_')),
        None => false,
    }
}

/// Does `alias` have the `<UPPER_PREFIX>_<NAME>` shape of a tool-branded env
/// var, as opposed to a bare external var (`CI`) or neutral proxy/Node var
/// (`HTTP_PROXY`, `NODE_OPTIONS`)? aube's settings table only ever emits its
/// own `env_prefix` as the branded prefix, so this just has to separate the
/// branded family from the recognized neutral vars.
fn looks_branded(alias: &str) -> bool {
    const NEUTRAL: &[&str] = &[
        "CI",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "PROXY",
        "NODE_OPTIONS",
    ];
    if NEUTRAL.contains(&alias) {
        return false;
    }
    match alias.split_once('_') {
        Some((head, _)) if !head.is_empty() => head.chars().all(|c| c.is_ascii_uppercase()),
        _ => false,
    }
}

pub fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}

pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(h.into());
    }
    #[cfg(windows)]
    if let Some(h) = std::env::var_os("USERPROFILE") {
        return Some(h.into());
    }
    None
}

fn non_empty_path_var(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub fn xdg_config_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CONFIG_HOME")
}

pub fn xdg_data_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_DATA_HOME")
}

pub fn xdg_cache_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CACHE_HOME")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under the default (AUBE) profile — `env_prefix = Some("AUBE")` — every
    /// settings env alias aube's table declares is honored: the branded
    /// `AUBE_*` form, the neutral `npm_config_*` / `NPM_CONFIG_*` forms, and
    /// the bare external vars. This is the standalone-neutrality contract for
    /// the env-prefix gate: a binary that registers no profile reads exactly
    /// what aube read before the gate existed.
    #[test]
    fn aube_profile_honors_every_settings_env_family() {
        // Branded family (the tool's own prefix).
        assert!(branded_env_alias_enabled("AUBE_NODE_LINKER"));
        assert!(branded_env_alias_enabled("AUBE_NO_LOCK"));
        assert!(branded_env_alias_enabled("AUBE_LINK_CONCURRENCY"));
        // npm-compat family — never gated.
        assert!(branded_env_alias_enabled("npm_config_node_linker"));
        assert!(branded_env_alias_enabled("NPM_CONFIG_NODE_LINKER"));
        // Bare external / neutral vars — never gated.
        assert!(branded_env_alias_enabled("CI"));
        assert!(branded_env_alias_enabled("HTTP_PROXY"));
        assert!(branded_env_alias_enabled("NODE_OPTIONS"));
    }

    /// `looks_branded` separates the tool-branded `<UPPER>_<NAME>` shape from
    /// the recognized neutral/external vars, so the `None`-prefix embedder
    /// skips exactly the branded family and nothing else.
    #[test]
    fn looks_branded_distinguishes_brand_from_neutral() {
        assert!(looks_branded("AUBE_NODE_LINKER"));
        assert!(looks_branded("FOO_BAR")); // any UPPER-prefixed var reads as branded
        assert!(!looks_branded("CI"));
        assert!(!looks_branded("HTTP_PROXY"));
        assert!(!looks_branded("NODE_OPTIONS"));
        assert!(!looks_branded("npm_config_node_linker")); // lowercase head
    }
}
