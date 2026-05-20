use super::{
    dep_path::{parse_dep_path, version_to_dep_path},
    parse, write,
};
use crate::{
    CatalogEntry, DepType, DirectDep, GitSource, LocalSource, LockedPackage, LockfileGraph,
};
use aube_manifest::PackageJson;
use std::collections::BTreeMap;
use std::path::Path;

#[test]
fn test_parse_dep_path_simple() {
    let (name, version) = parse_dep_path("lodash@4.17.21").unwrap();
    assert_eq!(name, "lodash");
    assert_eq!(version, "4.17.21");
}

#[test]
fn test_parse_dep_path_scoped() {
    let (name, version) = parse_dep_path("@babel/core@7.24.0").unwrap();
    assert_eq!(name, "@babel/core");
    assert_eq!(version, "7.24.0");
}

#[test]
fn test_parse_dep_path_scoped_nested() {
    let (name, version) = parse_dep_path("@types/node@20.11.0").unwrap();
    assert_eq!(name, "@types/node");
    assert_eq!(version, "20.11.0");
}

#[test]
fn test_parse_dep_path_with_leading_slash() {
    let (name, version) = parse_dep_path("/lodash@4.17.21").unwrap();
    assert_eq!(name, "lodash");
    assert_eq!(version, "4.17.21");
}

#[test]
fn test_parse_dep_path_with_peer_suffix() {
    let (name, version) = parse_dep_path("foo@1.0.0(react@18.0.0)").unwrap();
    assert_eq!(name, "foo");
    assert_eq!(version, "1.0.0");
}

#[test]
fn test_parse_dep_path_with_multiple_peer_suffixes() {
    let (name, version) = parse_dep_path("foo@2.0.0(react@18.0.0)(react-dom@18.0.0)").unwrap();
    assert_eq!(name, "foo");
    assert_eq!(version, "2.0.0");
}

#[test]
fn test_parse_dep_path_prerelease() {
    let (name, version) = parse_dep_path("foo@1.0.0-beta.1").unwrap();
    assert_eq!(name, "foo");
    assert_eq!(version, "1.0.0-beta.1");
}

#[test]
fn test_parse_dep_path_no_at() {
    assert!(parse_dep_path("invalid").is_none());
}

#[test]
fn test_version_to_dep_path() {
    assert_eq!(version_to_dep_path("foo", "1.0.0"), "foo@1.0.0");
    assert_eq!(
        version_to_dep_path("@scope/pkg", "2.0.0"),
        "@scope/pkg@2.0.0"
    );
}

#[test]
fn test_parse_fixture_lockfile() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
    if !fixture.exists() {
        return;
    }

    let graph = parse(&fixture).unwrap();

    // Check importers
    let root_deps = graph.importers.get(".").unwrap();
    assert_eq!(root_deps.len(), 2);
    assert!(root_deps.iter().any(|d| d.name == "is-odd"));
    assert!(root_deps.iter().any(|d| d.name == "is-even"));

    // Check packages
    assert_eq!(graph.packages.len(), 7);
    assert!(graph.packages.contains_key("is-odd@3.0.1"));
    assert!(graph.packages.contains_key("is-even@1.0.0"));
    assert!(graph.packages.contains_key("is-buffer@1.1.6"));

    // Check dependencies in snapshots
    let is_odd = graph.packages.get("is-odd@3.0.1").unwrap();
    assert_eq!(is_odd.dependencies.get("is-number").unwrap(), "6.0.0");

    let is_even = graph.packages.get("is-even@1.0.0").unwrap();
    assert_eq!(is_even.dependencies.get("is-odd").unwrap(), "0.1.2");

    // Check integrity hashes exist
    assert!(is_odd.integrity.is_some());
}

#[test]
fn test_parse_fixture_dep_types() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
    if !fixture.exists() {
        return;
    }

    let graph = parse(&fixture).unwrap();
    let root_deps = graph.importers.get(".").unwrap();

    // Both deps in basic fixture are production deps
    for dep in root_deps {
        assert_eq!(dep.dep_type, DepType::Production);
    }
}

#[test]
fn test_parse_fixture_transitive_chain() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
    if !fixture.exists() {
        return;
    }

    let graph = parse(&fixture).unwrap();

    // is-odd@3.0.1 -> is-number@6.0.0 (no further deps)
    let is_odd = graph.packages.get("is-odd@3.0.1").unwrap();
    assert_eq!(is_odd.dependencies.len(), 1);
    let is_number_6 = graph.packages.get("is-number@6.0.0").unwrap();
    assert!(is_number_6.dependencies.is_empty());

    // is-even@1.0.0 -> is-odd@0.1.2 -> is-number@3.0.0 -> kind-of@3.2.2 -> is-buffer@1.1.6
    let is_even = graph.packages.get("is-even@1.0.0").unwrap();
    assert_eq!(is_even.dependencies.get("is-odd").unwrap(), "0.1.2");

    let is_odd_old = graph.packages.get("is-odd@0.1.2").unwrap();
    assert_eq!(is_odd_old.dependencies.get("is-number").unwrap(), "3.0.0");

    let is_number_3 = graph.packages.get("is-number@3.0.0").unwrap();
    assert_eq!(is_number_3.dependencies.get("kind-of").unwrap(), "3.2.2");

    let kind_of = graph.packages.get("kind-of@3.2.2").unwrap();
    assert_eq!(kind_of.dependencies.get("is-buffer").unwrap(), "1.1.6");
}

#[test]
fn parse_normalizes_empty_root_importer_key() {
    // Some pnpm v9 lockfiles in the wild (e.g. npmx.dev) write the
    // root importer as `''` (empty key) rather than `'.'`. Both
    // mean "workspace root" — we must normalize so the linker's
    // `importers.get(".")` lookup still hits.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  '':
    dependencies:
      host:
        specifier: 1.0.0
        version: 1.0.0

packages:
  host@1.0.0:
    resolution: {integrity: sha512-host}

snapshots:
  host@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let root = graph
        .importers
        .get(".")
        .expect("empty-string importer should normalize to `.`");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "host");
    assert!(!graph.importers.contains_key(""));
}

#[test]
fn parse_handles_both_empty_and_dot_root_importer_keys() {
    // Degenerate case pnpm itself never emits: a lockfile with
    // *both* `''` and `'.'` as separate YAML keys for root. The
    // BTreeMap visits `''` first; without the collision guard
    // the real `'.'` entry silently overwrites the normalized
    // empty-key entry and its deps disappear. First-key wins is
    // arbitrary but deterministic; the important behavior is
    // that no deps get silently dropped on the floor.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  '':
    dependencies:
      from-empty:
        specifier: 1.0.0
        version: 1.0.0
  '.':
    dependencies:
      from-dot:
        specifier: 1.0.0
        version: 1.0.0

packages:
  from-empty@1.0.0:
    resolution: {integrity: sha512-empty}
  from-dot@1.0.0:
    resolution: {integrity: sha512-dot}

snapshots:
  from-empty@1.0.0: {}
  from-dot@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let root = graph.importers.get(".").expect("`.` importer present");
    let names: Vec<&str> = root.iter().map(|d| d.name.as_str()).collect();
    // The empty-key entry is visited first and wins; the `.`
    // entry's deps are ignored (rather than silently clobbering).
    assert_eq!(names, vec!["from-empty"]);
    assert!(!graph.importers.contains_key(""));
}

#[test]
fn parse_snapshot_optional_dependencies_as_edges() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      host:
        specifier: 1.0.0
        version: 1.0.0

packages:
  host@1.0.0:
    resolution: {integrity: sha512-host}

  native@1.0.0:
    resolution: {integrity: sha512-native}
    cpu: [arm64]
    os: [darwin]

snapshots:
  host@1.0.0:
    optionalDependencies:
      native: 1.0.0

  native@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let host = graph.packages.get("host@1.0.0").unwrap();
    assert_eq!(host.dependencies.get("native").unwrap(), "1.0.0");
    assert_eq!(host.optional_dependencies.get("native").unwrap(), "1.0.0");
}

