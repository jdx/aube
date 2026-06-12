//! `aube config` — read/write settings in aube config and `.npmrc`.
//!
//! The command's known setting surface is derived from
//! [`aube_settings::meta::SETTINGS`], generated at build time from
//! `settings.toml`. Known aube-owned user/global settings are written
//! to `~/.config/aube/config.toml`; unknown and registry/auth keys are
//! still accepted verbatim because `.npmrc` is free-form and includes
//! auth-token entries such as `//registry.npmjs.org/:_authToken`.

mod aube_config;
mod delete;
mod explain;
mod find;
#[path = "get.rs"]
mod get_cmd;
mod list;
#[path = "set.rs"]
mod set_cmd;
#[cfg(feature = "config-tui")]
mod tui;

use crate::commands::npmrc::{NpmrcEdit, user_npmrc_path};
use aube_settings::meta as settings_meta;
use clap::{Args, Subcommand, ValueEnum};
use miette::miette;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(flatten)]
    pub list: list::ListArgs,

    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Delete a key from aube config or the selected `.npmrc` file
    #[command(visible_aliases = ["rm", "remove", "unset"])]
    Delete(delete::DeleteArgs),
    /// Explain a known setting, including defaults and supported config sources
    Explain(explain::ExplainArgs),
    /// Search known settings by name, source key, or description
    #[command(visible_alias = "search")]
    Find(find::FindArgs),
    /// Print the effective value of a key
    Get(GetArgs),
    /// Print every key/value from aube config and selected `.npmrc` file(s)
    #[command(visible_alias = "ls")]
    List(list::ListArgs),
    /// Write a key=value pair to aube config or the selected `.npmrc` file
    Set(SetArgs),
    /// Browse known settings in an interactive terminal UI
    Tui,
}

#[derive(Debug, Args)]
pub struct KeyArgs {
    /// The setting key.
    ///
    /// Accepts either a pnpm canonical name (e.g. `autoInstallPeers`)
    /// or an `.npmrc` alias (e.g. `auto-install-peers`).
    pub key: String,

    /// Shortcut for `--location project`.
    #[arg(long, conflicts_with = "location")]
    pub local: bool,

    /// Which config location to act on.
    ///
    /// Defaults to `user`. Delete sweeps both aube's own config
    /// (`~/.config/aube/config.toml` at user-scope,
    /// `<cwd>/.config/aube/config.toml` at project-scope) and the
    /// matching `.npmrc`, so the call works regardless of which file
    /// the value was originally written to.
    #[arg(long, value_enum, default_value_t = Location::User)]
    pub location: Location,
}

