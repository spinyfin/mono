#!/bin/bash
#
# Pins what the remote-execution files promise each other.
#
# None of these files is exercised by a build: they are installed on the hub
# and the workers by hand. A value changed in one of them and not in the file
# that depends on it still looks fine in review and fails on the host, or
# worse, does not fail: a scheduler ceiling below what a worker may spend on
# one action ends work that would have finished.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
dir="${workspace}/tools/remote-execution"
hub="${dir}/nativelink/hub.json5"
worker="${dir}/nativelink/worker-macos.json5"
plist="${dir}/nativelink/com.brianduff.nativelink.plist"
hub_unit="${dir}/nativelink/nativelink-macos-hub.service"
health_unit="${dir}/nativelink/nativelink-macos-hub-health.service"
health_timer="${dir}/nativelink/nativelink-macos-hub-health.timer"
health="${dir}/nativelink/hub-health.sh"
textfile="${dir}/nativelink/hub-textfile-metrics.sh"
textfile_unit="${dir}/nativelink/nativelink-hub-textfile.service"
textfile_timer="${dir}/nativelink/nativelink-hub-textfile.timer"
slots_exporter="${dir}/nativelink/worker-slots-exporter.py"
slots_unit="${dir}/nativelink/nativelink-worker-slots-exporter.service"
build="${dir}/nativelink/build-worker.sh"
patches_dir="${dir}/nativelink/patches"
readme="${dir}/README.md"
dev_watchdog="${dir}/dev-remote-workspace-status.sh"
remote_status="${dir}/remote-status.sh"
grafana_overview="${dir}/grafana/dashboards/nativelink-overview.json"
grafana_alerts="${dir}/grafana/alerting/nativelink-hub.yaml"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# Prints the lines of a JSON5 file that are not comments, without indentation.
settings() {
  grep -v -E '^[[:space:]]*//' "$1" | sed -E 's/^[[:space:]]+//'
}

# setting <file> <key>: the number a `key: <number>,` line of the file sets.
# Fails the test if the file does not set it exactly once.
setting() {
  local values
  values="$(settings "$1" | sed -n -E "s/^$2: ([0-9]+),\$/\1/p")"
  if [[ ! "$values" =~ ^[0-9]+$ ]]; then
    fail "$(basename "$1") must set $2 exactly once, to a number (found: '${values}')"
    echo 0
    return
  fi
  echo "$values"
}

# ---------------------------------------------------------------------------
# The scheduler's timeouts: all written out, and each the value the runbook's
# table gives a reason for.
# ---------------------------------------------------------------------------

expect_setting() {
  local actual
  actual="$(setting "$1" "$2")"
  if [[ "$actual" != "$3" ]]; then
    fail "$(basename "$1"): $2 is ${actual}, expected $3. If the change is meant, change the reason next to it, README.md's table and this test together."
  fi
  # The runbook's table has a row for it with the same value.
  if ! grep -q -E "^\| \`$2\` +\| $3 " "$readme"; then
    fail "README.md's \"Timeouts\" table has no row giving $2 as $3"
  fi
}

expect_setting "$hub" worker_timeout_s 30
expect_setting "$hub" max_action_executing_timeout_s 2700
expect_setting "$hub" executing_sweep_interval_s 30
expect_setting "$hub" unacknowledged_kill_timeout_s 60
expect_setting "$hub" dispatch_ack_timeout_s 20
expect_setting "$hub" client_action_timeout_s 60
expect_setting "$hub" max_job_retries 3
expect_setting "$hub" max_worker_loss_retries 6
expect_setting "$hub" max_timeout_retries 1
expect_setting "$hub" no_worker_action_timeout_s 120
expect_setting "$hub" unsatisfiable_action_timeout_s 300
expect_setting "$hub" execute_stream_heartbeat_s 30
expect_setting "$worker" max_download_timeout_s 600
expect_setting "$worker" max_action_timeout_s 1200
expect_setting "$worker" max_upload_timeout_s 600
expect_setting "$worker" http2_keepalive_interval_s 10
expect_setting "$worker" http2_keepalive_timeout_s 20

