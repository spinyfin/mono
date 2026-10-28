#!/bin/bash
# disk-cache-gc-cap.sh -- size Bazel's disk-cache GC cap to the host's disk.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects. Every function prints its result on stdout and, on failure, a
# `[ci-env] error:` line on stderr and returns non-zero.
#
# Why: `.ci.bazelrc` sets `build:ci --experimental_disk_cache_gc_max_size=3T`
# for the whole fleet. On any host whose cache filesystem is smaller than that
# the size-based GC can never fire, so the cache grows until the disk is full.
# A full disk does not take the agent offline: it stays connected and keeps
# accepting jobs that then never run. ci-env.sh therefore computes a cap from
# the filesystem that actually holds the disk cache and passes it on the
# command line after `--config=ci-<os>`, where it overrides the rc value.
#
# The cap is CI_DISK_CACHE_GC_PERCENT of the filesystem's total size. The
# cache is not the only thing on that filesystem: on Linux the Bazel output
# bases (~10 GB each, one per agent on the host) and the repository cache share
# it, and the GC only runs once the server has been idle for
# --experimental_disk_cache_gc_idle_delay, so the cache can overshoot the cap by
# whatever the builds in between wrote. Half the disk leaves room for all of
# that. A disk is "too small" only when half of it rounds down to 0 GB; that is
# a configuration error, not a host to size the cache for.

CI_DISK_CACHE_GC_PERCENT=50

_ci_disk_cache_err() {
  echo "[ci-env] error: $*" >&2
}

# ci_disk_cache_path <os_type> <ci_bazelrc>
#
# Prints the --disk_cache path that <ci_bazelrc> configures for
# `build:ci-<os_type>`. Parsing the rc rather than duplicating the path keeps
# .ci.bazelrc the single place the cache location is defined. Exactly one
# matching line must exist.
ci_disk_cache_path() {
  local os_type="$1" rcfile="$2"
  local line path="" matches=0
  if [[ ! -r "$rcfile" ]]; then
    _ci_disk_cache_err "cannot read ${rcfile} to find the ci-${os_type} disk cache path"
    return 1
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      "build:ci-${os_type} --disk_cache="*)
        path="${line#*--disk_cache=}"
        matches=$((matches + 1))
        ;;
    esac
  done < "$rcfile"
  if [[ "$matches" -ne 1 || -z "$path" ]]; then
    _ci_disk_cache_err "expected exactly one 'build:ci-${os_type} --disk_cache=<path>' line in ${rcfile}, found ${matches}"
    return 1
  fi
  printf '%s\n' "$path"
}

# ci_disk_cache_fs_total_kb <dir>
#
# Prints the total size, in 1024-byte blocks, of the filesystem holding <dir>.
# <dir> must already exist: the caller creates the cache directory first (as
# bazel itself would), so that df reports the filesystem the cache really
# lands on rather than some ancestor's. Uses POSIX `df -P -k`, whose output is
# the same shape on Linux and macOS: a header line, then one line per
# filesystem whose second field is the total in 1024-blocks. The filesystem
# name can contain spaces on macOS, so the total is taken as the first field
# that is purely digits rather than positionally.
ci_disk_cache_fs_total_kb() {
  local dir="$1"
  local out status line field total=""
  if [[ ! -d "$dir" ]]; then
    _ci_disk_cache_err "disk-cache directory ${dir} does not exist, so its filesystem cannot be determined"
    return 1
  fi
  if ! command -v df > /dev/null 2>&1; then
    _ci_disk_cache_err "df is not available; cannot determine the filesystem holding ${dir}"
    return 1
  fi
  out="$(df -P -k "$dir" 2>&1)"
  status=$?
  if [[ "$status" -ne 0 ]]; then
    _ci_disk_cache_err "df -P -k ${dir} failed (exit ${status}): ${out}"
    return 1
  fi
  line="$(printf '%s\n' "$out" | sed -n '2p')"
  for field in $line; do
    if [[ "$field" =~ ^[0-9]+$ ]]; then
      total="$field"
      break
    fi
  done
  if [[ -z "$total" || "$total" -le 0 ]]; then
    _ci_disk_cache_err "could not parse a total size out of 'df -P -k ${dir}' output: ${out}"
    return 1
  fi
  printf '%s\n' "$total"
}

# ci_disk_cache_gc_cap_from_total_kb <total_kb>
#
# Prints the --experimental_disk_cache_gc_max_size value for a filesystem of
# <total_kb> 1024-byte blocks: CI_DISK_CACHE_GC_PERCENT of it, rounded down to
# whole gigabytes (the unit bazel's size flags take, e.g. `250G`).
ci_disk_cache_gc_cap_from_total_kb() {
  local total_kb="$1"
  local cap_gb
  if [[ ! "$total_kb" =~ ^[0-9]+$ ]]; then
    _ci_disk_cache_err "filesystem size '${total_kb}' is not a whole number of 1024-byte blocks"
    return 1
  fi
  cap_gb=$((total_kb * CI_DISK_CACHE_GC_PERCENT / 100 / 1048576))
  if [[ "$cap_gb" -lt 1 ]]; then
    _ci_disk_cache_err "filesystem of ${total_kb} KB is too small to host a CI disk cache (${CI_DISK_CACHE_GC_PERCENT}% of it is under 1 GB)"
    return 1
  fi
  printf '%sG\n' "$cap_gb"
}
