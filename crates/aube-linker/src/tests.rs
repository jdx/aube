use super::*;
use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use aube_store::Store;

fn setup_store_with_files(dir: &Path) -> (Store, BTreeMap<String, aube_store::PackageIndex>) {
    let store = Store::at(dir.join("store/files"));

    let mut indices = BTreeMap::new();

    // foo@1.0.0 with index.js
    let foo_stored = store
        .import_bytes(b"module.exports = 'foo';", false)
        .unwrap();
    let mut foo_index = PackageIndex::default();
    foo_index.insert("index.js".to_string(), foo_stored);

    // foo also has package.json
    let foo_pkg = store
        .import_bytes(b"{\"name\":\"foo\",\"version\":\"1.0.0\"}", false)
        .unwrap();
    foo_index.insert("package.json".to_string(), foo_pkg);
    indices.insert("foo@1.0.0".to_string(), foo_index);

    // bar@2.0.0 with index.js
    let bar_stored = store
        .import_bytes(b"module.exports = 'bar';", false)
        .unwrap();
    let mut bar_index = PackageIndex::default();
    bar_index.insert("index.js".to_string(), bar_stored);
    indices.insert("bar@2.0.0".to_string(), bar_index);

    (store, indices)
}

fn make_graph() -> LockfileGraph {
    let mut packages = BTreeMap::new();

    let mut foo_deps = BTreeMap::new();
    foo_deps.insert("bar".to_string(), "2.0.0".to_string());

    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: None,
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
            integrity: None,
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
            specifier: None,
        }],
    );

    LockfileGraph {
        importers,
        packages,
        ..Default::default()
    }
}

#[test]
fn test_detect_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let strategy = Linker::detect_strategy(dir.path());
    // Probe returns `Hardlink` or `Copy`; `Reflink` is only
    // reachable through explicit `packageImportMethod =
    // clone`/`clone-or-copy`, so the match guards that contract.
    match strategy {
        LinkStrategy::Hardlink | LinkStrategy::Copy => {}
        LinkStrategy::Reflink => panic!("detect_strategy must not return Reflink"),
    }
}

#[test]
fn test_link_all_handles_self_referential_dep_at_different_version() {
    // `react_ujs@3.3.0` (and other publish-script artifacts)
    // declares its own name as a dep at a *different* version
    // (`react_ujs: ^2.7.1`). The transitive-symlink pass would
    // try to create a symlink at `node_modules/react_ujs`,
    // which is exactly where the package's own files live —
    // EEXIST. Skip self-name deps regardless of version so
    // these install cleanly. `require('<self>')` from inside
    // the package then resolves to its own files, matching how
    // npm / pnpm / yarn end up after their hoisting passes.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let store = Store::at(dir.path().join("store/files"));

    let mut indices = BTreeMap::new();
    let host_index_js = store.import_bytes(b"/* react_ujs 3.3.0 */", false).unwrap();
    let host_pkg_json = store
        .import_bytes(b"{\"name\":\"react_ujs\",\"version\":\"3.3.0\"}", false)
        .unwrap();
    let mut host_index = PackageIndex::default();
    host_index.insert("index.js".to_string(), host_index_js);
    host_index.insert("package.json".to_string(), host_pkg_json);
    indices.insert("react_ujs@3.3.0".to_string(), host_index);

    let mut host_deps = BTreeMap::new();
    // Self-reference at a different version, the shape that
    // triggered the EEXIST bug.
    host_deps.insert("react_ujs".to_string(), "^2.7.1".to_string());

    let mut packages = BTreeMap::new();
    packages.insert(
        "react_ujs@3.3.0".to_string(),
        LockedPackage {
            name: "react_ujs".to_string(),
            version: "3.3.0".to_string(),
            integrity: None,
            dependencies: host_deps,
            dep_path: "react_ujs@3.3.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "react_ujs".to_string(),
            dep_path: "react_ujs@3.3.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let stats = linker
        .link_all(&project_dir, &graph, &indices)
        .expect("install must succeed despite self-named dep");
    assert_eq!(stats.packages_linked, 1);
    let host_index =
        project_dir.join("node_modules/.aube/react_ujs@3.3.0/node_modules/react_ujs/index.js");
    assert!(host_index.exists(), "host package files must be present");
}

#[test]
fn test_link_all_creates_pnpm_virtual_store() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();

    let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

    // .aube virtual store should exist
    assert!(project_dir.join("node_modules/.aube").exists());

    // .aube/foo@1.0.0 should be a symlink to the global virtual store
    let aube_foo = project_dir.join("node_modules/.aube/foo@1.0.0");
    assert!(aube_foo.symlink_metadata().unwrap().is_symlink());

    // foo@1.0.0 content should be accessible through the symlink
    let foo_in_pnpm = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/foo/index.js");
    assert!(foo_in_pnpm.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_in_pnpm).unwrap(),
        "module.exports = 'foo';"
    );

    // bar@2.0.0 should also be accessible
    let bar_in_pnpm = project_dir.join("node_modules/.aube/bar@2.0.0/node_modules/bar/index.js");
    assert!(bar_in_pnpm.exists());

    assert_eq!(stats.packages_linked, 2);
    assert!(stats.files_linked >= 3); // foo has 2 files, bar has 1
}

