//! Compile-time embedder profile — the binary's identity and embedder-fixed
//! behavior, centralized.
//!
//! aube hardcodes its own name, version, lockfile filename, cache namespace,
//! env-var prefix, and so on across many crates, and bakes in a handful of
//! behavior choices that an embedding host would want to flip. [`Embedder`]
//! gathers those *embedder-fixed* values — branding plus the behavior toggles
//! that are the host's to set, not the user's — into one place, so the
//! binary's identity is selected once, at the entry point, instead of being
//! scattered as literals and policy checks. Standalone aube ships [`AUBE`],
//! which reproduces every value verbatim, and consumers read it through
//! [`embedder`].
//!
//! This struct holds *embedder-fixed* data: branding (pure naming constants)
//! plus the four behavior toggles that an embedder — not an end user — owns
//! (`canonical_lockfile_always_wins`, `runtime_switching`, `self_engines_check`,
//! `self_update_enabled`). Genuinely *user-tunable* knobs do not belong here;
//! those stay settings.
//!
//! An embedder selects its profile by registering it with [`set_embedder`].
//! A host that goes through the library entry point `aube::cli_main` passes
//! its `&'static Embedder` there and `cli_main` registers it; a host that
//! drives the command layer in-process (`aube::commands::*::run`, bypassing
//! `cli_main` — the headline embedding use case) calls [`set_embedder`] itself
//! at startup. Internally the chosen profile is stored once in a private
//! [`OnceLock`]; [`embedder`] returns it, falling back to [`AUBE`] when nothing
//! was registered, so any caller or test that never sets one transparently
//! gets standalone-aube behavior.

use std::sync::OnceLock;

