#!/bin/bash
#
# Script-level test for compact execution-log wiring:
# - each build/test invocation gets a distinct sequence-numbered .bin.zst path
#   in a per-job directory under bazel-execlogs/,
# - an Xcode-mismatch retry allocates a second filename rather than overwriting,
# - a subprocess `bazel` on PATH (repobin / checkleft) gets its own file,
# - a second setup in the same job keeps existing logs and the sequence,
# - non-build subcommands (e.g. cquery) do not receive the flag,
# - callers passing --execution_log_json_file / --execution_log_binary_file /
#   --execution_log_compact_file are not given a conflicting flag,
# - .bazelignore lists bazel-execlogs,
# - every pipeline step whose script sources ci-env.sh (and therefore runs
#   Bazel) uploads bazel-execlogs/**/*.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
env_sh="${workspace}/.buildkite/steps/ci-env.sh"
execlog_sh="${workspace}/.buildkite/steps/ci-execlog.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

export TMPDIR="${TEST_TMPDIR:-${TMPDIR:-/tmp}}"
tmpdir="$(mktemp -d "${TMPDIR}/execlog-test.XXXXXX")"
export TMPDIR="$tmpdir"
trap 'rm -rf "$tmpdir"' EXIT

# shellcheck source=../ci-execlog.sh
source "$execlog_sh"

# Real bazel() from ci-env.sh, with _ci_run_bazel stubbed to record argv.
eval "$(
  awk '
    /^bazel\(\) \{/ { in_fn=1 }
    in_fn { print }
    in_fn && /^}$/ { exit }
  ' "$env_sh"
)"

recorded="${tmpdir}/recorded.txt"
stub_log="${tmpdir}/stub-bazel.log"
printf '%s\n' '#!/bin/bash' "printf '%s\\n' \"\$*\" >> \"${stub_log}\"" 'exit 0' > "${tmpdir}/real-bazel"
chmod +x "${tmpdir}/real-bazel"

run_wrapper_suite() {
  cd "$tmpdir"
  rm -rf bazel-execlogs "$recorded" "$stub_log"
  : > "$recorded"
  : > "$stub_log"
  unset CI_EXECLOG_SETUP_DONE CI_EXECLOG_DIR CI_EXECLOG_ROOT CI_EXECLOG_LIB
  export BUILDKITE_JOB_ID="job-42"
  export CI_REAL_BAZEL="${tmpdir}/real-bazel"
  export BAZEL_STARTUP_FLAGS="--max_idle_secs=1"
  export CI_BAZEL_FLAGS=""
  export CI_REMOTE_EXECUTION="off"

  mkdir -p bazel-execlogs/old-job
  printf 'stale\n' > bazel-execlogs/old-job/x.bin.zst

  _ci_execlog_setup "$execlog_sh"

  [[ ! -e bazel-execlogs/old-job ]] || fail "setup left a sibling job directory in place"
  [[ -d bazel-execlogs/job-42 ]] || fail "setup did not create this job's directory"
  wrap_bazel="$(type -P bazel || true)"
  [[ "$wrap_bazel" == *"/bazel-execlogs/job-42/.wrap/bazel" ]] \
    || fail "PATH bazel is ${wrap_bazel}, not the per-job wrap"

  _ci_run_bazel() {
    local _tmplog="$1"
    shift
    printf '%s\n' "$*" >> "$recorded"
    if [[ "${FORCE_XCODE_FAIL:-}" == "1" ]]; then
      printf 'Xcode version 16.0 is not available on the host machine\n' > "$_tmplog"
      FORCE_XCODE_FAIL=0
      return 1
    fi
    return 0
  }

  bazel build //foo:bar
  bazel test //foo:bar_test
  bazel cquery //foo:bar
  bazel build --execution_log_json_file=/tmp/custom.json //foo:bar

  FORCE_XCODE_FAIL=1
  bazel build //foo:retry

  # Subprocess launch: exec the PATH wrap the way Command::new("bazel") does
  # after locate() finds it. Invoke the wrap by absolute path so a hashed
  # `bazel` on PATH cannot start a real build inside the test sandbox.
  wrap_bin="${CI_EXECLOG_DIR}/.wrap/bazel"
  "$wrap_bin" --max_idle_secs=1 --config=ci-darwin build //tools/repobin:repobin
  "$wrap_bin" cquery //tools/repobin:repobin
  "$wrap_bin" --max_idle_secs=1 test --execution_log_json_file=/tmp/custom.json //foo:t

  # Second source in the same job must keep logs and not restart at 01.
  printf 'keep\n' > bazel-execlogs/job-42/01-build.bin.zst
  _ci_execlog_setup "$execlog_sh"
  [[ -f bazel-execlogs/job-42/01-build.bin.zst ]] || fail "re-setup deleted this job's logs"
  grep -qxF 'keep' bazel-execlogs/job-42/01-build.bin.zst || fail "re-setup truncated this job's logs"

  printf 'hello' > bazel-execlogs/job-42/02-test.bin.zst
  summary="$(_ci_execlog_print_summary)"
  [[ "$summary" == *"job=job-42 files=2 total_bytes=10"* ]] \
    || fail "summary missing expected job/files/total_bytes: ${summary}"
  [[ "$summary" == *"01-build.bin.zst: 5 bytes"* ]] \
    || fail "summary missing 01-build.bin.zst bytes: ${summary}"
  [[ "$summary" == *"02-test.bin.zst: 5 bytes"* ]] \
    || fail "summary missing 02-test.bin.zst bytes: ${summary}"
}