# The scheduler's ceiling on an executing action is above everything a healthy
# worker may spend on one, so that the worker's own error is the one reported.
# Below the sum it ends work that would have finished.
worker_budget=$(($(setting "$worker" max_download_timeout_s) + $(setting "$worker" max_action_timeout_s) + $(setting "$worker" max_upload_timeout_s)))
ceiling="$(setting "$hub" max_action_executing_timeout_s)"
if ((ceiling <= worker_budget)); then
  fail "hub.json5's max_action_executing_timeout_s (${ceiling}) must exceed the worker's fetch + command + upload limits (${worker_budget})"
fi

# A worker notices a connection that has stopped answering no later than the
# scheduler evicts a worker it has not heard from. Otherwise it goes on running
# actions for a scheduler that has already given them to another worker.
notice=$(($(setting "$worker" http2_keepalive_interval_s) + $(setting "$worker" http2_keepalive_timeout_s)))
evict="$(setting "$hub" worker_timeout_s)"
if ((notice > evict)); then
  fail "the worker notices a dead connection after ${notice}s, later than the hub's worker_timeout_s (${evict}s)"
fi

# The scheduler waits for several keepalives before it calls a worker silent.
# The worker sends one every half of worker_api_endpoint.timeout.
keepalive_timeout="$(settings "$worker" | sed -n -E 's/^timeout: ([0-9]+),$/\1/p')"
if [[ ! "$keepalive_timeout" =~ ^[0-9]+$ ]]; then
  fail "worker-macos.json5 must set worker_api_endpoint.timeout to a whole number"
elif ((evict < 4 * keepalive_timeout)); then
  fail "worker_timeout_s (${evict}s) is under eight keepalive intervals (timeout ${keepalive_timeout}s, a keepalive every half of it)"
fi

# ---------------------------------------------------------------------------
# Every variable a config expands is set by the unit that starts it.
# ---------------------------------------------------------------------------

variables() {
  settings "$1" | grep -o -E '\$\{[A-Z_]+\}' | tr -d '${}' | sort -u
}

for name in $(variables "$worker"); do
  if ! grep -q -F "<key>${name}</key>" "$plist"; then
    fail "worker-macos.json5 expands \${${name}}, which the launchd plist does not set"
  fi
  if ! grep -q -E "^//   ${name} " "$worker"; then
    fail "worker-macos.json5 expands \${${name}} without describing it in its header"
  fi
done
for name in $(variables "$hub"); do
  # The hub's come from hub.env, which is host configuration and not in the
  # repository, so the header comment is the only place they are listed.
  if ! grep -q -E "^//   ${name} " "$hub"; then
    fail "hub.json5 expands \${${name}} without describing it in its header"
  fi
  if ! grep -q -F "${name}=" "$readme"; then
    fail "README.md's hub.env example does not set ${name}"
  fi
done
grep -q -x 'EnvironmentFile=/home/bduff/nativelink-macos-hub/hub.env' "$hub_unit" ||
  fail "the hub's unit must read hub.env"
grep -q -x 'EnvironmentFile=%h/nativelink-macos-hub/hub.env' "$health_unit" ||
  fail "the health check's unit must read hub.env, where the admin address is"

# ---------------------------------------------------------------------------
# The health check reads what the hub is configured to provide.
# ---------------------------------------------------------------------------

# JSON log lines, which is the only format the check counts events in.
# ---------------------------------------------------------------------------
# Routing. One scheduler serves the macOS and the Linux workers, and the
# OSFamily property is all that keeps an action on the right operating system.
# ---------------------------------------------------------------------------

# Exact matching: with any other type a worker is offered actions whose
# OSFamily differs from its own.
settings "$hub" | grep -q -x 'OSFamily: "exact",' ||
  fail "hub.json5 must match OSFamily exactly: it is what keeps a macOS action off a Linux worker"