#[test]
fn parse_package_platform_fields_accept_scalar_strings() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      sass-embedded-linux-arm64:
        specifier: 1.99.0
        version: 1.99.0

packages:
  sass-embedded-linux-arm64@1.99.0:
    resolution: {integrity: sha512-native}
    engines: {node: '>=14.0.0'}
    cpu: arm64
    os: linux
    libc: glibc

snapshots:
  sass-embedded-linux-arm64@1.99.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let pkg = graph
        .packages
        .get("sass-embedded-linux-arm64@1.99.0")
        .unwrap();
    assert_eq!(pkg.os.as_slice(), &["linux".to_string()]);
    assert_eq!(pkg.cpu.as_slice(), &["arm64".to_string()]);
    assert_eq!(pkg.libc.as_slice(), &["glibc".to_string()]);
}

#[test]
fn parse_local_snapshot_optional_dependencies_as_edges() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &lockfile_path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      local-host:
        specifier: file:./local-host
        version: file:./local-host

packages:
  local-host@file:./local-host:
    resolution: {directory: ./local-host, type: directory}

  native@1.0.0:
    resolution: {integrity: sha512-native}
    cpu: [arm64]
    os: [darwin]

snapshots:
  local-host@file:./local-host:
    optionalDependencies:
      native: 1.0.0

  native@1.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let local = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "local-host")
        .unwrap();
    assert_eq!(local.dependencies.get("native").unwrap(), "1.0.0");
    assert_eq!(local.optional_dependencies.get("native").unwrap(), "1.0.0");
}

#[test]
fn parse_transitive_url_entry_uses_pnpm_version_field() {
    // Regression: pnpm writes non-registry transitive entries with
    // the tarball URL in the dep-path key and the real semver in a
    // `version:` field. Parsing used the URL as the `version`
    // itself, and the install path's store-content cross-check then
    // compared the URL against the tarball's declared `2.4.1` and
    // failed every override'd github dep.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      xml2json:
        specifier: ^0.12.0
        version: 0.12.0

packages:
  xml2json@0.12.0:
    resolution: {integrity: sha512-xxx}

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65:
    resolution: {tarball: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65}
    version: 2.4.1

snapshots:
  xml2json@0.12.0:
    dependencies:
      node-expat: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65: {}
"#,
        )
        .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let url = "https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65";
    let pkg = graph
        .packages
        .get(&format!("node-expat@{url}"))
        .expect("transitive remote-tarball entry present");
    assert_eq!(pkg.name, "node-expat");
    // pnpm's `version:` field, not the URL.
    assert_eq!(pkg.version, "2.4.1");
    // The URL drives the fetch path via `tarball_url`; dep-path
    // still carries the URL so xml2json's snapshot reference
    // resolves.
    assert_eq!(pkg.tarball_url.as_deref(), Some(url));
}

#[test]
fn url_dep_path_round_trips_with_pnpm_version_field() {
    // Write-side companion: the URL has to stay in the canonical
    // key and the `version:` field has to reappear in the written
    // output so tooling reading the file back sees the same shape
    // pnpm wrote.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let src = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      xml2json:
        specifier: ^0.12.0
        version: 0.12.0

packages:

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65:
    resolution: {tarball: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65}
    version: 2.4.1

  xml2json@0.12.0:
    resolution: {integrity: sha512-xxx}

snapshots:

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65: {}

  xml2json@0.12.0:
    dependencies:
      node-expat: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65
"#;
    std::fs::write(&lockfile_path, src).unwrap();
    let graph = parse(&lockfile_path).unwrap();

    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: [("xml2json".to_string(), "^0.12.0".to_string())]
            .into_iter()
            .collect(),
        ..PackageJson::default()
    };
    let out_path = dir.path().join("round-trip.yaml");
    write(&out_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out_path).unwrap();
    assert!(
            written.contains("node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65:"),
            "URL canonical key missing from output: {written}"
        );
    assert!(
        written.contains("    version: 2.4.1"),
        "`version:` field missing from output: {written}"
    );
    // Round-trip must preserve the `resolution: {tarball: …}` block.
    // URL-keyed transitives typically have no integrity, so gating
    // the block on `pkg.integrity` would silently drop the tarball
    // URL and a re-parse would have no way to fetch the package.
    assert!(
            written.contains("resolution: {tarball: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65}"),
            "`resolution: {{tarball: …}}` missing from output: {written}"
        );
    // Re-parse the written lockfile and assert the tarball URL
    // makes it all the way back onto `LockedPackage.tarball_url`.
    let reparsed = parse(&out_path).unwrap();
    let url = "https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65";
    let pkg = reparsed
        .packages
        .get(&format!("node-expat@{url}"))
        .expect("URL-keyed entry survives round-trip");
    assert_eq!(pkg.version, "2.4.1");
    assert_eq!(pkg.tarball_url.as_deref(), Some(url));
}

#[test]
fn direct_url_importer_strips_peer_suffix_from_fetch_url() {
    // Regression: when a direct dep's importer `version:` is a
    // tarball URL *with* a pnpm peer-context suffix
    // (`(peer@ver)`), the parser used to bake the whole string
    // into `RemoteTarballSource.url`, so the install path fetched
    // `…/tar.gz/SHA(peer@ver)` and hit a 404.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      dep-a:
        specifier: github:owner/dep-a#abcdef1234567890abcdef1234567890abcdef12
        version: https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12(encoding@0.1.13)

packages:
  dep-a@https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12:
    resolution: {tarball: https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12}
    version: 1.0.0

  encoding@0.1.13:
    resolution: {integrity: sha512-enc}

snapshots:
  dep-a@https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12(encoding@0.1.13):
    dependencies:
      encoding: 0.1.13

  encoding@0.1.13: {}
"#,
        )
        .unwrap();

    let graph = parse(&lockfile_path).unwrap();
    let clean_url =
        "https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12";

    let dep_a = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "dep-a")
        .expect("dep-a present after parse");
    match dep_a.local_source.as_ref() {
        Some(LocalSource::RemoteTarball(t)) => {
            assert_eq!(
                t.url, clean_url,
                "peer suffix leaked into RemoteTarballSource.url — fetch would 404"
            );
        }
        other => panic!("expected RemoteTarball, got {other:?}"),
    }
    // The snapshot carrying the peer suffix shouldn't produce a
    // second entry — that would round-trip as a stray packages
    // block.
    let dep_a_entries: Vec<_> = graph
        .packages
        .values()
        .filter(|p| p.name == "dep-a")
        .collect();
    assert_eq!(
        dep_a_entries.len(),
        1,
        "exactly one dep-a entry expected (suffix'd snapshot should fold into the local)"
    );
    // Transitive deps declared on the peer-context'd snapshot flow
    // onto the local package.
    assert_eq!(
        dep_a.dependencies.get("encoding"),
        Some(&"0.1.13".to_string())
    );
}

#[test]
fn test_write_and_reparse_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    // Build a graph
    let mut packages = BTreeMap::new();
    let mut foo_deps = BTreeMap::new();
    foo_deps.insert("bar".to_string(), "2.0.0".to_string());
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-abc123==".to_string()),
            dependencies: foo_deps,
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            integrity: Some("sha512-def456==".to_string()),
            dependencies: BTreeMap::new(),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: Some("^1.0.0".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let mut deps = BTreeMap::new();
    deps.insert("foo".to_string(), "^1.0.0".to_string());
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: deps,
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    // Re-parse and verify
    let reparsed = parse(&lockfile_path).unwrap();
    assert_eq!(reparsed.packages.len(), 2);
    assert_eq!(
        reparsed.packages.get("foo@1.0.0").unwrap().integrity,
        Some("sha512-abc123==".to_string())
    );
    assert_eq!(
        reparsed
            .packages
            .get("foo@1.0.0")
            .unwrap()
            .dependencies
            .get("bar")
            .unwrap(),
        "2.0.0"
    );

    let root_deps = reparsed.importers.get(".").unwrap();
    assert_eq!(root_deps.len(), 1);
    assert_eq!(root_deps[0].name, "foo");
    assert_eq!(root_deps[0].dep_type, DepType::Production);
}

