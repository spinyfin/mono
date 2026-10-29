#!/bin/bash
#
# Tests for .buildkite/steps/disk-cache-gc-cap.sh, the library ci-env.sh uses
# to size --experimental_disk_cache_gc_max_size to the filesystem that holds
# the CI disk cache.
#
# `df` is stubbed: the hermetic test sandbox's PATH does not carry it, and a
# stub is also what lets the test exercise specific filesystem sizes, a df
# failure, and df's output shape on both OSes. The pure arithmetic and the rc
# parsing run against the real library code.
#
# Fixture files are written with printf, not here-documents: bash 3.2 (the
# macOS /bin/bash) backs here-documents with files under /tmp regardless of
# TMPDIR, and the sandbox denies that.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
# shellcheck source=../disk-cache-gc-cap.sh
source "${workspace}/.buildkite/steps/disk-cache-gc-cap.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# expect_output <expected> <description> <command...>
expect_output() {
  local expected="$1" what="$2"
  shift 2
  local actual
  if ! actual="$("$@" 2> /dev/null)"; then
    fail "${what}: expected success with output '${expected}', command failed"
    return
  fi
  if [[ "$actual" != "$expected" ]]; then
    fail "${what}: expected '${expected}', got '${actual}'"
  fi
}

# expect_error <stderr-substring> <description> <command...>
expect_error() {
  local needle="$1" what="$2"
  shift 2
  local stdout stderr_file
  stderr_file="$(mktemp "${TEST_TMPDIR:?}/stderr.XXXXXX")"
  if stdout="$("$@" 2> "$stderr_file")"; then
    fail "${what}: expected failure, succeeded with output '${stdout}'"
    return
  fi
  if [[ -n "$stdout" ]]; then
    fail "${what}: a failing call must print nothing on stdout, printed '${stdout}'"
  fi
  if ! grep -q "\[ci-env\] error: " "$stderr_file"; then
    fail "${what}: stderr lacks the '[ci-env] error:' prefix: $(cat "$stderr_file")"
  fi
  if ! grep -qF -- "$needle" "$stderr_file"; then
    fail "${what}: stderr lacks '${needle}': $(cat "$stderr_file")"
  fi
}

# write_lines <file> <line...>
write_lines() {
  local file="$1"
  shift
  printf '%s\n' "$@" > "$file"
}

# ---------------------------------------------------------------------------
# ci_disk_cache_gc_cap_from_total_kb: cap arithmetic.
# ---------------------------------------------------------------------------

# The host from the incident: a 500 GB VM whose df reported 489 GiB total.
expect_output "391G" "489 GiB disk" ci_disk_cache_gc_cap_from_total_kb 512753664
# An exact 500 GiB filesystem.
expect_output "400G" "500 GiB disk" ci_disk_cache_gc_cap_from_total_kb 524288000
# The 3.6 TiB secondary disks on the office Linux hosts.
expect_output "2880G" "3.6 TiB disk" ci_disk_cache_gc_cap_from_total_kb 3774873600
# 1.25 GiB is the smallest filesystem that yields a whole gigabyte; anything
# less rounds down to zero and must be refused.
expect_output "1G" "1.25 GiB disk" ci_disk_cache_gc_cap_from_total_kb 1310720
expect_error "too small" "just under 1.25 GiB" ci_disk_cache_gc_cap_from_total_kb 1310719
expect_error "too small" "1 GiB disk" ci_disk_cache_gc_cap_from_total_kb 1048576
expect_error "too small" "zero-size filesystem" ci_disk_cache_gc_cap_from_total_kb 0
expect_error "not a whole number" "non-numeric size" ci_disk_cache_gc_cap_from_total_kb 12abc
expect_error "not a whole number" "empty size" ci_disk_cache_gc_cap_from_total_kb ""
expect_error "not a whole number" "negative size" ci_disk_cache_gc_cap_from_total_kb -5
# Bazel size flags take whole units, so the cap is never fractional.
expect_output "7G" "rounds down, never up" ci_disk_cache_gc_cap_from_total_kb 10485759
if [[ "$CI_DISK_CACHE_GC_PERCENT" -le 0 || "$CI_DISK_CACHE_GC_PERCENT" -ge 100 ]]; then
  fail "CI_DISK_CACHE_GC_PERCENT must leave headroom on the disk, is ${CI_DISK_CACHE_GC_PERCENT}"