#[test]
fn test_link_file_fresh_reports_missing_cas_shard_and_invalidates_cache() {
    // Reproduces jdx/aube#393: a partially corrupt CAS leaves the
    // cached package index pointing at a missing shard. Materialize
    // must distinguish "source CAS file missing" from a generic ENOENT
    // and drop the stale index JSON so the next install re-imports
    // the tarball.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    // Persist foo's index so invalidate_cached_index has something
    // to remove. Real installs save indices via the fetch path.
    let foo_index = indices.get("foo@1.0.0").unwrap();
    store.save_index("foo", "1.0.0", None, foo_index).unwrap();
    let cached_path = store.index_dir().join("foo@1.0.0.json");
    assert!(
        cached_path.exists(),
        "test setup: index cache must be written"
    );

    // Delete the CAS shard for foo's package.json (matches the
    // failure mode in #393 where one shard is missing while others
    // remain).
    let pkgjson_store_path = foo_index.get("package.json").unwrap().store_path.clone();
    std::fs::remove_file(&pkgjson_store_path).unwrap();

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();
    let err = linker
        .link_all(&project_dir, &graph, &indices)
        .expect_err("link must fail when a referenced CAS shard is gone");
    assert!(
        matches!(&err, Error::MissingStoreFile { rel_path, .. } if rel_path == "package.json"),
        "expected MissingStoreFile {{ rel_path: \"package.json\" }}, got {err:?}"
    );

    // Side effect: cached index dropped, so the next install will
    // miss load_index and re-fetch instead of looping on the same
    // dead shard reference.
    assert!(
        !cached_path.exists(),
        "stale index cache must be invalidated on MissingStoreFile"
    );
}

#[test]
#[cfg(unix)]
fn test_link_file_fresh_hardlink_short_circuits_when_source_missing() {
    // Hardlink path used to silently fall through to `std::fs::copy`
    // on ENOENT and emit a misleading "hardlink failed, falling back
    // to copy" trace, even though the real cause was the source
    // shard going missing. Short-circuit returns MissingStoreFile
    // directly so traces stay accurate.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));
    let stored = store.import_bytes(b"hello", false).unwrap();
    // Capture the path before we move `stored` into link_file_fresh.
    let store_path = stored.store_path.clone();
    std::fs::remove_file(&store_path).unwrap();

    let dst_dir = dir.path().join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join("hello.txt");

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Hardlink, true);
    let err = linker
        .link_file_fresh(&stored, "hello.txt", &dst)
        .expect_err("source missing must fail");
    assert!(
        matches!(
            &err,
            Error::MissingStoreFile { store_path: p, rel_path } if p == &store_path && rel_path == "hello.txt"
        ),
        "expected MissingStoreFile from Hardlink branch, got {err:?}"
    );
}

#[test]
fn test_link_all_creates_top_level_entries() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Top-level foo/ should exist (it's a direct dep)
    let foo_top = project_dir.join("node_modules/foo/index.js");
    assert!(foo_top.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_top).unwrap(),
        "module.exports = 'foo';"
    );

    // bar should NOT be top-level (it's only a transitive dep)
    let bar_top = project_dir.join("node_modules/bar/index.js");
    assert!(!bar_top.exists());

    assert_eq!(stats.top_level_linked, 1);
}