# Every action gets an OSFamily before it is scheduled: the client's, or
# "macos". The order matters. "add" overwrites, so the client's value is moved
# aside first and put back afterwards; in any other order a Linux action is
# scheduled as a macOS one, or an untagged action is offered to every worker.
expected_modifications='{ replace: { name: "OSFamily", new_name: "requested-OSFamily" } },
{ add: { name: "OSFamily", value: "macos" } },
{ replace: { name: "requested-OSFamily", new_name: "OSFamily" } },'
actual_modifications="$(settings "$hub" | sed -n '/^modifications: \[$/,/^\],$/p' | sed '1d;$d')"
if [[ "$actual_modifications" != "$expected_modifications" ]]; then
  fail "hub.json5's property_modifier must default OSFamily to macos in three steps (move aside, add, put back); found: ${actual_modifications}"
fi
# The macOS worker declares the value the default routes to, and a name a
# build can pin. The key is one the hub does not list, so it restricts only
# workers that declare it.
settings "$worker" | grep -q -x 'OSFamily: { values: \["macos"\] },' ||
  fail "worker-macos.json5 must declare OSFamily macos: the hub schedules an untagged action as a macOS action"
settings "$worker" | grep -q -x -F '"worker-host": { values: ["${NL_WORKER_NAME}"] },' ||
  fail "worker-macos.json5 must declare worker-host as its own name: a build pinned to one worker can otherwise land on the worker that declares none"
grep -q -x '## Routing' "$readme" ||
  fail "README.md must keep its \"Routing\" section: hub.json5, the units and the Linux runbook point at it"

grep -q -x 'Environment=NL_LOG=json' "$hub_unit" ||
  fail "the hub's unit must set NL_LOG=json: hub-health.sh reads that format"
# The journal of the unit the hub runs as.
grep -q -x 'ExecStart=%h/nativelink-macos-hub/hub-health.sh --journal-unit nativelink-macos-hub.service' "$health_unit" ||
  fail "the health check must read the journal of nativelink-macos-hub.service"
grep -q 'journalctl --quiet --unit' "$health" ||
  fail "hub-health.sh must pass --quiet to journalctl so an empty window is not '-- No entries --'"
if grep -q -E 'journalctl .*--user' "$health"; then
  fail "hub-health.sh reads the hub's journal, a system unit: journalctl must not pass --user"
fi
# The hub is a system unit (bduff's user manager ran it at oom_score_adj 200
# and the OOM killer took it first); the health timer and slots exporter stay
# user units.
for line in 'User=bduff' 'Group=bduff' 'Restart=always' 'RestartSec=5' 'LimitNOFILE=65536' \
  'RequiresMountsFor=/mnt/cache' 'OOMScoreAdjust=-900' 'MemoryLow=4G' 'MemorySwapMax=0' \
  'Wants=network-online.target' 'After=network-online.target' 'WantedBy=multi-user.target' \
  'ExecStart=/home/bduff/nativelink-macos-hub/nativelink /home/bduff/nativelink-macos-hub/hub.json5'; do
  grep -q -x -F "$line" "$hub_unit" || fail "the hub's system unit must contain: ${line}"
done
if grep -q -x 'WantedBy=default.target' "$hub_unit"; then
  fail "the hub's unit is a system unit: it must be WantedBy=multi-user.target, not default.target"
fi
for f in "$textfile" "$slots_exporter"; do
  if grep -v -E '^[[:space:]]*(#|""")' "$f" | grep -q -E '"?systemctl"?.*--user'; then
    fail "$(basename "$f") reads the hub, a system unit: systemctl must not pass --user"
  fi
done
grep -q -F 'sudo_copy_if_changed "$REPO/nativelink-macos-hub.service" /etc/systemd/system/nativelink-macos-hub.service 644' "$readme" ||
  fail "README.md must install the hub's unit under /etc/systemd/system with sudo_copy_if_changed"
if grep -q -E 'systemctl --user [a-z-]+ (--now )?nativelink-macos-hub\.service|journalctl --user --unit nativelink-macos-hub\.service|\.config/systemd/user/nativelink-macos-hub\.service 644' "$readme"; then
  fail "README.md must not install or drive the hub as a user unit"
fi

# The Linux worker on the hub's host: its limits keep a compile burst inside
# the worker's cgroup.
hw="${dir}/nativelink/headcrash-worker"
for line in 'MemoryHigh=16G' 'MemoryMax=18G' 'OOMScoreAdjust=300'; do
  grep -q -x -F "$line" "${hw}/nativelink-worker.service.d/memory.conf" ||
    fail "headcrash's memory.conf must contain: ${line}"
