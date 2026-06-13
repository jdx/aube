use aube_lockfile::LockfileGraph;
use aube_lockfile::dep_path_filename::dep_path_to_filename;
use std::collections::BTreeMap;
use std::path::Path;

/// A map of `name@version` -> raw multi-file unified diff text.
///
/// Keys must match the `spec_key()` value the resolver writes into
/// every `LockedPackage`. The value is the raw multi-file unified diff
/// text written by `aube patch-commit` (or any compatible tool).
pub type Patches = BTreeMap<String, String>;

/// The applied-patch sidecar filename, derived from the tool's identity:
/// `.<name>-applied-patches.json`. Standalone aube:
/// `.aube-applied-patches.json`.
pub(crate) fn applied_patches_sidecar_name() -> String {
    format!(".{}-applied-patches.json", aube_util::embedder().name)
}

pub(crate) fn current_patch_hashes(patches: &Patches) -> BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    patches
        .iter()
        .map(|(k, v)| {
            let mut h = Sha256::new();
            h.update(v.as_bytes());
            (k.clone(), hex::encode(h.finalize()))
        })
        .collect()
}

/// Read the previously-applied patch sidecar at
/// `node_modules/.aube-applied-patches.json`. Missing or malformed
/// files return an empty map — the caller treats them as "no patches
/// were ever applied here," which conservatively triggers a re-link
/// on the first run after the linker started writing the sidecar.
pub(crate) fn read_applied_patches(nm_dir: &Path) -> BTreeMap<String, String> {
    let path = nm_dir.join(applied_patches_sidecar_name());
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Default::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Write the applied-patch sidecar.
///
/// Next install reads this to compute which `.aube/<dep_path>`
/// entries need re-materializing because their patch set changed.
/// Old code was `let _ = fs::write(...)`, dropped any IO error. If
/// write silently failed (disk full, read-only mount, perms), the
/// sidecar was missing on next install, and
/// wipe_changed_patched_entries did not know which entries to
/// re-link. Install reported success while node_modules had stale
/// patched content on disk. Return Result, caller logs loudly.
pub(crate) fn write_applied_patches(
    nm_dir: &Path,
    map: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    let path = nm_dir.join(applied_patches_sidecar_name());
    let out = serde_json::to_string(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    aube_util::fs_atomic::atomic_write(&path, out.as_bytes())
}

/// Wipe `.aube/<dep_path>` for any package whose patch fingerprint
/// changed between the previous and current install. Used by the
/// per-project (no-global-store) link path, where the directory name
/// doesn't otherwise change when a patch is added or removed.
pub(crate) fn wipe_changed_patched_entries(
    aube_dir: &Path,
    graph: &LockfileGraph,
    prev: &BTreeMap<String, String>,
    curr: &BTreeMap<String, String>,
    max_length: usize,
) {
    let mut affected: std::collections::HashSet<String> = std::collections::HashSet::new();
    for k in prev.keys().chain(curr.keys()) {
        if prev.get(k) != curr.get(k) {
            affected.insert(k.clone());
        }
    }
    if affected.is_empty() {
        return;
    }
    for (dep_path, pkg) in &graph.packages {
        let key = pkg.spec_key();
        if affected.contains(&key) {
            let entry = aube_dir.join(dep_path_to_filename(dep_path, max_length));
            let _ = std::fs::remove_dir_all(entry);
        }
    }
}

/// Apply a git-style multi-file unified diff to a package directory.
///
/// The patch text is split on `diff --git ` boundaries; each section
/// is parsed as a single-file unified diff and applied to the matching
/// file under `pkg_dir`. We deliberately unlink the destination
/// before writing, because the linker materializes files via reflink
/// or hardlink — modifying the file in place would corrupt the global
/// content-addressed store the linked file points to.
fn is_safe_rel_component(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return false;
    }
    let p = Path::new(rel);
    if p.is_absolute()
        || p.has_root()
        || rel.starts_with('/')
        || rel.len() >= 2 && rel.as_bytes()[1] == b':'
    {
        return false;
    }
    p.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

fn ensure_no_symlink_in_chain(pkg_dir: &Path, rel: &str) -> Result<(), String> {
    let mut cursor = pkg_dir.to_path_buf();
    for comp in Path::new(rel).components() {
        cursor.push(comp);
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(format!("{}", cursor.display()));
                }
                // Junctions on Windows are `IO_REPARSE_TAG_MOUNT_POINT`
                // reparse points, not `IO_REPARSE_TAG_SYMLINK`, and
                // `FileType::is_symlink()` returns false for them.
                // Catch every reparse point via the file-attribute
                // bit so a junction can't sneak the patch out of the
                // package directory.
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        return Err(format!("{}", cursor.display()));
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("stat {}: {e}", cursor.display())),
        }
    }
    Ok(())
}