#[test]
fn test_link_all_transitive_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // foo's node_modules/bar should be a symlink (inside the global virtual store)
    // The path resolves through the .aube symlink into the global store
    let bar_symlink = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
    assert!(bar_symlink.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_link_all_cleans_existing_node_modules() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    let nm = project_dir.join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("stale-file.txt"), "old").unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Old file should be gone
    assert!(!nm.join("stale-file.txt").exists());
    // New structure should exist
    assert!(nm.join(".aube").exists());
}

#[test]
fn test_link_all_nested_node_modules_for_direct_deps() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // foo is a direct dep with bar as a transitive dep.
    // The top-level node_modules/foo is a symlink to .aube/foo@1.0.0/node_modules/foo,
    // and bar lives as a sibling at .aube/foo@1.0.0/node_modules/bar (also a symlink
    // pointing to .aube/bar@2.0.0/node_modules/bar). Node's directory walk from inside
    // foo finds bar this way without aube creating any nested node_modules.
    let foo_link = project_dir.join("node_modules/foo");
    assert!(foo_link.symlink_metadata().unwrap().is_symlink());
    let bar_sibling = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
    assert!(bar_sibling.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_global_virtual_store_is_populated() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Global virtual store should contain materialized packages
    let foo_global = virtual_store.join("foo@1.0.0/node_modules/foo/index.js");
    assert!(foo_global.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_global).unwrap(),
        "module.exports = 'foo';"
    );

    let bar_global = virtual_store.join("bar@2.0.0/node_modules/bar/index.js");
    assert!(bar_global.exists());
}

#[test]
fn test_global_virtual_store_gets_hidden_hoist() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let mut graph = make_graph();
    graph
        .packages
        .get_mut("foo@1.0.0")
        .unwrap()
        .dependencies
        .clear();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    let project_hidden = project_dir.join("node_modules/.aube/node_modules/bar");
    assert!(project_hidden.symlink_metadata().unwrap().is_symlink());

    let global_hidden = virtual_store.join("node_modules/bar");
    assert!(global_hidden.symlink_metadata().unwrap().is_symlink());

    let from_real_store = virtual_store.join("foo@1.0.0/node_modules/bar/index.js");
    assert!(
        !from_real_store.exists(),
        "bar is not a declared sibling of foo in this fixture"
    );
    let fallback = virtual_store.join("node_modules/bar/index.js");
    assert_eq!(
        std::fs::read_to_string(fallback).unwrap(),
        "module.exports = 'bar';"
    );
}

