use super::berry::{parse_berry_spec, range_has_protocol, split_berry_header};
use super::classic::{parse_npm_alias_real_name, parse_spec_name};
use super::*;
use crate::{DepType, LocalSource, LockedPackage};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn make_manifest(deps: &[(&str, &str)], dev: &[(&str, &str)]) -> aube_manifest::PackageJson {
    aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: deps
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect(),
        dev_dependencies: dev
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect(),
        peer_dependencies: Default::default(),
        optional_dependencies: Default::default(),
        update_config: None,
        scripts: Default::default(),
        engines: Default::default(),
        dev_engines: None,
        workspaces: None,
        bundled_dependencies: None,
        extra: Default::default(),
    }
}

#[test]
fn test_parse_spec_name() {
    assert_eq!(parse_spec_name("foo@^1.0.0"), Some("foo".to_string()));
    assert_eq!(parse_spec_name("foo@1.2.3"), Some("foo".to_string()));
    assert_eq!(
        parse_spec_name("@scope/pkg@^1.0.0"),
        Some("@scope/pkg".to_string())
    );
    assert_eq!(parse_spec_name("foo"), None);
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  resolved "https://example.com/foo-1.2.3.tgz"
  integrity sha512-aaa
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  resolved "https://example.com/bar-2.5.0.tgz"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert_eq!(graph.packages.len(), 2);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("2.5.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_parse_scoped_and_multi_spec() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@scope/pkg@^1.0.0", "@scope/pkg@^1.1.0":
  version "1.1.0"
  integrity sha512-zzz
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
}

/// Yarn classic supports the `npm:` protocol to rename a dep on
/// import — `react-loadable: "npm:@docusaurus/react-loadable@5.5.2"`
/// installs `@docusaurus/react-loadable` under
/// `node_modules/react-loadable/`. The lockfile records the alias
/// in the spec key and the real name only behind the `npm:` value.
/// Without surfacing the real name into `LockedPackage.alias_of`,
/// the install path would fetch the alias-qualified URL and 404
/// (https://github.com/jdx/aube/discussions/681).
#[test]
fn test_parse_npm_protocol_alias_transitive() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@docusaurus/core@2.1.0":
  version "2.1.0"
  integrity sha512-aaa
  dependencies:
    react-loadable "npm:@docusaurus/react-loadable@5.5.2"

"react-loadable@npm:@docusaurus/react-loadable@5.5.2":
  version "5.5.2"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@docusaurus/core", "2.1.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let aliased = graph
        .packages
        .get("react-loadable@5.5.2")
        .expect("aliased entry should be keyed by the alias dep_path");
    assert_eq!(aliased.name, "react-loadable");
    assert_eq!(aliased.version, "5.5.2");
    assert_eq!(
        aliased.alias_of.as_deref(),
        Some("@docusaurus/react-loadable")
    );
    assert_eq!(aliased.registry_name(), "@docusaurus/react-loadable");

    // The parent must still resolve the transitive ref to the
    // alias dep_path — symlinks under node_modules/.aube/<parent>/
    // key on the alias, not the real name.
    let core = &graph.packages["@docusaurus/core@2.1.0"];
    assert_eq!(
        core.dependencies.get("react-loadable").map(String::as_str),
        Some("5.5.2")
    );
}

#[test]
fn test_parse_classic_dependency_values_are_dep_path_tails() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"@rollup/plugin-replace@2.4.1":
  version "2.4.1"
  integrity sha512-aaa
  dependencies:
    "@rollup/pluginutils" "^3.1.0"
    magic-string "^0.25.9"

"@rollup/pluginutils@^3.1.0":
  version "3.1.0"
  integrity sha512-bbb

magic-string@^0.25.9:
  version "0.25.9"
  integrity sha512-ccc
  dependencies:
    sourcemap-codec "^1.4.8"

sourcemap-codec@^1.4.8:
  version "1.4.8"
  integrity sha512-ddd
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@rollup/plugin-replace", "2.4.1")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let replace = &graph.packages["@rollup/plugin-replace@2.4.1"];
    assert_eq!(
        replace
            .dependencies
            .get("@rollup/pluginutils")
            .map(String::as_str),
        Some("3.1.0")
    );
    assert_eq!(
        replace.dependencies.get("magic-string").map(String::as_str),
        Some("0.25.9")
    );

    let magic_string = &graph.packages["magic-string@0.25.9"];
    assert_eq!(
        magic_string
            .dependencies
            .get("sourcemap-codec")
            .map(String::as_str),
        Some("1.4.8")
    );
}