/// The binary's embedder profile — branding plus embedder-fixed behavior.
///
/// Branding fields are pure naming constants. The behavior toggles
/// (`canonical_lockfile_always_wins`, `runtime_switching`,
/// `self_engines_check`, `self_update_enabled`) are embedder-fixed, not
/// user-tunable: a host that mirrors the project's incumbent package manager,
/// owns Node provisioning, lives outside aube's version namespace, or owns its
/// own self-update flips them. Genuinely user-tunable knobs stay settings.
#[derive(Clone, Copy, Debug)]
pub struct Embedder {
    /// Tool name, lowercase (e.g. `"aube"`). The proper noun users type and
    /// see in output, and the clap command name driving help/usage/errors.
    /// Must be filesystem- and command-safe (no spaces, slashes, or shell
    /// metacharacters); it is used verbatim in on-disk sidecar paths (e.g.
    /// `.<name>_patch_state.json`, `.<name>-deploy-injected/`) and in command
    /// invocations, so the embedder is responsible for supplying a safe slug.
    pub name: &'static str,
    /// High-visibility display name shown in the progress banner (e.g.
    /// `"aube"`). Usually equal to [`name`](Self::name); split out so an
    /// embedder can brand the banner independently of the command name.
    pub display_name: &'static str,
    /// Vendor attribution rendered after the version in the progress banner,
    /// e.g. `Some("by jdx.dev")`. `None` suppresses the attribution entirely
    /// (an embedder that doesn't want a third-party vendor tag).
    pub vendor: Option<&'static str>,
    /// Version string — `env!("CARGO_PKG_VERSION")` for standalone aube.
    pub version: &'static str,
    /// HTTP `User-Agent` product token, e.g. `"aube/1.19.0"`. Sent to the
    /// registry and exported as the lifecycle `npm_config_user_agent`
    /// product.
    pub user_agent: &'static str,
    /// Names this tool recognizes as *itself* in a `packageManager` field or
    /// a lockfile-kind detection. Standalone aube: `["aube"]`.
    pub self_names: &'static [&'static str],
    /// Names accepted as compatible drop-in targets in the `packageManager`
    /// guardrail. Standalone aube: `["pnpm"]`.
    pub compatible_names: &'static [&'static str],
    /// Canonical lockfile filename, e.g. `"aube-lock.yaml"`.
    ///
    /// Invariant (checked in [`set_embedder`]): must contain a `.` (so the
    /// stem/extension split the lockfile-candidate machinery relies on holds)
    /// and must not collide with a foreign package manager's lockfile name
    /// (`pnpm-lock.yaml`, `package-lock.json`, `bun.lock`, `yarn.lock`,
    /// `npm-shrinkwrap.json`). Aliasing a foreign name would make aube's own
    /// lockfile indistinguishable from the incumbent's in the
    /// lockfile-candidate set (`io.rs` / `clean.rs` / `pack.rs`).
    pub lockfile_basename: &'static str,
    /// The *branded* workspace-config YAML this tool reads and writes, e.g.
    /// `"aube-workspace.yaml"`. `None` disables the tool's own branded YAML
    /// entirely (the shared `pnpm-workspace.yaml` compatibility surface is
    /// handled separately and is not configured here).
    pub workspace_yaml: Option<&'static str>,
    /// The `package.json` object key this tool reads its own config under,
    /// e.g. `"aube"`. `""` means this tool has *no own* branded manifest
    /// namespace: config reads fold only the
    /// [`compatible_names`](Self::compatible_names) namespaces plus any
    /// top-level (manifest-root) entry, and setting *writes* go to the
    /// manifest **root** as top-level `package.json` keys — never under a
    /// foreign brand's namespace, and never as a literal `""` key.
    pub manifest_namespace: &'static str,
    /// Env-var prefix for tool-specific variables, e.g. `Some("AUBE")` →
    /// `AUBE_*`. `None` means the tool reads no branded env family.
    pub env_prefix: Option<&'static str>,
    /// Leaf directory name under the OS cache root, e.g. `"aube"` →
    /// `<XDG_CACHE_HOME>/aube`.
    pub cache_namespace: &'static str,
    /// Leaf directory name under the OS data/state root, e.g. `"aube"`.
    pub data_namespace: &'static str,

    // --- embedder-fixed behavior toggles (not user-tunable) ---
    /// When `true` (aube's default), this tool's canonical lockfile
    /// (`lockfile_basename`) outranks any foreign lockfile present in
    /// lockfile-kind detection. An embedder that mirrors the project's
    /// incumbent package manager sets this `false` so the incumbent's
    /// lockfile wins instead. Embedder-fixed: it's the host's call, not the
    /// user's.
    pub canonical_lockfile_always_wins: bool,
    /// When `true` (aube's default), this tool resolves and switches the Node
    /// runtime from version files / devEngines and prepends it to `PATH`. An
    /// embedder that owns Node provisioning itself sets this `false`, leaving
    /// the runtime resolver inert. Embedder-fixed.
    pub runtime_switching: bool,
    /// When `true` (aube's default), this tool validates a manifest's
    /// `engines.<self>` constraint against its own version. An embedder whose
    /// version isn't in aube's version namespace sets this `false` to avoid
    /// spurious `engines.aube` mismatches. The `engines.node` check is
    /// unaffected. Embedder-fixed.
    pub self_engines_check: bool,
    /// When `true` (aube's default), this tool owns its own self-update:
    /// the update notifier (and its `aube.jdx.dev` endpoints) runs. An
    /// embedder that owns its own upgrade path sets this `false` so those
    /// code paths never run. Embedder-fixed.
    pub self_update_enabled: bool,
}

/// Standalone aube's embedder profile. Reproduces every hardcoded branding
/// constant and behavior default verbatim; this is the fallback whenever no
/// profile is registered.
pub const AUBE: Embedder = Embedder {
    name: "aube",
    display_name: "aube",
    vendor: Some("by jdx.dev"),
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("aube/", env!("CARGO_PKG_VERSION")),
    self_names: &["aube"],
    compatible_names: &["pnpm"],
    lockfile_basename: "aube-lock.yaml",
    workspace_yaml: Some("aube-workspace.yaml"),
    manifest_namespace: "aube",
    env_prefix: Some("AUBE"),
    cache_namespace: "aube",
    data_namespace: "aube",
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
};

