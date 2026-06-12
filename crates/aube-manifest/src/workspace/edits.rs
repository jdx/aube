//! Generic yaml/json edit helpers for workspace-level config.
//!
//! `package.json#pnpm.<key>` mutations (`remove_setting_entry`,
//! `edit_setting_map`), workspace-yaml round-trip editing
//! (`edit_workspace_yaml`, `workspace_yaml_submap`,
//! `write_workspace_yaml`), and the `upsert_map_entry` /
//! `remove_map_entry` pair that routes through `config_write_target`
//! to mutate whichever file holds the value today.

use super::config::{ConfigWriteTarget, config_write_target, workspace_yaml_existing};
use super::yaml_patch;
use std::path::{Path, PathBuf};

/// The manifest config namespaces, compatible first (lower precedence),
/// this tool's own namespace last (wins on conflict). Standalone aube:
/// `["pnpm", "aube"]`.
fn config_namespaces() -> Vec<&'static str> {
    let id = aube_util::embedder();
    let mut ns: Vec<&'static str> = id.compatible_names.to_vec();
    if !id.manifest_namespace.is_empty() {
        ns.push(id.manifest_namespace);
    }
    ns
}

/// Drop `entry_key` from `pnpm.<key>` and `aube.<key>` in
/// `package.json`. Returns `Ok(true)` when at least one namespace held
/// it. Empty inner maps and empty namespaces are scrubbed too. The
/// rewrite is skipped entirely when nothing structural changes —
/// mirrors the no-op-skip guarantee of [`edit_workspace_yaml`].
///
/// Walking both namespaces matters because the read side merges them
/// (`aube.*` wins on conflict), so an entry recorded in either
/// location is live; a one-namespace removal would leave a stale
/// duplicate behind.
pub fn remove_setting_entry(cwd: &Path, key: &str, entry_key: &str) -> Result<bool, crate::Error> {
    let path = cwd.join("package.json");
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
    let mut value = crate::parse_json::<serde_json::Value>(&path, raw)?;
    let obj = value.as_object_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.clone(), "package.json is not an object".to_string())
    })?;
    let before = obj.clone();

    let mut existed = false;
    for ns in config_namespaces() {
        let mut ns_empty = false;
        if let Some(ns_obj) = obj.get_mut(ns).and_then(|v| v.as_object_mut()) {
            if let Some(inner) = ns_obj.get_mut(key).and_then(|v| v.as_object_mut()) {
                if inner.remove(entry_key).is_some() {
                    existed = true;
                }
                if inner.is_empty() {
                    ns_obj.remove(key);
                }
            }
            ns_empty = ns_obj.is_empty();
        }
        if ns_empty {
            obj.remove(ns);
        }
    }

    if *obj == before {
        return Ok(existed);
    }

    let mut out = serde_json::to_string_pretty(&value)
        .map_err(|e| crate::Error::YamlParse(path.clone(), format!("failed to serialize: {e}")))?;
    out.push('\n');
    std::fs::write(&path, out).map_err(|e| crate::Error::Io(path, e))?;
    Ok(existed)
}