/// Round-trip safety: our writer emits the canonical
/// `"name@version"` spec first and the npm-alias spec alongside it.
/// On reparse the `[0]` spec carries no `npm:`, so the alias must
/// be detected by scanning every spec in the header — not just the
/// first one.
#[test]
fn test_parse_npm_protocol_alias_canonical_spec_first() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

"react-loadable@5.5.2", "react-loadable@npm:@docusaurus/react-loadable@5.5.2":
  version "5.5.2"
  integrity sha512-bbb
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let aliased = &graph.packages["react-loadable@5.5.2"];
    assert_eq!(
        aliased.alias_of.as_deref(),
        Some("@docusaurus/react-loadable")
    );
}

#[test]
fn test_parse_npm_alias_real_name_helper() {
    assert_eq!(
        parse_npm_alias_real_name("react-loadable@npm:@docusaurus/react-loadable@5.5.2"),
        Some("@docusaurus/react-loadable".to_string())
    );
    assert_eq!(
        parse_npm_alias_real_name("h3-v2@npm:h3@2.0.1-rc.20"),
        Some("h3".to_string())
    );
    assert_eq!(
        parse_npm_alias_real_name("@my-scope/alias@npm:@upstream/pkg@^1.0.0"),
        Some("@upstream/pkg".to_string())
    );
    // No npm: protocol — the common case.
    assert_eq!(parse_npm_alias_real_name("foo@^1.0.0"), None);
    assert_eq!(parse_npm_alias_real_name("@scope/pkg@^1.0.0"), None);
    // Other protocols pass through as non-aliases (workspace:, file:, …).
    assert_eq!(parse_npm_alias_real_name("foo@workspace:*"), None);
}

#[test]
fn test_detect_berry_vs_classic() {
    // The `__metadata:` marker is what distinguishes berry from
    // classic; `is_berry` is the primary dispatcher signal so we
    // assert it fires on every version berry has emitted
    // (`__metadata.version` 3 through 8 across yarn 2–4).
    assert!(is_berry("__metadata:\n  version: 6\n"));
    assert!(is_berry("# comment\n__metadata:\n  version: 8\n"));
    assert!(!is_berry(
        "# yarn lockfile v1\n\nfoo@^1.0.0:\n  version \"1.0.0\"\n"
    ));
}

/// Parse → write → parse should preserve package set,
/// versions, integrity, and the resolved transitive graph. If
/// the writer emits malformed block headers or forgets to
/// requote, round-trip breaks here.
#[test]
fn test_write_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  integrity sha512-foo
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  integrity sha512-bar
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_classic(out.path(), &graph, &manifest).unwrap();

    // Re-parse the output. The manifest is the same — direct-dep
    // resolution requires a spec key of `foo@^1.0.0`, but the
    // writer emits `"foo@1.2.3"`. So direct-dep lookup will
    // miss; we only assert the packages/transitives round-trip.
    let reparsed_manifest = make_manifest(&[], &[]);
    let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

    assert!(reparsed.packages.contains_key("foo@1.2.3"));
    assert!(reparsed.packages.contains_key("bar@2.5.0"));
    assert_eq!(
        reparsed.packages["foo@1.2.3"].integrity.as_deref(),
        Some("sha512-foo")
    );
    // foo's transitive dep on bar must still resolve: the writer
    // emits `bar "2.5.0"` under foo's dependencies, and reparse
    // finds the block keyed `"bar@2.5.0"` via spec_to_dep_path.
    assert_eq!(
        reparsed.packages["foo@1.2.3"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("2.5.0")
    );
}

#[test]
fn test_dev_dep_classification() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"foo@^1.0.0:
  version "1.0.0"
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[("foo", "^1.0.0")]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].dep_type, DepType::Dev);
}

// ---- berry (v2+) ---------------------------------------------------

#[test]
fn test_parse_berry_spec() {
    assert_eq!(
        parse_berry_spec("lodash@npm:^4.17.0"),
        Some(("lodash", "npm", "^4.17.0"))
    );
    assert_eq!(
        parse_berry_spec("@types/node@npm:20.1.0"),
        Some(("@types/node", "npm", "20.1.0"))
    );
    assert_eq!(
        parse_berry_spec("my-pkg@workspace:."),
        Some(("my-pkg", "workspace", "."))
    );
    // Missing protocol colon: malformed.
    assert_eq!(parse_berry_spec("no-protocol"), None);
}

