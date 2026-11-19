#!/bin/bash
# lost-inputs-repair.sh -- heal a Bazel disk cache that names blobs nobody has.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects.
#
# The failure: a step dies with
#   Testing //x:y failed: Lost inputs no longer available remotely: p/y.zip (<sha256>/<size>)
# after Bazel's own automatic restarts (--experimental_remote_cache_eviction_retries,
# five by default) have all lost an input. Tests never ran.
#
# The cause: a remote action needs, as an input, an output that an earlier
# action produced. Bazel did not download that output (--remote_download_minimal)
# and trusts that the remote cache has it. When the remote cache has lost the
# blob, Bazel falls back to the disk cache, whose action-cache entry for the
# producing action still names the blob. That entry is dangling: the blob is
# in neither the remote cache nor this disk cache's own cas/ directory, so
# there is nothing to upload. Every restart consults the same entry, hits it
# again, and loses the same or a sibling input. Re-running the producing
# action is the only way out, and Bazel never gets to it, because the dangling
# entry keeps answering "cached".
#
# The repair: delete exactly the disk-cache action-cache entries that name a
# lost blob which the disk cache cannot supply. Nothing else is touched: every
# entry whose blobs are present stays, a blob that is present in cas/ is left
# for Bazel to upload, and the remote cache and Bazel's action cache are never
# bypassed or disabled. The next attempt misses on the dangling entries only
# and re-runs those producing actions, which also re-uploads their outputs.

# ci_lost_input_digests <logfile>
#
# Prints the distinct sha256 digests Bazel reported as lost, one per line.
# Bazel prints "(<sha256>/<size bytes>)" after each lost path.
ci_lost_input_digests() {
  local logfile="$1"
  [[ -r "$logfile" ]] || return 0
  grep -o 'Lost inputs no longer available remotely: [^ ]* ([0-9a-f]\{64\}/[0-9]*)' "$logfile" 2> /dev/null \
    | sed -E 's/.*\(([0-9a-f]{64})\/[0-9]*\)$/\1/' \
    | sort -u
}

# ci_prune_dangling_ac_entries <disk_cache_dir> <digest>...
#
# For each digest absent from <disk_cache_dir>/cas, removes every
# <disk_cache_dir>/ac entry that names it, and logs each removal. Prints the
# number of entries removed on stdout.
ci_prune_dangling_ac_entries() {
  local cache="$1"
  shift
  local digest entry removed=0
  if [[ ! -d "${cache}/ac" ]]; then
    echo 0
    return 0
  fi
  for digest in "$@"; do
    [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || continue
    # A blob the disk cache still holds can be uploaded to the remote cache,
    # so its entries are not dangling.
    [[ -e "${cache}/cas/${digest:0:2}/${digest}" ]] && continue
    # ActionResult entries carry their output digests as hex text.
    while IFS= read -r entry; do
      [[ -n "$entry" ]] || continue
      rm -f -- "$entry"
      removed=$((removed + 1))
      echo "[lost-inputs-repair] removed dangling action-cache entry ${entry#"${cache}"/} (names lost blob ${digest})" >&2
    done < <(grep -rlF -- "$digest" "${cache}/ac" 2> /dev/null || true)
  done
  echo "$removed"
}

# ci_repair_lost_inputs <logfile> <disk_cache_dir>
#
# Returns 0 when the log shows lost inputs and at least one dangling
# action-cache entry was removed, i.e. when another attempt can differ from
# the one that failed. Returns 1 otherwise, so the caller does not repeat a
# failure it cannot have changed.
ci_repair_lost_inputs() {
  local logfile="$1" cache="$2"
  local -a digests=()
  local d removed
  while IFS= read -r d; do
    [[ -n "$d" ]] && digests+=("$d")
  done < <(ci_lost_input_digests "$logfile")
  [[ ${#digests[@]} -gt 0 ]] || return 1
  echo "[lost-inputs-repair] ${#digests[@]} blob(s) reported lost; checking ${cache} for dangling action-cache entries" >&2
  removed="$(ci_prune_dangling_ac_entries "$cache" "${digests[@]}")"
  [[ "$removed" -gt 0 ]]
}
