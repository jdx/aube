# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.18.1](https://github.com/endevco/aube/compare/aube-codes-v1.18.0...aube-codes-v1.18.1) - 2026-06-07

### Fixed

- *(install)* regenerate conflicted lockfiles ([#843](https://github.com/endevco/aube/pull/843))

### Other

- refresh benchmarks for v1.18.0 ([#841](https://github.com/endevco/aube/pull/841))

### Security

- *(install)* verify lockfile tarball URLs ([#842](https://github.com/endevco/aube/pull/842))

## [1.18.0](https://github.com/endevco/aube/compare/aube-codes-v1.17.1...aube-codes-v1.18.0) - 2026-06-04

### Added

- add sponsors command ([#824](https://github.com/endevco/aube/pull/824))

### Other

- refresh benchmarks for v1.17.1 ([#820](https://github.com/endevco/aube/pull/820))

## [1.17.1](https://github.com/endevco/aube/compare/aube-codes-v1.17.0...aube-codes-v1.17.1) - 2026-05-31

### Other

- *(ci)* switch back to namespace runners ([#819](https://github.com/endevco/aube/pull/819))

## [1.17.0](https://github.com/endevco/aube/compare/aube-codes-v1.16.1...aube-codes-v1.17.0) - 2026-05-31

### Other

- *(ci)* switch to github-hosted runners ([#814](https://github.com/endevco/aube/pull/814))
- refresh benchmarks for v1.16.1 ([#808](https://github.com/endevco/aube/pull/808))

## [1.16.1](https://github.com/endevco/aube/compare/aube-codes-v1.16.0...aube-codes-v1.16.1) - 2026-05-29

### Other

- refresh benchmarks for v1.16.0 ([#787](https://github.com/endevco/aube/pull/787))

### Security

- *(registry)* scope unqualified credentials ([#801](https://github.com/endevco/aube/pull/801))
- *(linker)* reject unsafe package aliases ([#800](https://github.com/endevco/aube/pull/800))

## [1.16.0](https://github.com/endevco/aube/compare/aube-codes-v1.15.0...aube-codes-v1.16.0) - 2026-05-25

### Other

- refresh benchmarks for v1.15.0 ([#750](https://github.com/endevco/aube/pull/750))

## [1.15.0](https://github.com/endevco/aube/compare/aube-codes-v1.14.1...aube-codes-v1.15.0) - 2026-05-17

### Added

- *(add)* add deny-build flag ([#730](https://github.com/endevco/aube/pull/730))

### Other

- refresh benchmarks for v1.14.1 ([#721](https://github.com/endevco/aube/pull/721))

## [1.14.0](https://github.com/endevco/aube/compare/aube-codes-v1.13.1...aube-codes-v1.14.0) - 2026-05-14

### Added

- *(install)* add OSV bloom-filter prefilter for lockfile installs ([#680](https://github.com/endevco/aube/pull/680))
- *(install)* content-sniff dep lifecycle scripts before approve-builds ([#685](https://github.com/endevco/aube/pull/685))

### Other

- refresh benchmarks for v1.13.1 ([#687](https://github.com/endevco/aube/pull/687))

## [1.13.0](https://github.com/endevco/aube/compare/aube-codes-v1.12.0...aube-codes-v1.13.0) - 2026-05-13

### Added

- *(install)* route OSV checks live-API vs local mirror by fresh-resolution ([#678](https://github.com/endevco/aube/pull/678))
- *(install)* bun-compatible security scanner ([#657](https://github.com/endevco/aube/pull/657))
- *(add)* block malicious packages via OSV + prompt on low downloads ([#656](https://github.com/endevco/aube/pull/656))

### Fixed

- *(scripts)* reap orphaned grandchildren on Windows when a lifecycle script aborts ([#661](https://github.com/endevco/aube/pull/661))

### Other

- refresh benchmarks for v1.12.0 ([#625](https://github.com/endevco/aube/pull/625))

## [1.12.0](https://github.com/endevco/aube/compare/aube-codes-v1.11.0...aube-codes-v1.12.0) - 2026-05-12

### Added

- *(config)* scope .npmrc to npm-shared keys, route aube settings to config.toml, support dotted map writes ([#634](https://github.com/endevco/aube/pull/634))

### Other

- refresh benchmarks for v1.11.0 ([#622](https://github.com/endevco/aube/pull/622))

## [1.11.0](https://github.com/endevco/aube/compare/aube-codes-v1.10.4...aube-codes-v1.11.0) - 2026-05-11

### Fixed

- *(registry)* coalesce slow-metadata warnings into one resolve summary ([#592](https://github.com/endevco/aube/pull/592))

### Other

- refresh benchmarks for v1.10.4 ([#600](https://github.com/endevco/aube/pull/600))

## [1.10.3](https://github.com/endevco/aube/compare/aube-codes-v1.10.2...aube-codes-v1.10.3) - 2026-05-10

### Other

- update Cargo.lock dependencies

## [1.10.1](https://github.com/endevco/aube/compare/aube-codes-v1.10.0...aube-codes-v1.10.1) - 2026-05-10

### Other

- refresh benchmarks for v1.10.0 ([#571](https://github.com/endevco/aube/pull/571))
- refresh benchmarks for v1.10.0 ([#566](https://github.com/endevco/aube/pull/566))

## [1.10.0](https://github.com/endevco/aube/compare/aube-codes-v1.9.1...aube-codes-v1.10.0) - 2026-05-10

### Added

- *(cli)* finish recursive-run flags and parallel output ([#545](https://github.com/endevco/aube/pull/545))

### Other

- refresh benchmarks for v1.9.1 ([#555](https://github.com/endevco/aube/pull/555))
- lead hero with auto-install promise over speed ([#557](https://github.com/endevco/aube/pull/557))
- refresh benchmarks for v1.9.1 ([#534](https://github.com/endevco/aube/pull/534))
- refresh benchmarks for v1.9.0 ([#532](https://github.com/endevco/aube/pull/532))

## [1.9.1](https://github.com/endevco/aube/compare/aube-codes-v1.9.0...aube-codes-v1.9.1) - 2026-05-06

### Fixed

- *(cli)* skip registry for workspace deps ([#523](https://github.com/endevco/aube/pull/523))

### Other

- refresh benchmarks for v1.9.0 ([#525](https://github.com/endevco/aube/pull/525))

## [1.9.0](https://github.com/endevco/aube/compare/aube-codes-v1.8.0...aube-codes-v1.9.0) - 2026-05-05

### Other

- refresh benchmarks for v1.8.0 ([#508](https://github.com/endevco/aube/pull/508))

## [1.8.0](https://github.com/endevco/aube/compare/aube-codes-v1.7.0...aube-codes-v1.8.0) - 2026-05-03

### Added

- *(progress)* redesign install progress UI ([#501](https://github.com/endevco/aube/pull/501))
- *(run)* prefer local bins for run and dlx ([#502](https://github.com/endevco/aube/pull/502))