#[test]
fn test_split_berry_header() {
    let specs = split_berry_header("lodash@npm:^4.17.0, lodash@npm:^4.18.0");
    assert_eq!(
        specs,
        vec![
            "lodash@npm:^4.17.0".to_string(),
            "lodash@npm:^4.18.0".to_string()
        ]
    );
    let single = split_berry_header("foo@npm:1.0.0");
    assert_eq!(single, vec!["foo@npm:1.0.0".to_string()]);
}

#[test]
fn test_range_has_protocol() {
    assert!(range_has_protocol("npm:^1.0.0"));
    assert!(range_has_protocol("workspace:*"));
    assert!(range_has_protocol("file:./pkgs/foo"));
    assert!(range_has_protocol("patch:react@^18.0.0#./mypatch.patch"));
    // Compound transports: berry emits these for git-over-ssh /
    // git-over-https, and the writer must not re-prefix them with
    // `npm:` when building header specs from the manifest range.
    assert!(range_has_protocol("git+ssh://git@github.com/u/r.git"));
    assert!(range_has_protocol("git+https://github.com/u/r.git"));
    assert!(range_has_protocol("git+file:./vendored.git"));
    // Bare semver ranges never have a protocol.
    assert!(!range_has_protocol("^1.0.0"));
    assert!(!range_has_protocol("1.2.3"));
    assert!(!range_has_protocol(">=1.0 <2.0"));
}

/// Realistic yarn 4 lockfile with `npm:` deps — the overwhelming
/// majority real-world case. Exercises `__metadata` parsing,
/// multi-spec block headers, nested `dependencies:`, and the
/// direct-dep pass that prepends `npm:` to manifest ranges.
#[test]
fn test_parse_berry_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/abcdef
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/123456
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert_eq!(graph.packages.len(), 2);
    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.version, "1.2.3");
    assert_eq!(foo.yarn_checksum.as_deref(), Some("10c0/abcdef"));
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("bar@2.5.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_parse_berry_patch_protocol() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"is-number@patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123":
  version: 7.0.0
  resolution: "is-number@patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[(
            "is-number",
            "patch:is-number@npm%3A7.0.0#./.yarn/patches/is-number.patch::version=7.0.0&hash=abc123",
        )],
        &[],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("is-number@7.0.0"));
    assert_eq!(
        graph
            .patched_dependencies
            .get("is-number@7.0.0")
            .map(String::as_str),
        Some("./.yarn/patches/is-number.patch")
    );
    assert_eq!(graph.importers["."][0].dep_path, "is-number@7.0.0");
}

#[test]
fn test_parse_berry_skips_builtin_patch_protocol() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"glob@patch:glob@npm%3A8.1.0#~builtin<compat/glob>":
  version: 8.1.0
  resolution: "glob@patch:glob@npm%3A8.1.0#~builtin<compat/glob>"
  checksum: 10c0/patched
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("glob", "^8.1.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.is_empty());
    assert!(graph.patched_dependencies.is_empty());
    assert!(graph.importers["."].is_empty());
}

/// Scoped package names (`@types/node`) and the `, `-joined
/// multi-spec header format berry uses when two package.json
/// ranges resolve to the same version.
#[test]
fn test_parse_berry_scoped_and_multi_spec() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"@scope/pkg@npm:^1.0.0, @scope/pkg@npm:^1.1.0":
  version: 1.1.0
  resolution: "@scope/pkg@npm:1.1.0"
  checksum: 10c0/zzz
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
}

/// Blocks for the project's own workspace entry shouldn't become
/// `LockedPackage`s — they're the root importer, not a
/// resolved dep. Skipping them keeps the graph shape identical to
/// what parsing the `package.json` alone would produce.
#[test]
fn test_parse_berry_skips_workspace_root() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"my-project@workspace:.":
  version: 0.0.0-use.local
  resolution: "my-project@workspace:."
  dependencies:
    foo: "npm:^1.0.0"
  languageName: unknown
  linkType: soft

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // Workspace block is skipped; only the real resolved dep survives.
    assert_eq!(graph.packages.len(), 1);
    assert!(graph.packages.contains_key("foo@1.0.0"));
    assert!(!graph.packages.contains_key("my-project@0.0.0-use.local"));
}

