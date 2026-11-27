#!/bin/bash
# Unit tests for the NativeLink hub Prometheus exporters:
#   - nativelink/worker-slots-exporter.py (connected workers, /status probe,
#     journal event classification and metric rendering)
#   - nativelink/hub-textfile-metrics.sh (restart state + cgroup memory metrics)
# Note: no bash here-documents (<<) because macOS bash 3.2 creates temp files
# outside TMPDIR, which hermetic_test_wrapper.sh's seatbelt sandbox denies.
set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
dir="${workspace}/tools/remote-execution"
slots_exporter="${dir}/nativelink/worker-slots-exporter.py"
textfile_script="${dir}/nativelink/hub-textfile-metrics.sh"

# ---------------------------------------------------------------------------
# 1. worker-slots-exporter.py: journal parsing, worker counts, /status probe
# ---------------------------------------------------------------------------
python3 -c '
import http.server
import importlib.util
import json
import os
import sys
import threading
import time

path = sys.argv[1]
spec = importlib.util.spec_from_file_location("worker_slots_exporter", path)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

# --- Connected worker counts ---
os.environ["NL_HEALTH_EXPECTED_WORKERS"] = "10"
workers_payload = [
    {"id": "anaplian-1", "platform_properties": {"OSFamily": "macos"}, "max_inflight_tasks": 12, "running_actions": 4, "is_paused": False, "is_draining": False},
    {"id": "skaffen-2", "platform_properties": {"OSFamily": "macos"}, "max_inflight_tasks": 12, "running_actions": 2, "is_paused": False, "is_draining": False},
    {"id": "headcrash-3", "platform_properties": {"OSFamily": "linux"}, "max_inflight_tasks": 12, "running_actions": 6, "is_paused": False, "is_draining": True},
]
rendered = mod.render_workers(workers_payload)
assert "nativelink_pool_admin_up 1" in rendered, rendered
assert "nativelink_hub_connected_workers 3" in rendered, rendered
assert "nativelink_hub_connected_workers_by_os{os=\"macos\"} 2" in rendered, rendered
assert "nativelink_hub_connected_workers_by_os{os=\"linux\"} 1" in rendered, rendered
assert "nativelink_hub_expected_workers 10" in rendered, rendered

# --- Journal event classification (real 2026-11-27 NativeLink v1.8.6 JSON shapes) ---
def jline(fields, level="WARN"):
    return json.dumps({"timestamp": "2026-11-27T22:22:32.870120Z", "level": level, "fields": fields}, separators=(",", ":"))

samples = [
    (
        jline({"message": "FilesystemStore::check_health: stat timed out", "content_path": "/mnt/cache/nativelink/cas-content", "timeout_secs": 2}),
        ["nativelink_hub_store_health_timeouts_total"],
    ),
    (
        jline({"message": "Worker did not acknowledge a kill in time, removing from pool", "worker_id": "zakalwe-1"}),
        ["nativelink_hub_worker_kill_ack_evictions_total"],
    ),
    (
        jline({"message": "Evicting worker from pool", "reason": "Worker zakalwe-1 did not acknowledge a kill within 60s, removing from pool"}, level="INFO"),
        [],
    ),
    (
        jline({"message": "Worker timed out, removing from pool", "worker_id": "diziet-1"}),
        ["nativelink_hub_worker_silence_evictions_total"],
    ),
    (
        jline({"message": "Timing out operation", "operation_id": "op1", "cause": "exceeded its Action.timeout of 300 seconds"}),
        ["nativelink_hub_scheduler_action_timeouts_total"],
    ),
    (
        jline({"message": "OperationId op1 exceeded its Action.timeout of 300 seconds, issuing a retry"}),
        [],
    ),
    (
        jline({"message": "Action ended by a kill", "worker_id": "rascal-1", "outcome": "KILLED_TIMEOUT"}),
        ["nativelink_hub_action_kills_total"],
    ),
    (
        jline({"message": "Command timed out on the worker; queued again for another worker", "operation_id": "op2"}),
        ["nativelink_hub_command_timeouts_total"],
    ),
    (
        jline({"error": "code: InvalidArgument, message: \"Decoded compressed upload size 12026 bytes exceeds digest size 11876 bytes : In ByteStreamServer::write_compressed\""}, level="ERROR"),
        ["nativelink_hub_compressed_upload_decoded_size_errors_total"],
    ),
    (
        jline({"message": "Worker registered with scheduler"}, level="INFO"),
        [],
    ),
]
for line, expected in samples:
    got = mod.record_journal_line(line)
    assert got == expected, f"line={line!r}: expected {expected}, got {got}"

