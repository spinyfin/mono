#!/bin/bash
#
# Tests for .buildkite/steps/lost-inputs-repair.sh and the retry in ci-env.sh's
# bazel() wrapper. The digests are read from Bazel's "Lost inputs no longer
# available remotely" errors; the disk-cache action-cache entries that name a
# lost blob are removed (whether or not the blob is in cas/) and nothing else
# is; and the wrapper retries exactly once per repair that removed something,
# returning the original exit status otherwise.
#
# Fixtures are written with printf, not here-documents: bash 3.2 (the macOS
# /bin/bash) backs here-documents with files under /tmp regardless of TMPDIR,
# and the sandbox denies that.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
# shellcheck source=../lost-inputs-repair.sh
source "${workspace}/.buildkite/steps/lost-inputs-repair.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

dangling="cd2aebfcd477a329b4909518cc434e0e70a6b15b5664531b63b13eaea486000f"
dangling2="3f85b678b501fd6f489269cbf3e5565e84eaeabf8054825b547307d01bf33820"
present="12715e77e201b1dfbb9dd957b777080003e4f0a1f1152f0c5a1c69b70e6f5531"
other="9b979db9de538f5c0d278fcbaa71369b789fb68f5718f7392da4d8399c5d680a"
third="796100f5a8b1c8727ffc3c63f0cd06c782da100ed4c0cf54767b8b159bc4ecaf"

tmp="$(mktemp -d "${TEST_TMPDIR:?}/lir.XXXXXX")"
log="${tmp}/bazel.log"
printf '%s\n' \
  "INFO: Elapsed time: 2.9s" \
  "ERROR: /w/BUILD.bazel:481:16: Testing //a:Shard4 failed: Lost inputs no longer available remotely: a/Shard4.zip (${dangling}/72932900)" \
  "ERROR: /w/BUILD.bazel:481:16: Testing //a:Shard5 failed: Lost inputs no longer available remotely: a/Shard 5.zip (${dangling2}/71746964)" \
  "ERROR: /w/BUILD.bazel:404:16: Testing //a:Shard1 failed: Lost inputs no longer available remotely: a/Shard1.zip (${present}/72188884)" \
  "ERROR: /w/BUILD.bazel:404:16: Testing //a:Shard1 failed: Lost inputs no longer available remotely: a/Shard1.zip (${present}/72188884)" \
  "ERROR: /w/BUILD.bazel:9:1: Testing //a:Multi failed: Lost inputs no longer available remotely: a/m1.zip (${third}/10), a/m2.zip (${dangling}/72932900)" \
  "(${other}/5) is mentioned on a line that is not a lost-inputs error" \
  > "$log"

digests="$(ci_lost_input_digests "$log" | tr '\n' ' ')"
expected="$(printf '%s\n' "$dangling" "$dangling2" "$present" "$third" | sort | tr '\n' ' ')"
[[ "$digests" == "$expected" ]] || fail "digests: expected '${expected}', got '${digests}'"

[[ -z "$(ci_lost_input_digests "${tmp}/missing.log")" ]] || fail "a missing log must yield no digests"

# A disk cache with: entries naming a lost blob that cas/ lacks (two naming one
# blob), an entry naming a lost blob that cas/ still holds, an entry naming
# two lost blobs, and an unrelated entry.
cache="${tmp}/disk_cache"
mkdir -p "${cache}/ac/aa" "${cache}/ac/bb" "${cache}/ac/cc" "${cache}/ac/dd" "${cache}/ac/ee" "${cache}/ac/ff" "${cache}/cas/${present:0:2}"
printf 'out/Shard4.zip\n%s\n' "$dangling" > "${cache}/ac/aa/entry-dangling"
printf 'out/Shard4b.zip\n%s\n' "$dangling" > "${cache}/ac/bb/entry-dangling-sibling"
printf 'out/Shard5.zip\n%s\n' "$dangling2" > "${cache}/ac/cc/entry-dangling2"
printf 'out/Shard1.zip\n%s\n' "$present" > "${cache}/ac/dd/entry-present"
printf 'out/Nav.zip\n%s\n' "$other" > "${cache}/ac/ee/entry-other"
printf 'out/Multi.zip\n%s\n%s\n' "$third" "$dangling2" > "${cache}/ac/ff/entry-two"
printf 'blob' > "${cache}/cas/${present:0:2}/${present}"

