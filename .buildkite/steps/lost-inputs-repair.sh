#!/bin/bash
# lost-inputs-repair.sh -- stop a Bazel disk cache from re-serving results whose
# outputs the remote cache no longer has.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects.
#
# The failure signature: a step dies with
#   Testing //x:y failed: Lost inputs no longer available remotely: p/y.zip (<sha256>/<size>)
# after Bazel's own automatic restarts (--experimental_remote_cache_eviction_retries)
# have all lost an input.
#
# Mechanism, as shown against real Bazel by tests/lost_inputs_integration.sh:
# when the disk cache holds an action-cache (AC) entry naming a blob that the
# remote cache no longer has, but the blob is still in the disk cache's cas/,
# Bazel treats the producer as done without downloading the blob, and a remote
# action handed that digest as an input cannot find it. (When the blob is in
# neither place, Bazel drops the entry itself and re-runs the producer.)
#
# The repair deletes the disk-cache AC entries that name a lost digest (cas/
# blobs stay), so the next attempt misses on them and re-runs the producer.
# It addresses only that layer. It does nothing when the dangling reference is
# held elsewhere, e.g. by the remote action cache or by the output base's
# action cache, and which layer holds it on the affected agents has not been
# established. The attempt is bounded to one retry, and no cache is bypassed or
# disabled.
#
# Whenever no disk-cache entry matches, or the retry fails again, the log
# carries every lost digest, whether cas/ has it, how many AC entries named it
# and how many AC entries the cache holds, and a Buildkite annotation raises
# the occurrence (ci_diagnose_lost_inputs).

# ci_lost_input_digests <logfile>
#
# Prints the distinct sha256 digests Bazel reported as lost, one per line. Every
# line mentioning the error is scanned, and every "(<sha256>/<size>)" on it is
# taken, so a message that lists several lost inputs yields all of them and
# paths containing spaces do not matter.
ci_lost_input_digests() {
  local logfile="$1"
  [[ -r "$logfile" ]] || return 0
  grep -a 'Lost inputs no longer available remotely' "$logfile" 2> /dev/null \
    | grep -aoE '\([0-9a-f]{64}/[0-9]+\)' \
    | sed -E 's/^\(([0-9a-f]{64})\/[0-9]+\)$/\1/' \
    | sort -u
}

# ci_prune_dangling_ac_entries <disk_cache_dir> <digest>...
#
# Removes every <disk_cache_dir>/ac entry that names one of the digests, with a
# log line for each lost digest (is its blob in cas/, how many entries named it)
# and for each removed entry. Prints the number of entries removed on stdout.
ci_prune_dangling_ac_entries() {
  local cache="$1"
  shift
  local digest entry matched in_cas total_ac removed=0 valid="" hits=""
  if [[ ! -d "${cache}/ac" ]]; then
    echo "[lost-inputs-repair] ${cache}/ac does not exist; nothing to prune" >&2
    echo 0
    return 0
  fi
  for digest in "$@"; do
    [[ "$digest" =~ ^[0-9a-f]{64}$ ]] && valid+="${digest}"$'\n'
  done
  [[ -n "$valid" ]] || {
    echo 0
    return 0
  }
  # One pass over ac/ for all digests (the patterns come through a process
  # substitution, so no temp file is needed). ActionResult entries are protobuf
  # and carry each output digest as hex text.
  while IFS= read -r entry; do
    [[ -n "$entry" ]] || continue
    while IFS= read -r matched; do
      hits+="${matched}"$'\n'
      echo "[lost-inputs-repair] removing action-cache entry ${entry#"${cache}"/} (names lost blob ${matched})" >&2
    done < <(grep -aoF -f <(printf '%s' "$valid") "$entry" 2> /dev/null | sort -u)
    # Count only entries that are really gone: a failed removal must not
    # look like progress to the caller.
    if rm -f -- "$entry" 2> /dev/null && [[ ! -e "$entry" ]]; then
      removed=$((removed + 1))
    else
      echo "[lost-inputs-repair] could not remove ${entry#"${cache}"/}" >&2
    fi
  done < <(grep -rlaF -f <(printf '%s' "$valid") "${cache}/ac" 2> /dev/null || true)
  total_ac="$(find "${cache}/ac" -type f 2> /dev/null | wc -l | tr -d ' ')"
  echo "[lost-inputs-repair] ${cache}/ac holds ${total_ac} entries" >&2
  while IFS= read -r digest; do
    [[ -n "$digest" ]] || continue
    # Presence is logged for diagnosis only. Search cas/ by file name rather
    # than assuming the cas/<xx>/<digest> layout (a compressed cache may name
    # or place blobs differently).
    if [[ -n "$(find "${cache}/cas" -name "${digest}*" -print -quit 2> /dev/null)" ]]; then in_cas=yes; else in_cas=no; fi
    echo "[lost-inputs-repair] lost blob ${digest}: in disk-cache cas=${in_cas}, action-cache entries that named it=$(printf '%s' "$hits" | grep -cxF -- "$digest" || true)" >&2
  done < <(printf '%s' "$valid" | sort -u)
  echo "$removed"
}

