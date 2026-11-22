#!/bin/bash
# Sourced by boss-release.sh (and by its test); defines functions only, no
# side effects on source.
#
# Cheap already-tagged check used before ci-env.sh builds //tools/release.
# A tag is a Boss release tag when it matches the same version-family rule
# `bin/release prepare` uses: tag_prefix plus patch-counter major.minor.N
# loaded from tools/boss/release.toml.
# Prefix glob `<tag_prefix>*` is the fetch/list step; the u64 digit suffix is
# the filter `counter_for_tag` applies. Tests in boss_head_release_tag_test.sh
# and tools/release/src/version.rs read the same tools/boss/release.toml and
# shared case corpus to keep those two in agreement.

CI_BOSS_RELEASE_REPO="${CI_BOSS_RELEASE_REPO:-}"
CI_BOSS_RELEASE_TAG_PREFIX="${CI_BOSS_RELEASE_TAG_PREFIX:-}"
CI_BOSS_RELEASE_MAJOR_MINOR="${CI_BOSS_RELEASE_MAJOR_MINOR:-}"

# ci_boss_load_release_config [config_path]
#
# Parses repo, tag_prefix, [version].scheme, and [version].major_minor from
# tools/boss/release.toml. Returns 1 if the file is missing, invalid, or uses
# a scheme other than patch-counter with a literal major_minor.
ci_boss_load_release_config() {
  local config_path="${1:-tools/boss/release.toml}"
  [[ -f "$config_path" ]] || return 1

  local parsed
  parsed="$(awk '
    function trim(s) {
      sub(/^[ \t\r\n]+/, "", s)
      sub(/[ \t\r\n]+$/, "", s)
      return s
    }
    function unquote(s) {
      s = trim(s)
      if (s ~ /^"[^"]*"$/) {
        return substr(s, 2, length(s) - 2)
      }
      return ""
    }
    {
      line = $0
      sub(/#.*$/, "", line)
      line = trim(line)
      if (line == "") next
      if (line ~ /^\[[^][]+\]$/) {
        section = trim(substr(line, 2, length(line) - 2))
        next
      }
      eq = index(line, "=")
      if (eq == 0) next
      key = trim(substr(line, 1, eq - 1))
      val = unquote(substr(line, eq + 1))
      if (section == "" && key == "repo") repo = val
      else if (section == "" && key == "tag_prefix") tag_prefix = val
      else if (section == "version" && key == "scheme") scheme = val
      else if (section == "version" && key == "major_minor") major_minor = val
    }
    END {
      if (scheme != "patch-counter" || repo == "" || tag_prefix == "" || major_minor !~ /^[0-9]+\.[0-9]+$/) {
        exit 1
      }
      printf "%s\n%s\n%s\n", repo, tag_prefix, major_minor
    }
  ' "$config_path")" || return 1

  {
    IFS= read -r CI_BOSS_RELEASE_REPO &&
    IFS= read -r CI_BOSS_RELEASE_TAG_PREFIX &&
    IFS= read -r CI_BOSS_RELEASE_MAJOR_MINOR
  } < <(printf '%s\n' "$parsed") || return 1
}

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

# ci_boss_u64_normalize <digits>
#
# Prints the leading-zero-stripped decimal string when `digits` is a valid u64
# (0..=18446744073709551615), matching Rust's str::parse::<u64>(); returns 1
# otherwise.
ci_boss_u64_normalize() {
  local value="$1"
  [[ "$value" =~ ^[0-9]+$ ]] || return 1
  while [[ "$value" == 0?* ]]; do
    value="${value#0}"
  done
  local len="${#value}"
  if (( len > 20 )); then
    return 1
  elif (( len == 20 )) && [[ "$value" > "18446744073709551615" ]]; then
    return 1
  fi
  printf '%s' "$value"
}