#[test]
fn test_write_prunes_time_to_direct_importer_deps() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-foo==".to_string()),
            dependencies: [("bar".to_string(), "2.0.0".to_string())]
                .into_iter()
                .collect(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            integrity: Some("sha512-bar==".to_string()),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );

    let graph = LockfileGraph {
        importers: [(
            ".".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            }],
        )]
        .into_iter()
        .collect(),
        packages,
        times: [
            (
                "foo@1.0.0".to_string(),
                "2026-01-01T00:00:00.000Z".to_string(),
            ),
            (
                "bar@2.0.0".to_string(),
                "2026-01-02T00:00:00.000Z".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &PackageJson::default()).unwrap();
    let written = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(written.contains("\n  foo@1.0.0: 2026-01-01T00:00:00.000Z\n"));
    assert!(!written.contains("\n  bar@2.0.0: 2026-01-02T00:00:00.000Z\n"));
}

#[test]
fn test_write_preserves_real_name_time_for_aube_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("aube-lock.yaml");

    let graph = LockfileGraph {
        importers: [(
            ".".to_string(),
            vec![DirectDep {
                name: "alias-pkg".to_string(),
                dep_path: "alias-pkg@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("npm:real-pkg@^1.0.0".to_string()),
            }],
        )]
        .into_iter()
        .collect(),
        packages: [(
            "alias-pkg@1.0.0".to_string(),
            LockedPackage {
                name: "alias-pkg".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-alias==".to_string()),
                dep_path: "alias-pkg@1.0.0".to_string(),
                alias_of: Some("real-pkg".to_string()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        times: [(
            "real-pkg@1.0.0".to_string(),
            "2026-01-01T00:00:00.000Z".to_string(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &PackageJson::default()).unwrap();
    let written = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(written.contains("\n  alias-pkg@1.0.0: 2026-01-01T00:00:00.000Z\n"));
    assert!(!written.contains("\n  real-pkg@1.0.0: 2026-01-01T00:00:00.000Z\n"));
}

#[test]
fn writer_preserves_workspace_importer_specifiers() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "@dev/build-tools@1.0.0".to_string(),
        LockedPackage {
            name: "@dev/build-tools".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "@dev/build-tools@1.0.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "@dev/build-tools".to_string(),
            dep_path: "@dev/build-tools@1.0.0".to_string(),
            dep_type: DepType::Dev,
            specifier: Some("^1.0.0".to_string()),
        }],
    );
    importers.insert(
        "packages/public/umd/babylonjs".to_string(),
        vec![DirectDep {
            name: "@dev/build-tools".to_string(),
            dep_path: "@dev/build-tools@1.0.0".to_string(),
            dep_type: DepType::Dev,
            specifier: Some("1.0.0".to_string()),
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let mut root_dev_dependencies = BTreeMap::new();
    root_dev_dependencies.insert("@dev/build-tools".to_string(), "^1.0.0".to_string());
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: BTreeMap::new(),
        dev_dependencies: root_dev_dependencies,
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lockfile_path).unwrap();
    let workspace_deps = reparsed
        .importers
        .get("packages/public/umd/babylonjs")
        .unwrap();
    assert_eq!(workspace_deps[0].specifier.as_deref(), Some("1.0.0"));
}

#[test]
fn overrides_round_trip_through_pnpm_lock_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut overrides = BTreeMap::new();
    overrides.insert("lodash".to_string(), "4.17.21".to_string());
    overrides.insert("foo".to_string(), "npm:bar@^2".to_string());

    let graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages: BTreeMap::new(),
        overrides,
        ..Default::default()
    };

    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    // The serialized YAML must contain an `overrides:` block — guard
    // against a future serde change silently dropping the field.
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("overrides:"),
        "expected `overrides:` block in:\n{yaml}"
    );

    let reparsed = parse(&lockfile_path).unwrap();
    assert_eq!(reparsed.overrides.len(), 2);
    assert_eq!(reparsed.overrides.get("lodash").unwrap(), "4.17.21");
    assert_eq!(reparsed.overrides.get("foo").unwrap(), "npm:bar@^2");
}

/// `patchedDependencies:` must land between `overrides:` and
/// `catalogs:` in the emitted YAML — that's where pnpm itself
/// writes it, and any other position produces a gratuitous diff
/// against pnpm's output on every install.
#[test]
fn patched_dependencies_emitted_after_overrides_before_catalogs() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut overrides = BTreeMap::new();
    overrides.insert("lodash".to_string(), "4.17.21".to_string());
    let mut patched_dependencies = BTreeMap::new();
    patched_dependencies.insert(
        "lodash@4.17.21".to_string(),
        "patches/lodash@4.17.21.patch".to_string(),
    );
    let mut default_catalog = BTreeMap::new();
    default_catalog.insert(
        "react".to_string(),
        CatalogEntry {
            specifier: "^18.2.0".to_string(),
            version: "18.2.0".to_string(),
        },
    );
    let mut catalogs = BTreeMap::new();
    catalogs.insert("default".to_string(), default_catalog);

    let graph = LockfileGraph {
        overrides,
        patched_dependencies,
        catalogs,
        ..Default::default()
    };

    let manifest = PackageJson {
        name: Some("test".to_string()),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    let overrides_at = yaml.find("overrides:").expect("overrides:");
    let patched_at = yaml
        .find("patchedDependencies:")
        .expect("patchedDependencies:");
    let catalogs_at = yaml.find("catalogs:").expect("catalogs:");
    assert!(
        overrides_at < patched_at && patched_at < catalogs_at,
        "expected order: overrides < patchedDependencies < catalogs, got\n{yaml}"
    );
}

#[test]
fn empty_overrides_block_omitted_from_yaml() {
    // Default-empty overrides should not introduce an `overrides:` key
    // in the lockfile — important for byte-identical parity with pnpm
    // on the no-overrides path.
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let graph = LockfileGraph::default();
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };
    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        !yaml.contains("overrides:"),
        "unexpected overrides block:\n{yaml}"
    );
}

#[test]
fn test_write_dev_and_optional_deps() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    for (name, ver) in [("foo", "1.0.0"), ("bar", "2.0.0"), ("baz", "3.0.0")] {
        packages.insert(
            format!("{name}@{ver}"),
            LockedPackage {
                name: name.to_string(),
                version: ver.to_string(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: format!("{name}@{ver}"),
                ..Default::default()
            },
        );
    }

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            },
            DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@2.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^2.0.0".to_string()),
            },
            DirectDep {
                name: "baz".to_string(),
                dep_path: "baz@3.0.0".to_string(),
                dep_type: DepType::Optional,
                specifier: Some("^3.0.0".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let mut deps = BTreeMap::new();
    deps.insert("foo".to_string(), "^1.0.0".to_string());
    let mut dev_deps = BTreeMap::new();
    dev_deps.insert("bar".to_string(), "^2.0.0".to_string());
    let mut opt_deps = BTreeMap::new();
    opt_deps.insert("baz".to_string(), "^3.0.0".to_string());

    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: deps,
        dev_dependencies: dev_deps,
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: opt_deps,
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lockfile_path).unwrap();
    let root_deps = reparsed.importers.get(".").unwrap();
    assert_eq!(root_deps.len(), 3);

    let bar = root_deps.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(bar.dep_type, DepType::Dev);

    let baz = root_deps.iter().find(|d| d.name == "baz").unwrap();
    assert_eq!(baz.dep_type, DepType::Optional);
}