#[test]
fn test_global_virtual_store_hidden_hoist_prunes_only_dead_entries() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let hidden = virtual_store.join("node_modules");
    std::fs::create_dir_all(&hidden).unwrap();
    let dotfile = hidden.join(".sentinel");
    std::fs::write(&dotfile, "shared").unwrap();
    let stale = hidden.join("stale");
    std::fs::write(&stale, "old").unwrap();
    let stale_scope = hidden.join("@stale-scope");
    std::fs::write(&stale_scope, "old").unwrap();
    let external_target = virtual_store.join("external@1.0.0/node_modules/external");
    std::fs::create_dir_all(&external_target).unwrap();
    let external_link = hidden.join("external");
    sys::create_dir_link(
        &pathdiff::diff_paths(&external_target, &hidden).unwrap(),
        &external_link,
    )
    .unwrap();
    let dead_link = hidden.join("dead");
    sys::create_dir_link(
        Path::new("../missing@1.0.0/node_modules/missing"),
        &dead_link,
    )
    .unwrap();

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    linker
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    assert_eq!(std::fs::read_to_string(dotfile).unwrap(), "shared");
    assert!(!stale.exists());
    assert!(stale_scope.symlink_metadata().is_err());
    assert!(external_link.symlink_metadata().unwrap().is_symlink());
    assert!(dead_link.symlink_metadata().is_err());
    assert!(hidden.join("bar").symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_global_virtual_store_hidden_hoist_disabled_keeps_live_shared_links() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    linker
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    let global_hidden = virtual_store.join("node_modules/bar");
    assert!(global_hidden.symlink_metadata().unwrap().is_symlink());

    Linker::new_with_gvs(&store, LinkStrategy::Copy, true)
        .with_hoist(false)
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    assert!(global_hidden.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_second_install_reuses_global_store() {
    let dir = tempfile::tempdir().unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();

    // First install
    let project1 = dir.path().join("project1");
    std::fs::create_dir_all(&project1).unwrap();
    let stats1 = linker.link_all(&project1, &graph, &indices).unwrap();
    assert_eq!(stats1.packages_linked, 2);
    assert_eq!(stats1.packages_cached, 0);

    // Second install with same deps — should reuse global virtual store
    let project2 = dir.path().join("project2");
    std::fs::create_dir_all(&project2).unwrap();
    let stats2 = linker.link_all(&project2, &graph, &indices).unwrap();
    assert_eq!(stats2.packages_linked, 0);
    assert_eq!(stats2.packages_cached, 2);
    assert_eq!(stats2.files_linked, 0); // no CAS linking needed

    // Both projects should work
    let foo1 = project1.join("node_modules/foo/index.js");
    let foo2 = project2.join("node_modules/foo/index.js");
    assert!(foo1.exists());
    assert!(foo2.exists());
    assert_eq!(
        std::fs::read_to_string(&foo1).unwrap(),
        std::fs::read_to_string(&foo2).unwrap()
    );
}

/// Regression: a version bump keeps the same top-level name
/// (`foo`) but must repoint `node_modules/foo` at the new
/// `.aube/foo@<new>` entry. The old `.aube/foo@<old>/` is left
/// on disk (no one sweeps the virtual store by name), so a
/// plain `path.exists()` check would see a still-resolving
/// stale symlink and keep it. The target-aware
/// `reconcile_top_level_link` compares the expected target
/// string and rewrites the link.
#[test]
fn test_link_all_repoints_symlink_after_version_bump() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let store = Store::at(dir.path().join("store/files"));

    // Install 1: foo@1.0.0 as the root's direct dep.
    let mut indices_v1 = BTreeMap::new();
    let foo_v1 = store
        .import_bytes(b"module.exports = 'foo@1';", false)
        .unwrap();
    let mut foo_v1_index = PackageIndex::default();
    foo_v1_index.insert("index.js".to_string(), foo_v1);
    indices_v1.insert("foo@1.0.0".to_string(), foo_v1_index);

    let mut graph_v1 = LockfileGraph::default();
    graph_v1.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v1.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let linker = Linker::new(&store, LinkStrategy::Copy);
    linker
        .link_all(&project_dir, &graph_v1, &indices_v1)
        .unwrap();
    let foo_link = project_dir.join("node_modules/foo");
    assert!(foo_link.symlink_metadata().unwrap().is_symlink());
    assert_eq!(
        std::fs::read_to_string(foo_link.join("index.js")).unwrap(),
        "module.exports = 'foo@1';"
    );

    // Install 2: foo upgraded to 2.0.0. The `.aube/foo@1.0.0/`
    // tree stays on disk (nothing prunes the virtual store by
    // name), so the old `node_modules/foo` symlink still
    // resolves — a naive "does the target exist?" check would
    // keep it.
    let mut indices_v2 = BTreeMap::new();
    let foo_v2 = store
        .import_bytes(b"module.exports = 'foo@2';", false)
        .unwrap();
    let mut foo_v2_index = PackageIndex::default();
    foo_v2_index.insert("index.js".to_string(), foo_v2);
    indices_v2.insert("foo@2.0.0".to_string(), foo_v2_index);

    let mut graph_v2 = LockfileGraph::default();
    graph_v2.packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v2.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    linker
        .link_all(&project_dir, &graph_v2, &indices_v2)
        .unwrap();

    // The top-level symlink must now resolve to foo@2.0.0's
    // bytes, not foo@1.0.0's.
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@2';"
    );
}

