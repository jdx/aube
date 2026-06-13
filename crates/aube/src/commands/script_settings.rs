use std::path::Path;

use miette::{Context, IntoDiagnostic};

use super::FileSources;

pub(crate) fn configure_script_settings(
    ctx: &aube_settings::ResolveCtx<'_>,
    command: Option<&str>,
) {
    let node_options = aube_settings::resolved::node_options(ctx).and_then(non_empty_string);
    let script_shell = aube_settings::resolved::script_shell(ctx)
        .and_then(|s| non_empty_string(s).map(Into::into));
    let unsafe_perm = aube_settings::resolved::unsafe_perm(ctx);
    let shell_emulator = aube_settings::resolved::shell_emulator(ctx);
    // Runtime switching: `crate::runtime::ensure` must have run before
    // this for lifecycle scripts to see the pinned node — the install
    // driver resolves the runtime early, then configures script
    // settings. When no context exists (or no switching is active)
    // `node_bin_dir` stays `None` (PATH untouched); `node_exe` still
    // falls back to the ambient `node` on PATH so `npm_node_execpath` /
    // `NODE` are populated for lifecycle scripts the way pnpm/npm do.
    let runtime = crate::runtime::current();
    aube_scripts::set_script_settings(aube_scripts::ScriptSettings {
        node_options,
        script_shell,
        unsafe_perm,
        shell_emulator,
        node_bin_dir: runtime.and_then(|r| r.bin_dir.clone()),
        node_exe: runtime
            .and_then(|r| r.node_bin.clone())
            .or_else(aube_runtime::node_on_path),
        command: command.map(str::to_string),
        // `npm_config_node_gyp` parity: hand every lifecycle script a
        // runnable node-gyp stand-in. The shim is written once into
        // aube's cache; a write failure here is non-fatal (the var just
        // stays unset, matching pre-parity behavior).
        node_gyp_js: super::install::node_gyp_bootstrap::lazy_js_shim_path().ok(),
    });
}

/// Load `.npmrc` + workspace settings for `cwd` and push them into the
/// process-wide script settings snapshot. Used by commands that run
/// lifecycle hooks (pack/publish/version) outside the install path,
/// which already does this via `configure_script_settings` directly.
/// `command` is the npm-compat command label exported as
/// `npm_command` (e.g. `"pack"`).
pub(crate) fn configure_script_settings_for_cwd(
    cwd: &Path,
    command: Option<&str>,
) -> miette::Result<()> {
    let files = FileSources::load(cwd);
    let (_, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let ctx = files.ctx(&raw_workspace, &env_snapshot, &[]);
    configure_script_settings(&ctx, command);
    Ok(())
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