#[test]
fn test_catalogs_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut default_cat = BTreeMap::new();
    default_cat.insert(
        "react".to_string(),
        CatalogEntry {
            specifier: "^18.0.0".to_string(),
            version: "18.2.0".to_string(),
        },
    );
    let mut catalogs = BTreeMap::new();
    catalogs.insert("default".to_string(), default_cat);

    let graph = LockfileGraph {
        catalogs,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };
    write(&lockfile_path, &graph, &manifest).unwrap();

    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("catalogs:"),
        "missing catalogs section: {yaml}"
    );
    assert!(yaml.contains("react"), "missing entry: {yaml}");

    let reparsed = parse(&lockfile_path).unwrap();
    let entry = reparsed
        .catalogs
        .get("default")
        .and_then(|c| c.get("react"))
        .expect("react catalog entry");
    assert_eq!(entry.specifier, "^18.0.0");
    assert_eq!(entry.version, "18.2.0");
}

#[test]
fn ignored_optional_dependencies_section_matches_pnpm_order() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut ignored_optional_dependencies = std::collections::BTreeSet::new();
    ignored_optional_dependencies.insert("fsevents".to_string());

    let mut default_cat = BTreeMap::new();
    default_cat.insert(
        "react".to_string(),
        CatalogEntry {
            specifier: "^18.0.0".to_string(),
            version: "18.2.0".to_string(),
        },
    );
    let mut catalogs = BTreeMap::new();
    catalogs.insert("default".to_string(), default_cat);

    let graph = LockfileGraph {
        ignored_optional_dependencies,
        catalogs,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("test".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };
    write(&lockfile_path, &graph, &manifest).unwrap();

    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    let catalogs = yaml.find("\ncatalogs:").expect("missing catalogs");
    let importers = yaml.find("\nimporters:").expect("missing importers");
    let packages = yaml.find("\npackages:").expect("missing packages");
    let ignored = yaml
        .find("\nignoredOptionalDependencies:")
        .expect("missing ignoredOptionalDependencies");
    let snapshots = yaml.find("\nsnapshots:").expect("missing snapshots");

    assert!(
        catalogs < importers && importers < packages && packages < ignored && ignored < snapshots,
        "unexpected pnpm section order:\n{yaml}"
    );
}

// Build a graph with one `link:` dep and one registry dep, write it
// with `excludeLinksFromLockfile: true`, and confirm the `link:`
// entry vanishes from the importer's `dependencies:` map while the
// registry dep survives. Guards the filter in the importer loop.
#[test]
fn exclude_links_from_lockfile_drops_link_deps_from_importer() {
    use crate::{LocalSource, LockfileSettings};
    use std::path::PathBuf;

    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let mut packages = BTreeMap::new();
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some("sha512-abc==".to_string()),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "sibling@link:../sibling".to_string(),
        LockedPackage {
            name: "sibling".to_string(),
            version: "0.0.0".to_string(),
            dep_path: "sibling@link:../sibling".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("../sibling"))),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            },
            DirectDep {
                name: "sibling".to_string(),
                dep_path: "sibling@link:../sibling".to_string(),
                dep_type: DepType::Production,
                specifier: Some("link:../sibling".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        settings: LockfileSettings {
            auto_install_peers: true,
            exclude_links_from_lockfile: true,
            lockfile_include_tarball_url: false,
        },
        ..Default::default()
    };

    let mut deps = BTreeMap::new();
    deps.insert("foo".to_string(), "^1.0.0".to_string());
    deps.insert("sibling".to_string(), "link:../sibling".to_string());
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: deps,
        dev_dependencies: BTreeMap::new(),
        peer_dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        update_config: None,
        scripts: BTreeMap::new(),
        engines: BTreeMap::new(),
        workspaces: None,
        bundled_dependencies: None,
        extra: BTreeMap::new(),
    };

    write(&lockfile_path, &graph, &manifest).unwrap();

    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("excludeLinksFromLockfile: true"),
        "settings header must record the flag: {yaml}"
    );
    assert!(
        !yaml.contains("sibling:"),
        "sibling link dep should be filtered out of importers: {yaml}"
    );
    assert!(
        yaml.contains("foo:"),
        "registry dep foo must still appear: {yaml}"
    );

    // Sanity: with the flag off, the same graph keeps the link dep.
    let graph_off = LockfileGraph {
        settings: LockfileSettings::default(),
        ..graph
    };
    write(&lockfile_path, &graph_off, &manifest).unwrap();
    let yaml_off = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml_off.contains("sibling:"),
        "with flag off, sibling must reappear: {yaml_off}"
    );
}

#[test]
fn writer_uses_pnpm_resolution_types_for_portal_and_exec() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");

    let portal_source = LocalSource::Portal(std::path::PathBuf::from("./packages/portal"));
    let exec_source = LocalSource::Exec(std::path::PathBuf::from("./scripts/generate-exec.js"));
    let portal_dep_path = portal_source.dep_path("portal-pkg");
    let exec_dep_path = exec_source.dep_path("exec-pkg");

    let mut packages = BTreeMap::new();
    packages.insert(
        portal_dep_path.clone(),
        LockedPackage {
            name: "portal-pkg".to_string(),
            version: "1.0.0".to_string(),
            dep_path: portal_dep_path.clone(),
            local_source: Some(portal_source),
            ..Default::default()
        },
    );
    packages.insert(
        exec_dep_path.clone(),
        LockedPackage {
            name: "exec-pkg".to_string(),
            version: "2.0.0".to_string(),
            dep_path: exec_dep_path.clone(),
            local_source: Some(exec_source),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "portal-pkg".to_string(),
                dep_path: portal_dep_path,
                dep_type: DepType::Production,
                specifier: Some("portal:./packages/portal".to_string()),
            },
            DirectDep {
                name: "exec-pkg".to_string(),
                dep_path: exec_dep_path,
                dep_type: DepType::Production,
                specifier: Some("exec:./scripts/generate-exec.js".to_string()),
            },
        ],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        ..Default::default()
    };

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

    assert!(
        yaml.contains("portal-pkg@portal:./packages/portal:"),
        "portal package should be written:\n{yaml}"
    );
    assert!(
        yaml.contains("resolution: {directory: ./packages/portal, type: directory}"),
        "portal should use pnpm's directory resolution type:\n{yaml}"
    );
    assert!(
        !yaml.contains("exec-pkg@exec:./scripts/generate-exec.js:"),
        "exec packages should be omitted from pnpm packages entries:\n{yaml}"
    );
    assert!(
        !yaml.contains("type: portal") && !yaml.contains("type: exec"),
        "pnpm lockfiles must not contain non-standard local source types:\n{yaml}"
    );
}

#[test]
fn test_parse_invalid_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, "{{{{not yaml").unwrap();
    assert!(parse(&path).is_err());
}

#[test]
fn test_parse_nonexistent_file() {
    let path = Path::new("/nonexistent/pnpm-lock.yaml");
    assert!(parse(path).is_err());
}