pub(crate) fn apply_multi_file_patch(pkg_dir: &Path, patch_text: &str) -> Result<(), String> {
    let sections = split_patch_sections(patch_text);
    if sections.is_empty() {
        return Err("patch contained no `diff --git` sections".to_string());
    }
    for section in sections {
        let rel = section
            .rel_path
            .as_ref()
            .ok_or_else(|| "patch section missing file path".to_string())?;
        // Refuse patch headers that escape the package directory.
        // A hostile diff with `b/../../etc/shadow` as the target
        // would otherwise let the patch step overwrite or delete
        // files outside the installed package. Same rules we apply
        // to tar entries over in aube-store (no absolute, no drive
        // prefix, no `..`, no backslash, no NUL).
        if !is_safe_rel_component(rel) {
            return Err(format!("patch file path escapes package: {rel:?}"));
        }
        // Walk every parent component of the target on disk and refuse
        // to follow any symlink or junction. Without this guard, a
        // package that planted a directory link inside its own tree
        // (or a workspace where the user has a symlinked dep dir)
        // would let `pkg_dir.join(rel)` resolve through the link, and
        // `atomic_write` would overwrite a file outside `pkg_dir`.
        // CVE-2018-1000156 (GNU patch) class.
        if let Err(e) = ensure_no_symlink_in_chain(pkg_dir, rel) {
            return Err(format!("patch target contains symlink: {e}"));
        }
        let target = pkg_dir.join(rel);
        let original = if target.exists() {
            std::fs::read_to_string(&target)
                .map_err(|e| format!("failed to read {}: {e}", target.display()))?
        } else {
            String::new()
        };
        // `+++ /dev/null` means the patch deletes the file. Skip diffy
        // entirely — `diffy::apply` would otherwise produce an empty
        // string and we'd write a zero-byte file in place of the
        // original, leaving `require('./removed')` resolving to an
        // empty module instead of the expected `MODULE_NOT_FOUND`.
        if section.is_deletion {
            if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            }
            continue;
        }
        // git-style patches always use LF line endings, but published
        // tarballs frequently ship files with CRLF (Windows editors,
        // `core.autocrlf=true` checkouts). Diffy is byte-exact and
        // refuses to match CRLF context against LF hunk lines, so we
        // normalize the original to LF before applying and restore the
        // CRLF on write. pnpm's patch applier does the same thing.
        let was_crlf = original.contains("\r\n");
        let normalized = if was_crlf {
            original.replace("\r\n", "\n")
        } else {
            original
        };
        let parsed = diffy::Patch::from_str(&section.body)
            .map_err(|e| format!("failed to parse patch for {rel}: {e}"))?;
        let patched_lf = diffy::apply(&normalized, &parsed)
            .map_err(|e| format!("failed to apply patch for {rel}: {e}"))?;
        let patched = if was_crlf {
            // Promote bare `\n` to `\r\n`, then collapse any `\r\r\n`
            // back so a patch line containing a literal `\r` byte (rare
            // but legal for binary-ish text) doesn't gain a second CR.
            patched_lf.replace('\n', "\r\n").replace("\r\r\n", "\r\n")
        } else {
            patched_lf
        };
        // Break any reflink/hardlink to the global store before
        // writing the patched bytes — otherwise we'd silently mutate
        // every other project sharing this CAS file. Stage the write
        // through a sibling tempfile and `rename` into place so a
        // crash or Ctrl-C mid-patch cannot leave the package with
        // the original file unlinked and no replacement written.
        // POSIX `rename(2)` atomically replaces the destination, so
        // no pre-removal is needed and removing first would create
        // the exact TOCTOU window the rename is supposed to close.
        // Windows `MoveFileExW` fails when the destination exists,
        // so the unlink is gated behind `cfg(windows)`.
        #[cfg(windows)]
        {
            if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|e| format!("failed to unlink {}: {e}", target.display()))?;
            }
        }
        aube_util::fs_atomic::atomic_write(&target, patched.as_bytes()).map_err(|e| {
            format!(
                "failed to write patched file into place {}: {e}",
                target.display()
            )
        })?;
    }
    Ok(())
}

struct PatchSection {
    rel_path: Option<String>,
    /// Single-file unified diff body — `diffy::Patch::from_str` parses
    /// this directly. Always begins with `--- ` so the diffy parser
    /// finds its anchor.
    body: String,
    /// `+++ /dev/null` was seen in the header — the patch deletes this
    /// file, so the linker should `remove_file` instead of writing
    /// patched bytes (which `diffy::apply` would emit as an empty
    /// string).
    is_deletion: bool,
}