done
grep -q -F 'nativelink-1.8.6+stale-entry' "${hw}/nativelink-worker.service.d/60-v1.8.conf" ||
  fail "headcrash's 60-v1.8.conf must start the pinned worker binary"
grep -q -x -F '        max_inflight_tasks: 12,' "${hw}/worker.json5" ||
  fail "headcrash's worker.json5 must run 12 slots"
grep -q -F 'cpu_count: { values: ["12"] }' "${hw}/worker.json5" ||
  fail "headcrash's cpu_count must equal its slots"
if grep -q 'empiricist' "${hw}/nativelink-worker.service"; then
  fail "headcrash's nativelink-worker.service must not name empiricist as the hub"
fi
grep -q -F '/swap2.img' "$readme" ||
  fail "README.md must document the host's swap files"

grep -q -x 'OnUnitActiveSec=1min' "$health_timer" ||
  fail "the health check must run every minute"
# The scheduler by the name the admin API knows it under.
scheduler="$(sed -n -E 's/^SCHEDULER_NAME="([A-Z_]+)"$/\1/p' "$health")"
if [[ -z "$scheduler" ]] || ! settings "$hub" | grep -q -x "name: \"${scheduler}\","; then
  fail "hub-health.sh asks the admin API about scheduler '${scheduler}', which hub.json5 does not define"
fi
# The admin API and a status page on the loopback listener, and the status
# page on the client port too, for a CI step's probe.
settings "$hub" | grep -q -x 'services: { admin: {}, health: {} },' ||
  fail "hub.json5 must serve the admin API and a status page on the admin listener"
if [[ "$(settings "$hub" | grep -c -x 'health: {},')" != "1" ]]; then
  fail "hub.json5 must serve a status page on the client listener: CI's probe asks it"
fi
# The check must never act on what it finds: no command that restarts, stops
# or signals something, at the start of a line or after a shell operator.
if grep -v -E '^[[:space:]]*#' "$health" |
  grep -q -E '(^|\|\||&&|;|\$\()[[:space:]]*(systemctl|launchctl|kill|pkill|killall)[[:space:]]'; then
  fail "hub-health.sh must only report: it must not restart, stop or signal anything"
fi
if cat "$health_unit" "$hub_unit" "$health_timer" "$textfile_unit" "$textfile_timer" "$slots_unit" | grep -v -E '^[[:space:]]*#' |
  grep -q -E 'OnFailure|RuntimeMaxSec|WatchdogSec|OnCalendar'; then
  fail "no unit may restart the hub on a schedule or on a failed health check"
fi

# ---------------------------------------------------------------------------
# Hub restart exporter (textfile) and slots/CAS exporter.
# ---------------------------------------------------------------------------

# CAS cap: 95% of the store filesystem, minus 5 GB for the action cache.
# The live hub.env value is that formula; the number stays on the host.
grep -q -E '^//   NL_HUB_CAS_MAX_BYTES   Size of the CAS in bytes. 95% of the filesystem' "$hub" ||
  fail "hub.json5 must size NL_HUB_CAS_MAX_BYTES at 95% of the filesystem"
grep -q -F 'minus 5 GB for the' "$hub" ||
  fail "hub.json5 must subtract 5 GB from the CAS cap for the action cache"
grep -q -F 'NL_HUB_CAS_MAX_BYTES=<95% of that filesystem, minus 5 GB, in bytes>' "$readme" ||
  fail "README.md's hub.env example must size the CAS at 95% minus 5 GB"

# Restart exporter: reads the system unit as root, writes node_exporter's
# textfile directory atomically, never restarts the hub.
grep -q -x 'OnUnitActiveSec=30s' "$textfile_timer" ||
  fail "the hub restart exporter must run every 30 s"
grep -q -x 'Type=oneshot' "$textfile_unit" ||
  fail "nativelink-hub-textfile.service must be oneshot"
grep -q -x 'After=nativelink-macos-hub.service' "$textfile_unit" ||
  fail "nativelink-hub-textfile.service must be ordered after nativelink-macos-hub.service"