run_wrapper_suite

lines=()
while IFS= read -r line; do
  lines+=("$line")
done < "$recorded"

# 4 ordinary wrapper calls + 2 from the retry (failing attempt + retry).
if [[ "${#lines[@]}" != "6" ]]; then
  fail "expected 6 recorded bazel() calls, got ${#lines[@]}"
else
  [[ "${lines[0]}" == *"--execution_log_compact_file="*"/bazel-execlogs/job-42/01-build.bin.zst"* ]] \
    || fail "first call missing 01-build.bin.zst: ${lines[0]}"
  [[ "${lines[1]}" == *"--execution_log_compact_file="*"/bazel-execlogs/job-42/02-test.bin.zst"* ]] \
    || fail "second call missing 02-test.bin.zst: ${lines[1]}"
  if [[ "${lines[2]}" == *"--execution_log_compact_file"* ]]; then
    fail "cquery must not receive --execution_log_compact_file: ${lines[2]}"
  fi
  if [[ "${lines[3]}" == *"--execution_log_compact_file"* ]]; then
    fail "call with --execution_log_json_file must not also receive --execution_log_compact_file: ${lines[3]}"
  fi
  [[ "${lines[4]}" == *"--execution_log_compact_file="*"/bazel-execlogs/job-42/03-build.bin.zst"* ]] \
    || fail "Xcode-failing attempt missing 03-build.bin.zst: ${lines[4]}"
  [[ "${lines[5]}" == *"--execution_log_compact_file="*"/bazel-execlogs/job-42/04-build.bin.zst"* ]] \
    || fail "Xcode retry missing 04-build.bin.zst: ${lines[5]}"
  if [[ "${lines[4]}" == "${lines[5]}" ]]; then
    fail "Xcode retry reused the first attempt's execution log path"
  fi
fi

stub_lines=()
while IFS= read -r line; do
  [[ -n "$line" ]] && stub_lines+=("$line")
done < "$stub_log"

# clean --expunge from the retry, then three PATH-wrapper launches.
if [[ "${#stub_lines[@]}" != "4" ]]; then
  fail "expected 4 real-bazel subprocess calls (clean + 3 PATH launches), got ${#stub_lines[@]}: $(paste -sd '; ' "$stub_log")"
