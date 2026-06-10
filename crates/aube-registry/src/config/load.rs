use std::path::{Path, PathBuf};

use super::env::npm_config_env_entries_from;
use super::npmrc::{parse_npmrc, parse_npmrc_untrusted};
use super::types::{NpmConfig, NpmrcSource};

impl NpmConfig {
    /// Load config by reading .npmrc files in priority order:
    /// 1. ~/.npmrc (user)
    /// 2. .npmrc in project dir (project)
    ///
    /// Project-level values override user-level values. Shares file
    /// discovery with [`load_npmrc_entries`] so the registry client and
    /// the generic settings resolver (`aube_cli::settings_values`) can
    /// never disagree on precedence.
    pub fn load(project_dir: &Path) -> Self {
        let env: Vec<(String, String)> = std::env::vars().collect();
        Self::load_with_env(project_dir, &env)
    }

    /// Test-only loader that reads `project_dir/.npmrc` with a
    /// tempdir pinned as the user's `$HOME` and no env-var merge, so
    /// the developer's real `~/.npmrc` and `NPM_CONFIG_*` vars can't
    /// bleed into assertions. Returns a config seeded the same way
    /// [`NpmConfig::load`] does (npmjs default registry, builtin `@jsr`
    /// scope), so assertions that pin `.registry` or scoped lookups
    /// behave the same as they would on a fresh user machine.
    ///
    /// Keep the `TempDir` binding alive inside the function scope:
    /// `load_npmrc_entries_with_home` reads the files synchronously
    /// and returns before the tempdir drops, so callers don't need to
    /// juggle the handle themselves.
    #[cfg(test)]
    pub(crate) fn load_isolated(project_dir: &Path) -> Self {
        let home = tempfile::tempdir().expect("tempdir for isolated config load");
        let mut config = Self {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            project_dir,
            None,
        ));
        config.apply_builtin_scoped_defaults();
        config
    }

    /// Same as [`NpmConfig::load`] but takes a captured env snapshot
    /// instead of reading `std::env` directly. Tests that assert on
    /// file-only behavior pass an empty slice so `npm_config_*` vars
    /// leaking from the developer's shell can't perturb the result.
    pub(crate) fn load_with_env(project_dir: &Path, env: &[(String, String)]) -> Self {
        let mut config = Self {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        // Feed tagged entries so `apply_tagged` can reject
        // high-privilege settings sourced from untrusted locations.
        let xdg = aube_util::env::xdg_config_home();
        let home = home_dir();
        // `NPM_CONFIG_USERCONFIG` / `npm_config_userconfig` move the
        // user-level `.npmrc` off the default `$HOME/.npmrc`. npm and
        // pnpm both honor this for XDG layouts and CI secret mounts.
        // Resolve once from the captured env slice and pass it to the
        // loader so tests that drive `load_with_env` can exercise the
        // same code path without mutating process-wide env.
        let user_rc_override = userconfig_override_from_env(env, home.as_deref());
        let mut tagged = load_npmrc_entries_tagged_with_home(
            home.as_deref(),
            xdg.as_deref(),
            project_dir,
            user_rc_override.as_deref(),
        );
        // `npm_config_*` / `NPM_CONFIG_*` env vars beat file config in
        // npm/pnpm. Apply them after `.npmrc` so last-write-wins gives
        // env the higher slot, and tag them as `Env` so
        // subprocess-settings gating still trusts them.
        tagged.extend(
            npm_config_env_entries_from(env)
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Env, k, v)),
        );
        config.apply_tagged(tagged);
        // Env vars fill in any proxy fields the .npmrc didn't set.
        // npm/pnpm/curl all check both the upper- and lowercase forms.
        config.apply_proxy_env();
        config.apply_builtin_scoped_defaults();
        config
    }
}