if grep -q 'user@1000\.service' "$textfile_unit"; then
  fail "nativelink-hub-textfile.service must not reference user@1000.service"
fi
grep -q -x 'ExecStart=/home/bduff/nativelink-macos-hub/hub-textfile-metrics.sh' "$textfile_unit" ||
  fail "the restart exporter's system unit must run hub-textfile-metrics.sh"
grep -q 'mktemp' "$textfile" && grep -q -F 'mv "$tmp" "$OUT"' "$textfile" ||
  fail "hub-textfile-metrics.sh must write the textfile atomically (temp file, then mv)"
if grep -q 'HUB_USER' "$textfile"; then
  fail "hub-textfile-metrics.sh must not define dead HUB_USER / NL_HUB_USER"
fi
if grep -v -E '^[[:space:]]*#' "$textfile" | grep -q -E 'systemctl.*(restart|stop|kill|disable)'; then
  fail "hub-textfile-metrics.sh must only read the hub unit: it must not restart or stop it"
fi

# Slots/CAS exporter: loopback only, inotify insert/eviction counters, 15-minute
# low-priority walk, cap from the running hub process.
grep -q -x 'EnvironmentFile=%h/nativelink-macos-hub/hub.env' "$slots_unit" ||
  fail "the slots exporter's unit must read hub.env, where the admin address is"
grep -q -x 'ExecStart=/usr/bin/python3 %h/nativelink-macos-hub/worker-slots-exporter.py' "$slots_unit" ||
  fail "the slots exporter's unit must start worker-slots-exporter.py"
grep -q -F 'HTTPServer(("127.0.0.1", 9108), H)' "$slots_exporter" ||
  fail "worker-slots-exporter.py must listen on 127.0.0.1:9108"
if grep -q -E 'HTTPServer\(\("0\.0\.0\.0"' "$slots_exporter"; then
  fail "worker-slots-exporter.py must not bind non-loopback"
fi
grep -q -F '/admin/scheduler/MAIN_SCHEDULER/workers' "$slots_exporter" ||
  fail "worker-slots-exporter.py must ask the admin API about MAIN_SCHEDULER"
grep -q -F 'IN_MOVED_TO into' "$slots_exporter" ||
  fail "worker-slots-exporter.py must keep its comments on inotify insert/eviction counters"
grep -q -F 'A walk of cas-content/d2 every 15 min' "$slots_exporter" ||
  fail "worker-slots-exporter.py must keep its comments on the 15-minute low-priority walk"
grep -q -F 'time.sleep(900)' "$slots_exporter" ||
  fail "worker-slots-exporter.py must walk the CAS every 15 minutes"
grep -q -F 'os.setpriority(os.PRIO_PROCESS, tid, 19)' "$slots_exporter" ||
  fail "worker-slots-exporter.py must run the CAS walk at nice 19"
grep -q -F 'NL_HUB_CAS_MAX_BYTES of the *running* hub' "$slots_exporter" ||
  fail "worker-slots-exporter.py must keep its comments on reading the cap from the running hub process"
grep -q -F '/proc/{pid}/environ' "$slots_exporter" ||
  fail "worker-slots-exporter.py must read NL_HUB_CAS_MAX_BYTES from the running hub process"
if grep -v -E '^[[:space:]]*(#|")' "$slots_exporter" | grep -q -E 'systemctl.*restart|systemctl.*stop'; then
  fail "worker-slots-exporter.py must only read: it must not restart or stop the hub"
fi
grep -q -x '## Hub metrics' "$readme" ||
  fail "README.md must keep its \"Hub metrics\" section"
grep -q -F 'copy_if_changed' "$readme" && grep -q -F 'cmp -s' "$readme" ||
  fail "README.md install steps must copy a file only when content differs (identical content is a no-op)"
grep -q -x '## Installing or updating hub files' "$readme" ||
  fail "README.md must keep its \"Installing or updating hub files\" section"

# ---------------------------------------------------------------------------
# One NativeLink version: the build script, the patches and the runbook agree.
# ---------------------------------------------------------------------------

