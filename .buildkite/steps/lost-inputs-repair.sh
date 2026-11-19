#!/bin/bash
# lost-inputs-repair.sh -- stop a Bazel disk cache from re-serving results whose
# outputs the remote cache no longer has.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects.
#
# The failure: a step dies with
#   Testing //x:y failed: Lost inputs no longer available remotely: p/y.zip (<sha256>/<size>)
# after Bazel's own automatic restarts (--experimental_remote_cache_eviction_retries,
# five by default) have all lost an input. Tests never ran.
#
# What the logs show (buildkite mono builds 27439 and 27448, mac-app-build):
#   * The first build reports "2895 action cache hit, 57 disk cache hit,
#     1 remote cache hit"; each restart then reports roughly "934 disk cache hit,
#     10 remote cache hit" and finishes in 0.4 to 3 seconds without executing
#     anything. The results being reused come from the agent's disk cache.
#   * The same blob is lost again by later restarts (in 27439, the Chrome,
#     CommentsTranscript and Shard5 zips each twice). A restart that leaves the
#     disk cache's action-cache entries in place just meets them again.
# The disk cache's action-cache entry names output blobs by digest. Bazel, which
# does not download these outputs, takes the entry as proof that the outputs
# exist, hands the digest to a remote action as an input, and finds the remote
# cache no longer has it. Whether the agent's own cas/ still holds the blob is
# not visible in the logs, and it does not decide the outcome: the restarts
# demonstrably do not recover either way.
#
# The repair: delete the disk-cache action-cache entries that name a lost
# digest. Nothing else is touched: other entries and every cas/ blob stay, and
# no cache is bypassed or disabled. The next attempt misses on exactly those
# entries, re-runs the producing actions, and uploads their outputs again.
#
# Not covered: an action-cache entry held only by the remote cache cannot be
# deleted from here. When that is the cause no disk-cache entry matches, the
# repair says so (and returns non-zero so the caller does not repeat a failure
# it did not change), and the log carries each lost digest so the next look can
# trace it.

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
  local digest entry matched in_cas removed=0 valid="" hits=""
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
    rm -f -- "$entry"
    removed=$((removed + 1))
  done < <(grep -rlaF -f <(printf '%s' "$valid") "${cache}/ac" 2> /dev/null || true)
  while IFS= read -r digest; do
    [[ -n "$digest" ]] || continue
    # Presence is logged for diagnosis only. The file may carry a suffix when
    # the disk cache stores blobs compressed, so match on the digest prefix.
    if compgen -G "${cache}/cas/${digest:0:2}/${digest}*" > /dev/null; then in_cas=yes; else in_cas=no; fi
    echo "[lost-inputs-repair] lost blob ${digest}: in disk-cache cas=${in_cas}, action-cache entries that named it=$(printf '%s' "$hits" | grep -cxF -- "$digest" || true)" >&2
  done < <(printf '%s' "$valid" | sort -u)
  echo "$removed"
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
    echo "[lost-inputs-repair] no disk-cache entry names a lost blob; the dangling reference is not in this disk cache, so a retry would fail the same way" >&2
  fi
  [[ "$removed" -gt 0 ]]
}
