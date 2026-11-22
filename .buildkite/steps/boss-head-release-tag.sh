#!/bin/bash
# Sourced by boss-release.sh (and by its test); defines functions only, no
# side effects on source.
#
# Cheap already-tagged check used before ci-env.sh builds //tools/release.
# A tag is a Boss release tag when it matches the same version-family rule
# `bin/release prepare` uses: tag_prefix plus patch-counter major.minor.N
# (tools/boss/release.toml: tag_prefix = "boss-v", major_minor = "1.0").
# Prefix glob `boss-v*` is the fetch/list step; the digit suffix is the
# filter `counter_for_tag` applies. Tests in boss_head_release_tag_test.sh
# and tools/release/src/version.rs keep those two in agreement.

# Must match tools/boss/release.toml. Overridable in tests.
CI_BOSS_RELEASE_TAG_PREFIX="${CI_BOSS_RELEASE_TAG_PREFIX:-boss-v}"
CI_BOSS_RELEASE_MAJOR_MINOR="${CI_BOSS_RELEASE_MAJOR_MINOR:-1.0}"

# ci_boss_git <repo_dir> <git args...>
#
# Same checkout layout as ci_fetch_boss_tags: a normal git work tree, or a
# jj workspace whose bare store lives at .jj/repo/store/git.
ci_boss_git() {
  local repo_dir="$1"
  shift
  if [[ -d "${repo_dir}/.jj/repo/store/git" && ! -e "${repo_dir}/.git" ]]; then
    env "GIT_DIR=${repo_dir}/.jj/repo/store/git" git "$@"
  else
    git -C "$repo_dir" "$@"
  fi
}

# ci_boss_release_tag_matches <tag> [prefix] [major.minor]
#
# Returns 0 when `tag` is in the Boss patch-counter version family.
ci_boss_release_tag_matches() {
  local tag="$1"
  local prefix="${2:-${CI_BOSS_RELEASE_TAG_PREFIX}}"
  local major_minor="${3:-${CI_BOSS_RELEASE_MAJOR_MINOR}}"
  local suffix n
  [[ "$tag" == "${prefix}"* ]] || return 1
  suffix="${tag#"$prefix"}"
  [[ "$suffix" == "${major_minor}."* ]] || return 1
  n="${suffix#"${major_minor}."}"
  [[ "$n" =~ ^[0-9]+$ ]] || return 1
  return 0
}

# ci_boss_head_release_tag [repo_dir]
#
# Prints the highest version-family tag pointing at HEAD, or returns 1 when
# HEAD carries none. Listing uses the tag_prefix glob; filtering uses
# ci_boss_release_tag_matches.
ci_boss_head_release_tag() {
  local repo_dir="${1:-.}"
  local prefix="${CI_BOSS_RELEASE_TAG_PREFIX}"
  local major_minor="${CI_BOSS_RELEASE_MAJOR_MINOR}"
  local tag suffix n best="" best_n=""

  while IFS= read -r tag || [[ -n "$tag" ]]; do
    [[ -n "$tag" ]] || continue
    ci_boss_release_tag_matches "$tag" "$prefix" "$major_minor" || continue
    suffix="${tag#"$prefix"}"
    n="${suffix#"${major_minor}."}"
    if [[ -z "$best_n" ]] || (( 10#$n > 10#$best_n )); then
      best="$tag"
      best_n="$n"
    fi
  done < <(ci_boss_git "$repo_dir" tag --points-at HEAD --list "${prefix}*" || true)

  if [[ -n "$best" ]]; then
    printf '%s' "$best"
    return 0
  fi
  return 1
}

# Log line `bin/release prepare` prints for SkipReason::AlreadyReleased.
# Keep in lockstep with tools/release/src/decision.rs and commands.rs.
ci_boss_already_released_skip_message() {
  local tag="$1"
  printf 'release skipped: HEAD is already %s — re-releasing the same commit is a no-op' "$tag"
}