fi

# ---------------------------------------------------------------------------
# ci_disk_cache_path: locating the configured --disk_cache in .ci.bazelrc.
# ---------------------------------------------------------------------------

rc_dir="$(mktemp -d "${TEST_TMPDIR:?}/rc.XXXXXX")"
write_lines "${rc_dir}/good.bazelrc" \
  'build:ci --experimental_disk_cache_gc_max_size=3T' \
  'build:ci-linux --config=ci' \
  'build:ci-linux --repository_cache=/mnt/ssd/bazel/repo_cache' \
  'build:ci-linux --disk_cache=/mnt/ssd/bazel/disk_cache' \
  'build:ci-darwin --config=ci' \
  'build:ci-darwin --disk_cache=/Volumes/ssd/bazel/disk_cache'
expect_output "/mnt/ssd/bazel/disk_cache" "linux path" ci_disk_cache_path linux "${rc_dir}/good.bazelrc"
expect_output "/Volumes/ssd/bazel/disk_cache" "darwin path" ci_disk_cache_path darwin "${rc_dir}/good.bazelrc"
expect_error "found 0" "unknown os" ci_disk_cache_path freebsd "${rc_dir}/good.bazelrc"
expect_error "cannot read" "missing rc file" ci_disk_cache_path linux "${rc_dir}/absent.bazelrc"

write_lines "${rc_dir}/dup.bazelrc" \
  'build:ci-linux --disk_cache=/mnt/ssd/bazel/disk_cache' \
  'build:ci-linux --disk_cache=/mnt/other/disk_cache'
expect_error "found 2" "ambiguous rc" ci_disk_cache_path linux "${rc_dir}/dup.bazelrc"

write_lines "${rc_dir}/empty-path.bazelrc" 'build:ci-linux --disk_cache='
expect_error "found 1" "empty path" ci_disk_cache_path linux "${rc_dir}/empty-path.bazelrc"

# Bazel expands `~` and `%workspace%` in --disk_cache; this library does not, so
# anything but an absolute path must fail rather than be measured in the wrong
# place.
write_lines "${rc_dir}/tilde.bazelrc" 'build:ci-linux --disk_cache=~/.cache/bazelcache'
expect_error "must be an absolute path" "tilde path" ci_disk_cache_path linux "${rc_dir}/tilde.bazelrc"
write_lines "${rc_dir}/workspace.bazelrc" 'build:ci-linux --disk_cache=%workspace%/cache'
expect_error "must be an absolute path" "%workspace% path" ci_disk_cache_path linux "${rc_dir}/workspace.bazelrc"
write_lines "${rc_dir}/relative.bazelrc" 'build:ci-linux --disk_cache=cache/dir'
expect_error "must be an absolute path" "relative path" ci_disk_cache_path linux "${rc_dir}/relative.bazelrc"

# A final line without a trailing newline still counts.
printf 'build:ci-linux --disk_cache=/mnt/ssd/bazel/disk_cache' > "${rc_dir}/no-newline.bazelrc"
expect_output "/mnt/ssd/bazel/disk_cache" "no trailing newline" ci_disk_cache_path linux "${rc_dir}/no-newline.bazelrc"

# The real rc must keep the lines the library looks for, on both OSes.
real_rc="${workspace}/.ci.bazelrc"
expect_output "/mnt/ssd/bazel/disk_cache" "real .ci.bazelrc linux" ci_disk_cache_path linux "$real_rc"
expect_output "/Volumes/ssd/bazel/disk_cache" "real .ci.bazelrc darwin" ci_disk_cache_path darwin "$real_rc"

# ---------------------------------------------------------------------------
# ci_disk_cache_fs_total_kb: reading the filesystem size via df.
# ---------------------------------------------------------------------------

cache_dir="$(mktemp -d "${TEST_TMPDIR:?}/disk_cache.XXXXXX")"
stub_bin="$(mktemp -d "${TEST_TMPDIR:?}/bin.XXXXXX")"
original_path="$PATH"

# install_df <line...>: installs a `df` stub whose body is the given lines.
install_df() {
  write_lines "${stub_bin}/df" '#!/bin/bash' "$@"
  chmod +x "${stub_bin}/df"
}

