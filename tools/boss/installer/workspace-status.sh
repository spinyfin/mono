#!/usr/bin/env bash
# Workspace-status script for the Boss release build.
#
# Called by Bazel when --workspace_status_command is set (always, not just
# with --stamp). The STABLE_* keys go to stable-status.txt; all others go
# to volatile-status.txt.
#
# BUILD_EMBED_LABEL is a special Bazel key: its value is used by
# apple_bundle_version's build_label_pattern mechanism to stamp
# CFBundleShortVersionString in Boss.app's Info.plist.
set -euo pipefail

SHA=$(jj log --no-graph -r @ -T 'commit_id.short(7)' 2>/dev/null || git rev-parse --short HEAD 2>/dev/null || echo "unknown")

# Full (unabbreviated) commit sha, for `engine::build_provenance` — long
# enough to feed straight to `git merge-base --is-ancestor <sha> main`
# without any abbreviation-collision risk. $SHA (short) above stays as-is;
# it's embedded in the .pkg filename and callers of that are unaffected.
FULL_SHA=$(jj log --no-graph -r @ -T 'commit_id' 2>/dev/null || git rev-parse HEAD 2>/dev/null || echo "unknown")

# Working-tree dirty flag for `engine::build_provenance`.
#   - jj workspace: jj auto-snapshots every file edit into `@` on every jj
#     invocation, so there is no git-style "uncommitted changes" state to
#     detect; the one genuinely-broken build state jj can be in is `@`
#     carrying unresolved conflicts, so that's what "dirty" means here.
#   - plain git checkout: the standard uncommitted-changes (tracked or
#     untracked) signal via `git status --porcelain`.
if [ -d ".jj" ]; then
    if [ "$(jj log --no-graph -r @ -T 'if(conflict, "true", "false")' 2>/dev/null)" = "true" ]; then
        DIRTY=true
    else
        DIRTY=false
    fi
else
    if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
        DIRTY=true
    else
        DIRTY=false
    fi
fi

# Build wall-clock time. Deliberately NOT written to stable-status.txt (see
# the build_info_rs note below) — it goes to volatile-status.txt instead,
# which Bazel already regenerates on every build without that forcing a
# rebuild of unrelated actions; only `build_provenance_rs` (a dedicated,
# intentionally-small genrule/crate — see its own doc comment) reads it.
BUILD_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Compute a semantic version string from git tags (boss-v* prefix).
# Release build (exact tag match): boss-v1.0.4 → "1.0.4"
# Dev build (commits past tag):    boss-v1.0.4-16-gf3be785 → "1.0.4-dev-<SHA>"
# Uses $SHA (from jj/git above) for the dev suffix so STABLE_BOSS_VERSION
# and STABLE_BOSS_GIT_SHA always contain the same commit identifier.
#
# In a jj workspace there is no .git directory — git describe must be pointed
# at the bare git store jj maintains at .jj/repo/store/git.  jj commit IDs
# are git SHAs, so we can pass the full commit_id directly to git describe.
# In a plain git checkout the standard `git describe` path is used instead.
if [ -d ".jj" ]; then
    # jj workspace: resolve tags via the bare git store.
    # Use the full jj commit ID (which is the git SHA) as the describe target.
    FULL_SHA=$(jj log --no-graph -r @ -T 'commit_id' 2>/dev/null || echo "")
    if [ -n "$FULL_SHA" ]; then
        export GIT_DIR=".jj/repo/store/git"
        DESCRIBE=$(git describe --tags --match "boss-v*" --abbrev=0 "$FULL_SHA" 2>/dev/null || echo "")
        DESCRIBE_EXACT=$(git describe --tags --match "boss-v*" --exact-match "$FULL_SHA" 2>/dev/null || echo "")
        unset GIT_DIR
    else
        DESCRIBE=""
        DESCRIBE_EXACT=""
    fi
else
    DESCRIBE=$(git describe --tags --match "boss-v*" --abbrev=0 2>/dev/null || echo "")
    DESCRIBE_EXACT=$(git describe --tags --match "boss-v*" --exact-match 2>/dev/null || echo "")
fi

if [ -z "$DESCRIBE" ]; then
    BOSS_VERSION="0.0.0-dev-${SHA}"
    BOSS_BASE_VERSION="0.0.0"
elif [ -n "$DESCRIBE_EXACT" ]; then
    # Exactly on a release tag: strip the "boss-v" prefix.
    BOSS_VERSION="${DESCRIBE#boss-v}"
    BOSS_BASE_VERSION="${BOSS_VERSION}"
else
    # Dev build: strip "boss-v" from the latest tag, append "-dev-<SHA>".
    BOSS_BASE_VERSION="${DESCRIBE#boss-v}"
    BOSS_VERSION="${BOSS_BASE_VERSION}-dev-${SHA}"
fi

# Goes to stable-status.txt. Consumers:
#   - boss_short_version_plist: STABLE_BOSS_VERSION / STABLE_BOSS_BASE_VERSION
#   - boss_pkg_unsigned:        STABLE_BOSS_GIT_SHA (embedded in the .pkg filename)
#   - build_info_rs:            STABLE_BOSS_BASE_VERSION only
#   - build_provenance_rs:      STABLE_BOSS_GIT_SHA_FULL / STABLE_BOSS_GIT_DIRTY
#
# IMPORTANT: do not add a per-build or per-commit value (wall-clock time, full
# git SHA, dev-suffixed version) to anything build_info_rs reads. That file is
# compiled into engine_lib/cli/bossctl, so a value that changes every build or
# every commit busts the Rust action cache and forces a full recompile on every
# CI run. build_info_rs deliberately reads only the rarely-changing base version
# so its output is byte-stable and downstream compiles stay cached. The SHA and
# full version remain here solely for the terminal packaging actions above,
# which are cheap and do not recompile Rust. (A wall-clock STABLE_BOSS_BUILD_TIME
# used to live here; it was the prime offender and has been removed.)
#
# STABLE_BOSS_GIT_SHA_FULL / STABLE_BOSS_GIT_DIRTY are read ONLY by
# `build_provenance_rs`, which stamps its own dedicated, single-purpose
# `boss-build-provenance` crate — never `build_info_rs`/engine_lib directly
# — so a value that changes every commit only invalidates that tiny crate,
# not a recompile of engine_lib itself. See that crate's doc comment.
echo "STABLE_BOSS_VERSION $BOSS_VERSION"
echo "STABLE_BOSS_BASE_VERSION $BOSS_BASE_VERSION"
echo "STABLE_BOSS_GIT_SHA $SHA"
echo "STABLE_BOSS_GIT_SHA_FULL $FULL_SHA"
echo "STABLE_BOSS_GIT_DIRTY $DIRTY"

# Goes to volatile-status.txt — not used for version stamping but kept for
# build tooling compatibility.
echo "BUILD_EMBED_LABEL $BOSS_BASE_VERSION"

# Goes to volatile-status.txt. Read ONLY by `build_provenance_rs`'s tiny,
# dedicated crate (see above) — regenerating on every build is expected and
# does not force a recompile of anything else.
echo "BOSS_BUILD_TIME $BUILD_TIME"
