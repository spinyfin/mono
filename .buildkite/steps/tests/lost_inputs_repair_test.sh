#!/bin/bash
#
# Tests for .buildkite/steps/lost-inputs-repair.sh: the digests are read from
# Bazel's "Lost inputs no longer available remotely" errors, and only the
# disk-cache action-cache entries that name a lost blob the cache cannot supply
# are removed.
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
present="12715e77e201b1dfbb9dd957b777080003e4f0a1f1152f0c5a1c69b7bb4b6d11"
other="9b979db9de538f5c0d278fcbaa71369b789fb68f5718f7392da4d8399c5d680a"

tmp="$(mktemp -d "${TEST_TMPDIR:?}/lir.XXXXXX")"
log="${tmp}/bazel.log"
printf '%s\n' \
  "INFO: Elapsed time: 2.9s" \
  "ERROR: /w/BUILD.bazel:481:16: Testing //a:Shard4 failed: Lost inputs no longer available remotely: a/Shard4.zip (${dangling}/72932900)" \
  "ERROR: /w/BUILD.bazel:481:16: Testing //a:Shard5 failed: Lost inputs no longer available remotely: a/Shard5.zip (${dangling2}/71746964)" \
  "ERROR: /w/BUILD.bazel:404:16: Testing //a:Shard1 failed: Lost inputs no longer available remotely: a/Shard1.zip (${present}/72188884)" \
  "ERROR: /w/BUILD.bazel:404:16: Testing //a:Shard1 failed: Lost inputs no longer available remotely: a/Shard1.zip (${present}/72188884)" \
  > "$log"

digests="$(ci_lost_input_digests "$log" | tr '\n' ' ')"
expected="$(printf '%s\n' "$dangling" "$dangling2" "$present" | sort | tr '\n' ' ')"
[[ "$digests" == "$expected" ]] || fail "digests: expected '${expected}', got '${digests}'"

[[ -z "$(ci_lost_input_digests "${tmp}/missing.log")" ]] || fail "a missing log must yield no digests"

# A disk cache with: two dangling entries (one naming the same blob as a
# sibling), an entry whose blob the cache still holds, and an unrelated entry.
cache="${tmp}/disk_cache"
mkdir -p "${cache}/ac/aa" "${cache}/ac/bb" "${cache}/ac/cc" "${cache}/ac/dd" "${cache}/ac/ee" "${cache}/cas/${present:0:2}"
printf 'out/Shard4.zip\n%s\n' "$dangling" > "${cache}/ac/aa/entry-dangling"
printf 'out/Shard4b.zip\n%s\n' "$dangling" > "${cache}/ac/bb/entry-dangling-sibling"
printf 'out/Shard5.zip\n%s\n' "$dangling2" > "${cache}/ac/cc/entry-dangling2"
printf 'out/Shard1.zip\n%s\n' "$present" > "${cache}/ac/dd/entry-present"
printf 'out/Nav.zip\n%s\n' "$other" > "${cache}/ac/ee/entry-other"
printf 'blob' > "${cache}/cas/${present:0:2}/${present}"

ci_repair_lost_inputs "$log" "$cache" 2> /dev/null || fail "repair must report a change when it removed entries"

for gone in aa/entry-dangling bb/entry-dangling-sibling cc/entry-dangling2; do
  [[ ! -e "${cache}/ac/${gone}" ]] || fail "dangling entry ${gone} must be removed"
done
for kept in dd/entry-present ee/entry-other; do
  [[ -e "${cache}/ac/${kept}" ]] || fail "entry ${kept} must be kept"
done
[[ -e "${cache}/cas/${present:0:2}/${present}" ]] || fail "cas blobs must never be removed"

# Nothing left to remove: a second repair changes nothing, so the caller must
# not retry the same failure again.
if ci_repair_lost_inputs "$log" "$cache" 2> /dev/null; then
  fail "a repair that removed nothing must return non-zero"
fi

# A log with no lost inputs, and a cache with no ac/ directory, are no-ops.
printf 'ERROR: some other failure\n' > "${tmp}/other.log"
if ci_repair_lost_inputs "${tmp}/other.log" "$cache" 2> /dev/null; then
  fail "a log without lost inputs must return non-zero"
fi
if ci_repair_lost_inputs "$log" "${tmp}/no-such-cache" 2> /dev/null; then
  fail "a cache without ac/ must return non-zero"
fi

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