/// Mutate a namespaced map setting (e.g. `patchedDependencies`,
/// `allowBuilds`) inside `package.json` and write back.
///
/// The closure receives a **merged** view of `pnpm.<key>` and
/// `aube.<key>`, with `aube.*` winning on key conflict — the same
/// precedence the read side already uses. After the closure runs,
/// the merged result is written to a single namespace and the other
/// is cleared, so a future read sees exactly one source of truth and
/// can never silently shadow a stale entry. This matters because
/// pnpm-aware tools (and pnpm itself) can introduce a `pnpm` key into
/// a manifest after aube has already populated `aube.<key>`; without
/// the merge-and-collapse, a re-record would leave the new value in
/// `pnpm.<key>` while the stale `aube.<key>` entry kept winning on
/// read.
///
/// The chosen namespace follows [`config_write_target`]'s rule:
/// `pnpm` if a `pnpm` namespace is already declared in the manifest,
/// `aube` otherwise. Empty namespaces and inner maps are scrubbed,
/// and the rewrite is skipped entirely when nothing structural
/// changes — mirrors the no-op-skip guarantee of [`edit_workspace_yaml`].
pub fn edit_setting_map<F>(cwd: &Path, key: &str, f: F) -> Result<(), crate::Error>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>),
{
    let path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
    let mut value = crate::parse_json::<serde_json::Value>(&path, raw)?;

    let obj = value.as_object_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.clone(), "package.json is not an object".to_string())
    })?;
    let before = obj.clone();

    // Build the merged view (pnpm first, aube overrides on conflict)
    // before mutating, so the closure sees the same map the install
    // path would.
    let mut merged: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for ns in config_namespaces() {
        if let Some(inner) = obj
            .get(ns)
            .and_then(serde_json::Value::as_object)
            .and_then(|m| m.get(key))
            .and_then(serde_json::Value::as_object)
        {
            for (k, v) in inner {
                merged.insert(k.clone(), v.clone());
            }
        }
    }

    f(&mut merged);

    // Write to a compatible namespace if the manifest already declares
    // one (`pnpm` for standalone aube), else this tool's own namespace.
    let id = aube_util::embedder();
    let chosen_ns = id
        .compatible_names
        .iter()
        .copied()
        .find(|ns| obj.contains_key(*ns))
        .unwrap_or(id.manifest_namespace);
    let other_ns = if chosen_ns == id.manifest_namespace {
        id.compatible_names.first().copied().unwrap_or("")
    } else {
        id.manifest_namespace
    };

    // Drop `<key>` from the other namespace so the post-write state
    // has one source of truth.
    let mut other_ns_empty_after = false;
    if let Some(other_obj) = obj.get_mut(other_ns).and_then(|v| v.as_object_mut()) {
        other_obj.remove(key);
        other_ns_empty_after = other_obj.is_empty();
    }
    if other_ns_empty_after {
        obj.remove(other_ns);
    }

    // Write merged into the chosen namespace, or scrub it if empty.
    if merged.is_empty() {
        let mut chosen_ns_empty_after = false;
        if let Some(chosen_obj) = obj.get_mut(chosen_ns).and_then(|v| v.as_object_mut()) {
            chosen_obj.remove(key);
            chosen_ns_empty_after = chosen_obj.is_empty();
        }
        if chosen_ns_empty_after {
            obj.remove(chosen_ns);
        }
    } else {
        let chosen_value = obj
            .entry(chosen_ns.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let chosen_obj = chosen_value.as_object_mut().ok_or_else(|| {
            crate::Error::YamlParse(path.clone(), format!("`{chosen_ns}` is not an object"))
        })?;
        chosen_obj.insert(key.to_string(), serde_json::Value::Object(merged));
    }

    if *obj == before {
        return Ok(());
    }

    let mut out = serde_json::to_string_pretty(&value)
        .map_err(|e| crate::Error::YamlParse(path.clone(), format!("failed to serialize: {e}")))?;
    out.push('\n');
    std::fs::write(&path, out).map_err(|e| crate::Error::Io(path, e))?;
    Ok(())
}

/// Upsert a single `<map>.<entry>` pair into the project's
/// workspace-level config. Routes through [`config_write_target`]:
/// workspace yaml when one exists, otherwise `<pnpm|aube>.<map>` in
/// `package.json`. Returns the file that was written.
///
/// Used by `aube config set --local <map>.<entry> <value>` for any
/// object-typed aube setting (`allowBuilds`, `overrides`,
/// `packageExtensions`, …) so the dotted-key CLI syntax can write
/// directly into the same maps `aube approve-builds` /
/// install-time auto-deny seeding mutate. The value is passed in
/// both yaml and json forms so the caller can choose the right scalar
/// shape (bool vs string vs int) without this helper having to guess.
pub fn upsert_map_entry(
    project_dir: &Path,
    map_name: &str,
    entry_key: &str,
    yaml_value: yaml_serde::Value,
    json_value: serde_json::Value,
) -> Result<PathBuf, crate::Error> {
    match config_write_target(project_dir) {
        ConfigWriteTarget::WorkspaceYaml(path) => {
            edit_workspace_yaml(&path, |map| {
                let submap = workspace_yaml_submap(map, map_name, &path)?;
                submap.insert(yaml_serde::Value::String(entry_key.to_string()), yaml_value);
                Ok(())
            })?;
            Ok(path)
        }
        ConfigWriteTarget::PackageJson => {
            edit_setting_map(project_dir, map_name, |map| {
                map.insert(entry_key.to_string(), json_value);
            })?;
            Ok(project_dir.join("package.json"))
        }
    }
}

/// Remove a single `<map>.<entry>` pair from the project's
/// workspace-level config. Mirrors [`upsert_map_entry`]: sweeps both
/// the workspace yaml (when one exists) and
/// `<pnpm|aube>.<map>.<entry>` in `package.json` so a value set
/// through either file can be deleted regardless of which one the
/// current layout would have written to. Drops empty `<map>:`
/// containers behind it so a removal doesn't leave a `{}` stub.
///
/// Returns `true` when at least one location held the entry. Used by
/// `aube config delete --local <map>.<entry>` so dotted writes have
/// a symmetric round-trip.
pub fn remove_map_entry(
    project_dir: &Path,
    map_name: &str,
    entry_key: &str,
) -> Result<bool, crate::Error> {
    let mut existed = false;
    if let Some(yaml_path) = workspace_yaml_existing(project_dir) {
        edit_workspace_yaml(&yaml_path, |map| {
            let yaml_key = yaml_serde::Value::String(map_name.to_string());
            let Some(submap) = map.get_mut(&yaml_key).and_then(|v| v.as_mapping_mut()) else {
                return Ok(());
            };
            if submap.shift_remove(entry_key).is_some() {
                existed = true;
            }
            if submap.is_empty() {
                map.shift_remove(&yaml_key);
            }
            Ok(())
        })?;
    }
    if remove_setting_entry(project_dir, map_name, entry_key)? {
        existed = true;
    }
    Ok(existed)
}