else
  [[ "${stub_lines[0]}" == *"clean --expunge"* ]] \
    || fail "retry should run clean --expunge: ${stub_lines[0]}"
  [[ "${stub_lines[1]}" == *"--execution_log_compact_file="*"/bazel-execlogs/job-42/05-build.bin.zst"* ]] \
    || fail "PATH-wrapper build missing 05-build.bin.zst: ${stub_lines[1]}"
  [[ "${stub_lines[1]}" == *"--max_idle_secs=1"* ]] \
    || fail "PATH-wrapper dropped startup flags: ${stub_lines[1]}"
  [[ "${stub_lines[1]}" == *"--config=ci-darwin"* ]] \
    || fail "PATH-wrapper dropped CI flags: ${stub_lines[1]}"
  if [[ "${stub_lines[2]}" == *"--execution_log_compact_file"* ]]; then
    fail "PATH-wrapper cquery must not receive --execution_log_compact_file: ${stub_lines[2]}"
  fi
  if [[ "${stub_lines[3]}" == *"--execution_log_compact_file"* ]]; then
    fail "PATH-wrapper call with --execution_log_json_file must not also receive compact file: ${stub_lines[3]}"
  fi
fi

grep -qxF 'bazel-execlogs' "${workspace}/.bazelignore" \
  || fail ".bazelignore does not list bazel-execlogs"

# Every step whose command runs a script that sources ci-env.sh must upload
# bazel-execlogs. Scan pipeline YAML and the integrity commit-delta uploader.
sourcing=""
for f in "${workspace}/.buildkite/steps"/*.sh; do
  [[ -f "$f" ]] || continue
  if grep -qE 'source[[:space:]]+".*ci-env\.sh"' "$f"; then
    sourcing="${sourcing} $(basename "$f")"
  fi
done
[[ -n "${sourcing# }" ]] || fail "found no step scripts that source ci-env.sh"

scan_file() {
  local file="$1"
  awk -v srcs="${sourcing# }" -v file="$file" '
    BEGIN {
      n = split(srcs, a, " ")
      for (i = 1; i <= n; i++) if (a[i] != "") is_sourcing[a[i]] = 1
      step = ""
    }
    /^  - / {
      if (step != "") check()
      step = $0 "\n"
      next
    }
    { step = step $0 "\n" }
    END { if (step != "") check() }
    function check(    i, nlines, line, script, has_log) {
      if (step !~ /command:/) return
      nlines = split(step, lines, "\n")
      script = ""
      for (i = 1; i <= nlines; i++) {
        line = lines[i]
        if (match(line, /\.buildkite\/steps\/[A-Za-z0-9._-]+\.sh/)) {
          script = substr(line, RSTART, RLENGTH)
          sub(".*/", "", script)
          break
        }
      }
      if (script == "" || !is_sourcing[script]) return
      has_log = (step ~ /bazel-execlogs\/\*\*\/\*/)
      if (!has_log) {
        printf "FAIL: %s step running %s has no bazel-execlogs artifact_paths\n", file, script > "/dev/stderr"
        err = 1
      }
    }
    END { exit err+0 }
  ' "$file"
}

shopt -s nullglob
for yml in "${workspace}/.buildkite"/pipeline*.yml; do
  scan_file "$yml" || fail "artifact_paths missing in $(basename "$yml")"
done
scan_file "${workspace}/.buildkite/steps/integrity-commit-delta.sh" \
  || fail "artifact_paths missing in integrity-commit-delta.sh"

# Also extract the EXIT-trap composition block from ci-env.sh.
trap_block_file="${tmpdir}/ci-exit-trap.sh"
awk '
  /^_CI_STEP_EXIT_TRAP=/ { in_blk=1 }
  in_blk { print }
  in_blk && /^trap \x27_ci_on_exit\x27 EXIT$/ { exit }
' "$env_sh" > "$trap_block_file"

