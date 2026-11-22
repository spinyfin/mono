#!/bin/bash
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects on source.
#
# Syncs `boss-v*` tags from origin before any Bazel command runs.
#
# Buildkite checks out commits with `git fetch -v --prune -- origin <sha>` and
# does not fetch tags, so each agent's checkout accumulates a different subset
# of `boss-v*` tags over time. tools/boss/installer/workspace-status.sh derives
# STABLE_BOSS_BASE_VERSION from `git describe --tags --match "boss-v*"`, which
# feeds into build_info_rs and downstream Rust crates/tests; different tags per
# agent produce different cache keys for the same commit.
#
# Using an explicit refspec with `--prune --no-tags` and leading `+` fetches
# all `boss-v*` tags, updates any that moved on origin, and prunes stale local
# `boss-v*` tags that were deleted on origin, without fetching or pruning
# unrelated tags.

# ci_fetch_boss_tags [repo_dir] [remote]
#
# Fetches and prunes `refs/tags/boss-v*` from `remote` (default: origin) in
# `repo_dir` (default: .). Supports both normal git checkouts and jj
# workspaces (whose bare git store lives at .jj/repo/store/git).
ci_fetch_boss_tags() {
  local repo_dir="${1:-.}"
  local remote="${2:-origin}"
  local -a git_cmd=(git -C "$repo_dir")

  if [[ -d "${repo_dir}/.jj/repo/store/git" && ! -e "${repo_dir}/.git" ]]; then
    git_cmd=(env "GIT_DIR=${repo_dir}/.jj/repo/store/git" git)
  fi

  "${git_cmd[@]}" fetch --prune --no-tags "$remote" '+refs/tags/boss-v*:refs/tags/boss-v*'
}