repair_log="${tmp}/repair.stderr"
ci_repair_lost_inputs "$log" "$cache" 2> "$repair_log" || fail "repair must report a change when it removed entries"

for gone in aa/entry-dangling bb/entry-dangling-sibling cc/entry-dangling2 dd/entry-present ff/entry-two; do
  [[ ! -e "${cache}/ac/${gone}" ]] || fail "entry ${gone} names a lost blob and must be removed"
done
[[ -e "${cache}/ac/ee/entry-other" ]] || fail "entry naming no lost blob must be kept"
[[ -e "${cache}/cas/${present:0:2}/${present}" ]] || fail "cas blobs must never be removed"

# Every lost digest is logged with whether cas/ has it and how many entries named it.
grep -qF "lost blob ${dangling}: in disk-cache cas=no, action-cache entries that named it=2" "$repair_log" \
  || fail "diagnostic for ${dangling} missing: $(cat "$repair_log")"
grep -qF "lost blob ${present}: in disk-cache cas=yes, action-cache entries that named it=1" "$repair_log" \
  || fail "diagnostic for ${present} missing"
grep -qF "lost blob ${dangling2}: in disk-cache cas=no, action-cache entries that named it=2" "$repair_log" \
  || fail "diagnostic for ${dangling2} missing"

# Nothing left to remove: a second repair changes nothing, says so, and the
# caller must not retry the same failure again.
if ci_repair_lost_inputs "$log" "$cache" 2> "$repair_log"; then
  fail "a repair that removed nothing must return non-zero"
fi
grep -qF "no disk-cache entry names a lost blob" "$repair_log" || fail "a no-op repair must say why"

# A log with no lost inputs, and a cache with no ac/ directory, are no-ops.
printf 'ERROR: some other failure\n' > "${tmp}/other.log"
if ci_repair_lost_inputs "${tmp}/other.log" "$cache" 2> /dev/null; then
  fail "a log without lost inputs must return non-zero"
fi
if ci_repair_lost_inputs "$log" "${tmp}/no-such-cache" 2> /dev/null; then
  fail "a cache without ac/ must return non-zero"
fi

# --- The bazel() wrapper's retry, with a fake bazel that models the failure ---
#
# The fake reuses a cached result while any disk-cache ac/ entry names $BLOB
# and the "remote cache" (a marker file) lacks the blob: it then prints the
# lost-inputs error and exits $FAKE_EXIT. With no such entry it "re-executes
# the producer": it records that and puts the blob in the remote cache. With
# FAKE_ALWAYS_FAIL set it fails the same way regardless of the cache, which
# models a dangling reference the disk cache does not hold.
eval "$(sed -n '/^bazel() {/,/^}/p' "${workspace}/.buildkite/steps/ci-env.sh")"
[[ "$(type -t bazel)" == "function" ]] || fail "could not extract bazel() from ci-env.sh"

fakebin="${tmp}/bin"
mkdir -p "$fakebin"
fake="${fakebin}/bazel"
printf '%s\n' \
  '#!/bin/bash' \
  'echo "$*" >> "${FAKE_STATE}/calls"' \
  'if [[ -n "${FAKE_ALWAYS_FAIL:-}" ]] || { [[ ! -e "${FAKE_STATE}/remote_blob" ]] && grep -rlaF "$BLOB" "${FAKE_CACHE}/ac" > /dev/null 2>&1; }; then' \
  '  echo "ERROR: /w/BUILD.bazel:1:1: Testing //a:b failed: Lost inputs no longer available remotely: a/b.zip (${BLOB}/100)"' \
  '  exit "${FAKE_EXIT:-1}"' \
  'fi' \
  'echo reexecuted >> "${FAKE_STATE}/reexec"' \
  ': > "${FAKE_STATE}/remote_blob"' \
  > "$fake"
chmod +x "$fake"
export TMPDIR="$tmp" PATH="${fakebin}:${PATH}"
export BAZEL_STARTUP_FLAGS="--max_idle_secs=1" CI_BAZEL_FLAGS="--config=ci-test"

