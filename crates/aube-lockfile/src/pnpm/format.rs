/// Post-process a `yaml_serde`-emitted pnpm-lock.yaml into the exact
/// shape real pnpm writes. Two tweaks:
///
///   1. Collapse `resolution:` / `engines:` block maps into flow form
///      (`resolution: {integrity: sha512-…}`). pnpm writes both inline
///      and `yaml_serde` can't be coerced into flow style per-field
///      without a custom emitter.
///   2. Insert blank-line separators above every top-level section
///      (`settings:`, `importers:`, `packages:`, `snapshots:`, …) and
///      between 2-indent entries inside the entry-bearing sections
///      (`importers:`, `packages:`, `snapshots:`, `catalogs:`).
///
/// The rewrites are textual — not YAML-aware — but the keys aube emits
/// are all simple scalars in the fixed set above, so there's nothing to
/// quote-escape. Validated by `test_write_byte_identical_to_native_pnpm`.
pub(super) fn reformat_for_pnpm_parity(yaml: &str) -> String {
    let lines: Vec<&str> = yaml.lines().collect();

    // Pass 1: flow-style `resolution:` / `engines:` blocks.
    let mut compact: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        let key = stripped.strip_suffix(':');
        let is_flow_candidate = matches!(key, Some("resolution") | Some("engines"));
        if is_flow_candidate && i + 1 < lines.len() {
            let inner_indent = indent + 2;
            let mut entries: Vec<String> = Vec::new();
            let mut all_scalar = true;
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j];
                let n_stripped = next.trim_start();
                let n_indent = next.len() - n_stripped.len();
                if n_stripped.is_empty() || n_indent < inner_indent {
                    break;
                }
                if n_indent > inner_indent {
                    // Nested structure (e.g. a `variants:` list inside
                    // a `type: variations` resolution) — flow form
                    // can't represent it with this rewriter; keep the
                    // whole block as-is.
                    all_scalar = false;
                    break;
                }
                match n_stripped.split_once(": ") {
                    Some((k, v)) => entries.push(format!("{k}: {v}")),
                    None => {
                        // A key with no inline value (`variants:`)
                        // introduces a nested block — leave it alone.
                        all_scalar = false;
                        break;
                    }
                }
                j += 1;
            }
            // pnpm renders `binary` / `variations` resolutions in
            // block form even when (like a map-less binary) every
            // field happens to be scalar — match that.
            let block_form_type = entries
                .iter()
                .any(|e| e == "type: binary" || e == "type: variations");
            if all_scalar && !block_form_type && !entries.is_empty() {
                compact.push(format!(
                    "{}{}: {{{}}}",
                    " ".repeat(indent),
                    key.unwrap(),
                    entries.join(", ")
                ));
                i = j;
                continue;
            }
        }
        compact.push(line.to_string());
        i += 1;
    }

    // Pass 2: blank-line separators.
    // Sections where each 2-indent key-ending-in-`:` is an entry header
    // that pnpm separates with a blank line above. `overrides:` /
    // `time:` / `settings:` carry scalar key→value pairs instead and
    // stay tight.
    const ENTRY_SECTIONS: &[&str] = &["importers:", "packages:", "snapshots:", "catalogs:"];
    let mut out = String::with_capacity(yaml.len() + 512);
    let mut in_entries = false;
    for (idx, line) in compact.iter().enumerate() {
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        let is_top = indent == 0 && !stripped.is_empty();
        // Entry headers inside `packages:` / `snapshots:` are always at
        // 2-indent with a `:` in the line. Either trailing (`foo@1:`
        // with a child block below) or inline (`foo@1: {}` for empty
        // snapshots). List markers (`- …`) never appear at this level,
        // so a leading `-` rules out false positives on
        // `ignoredOptionalDependencies:` items.
        let is_entry_header =
            in_entries && indent == 2 && !stripped.starts_with('-') && stripped.contains(':');

        if (is_top && idx > 0) || is_entry_header {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');

        if is_top {
            in_entries = ENTRY_SECTIONS.contains(&stripped);
        }
    }
    out
}