static ACTIVE: OnceLock<&'static Embedder> = OnceLock::new();

/// Register the active embedder profile.
///
/// Call this **once at startup**, before invoking any `aube::commands`
/// directly. `aube::cli_main` calls it for you, so binaries that go through
/// `cli_main` don't need to; embedders that drive the command layer in-process
/// — calling `aube::commands::*::run` directly, bypassing `cli_main` (the
/// headline embedding use case) — call it themselves to register their
/// profile before the first command runs.
///
/// Set-once / first-wins: the first registration is the active profile for the
/// process; later calls are silently ignored. A process that never registers
/// one transparently gets standalone-aube behavior ([`AUBE`]) — which is also
/// why tests that don't register a profile see `AUBE`.
///
/// Validates the profile's lockfile invariant in debug builds: a profile whose
/// `lockfile_basename` has no extension or aliases a foreign package manager's
/// lockfile would silently corrupt the lockfile-candidate set, so it trips a
/// `debug_assert!` here — at registration, the single choke point — rather than
/// misbehaving deep inside `io.rs` / `clean.rs` / `pack.rs`.
pub fn set_embedder(embedder: &'static Embedder) {
    debug_assert!(
        embedder.lockfile_basename.contains('.'),
        "embedder lockfile_basename {:?} must contain a `.` (stem/extension split is load-bearing)",
        embedder.lockfile_basename,
    );
    debug_assert!(
        !FOREIGN_LOCKFILE_NAMES.contains(&embedder.lockfile_basename),
        "embedder lockfile_basename {:?} aliases a foreign package manager's lockfile; \
         pick a distinct name so aube's lockfile stays distinguishable in the candidate set",
        embedder.lockfile_basename,
    );
    let _ = ACTIVE.set(embedder);
}

/// Foreign package-manager lockfile names an embedder's `lockfile_basename`
/// must not alias. Aliasing one would make aube's own lockfile collide with
/// the incumbent's in the lockfile-candidate machinery.
const FOREIGN_LOCKFILE_NAMES: &[&str] = &[
    "pnpm-lock.yaml",
    "package-lock.json",
    "bun.lock",
    "yarn.lock",
    "npm-shrinkwrap.json",
];

/// The active embedder profile, or [`AUBE`] when none was registered. Never
/// panics: an unset profile transparently yields standalone-aube behavior.
pub fn embedder() -> &'static Embedder {
    ACTIVE.get().copied().unwrap_or(&AUBE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no profile registered, `embedder()` is `AUBE` and every field
    /// reproduces aube's standalone branding and behavior defaults verbatim.
    /// This is the behavior-neutrality contract: an embedder that sets nothing
    /// gets aube.
    ///
    /// Relies on no other test in this binary calling `set_embedder` — the
    /// `ACTIVE` `OnceLock` is process-global and first-write-wins, so a test
    /// that registers a non-aube profile would flip the fallback this asserts.
    /// Keep profile registration out of this crate's unit tests.
    #[test]
    fn embedder_unset_is_aube() {
        let id = embedder();
        assert_eq!(id.name, "aube");
        assert_eq!(id.display_name, "aube");
        assert_eq!(id.vendor, Some("by jdx.dev"));
        assert_eq!(id.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(id.user_agent, concat!("aube/", env!("CARGO_PKG_VERSION")));
        assert_eq!(id.self_names, &["aube"]);
        assert_eq!(id.compatible_names, &["pnpm"]);
        assert_eq!(id.lockfile_basename, "aube-lock.yaml");
        assert_eq!(id.workspace_yaml, Some("aube-workspace.yaml"));
        assert_eq!(id.manifest_namespace, "aube");
        assert_eq!(id.env_prefix, Some("AUBE"));
        assert_eq!(id.cache_namespace, "aube");
        assert_eq!(id.data_namespace, "aube");
        assert!(id.canonical_lockfile_always_wins);
        assert!(id.runtime_switching);
        assert!(id.self_engines_check);
        assert!(id.self_update_enabled);
    }
}