/// Scope-split view of [`load_npmrc_entries`]. Returns user-scope
/// entries (user `~/.npmrc` + pnpm `auth.ini`) and project-scope entries
/// (project `<cwd>/.npmrc` + `npmrcAuthFile`) as separate slices so the
/// settings resolver can apply the locality principle (project beats
/// user) while interleaving aube's own config sources.
///
/// Concatenating `user` and `project` (in that order) yields the same
/// list as [`load_npmrc_entries`].
pub fn load_npmrc_entries_split(project_dir: &Path) -> SplitNpmrcEntries {
    use std::sync::{Mutex, OnceLock};
    type CacheMap = std::collections::HashMap<PathBuf, SplitNpmrcEntries>;
    static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(project_dir)
    {
        return hit.clone();
    }
    let xdg = aube_util::env::xdg_config_home();
    let home = home_dir();
    let user_rc_override = std::env::var("NPM_CONFIG_USERCONFIG")
        .ok()
        .or_else(|| std::env::var("npm_config_userconfig").ok())
        .and_then(|raw| expand_userconfig_path(&raw, home.as_deref()));
    let tagged = load_npmrc_entries_tagged_with_home(
        home.as_deref(),
        xdg.as_deref(),
        project_dir,
        user_rc_override.as_deref(),
    );
    let mut split = SplitNpmrcEntries::default();
    for (src, k, v) in tagged {
        match src {
            NpmrcSource::User | NpmrcSource::PnpmAuth => split.user.push((k, v)),
            NpmrcSource::Project | NpmrcSource::NpmrcAuthFile => split.project.push((k, v)),
            // Env-derived entries (npm_config_*) aren't loaded by the
            // tagged file walker, so this arm is unreachable here.
            NpmrcSource::Env => continue,
        }
    }
    if let Ok(mut map) = cache.lock() {
        map.insert(project_dir.to_path_buf(), split.clone());
    }
    split
}

#[derive(Default, Clone)]
pub struct SplitNpmrcEntries {
    pub user: Vec<(String, String)>,
    pub project: Vec<(String, String)>,
}

/// Load raw `.npmrc` key/value pairs from the same file precedence as
/// [`NpmConfig::load`]: user-level (`~/.npmrc`) first, then project-level
/// (`<cwd>/.npmrc`). Returned in encounter order — a later duplicate key
/// overrides an earlier one, matching npm's own precedence rules.
///
/// Callers that want typed, per-setting values should consume this via
/// `aube_cli::settings_values`, which walks `settings_meta::SETTINGS` and
/// looks up each setting's declared `sources.npmrc` keys. That keeps the
/// registry of "which keys map to which setting" in `settings.toml`
/// instead of scattering it through a hand-rolled parser.
pub fn load_npmrc_entries(project_dir: &Path) -> Vec<(String, String)> {
    // Process-wide memoization keyed by project_dir. `.npmrc` files are
    // not expected to change mid-install, and callers on the hot path
    // (main startup, `with_settings_ctx`, install::run) invoke this
    // repeatedly with the same path. Same pattern as
    // `aube_lockfile::aube_lock_filename`.
    use std::sync::{Mutex, OnceLock};
    type CacheMap = std::collections::HashMap<PathBuf, Vec<(String, String)>>;
    static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(project_dir)
    {
        return hit.clone();
    }
    // Read `XDG_CONFIG_HOME` only on the public entry point so that
    // `pnpm` and `aube` agree on where `~/.config/pnpm/auth.ini`
    // resolves when the user has a non-default XDG layout. The env
    // read is confined here — the `_with_home` helper keeps taking an
    // explicit override so tests don't inherit the developer's real
    // `XDG_CONFIG_HOME` and pick up whatever auth tokens live there.
    let xdg = aube_util::env::xdg_config_home();
    let home = home_dir();
    // `NPM_CONFIG_USERCONFIG` / `npm_config_userconfig` relocate the
    // user-level `.npmrc` (XDG layouts, `~/.config/npm/npmrc`, etc.).
    // Read directly rather than collecting `std::env::vars()` — we
    // only need these two keys, and confining the env read to the
    // public entry point keeps `_with_home` fully injectable for
    // tests.
    let user_rc_override = std::env::var("NPM_CONFIG_USERCONFIG")
        .ok()
        .or_else(|| std::env::var("npm_config_userconfig").ok())
        .and_then(|raw| expand_userconfig_path(&raw, home.as_deref()));
    let entries = load_npmrc_entries_with_home(
        home.as_deref(),
        xdg.as_deref(),
        project_dir,
        user_rc_override.as_deref(),
    );
    if let Ok(mut map) = cache.lock() {
        map.insert(project_dir.to_path_buf(), entries.clone());
    }
    entries
}