# run_scenario <name> <cas has blob: 0|1> <entry names blob: 0|1> <always fail: 0|1> <exit code>
# Sets scenario_rc, calls (bazel invocations) and reexec (producer re-executions).
run_scenario() {
  local name="$1" in_cas="$2" has_entry="$3" always="$4" code="$5"
  export FAKE_STATE="${tmp}/${name}.state" FAKE_CACHE="${tmp}/${name}.cache" FAKE_EXIT="$code" BLOB="$dangling"
  mkdir -p "$FAKE_STATE" "${FAKE_CACHE}/ac/aa" "${FAKE_CACHE}/ac/bb" "${FAKE_CACHE}/cas/${dangling:0:2}"
  [[ "$has_entry" == 1 ]] && printf 'out/b.zip\n%s\n' "$dangling" > "${FAKE_CACHE}/ac/aa/names-lost-blob"
  printf 'out/unrelated\n%s\n' "$other" > "${FAKE_CACHE}/ac/bb/unrelated"
  [[ "$in_cas" == 1 ]] && printf 'blob' > "${FAKE_CACHE}/cas/${dangling:0:2}/${dangling}"
  if [[ "$always" == 1 ]]; then export FAKE_ALWAYS_FAIL=1; else unset FAKE_ALWAYS_FAIL; fi
  CI_DISK_CACHE_DIR="$FAKE_CACHE"
  scenario_rc=0
  bazel test //a:b > "${FAKE_STATE}/out" 2>&1 || scenario_rc=$?
  scenario_out="$(cat "${FAKE_STATE}/out" 2> /dev/null || true)"
  calls="$(wc -l < "${FAKE_STATE}/calls" | tr -d ' ')"
  reexec="$( (cat "${FAKE_STATE}/reexec" 2> /dev/null || true) | wc -l | tr -d ' ')"
}

# Blob missing from cas/ (dangling): one retry, the producer re-executes, the
# unrelated entry survives.
run_scenario dangling 0 1 0 1
[[ "$scenario_rc" == 0 && "$calls" == 2 && "$reexec" == 1 ]] || fail "dangling: want rc=0 calls=2 reexec=1, got rc=${scenario_rc} calls=${calls} reexec=${reexec}"
[[ -e "${FAKE_CACHE}/ac/bb/unrelated" ]] || fail "dangling: unrelated entry must survive the repair"

# Blob still in cas/ but absent remotely: the same recovery, and the cas blob stays.
run_scenario incas 1 1 0 1
[[ "$scenario_rc" == 0 && "$calls" == 2 && "$reexec" == 1 ]] || fail "in-cas: want rc=0 calls=2 reexec=1, got rc=${scenario_rc} calls=${calls} reexec=${reexec}"
[[ -e "${FAKE_CACHE}/cas/${dangling:0:2}/${dangling}" ]] || fail "in-cas: the cas blob must be retained"
[[ -e "${FAKE_CACHE}/ac/bb/unrelated" ]] || fail "in-cas: unrelated entry must survive the repair"

# Nothing in this disk cache names the blob (the reference is remote): no
# retry, and the original exit status comes back.
run_scenario remote_only 0 0 1 7
[[ "$scenario_rc" == 7 && "$calls" == 1 && "$reexec" == 0 ]] || fail "remote-only: want rc=7 calls=1 reexec=0, got rc=${scenario_rc} calls=${calls} reexec=${reexec}"

# A repair that removed an entry but did not cure the failure: exactly one
# retry (the second failure removes nothing), then the original status.
run_scenario still_failing 0 1 1 7
[[ "$scenario_rc" == 7 && "$calls" == 2 ]] || fail "still-failing: want rc=7 calls=2, got rc=${scenario_rc} calls=${calls}"

# A failure that is not lost inputs is not retried and keeps its status.
fakefail="${fakebin}/bazel"
printf '%s\n' '#!/bin/bash' 'echo "$*" >> "${FAKE_STATE}/calls"' 'echo "ERROR: compile failed"' 'exit 3' > "$fakefail"
rm -rf "${tmp}/plain.state"
run_scenario plain 0 0 0 3
[[ "$scenario_rc" == 3 && "$calls" == 1 ]] || fail "plain failure: want rc=3 calls=1, got rc=${scenario_rc} calls=${calls}: ${scenario_out}"

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