version="$(sed -n -E 's/^NATIVELINK_VERSION="([0-9.]+)"$/\1/p' "$build")"
commit="$(sed -n -E 's/^NATIVELINK_COMMIT="([0-9a-f]{40})"$/\1/p' "$build")"
if [[ -z "$version" || -z "$commit" ]]; then
  fail "build-worker.sh must pin NATIVELINK_VERSION and NATIVELINK_COMMIT"
fi

# The lines of build-worker.sh's PATCHES list:
# "<file> <marker> <platforms> <role>".
# In a file: the test sandbox does not let bash create the temporary file a
# here-string needs.
patch_list="${TEST_TMPDIR:?}/patches.list"
sed -n -E 's/^  "([a-z0-9-]+\.patch) ([a-z-]+) ((Darwin|Linux)(,(Darwin|Linux))*) (worker|hub)"$/\1 \2 \3 \7/p' "$build" > "$patch_list"
# reported_version <role> <platform>: what the binary of that role built on
# that platform reports, which is the release and the marker of every patch it
# takes.
reported_version() {
  local markers
  markers="$(awk -v r="$1" -v p="$2" '$4 == r && ("," $3 ",") ~ ("," p ",") { s = s (s == "" ? "" : ".") $2 } END { print s }' "$patch_list")"
  [[ -n "$markers" ]] && echo "${version}+${markers}"
}

