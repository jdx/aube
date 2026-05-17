use std::path::Path;

use miette::{Context, IntoDiagnostic};

use super::settings_context::FileSources;

pub(crate) fn configure_script_settings(ctx: &aube_settings::ResolveCtx<'_>) {
    let node_options = aube_settings::resolved::node_options(ctx).and_then(non_empty_string);
    let script_shell = aube_settings::resolved::script_shell(ctx)
        .and_then(|s| non_empty_string(s).map(Into::into));
    let unsafe_perm = aube_settings::resolved::unsafe_perm(ctx);
    let shell_emulator = aube_settings::resolved::shell_emulator(ctx);
    aube_scripts::set_script_settings(aube_scripts::ScriptSettings {
        node_options,
        script_shell,
        unsafe_perm,
        shell_emulator,
    });
}

/// Load `.npmrc` + workspace settings for `cwd` and push them into the
/// process-wide script settings snapshot. Used by commands that run
/// lifecycle hooks (pack/publish/version) outside the install path,
/// which already does this via `configure_script_settings` directly.
pub(crate) fn configure_script_settings_for_cwd(cwd: &Path) -> miette::Result<()> {
    let files = FileSources::load(cwd);
    let (_, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let ctx = files.ctx(&raw_workspace, &env_snapshot, &[]);
    configure_script_settings(&ctx);
    Ok(())
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