/// Split a git-style multi-file patch into one section per file.
/// We look for `diff --git a/<path> b/<path>` markers, pull the path
/// out of the `b/...` half (post-edit name), and capture everything
/// from the next `--- ` line until the following `diff --git ` (or
/// EOF) as the diffy-compatible body.
fn parse_diff_git_b_path(rest: &str) -> Option<String> {
    if let Some(after) = rest.strip_prefix("\"a/") {
        let end_a = after.find("\" \"b/")?;
        let after_b = &after[end_a + 5..];
        let close = after_b.rfind('"')?;
        return unescape_git_quoted(&after_b[..close]);
    }
    let body = rest.strip_prefix("a/")?;
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find(" b/") {
        let abs = search_from + rel;
        let path_a = &body[..abs];
        let path_b = &body[abs + 3..];
        if path_a == path_b {
            return Some(path_b.to_string());
        }
        search_from = abs + 1;
    }
    body.find(" b/").map(|i| body[i + 3..].to_string())
}

fn unescape_git_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            return None;
        }
        match bytes[i + 1] {
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'"' => {
                out.push(b'"');
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b'a' => {
                out.push(0x07);
                i += 2;
            }
            b'b' => {
                out.push(0x08);
                i += 2;
            }
            b'f' => {
                out.push(0x0C);
                i += 2;
            }
            b'v' => {
                out.push(0x0B);
                i += 2;
            }
            d0 @ b'0'..=b'3'
                if i + 3 < bytes.len()
                    && (b'0'..=b'7').contains(&bytes[i + 2])
                    && (b'0'..=b'7').contains(&bytes[i + 3]) =>
            {
                let n = ((d0 - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0');
                out.push(n);
                i += 4;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

fn split_patch_sections(text: &str) -> Vec<PatchSection> {
    let mut out: Vec<PatchSection> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut body = String::new();
    let mut in_body = false;
    let mut is_deletion = false;

    let flush = |out: &mut Vec<PatchSection>,
                 path: &mut Option<String>,
                 body: &mut String,
                 is_deletion: &mut bool| {
        if !body.is_empty() || *is_deletion {
            out.push(PatchSection {
                rel_path: path.take(),
                body: std::mem::take(body),
                is_deletion: std::mem::replace(is_deletion, false),
            });
        } else {
            *path = None;
        }
    };

    for line in text.split_inclusive('\n') {
        let stripped = line.trim_end_matches(['\n', '\r']);
        if let Some(rest) = stripped.strip_prefix("diff --git ") {
            // New file boundary — flush whatever we were collecting.
            flush(&mut out, &mut current_path, &mut body, &mut is_deletion);
            in_body = false;
            // Parse `a/<path> b/<path>` and prefer the post-edit
            // (`b/`) path so renames land on the new name.
            current_path = parse_diff_git_b_path(rest);
            continue;
        }
        if !in_body {
            if stripped.starts_with("--- ") {
                in_body = true;
                // Rewrite `--- /dev/null` (file addition) to `--- a/<path>`
                // so diffy's parser still gets a valid header. The
                // original file content we feed `diffy::apply` is empty
                // for additions, which is what diffy expects.
                if stripped == "--- /dev/null"
                    && let Some(rel) = current_path.as_deref()
                {
                    body.push_str(&format!("--- a/{rel}\n"));
                } else {
                    body.push_str(stripped);
                    body.push('\n');
                }
            }
            // Skip git's `index ...` / `new file mode ...` /
            // `similarity index ...` decorations — diffy doesn't
            // understand them and they aren't needed once we know
            // the target path.
            continue;
        }
        if stripped == "+++ /dev/null" {
            // File deletion — note it and drop this header line. The
            // linker will `remove_file` and skip the diffy apply path
            // entirely, so the rest of the body (the hunk that empties
            // the file) is intentionally discarded.
            is_deletion = true;
            continue;
        }
        body.push_str(stripped);
        body.push('\n');
    }
    flush(&mut out, &mut current_path, &mut body, &mut is_deletion);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn apply_multi_file_patch_refuses_to_follow_junction_outside_pkg() {
        let outside = tempfile::tempdir().unwrap();
        let pkg_root = tempfile::tempdir().unwrap();
        let pkg = pkg_root.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let escape = pkg.join("escape");
        junction::create(outside.path(), &escape).unwrap();
        let target = outside.path().join("victim.txt");
        std::fs::write(&target, "untouched\n").unwrap();
        let patch = "diff --git a/escape/victim.txt b/escape/victim.txt\n\
                     --- a/escape/victim.txt\n\
                     +++ b/escape/victim.txt\n\
                     @@ -1 +1 @@\n\
                     -untouched\n\
                     +PWNED\n";
        let result = apply_multi_file_patch(&pkg, patch);
        assert!(result.is_err(), "patch must refuse junction-bearing rel");
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(after, "untouched\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_multi_file_patch_refuses_to_follow_symlink_outside_pkg() {
        let outside = tempfile::tempdir().unwrap();
        let pkg_root = tempfile::tempdir().unwrap();
        let pkg = pkg_root.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let escape = pkg.join("escape");
        std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
        let target = outside.path().join("victim.txt");
        std::fs::write(&target, "untouched\n").unwrap();
        let patch = "diff --git a/escape/victim.txt b/escape/victim.txt\n\
                     --- a/escape/victim.txt\n\
                     +++ b/escape/victim.txt\n\
                     @@ -1 +1 @@\n\
                     -untouched\n\
                     +PWNED\n";
        let result = apply_multi_file_patch(&pkg, patch);
        assert!(result.is_err(), "patch must refuse symlink-bearing rel");
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(after, "untouched\n");
    }

    #[test]
    fn round_trips_simple_patch() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("index.js"), "module.exports = 'old';\n").unwrap();

        let patch = "diff --git a/index.js b/index.js\n\
                     --- a/index.js\n\
                     +++ b/index.js\n\
                     @@ -1 +1 @@\n\
                     -module.exports = 'old';\n\
                     +module.exports = 'new';\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(pkg.join("index.js")).unwrap(),
            "module.exports = 'new';\n"
        );
    }

    #[test]
    fn crlf_patch_path_does_not_carry_carriage_return() {
        let patch = "diff --git a/index.js b/index.js\r\n\
                     --- a/index.js\r\n\
                     +++ b/index.js\r\n\
                     @@ -1 +1 @@\r\n\
                     -module.exports = 'old';\r\n\
                     +module.exports = 'new';\r\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("index.js"));
    }

    #[test]
    fn crlf_deletion_patch_recognized() {
        let patch = "diff --git a/removed.js b/removed.js\r\n\
                     deleted file mode 100644\r\n\
                     --- a/removed.js\r\n\
                     +++ /dev/null\r\n\
                     @@ -1 +0,0 @@\r\n\
                     -gone\r\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].is_deletion);
    }

    #[test]
    fn diff_git_path_with_space_b_substring() {
        let patch = "diff --git a/a b/c.js b/a b/c.js\n\
                     --- a/a b/c.js\n\
                     +++ b/a b/c.js\n\
                     @@ -1 +1 @@\n\
                     -x\n\
                     +y\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("a b/c.js"));
    }

    #[test]
    fn diff_git_quoted_path_form() {
        let patch = "diff --git \"a/path with spaces.js\" \"b/path with spaces.js\"\n\
                     --- a/path with spaces.js\n\
                     +++ b/path with spaces.js\n\
                     @@ -1 +1 @@\n\
                     -x\n\
                     +y\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("path with spaces.js"));
    }

    #[test]
    fn applies_lf_patch_against_crlf_file() {
        // Tarballs published from Windows editors ship CRLF text. pnpm
        // / git emit LF-only patches even against those files. Diffy is
        // byte-exact, so the apply path normalizes CRLF -> LF before
        // matching and restores CRLF on write.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").unwrap();

        let patch = "diff --git a/a.txt b/a.txt\n\
                     --- a/a.txt\n\
                     +++ b/a.txt\n\
                     @@ -1,3 +1,3 @@\n\
                     \x20one\n\
                     -two\n\
                     +TWO\n\
                     \x20three\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        let bytes = std::fs::read(pkg.join("a.txt")).unwrap();
        assert_eq!(bytes, b"one\r\nTWO\r\nthree\r\n");
    }

    #[test]
    fn crlf_restore_preserves_embedded_cr_byte() {
        // A patch line that adds a literal `\r` byte mid-line must not
        // gain a second `\r` when we re-CRLF the output. Naive
        // `replace('\n', "\r\n")` would turn `\r\n` into `\r\r\n`; the
        // `\r\r\n` collapse undoes that.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"one\r\ntwo\r\n").unwrap();
        let patch = "diff --git a/a.txt b/a.txt\n\
                     --- a/a.txt\n\
                     +++ b/a.txt\n\
                     @@ -1,2 +1,2 @@\n\
                     -one\n\
                     +has\rcr\n\
                     \x20two\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        let bytes = std::fs::read(pkg.join("a.txt")).unwrap();
        assert_eq!(bytes, b"has\rcr\r\ntwo\r\n");
    }

    #[test]
    fn diff_git_quoted_path_unescapes_git_escapes() {
        let path = parse_diff_git_b_path(r#""a/foo\".js" "b/foo\".js""#).expect("quoted parse");
        assert_eq!(path, "foo\".js");
        let path = parse_diff_git_b_path(r#""a/back\\slash.js" "b/back\\slash.js""#)
            .expect("backslash parse");
        assert_eq!(path, "back\\slash.js");
        let path = parse_diff_git_b_path("\"a/caf\\303\\251.js\" \"b/caf\\303\\251.js\"")
            .expect("octal parse");
        assert_eq!(path, "café.js");
    }
}