# ci_boss_u64_gt <a> <b>
#
# Compares two normalized u64 decimal strings without bash signed 64-bit
# arithmetic overflow.
ci_boss_u64_gt() {
  local a="$1" b="$2"
  (( ${#a} > ${#b} )) || { (( ${#a} == ${#b} )) && [[ "$a" > "$b" ]]; }
}

# ci_boss_release_tag_matches <tag> [prefix] [major.minor]
#
# Returns 0 when `tag` is in the Boss patch-counter version family.
ci_boss_release_tag_matches() {
  local tag="$1"
  local prefix="${2:-${CI_BOSS_RELEASE_TAG_PREFIX}}"
  local major_minor="${3:-${CI_BOSS_RELEASE_MAJOR_MINOR}}"
  local suffix n
  [[ -n "$prefix" && -n "$major_minor" ]] || return 1
  [[ "$tag" == "${prefix}"* ]] || return 1
  suffix="${tag#"$prefix"}"
  [[ "$suffix" == "${major_minor}."* ]] || return 1
  n="${suffix#"${major_minor}."}"
  ci_boss_u64_normalize "$n" >/dev/null
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
  local tag suffix n norm best="" best_n=""

  [[ -n "$prefix" && -n "$major_minor" ]] || return 1

  while IFS= read -r tag || [[ -n "$tag" ]]; do
    [[ -n "$tag" ]] || continue
    ci_boss_release_tag_matches "$tag" "$prefix" "$major_minor" || continue
    suffix="${tag#"$prefix"}"
    n="${suffix#"${major_minor}."}"
    norm="$(ci_boss_u64_normalize "$n")" || continue
    if [[ -z "$best_n" ]] || ci_boss_u64_gt "$norm" "$best_n"; then
      best="$tag"
      best_n="$norm"
    fi
  done < <(ci_boss_git "$repo_dir" tag --points-at HEAD --list "${prefix}*" || true)

  if [[ -n "$best" ]]; then
    printf '%s' "$best"
    return 0
  fi
  return 1
}

# ci_boss_resume_draft_enabled
#
# Matches tools/release/src/main.rs: RELEASE_RESUME_DRAFT=1, falling back to
# CHECKLEFT_RESUME_DRAFT=1 when RELEASE_RESUME_DRAFT is unset.
ci_boss_resume_draft_enabled() {
  local val=""
  if [[ -n "${RELEASE_RESUME_DRAFT+x}" ]]; then
    val="$RELEASE_RESUME_DRAFT"
  else
    val="${CHECKLEFT_RESUME_DRAFT:-}"
  fi
  [[ "$val" == "1" ]]
}

# Last successful TSV listing from ci_boss_github_family_release_rows.
CI_BOSS_RELEASE_ROWS=""
# Stderr captured from the last failed ci_boss_github_family_release_rows call.
CI_BOSS_GH_STDERR=""

# ci_boss_github_family_release_rows [repo]
#
# Stores TSV rows `tag<TAB>draft` for every GitHub Release in `repo` into
# CI_BOSS_RELEASE_ROWS (`draft` is `true` or `false`). Uses `gh api --paginate`
# rather than `/releases/latest`, which would pick a non-Boss family such as
# checkleft. On gh failure, returns 1 and stores captured stderr in
# CI_BOSS_GH_STDERR. Globals are used so the values survive the call; a
# command substitution would run this function in a subshell and drop them.
ci_boss_github_family_release_rows() {
  local repo="${1:-${CI_BOSS_RELEASE_REPO}}"
  local listing status=0
  CI_BOSS_RELEASE_ROWS=""
  CI_BOSS_GH_STDERR=""
  [[ -n "$repo" ]] || return 1
  # Merge stderr into the capture: a failed `gh` prints nothing on stdout, so
  # the captured text is the error. Avoids a temp file in the CI sandbox.
  listing="$(gh api "repos/${repo}/releases" --paginate -X GET -F per_page=100 \
    --jq '.[] | [.tag_name, (.draft|tostring)] | @tsv' 2>&1)" || status=$?
  if (( status != 0 )); then
    CI_BOSS_GH_STDERR="$listing"
    return 1
  fi
  CI_BOSS_RELEASE_ROWS="$listing"
  return 0
}

# ci_boss_select_latest_published_family_tag
#
# Reads TSV rows `tag<TAB>draft` from stdin and prints the highest published
# (draft == false) version-family tag. Returns 1 when none match.
ci_boss_select_latest_published_family_tag() {
  local tag draft suffix n norm best="" best_n=""
  local prefix="${CI_BOSS_RELEASE_TAG_PREFIX}"
  local major_minor="${CI_BOSS_RELEASE_MAJOR_MINOR}"
  [[ -n "$prefix" && -n "$major_minor" ]] || return 1
  while IFS=$'\t' read -r tag draft || [[ -n "$tag" ]]; do
    [[ -n "$tag" ]] || continue
    [[ "$draft" == "false" ]] || continue
    ci_boss_release_tag_matches "$tag" "$prefix" "$major_minor" || continue
    suffix="${tag#"$prefix"}"
    n="${suffix#"${major_minor}."}"
    norm="$(ci_boss_u64_normalize "$n")" || continue
    if [[ -z "$best_n" ]] || ci_boss_u64_gt "$norm" "$best_n"; then
      best="$tag"
      best_n="$norm"
    fi
  done
  if [[ -n "$best" ]]; then
    printf '%s' "$best"
    return 0
  fi
  return 1
}

# ci_boss_row_draft_for_tag <tag>
#
# Reads TSV rows `tag<TAB>draft` from stdin and prints the draft flag for
# `tag` (`true` or `false`). Prints nothing when `tag` is absent.
ci_boss_row_draft_for_tag() {
  local want="$1" tag draft found=""
  while IFS=$'\t' read -r tag draft || [[ -n "$tag" ]]; do
    if [[ "$tag" == "$want" ]]; then
      found="$draft"
    fi
  done
  printf '%s' "$found"
}

# Log line `bin/release prepare` prints for SkipReason::AlreadyReleased.
# Keep in lockstep with tools/release/src/decision.rs and commands.rs.
ci_boss_already_released_skip_message() {
  local tag="$1"
  printf 'release skipped: HEAD is already %s — re-releasing the same commit is a no-op' "$tag"
}

# One-line fall-through reason when a HEAD family tag exists but the cheap
# skip is not taken. Callers still proceed to `bin/release prepare`.
ci_boss_early_skip_not_taken() {
  echo "release early-skip not taken: $*" >&2
}