/// Berry emits `version:` unquoted, so scalar-looking values can
/// parse as numbers instead of strings. Our parser must unfold
/// those back to strings instead of failing with "has no version" —
/// real packages with fewer-than-three-component versions do exist
/// (even if rare).
#[test]
fn test_parse_berry_unquoted_numeric_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"int-version@npm:5":
  version: 5
  resolution: "int-version@npm:5"
  languageName: node
  linkType: hard

"two-part@npm:1.0":
  version: 1.0
  resolution: "two-part@npm:1.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    assert!(graph.packages.contains_key("int-version@5"));
    assert!(graph.packages.contains_key("two-part@1.0"));
    assert_eq!(graph.packages["int-version@5"].version, "5");
    assert_eq!(graph.packages["two-part@1.0"].version, "1.0");
}

/// Same scalar hazard applies to dependency values:
/// `peerDependencies: { foo: 5 }` writes a YAML number, and
/// boolean-looking tags or ranges can parse as booleans. The parser
/// routes dep values through `yaml_scalar_as_string` so a future
/// regression shows up as a missing peer edge rather than a parse
/// error.
#[test]
fn test_parse_berry_typed_dep_values() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  peerDependencies:
    numeric-peer: 5
    bool-peer: true
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.peer_dependencies
            .get("numeric-peer")
            .map(String::as_str),
        Some("5")
    );
    assert_eq!(
        foo.peer_dependencies.get("bool-peer").map(String::as_str),
        Some("true")
    );
}

/// Berry's `https:` tarball protocol and `git+ssh:` / `git:`
/// transports both survive parsing with a populated
/// `LocalSource`, rather than falling through to the "unknown
/// protocol" skip path.
///
/// The hazard this guards against: `parse_berry_spec` splits
/// `"foo@https://host/path"` into `res_protocol = "https"` /
/// `res_body = "//host/path"` — the body never starts with
/// `https://`, so a URL-body check would always miss. Parsing the
/// file and verifying the package lands in the graph with the
/// right `LocalSource` catches any future regression of the
/// dispatch match arms.
#[test]
fn test_parse_berry_http_and_git_protocols() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"tarball-pkg@https://example.com/pkg-1.0.0.tgz":
  version: 1.0.0
  resolution: "tarball-pkg@https://example.com/pkg-1.0.0.tgz"
  languageName: node
  linkType: hard

"git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01":
  version: 2.0.0
  resolution: "git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01"
  languageName: node
  linkType: hard

"ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef":
  version: 3.0.0
  resolution: "ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    // All three packages should be present — none silently
    // skipped as "unrecognized protocol". The `.values()` scan
    // below asserts the `LocalSource` shape for each.
    assert_eq!(graph.packages.len(), 3);
    let by_name: BTreeMap<&str, &LockedPackage> = graph
        .packages
        .values()
        .map(|p| (p.name.as_str(), p))
        .collect();

    // `.tgz` on https → remote tarball.
    let tar = by_name["tarball-pkg"];
    assert!(matches!(
        &tar.local_source,
        Some(LocalSource::RemoteTarball(_))
    ));

    // `.git` on https → git source, not tarball.
    let git = by_name["git-pkg"];
    let Some(LocalSource::Git(git)) = &git.local_source else {
        panic!("expected git LocalSource");
    };
    assert_eq!(git.url, "https://github.com/user/repo.git");
    assert_eq!(git.resolved, "abcdef0123456789abcdef0123456789abcdef01");

    // `git+ssh:` prefix → git source.
    let ssh = by_name["ssh-git-pkg"];
    assert!(matches!(&ssh.local_source, Some(LocalSource::Git(_))));
}

#[test]
fn test_parse_berry_portal_and_exec_protocols() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"portal-pkg@portal:./packages/portal":
  version: 1.0.0
  resolution: "portal-pkg@portal:./packages/portal"
  dependencies:
    left-pad: "npm:^1.3.0"
  languageName: node
  linkType: soft

"exec-pkg@exec:./scripts/generate-exec.js":
  version: 2.0.0
  resolution: "exec-pkg@exec:./scripts/generate-exec.js"
  languageName: node
  linkType: hard

