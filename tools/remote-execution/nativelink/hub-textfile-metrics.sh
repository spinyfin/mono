#!/bin/bash
# hub-textfile-metrics.sh -- export the NativeLink hub's restart state to
# Prometheus through node_exporter's textfile collector.
#
# The hub runs as a *user* systemd unit (bduff's nativelink-macos-hub.service),
# which node_exporter's systemd collector cannot see, so an OOM kill followed
# by systemd's Restart=always left no trace on the dashboards. This script
# reads the unit's state and writes it to
# /var/lib/prometheus/node-exporter/nativelink_hub.prom, atomically (temp file
# in the same directory, then mv).
#
# Runs as root every 30 s from nativelink-hub-textfile.timer
# (/etc/systemd/system/nativelink-hub-textfile.{service,timer}).
#
# Metrics:
#   nativelink_hub_restarts_total      systemd NRestarts (automatic restarts;
#                                      resets to 0 on a manual restart)
#   nativelink_hub_start_time_seconds  unix time the current main process
#                                      started; changes on *every* restart,
#                                      so changes() over it counts all restarts
#   nativelink_hub_up                  1 if the unit is active
#   nativelink_hub_last_result{result} systemd Result of the unit (oom-kill,
#                                      success, exit-code, ...)
#   nativelink_hub_main_exit_status    ExecMainStatus of the last main process
# The check only reads; it never restarts or stops anything.
set -euo pipefail

UNIT=${NL_HUB_UNIT:-nativelink-macos-hub.service}
HUB_USER=${NL_HUB_USER:-bduff}
OUT_DIR=${NL_TEXTFILE_DIR:-/var/lib/prometheus/node-exporter}
OUT="$OUT_DIR/nativelink_hub.prom"

declare -A P
while IFS='=' read -r k v; do P[$k]=$v; done < <(
  systemctl --user -M "${HUB_USER}@" show "$UNIT" --timestamp=unix \
    -p NRestarts,ExecMainStartTimestamp,ActiveState,Result,ExecMainStatus
)

restarts=${P[NRestarts]:-0}
start=${P[ExecMainStartTimestamp]:-}
start=${start#@}
[[ $start =~ ^[0-9]+$ ]] || start=0
up=0; [[ ${P[ActiveState]:-} == active ]] && up=1
result=${P[Result]:-unknown}
result=${result//[^a-zA-Z0-9_-]/_}
status=${P[ExecMainStatus]:-0}
[[ $status =~ ^[0-9]+$ ]] || status=0

tmp=$(mktemp "$OUT_DIR/.nativelink_hub.prom.XXXXXX")
trap 'rm -f "$tmp"' EXIT
cat > "$tmp" <<PROM
# HELP nativelink_hub_restarts_total Automatic restarts of the hub unit (systemd NRestarts; resets on manual restart).
# TYPE nativelink_hub_restarts_total counter
nativelink_hub_restarts_total $restarts
# HELP nativelink_hub_start_time_seconds Unix time the hub's current main process started.
# TYPE nativelink_hub_start_time_seconds gauge
nativelink_hub_start_time_seconds $start
# HELP nativelink_hub_up 1 if the hub's systemd unit is active.
# TYPE nativelink_hub_up gauge
nativelink_hub_up $up
# HELP nativelink_hub_last_result systemd Result of the hub unit.
# TYPE nativelink_hub_last_result gauge
nativelink_hub_last_result{result="$result"} 1
# HELP nativelink_hub_main_exit_status Exit status of the hub's last main process.
# TYPE nativelink_hub_main_exit_status gauge
nativelink_hub_main_exit_status $status
PROM
chmod 0644 "$tmp"
mv "$tmp" "$OUT"
trap - EXIT