// Byte-parity with a real pnpm-lock.yaml. The fixture was produced by
// `pnpm install` against a `{ chalk, picocolors, semver }` manifest and
// lightly pinned — if pnpm's own output format drifts in a future
// release, regenerate the fixture rather than loosening the assertion.
// The test guards against silent regressions in the four churn sources
// we fixed: stray `time:`, block-form `resolution:`, missing blank
// lines, and dropped `engines:` / `hasBin:`.
#[test]
fn test_write_byte_identical_to_native_pnpm() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pnpm-native.yaml");
    // Windows' `core.autocrlf=true` rewrites checked-out files to
    // CRLF even when `.gitattributes` asks for LF; normalize both
    // sides before comparing so a misconfigured checkout gets a
    // meaningful failure rather than a line-ending false positive.
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");

    let graph = parse(&fixture).unwrap();
    let manifest = PackageJson {
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

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("pnpm-lock.yaml");
    write(&out, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();

    if written != original {
        // pretty-print a short contextual diff so CI logs are actionable.
        let diff = similar_diff(&original, &written);
        panic!(
            "pnpm writer drifted from native pnpm output:\n{diff}\n\n--- full written output ---\n{written}"
        );
    }
}

// Minimal line diff for the byte-parity test failure message. We don't
// pull in a diff crate just for this — the lockfile is small enough
// that a line-by-line comparison is readable.
/// Line-aligned diff with a bounded lookahead so a single
/// insertion doesn't flag every following line as "modified".
/// When sides diverge at `(i, j)`, scan up to `LOOKAHEAD` steps in
/// both directions for the nearest `al[ii] == bl[jj]` and emit the
/// skipped-over ranges as `- …` / `+ …` runs; that keeps the
/// failure output readable for the ≤100-line fixtures this test
/// exercises without pulling in a full LCS dependency.
fn similar_diff(a: &str, b: &str) -> String {
    const LOOKAHEAD: usize = 8;
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let mut out = String::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < al.len() || j < bl.len() {
        if i < al.len() && j < bl.len() && al[i] == bl[j] {
            i += 1;
            j += 1;
            continue;
        }
        // Find the nearest resync point within the lookahead
        // window. `k` is the combined distance from `(i, j)`;
        // smaller `k` wins, matching how a developer eyeballs
        // the diff.
        let mut sync: Option<(usize, usize)> = None;
        'outer: for k in 1..=LOOKAHEAD {
            for dx in 0..=k {
                let dy = k - dx;
                let ii = i + dx;
                let jj = j + dy;
                if ii < al.len() && jj < bl.len() && al[ii] == bl[jj] {
                    sync = Some((ii, jj));
                    break 'outer;
                }
            }
        }
        match sync {
            Some((ii, jj)) => {
                for line in &al[i..ii] {
                    out.push_str(&format!("  - {line:?}\n"));
                }
                for line in &bl[j..jj] {
                    out.push_str(&format!("  + {line:?}\n"));
                }
                i = ii;
                j = jj;
            }
            None => {
                // No sync in the window — dump the rest and stop.
                for line in &al[i..] {
                    out.push_str(&format!("  - {line:?}\n"));
                }
                for line in &bl[j..] {
                    out.push_str(&format!("  + {line:?}\n"));
                }
                break;
            }
        }
    }
    out
}

#[test]
fn parse_multi_document_lockfile_picks_project_doc() {
    // pnpm v11 emits two YAML documents in one file: a bootstrap
    // doc for `packageManagerDependencies` and the real project
    // lockfile. We want the latter.
    let yaml = r#"---
lockfileVersion: '9.0'

importers:

  .:
    packageManagerDependencies:
      pnpm:
        specifier: 11.0.0-rc.1
        version: 11.0.0-rc.1

packages:

  'pnpm@11.0.0-rc.1':
    resolution: {integrity: sha512-aaa}

snapshots:

  'pnpm@11.0.0-rc.1': {}

---
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true

importers:

  .:
    dependencies:
      lodash:
        specifier: ^4.17.0
        version: 4.17.21

packages:

  'lodash@4.17.21':
    resolution: {integrity: sha512-bbb}

snapshots:

  'lodash@4.17.21': {}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();
    let graph = parse(&path).expect("multi-doc lockfile should parse");
    let root = graph.importers.get(".").expect("root importer");
    let names: Vec<_> = root.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"lodash"),
        "expected lodash from project doc, got {names:?}"
    );
    assert!(
        !names.contains(&"pnpm"),
        "bootstrap doc's packageManagerDependencies should not leak in, got {names:?}"
    );
}