[[ -s "$patch_list" ]] || fail "build-worker.sh lists no patches"
for patch in "$patches_dir"/*.patch; do
  name="$(basename "$patch")"
  grep -q "^${name} " "$patch_list" ||
    fail "${name} is in patches/ but not in build-worker.sh's PATCHES, so no worker is built with it"
  grep -q -x "Applies to NativeLink v${version} (${commit})." "$patch" ||
    fail "${name} does not say it applies to v${version} (${commit})"
  # The version depends on which patches a platform takes, so build-worker.sh
  # stamps it. A patch that set it would not apply next to another that did.
  if grep -q '^diff --git a/BUILD.bazel ' "$patch"; then
    fail "${name} changes BUILD.bazel; the binary's version is stamped by build-worker.sh"
  fi
done
while read -r name _; do
  [[ -f "${patches_dir}/${name}" ]] ||
    fail "build-worker.sh lists ${name}, which is not in patches/"
done < "$patch_list"
for role_platform in worker:Darwin worker:Linux hub:Linux; do
  role="${role_platform%%:*}"
  platform="${role_platform##*:}"
  reported="$(reported_version "$role" "$platform")" ||
    fail "no patch in build-worker.sh applies to a ${role} on ${platform}"
  grep -q -F "nativelink ${reported}" "$readme" ||
    fail "README.md does not name the version a ${platform} ${role} reports (nativelink ${reported})"
done
grep -q -F "$commit" "$readme" ||
  fail "README.md does not name the pinned commit ${commit}"
for file in "$hub" "$worker"; do
  grep -q -F "$version" "$file" || fail "$(basename "$file") does not say which NativeLink version it is for (${version})"
done

# ---------------------------------------------------------------------------
# No address is written into the repository.
# ---------------------------------------------------------------------------

if [[ -e "${dir}/nativelink/hub.env" ]]; then
  fail "hub.env names listen addresses and must not be in the repository"
fi

for file in "$hub" "$worker" "$plist" "$hub_unit" "$health_unit" "$health_timer" "$textfile" "$textfile_unit" "$textfile_timer" "$slots_exporter" "$slots_unit" "$readme" "$grafana_overview" "$grafana_alerts"; do
  # Loopback is the one address that is the same on every host.
  if grep -o -E '([0-9]{1,3}\.){3}[0-9]{1,3}' "$file" | grep -v -x -q '127\.0\.0\.1'; then
    fail "$(basename "$file") contains an IP address; addresses are host configuration"
  fi
  if grep -q -E 'grpcs?://[a-z0-9]' "$file"; then
    fail "$(basename "$file") names a remote endpoint"
  fi
done

# ---------------------------------------------------------------------------
# Grafana dashboard and alerting rules.
# ---------------------------------------------------------------------------

if ! python3 -c '
import json
import re
import sys

dash_path, alert_path = sys.argv[1], sys.argv[2]
with open(dash_path) as f:
    dash = json.load(f)

uid = dash.get("uid")
assert uid == "nativelink-overview", f"unexpected uid: {uid}"
panels = dash.get("panels", [])
ids = [p.get("id") for p in panels]
assert len(ids) == len(set(ids)), "duplicate panel ids in nativelink-overview.json"
assert len(panels) >= 50, f"expected >= 50 panels (37 original + new monitoring panels), got {len(panels)}"
for orig_id in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138):
    assert orig_id in ids, f"original panel id {orig_id} missing"

all_exprs = "\n".join(
    t.get("expr", "")
    for p in panels
    for t in p.get("targets", [])
)
for metric in (
    "nativelink_hub_status_up",
    "nativelink_hub_status_duration_seconds",
    "nativelink_hub_connected_workers",
    "nativelink_hub_store_health_timeouts_total",
    "nativelink_hub_worker_kill_ack_evictions_total",
    "nativelink_hub_scheduler_action_timeouts_total",
    "nativelink_hub_action_kills_total",
    "nativelink_hub_command_timeouts_total",
    "nativelink_hub_compressed_upload_decoded_size_errors_total",
    "nativelink_hub_cache_used_bytes",
    "node_disk_io_time_seconds_total",
    "node_disk_write_time_seconds_total",
    "node_pressure_io_stalled_seconds_total",
    "node_memory_SwapFree_bytes",
    "nativelink_hub_memory_current_bytes",
):
    assert metric in all_exprs, f"dashboard missing metric {metric}"

with open(alert_path) as f:
    alert_text = f.read()

assert "apiVersion: 1" in alert_text
assert "PBFA97CFB590B2093" in alert_text
for uid in (
    "nl-hub-store-health-timeout",
    "nl-hub-status-probe-failed",
    "nl-hub-workers-below-normal",
    "nl-hub-kills-timeouts-burst",
    "nl-hub-sched-timeouts-evictions",
    "nl-hub-decoded-size-error",
    "nl-hub-cache-disk-full",
):
    assert f"uid: {uid}" in alert_text, f"missing alert rule uid {uid}"
for m in re.finditer(r"panelId:\s*(\d+)", alert_text):
    pid = int(m.group(1))
    assert pid in ids, f"alert rule references nonexistent panelId {pid}"
' "$grafana_overview" "$grafana_alerts"; then
  fail "Grafana dashboard or alerting validation failed"
fi

# The developer-side bound on a silent hub is this script, and it must still
# produce the same workspace-status keys every other build uses.
if [[ ! -x "$dev_watchdog" ]]; then
  fail "dev-remote-workspace-status.sh must exist and be executable"
fi
if ! grep -q 'exec "$original"' "$dev_watchdog" && ! grep -q 'exec "$original" "$@"' "$dev_watchdog"; then
  fail "dev-remote-workspace-status.sh must exec the original workspace-status command"
fi
if ! grep -q 'dev-remote-workspace-status.sh' "$readme"; then
  fail "README.md must name the developer status watchdog"
fi
if [[ ! -f "$remote_status" ]]; then
  fail "remote-status.sh must exist"
fi
if ! grep -q 'source "${here}/remote-status.sh"' "$dev_watchdog"; then
  fail "dev-remote-workspace-status.sh must source remote-status.sh"
fi
if ! grep -q 'ci-env.sh' "$readme" || ! grep -q 'repobin and checkleft' "$readme"; then
  fail "README.md must say the CI step watchdog covers bazel clients repobin and checkleft launch"
fi
if ! grep -q '~15 s + 10 s (derived)' "$readme"; then
  fail "README.md must mark the during-a-build bound as derived"
fi

# ---------------------------------------------------------------------------
# remote-status.sh: endpoint parsing, TCP-then-HTTP probe, stuck classification.
# ---------------------------------------------------------------------------
# shellcheck source=../remote-status.sh
source "$remote_status"

expect_equal() {
  if [[ "$1" != "$2" ]]; then
    fail "$3: expected '$1', got '$2'"
  fi
}

expect_host_port() {
  local actual
  if ! actual="$(remote_status_endpoint_host_port "$2")"; then
    fail "endpoint '$2': expected '$1', call failed"
    return
  fi
  expect_equal "$1" "$actual" "endpoint '$2'"
}

expect_malformed() {
  local stdout
  if stdout="$(remote_status_endpoint_host_port "$1")"; then
    fail "endpoint '$1': expected failure, succeeded with '${stdout}'"
    return
  fi
  if [[ -n "$stdout" ]]; then
    fail "endpoint '$1': a failing call must print nothing on stdout, printed '${stdout}'"
  fi
}

expect_host_port "executor.test 50051" "grpc://executor.test:50051"
expect_host_port "executor.test 443" "grpcs://executor.test:443"
expect_host_port "100.64.0.7 50051" "grpc://100.64.0.7:50051"
expect_host_port "::1 50051" "grpc://[::1]:50051"
expect_malformed "executor.test:50051"
expect_malformed "http://executor.test:50051"
expect_malformed "grpc://executor.test"
expect_malformed "grpc://:50051"
expect_malformed "grpc://executor.test:port"
expect_malformed "grpc://executor.test:50051/instance"
expect_malformed "grpc://executor test:50051"
expect_malformed "grpc://:"

tmp="${TEST_TMPDIR:?}"
nc_calls="${tmp}/nc-calls"
nc_status=0
nc() {
  printf '%s\n' "$*" >> "$nc_calls"
  return "$nc_status"
}

curl_calls="${tmp}/curl-calls"
curl_http=200
curl_fail_exit=""
curl() {
  printf '%s\n' "$*" >> "$curl_calls"
  printf '%s' "$curl_http"
  if [[ -n "$curl_fail_exit" ]]; then
    return "$curl_fail_exit"
  fi
  if [[ "$curl_http" == "000" ]]; then
    return 28
  fi
  return 0
}

: > "$nc_calls"
expect_equal "0" "$(remote_status_tcp_reachable darwin hub.test 50051 5 && echo 0 || echo 1)" "tcp reachable"
expect_equal "-z -G 5 hub.test 50051" "$(cat "$nc_calls")" "darwin nc flags"
: > "$nc_calls"
remote_status_tcp_reachable linux hub.test 50051 5 || true
expect_equal "-z -w 5 hub.test 50051" "$(cat "$nc_calls")" "linux nc flags"
nc_status=1
expect_equal "1" "$(remote_status_tcp_reachable darwin hub.test 50051 5 && echo 0 || echo 1)" "tcp unreachable"
nc_status=0

: > "$curl_calls"
expect_equal "200" "$(remote_status_http "grpc://hub.test:50051" "hub.test" 50051 10)" "http 200"
expect_equal \
  "--silent --output /dev/null --max-time 10 --write-out %{http_code} http://hub.test:50051/status" \
  "$(cat "$curl_calls")" "http curl args"
curl_http=000
expect_equal "timeout" "$(remote_status_http "grpc://hub.test:50051" "hub.test" 50051 10)" "http timeout"
curl_fail_exit=52
expect_equal "curl:52" "$(remote_status_http "grpc://hub.test:50051" "hub.test" 50051 10)" "http curl 52"
curl_fail_exit=""
curl_http=503
expect_equal "503" "$(remote_status_http "grpc://hub.test:50051" "hub.test" 50051 10)" "http 503"
curl_http=200
expect_equal "200" "$(remote_status_http "grpcs://hub.test:443" "hub.test" 443 10)" "https"

nc_status=1
: > "$curl_calls"
expect_equal "unreachable" "$(remote_status_probe "grpc://hub.test:50051" darwin 5 10)" "probe unreachable"
expect_equal "" "$(cat "$curl_calls")" "probe unreachable: curl was not run"
if remote_status_stuck unreachable; then
  fail "unreachable must not be stuck"
fi
nc_status=0
curl_http=000
expect_equal "timeout" "$(remote_status_probe "grpc://hub.test:50051" darwin 5 10)" "probe timeout after connect"
if ! remote_status_stuck timeout; then
  fail "timeout must be stuck"
fi
if ! remote_status_stuck 503; then
  fail "503 must be stuck"
fi
if remote_status_stuck "curl:52"; then
  fail "curl:52 must not be stuck"
fi
if remote_status_stuck 200; then
  fail "200 must not be stuck"
fi

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
