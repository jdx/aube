#!/usr/bin/env bats
#
# Regression: under `sharedWorkspaceLockfile=false` the workspace root
# has no shared lockfile — each member owns its own. The install warm
# path anchored freshness on a single root lockfile, so it always read
# "no lockfile found" and fell through to the full resolve/fetch/delta
# pipeline on every `aube install` (and every `aube run` auto-install
# check). On a large monorepo that re-walks the whole graph and visibly
# re-links, even when nothing changed.
#
# The fix fingerprints each member's lockfile in the install state so
# the warm path can short-circuit, while still re-enumerating members so
# an added/removed/edited member correctly busts the warm path.
#
# The discriminator used throughout: the *fast path* prints a bare
# `✓ Already up to date`; the *full pipeline* (even on a no-op) prints
# `✓ Already up to date (N packages)`. So a bare message with no count
# proves the warm fast path engaged.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Workspace with `sharedWorkspaceLockfile=false`: a leaf lib and a
# service that depends on both a registry package and the leaf lib via
# `workspace:*`.
#
# The root is config-only — a `pnpm-workspace.yaml` with no root
# `package.json`. pnpm (and aube) write no root lockfile for a
# non-project root even under sharedWorkspaceLockfile=false, so the warm
# path has only the per-member lockfiles to anchor on. That is exactly
# the case this guards: with no root lockfile to read, the pre-fix check
# always reported "no lockfile found" and re-ran the full pipeline. Using
# a config-only root keeps the regression reproducible regardless of
# whether a root *project* would have been given its own lockfile.
_setup_no_shared_workspace() {
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - packages/*
		sharedWorkspaceLockfile: false
	YAML
	mkdir -p packages/lib-a
	cat >packages/lib-a/package.json <<-'JSON'
		{
		  "name": "@ws/lib-a",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-number": "6.0.0"
		  }
		}
	JSON
	mkdir -p packages/service-name
	cat >packages/service-name/package.json <<-'JSON'
		{
		  "name": "service-name",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1",
		    "@ws/lib-a": "workspace:*"
		  }
		}
	JSON
}

@test "sharedWorkspaceLockfile=false: member install short-circuits on the warm path" {
	_setup_no_shared_workspace

	run aube install
	assert_success
	# Per-member layout: each member owns a lockfile, the root owns none.
	run test -e aube-lock.yaml
	assert_failure
	assert_file_exists packages/service-name/aube-lock.yaml
	assert_dir_exists node_modules/.aube-state

	local root_state_before
	root_state_before="$(cat node_modules/.aube-state/state.json)"

	cd packages/service-name
	run aube install
	assert_success
	cd ../..

	# Warm fast path: bare "Already up to date", no "(N packages)" count.
	assert_output --partial "Already up to date"
	refute_output --partial "up to date ("

	# The member never grows its own virtual store or install state —
	# the install resolved up to the workspace root.
	run test -d packages/service-name/node_modules/.aube
	assert_failure
	run test -e packages/service-name/node_modules/.aube-state
	assert_failure

	# A true no-op writes nothing: the root state is byte-identical.
	assert_equal "$(cat node_modules/.aube-state/state.json)" "$root_state_before"
}

@test "sharedWorkspaceLockfile=false: settings-only nested member yaml resolves to the parent root" {
	_setup_no_shared_workspace

	# The member drops a *settings-only* pnpm-workspace.yaml (no
	# `packages:`) into its own dir. That configures the member; it does
	# not declare a new workspace. Workspace-root discovery must walk
	# past it to the real root — otherwise `cd member && aube install`
	# resolves to the member-as-its-own-root, which can't see its
	# `workspace:*` sibling and re-resolves the whole graph every run.
	cat >packages/service-name/pnpm-workspace.yaml <<-'YAML'
		minimumReleaseAge: 0
	YAML

	run aube install
	assert_success
	# The root install links the workspace sibling into the member.
	assert_link_exists packages/service-name/node_modules/@ws/lib-a

	cd packages/service-name
	run aube install
	assert_success
	cd ../..

	# Warm fast path: bare "Already up to date", no "(N packages)" count.
	assert_output --partial "Already up to date"
	refute_output --partial "up to date ("

	# Resolved to the parent root: the member grew no install state of
	# its own, and its workspace sibling is still linked (a member-rooted
	# resolve would have dropped the sibling and rebuilt the member).
	run test -e packages/service-name/node_modules/.aube-state
	assert_failure
	assert_link_exists packages/service-name/node_modules/@ws/lib-a
}

@test "sharedWorkspaceLockfile=false: deleting a member node_modules relinks on the next root install" {
	_setup_no_shared_workspace

	run aube install
	assert_success
	assert_link_exists packages/service-name/node_modules/is-odd
	assert_link_exists packages/service-name/node_modules/@ws/lib-a

	# Wipe just the member's node_modules. The lockfile and install state
	# still claim it's installed, so the freshness check must notice the
	# member's direct symlinks vanished and relink — not report a bare
	# "Already up to date" while the member stays broken. Pre-fix the
	# state only tracked the *root* importer's entries, so a missing
	# member node_modules was invisible to the warm-path check.
	rm -rf packages/service-name/node_modules

	run aube install
	assert_success
	# A relink links packages, so the summary can't be "Already up to date".
	refute_output --partial "Already up to date"
	assert_link_exists packages/service-name/node_modules/is-odd
	assert_link_exists packages/service-name/node_modules/@ws/lib-a
}

@test "sharedWorkspaceLockfile=false: member install relinks its own deleted node_modules" {
	_setup_no_shared_workspace

	run aube install
	assert_success
	assert_link_exists packages/service-name/node_modules/is-odd
	assert_link_exists packages/service-name/node_modules/@ws/lib-a

	# The reported repro: from inside the member, delete its node_modules
	# and reinstall. The member resolves up to the parent root, which must
	# detect the missing member layout and relink it — including the
	# `workspace:*` sibling, which is the analogue of a virtual-store
	# sibling a dependency loads at runtime. Pre-fix this short-circuited
	# to "Already up to date" and left the member with no node_modules.
	rm -rf packages/service-name/node_modules
	cd packages/service-name
	run aube install
	assert_success
	cd ../..
	refute_output --partial "Already up to date"
	assert_link_exists packages/service-name/node_modules/is-odd
	assert_link_exists packages/service-name/node_modules/@ws/lib-a
}

@test "sharedWorkspaceLockfile=false: repeat root install short-circuits on the warm path" {
	_setup_no_shared_workspace

	run aube install
	assert_success
	local root_state_before
	root_state_before="$(cat node_modules/.aube-state/state.json)"

	run aube install
	assert_success
	assert_output --partial "Already up to date"
	refute_output --partial "up to date ("
	assert_equal "$(cat node_modules/.aube-state/state.json)" "$root_state_before"
}

@test "sharedWorkspaceLockfile=false: editing a member dependency busts the warm path" {
	_setup_no_shared_workspace

	run aube install
	assert_success

	# Add a new direct dep to a member. Before the edit, is-number is
	# only a transitive dep of is-odd (it lives in the virtual store,
	# not the member's own node_modules), so this link appearing proves
	# the install re-resolved instead of short-circuiting.
	cat >packages/service-name/package.json <<-'JSON'
		{
		  "name": "service-name",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1",
		    "is-number": "6.0.0",
		    "@ws/lib-a": "workspace:*"
		  }
		}
	JSON

	run aube install
	assert_success
	assert_link_exists packages/service-name/node_modules/is-number
}

@test "sharedWorkspaceLockfile=false: adding a new member busts the warm path" {
	_setup_no_shared_workspace

	run aube install
	assert_success

	# A brand-new member is not in the recorded state and has no
	# lockfile yet. The warm path must re-enumerate members and notice
	# it, otherwise the new member never gets installed.
	mkdir -p packages/new-svc
	cat >packages/new-svc/package.json <<-'JSON'
		{
		  "name": "new-svc",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1"
		  }
		}
	JSON

	run aube install
	assert_success
	assert_link_exists packages/new-svc/node_modules/is-odd
	assert_file_exists packages/new-svc/aube-lock.yaml
}

@test "sharedWorkspaceLockfile=false: removing a member busts the warm path" {
	_setup_no_shared_workspace
	# A standalone member nothing else depends on, so removing it is clean.
	mkdir -p packages/extra
	cat >packages/extra/package.json <<-'JSON'
		{
		  "name": "extra",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1"
		  }
		}
	JSON

	run aube install
	assert_success
	# The removed member is recorded in the root install state.
	run grep -q "packages/extra" node_modules/.aube-state/state.json
	assert_success

	rm -rf packages/extra

	run aube install
	assert_success
	# The warm path noticed the member vanished and rewrote state
	# without it (a fast-path no-op would have left the stale entry).
	run grep -q "packages/extra" node_modules/.aube-state/state.json
	assert_failure
}

@test "sharedWorkspaceLockfile=true control: repeat install still short-circuits" {
	# Sanity: the default shared layout is unaffected by the per-member
	# warm-path handling — it still anchors on the shared root lockfile.
	cat >package.json <<-'JSON'
		{
		  "name": "ws-root",
		  "version": "0.0.0",
		  "private": true
		}
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - packages/*
	YAML
	mkdir -p packages/a
	cat >packages/a/package.json <<-'JSON'
		{
		  "name": "@ws/a",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1"
		  }
		}
	JSON

	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
	run test -e packages/a/aube-lock.yaml
	assert_failure

	run aube install
	assert_success
	assert_output --partial "Already up to date"
	refute_output --partial "up to date ("
}