jtext = "\n".join(mod.journal_metrics())
for metric in (
    "nativelink_hub_store_health_timeouts_total 1",
    "nativelink_hub_worker_kill_ack_evictions_total 1",
    "nativelink_hub_worker_silence_evictions_total 1",
    "nativelink_hub_scheduler_action_timeouts_total 1",
    "nativelink_hub_action_kills_total 1",
    "nativelink_hub_command_timeouts_total 1",
    "nativelink_hub_compressed_upload_decoded_size_errors_total 1",
    "nativelink_hub_journal_watch_up 0",
):
    assert metric in jtext, f"missing {metric!r} in:\n{jtext}"

# --- Reconnect resumes the cursor and counts gap events exactly once ---
from unittest.mock import patch
from types import SimpleNamespace
import io

def entry(cursor, message):
    return json.dumps({"__CURSOR": cursor, "MESSAGE": message})

baseline = entry("before", "hub started")
fault = entry("gap", samples[0][0])
commands = []
def follow(cmd, **kwargs):
    commands.append(cmd)
    # The first follower exits; an event arrives during its reconnect delay.
    lines = "" if len(commands) == 1 else fault + "\n"
    return SimpleNamespace(stdout=io.StringIO(lines), terminate=lambda: None, wait=lambda **kw: 0)

before = mod._journal_counts["nativelink_hub_store_health_timeouts_total"]
with patch.object(mod.subprocess, "run", return_value=SimpleNamespace(returncode=0, stderr="", stdout=baseline)), patch.object(mod.subprocess, "Popen", side_effect=follow):
    cursor = mod._journal_follow_once(None)
    assert cursor == "before"
    cursor = mod._journal_follow_once(cursor)
    assert cursor == "gap"
    cursor = mod._journal_follow_once(cursor)
assert "--no-tail" in commands[1]
assert commands[1][-2:] == ["--after-cursor", "before"]
assert commands[2][-2:] == ["--after-cursor", "gap"]
assert mod._journal_counts["nativelink_hub_store_health_timeouts_total"] == before + 1
with patch.object(mod.subprocess, "run", return_value=SimpleNamespace(returncode=0, stderr="Permission denied", stdout="")), patch.object(mod.subprocess, "Popen") as popen:
    assert mod._journal_follow_once(cursor) == cursor
    popen.assert_not_called()
assert mod._journal_watch_up == 0

# --- Hub-side /status probe (200, 503, timeout) ---
mode = {"kind": "200"}

class StubHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if mode["kind"] == "200":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"ok")
        elif mode["kind"] == "503":
            self.send_response(503)
            self.end_headers()
            self.wfile.write(b"unhealthy")
        elif mode["kind"] == "timeout":
            time.sleep(1.5)
            try:
                self.send_response(200)
                self.end_headers()
            except BrokenPipeError:
                pass
    def log_message(self, *a):
        pass

srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StubHandler)
port = srv.server_address[1]
t = threading.Thread(target=srv.serve_forever, daemon=True)
t.start()
try:
    url = f"http://127.0.0.1:{port}/status"

    up, code, dur, outcome = mod.probe_status_once(url=url, timeout=2)
    assert (up, code, outcome) == (1, 200, "ok") and dur >= 0, (up, code, dur, outcome)
    mod._record_status(up, code, dur, outcome, now=1700000000.0)

    mode["kind"] = "503"
    up, code, dur, outcome = mod.probe_status_once(url=url, timeout=2)
    assert (up, code, outcome) == (0, 503, "503") and dur >= 0, (up, code, dur, outcome)
    mod._record_status(up, code, dur, outcome, now=1700000015.0)

    mode["kind"] = "timeout"
    up, code, dur, outcome = mod.probe_status_once(url=url, timeout=0.2)
    assert (up, code, outcome) == (0, 0, "timeout") and dur >= 0.15, (up, code, dur, outcome)
    mod._record_status(up, code, dur, outcome, now=1700000030.0)

    stext = "\n".join(mod.status_metrics())
    for expected in (
        "nativelink_hub_status_up 0",
        "nativelink_hub_status_http_code 0",
        "nativelink_hub_status_probes_total{outcome=\"ok\"} 1",
        "nativelink_hub_status_probes_total{outcome=\"503\"} 1",
        "nativelink_hub_status_probes_total{outcome=\"timeout\"} 1",
        "nativelink_hub_status_probes_total{outcome=\"error\"} 0",
    ):
        assert expected in stext, f"missing {expected!r} in:\n{stext}"
finally:
    srv.shutdown()
    srv.server_close()

print("worker-slots-exporter unit tests ok")
' "$slots_exporter"

# ---------------------------------------------------------------------------
# 2. hub-textfile-metrics.sh: restart state + cgroup memory metrics
# ---------------------------------------------------------------------------
stub_bin="${TEST_TMPDIR:?}/bin"
out_dir="${TEST_TMPDIR:?}/textfile"
mkdir -p "$stub_bin" "$out_dir"

printf '%s\n' \
  '#!/bin/bash' \
  'printf "%s\n" "NRestarts=2" "ExecMainStartTimestamp=@1795453365" "ActiveState=active" "Result=success" "ExecMainStatus=0" "MemoryCurrent=5688541184" "MemoryPeak=16464990208"' \
  > "${stub_bin}/systemctl"
chmod +x "${stub_bin}/systemctl"

# Note: hub-textfile-metrics.sh uses `declare -A` (bash 4+ on Linux); run it
# with bash if >=4 or skip the associative-array execution on macOS bash 3.2
# after checking shell syntax and the memory property/metric contract.
if ((BASH_VERSINFO[0] >= 4)); then
  PATH="${stub_bin}:${PATH}" NL_TEXTFILE_DIR="$out_dir" bash "$textfile_script"
  prom="${out_dir}/nativelink_hub.prom"
  grep -q -x 'nativelink_hub_restarts_total 2' "$prom"
  grep -q -x 'nativelink_hub_start_time_seconds 1795453365' "$prom"
  grep -q -x 'nativelink_hub_up 1' "$prom"
  grep -q -x 'nativelink_hub_last_result{result="success"} 1' "$prom"
  grep -q -x 'nativelink_hub_main_exit_status 0' "$prom"
  grep -q -x 'nativelink_hub_memory_current_bytes 5688541184' "$prom"
  grep -q -x 'nativelink_hub_memory_peak_bytes 16464990208' "$prom"

  printf '%s\n' \
    '#!/bin/bash' \
    'printf "%s\n" "NRestarts=0" "ExecMainStartTimestamp=@1795453365" "ActiveState=active" "Result=success" "ExecMainStatus=0" "MemoryCurrent=[not set]" "MemoryPeak=infinity"' \
    > "${stub_bin}/systemctl"
  PATH="${stub_bin}:${PATH}" NL_TEXTFILE_DIR="$out_dir" bash "$textfile_script"
  if grep -q 'nativelink_hub_memory_' "$prom"; then
    echo "FAIL: non-numeric memory values must not be emitted" >&2
    cat "$prom" >&2
    exit 1
  fi
else
  bash -n "$textfile_script"
  for name in MemoryCurrent MemoryPeak nativelink_hub_memory_current_bytes nativelink_hub_memory_peak_bytes; do
    grep -q -F "$name" "$textfile_script"
  done
  echo "bash 3.2: memory exporter syntax and names checked; runtime requires bash 4+"
fi

echo "ok"