test_exit_trap_composition() {
  local marker="${tmpdir}/trap-marker"
  local out_file="${tmpdir}/trap-out"
  local rc

  # 1. Pre-existing EXIT trap (verify-boss-tag-cache.sh pattern) runs on
  # failure, preserves original exit status, and still prints execlog summary.
  rm -f "$marker" "$out_file"
  mkdir -p "${tmpdir}/trap-logs-1"
  printf 'abc' > "${tmpdir}/trap-logs-1/01-build.bin.zst"
  set +e
  (
    set -euo pipefail
    export CI_EXECLOG_DIR="${tmpdir}/trap-logs-1"
    export BUILDKITE_JOB_ID="job-trap-1"
    _ci_remote_cleanup_step_watchdog() { :; }
    trap "printf 'pre:%s\n' \"\$?\" > '${marker}'" EXIT
    set -- "arg1" "arg2"
    source "$trap_block_file"
    [[ "$1" == "arg1" && "$2" == "arg2" ]] || exit 99
    exit 42
  ) > "$out_file"
  rc=$?
  set -e
  [[ "$rc" == "42" ]] || fail "pre-existing EXIT trap changed exit code: got ${rc}, expected 42"
  grep -qxF 'pre:42' "$marker" || fail "pre-existing EXIT trap did not run with rc=42: $(cat "$marker" 2>/dev/null || true)"
  grep -qF 'job=job-trap-1 files=1 total_bytes=3' "$out_file" || fail "execlog summary missing with pre-existing EXIT trap: $(cat "$out_file")"

  # 2. Clearing pre-existing trap via ci_clear_exit_trap suppresses failure
  # handler while keeping _ci_on_exit active.
  rm -f "$marker" "$out_file"
  (
    set -euo pipefail
    export CI_EXECLOG_DIR="${tmpdir}/trap-logs-1"
    export BUILDKITE_JOB_ID="job-trap-2"
    _ci_remote_cleanup_step_watchdog() { :; }
    trap "printf 'pre-should-not-run\n' > '${marker}'" EXIT
    source "$trap_block_file"
    ci_clear_exit_trap
  ) > "$out_file"
  [[ ! -e "$marker" ]] || fail "ci_clear_exit_trap did not clear pre-existing trap"
  grep -qF 'job=job-trap-2 files=1 total_bytes=3' "$out_file" || fail "execlog summary missing after ci_clear_exit_trap: $(cat "$out_file")"

  # 3. Release step installs cleanup via ci_set_exit_trap; on failure, both
  # cleanup and execlog summary run and original exit status is preserved.
  rm -f "$marker" "$out_file"
  set +e
  (
    set -euo pipefail
    export CI_EXECLOG_DIR="${tmpdir}/trap-logs-1"
    export BUILDKITE_JOB_ID="job-trap-3"
    _ci_remote_cleanup_step_watchdog() { :; }
    source "$trap_block_file"
    cleanup() { printf 'cleanup:%s\n' "$?" > "${marker}"; }
    ci_set_exit_trap cleanup
    exit 17
  ) > "$out_file"
  rc=$?
  set -e
  [[ "$rc" == "17" ]] || fail "ci_set_exit_trap changed exit code: got ${rc}, expected 17"
  grep -qxF 'cleanup:17' "$marker" || fail "ci_set_exit_trap handler did not run with rc=17: $(cat "$marker" 2>/dev/null || true)"
  grep -qF 'job=job-trap-3 files=1 total_bytes=3' "$out_file" || fail "execlog summary missing with ci_set_exit_trap: $(cat "$out_file")"

  # 4. Release step removes its cleanup trap via ci_clear_exit_trap after
  # manual cleanup; execlog summary still prints on exit.
  rm -f "$marker" "$out_file"
  (
    set -euo pipefail
    export CI_EXECLOG_DIR="${tmpdir}/trap-logs-1"
    export BUILDKITE_JOB_ID="job-trap-4"
    _ci_remote_cleanup_step_watchdog() { :; }
    source "$trap_block_file"
    cleanup() { printf 'cleanup-should-not-run\n' > "${marker}"; }
    ci_set_exit_trap cleanup
    ci_clear_exit_trap
  ) > "$out_file"
  [[ ! -e "$marker" ]] || fail "ci_clear_exit_trap did not clear release cleanup trap"
  grep -qF 'job=job-trap-4 files=1 total_bytes=3' "$out_file" || fail "execlog summary missing after release ci_clear_exit_trap: $(cat "$out_file")"
}

test_exit_trap_composition

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