impl KeyArgs {
    pub(super) fn effective_location(&self) -> Location {
        if self.local {
            Location::Project
        } else {
            self.location
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Location {
    /// User config (`~/.config/aube/config.toml` for known aube
    /// settings, `~/.npmrc` for registry/auth and unknown keys)
    User,
    /// `<cwd>/.npmrc`
    Project,
    /// Alias for `user` — aube has no separate global config file.
    Global,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ListLocation {
    /// Merge `~/.npmrc`, user aube config, and project `.npmrc`,
    /// last-write-wins (same precedence install uses).
    Merged,
    /// Only user config (`~/.config/aube/config.toml` + `~/.npmrc`)
    User,
    /// Only `<cwd>/.npmrc`
    Project,
    /// Alias for `user`.
    Global,
}

pub(crate) use aube_config::{
    load_project_entries as load_project_aube_config_entries,
    load_user_entries as load_user_aube_config_entries,
};
pub(crate) use get_cmd::GetArgs;
pub(crate) use set_cmd::SetArgs;

impl Location {
    pub(super) fn path(self) -> miette::Result<PathBuf> {
        match self {
            Location::User | Location::Global => user_npmrc_path(),
            Location::Project => Ok(crate::dirs::project_root_or_cwd()?.join(".npmrc")),
        }
    }
}

pub async fn run(args: ConfigArgs) -> miette::Result<()> {
    match args.command {
        Some(ConfigCommand::Get(a)) => {
            reject_parent_list_args(&args.list, "get")?;
            get(a)
        }
        Some(ConfigCommand::Set(a)) => {
            reject_parent_list_args(&args.list, "set")?;
            set(a)
        }
        Some(ConfigCommand::Delete(a)) => {
            reject_parent_list_args(&args.list, "delete")?;
            delete::run(a)
        }
        Some(ConfigCommand::Explain(a)) => {
            reject_parent_list_args(&args.list, "explain")?;
            explain::run(a)
        }
        Some(ConfigCommand::Find(a)) => {
            reject_parent_list_args(&args.list, "find")?;
            find::run(a)
        }
        Some(ConfigCommand::List(mut a)) => {
            a.apply_parent(args.list);
            list::run(a)
        }
        Some(ConfigCommand::Tui) => {
            reject_parent_list_args(&args.list, "tui")?;
            tui::run()
        }
        None => list::run(args.list),
    }
}

fn reject_parent_list_args(args: &list::ListArgs, subcommand: &str) -> miette::Result<()> {
    if args.has_parent_overrides() {
        Err(miette!(
            "`aube config` list flags must be used with `aube config` or `aube config list`, not `aube config {subcommand}`"
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(feature = "config-tui"))]
mod tui {
    use miette::miette;

    pub fn run() -> miette::Result<()> {
        Err(miette!(
            "`aube config tui` was not enabled in this build; rebuild with the `config-tui` feature"
        ))
    }
}

pub(crate) fn get(args: GetArgs) -> miette::Result<()> {
    get_cmd::run(args)
}

pub(crate) fn set(args: SetArgs) -> miette::Result<()> {
    set_cmd::run(args)
}

/// True for entries in `SettingMeta::npmrc_keys` that are real, literal
/// `.npmrc` keys — not pattern templates like `@scope:registry` or
/// `//host/:_authToken`.
fn is_literal_alias(key: &str) -> bool {
    !key.starts_with("//") && !key.contains(':')
}

/// Expand a user-supplied key into the full set of `.npmrc` aliases it
/// covers. Pattern-template entries in `npmrc_keys` (e.g.
/// `@scope:registry`) are filtered out — see [`is_literal_alias`].
pub(super) fn resolve_aliases(key: &str) -> Vec<String> {
    if let Some(meta) = settings_meta::find(key) {
        let literals = literal_aliases(meta.npmrc_keys);
        if !literals.is_empty() {
            return literals;
        }
    }
    for meta in settings_meta::all() {
        let literals = literal_aliases(meta.npmrc_keys);
        if literals.iter().any(|a| a == key) {
            return literals;
        }
    }
    vec![key.to_string()]
}

pub(super) fn literal_aliases(keys: &[&'static str]) -> Vec<String> {
    keys.iter()
        .filter(|k| is_literal_alias(k))
        .map(|s| s.to_string())
        .collect()
}

/// True when `key` belongs to the npm-shared `.npmrc` surface: npm,
/// pnpm, and yarn read it from `.npmrc` so `aube config set` keeps
/// the value there for cross-tool visibility. The two pattern checks
/// cover per-host auth/cert templates (`//host/:_authToken`, etc.)
/// and scoped registries (`@scope:registry`); everything else is
/// driven by the `npmShared` flag on each entry in `settings.toml`,
/// so the answer for any specific key lives next to that setting's
/// other metadata rather than in a hardcoded list here.
pub(super) fn is_npm_shared_key(key: &str) -> bool {
    if key.starts_with("//") {
        return true;
    }
    if let Some(rest) = key.strip_prefix('@')
        && rest.ends_with(":registry")
    {
        return true;
    }
    setting_for_key(key).is_some_and(|meta| meta.npm_shared)
}

pub(super) fn setting_for_key(key: &str) -> Option<&'static settings_meta::SettingMeta> {
    settings_meta::find(key).or_else(|| {
        settings_meta::all().iter().find(|meta| {
            meta.npmrc_keys.iter().any(|candidate| candidate == &key)
                || meta
                    .workspace_yaml_keys
                    .iter()
                    .any(|candidate| candidate == &key)
                || meta.env_vars.iter().any(|candidate| candidate == &key)
                || meta.cli_flags.iter().any(|candidate| candidate == &key)
        })
    })
}

pub(super) fn setting_search_score(meta: &settings_meta::SettingMeta, terms: &[String]) -> usize {
    let names = setting_search_text(&[
        &[meta.name],
        meta.cli_flags,
        meta.env_vars,
        meta.npmrc_keys,
        meta.workspace_yaml_keys,
    ]);
    let summary = setting_search_text(&[&[meta.description]]);
    let body = setting_search_text(&[&[meta.docs], meta.examples]);

    terms
        .iter()
        .map(|term| {
            usize::from(search_text_matches(&names, term)) * 4
                + usize::from(search_text_matches(&summary, term)) * 2
                + usize::from(search_text_matches(&body, term))
        })
        .sum()
}

fn setting_search_text(groups: &[&[&str]]) -> String {
    let mut out = String::new();
    for value in groups.iter().copied().flatten().copied() {
        out.push(' ');
        out.push_str(value);
    }
    out.to_ascii_lowercase()
}

fn search_text_matches(haystack: &str, term: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word.starts_with(term))
}

/// Walk every config source in low-to-high precedence order so a later
/// duplicate wins. Mirrors the chain the install pipeline applies via
/// [`aube_settings::resolved`]:
/// `userNpmrc < userAubeConfig < workspaceYaml < projectNpmrc <
/// projectAubeConfig`. `workspaceYaml` sits above user-scope sources
/// because it lives at the project root (scope locality). Per-setting
/// `precedence` overrides in `settings.toml` can reorder file sources
/// (e.g. `minimumReleaseAge` puts `workspaceYaml` first); `aube config
/// get` shows the default-precedence view, which is accurate for the
/// common cases.
pub(super) fn read_merged(cwd: &Path) -> miette::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Ok(user) = user_npmrc_path() {
        out.extend(read_single(&user)?);
    }
    out.extend(aube_config::load_user_entries());
    out.extend(read_workspace_yaml_flat(cwd));
    out.extend(read_single(&cwd.join(".npmrc"))?);
    out.extend(aube_config::load_project_entries(cwd));
    Ok(out)
}

/// Surface flat scalar entries from the project's workspace yaml so
/// `aube config get/list` can report values aube actually reads from
/// there (`autoInstallPeers`, `nodeLinker`, `minimumReleaseAge`, …).
/// Nested mappings (`updateConfig.ignoreDependencies`, `catalog`,
/// `allowBuilds`) are skipped — they don't round-trip through a simple
/// `(key, raw)` view and aren't what `config get <bare-key>` asks for.
pub(super) fn read_workspace_yaml_flat(cwd: &Path) -> Vec<(String, String)> {
    let Ok(map) = aube_manifest::workspace::load_raw(cwd) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, v)| yaml_scalar_string(v).map(|raw| (k.clone(), raw)))
        .collect()
}

fn yaml_scalar_string(value: &yaml_serde::Value) -> Option<String> {
    match value {
        yaml_serde::Value::String(s) => Some(s.clone()),
        yaml_serde::Value::Number(n) => Some(n.to_string()),
        yaml_serde::Value::Bool(b) => Some(b.to_string()),
        yaml_serde::Value::Sequence(items) => {
            let parts: Vec<String> = items.iter().filter_map(yaml_scalar_string).collect();
            (!parts.is_empty()).then(|| parts.join(","))
        }
        _ => None,
    }
}

pub(super) fn read_single(path: &std::path::Path) -> miette::Result<Vec<(String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let edit = NpmrcEdit::load(path)?;
    Ok(edit.entries())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_list_key_collapses_alias_to_primary() {
        assert_eq!(
            list::canonical_list_key("autoInstallPeers"),
            "auto-install-peers"
        );
        assert_eq!(
            list::canonical_list_key("auto-install-peers"),
            "auto-install-peers"
        );
    }

    #[test]
    fn canonical_list_key_passthrough_for_unknown_key() {
        assert_eq!(
            list::canonical_list_key("//registry.example.com/:_authToken"),
            "//registry.example.com/:_authToken"
        );
    }

    #[test]
    fn resolve_aliases_canonical_name() {
        let aliases = resolve_aliases("autoInstallPeers");
        assert!(aliases.iter().any(|a| a == "auto-install-peers"));
        assert!(aliases.iter().any(|a| a == "autoInstallPeers"));
    }

    #[test]
    fn resolve_aliases_from_alias() {
        let aliases = resolve_aliases("auto-install-peers");
        assert!(aliases.iter().any(|a| a == "auto-install-peers"));
        assert!(aliases.iter().any(|a| a == "autoInstallPeers"));
    }

    #[test]
    fn resolve_aliases_registry_excludes_template_keys() {
        let aliases = resolve_aliases("registry");
        assert_eq!(aliases, vec!["registry".to_string()]);
        for a in &aliases {
            assert!(is_literal_alias(a), "leaked template alias: {a}");
        }
    }

    #[test]
    fn resolve_aliases_template_input_is_identity() {
        for template in [
            "@scope:registry",
            "//registry.example.com/:_authToken",
            "//registry.example.com/:_auth",
        ] {
            assert_eq!(
                resolve_aliases(template),
                vec![template.to_string()],
                "{template} should be identity, not registries-grouped"
            );
        }
    }

    #[test]
    fn is_literal_alias_recognizes_templates() {
        assert!(is_literal_alias("registry"));
        assert!(is_literal_alias("auto-install-peers"));
        assert!(!is_literal_alias("@scope:registry"));
        assert!(!is_literal_alias("//host/:_authToken"));
        assert!(!is_literal_alias("//host/:_auth"));
    }

    #[test]
    fn resolve_aliases_unknown_key_is_identity() {
        let aliases = resolve_aliases("//registry.example.com/:_authToken");
        assert_eq!(
            aliases,
            vec!["//registry.example.com/:_authToken".to_string()]
        );
    }

    #[test]
    fn preferred_write_key_keeps_user_typed_alias() {
        let aliases = vec![
            "auto-install-peers".to_string(),
            "autoInstallPeers".to_string(),
        ];
        assert_eq!(
            set_cmd::preferred_write_key("autoInstallPeers", &aliases),
            "autoInstallPeers"
        );
        assert_eq!(
            set_cmd::preferred_write_key("auto-install-peers", &aliases),
            "auto-install-peers"
        );
    }

    #[test]
    fn preferred_write_key_falls_back_to_first_alias() {
        let aliases = vec![
            "auto-install-peers".to_string(),
            "autoInstallPeers".to_string(),
        ];
        assert_eq!(
            set_cmd::preferred_write_key("something-else", &aliases),
            "auto-install-peers"
        );
    }
}
