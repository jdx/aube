use super::layout::package_name_from_install_path;
use super::source::local_git_source_from_resolved;
use super::*;
use crate::{DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[test]
fn test_package_name_from_install_path() {
    assert_eq!(
        package_name_from_install_path("node_modules/foo"),
        Some("foo".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/@scope/pkg"),
        Some("@scope/pkg".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/foo/node_modules/bar"),
        Some("bar".to_string())
    );
    assert_eq!(
        package_name_from_install_path("node_modules/foo/node_modules/@scope/pkg"),
        Some("@scope/pkg".to_string())
    );
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" },
                    "devDependencies": { "bar": "^2.0.0" }
                },
                "node_modules/foo": {
                    "version": "1.2.3",
                    "integrity": "sha512-aaa",
                    "dependencies": { "nested": "^3.0.0" }
                },
                "node_modules/nested": {
                    "version": "3.1.0",
                    "integrity": "sha512-bbb"
                },
                "node_modules/bar": {
                    "version": "2.5.0",
                    "integrity": "sha512-ccc",
                    "dev": true
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();

    assert_eq!(graph.packages.len(), 3);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("nested@3.1.0"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
    // `LockedPackage.dependencies` values are dep_path *tails* (the
    // substring after `<name>@`), not full dep_paths — matches the
    // pnpm parser and the linker's sibling-symlink builder.
    assert_eq!(
        foo.dependencies.get("nested").map(String::as_str),
        Some("3.1.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 2);
    assert!(
        root.iter()
            .any(|d| d.name == "foo" && d.dep_type == DepType::Production)
    );
    assert!(
        root.iter()
            .any(|d| d.name == "bar" && d.dep_type == DepType::Dev)
    );
}

#[test]
fn test_parse_git_resolved_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sha = "abcdef1234567890abcdef1234567890abcdef12";
    let content = format!(
        r#"{{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 2,
            "packages": {{
                "": {{
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {{ "git-only": "github:owner/repo#{sha}" }}
                }},
                "node_modules/git-only": {{
                    "version": "1.2.3",
                    "resolved": "git+ssh://git@github.com/owner/repo.git#{sha}",
                    "integrity": "sha512-aaa"
                }}
            }}
        }}"#
    );
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root = &graph.importers["."];
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "git-only");
    assert!(!graph.packages.contains_key("git-only@1.2.3"));

    let pkg = &graph.packages[&root[0].dep_path];
    assert_eq!(pkg.name, "git-only");
    assert_eq!(pkg.version, "1.2.3");
    assert_eq!(pkg.integrity.as_deref(), Some("sha512-aaa"));
    assert!(pkg.tarball_url.is_none());

    let Some(LocalSource::Git(git)) = &pkg.local_source else {
        panic!("expected git local source, got {:?}", pkg.local_source);
    };
    assert_eq!(git.url, "ssh://git@github.com/owner/repo.git");
    assert_eq!(git.committish.as_deref(), Some(sha));
    assert_eq!(git.resolved, sha);
}

#[test]
fn test_unpinned_git_resolved_url_is_not_locked_git_source() {
    assert!(local_git_source_from_resolved("git+https://github.com/owner/repo.git").is_none());
}

#[test]
fn test_write_preserves_git_resolved_url() {
    let sha = "abcdef1234567890abcdef1234567890abcdef12";
    let mut graph = LockfileGraph::default();
    let local = LocalSource::Git(GitSource {
        url: "ssh://git@github.com/owner/repo.git".to_string(),
        committish: Some(sha.to_string()),
        resolved: sha.to_string(),
        integrity: None,
        subpath: None,
    });
    let dep_path = local.dep_path("git-only");
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "git-only".to_string(),
            version: "1.2.3".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "git-only".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some(format!("github:owner/repo#{sha}")),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("git-only".to_string(), format!("github:owner/repo#{sha}"))]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains(&format!(
            "\"resolved\": \"git+ssh://git@github.com/owner/repo.git#{sha}\""
        )),
        "expected git resolved URL emitted; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let pkg = &reparsed.packages[&graph.importers["."][0].dep_path];
    assert!(matches!(pkg.local_source, Some(LocalSource::Git(_))));
}

#[test]
fn test_write_skips_non_git_local_sources() {
    let local = LocalSource::Directory(PathBuf::from("vendor/local-dir"));
    let dep_path = local.dep_path("local-dir");
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "local-dir".to_string(),
            version: "1.0.0".to_string(),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "local-dir".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some("file:vendor/local-dir".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("local-dir".to_string(), "file:vendor/local-dir".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(!body.contains("\"node_modules/local-dir\""));
}

#[test]
fn test_parse_file_resolved_without_link() {
    // npm writes `resolved: "file:..."` without `link: true` for
    // local tarball deps (`npm install file:../foo-1.0.0.tgz`) and
    // for some directory deps. Both shapes must surface as a
    // LocalSource so the resolver dispatches the local-source
    // branch and doesn't fall through to a registry fetch.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": {
                        "tar-dep": "file:../utils/tar-dep-1.0.0.tgz",
                        "dir-dep": "file:../utils"
                    }
                },
                "node_modules/tar-dep": {
                    "version": "1.0.0",
                    "resolved": "file:../utils/tar-dep-1.0.0.tgz",
                    "integrity": "sha512-aaa"
                },
                "node_modules/dir-dep": {
                    "version": "1.0.0",
                    "resolved": "file:../utils"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();

    let tar_pkg = graph
        .packages
        .values()
        .find(|p| p.name == "tar-dep")
        .expect("tar-dep entry");
    assert!(
        matches!(&tar_pkg.local_source, Some(LocalSource::Tarball(p)) if p == Path::new("../utils/tar-dep-1.0.0.tgz")),
        "expected Tarball source, got {:?}",
        tar_pkg.local_source,
    );
    assert!(
        tar_pkg.dep_path.starts_with("tar-dep@file+"),
        "tarball dep_path should be local-source-keyed, got {}",
        tar_pkg.dep_path,
    );

    let dir_pkg = graph
        .packages
        .values()
        .find(|p| p.name == "dir-dep")
        .expect("dir-dep entry");
    assert!(
        matches!(&dir_pkg.local_source, Some(LocalSource::Directory(p)) if p == Path::new("../utils")),
        "expected Directory source, got {:?}",
        dir_pkg.local_source,
    );
    assert!(
        dir_pkg.dep_path.starts_with("dir-dep@file+"),
        "directory dep_path should be local-source-keyed, got {}",
        dir_pkg.dep_path,
    );

    let root = graph.importers.get(".").unwrap();
    let tar_direct = root.iter().find(|d| d.name == "tar-dep").unwrap();
    assert_eq!(tar_direct.dep_path, tar_pkg.dep_path);
    let dir_direct = root.iter().find(|d| d.name == "dir-dep").unwrap();
    assert_eq!(dir_direct.dep_path, dir_pkg.dep_path);
}

#[test]
fn test_parse_scoped_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "@scope/pkg": "^1.0.0" }
                },
                "node_modules/@scope/pkg": {
                    "version": "1.0.0",
                    "integrity": "sha512-zzz"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert!(graph.packages.contains_key("@scope/pkg@1.0.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
    assert_eq!(root[0].dep_path, "@scope/pkg@1.0.0");
}

#[test]
fn test_parse_multi_version_nested() {
    // bar exists at two versions: 2.0.0 hoisted to root, 1.0.0 nested under foo.
    // foo's transitive dep on bar must resolve to 1.0.0, not 2.0.0.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    // Both versions of bar should be present.
    assert!(graph.packages.contains_key("bar@2.0.0"));
    assert!(graph.packages.contains_key("bar@1.0.0"));
    assert!(graph.packages.contains_key("foo@1.0.0"));

    // foo's transitive dep must point to the nested (1.0.0), not the hoisted (2.0.0).
    // Value is the dep_path tail (version) — see the `LockedPackage.dependencies` doc.
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("1.0.0")
    );

    // Root's direct bar dep points to the hoisted 2.0.0.
    let root = graph.importers.get(".").unwrap();
    let root_bar = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(root_bar.dep_path, "bar@2.0.0");
}

/// Regression: a package reachable from both a dev root and
/// an optional root (but *not* from any production root) must
/// be written with `devOptional: true`, not with both `dev: true`
/// and `optional: true`. Emitting both trips `npm install
/// --omit=dev` (and `--omit=optional`) into dropping a package
/// the other chain still needs.
#[test]
fn test_write_dev_and_optional_reachable_uses_dev_optional() {
    let mut graph = LockfileGraph::default();
    let mk = |name: &str| LockedPackage {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        integrity: Some(format!("sha512-{name}")),
        dep_path: format!("{name}@1.0.0"),
        dependencies: [("shared".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    graph
        .packages
        .insert("dev-root@1.0.0".to_string(), mk("dev-root"));
    graph
        .packages
        .insert("opt-root@1.0.0".to_string(), mk("opt-root"));
    graph.packages.insert(
        "shared@1.0.0".to_string(),
        LockedPackage {
            name: "shared".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-shared".to_string()),
            dep_path: "shared@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "dev-root".to_string(),
                dep_path: "dev-root@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: None,
            },
            DirectDep {
                name: "opt-root".to_string(),
                dep_path: "opt-root@1.0.0".to_string(),
                dep_type: DepType::Optional,
                specifier: None,
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dev_dependencies: [("dev-root".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        optional_dependencies: [("opt-root".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

    let shared = &json["packages"]["node_modules/shared"];
    assert_eq!(shared["devOptional"], true, "expected devOptional flag");
    assert!(
        shared.get("dev").is_none(),
        "must not emit dev: true alongside devOptional",
    );
    assert!(
        shared.get("optional").is_none(),
        "must not emit optional: true alongside devOptional",
    );

    // Roots themselves retain their specific flag.
    assert_eq!(json["packages"]["node_modules/dev-root"]["dev"], true);
    assert_eq!(json["packages"]["node_modules/opt-root"]["optional"], true);
}

/// Regression: the npm writer must drop `dependencies` entries
/// whose target isn't in the canonical map. Platform-filtered
/// optionals and `ignoredOptionalDependencies` leave the parent's
/// declared `dependencies` map pointing at packages the resolver
/// already removed; emitting them anyway produces a lockfile
/// where `npm ci` sees a reference with no matching `packages`
/// entry and refuses to install. Must match the bun/yarn
/// writers, which already filter this way.
#[test]
fn test_write_filters_missing_canonical_deps() {
    let mut graph = LockfileGraph::default();
    // Root has one real package, `foo`, which declares a dep on
    // `ghost@1.0.0` — but `ghost` was filtered out of the graph
    // (e.g. a platform-gated optional). The canonical map won't
    // contain it.
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            dependencies: [("ghost".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    // Parse the raw JSON directly — the aube reparser tolerates
    // dangling references so we assert on the serialized shape.
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let foo_entry = &json["packages"]["node_modules/foo"];
    assert!(
        foo_entry
            .get("dependencies")
            .and_then(|d| d.get("ghost"))
            .is_none(),
        "writer emitted a ghost dep that has no packages entry: {foo_entry}",
    );
    // And there should be no node_modules/ghost entry at all.
    assert!(
        json["packages"].get("node_modules/ghost").is_none(),
        "writer hallucinated a ghost entry",
    );
}

/// Regression for the shadow-nesting bug: if an intermediate
/// ancestor carries the *wrong* version of a dep, Node's
/// runtime walk stops there and never reaches a correct entry
/// at root. The writer must nest a fresh entry inside the
/// current parent's own `node_modules` instead of assuming
/// hoisting is fine just because root happens to have the
/// right version.
///
/// Shape:
///   root → foo → baz, baz depends on bar@2.0.0
///   foo already pulled in bar@1.0.0 for a sibling, so bar@1.0.0
///     lives at node_modules/foo/node_modules/bar
///   root has bar@2.0.0 at node_modules/bar
///
///   When we walk baz's deps and get to bar@2.0.0, the nearest
///   ancestor hit is bar@1.0.0 (shadowing), not root. We must
///   place a fresh entry at
///   `node_modules/foo/node_modules/baz/node_modules/bar` so
///   Node resolves the right version.
#[test]
fn test_nested_shadow_forces_nested_placement() {
    // Build a graph by hand to control the dep order deterministically.
    let mut graph = LockfileGraph::default();
    let mk = |name: &str, version: &str, deps: &[(&str, &str)]| LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        integrity: Some(format!("sha512-{name}-{version}")),
        dep_path: format!("{name}@{version}"),
        dependencies: deps
            .iter()
            .map(|(n, v)| (n.to_string(), (*v).to_string()))
            .collect(),
        ..Default::default()
    };
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        mk(
            "foo",
            "1.0.0",
            &[
                // foo pulls in bar@1.0.0 and baz@1.0.0 as siblings.
                ("bar", "1.0.0"),
                ("baz", "1.0.0"),
            ],
        ),
    );
    graph.packages.insert(
        "baz@1.0.0".to_string(),
        // baz wants bar@2.0.0, which matches the root version.
        mk("baz", "1.0.0", &[("bar", "2.0.0")]),
    );
    graph
        .packages
        .insert("bar@1.0.0".to_string(), mk("bar", "1.0.0", &[]));
    graph
        .packages
        .insert("bar@2.0.0".to_string(), mk("bar", "2.0.0", &[]));
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@2.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    // baz's transitive dep must resolve to bar@2.0.0, not the
    // shadowing bar@1.0.0 under foo. Value is the dep_path tail
    // (version) so the linker can recombine it with the dep name.
    let baz = &reparsed.packages["baz@1.0.0"];
    assert_eq!(
        baz.dependencies.get("bar").map(String::as_str),
        Some("2.0.0"),
        "baz's bar dep was shadowed by foo/bar@1.0.0 — shadow-nest fix regressed",
    );
}

#[test]
fn test_parse_npm_preserves_platform_optional_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "platform-optional-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "platform-optional-root",
                    "version": "1.0.0",
                    "dependencies": { "host": "file:host" }
                },
                "node_modules/host": {
                    "resolved": "host",
                    "link": true
                },
                "host": {
                    "name": "host",
                    "version": "1.0.0",
                    "optionalDependencies": { "native-win": "1.0.0" }
                },
                "node_modules/native-win": {
                    "version": "1.0.0",
                    "resolved": "https://registry.npmjs.org/native-win/-/native-win-1.0.0.tgz",
                    "integrity": "sha512-native",
                    "optional": true,
                    "os": ["win32"],
                    "cpu": ["x64"],
                    "libc": ["glibc"]
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let host_dep_path = &graph.importers["."][0].dep_path;
    let host = &graph.packages[host_dep_path];
    assert_eq!(
        host.dependencies.get("native-win").map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        host.optional_dependencies
            .get("native-win")
            .map(String::as_str),
        Some("1.0.0")
    );

    let native = &graph.packages["native-win@1.0.0"];
    assert_eq!(
        native.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["win32"]
    );
    assert_eq!(
        native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["x64"]
    );
    assert_eq!(
        native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

/// npm sometimes emits `os` / `cpu` / `libc` as scalar strings instead
/// of arrays (e.g. `sass-embedded-linux-arm@1.99.0` ships
/// `"libc": "glibc"`). Verbatim-roundtripped into package-lock.json,
/// the field stays scalar — accept both shapes the same way the
/// pnpm + bun parsers already do.
#[test]
fn parse_npm_package_platform_fields_accept_scalar_strings() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "scalar-platform-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "scalar-platform-root",
                    "version": "1.0.0",
                    "dependencies": { "sass-embedded-linux-arm": "1.99.0" }
                },
                "node_modules/sass-embedded-linux-arm": {
                    "version": "1.99.0",
                    "resolved": "https://registry.npmjs.org/sass-embedded-linux-arm/-/sass-embedded-linux-arm-1.99.0.tgz",
                    "integrity": "sha512-native",
                    "cpu": "arm",
                    "os": "linux",
                    "libc": "glibc"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["sass-embedded-linux-arm@1.99.0"];
    assert_eq!(
        pkg.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["linux"]
    );
    assert_eq!(
        pkg.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["arm"]
    );
    assert_eq!(
        pkg.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

#[test]
fn test_write_npm_preserves_platform_optional_metadata() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "host@1.0.0".to_string(),
        LockedPackage {
            name: "host".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-host".to_string()),
            dep_path: "host@1.0.0".to_string(),
            dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            optional_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            declared_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "native-win@1.0.0".to_string(),
        LockedPackage {
            name: "native-win".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-native".to_string()),
            dep_path: "native-win@1.0.0".to_string(),
            os: vec!["win32".to_string()].into(),
            cpu: vec!["x64".to_string()].into(),
            libc: vec!["glibc".to_string()].into(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "host".to_string(),
            dep_path: "host@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("1.0.0".to_string()),
        }],
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("platform-optional-root".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("host".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

    let host = &json["packages"]["node_modules/host"];
    assert_eq!(host["optionalDependencies"]["native-win"], "1.0.0");
    assert!(
        host.get("dependencies")
            .and_then(|deps| deps.get("native-win"))
            .is_none(),
        "optional child must not be duplicated as a required dependency: {host}",
    );

    let native = &json["packages"]["node_modules/native-win"];
    assert_eq!(native["os"], serde_json::json!(["win32"]));
    assert_eq!(native["cpu"], serde_json::json!(["x64"]));
    assert_eq!(native["libc"], serde_json::json!(["glibc"]));

    let reparsed = parse(out.path()).unwrap();
    let host = &reparsed.packages["host@1.0.0"];
    assert_eq!(
        host.optional_dependencies
            .get("native-win")
            .map(String::as_str),
        Some("1.0.0")
    );
    assert_eq!(
        host.dependencies.get("native-win").map(String::as_str),
        Some("1.0.0")
    );
    let native = &reparsed.packages["native-win@1.0.0"];
    assert_eq!(
        native.os.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["win32"]
    );
    assert_eq!(
        native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["x64"]
    );
    assert_eq!(
        native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["glibc"]
    );
}

/// Regression: `canonical_key_from_dep_path` must strip the
/// `(peer@ver)` suffix *before* splitting on `@`. A naive
/// `rfind('@')` lands inside the peer suffix and returns the
/// input unchanged, which silently drops every peer-contextualized
/// root dep from the written lockfile. Hashed suffixes use the
/// same canonical identity; otherwise long peer suffixes drop
/// out of npm package-lock output.
#[test]
fn test_canonical_key_strips_peer_suffix() {
    assert_eq!(canonical_key_from_dep_path("foo@1.0.0"), "foo@1.0.0");
    assert_eq!(
        canonical_key_from_dep_path("styled-components@6.1.0(react@18.2.0)"),
        "styled-components@6.1.0"
    );
    assert_eq!(
        canonical_key_from_dep_path("@scope/pkg@2.0.0(peer@1.0.0)"),
        "@scope/pkg@2.0.0"
    );
    assert_eq!(
        canonical_key_from_dep_path("expo-router@4.0.22_94c00fd028"),
        "expo-router@4.0.22"
    );
    assert_eq!(
        child_canonical_key("expo-router", "4.0.22_94c00fd028"),
        "expo-router@4.0.22"
    );
    assert_eq!(
        dep_value_as_version("expo-router", "expo-router@4.0.22_94c00fd028"),
        "4.0.22"
    );
    assert_eq!(
        canonical_key_from_dep_path("expo-router@4.0.22_94C00FD028"),
        "expo-router@4.0.22"
    );
}

fn test_manifest() -> aube_manifest::PackageJson {
    aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("foo".to_string(), "^1.0.0".to_string()),
            ("bar".to_string(), "^2.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    }
}

/// Parse a fixture, write it back, re-parse: the resulting graph
/// must have the same packages, direct deps, and integrity hashes.
/// Catches silent data loss in the hoist/nest walk.
#[test]
fn test_write_roundtrip_multi_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let manifest = test_manifest();

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    // Both versions of bar survived the round-trip.
    assert!(reparsed.packages.contains_key("bar@1.0.0"));
    assert!(reparsed.packages.contains_key("bar@2.0.0"));
    assert!(reparsed.packages.contains_key("foo@1.0.0"));
    assert_eq!(
        reparsed.packages["bar@2.0.0"].integrity.as_deref(),
        Some("sha512-top-bar")
    );
    assert_eq!(
        reparsed.packages["bar@1.0.0"].integrity.as_deref(),
        Some("sha512-nested-bar")
    );
    // foo's nested bar dep still resolves to 1.0.0, not the
    // hoisted 2.0.0. If the writer failed to nest, reparse would
    // snap this to bar@2.0.0. Value is the dep_path tail.
    assert_eq!(
        reparsed.packages["foo@1.0.0"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("1.0.0")
    );
}

/// Dev-only and optional-only packages get the right flags after
/// round-trip so `npm install --omit=dev` on the written file
/// does the right thing.
#[test]
fn test_write_dev_optional_flags() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "devdep@1.0.0".to_string(),
        LockedPackage {
            name: "devdep".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-dev".to_string()),
            dep_path: "devdep@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "devdep".to_string(),
                dep_path: "devdep@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: None,
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("foo".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        dev_dependencies: [("devdep".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    let packages = &json["packages"];
    assert_eq!(packages["node_modules/devdep"]["dev"], true);
    // Prod dep should have no dev field (skipped when false).
    assert!(packages["node_modules/foo"].get("dev").is_none());
}

#[test]
fn test_reject_v1() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "lockfileVersion": 1,
            "dependencies": {}
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let err = parse(tmp.path()).unwrap_err();
    assert!(matches!(err, Error::Parse(_, msg) if msg.contains("lockfileVersion 1")));
}

/// Pre-npm-2.x packages (e.g. `ansi-html-community@0.0.8`) ship
/// `"engines": ["node >= 0.8.0"]` as an array; npm preserves that
/// shape verbatim in v2/v3 lockfiles. Without tolerant parsing, a
/// single such entry blows up the whole `aube ci`. Normalize to an
/// empty map (matches what modern npm does for engine-strict on
/// the array shape) so the install proceeds.
#[test]
fn test_parse_legacy_array_engines() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "ansi-html-community": "0.0.8" }
                },
                "node_modules/ansi-html-community": {
                    "version": "0.0.8",
                    "integrity": "sha512-aaa",
                    "engines": ["node >= 0.8.0"]
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["ansi-html-community@0.0.8"];
    // Array shape gets normalized to an empty map — same as the
    // manifest parser, and same as what modern npm honors for the
    // engine-strict check on the array form.
    assert!(pkg.engines.is_empty());
}

/// npm writes `"h3-v2": "npm:h3@..."` aliases as a packages entry
/// at `node_modules/h3-v2` with `name: "h3"` and the real registry
/// `resolved:` URL. Aube keys the graph on the *alias* (so
/// `node_modules/h3-v2` ends up at `.aube/h3-v2@.../node_modules/h3-v2`)
/// but remembers the real package name in `alias_of` so fetches
/// and store-index lookups use the URL that actually exists.
#[test]
fn test_parse_npm_alias_dependency() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "h3-v2": "npm:h3@2.0.1-rc.20" }
                },
                "node_modules/h3-v2": {
                    "name": "h3",
                    "version": "2.0.1-rc.20",
                    "resolved": "https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz",
                    "integrity": "sha512-aliased"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(graph.packages.len(), 1);
    // Graph key and LockedPackage.name both carry the alias —
    // that's what consumers (and the linker's folder-name logic)
    // refer to when they say "h3-v2".
    let pkg = graph
        .packages
        .get("h3-v2@2.0.1-rc.20")
        .expect("aliased entry should be keyed by the alias dep_path");
    assert_eq!(pkg.name, "h3-v2");
    assert_eq!(pkg.version, "2.0.1-rc.20");
    assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
    assert_eq!(pkg.registry_name(), "h3");
    // `resolved:` round-trips into `tarball_url` so the fetcher
    // skips re-deriving from the alias-qualified name (which
    // would 404 the registry).
    assert_eq!(
        pkg.tarball_url.as_deref(),
        Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "h3-v2");
    assert_eq!(root[0].dep_path, "h3-v2@2.0.1-rc.20");
}

/// Non-aliased entries (the common case) leave `alias_of` unset
/// and `registry_name()` degenerates to `name`. Regression guard
/// against over-aggressive alias detection that would flag every
/// entry carrying an explicit `name:` field (npm sometimes emits
/// one for non-aliased roots too).
#[test]
fn test_parse_non_alias_preserves_empty_alias_of() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" }
                },
                "node_modules/foo": {
                    "name": "foo",
                    "version": "1.2.3",
                    "integrity": "sha512-foo"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["foo@1.2.3"];
    assert_eq!(pkg.name, "foo");
    assert!(pkg.alias_of.is_none());
    assert_eq!(pkg.registry_name(), "foo");
    assert!(pkg.tarball_url.is_none());
}

/// Round-trip: writer must emit `name:` and `resolved:` for the
/// aliased entry so a subsequent `parse()` still recognizes it as
/// an alias. Without both fields the re-parser would see
/// `node_modules/h3-v2` with no `name:` and treat it as a plain
/// package called `h3-v2` — which doesn't exist on the registry.
#[test]
fn test_write_roundtrip_npm_alias() {
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        "h3-v2@2.0.1-rc.20".to_string(),
        LockedPackage {
            name: "h3-v2".to_string(),
            version: "2.0.1-rc.20".to_string(),
            integrity: Some("sha512-aliased".to_string()),
            dep_path: "h3-v2@2.0.1-rc.20".to_string(),
            alias_of: Some("h3".to_string()),
            tarball_url: Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz".to_string()),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "h3-v2".to_string(),
            dep_path: "h3-v2@2.0.1-rc.20".to_string(),
            dep_type: DepType::Production,
            specifier: Some("npm:h3@2.0.1-rc.20".to_string()),
        }],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains("\"name\": \"h3\""),
        "expected `name: h3` emitted for aliased entry; got:\n{body}"
    );
    assert!(
        body.contains("\"resolved\": \"https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz\""),
        "expected `resolved:` URL emitted for aliased entry; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let pkg = &reparsed.packages["h3-v2@2.0.1-rc.20"];
    assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
    assert_eq!(pkg.registry_name(), "h3");
}

/// npm v7+ writes `peerDependencies` / `peerDependenciesMeta` onto
/// every package entry. The parser must populate the matching
/// `LockedPackage` fields so the resolver's `apply_peer_contexts`
/// pass (run on npm-lockfile installs to wire peer siblings in the
/// isolated virtual store) actually has peer info to work with.
/// Before this parser change, peer-dependent packages like
/// `@tanstack/devtools-vite` would install without a sibling
/// `vite` link and die at runtime.
#[test]
fn test_parse_peer_dependencies() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "peer-test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "peer-test",
                    "version": "1.0.0",
                    "dependencies": { "devtools-vite": "0.6.0", "vite": "8.0.0" }
                },
                "node_modules/devtools-vite": {
                    "version": "0.6.0",
                    "integrity": "sha512-a",
                    "peerDependencies": {
                        "vite": "^6.0.0 || ^7.0.0 || ^8.0.0"
                    },
                    "peerDependenciesMeta": {
                        "vite": { "optional": false }
                    }
                },
                "node_modules/vite": {
                    "version": "8.0.0",
                    "integrity": "sha512-b"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let devtools = &graph.packages["devtools-vite@0.6.0"];
    assert_eq!(
        devtools.peer_dependencies.get("vite").map(String::as_str),
        Some("^6.0.0 || ^7.0.0 || ^8.0.0")
    );
    assert_eq!(
        devtools
            .peer_dependencies_meta
            .get("vite")
            .map(|m| m.optional),
        Some(false)
    );
}

/// Packages without peer fields keep both maps empty — guard
/// against accidental defaulting to `optional: true` or spurious
/// keys showing up in the LockedPackage from serde leak paths.
#[test]
fn test_parse_no_peer_fields_stays_empty() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "no-peers",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "no-peers", "version": "1.0.0", "dependencies": { "foo": "1.0.0" } },
                "node_modules/foo": { "version": "1.0.0", "integrity": "sha512-x" }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert!(foo.peer_dependencies.is_empty());
    assert!(foo.peer_dependencies_meta.is_empty());
}

/// Writer round-trips `peerDependencies` so a second `parse()` on
/// the rewritten lockfile still feeds the peer-context pass. The
/// install path writes out the lockfile after every install; if
/// peers vanished on the first write-back, the *next* install
/// would ship without peer siblings again.
#[test]
fn test_write_roundtrip_peer_dependencies() {
    let mut graph = LockfileGraph::default();
    let mut peer_deps = BTreeMap::new();
    peer_deps.insert("vite".to_string(), "^6.0.0 || ^7.0.0 || ^8.0.0".to_string());
    // Include an `optional: true` entry so the round-trip covers
    // `peerDependenciesMeta` — without it, the writer's meta
    // block isn't exercised and the round-trip would silently
    // re-flag the peer as required on every subsequent install
    // (see `hoist_auto_installed_peers` + `detect_unmet_peers`,
    // which key off `optional`).
    let mut peer_deps_meta = BTreeMap::new();
    peer_deps_meta.insert("vite".to_string(), crate::PeerDepMeta { optional: true });
    graph.packages.insert(
        "devtools-vite@0.6.0".to_string(),
        LockedPackage {
            name: "devtools-vite".to_string(),
            version: "0.6.0".to_string(),
            integrity: Some("sha512-a".to_string()),
            dep_path: "devtools-vite@0.6.0".to_string(),
            peer_dependencies: peer_deps,
            peer_dependencies_meta: peer_deps_meta,
            ..Default::default()
        },
    );
    graph.packages.insert(
        "vite@8.0.0".to_string(),
        LockedPackage {
            name: "vite".to_string(),
            version: "8.0.0".to_string(),
            integrity: Some("sha512-b".to_string()),
            dep_path: "vite@8.0.0".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "devtools-vite".to_string(),
                dep_path: "devtools-vite@0.6.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "vite".to_string(),
                dep_path: "vite@8.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );

    let manifest = test_manifest();
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains("\"peerDependencies\""),
        "expected peerDependencies block to round-trip; got:\n{body}"
    );
    assert!(
        body.contains("\"peerDependenciesMeta\""),
        "expected peerDependenciesMeta block to round-trip; got:\n{body}"
    );

    let reparsed = parse(out.path()).unwrap();
    let devtools = &reparsed.packages["devtools-vite@0.6.0"];
    assert_eq!(
        devtools.peer_dependencies.get("vite").map(String::as_str),
        Some("^6.0.0 || ^7.0.0 || ^8.0.0")
    );
    assert_eq!(
        devtools
            .peer_dependencies_meta
            .get("vite")
            .map(|m| m.optional),
        Some(true),
        "peerDependenciesMeta.optional must survive write → parse round-trip"
    );
}

#[test]
fn test_parse_npm_workspace_importers() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["web"]
                },
                "node_modules/mise-versions-web": {
                    "resolved": "web",
                    "link": true
                },
                "web": {
                    "name": "mise-versions-web",
                    "version": "0.0.1",
                    "dependencies": { "astro": "^6.0.0" },
                    "devDependencies": { "vite": "^7.3.2" }
                },
                "web/node_modules/astro": {
                    "version": "6.2.1",
                    "integrity": "sha512-astro"
                },
                "web/node_modules/vite": {
                    "version": "7.3.2",
                    "integrity": "sha512-vite"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "mise-versions-web");
    assert!(matches!(
        graph.packages[&root[0].dep_path].local_source,
        Some(LocalSource::Link(_))
    ));

    let web = graph.importers.get("web").expect("web importer");
    assert_eq!(web.len(), 2);
    assert!(web.iter().any(|dep| {
        dep.name == "astro"
            && dep.dep_type == DepType::Production
            && dep.specifier.as_deref() == Some("^6.0.0")
    }));
    assert!(web.iter().any(|dep| {
        dep.name == "vite"
            && dep.dep_type == DepType::Dev
            && dep.specifier.as_deref() == Some("^7.3.2")
    }));
}

#[test]
fn test_write_npm_workspace_importers() {
    let mut graph = LockfileGraph::default();
    let web_link = LocalSource::Link(PathBuf::from("web"));
    let web_dep_path = web_link.dep_path("mise-versions-web");
    graph.packages.insert(
        web_dep_path.clone(),
        LockedPackage {
            name: "mise-versions-web".to_string(),
            version: "0.0.1".to_string(),
            dep_path: web_dep_path.clone(),
            local_source: Some(web_link),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "astro@6.2.1".to_string(),
        LockedPackage {
            name: "astro".to_string(),
            version: "6.2.1".to_string(),
            integrity: Some("sha512-astro".to_string()),
            dep_path: "astro@6.2.1".to_string(),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "vite@7.3.2".to_string(),
        LockedPackage {
            name: "vite".to_string(),
            version: "7.3.2".to_string(),
            integrity: Some("sha512-vite".to_string()),
            dep_path: "vite@7.3.2".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "mise-versions-web".to_string(),
            dep_path: web_dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    graph.importers.insert(
        "web".to_string(),
        vec![
            DirectDep {
                name: "astro".to_string(),
                dep_path: "astro@6.2.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^6.0.0".to_string()),
            },
            DirectDep {
                name: "vite".to_string(),
                dep_path: "vite@7.3.2".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^7.3.2".to_string()),
            },
        ],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("workspace-root".to_string()),
        version: Some("1.0.0".to_string()),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(
        json["packages"]["node_modules/mise-versions-web"]["link"],
        true
    );
    assert_eq!(
        json["packages"]["node_modules/mise-versions-web"]["resolved"],
        "web"
    );
    assert_eq!(json["packages"]["web"]["dependencies"]["astro"], "^6.0.0");
    assert_eq!(json["packages"]["web"]["devDependencies"]["vite"], "^7.3.2");
    assert_eq!(
        json["packages"]["web/node_modules/astro"]["version"],
        "6.2.1"
    );
    assert_eq!(
        json["packages"]["web/node_modules/vite"]["version"],
        "7.3.2"
    );

    let reparsed = parse(out.path()).unwrap();
    assert!(reparsed.importers.contains_key("web"));
}

/// When the root tree already hoists a package to
/// `node_modules/<name>`, the workspace tree must NOT emit a
/// redundant `<workspace>/node_modules/<name>` for the same
/// version — Node's upward `node_modules` walk resolves the root
/// copy. Real `npm install` omits the redundant entry, and
/// emitting it produces a diff on every round-trip.
#[test]
fn test_write_npm_workspace_skips_root_hoisted_dups() {
    let mut graph = LockfileGraph::default();
    let web_link = LocalSource::Link(PathBuf::from("web"));
    let web_dep_path = web_link.dep_path("workspace-web");
    graph.packages.insert(
        web_dep_path.clone(),
        LockedPackage {
            name: "workspace-web".to_string(),
            version: "0.0.1".to_string(),
            dep_path: web_dep_path.clone(),
            local_source: Some(web_link),
            ..Default::default()
        },
    );
    graph.packages.insert(
        "astro@6.2.1".to_string(),
        LockedPackage {
            name: "astro".to_string(),
            version: "6.2.1".to_string(),
            integrity: Some("sha512-astro".to_string()),
            dep_path: "astro@6.2.1".to_string(),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "astro".to_string(),
                dep_path: "astro@6.2.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^6.0.0".to_string()),
            },
            DirectDep {
                name: "workspace-web".to_string(),
                dep_path: web_dep_path.clone(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );
    graph.importers.insert(
        "web".to_string(),
        vec![DirectDep {
            name: "astro".to_string(),
            dep_path: "astro@6.2.1".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^6.0.0".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("workspace-root".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("astro".to_string(), "^6.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
    assert_eq!(json["packages"]["node_modules/astro"]["version"], "6.2.1");
    assert!(
        json["packages"].get("web/node_modules/astro").is_none(),
        "redundant workspace-nested astro should not be emitted"
    );
}

/// Byte-parity with a real `npm install`-generated lockfile. The
/// fixture at `tests/fixtures/npm-native.json` was produced by
/// `npm install` (v11) against a `{ chalk, picocolors, semver }`
/// manifest. A parse → write round-trip must reproduce the exact
/// bytes. Covers `resolved:` on every entry, `license:` /
/// `engines:` / `bin:` / `funding:` field preservation, and the
/// sibling declared-range preservation that rides on
/// `declared_dependencies`.
#[test]
fn test_write_byte_identical_to_native_npm() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/npm-native.json");
    // Same LF normalization as the pnpm / bun byte-parity tests —
    // Windows' `core.autocrlf=true` rewrites the checked-out
    // fixture to CRLF even with `.gitattributes eol=lf`.
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");
    let graph = parse(&fixture).unwrap();
    let manifest = aube_manifest::PackageJson {
        name: Some("aube-lockfile-stability".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("chalk".to_string(), "^4.1.2".to_string()),
            ("picocolors".to_string(), "^1.1.1".to_string()),
            ("semver".to_string(), "^7.6.3".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let tmp = tempfile::NamedTempFile::new().unwrap();
    write(tmp.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(tmp.path()).unwrap();

    if written != original {
        panic!(
            "npm writer drifted from native npm output.\n\n--- expected ---\n{original}\n--- got ---\n{written}"
        );
    }
}

#[test]
fn test_parse_workspace_links() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "dependencies": { "@scope/app": "file:packages/app" }
                },
                "node_modules/@scope/app": {
                    "resolved": "packages/app",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "packages/app": {
                    "name": "@scope/app",
                    "version": "0.68.1",
                    "dependencies": {
                        "chalk": "^5.4.1"
                    }
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let dep_path = LocalSource::Link(PathBuf::from("packages/app")).dep_path("@scope/app");

    let importer = &graph.importers["."];
    assert_eq!(importer.len(), 1);
    assert_eq!(importer[0].name, "@scope/app");
    assert_eq!(importer[0].dep_path, dep_path);
    assert!(matches!(importer[0].dep_type, DepType::Production));
    assert!(importer[0].specifier.is_none());

    let app = &graph.packages[&importer[0].dep_path];
    assert_eq!(app.version, "0.68.1");
    assert_eq!(
        app.local_source,
        Some(LocalSource::Link(PathBuf::from("packages/app")))
    );
    assert_eq!(
        app.dependencies.get("chalk").map(String::as_str),
        Some("5.4.1")
    );
    assert!(!graph.packages.contains_key("@scope/app@0.68.1"));
}

/// npm workspaces that aren't listed in the root manifest's
/// `dependencies`/`devDependencies` still get a `node_modules/<name>`
/// link entry in the lockfile — npm symlinks every workspace member
/// at the workspace root regardless. The siemens/element repo
/// (https://github.com/siemens/element) hits this: its 11 workspace
/// projects under `projects/*` aren't declared as deps of the root
/// `package.json`, so the linker had nothing to link at the root and
/// `node_modules/@siemens/element-ng` (and friends) silently went
/// missing — breaking Angular CLI builds that resolve workspace
/// libraries from the repo root.
#[test]
fn test_parse_workspace_links_undeclared_in_root_deps() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["projects/element-ng", "projects/charts-ng"],
                    "dependencies": { "chalk": "^5.4.1" }
                },
                "node_modules/@siemens/element-ng": {
                    "resolved": "projects/element-ng",
                    "link": true
                },
                "node_modules/@siemens/charts-ng": {
                    "resolved": "projects/charts-ng",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "projects/element-ng": {
                    "name": "@siemens/element-ng",
                    "version": "21.0.0"
                },
                "projects/charts-ng": {
                    "name": "@siemens/charts-ng",
                    "version": "21.0.0"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let importer = &graph.importers["."];

    let names: Vec<&str> = importer.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"chalk"));
    assert!(
        names.contains(&"@siemens/element-ng"),
        "workspace package `@siemens/element-ng` should be a direct dep of root \
             so the linker creates `node_modules/@siemens/element-ng`, even though \
             the root manifest doesn't list it; got importer deps {names:?}"
    );
    assert!(
        names.contains(&"@siemens/charts-ng"),
        "workspace package `@siemens/charts-ng` should be a direct dep of root; \
             got importer deps {names:?}"
    );

    // Each workspace dep_path round-trips through LocalSource::Link.
    let element_ng = importer
        .iter()
        .find(|d| d.name == "@siemens/element-ng")
        .unwrap();
    assert_eq!(
        graph.packages[&element_ng.dep_path].local_source,
        Some(LocalSource::Link(PathBuf::from("projects/element-ng")))
    );
}

/// npm copies `funding:` verbatim from each package's
/// `package.json`, so all three registry-permitted shapes (bare
/// string, `{url}` object, mixed array of either) appear in real
/// lockfiles. The pre-fix parser only accepted the object form
/// and would hard-fail on any project pulling in `htmlparser2`,
/// `@csstools/*`, etc. Aube only carries one URL per package, so
/// the contract is "first URL wins, no shape rejected".
#[test]
fn test_parse_funding_all_shapes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {
                        "string-funding": "1.0.0",
                        "object-funding": "1.0.0",
                        "array-funding": "1.0.0",
                        "mixed-array-funding": "1.0.0",
                        "no-funding": "1.0.0"
                    }
                },
                "node_modules/string-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-aaa",
                    "funding": "https://example.com/sponsor"
                },
                "node_modules/object-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-bbb",
                    "funding": { "type": "github", "url": "https://github.com/sponsors/foo" }
                },
                "node_modules/array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ccc",
                    "funding": [
                        { "type": "github", "url": "https://github.com/sponsors/csstools" },
                        { "type": "opencollective", "url": "https://opencollective.com/csstools" }
                    ]
                },
                "node_modules/mixed-array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ddd",
                    "funding": [
                        "https://github.com/fb55/htmlparser2?sponsor=1",
                        { "type": "github", "url": "https://github.com/sponsors/fb55" }
                    ]
                },
                "node_modules/no-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-eee"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(
        graph.packages["string-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://example.com/sponsor"),
    );
    assert_eq!(
        graph.packages["object-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://github.com/sponsors/foo"),
    );
    // Array form: aube collapses to the first URL.
    assert_eq!(
        graph.packages["array-funding@1.0.0"].funding_url.as_deref(),
        Some("https://github.com/sponsors/csstools"),
    );
    // Mixed array (bare string + object): first element is a
    // string, so its value is the URL.
    assert_eq!(
        graph.packages["mixed-array-funding@1.0.0"]
            .funding_url
            .as_deref(),
        Some("https://github.com/fb55/htmlparser2?sponsor=1"),
    );
    assert!(graph.packages["no-funding@1.0.0"].funding_url.is_none());
}

/// Real-world `package-lock.json` entries can carry the legacy
/// object / array-of-objects shapes for `license:` (npm copies
/// whatever's in the package's `package.json` verbatim, and older
/// packages like `tv4` still ship the deprecated forms). Regression
/// guard for https://github.com/endevco/aube/discussions/510.
#[test]
fn test_parse_license_all_shapes() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {
                        "string-license": "1.0.0",
                        "object-license": "1.0.0",
                        "array-license": "1.0.0",
                        "mixed-array-license": "1.0.0",
                        "no-license": "1.0.0"
                    }
                },
                "node_modules/string-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-aaa",
                    "license": "MIT"
                },
                "node_modules/object-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-bbb",
                    "license": { "type": "ISC", "url": "https://example.com/ISC" }
                },
                "node_modules/array-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-ccc",
                    "license": [
                        { "type": "Public Domain", "url": "http://geraintluff.github.io/tv4/LICENSE.txt" },
                        { "type": "MIT", "url": "http://jsonary.com/LICENSE.txt" }
                    ]
                },
                "node_modules/mixed-array-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-ddd",
                    "license": [
                        "MIT",
                        { "type": "Apache-2.0", "url": "https://example.com/apache" }
                    ]
                },
                "node_modules/no-license": {
                    "version": "1.0.0",
                    "integrity": "sha512-eee"
                }
            }
        }"#;
    std::fs::write(tmp.path(), content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    assert_eq!(
        graph.packages["string-license@1.0.0"].license.as_deref(),
        Some("MIT"),
    );
    assert_eq!(
        graph.packages["object-license@1.0.0"].license.as_deref(),
        Some("ISC"),
    );
    // Array form: aube collapses to the first license type.
    assert_eq!(
        graph.packages["array-license@1.0.0"].license.as_deref(),
        Some("Public Domain"),
    );
    // Mixed array (bare string + object): first element is a
    // string, so its value is the license.
    assert_eq!(
        graph.packages["mixed-array-license@1.0.0"]
            .license
            .as_deref(),
        Some("MIT"),
    );
    assert!(graph.packages["no-license@1.0.0"].license.is_none());
}