"left-pad@npm:^1.3.0":
  version: 1.3.0
  resolution: "left-pad@npm:1.3.0"
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(
        &[
            ("portal-pkg", "portal:./packages/portal"),
            ("exec-pkg", "exec:./scripts/generate-exec.js"),
        ],
        &[],
    );
    let graph = parse(tmp.path(), &manifest).unwrap();

    let portal_key = LocalSource::Portal(PathBuf::from("./packages/portal")).dep_path("portal-pkg");
    let portal = &graph.packages[&portal_key];
    assert!(matches!(
        &portal.local_source,
        Some(LocalSource::Portal(p)) if p == &PathBuf::from("./packages/portal")
    ));
    assert_eq!(
        portal.dependencies.get("left-pad").map(String::as_str),
        Some("left-pad@1.3.0")
    );

    let exec_key =
        LocalSource::Exec(PathBuf::from("./scripts/generate-exec.js")).dep_path("exec-pkg");
    let exec = &graph.packages[&exec_key];
    assert!(matches!(
        &exec.local_source,
        Some(LocalSource::Exec(p)) if p == &PathBuf::from("./scripts/generate-exec.js")
    ));
    assert_eq!(graph.importers["."].len(), 2);
}

/// Round-trip: parse berry → write berry → parse berry should
/// preserve packages, versions, checksum (via `yarn_checksum`),
/// and transitive edges. This is the core round-trip contract.
#[test]
fn test_write_berry_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/foohash
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/barhash
  languageName: node
  linkType: hard
"#;
    std::fs::write(tmp.path(), content).unwrap();
    let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let graph = parse(tmp.path(), &manifest).unwrap();

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();

    // Confirm the output is berry-shaped so dispatcher picks the
    // right parser on reparse.
    let written = std::fs::read_to_string(out.path()).unwrap();
    assert!(is_berry(&written));

    let reparsed_manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
    let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

    assert!(reparsed.packages.contains_key("foo@1.2.3"));
    assert!(reparsed.packages.contains_key("bar@2.5.0"));
    assert_eq!(
        reparsed.packages["foo@1.2.3"].yarn_checksum.as_deref(),
        Some("10c0/foohash")
    );
    assert_eq!(
        reparsed.packages["foo@1.2.3"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("bar@2.5.0")
    );
    // The manifest spec `foo@^1.0.0` appears verbatim (with `npm:`
    // prepended) in the block header, so direct-dep lookup
    // succeeds on reparse — which it did NOT for classic, so this
    // is a stronger round-trip guarantee.
    let root = reparsed.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].dep_path, "foo@1.2.3");
}

#[test]
fn test_write_berry_roundtrips_patch_protocol() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "is-number@7.0.0".to_string(),
        LockedPackage {
            name: "is-number".to_string(),
            version: "7.0.0".to_string(),
            dep_path: "is-number@7.0.0".to_string(),
            yarn_checksum: Some("10c0/patched".to_string()),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(
                ".".to_string(),
                vec![crate::DirectDep {
                    name: "is-number".to_string(),
                    dep_path: "is-number@7.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                }],
            );
            m
        },
        packages,
        patched_dependencies: BTreeMap::from([(
            "is-number@7.0.0".to_string(),
            ".yarn/patches/is-number.patch".to_string(),
        )]),
        ..Default::default()
    };
    let manifest = make_manifest(
        &[(
            "is-number",
            "patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch::version=7.0.0&hash=abc123",
        )],
        &[],
    );

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();
    let full_spec = "is-number@patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch::version=7.0.0&hash=abc123";
    assert!(written.contains(full_spec));
    assert!(!written.contains(&format!(
        "is-number@patch:is-number@npm%3A7.0.0#.yarn/patches/is-number.patch, {full_spec}"
    )));

    let reparsed = parse(out.path(), &manifest).unwrap();
    assert_eq!(
        reparsed
            .patched_dependencies
            .get("is-number@7.0.0")
            .map(String::as_str),
        Some(".yarn/patches/is-number.patch")
    );
    assert_eq!(reparsed.importers["."][0].dep_path, "is-number@7.0.0");
}