/// Regression: `shamefully_hoist` hoists transitive deps to the
/// top-level `node_modules/<name>`. When the hoisted version
/// changes between installs (transitive bump), the previous
/// implementation kept the stale symlink because
/// `keep_or_reclaim_broken_symlink` only checked "does target
/// resolve?" and the old `.aube/<old-dep-path>/` was still on
/// disk. `reconcile_top_level_link` + the explicit
/// direct-dep/claimed tracking in `hoist_remaining_into` together
/// fix this.
#[test]
fn test_shamefully_hoist_repoints_after_transitive_version_bump() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let store = Store::at(dir.path().join("store/files"));

    // Install 1: root → bar@1.0.0 → foo@1.0.0 (transitive).
    let foo_v1 = store
        .import_bytes(b"module.exports = 'foo@1';", false)
        .unwrap();
    let mut foo_v1_idx = PackageIndex::default();
    foo_v1_idx.insert("index.js".to_string(), foo_v1);
    let bar_v1 = store
        .import_bytes(b"module.exports = 'bar@1';", false)
        .unwrap();
    let mut bar_v1_idx = PackageIndex::default();
    bar_v1_idx.insert("index.js".to_string(), bar_v1);
    let mut indices_v1 = BTreeMap::new();
    indices_v1.insert("foo@1.0.0".to_string(), foo_v1_idx);
    indices_v1.insert("bar@1.0.0".to_string(), bar_v1_idx);

    let mut graph_v1 = LockfileGraph::default();
    let mut bar_deps_v1 = BTreeMap::new();
    bar_deps_v1.insert("foo".to_string(), "1.0.0".to_string());
    graph_v1.packages.insert(
        "bar@1.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dependencies: bar_deps_v1,
            ..Default::default()
        },
    );
    graph_v1.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v1.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "bar".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let linker = Linker::new(&store, LinkStrategy::Copy).with_shamefully_hoist(true);
    linker
        .link_all(&project_dir, &graph_v1, &indices_v1)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@1';",
        "install 1 should hoist foo@1.0.0"
    );

    // Install 2: bar@1.0.0 → foo@2.0.0 (transitive bump). The
    // stale `.aube/foo@1.0.0/` tree is still on disk (nothing
    // sweeps the virtual store by name), so the old hoisted
    // symlink would still resolve — the old `exists?` check
    // would silently keep it.
    let foo_v2 = store
        .import_bytes(b"module.exports = 'foo@2';", false)
        .unwrap();
    let mut foo_v2_idx = PackageIndex::default();
    foo_v2_idx.insert("index.js".to_string(), foo_v2);
    let mut indices_v2 = BTreeMap::new();
    // Reuse bar's materialized index from v1.
    let bar_v1_for_v2 = store
        .import_bytes(b"module.exports = 'bar@1';", false)
        .unwrap();
    let mut bar_v1_idx_v2 = PackageIndex::default();
    bar_v1_idx_v2.insert("index.js".to_string(), bar_v1_for_v2);
    indices_v2.insert("bar@1.0.0".to_string(), bar_v1_idx_v2);
    indices_v2.insert("foo@2.0.0".to_string(), foo_v2_idx);

    let mut graph_v2 = LockfileGraph::default();
    let mut bar_deps_v2 = BTreeMap::new();
    bar_deps_v2.insert("foo".to_string(), "2.0.0".to_string());
    graph_v2.packages.insert(
        "bar@1.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dependencies: bar_deps_v2,
            ..Default::default()
        },
    );
    graph_v2.packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v2.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "bar".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    linker
        .link_all(&project_dir, &graph_v2, &indices_v2)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@2';",
        "install 2 should repoint the hoisted symlink to foo@2.0.0"
    );
}

// ---------------------------------------------------------------
// `validate_index_key` rejects every shape of index key that
// would make `base.join(key)` escape `base`. Primary defence is
// in `aube-store::import_tarball`; this is the last-chance guard
// before the linker actually writes to disk.
// ---------------------------------------------------------------

#[test]
fn validate_index_key_accepts_normal_keys() {
    validate_index_key("index.js").unwrap();
    validate_index_key("lib/sub/a.js").unwrap();
    validate_index_key("package.json").unwrap();
    validate_index_key("a/b/c/d/e/f.js").unwrap();
}

#[cfg(not(windows))]
#[test]
fn validate_index_key_accepts_posix_colon_filename() {
    validate_index_key("dist/__mocks__/package-json:version.d.ts").unwrap();
}

#[test]
fn validate_index_key_rejects_empty() {
    assert!(matches!(
        validate_index_key(""),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_leading_slash() {
    assert!(matches!(
        validate_index_key("/etc/passwd"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("\\evil"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_parent_dir() {
    assert!(matches!(
        validate_index_key("../../etc/passwd"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("lib/../../../etc"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_nul_and_backslash() {
    assert!(matches!(
        validate_index_key("lib\0evil"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("lib\\..\\etc"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[cfg(windows)]
#[test]
fn validate_index_key_rejects_windows_drive() {
    assert!(matches!(
        validate_index_key("C:Windows"),
        Err(Error::UnsafeIndexKey(_))
    ));
}