# ci_diagnose_lost_inputs <logfile> <disk_cache_dir>
#
# Non-mutating: logs, for every lost digest in the log, whether the disk
# cache's cas/ has it and how many disk-cache AC entries name it, plus the size
# of the AC, then raises the "NOT repaired" log group and Buildkite annotation.
# Removes nothing and retries nothing; used when no repair is possible or when
# the one allowed retry has already failed. Returns 0 if the log named any lost
# digest, 1 otherwise.
ci_diagnose_lost_inputs() {
  local logfile="$1" cache="$2"
  local -a digests=()
  local d in_cas n_ac total_ac
  while IFS= read -r d; do
    [[ -n "$d" ]] && digests+=("$d")
  done < <(ci_lost_input_digests "$logfile")
  [[ ${#digests[@]} -gt 0 ]] || return 1
  total_ac="$(find "${cache}/ac" -type f 2> /dev/null | wc -l | tr -d ' ')"
  echo "[lost-inputs-repair] ${cache}/ac holds ${total_ac} entries" >&2
  for d in "${digests[@]}"; do
    if [[ -n "$(find "${cache}/cas" -name "${d}*" -print -quit 2> /dev/null)" ]]; then in_cas=yes; else in_cas=no; fi
    n_ac="$(grep -rlaF -- "$d" "${cache}/ac" 2> /dev/null | wc -l | tr -d ' ')"
    echo "[lost-inputs-repair] lost blob ${d}: in disk-cache cas=${in_cas}, disk-cache action-cache entries naming it=${n_ac}" >&2
  done
  echo "+++ Lost inputs NOT repaired: the disk cache cannot account for ${#digests[@]} lost blob(s) (or a retry after pruning lost inputs again); the dangling reference is held by another layer" >&2
  if command -v buildkite-agent > /dev/null 2>&1; then
    buildkite-agent annotate --style warning --context lost-inputs-unrepaired \
      "Bazel lost ${#digests[@]} input(s) the remote cache no longer has and the repair did not resolve it. Digests: ${digests[*]}. If the remote action cache names them, its operator must purge those entries." \
      > /dev/null 2>&1 || true
  fi
  return 0
}

# ci_repair_lost_inputs <logfile> <disk_cache_dir>
#
# Returns 0 when the log shows lost inputs and at least one action-cache entry
# naming them was removed, i.e. when another attempt can differ from the one
# that failed. Returns 1 otherwise, so the caller does not repeat a failure it
# cannot have changed.
ci_repair_lost_inputs() {
  local logfile="$1" cache="$2"
  local -a digests=()
  local d removed
  while IFS= read -r d; do
    [[ -n "$d" ]] && digests+=("$d")
  done < <(ci_lost_input_digests "$logfile")
  [[ ${#digests[@]} -gt 0 ]] || return 1
  echo "[lost-inputs-repair] ${#digests[@]} blob(s) reported lost; checking ${cache} for action-cache entries that name them" >&2
  removed="$(ci_prune_dangling_ac_entries "$cache" "${digests[@]}")"
  if [[ "$removed" -eq 0 ]]; then
    ci_diagnose_lost_inputs "$logfile" "$cache" || true
  fi
  [[ "$removed" -gt 0 ]]
}