/// `link:` deps are pure symlinks in berry's model, which means
/// the block must carry `linkType: soft` — writing `hard` makes
/// yarn's own linker try to copy/hardlink the target into the
/// virtual store on the next install. Registry packages (no
/// `local_source`) stay `hard`, the default.
#[test]
fn test_write_berry_link_type_soft_for_link_deps() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "linked-pkg@1.0.0".to_string(),
        LockedPackage {
            name: "linked-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "linked-pkg@1.0.0".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("./vendor/linked-pkg"))),
            ..Default::default()
        },
    );
    packages.insert(
        "regular-pkg@2.0.0".to_string(),
        LockedPackage {
            name: "regular-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "regular-pkg@2.0.0".to_string(),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // The `link:` block gets `soft`; the registry block stays `hard`.
    // Block order is sorted by canonical key, so `linked-pkg`
    // comes before `regular-pkg` and each block's `linkType`
    // appears after its `languageName` line.
    let linked_idx = written.find("linked-pkg@").unwrap();
    let regular_idx = written.find("regular-pkg@").unwrap();
    let linked_block = &written[linked_idx..regular_idx];
    let regular_block = &written[regular_idx..];
    assert!(
        linked_block.contains("linkType: soft"),
        "link: block should be soft-linked:\n{linked_block}"
    );
    assert!(
        regular_block.contains("linkType: hard"),
        "registry block should be hard-linked:\n{regular_block}"
    );
}

#[test]
fn test_write_berry_roundtrips_portal_and_exec_protocols() {
    let portal_source = LocalSource::Portal(PathBuf::from("./packages/portal"));
    let exec_source = LocalSource::Exec(PathBuf::from("./scripts/generate-exec.js"));
    let mut packages = BTreeMap::new();
    packages.insert(
        portal_source.dep_path("portal-pkg"),
        LockedPackage {
            name: "portal-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: portal_source.dep_path("portal-pkg"),
            local_source: Some(portal_source),
            ..Default::default()
        },
    );
    packages.insert(
        exec_source.dep_path("exec-pkg"),
        LockedPackage {
            name: "exec-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: exec_source.dep_path("exec-pkg"),
            local_source: Some(exec_source),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert!(written.contains("portal-pkg@portal:./packages/portal"));
    assert!(written.contains("exec-pkg@exec:./scripts/generate-exec.js"));
    let portal_idx = written.find("portal-pkg@portal:").unwrap();
    let exec_idx = written.find("exec-pkg@exec:").unwrap();
    assert!(
        exec_idx < portal_idx,
        "expected exec block before portal block:\n{written}"
    );
    let portal_block = &written[portal_idx..];
    let exec_block = &written[exec_idx..portal_idx];
    assert!(portal_block.contains("linkType: soft"));
    assert!(exec_block.contains("linkType: hard"));
}

/// Header and `resolution:` both carry spec strings that may
/// contain backslashes (Windows-style `file:` paths) or embedded
/// quotes (patched-package descriptors). The writer must route
/// them through `quote_yaml_scalar` so the emitted YAML is
/// well-formed. We can't easily drive backslashes into the model
/// from a parsed berry file (berry itself doesn't emit them on
/// macOS/Linux), so we construct a package with a `file:` source
/// that contains a backslash directly and assert the output
/// escapes it and round-trips through `yaml_serde::from_str`.
#[test]
fn test_write_berry_escapes_resolution_and_header() {
    let mut packages = BTreeMap::new();
    packages.insert(
        "weird-pkg@1.0.0".to_string(),
        LockedPackage {
            name: "weird-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "weird-pkg@1.0.0".to_string(),
            // A file: source whose path has a backslash. The
            // header and resolution both become
            // `weird-pkg@file:./a\b/c`; without escaping, the
            // raw backslash in the YAML string would be a
            // malformed escape.
            local_source: Some(LocalSource::Directory(PathBuf::from("./a\\b/c"))),
            ..Default::default()
        },
    );
    let graph = LockfileGraph {
        importers: {
            let mut m = BTreeMap::new();
            m.insert(".".to_string(), vec![]);
            m
        },
        packages,
        ..Default::default()
    };
    let manifest = make_manifest(&[], &[]);

    let out = tempfile::NamedTempFile::new().unwrap();
    write_berry(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // The emitted file must parse as YAML — any missing escape
    // blows up here instead of corrupting a real install.
    let _doc: yaml_serde::Value = yaml_serde::from_str(&written)
        .unwrap_or_else(|e| panic!("berry writer produced malformed YAML: {e}\n{written}"));
}