/// Same as [`load_npmrc_entries_with_home`] but each entry is tagged
/// with the file it came from. `apply_tagged` uses the tag to refuse
/// high-privilege settings (currently `tokenHelper`) that originated
/// from a project-scope `.npmrc` a hostile repo can commit.
pub(super) fn load_npmrc_entries_tagged_with_home(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    project_dir: &Path,
    user_rc_override: Option<&Path>,
) -> Vec<(NpmrcSource, String, String)> {
    let mut out: Vec<(NpmrcSource, String, String)> = Vec::new();
    // User-level rc: explicit override (from `NPM_CONFIG_USERCONFIG`)
    // wins over `$HOME/.npmrc`. Keeps the `User` source tag either
    // way — the user chose the file location, so `apply_tagged`'s
    // trust level is unchanged. The pnpm `auth.ini` is a separate
    // file under `$HOME`/`XDG_CONFIG_HOME` and is not affected by
    // the userconfig override.
    let user_rc = user_rc_override
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".npmrc")));
    if let Some(user_rc) = user_rc
        && user_rc.exists()
        && let Ok(entries) = parse_npmrc(&user_rc)
    {
        out.extend(entries.into_iter().map(|(k, v)| (NpmrcSource::User, k, v)));
    }
    if let Some(home) = home {
        let auth_ini = pnpm_global_auth_ini_path(home, xdg_config_home);
        if auth_ini.exists()
            && let Ok(entries) = parse_npmrc(&auth_ini)
        {
            out.extend(
                entries
                    .into_iter()
                    .map(|(k, v)| (NpmrcSource::PnpmAuth, k, v)),
            );
        }
    }
    let project_rc = project_dir.join(".npmrc");
    if project_rc.exists()
        && let Ok(entries) = parse_npmrc_untrusted(&project_rc)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::Project, k, v)),
        );
    }
    // Resolve `npmrc-auth-file` by borrowing the tagged entries we
    // already parsed. No clone, the iterator just drops the tag.
    if let Some(auth_path) = resolve_npmrc_auth_file(
        home,
        project_dir,
        out.iter().map(|(_, k, v)| (k.as_str(), v.as_str())),
    ) && auth_path.exists()
        && let Ok(entries) = parse_npmrc_untrusted(&auth_path)
    {
        out.extend(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::NpmrcAuthFile, k, v)),
        );
    }
    out
}
/// Same as [`load_npmrc_entries`] but with an injectable user-home
/// directory and XDG config-home override. Used by tests that need to
/// isolate from the developer's real `~/.npmrc` and pnpm config dir
/// without mutating process-wide environment variables.
pub(super) fn load_npmrc_entries_with_home(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    project_dir: &Path,
    user_rc_override: Option<&Path>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // User-level rc: explicit override (from `NPM_CONFIG_USERCONFIG`)
    // wins over `$HOME/.npmrc`. When the override is set, the default
    // path is skipped entirely — matching npm/pnpm, which treat the
    // env var as "this is where the user rc lives," not "also read
    // this file on top of the default."
    let user_rc = user_rc_override
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".npmrc")));
    if let Some(user_rc) = user_rc
        && user_rc.exists()
        && let Ok(entries) = parse_npmrc(&user_rc)
    {
        out.extend(entries);
    }
    if let Some(home) = home {
        // pnpm's global auth file: `~/.config/pnpm/auth.ini`. Same
        // `key=value` grammar as `.npmrc`, but lives under the pnpm
        // config dir so a user can keep registry credentials out of
        // `~/.npmrc` (which tooling like `npm login` rewrites). Loaded
        // after the user rc so it overrides any stale token there but
        // before the project rc, which still wins for per-repo pins.
        let auth_ini = pnpm_global_auth_ini_path(home, xdg_config_home);
        if auth_ini.exists()
            && let Ok(entries) = parse_npmrc(&auth_ini)
        {
            out.extend(entries);
        }
    }
    let project_rc = project_dir.join(".npmrc");
    if project_rc.exists()
        && let Ok(entries) = parse_npmrc_untrusted(&project_rc)
    {
        out.extend(entries);
    }
    // pnpm's `npmrcAuthFile` setting points at an out-of-tree file
    // (typically a CI secret mount or a per-user override) that holds
    // auth tokens. Load it last so anything declared there wins —
    // users who put auth tokens in this file expect them to take
    // precedence over whatever happens to be in `~/.npmrc`.
    if let Some(auth_path) = resolve_npmrc_auth_file(
        home,
        project_dir,
        out.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    ) && auth_path.exists()
        && let Ok(entries) = parse_npmrc_untrusted(&auth_path)
    {
        out.extend(entries);
    }
    out
}