# An unresolvable path never reaches df.
expect_error "does not exist" "missing cache dir" ci_disk_cache_fs_total_kb "${cache_dir}/absent"

# Linux df -P -k output for the incident host.
install_df \
  'echo "Filesystem     1024-blocks      Used Available Capacity Mounted on"' \
  'echo "/dev/sda1        512753664  42991616 469762048       9% /mnt/ssd"'
PATH="${stub_bin}:${original_path}"
expect_output "512753664" "linux df output" ci_disk_cache_fs_total_kb "$cache_dir"

# macOS df -P -k output.
install_df \
  'echo "Filesystem   1024-blocks       Used Available Capacity  Mounted on"' \
  'echo "/dev/disk4s1  3906250000 1722325780 2183924220    45%    /Volumes/ssd"'
expect_output "3906250000" "macos df output" ci_disk_cache_fs_total_kb "$cache_dir"

# A filesystem name containing a space must not shift the total.
install_df \
  'echo "Filesystem   1024-blocks       Used Available Capacity  Mounted on"' \
  'echo "map auto_home   1000000          0   1000000     0%    /System/Volumes/Data/home"'
expect_output "1000000" "spaced filesystem name" ci_disk_cache_fs_total_kb "$cache_dir"

# The stub must be asked about the cache directory itself, with POSIX flags.
install_df \
  "if [[ \"\$1\" == \"-P\" && \"\$2\" == \"-k\" && \"\$3\" == \"${cache_dir}\" ]]; then" \
  '  echo "Filesystem 1024-blocks Used Available Capacity Mounted on"' \
  '  echo "/dev/x 2097152 0 2097152 0% /x"' \
  'else' \
  '  echo "unexpected df arguments: $*" >&2' \
  '  exit 2' \
  'fi'
expect_output "2097152" "df invoked as df -P -k <dir>" ci_disk_cache_fs_total_kb "$cache_dir"

# df failing (e.g. the mount vanished between mkdir and df) is an error that
# carries df's own message.
install_df \
  "echo \"df: ${cache_dir}: No such file or directory\" >&2" \
  'exit 1'
expect_error "No such file or directory" "df failure" ci_disk_cache_fs_total_kb "$cache_dir"

# Output that has no numeric total is an error, not a zero cap.
install_df 'echo "Filesystem 1024-blocks Used Available Capacity Mounted on"'
expect_error "could not parse" "header-only df output" ci_disk_cache_fs_total_kb "$cache_dir"
install_df \
  'echo "Filesystem 1024-blocks Used Available Capacity Mounted on"' \
  'echo "/dev/x - - - - /x"'
expect_error "could not parse" "dashes for sizes" ci_disk_cache_fs_total_kb "$cache_dir"

# No df on PATH at all. The sandbox PATH already lacks df, but the case must
# not depend on that: build a PATH holding only the tools this test and the
# library need, with df deliberately absent.
no_df_bin="$(mktemp -d "${TEST_TMPDIR:?}/no-df-bin.XXXXXX")"
for tool in bash cat chmod grep ln mktemp rm sed; do
  ln -s "$(command -v "$tool")" "${no_df_bin}/${tool}"
done
PATH="$no_df_bin"
expect_error "df is not available" "df missing" ci_disk_cache_fs_total_kb "$cache_dir"
PATH="$original_path"

# ---------------------------------------------------------------------------
# End to end: the three pieces composed the way ci-env.sh composes them.
# ---------------------------------------------------------------------------

install_df \
  'echo "Filesystem 1024-blocks Used Available Capacity Mounted on"' \
  'echo "/dev/sda1 512753664 42991616 469762048 9% /mnt/ssd"'
PATH="${stub_bin}:${original_path}"
end_to_end() {
  local path total
  path="$(ci_disk_cache_path linux "${rc_dir}/good.bazelrc")" || return 1
  [[ "$path" == "/mnt/ssd/bazel/disk_cache" ]] || return 1
  total="$(ci_disk_cache_fs_total_kb "$cache_dir")" || return 1
  ci_disk_cache_gc_cap_from_total_kb "$total"
}
expect_output "391G" "end to end" end_to_end
PATH="$original_path"

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