#[test]
fn snapshot_optional_and_transitive_peer_deps_roundtrip() {
    let yaml = r#"lockfileVersion: '9.0'
settings:
  autoInstallPeers: true
importers:
  .:
    dependencies:
      '@reflink/reflink':
        specifier: ^0.1.19
        version: 0.1.19
      '@babel/generator':
        specifier: ^7.29.1
        version: 7.29.1
packages:
  '@reflink/reflink-darwin-arm64@0.1.19':
    resolution: {integrity: sha512-darwin}
    cpu: [arm64]
    os: [darwin]
  '@reflink/reflink@0.1.19':
    resolution: {integrity: sha512-reflink}
  '@babel/generator@7.29.1':
    resolution: {integrity: sha512-gen}
  '@babel/parser@7.29.2':
    resolution: {integrity: sha512-parser}
snapshots:
  '@reflink/reflink-darwin-arm64@0.1.19':
    optional: true
  '@reflink/reflink@0.1.19':
    optionalDependencies:
      '@reflink/reflink-darwin-arm64': 0.1.19
  '@babel/generator@7.29.1':
    dependencies:
      '@babel/parser': 7.29.2
    transitivePeerDependencies:
      - supports-color
  '@babel/parser@7.29.2': {}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();

    let graph = parse(&path).unwrap();
    let darwin = graph
        .packages
        .get("@reflink/reflink-darwin-arm64@0.1.19")
        .expect("darwin snapshot present");
    assert!(darwin.optional, "optional: true must round-trip");

    let generator = graph
        .packages
        .get("@babel/generator@7.29.1")
        .expect("generator snapshot present");
    assert_eq!(
        generator.transitive_peer_dependencies,
        vec!["supports-color".to_string()],
    );

    let parser_pkg = graph.packages.get("@babel/parser@7.29.2").unwrap();
    assert!(!parser_pkg.optional);
    assert!(parser_pkg.transitive_peer_dependencies.is_empty());

    let manifest = PackageJson {
        name: Some("rt".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: [
            ("@reflink/reflink".to_string(), "^0.1.19".to_string()),
            ("@babel/generator".to_string(), "^7.29.1".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let out_path = dir.path().join("out.yaml");
    write(&out_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out_path).unwrap();

    assert!(
        written.contains("optional: true"),
        "writer must emit optional: true; got:\n{written}"
    );
    assert!(
        written.contains("transitivePeerDependencies:"),
        "writer must emit transitivePeerDependencies; got:\n{written}"
    );
    assert!(
        written.contains("- supports-color"),
        "writer must list bubbled peers; got:\n{written}"
    );

    // Field order within a snapshot must match pnpm's
    // `LockfilePackageSnapshot` emit order so a round-trip stays
    // diff-clean against pnpm's own output: dependencies →
    // optionalDependencies → transitivePeerDependencies → optional.
    // The `@babel/generator` snapshot has `dependencies` followed
    // by `transitivePeerDependencies`, which is the pair Greptile
    // flagged as ordered wrong.
    let deps_line = "\n    dependencies:\n";
    let tpd_line = "\n    transitivePeerDependencies:\n";
    let deps_at = written.find(deps_line).expect("dependencies line emitted");
    let tpd_at = written
        .find(tpd_line)
        .expect("transitivePeerDependencies line emitted");
    assert!(
        deps_at < tpd_at,
        "dependencies must precede transitivePeerDependencies; got:\n{written}"
    );

    let reparsed = parse(&out_path).unwrap();
    assert!(
        reparsed
            .packages
            .get("@reflink/reflink-darwin-arm64@0.1.19")
            .unwrap()
            .optional
    );
    assert_eq!(
        reparsed
            .packages
            .get("@babel/generator@7.29.1")
            .unwrap()
            .transitive_peer_dependencies,
        vec!["supports-color".to_string()]
    );
}

#[test]
fn adversarial_native_pnpm_features_roundtrip_together() {
    let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: false
  excludeLinksFromLockfile: false
  lockfileIncludeTarballUrl: true

overrides:
  is-number: 6.0.0
  react: 'catalog:'

patchedDependencies:
  is-odd@3.0.1:
    path: patches/is-odd@3.0.1.patch
    hash: sha256-deadbeef

catalogs:
  default:
    react:
      specifier: ^18.2.0
      version: 18.2.0
  evens:
    is-even:
      specifier: ^1.0.0
      version: 1.0.0

importers:

  .:
    dependencies:
      odd-alias:
        specifier: npm:is-odd@3.0.1
        version: is-odd@3.0.1
      react:
        specifier: 'catalog:'
        version: 18.2.0
    devDependencies:
      peer-host:
        specifier: 1.0.0
        version: 1.0.0(@types/node@20.11.0)
    optionalDependencies:
      fsevents:
        specifier: ^2.3.3
        version: 2.3.3
    skippedOptionalDependencies:
      optional-native:
        specifier: ^1.0.0
        version: 1.0.0

packages:

  '@types/node@20.11.0':
    resolution: {integrity: sha512-types}

  fsevents@2.3.3:
    resolution: {integrity: sha512-fsevents, tarball: https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz}
    os: [darwin]
    cpu: [x64]

  is-number@6.0.0:
    resolution: {integrity: sha512-number}

  is-odd@3.0.1:
    resolution: {integrity: sha512-odd, tarball: https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz}

  peer-host@1.0.0(@types/node@20.11.0):
    resolution: {integrity: sha512-peer}
    peerDependencies:
      '@types/node': '>=20'
    peerDependenciesMeta:
      '@types/node':
        optional: true

  react@18.2.0:
    resolution: {integrity: sha512-react}

ignoredOptionalDependencies:
  - optional-native

snapshots:

  '@types/node@20.11.0': {}

  fsevents@2.3.3:
    optional: true

  is-number@6.0.0: {}

  is-odd@3.0.1:
    dependencies:
      is-number: 6.0.0
    transitivePeerDependencies:
      - '@types/node'

  peer-host@1.0.0(@types/node@20.11.0): {}

  react@18.2.0: {}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(&path, yaml).unwrap();

    let graph = parse(&path).unwrap();

    assert!(!graph.settings.auto_install_peers);
    assert!(graph.settings.lockfile_include_tarball_url);
    assert_eq!(graph.overrides.get("react").unwrap(), "catalog:");
    assert_eq!(
        graph.patched_dependencies.get("is-odd@3.0.1").unwrap(),
        "patches/is-odd@3.0.1.patch"
    );
    assert_eq!(
        graph.catalogs["evens"]["is-even"].specifier, "^1.0.0",
        "named catalogs must survive parse"
    );
    assert!(
        graph
            .ignored_optional_dependencies
            .contains("optional-native")
    );
    assert_eq!(
        graph.skipped_optional_dependencies["."]["optional-native"],
        "^1.0.0"
    );

    let root = graph.importers.get(".").expect("root importer");
    let alias_dep = root.iter().find(|d| d.name == "odd-alias").unwrap();
    assert_eq!(alias_dep.dep_path, "odd-alias@3.0.1");
    assert_eq!(alias_dep.specifier.as_deref(), Some("npm:is-odd@3.0.1"));
    let peer_dep = root.iter().find(|d| d.name == "peer-host").unwrap();
    assert_eq!(peer_dep.dep_type, DepType::Dev);
    let optional_dep = root.iter().find(|d| d.name == "fsevents").unwrap();
    assert_eq!(optional_dep.dep_type, DepType::Optional);

    let alias_pkg = graph.packages.get("odd-alias@3.0.1").unwrap();
    assert_eq!(alias_pkg.alias_of.as_deref(), Some("is-odd"));
    assert_eq!(
        alias_pkg
            .transitive_peer_dependencies
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["@types/node"]
    );
    let fsevents = graph.packages.get("fsevents@2.3.3").unwrap();
    assert!(fsevents.optional);
    assert_eq!(fsevents.os.as_slice(), ["darwin"]);
    assert_eq!(fsevents.cpu.as_slice(), ["x64"]);
    assert_eq!(
        fsevents.tarball_url.as_deref(),
        Some("https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz")
    );
    let peer_host = graph
        .packages
        .get("peer-host@1.0.0(@types/node@20.11.0)")
        .unwrap();
    assert_eq!(peer_host.peer_dependencies["@types/node"], ">=20");
    assert!(peer_host.peer_dependencies_meta["@types/node"].optional);

    let manifest = PackageJson {
        name: Some("adversarial-native-pnpm".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string()),
            ("react".to_string(), "catalog:".to_string()),
        ]
        .into_iter()
        .collect(),
        dev_dependencies: [("peer-host".to_string(), "1.0.0".to_string())]
            .into_iter()
            .collect(),
        optional_dependencies: [("fsevents".to_string(), "^2.3.3".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = dir.path().join("out.yaml");
    write(&out, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&out).unwrap();

    for needle in [
        "lockfileIncludeTarballUrl: true",
        "overrides:",
        "patchedDependencies:",
        "catalogs:",
        "skippedOptionalDependencies:",
        "ignoredOptionalDependencies:",
        "aliasOf: is-odd",
        "peerDependencies:",
        "peerDependenciesMeta:",
        "transitivePeerDependencies:",
        "optional: true",
        "tarball: https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz",
    ] {
        assert!(
            written.contains(needle),
            "missing {needle:?} in:\n{written}"
        );
    }

    let overrides_at = written.find("\noverrides:").expect("overrides");
    let patched_at = written
        .find("\npatchedDependencies:")
        .expect("patchedDependencies");
    let catalogs_at = written.find("\ncatalogs:").expect("catalogs");
    let importers_at = written.find("\nimporters:").expect("importers");
    assert!(
        overrides_at < patched_at && patched_at < catalogs_at && catalogs_at < importers_at,
        "pnpm top-level section order drifted:\n{written}"
    );
    let packages_at = written.find("\npackages:").expect("packages");
    let ignored_at = written
        .find("\nignoredOptionalDependencies:")
        .expect("ignored optional");
    let snapshots_at = written.find("\nsnapshots:").expect("snapshots");
    assert!(
        packages_at < ignored_at && ignored_at < snapshots_at,
        "ignoredOptionalDependencies must stay between packages and snapshots:\n{written}"
    );

    let reparsed = parse(&out).unwrap();
    assert_eq!(
        reparsed
            .patched_dependencies
            .get("is-odd@3.0.1")
            .unwrap_or_else(|| panic!("patched deps lost after reparse:\n{written}")),
        "patches/is-odd@3.0.1.patch"
    );
    assert_eq!(reparsed.catalogs["default"]["react"].version, "18.2.0");
    assert_eq!(
        reparsed
            .packages
            .get("odd-alias@3.0.1")
            .unwrap_or_else(|| panic!("alias package lost after reparse:\n{written}"))
            .alias_of
            .as_deref(),
        Some("is-odd")
    );
    assert!(reparsed.packages.get("fsevents@2.3.3").unwrap().optional);
    assert_eq!(
        reparsed.skipped_optional_dependencies["."]["optional-native"],
        "^1.0.0"
    );
}

#[test]
fn write_pnpm_lockfile_uses_native_alias_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    let manifest = PackageJson {
        name: Some("alias-native-pnpm".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let graph = LockfileGraph {
        importers: [(
            ".".to_string(),
            vec![DirectDep {
                name: "odd-alias".to_string(),
                dep_path: "odd-alias@3.0.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("npm:is-odd@3.0.1".to_string()),
            }],
        )]
        .into_iter()
        .collect(),
        packages: [
            (
                "odd-alias@3.0.1".to_string(),
                LockedPackage {
                    name: "odd-alias".to_string(),
                    version: "3.0.1".to_string(),
                    integrity: Some("sha512-odd".to_string()),
                    dep_path: "odd-alias@3.0.1".to_string(),
                    alias_of: Some("is-odd".to_string()),
                    ..Default::default()
                },
            ),
            (
                "consumer@1.0.0".to_string(),
                LockedPackage {
                    name: "consumer".to_string(),
                    version: "1.0.0".to_string(),
                    integrity: Some("sha512-consumer".to_string()),
                    dep_path: "consumer@1.0.0".to_string(),
                    dependencies: [(
                        "odd-alias".to_string(),
                        "3.0.1(peer-host@1.0.0)".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    write(&path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("version: is-odd@3.0.1"), "{written}");
    assert!(written.contains("is-odd@3.0.1:"), "{written}");
    assert!(
        written.contains("odd-alias: is-odd@3.0.1(peer-host@1.0.0)"),
        "{written}"
    );
    assert!(!written.contains("aliasOf:"), "{written}");

    let reparsed = parse(&path).unwrap();
    let alias_pkg = reparsed.packages.get("odd-alias@3.0.1").unwrap();
    assert_eq!(alias_pkg.alias_of.as_deref(), Some("is-odd"));
}

#[test]
fn parse_synthesizes_npm_alias_from_pnpm_v9_lockfile() {
    // pnpm v9 encodes npm-aliases implicitly (importer key is the
    // alias, `version:` is `<real>@<resolved>`, no `aliasOf:`
    // field on the package entry). The reader must reconstruct
    // an alias-keyed LockedPackage with `alias_of=Some(real)` so
    // the linker creates `node_modules/<alias>` correctly.
    // Repro: https://github.com/rubnogueira/aube-exotic-bug
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      express-fork:
        specifier: npm:express@^4.22.1
        version: express@4.22.1

packages:
  express@4.22.1:
    resolution: {integrity: sha512-fake}
    engines: {node: '>= 0.10.0'}

snapshots:
  express@4.22.1: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();

    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root.len(), 1);
    let dep = &root[0];
    assert_eq!(dep.name, "express-fork", "DirectDep keeps the alias name");
    assert_eq!(
        dep.dep_path, "express-fork@4.22.1",
        "DirectDep dep_path is alias-keyed (not the malformed express-fork@express@4.22.1)"
    );
    assert_eq!(dep.specifier.as_deref(), Some("npm:express@^4.22.1"));

    let pkg = graph
        .packages
        .get("express-fork@4.22.1")
        .expect("synthesized alias-keyed package");
    assert_eq!(pkg.name, "express-fork");
    assert_eq!(pkg.alias_of.as_deref(), Some("express"));
    assert_eq!(pkg.dep_path, "express-fork@4.22.1");
    // Real-keyed entry stays in place — other importers may
    // reference the package directly, and the canonical entry is
    // needed for byte-identical round-trips back to pnpm format.
    let real = graph.packages.get("express@4.22.1").expect("real entry");
    assert_eq!(real.name, "express");
    assert!(real.alias_of.is_none());
}

#[test]
fn parse_synthesizes_npm_alias_from_pnpm_lockfile_catalog_specifier() {
    // pnpm-resolved catalog aliases keep `specifier: 'catalog:'`
    // in the importer block while the `version:` field already
    // carries the resolved alias (`<real>@<resolved>`). The
    // reader must detect the alias from the version shape alone
    // — gating on `specifier.starts_with("npm:")` would silently
    // drop the dep and leave node_modules empty.
    // Repro:
    //   https://github.com/endevco/aube/discussions/383#discussioncomment-16759640
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

catalogs:
  default:
    beamcoder:
      specifier: npm:beamcoder-prebuild@0.7.1-rc.18
      version: 0.7.1-rc.18

importers:
  packages/app:
    dependencies:
      beamcoder:
        specifier: 'catalog:'
        version: beamcoder-prebuild@0.7.1-rc.18

packages:
  beamcoder-prebuild@0.7.1-rc.18:
    resolution: {integrity: sha512-fake}

snapshots:
  beamcoder-prebuild@0.7.1-rc.18: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let app = graph
        .importers
        .get("packages/app")
        .expect("packages/app importer");
    assert_eq!(app.len(), 1, "alias-resolved catalog dep must be parsed");
    let dep = &app[0];
    assert_eq!(dep.name, "beamcoder", "DirectDep keeps the alias name");
    assert_eq!(
        dep.dep_path, "beamcoder@0.7.1-rc.18",
        "DirectDep dep_path is alias-keyed"
    );
    assert_eq!(dep.specifier.as_deref(), Some("catalog:"));

    let pkg = graph
        .packages
        .get("beamcoder@0.7.1-rc.18")
        .expect("synthesized alias-keyed package");
    assert_eq!(pkg.name, "beamcoder");
    assert_eq!(pkg.alias_of.as_deref(), Some("beamcoder-prebuild"));
}

#[test]
fn parse_synthesizes_npm_alias_when_real_name_is_scoped() {
    // Scoped real package + non-scoped alias: `parse_dep_path` must
    // correctly split `@scope/pkg` from the version when the
    // version field is `@scope/pkg@1.0.0`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      types-alias:
        specifier: npm:@types/node@^20.0.0
        version: '@types/node@20.11.0'

packages:
  '@types/node@20.11.0':
    resolution: {integrity: sha512-fake}

snapshots:
  '@types/node@20.11.0': {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();

    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root[0].name, "types-alias");
    assert_eq!(root[0].dep_path, "types-alias@20.11.0");

    let pkg = graph
        .packages
        .get("types-alias@20.11.0")
        .expect("synthesized alias-keyed package");
    assert_eq!(pkg.name, "types-alias");
    assert_eq!(pkg.alias_of.as_deref(), Some("@types/node"));
    let real = graph
        .packages
        .get("@types/node@20.11.0")
        .expect("real entry");
    assert_eq!(real.name, "@types/node");
    assert!(real.alias_of.is_none());
}

#[test]
fn parse_synthesizes_npm_alias_for_transitive_deps() {
    // pnpm encodes npm-aliased *transitive* deps as
    // `<alias>: <real>@<resolved>` inside a snapshot's
    // dependencies map (e.g. `@isaacs/cliui@8.0.2` declares
    // `"string-width-cjs": "npm:string-width@^4.2.0"` and
    // pnpm resolves it as `string-width-cjs: string-width@4.2.3`).
    // The reader must rewrite the dep value to the resolved
    // version and synthesize the alias-keyed package entry, or
    // the linker creates a broken symlink to a non-existent
    // `string-width-cjs@string-width@4.2.3` virtual store dir
    // and the resolver's lockfile-reuse path enqueues a
    // transitive task with a malformed `string-width@4.2.3`
    // range that no string-width-cjs version can satisfy.
    // Repro: https://github.com/stevelandeydescript/aube-bug-repros/tree/main/npm-alias-resolution-failure
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      jackspeak:
        specifier: 4.1.1
        version: 4.1.1

packages:
  '@isaacs/cliui@8.0.2':
    resolution: {integrity: sha512-fake}
  jackspeak@4.1.1:
    resolution: {integrity: sha512-fake}
  string-width@4.2.3:
    resolution: {integrity: sha512-fake}
  string-width@5.1.2:
    resolution: {integrity: sha512-fake}

snapshots:
  '@isaacs/cliui@8.0.2':
    dependencies:
      string-width: 5.1.2
      string-width-cjs: string-width@4.2.3
  jackspeak@4.1.1:
    dependencies:
      '@isaacs/cliui': 8.0.2
  string-width@4.2.3: {}
  string-width@5.1.2: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();

    let cliui = graph
        .packages
        .get("@isaacs/cliui@8.0.2")
        .expect("cliui entry");
    assert_eq!(
        cliui.dependencies.get("string-width-cjs").unwrap(),
        "4.2.3",
        "transitive alias dep value rewritten from `string-width@4.2.3` to bare `4.2.3`"
    );
    assert_eq!(cliui.dependencies.get("string-width").unwrap(), "5.1.2");

    let alias = graph
        .packages
        .get("string-width-cjs@4.2.3")
        .expect("synthesized alias-keyed package for transitive");
    assert_eq!(alias.name, "string-width-cjs");
    assert_eq!(alias.alias_of.as_deref(), Some("string-width"));
    assert_eq!(alias.dep_path, "string-width-cjs@4.2.3");

    let real = graph
        .packages
        .get("string-width@4.2.3")
        .expect("real entry stays put");
    assert_eq!(real.name, "string-width");
    assert!(real.alias_of.is_none());
}

#[test]
fn parse_handles_npm_alias_for_transitive_deps_with_peer_suffix() {
    // Aliased transitive whose alias target carries a peer
    // suffix: `<alias>: <real>@<resolved>(peer@ver)`. The
    // peer-context tail must follow through to the synthetic
    // alias dep_path so the linker keys the same context.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      parent-pkg:
        specifier: 1.0.0
        version: 1.0.0

packages:
  parent-pkg@1.0.0:
    resolution: {integrity: sha512-fake}
  real-pkg@2.0.0:
    resolution: {integrity: sha512-fake}
  peer-pkg@3.0.0:
    resolution: {integrity: sha512-fake}

snapshots:
  parent-pkg@1.0.0:
    dependencies:
      alias-pkg: real-pkg@2.0.0(peer-pkg@3.0.0)
  real-pkg@2.0.0(peer-pkg@3.0.0):
    dependencies:
      peer-pkg: 3.0.0
  peer-pkg@3.0.0: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let parent = graph.packages.get("parent-pkg@1.0.0").expect("parent");
    assert_eq!(
        parent.dependencies.get("alias-pkg").unwrap(),
        "2.0.0(peer-pkg@3.0.0)",
        "peer-context suffix preserved on the rewritten alias dep value"
    );
    let alias = graph
        .packages
        .get("alias-pkg@2.0.0(peer-pkg@3.0.0)")
        .expect("synthesized alias entry with peer suffix");
    assert_eq!(alias.name, "alias-pkg");
    assert_eq!(alias.alias_of.as_deref(), Some("real-pkg"));
}

#[test]
fn parse_synthesizes_npm_alias_for_transitive_deps_of_local_packages() {
    // The local-packages absorption loop runs before the main
    // snapshot loop and pulls a `file:` workspace package's
    // transitive deps directly out of `raw.snapshots`. Those
    // values must go through the same alias rewrite as the main
    // path, or a workspace package depending on
    // `"string-width-cjs": "npm:string-width@^4.2.0"` would still
    // produce the broken `string-width-cjs@string-width@4.2.3`
    // virtual store path on install.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      local-pkg:
        specifier: file:./local-pkg
        version: file:./local-pkg

packages:
  local-pkg@file:./local-pkg:
    resolution: {directory: ./local-pkg, type: directory}
  string-width@4.2.3:
    resolution: {integrity: sha512-fake}

snapshots:
  local-pkg@file:./local-pkg:
    dependencies:
      string-width-cjs: string-width@4.2.3
  string-width@4.2.3: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let local = graph
        .packages
        .values()
        .find(|p| p.name == "local-pkg")
        .expect("local-pkg entry");
    assert_eq!(
        local.dependencies.get("string-width-cjs").unwrap(),
        "4.2.3",
        "transitive alias on a local package gets rewritten too"
    );
    let alias = graph
        .packages
        .get("string-width-cjs@4.2.3")
        .expect("synthesized alias entry from local package's transitive");
    assert_eq!(alias.name, "string-width-cjs");
    assert_eq!(alias.alias_of.as_deref(), Some("string-width"));
}

#[test]
fn git_resolution_integrity_roundtrips() {
    let sha = "abcdef0123456789abcdef0123456789abcdef01";
    let integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
    let local = LocalSource::Git(GitSource {
        url: "https://github.com/owner/repo.git".to_string(),
        committish: Some(sha.to_string()),
        resolved: sha.to_string(),
        integrity: Some(integrity.to_string()),
        subpath: None,
    });
    let dep_path = local.dep_path("gitdep");
    let mut packages = BTreeMap::new();
    packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "gitdep".to_string(),
            version: "1.0.0".to_string(),
            integrity: Some(integrity.to_string()),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "gitdep".to_string(),
            dep_path: dep_path.clone(),
            dep_type: DepType::Production,
            specifier: Some("github:owner/repo".to_string()),
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..LockfileGraph::default()
    };
    let manifest = PackageJson {
        name: Some("root".to_string()),
        version: Some("0.0.0".to_string()),
        dependencies: [("gitdep".to_string(), "github:owner/repo".to_string())]
            .into_iter()
            .collect(),
        ..PackageJson::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(yaml.contains("type: git"));
    assert!(yaml.contains(&format!("integrity: {integrity}")));

    let reparsed = parse(&lockfile_path).unwrap();
    let pkg = reparsed.packages.get(&dep_path).unwrap();
    assert_eq!(pkg.integrity.as_deref(), Some(integrity));
    let Some(LocalSource::Git(git)) = pkg.local_source.as_ref() else {
        panic!("expected git local source");
    };
    assert_eq!(git.integrity.as_deref(), Some(integrity));
}

#[test]
fn writer_emits_git_hosted_for_hosted_git_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let dep_path =
        "demo@git+ssh://git@github.com/acme/demo.git#abcdef0123456789abcdef0123456789abcdef01";
    let graph = LockfileGraph {
        packages: BTreeMap::from([(
            dep_path.to_string(),
            LockedPackage {
                name: "demo".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-hosted".to_string()),
                dep_path: dep_path.to_string(),
                local_source: Some(LocalSource::Git(GitSource {
                    url: "git+ssh://git@github.com/acme/demo.git".to_string(),
                    committish: Some("main".to_string()),
                    resolved: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
                    integrity: None,
                    subpath: None,
                })),
                ..Default::default()
            },
        )]),
        importers: BTreeMap::from([(
            ".".to_string(),
            vec![DirectDep {
                name: "demo".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Production,
                specifier: Some("github:acme/demo".to_string()),
            }],
        )]),
        ..Default::default()
    };
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("demo".to_string(), "github:acme/demo".to_string());

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(yaml.contains("gitHosted: true"), "{yaml}");
    assert!(yaml.contains("integrity: sha512-hosted"), "{yaml}");
}

#[test]
fn writer_preserves_non_derivable_registry_tarball_url_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let lockfile_path = dir.path().join("pnpm-lock.yaml");
    let graph = LockfileGraph {
        packages: BTreeMap::from([(
            "@scope/pkg@1.0.0".to_string(),
            LockedPackage {
                name: "@scope/pkg".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-private".to_string()),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                tarball_url: Some(
                    "https://npm.pkg.github.com/download/@scope/pkg/1.0.0/deadbeef".to_string(),
                ),
                ..Default::default()
            },
        )]),
        importers: BTreeMap::from([(
            ".".to_string(),
            vec![DirectDep {
                name: "@scope/pkg".to_string(),
                dep_path: "@scope/pkg@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        )]),
        ..Default::default()
    };
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("@scope/pkg".to_string(), "1.0.0".to_string());

    write(&lockfile_path, &graph, &manifest).unwrap();
    let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
    assert!(
        yaml.contains("tarball: https://npm.pkg.github.com/download/@scope/pkg/1.0.0/deadbeef"),
        "{yaml}"
    );
    assert!(!yaml.contains("lockfileIncludeTarballUrl: true"), "{yaml}");
}

#[test]
fn parser_round_trips_git_hosted_remote_tarball_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pnpm-lock.yaml");
    std::fs::write(
        &path,
        r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      forge-dep:
        specifier: https://forge.example.test/acme/dep/archive/abcdef.tgz
        version: https://forge.example.test/acme/dep/archive/abcdef.tgz

packages:
  forge-dep@https://forge.example.test/acme/dep/archive/abcdef.tgz:
    resolution: {integrity: sha512-forge, tarball: https://forge.example.test/acme/dep/archive/abcdef.tgz, gitHosted: true}
    version: 1.0.0

snapshots:
  forge-dep@https://forge.example.test/acme/dep/archive/abcdef.tgz: {}
"#,
    )
    .unwrap();

    let graph = parse(&path).unwrap();
    let pkg = graph
        .packages
        .values()
        .find(|pkg| pkg.name == "forge-dep")
        .expect("forge-dep package");
    let Some(LocalSource::RemoteTarball(source)) = &pkg.local_source else {
        panic!("expected remote tarball source, got {:?}", pkg.local_source);
    };
    assert!(source.git_hosted);

    write(&path, &graph, &PackageJson::default()).unwrap();
    let yaml = std::fs::read_to_string(&path).unwrap();
    assert!(yaml.contains("gitHosted: true"), "{yaml}");
}