/// Walk the loaded `.npmrc` entries (last-write-wins) for an
/// `npmrcAuthFile` / `npmrc-auth-file` key and resolve it to an
/// absolute path. `~` expands against `home`; relative paths resolve
/// against the project root, matching the storeDir convention.
pub(super) fn resolve_npmrc_auth_file<'a, I>(
    home: Option<&Path>,
    project_dir: &Path,
    entries: I,
) -> Option<PathBuf>
where
    I: DoubleEndedIterator<Item = (&'a str, &'a str)>,
{
    let raw = entries
        .rev()
        .find(|(k, _)| matches!(*k, "npmrcAuthFile" | "npmrc-auth-file"))
        .map(|(_, v)| v)?;
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        home.map(|h| h.join(rest))?
    } else if raw == "~" {
        home.map(PathBuf::from)?
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(project_dir.join(expanded))
    }
}

/// Expand a raw `userconfig` / `NPM_CONFIG_USERCONFIG` value into a
/// concrete path, applying the same tilde-expansion rules
/// [`resolve_npmrc_auth_file`] uses so both env-var and `.npmrc`-derived
/// path overrides behave the same way. Empty (after trim) returns
/// `None` so callers can skip a pointless file probe. Relative paths
/// are returned verbatim and resolve against the process cwd when
/// later fed to `exists()` / `parse_npmrc` — matching npm's behavior.
pub(super) fn expand_userconfig_path(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    if trimmed == "~" {
        return home.map(PathBuf::from);
    }
    Some(PathBuf::from(trimmed))
}

/// Find the `NPM_CONFIG_USERCONFIG` / `npm_config_userconfig` value
/// in a captured env slice and expand it. npm/pnpm accept both
/// casings; the SCREAMING form is canonical so it wins when both are
/// set. Positional ordering can't be the tiebreaker — the typical
/// caller builds the slice from `std::env::vars()`, which iterates
/// in HashMap order — so we pick explicitly by casing instead. This
/// keeps [`NpmConfig::load_with_env`] agreeing with the direct
/// `std::env::var` chain in [`load_npmrc_entries`], so generic
/// settings and auth config can't resolve to different files on the
/// same host.
pub(super) fn userconfig_override_from_env(
    env: &[(String, String)],
    home: Option<&Path>,
) -> Option<PathBuf> {
    let raw = env
        .iter()
        .find(|(name, _)| name == "NPM_CONFIG_USERCONFIG")
        .or_else(|| env.iter().find(|(name, _)| name == "npm_config_userconfig"))?;
    expand_userconfig_path(&raw.1, home)
}
pub(super) fn home_dir() -> Option<PathBuf> {
    aube_util::env::home_dir()
}

/// Resolve the path to pnpm's global auth file. When an explicit
/// `xdg_config_home` is supplied (production reads it from
/// `$XDG_CONFIG_HOME` in [`load_npmrc_entries`]; tests pass an
/// injected override or `None`), the file lives at
/// `<xdg>/pnpm/auth.ini`. Otherwise it falls back to
/// `<home>/.config/pnpm/auth.ini`, matching pnpm's default layout
/// on Linux and the README's documented path.
fn pnpm_global_auth_ini_path(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    let config_root = xdg_config_home
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    config_root.join("pnpm").join("auth.ini")
}