/// Get the inner mapping for a top-level workspace-yaml key, creating
/// it if absent. Errors when the key exists but isn't a mapping (a
/// hand-edited file shape we shouldn't silently replace).
pub(super) fn workspace_yaml_submap<'a>(
    map: &'a mut yaml_serde::Mapping,
    key: &str,
    path: &Path,
) -> Result<&'a mut yaml_serde::Mapping, crate::Error> {
    let entry = map
        .entry(yaml_serde::Value::String(key.to_string()))
        .or_insert_with(|| yaml_serde::Value::Mapping(yaml_serde::Mapping::new()));
    entry.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(path.to_path_buf(), format!("`{key}` must be a mapping"))
    })
}

/// Apply `f` to the parsed top-level mapping of the workspace yaml at
/// `path` and write it back. The helper exists so every workspace-yaml
/// writer (allowBuilds, patchedDependencies, catalog cleanup, future
/// settings) shares one comment-preserving rule: **user-authored
/// comments and formatting in the file survive every edit**.
///
/// The closure mutates a parsed `yaml_serde::Mapping`. After it runs,
/// the helper diffs before-vs-after and reduces the change set to a
/// minimal sequence of `yamlpatch` operations applied directly to the
/// original source. yamlpatch is comment- and format-preserving, so
/// keys, comments, and whitespace that the closure didn't touch land
/// back on disk byte-identical. A no-op closure produces an empty
/// patch list and the file isn't rewritten at all.
///
/// For brand-new or empty files there is no source to preserve, so the
/// helper falls back to `yaml_serde::to_string` for the initial write.
pub fn edit_workspace_yaml<F>(path: &Path, f: F) -> Result<PathBuf, crate::Error>
where
    F: FnOnce(&mut yaml_serde::Mapping) -> Result<(), crate::Error>,
{
    use yaml_serde::{Mapping, Value};

    let original_source: Option<String> = if path.exists() {
        let content =
            std::fs::read_to_string(path).map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
        if content.trim().is_empty() {
            None
        } else {
            Some(content)
        }
    } else {
        None
    };

    let mut doc: Value = match original_source.as_deref() {
        Some(content) => crate::parse_yaml(path, content.to_string())?,
        None => Value::Mapping(Mapping::new()),
    };

    let map = doc.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(
            path.to_path_buf(),
            "top-level yaml must be a mapping".to_string(),
        )
    })?;

    let before = map.clone();
    f(map)?;
    if *map == before {
        return Ok(path.to_path_buf());
    }

    let after = std::mem::take(map);
    write_workspace_yaml(path, original_source.as_deref(), &before, &after)?;
    Ok(path.to_path_buf())
}

/// Persist a structural change against `path`. When `original_source`
/// is `Some`, the change is encoded as a list of `yamlpatch`
/// operations applied to the original text — comments and formatting
/// the closure didn't touch survive the round trip. When it is `None`
/// (fresh file or one that was empty), the after-state is serialized
/// directly via `yaml_serde::to_string`; there is no source to
/// preserve. Both paths atomic-write the result.
fn write_workspace_yaml(
    path: &Path,
    original_source: Option<&str>,
    before: &yaml_serde::Mapping,
    after: &yaml_serde::Mapping,
) -> Result<(), crate::Error> {
    let bytes: Vec<u8> = match original_source {
        Some(source) => yaml_patch::apply_diff(path, source, before, after)?,
        None => {
            let raw = yaml_serde::to_string(&yaml_serde::Value::Mapping(after.clone()))
                .map_err(|e| crate::Error::YamlParse(path.to_path_buf(), e.to_string()))?;
            indent_block_sequences(&raw).into_bytes()
        }
    };
    aube_util::fs_atomic::atomic_write(path, &bytes)
        .map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
    Ok(())
}

/// Bump every block-sequence item line (`- ...`) by two spaces. Leaves
/// already-indented lines and non-sequence lines alone. yaml_serde's
/// output uses a single indent step per nesting level, so this produces
/// the `parent:\n  - item` shape humans expect. Only used on the
/// fresh-file write path; yamlpatch preserves the user's existing
/// indentation otherwise.
fn indent_block_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for line in input.split_inclusive('\n') {
        let stripped = line.trim_start_matches(' ');
        if stripped.starts_with("- ") || stripped == "-\n" || stripped == "-" {
            out.push_str("  ");
        }
        out.push_str(line);
    }
    out
}
