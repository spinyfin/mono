#!/bin/bash
#
# Opt-in integration check for lost-inputs-repair.sh against real Bazel. It is
# not a bazel test: it starts a real Bazel server, which the hermetic test
# sandbox denies. Run it from a checkout with `bash <this file>`; it needs
# bazel (or bazelisk), python3 and no network access, and it uses a temporary
# disk cache, output bases and loopback cache of its own, never the real ones.
#
# What it shows, with every cache enabled throughout:
#   1. A build leaves the producer's output blob in the disk cache and the
#      remote cache; the blob is then removed from the remote cache only.
#   2. Without the repair, a fresh build still reports success from the disk
#      cache's action-cache entry and the remote cache stays without the blob:
#      the state in which a remote executor, handed that blob as an input,
#      fails with "Lost inputs no longer available remotely".
#   3. After ci_repair_lost_inputs prunes the disk-cache entry that names the
#      blob, the producer is still not re-executed if the remote action cache
#      names the blob too: that cache answers instead (a remote cache hit).
#      The agent cannot prune it.
#   4. Once the remote action cache's entry is purged as well (here by the
#      script; in production only the cache's operator can), the repair makes
#      the next build re-execute the producer and upload the blob again.
# It also checks that a blob missing from both caches needs no repair: Bazel
# drops that disk-cache entry itself and re-executes the producer.
#
# It cannot produce the remote executor's own error; that needs a remote
# execution service, which a loopback cache is not.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lost-inputs-repair.sh
source "${here}/../lost-inputs-repair.sh"

tmp="$(mktemp -d)"
trap 'kill "${server_pid:-0}" 2> /dev/null || true; rm -rf "$tmp"' EXIT
ws="${tmp}/ws" data="${tmp}/remote" disk="${tmp}/disk"
mkdir -p "$ws" "$data"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

python3 -I "${here}/loopback_cache.py" "$data" "${tmp}/port" &
server_pid=$!
for _ in $(seq 50); do [[ -s "${tmp}/port" ]] && break || sleep 0.1; done
[[ -s "${tmp}/port" ]] || fail "loopback cache did not start"
cache_url="http://127.0.0.1:$(cat "${tmp}/port")"

printf '' > "${ws}/MODULE.bazel"
printf '%s\n' \
  'genrule(name = "producer", srcs = ["p.txt"], outs = ["p.out"], cmd = "echo PRODUCER-OUTPUT-MARKER > $@; cat $< >> $@")' \
  'genrule(name = "consumer", srcs = ["c.txt", ":producer"], outs = ["c.out"], cmd = "cat $(SRCS) > $@")' \
  > "${ws}/BUILD.bazel"
echo p > "${ws}/p.txt"
cp "${here}/../../../.bazelversion" "${ws}/.bazelversion"

round=0
# build <label>: a build in a fresh output base, so only the disk and remote
# caches can supply results. Prints bazel's output to $tmp/<label>.log.
build() {
  round=$((round + 1))
  echo "round ${round}" > "${ws}/c.txt"
  (cd "$ws" && bazel --output_base="${tmp}/ob${round}" build \
    --remote_cache="$cache_url" --disk_cache="$disk" --remote_download_minimal \
    //:consumer) > "${tmp}/$1.log" 2>&1 || {
    cat "${tmp}/$1.log" >&2
    fail "bazel build ($1) failed"
  }
}
remote_blob() { [[ -e "${data}/cas_$1" ]]; }
disk_ac_names() { grep -rlaF "$1" "${disk}/ac" > /dev/null 2>&1; }

build seed
producer_blob_file=""
for f in "$data"/cas_*; do
  [[ "$(head -c 22 "$f" | LC_ALL=C tr -d '\0')" == "PRODUCER-OUTPUT-MARKER" ]] && producer_blob_file="$f"
done
[[ -n "$producer_blob_file" ]] || fail "producer output was not uploaded"
blob="${producer_blob_file##*/cas_}"
disk_ac_names "$blob" || fail "disk-cache action cache does not name the producer output"

# Control: gone from both caches, Bazel re-executes the producer by itself.
mkdir -p "${tmp}/stash"
mv "$producer_blob_file" "${tmp}/stash/remote_blob"
mv "${disk}/cas/${blob:0:2}/${blob}" "${tmp}/stash/disk_blob"
build both_gone
remote_blob "$blob" || fail "control: producer was not re-executed when the blob was gone everywhere"
echo "ok: a blob missing from both caches is re-produced without any repair"

# The case the repair exists for: gone from the remote cache only.
rm "${data}/cas_${blob}"
[[ -e "${disk}/cas/${blob:0:2}/${blob}" ]] || fail "disk cache lost the blob; fixture is wrong"
build unrepaired
remote_blob "$blob" && fail "without the repair the producer unexpectedly re-ran"
echo "ok: without the repair the build passes on the disk-cache entry and the remote cache stays without the blob"

printf 'ERROR: x: Testing //:t failed: Lost inputs no longer available remotely: bazel-out/p.out (%s/%s)\n' \
  "$blob" "$(wc -c < "${disk}/cas/${blob:0:2}/${blob}" | tr -d ' ')" > "${tmp}/lost.log"
ci_repair_lost_inputs "${tmp}/lost.log" "$disk" || fail "repair removed nothing"
disk_ac_names "$blob" && fail "an action-cache entry still names the lost blob"
[[ -e "${disk}/cas/${blob:0:2}/${blob}" ]] || fail "repair must not touch cas/"
build repaired
grep -q 'remote cache hit' "${tmp}/repaired.log" || fail "expected the pruned build to be served by the remote action cache"
remote_blob "$blob" && fail "the blob came back although the remote action cache still names it"
echo "ok: pruning the disk cache alone is not enough while the remote action cache also names the blob (served as a remote cache hit; blob still absent)"

# The remote action cache is the layer responsible then. A loopback cache can
# be purged; the real remote cache can only be purged by its operator.
purged=0
for f in $(grep -laF "$blob" "$data"/ac_* 2> /dev/null || true); do rm -f "$f" && purged=$((purged + 1)); done
[[ "$purged" -gt 0 ]] || fail "no remote action-cache entry names the blob"
ci_repair_lost_inputs "${tmp}/lost.log" "$disk" || fail "second repair removed nothing"
build recovered
remote_blob "$blob" || fail "with both action caches pruned the producer did not re-execute and re-upload"
echo "ok: with the disk and remote action-cache entries pruned the producer re-executed and the remote cache has the blob again"
