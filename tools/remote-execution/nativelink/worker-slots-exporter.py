#!/usr/bin/env python3
"""Exposes hub health, worker slots, CAS eviction, and journal counters to Prometheus.

The NativeLink hub's own metrics omit per-worker slot counts, CAS eviction
volume, and several scheduler/store journal fault events; this exporter exposes
them on 127.0.0.1:9108/metrics:
  * Per-worker slots, running actions, paused/draining flags, and connected
    worker counts from the hub's loopback admin API (NL_HUB_ADMIN_LISTEN).
  * Hub-side /status health probe result and latency (polled in a background
    thread so a stalled /status never blocks Prometheus scrapes).
  * Hub journal event counters (store health-probe stat timeouts, kill-ack and
    silence evictions, scheduler Action.timeout events, worker command timeouts,
    action kills, and compressed-upload decoded-size errors) tailed from
    journalctl on nativelink-macos-hub.service.
  * Store volume size and CAS insert/eviction figures via inotify and a
    low-priority 15-minute scan.

Started by the user unit nativelink-worker-slots-exporter.service (the hub it
watches is a system unit). It only reads; it never changes anything on the hub.
"""
import json, os, re, socket, subprocess, urllib.error, urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

ADMIN = os.environ.get("NL_HUB_ADMIN_LISTEN", "127.0.0.1:50071")
URL = f"http://{ADMIN}/admin/scheduler/MAIN_SCHEDULER/workers"
STATUS_URL = f"http://{ADMIN}/status"
HUB_UNIT = os.environ.get("NL_HUB_UNIT", "nativelink-macos-hub.service")
STATUS_PROBE_INTERVAL_S = max(1, int(os.environ.get("NL_STATUS_PROBE_INTERVAL_S", "15") or 15))
STATUS_PROBE_TIMEOUT_S = max(1, int(os.environ.get("NL_STATUS_PROBE_TIMEOUT_S", "10") or 10))

def expected_workers():
    raw = os.environ.get("NL_HEALTH_EXPECTED_WORKERS", "").strip()
    return int(raw) if raw.isdigit() and int(raw) > 0 else None

def render_workers(workers):
    out = [
        "# HELP nativelink_pool_admin_up 1 if the hub's admin API answered.",
        "# TYPE nativelink_pool_admin_up gauge",
        "nativelink_pool_admin_up 1",
    ]
    rows = {"slots": [], "running": [], "paused": [], "draining": []}
    by_os = {}
    for w in workers:
        m = re.match(r"^([a-z]+)-", w.get("id", ""))
        host = m.group(1) if m else "unknown"
        # The operating system the worker declares (OSFamily). The one hub
        # serves macOS and Linux workers and routes on it, so the two are
        # separate pools as far as capacity goes.
        osf = (w.get("platform_properties") or {}).get("OSFamily") or "unknown"
        osf = osf if re.fullmatch(r"[a-z0-9_-]+", osf) else "unknown"
        by_os[osf] = by_os.get(osf, 0) + 1
        key = (host, osf)
        rows["slots"].append((key, w.get("max_inflight_tasks", 0)))
        rows["running"].append((key, w.get("running_actions", 0)))
        rows["paused"].append((key, 1 if w.get("is_paused") else 0))
        rows["draining"].append((key, 1 if w.get("is_draining") else 0))
    out += [
        "# HELP nativelink_hub_connected_workers Total workers connected to the hub scheduler.",
        "# TYPE nativelink_hub_connected_workers gauge",
        f"nativelink_hub_connected_workers {len(workers)}",
        "# HELP nativelink_hub_connected_workers_by_os Workers connected to the hub scheduler, by OSFamily.",
        "# TYPE nativelink_hub_connected_workers_by_os gauge",
    ]
    out += [f'nativelink_hub_connected_workers_by_os{{os="{o}"}} {v}' for o, v in sorted(by_os.items())]
    exp = expected_workers()
    if exp is not None:
        out += [
            "# HELP nativelink_hub_expected_workers Expected total workers (NL_HEALTH_EXPECTED_WORKERS from hub.env).",
            "# TYPE nativelink_hub_expected_workers gauge",
            f"nativelink_hub_expected_workers {exp}",
        ]
    for name, help_ in (("slots", "Slots the worker offers (max_inflight_tasks)."),
                        ("running", "Actions the worker is running now."),
                        ("paused", "1 if the worker is paused."),
                        ("draining", "1 if the worker is draining.")):
        metric = {"slots": "nativelink_pool_worker_slots", "running": "nativelink_pool_worker_running_actions",
                  "paused": "nativelink_pool_worker_paused", "draining": "nativelink_pool_worker_draining"}[name]
        out += [f"# HELP {metric} {help_}", f"# TYPE {metric} gauge"]
        totals = {}
        for key, v in rows[name]:
            totals[key] = totals.get(key, 0) + v
        out += [f'{metric}{{host="{h}",os="{o}"}} {v}' for (h, o), v in sorted(totals.items())]
    return "\n".join(out) + "\n"

def render():
    try:
        with urllib.request.urlopen(URL, timeout=5) as r:
            workers = json.load(r)
    except Exception:
        out = [
            "# HELP nativelink_pool_admin_up 1 if the hub's admin API answered.",
            "# TYPE nativelink_pool_admin_up gauge",
            "nativelink_pool_admin_up 0",
        ]
        exp = expected_workers()
        if exp is not None:
            out += [
                "# HELP nativelink_hub_expected_workers Expected total workers (NL_HEALTH_EXPECTED_WORKERS from hub.env).",
                "# TYPE nativelink_hub_expected_workers gauge",
                f"nativelink_hub_expected_workers {exp}",
            ]
        return "\n".join(out) + "\n"
    return render_workers(workers)

def cache():
    """Size of the hub's store volume, from the filesystem: cheap, no directory walk."""
    data = os.environ.get("NL_HUB_DATA", "")
    out = []
    try:
        s = os.statvfs(data)
    except Exception:
        return out
    used = (s.f_blocks - s.f_bfree) * s.f_frsize
    for metric, help_, v in (
        ("nativelink_hub_cache_used_bytes", "Bytes in use on the volume that holds the hub's stores.", used),
        ("nativelink_hub_cache_filesystem_bytes", "Size of that volume.", s.f_blocks * s.f_frsize),
        ("nativelink_hub_cache_cap_bytes", "CAS size at which the running hub evicts (NL_HUB_CAS_MAX_BYTES from the hub process environment).", hub_cas_cap()),
        ("nativelink_hub_cache_files", "Files on that volume (inodes in use): roughly the number of cache entries.", s.f_files - s.f_ffree),
    ):
        out += [f"# HELP {metric} {help_}", f"# TYPE {metric} gauge", f"{metric} {v}"]
    return out

# ---------------------------------------------------------------------------
# CAS eviction figures.
#
# NativeLink 1.8.6 exports no eviction counter (its EvictingMap keeps
# evicted_bytes/evicted_items internally, but they are not in the OTel
# metrics), and it logs evictions only at debug level. So these figures are
# measured from the filesystem, using how its filesystem store works
# (nativelink-store/src/filesystem_store.rs, v1.8.6):
#   * a new CAS entry is written under cas-tmp and renamed into
#     cas-content/d2/<sha256>-<size>-<generation>;
#   * evicting an entry (unref) renames it from cas-content/d2 to
#     cas-tmp/d2/<altered hash>-<size>-<generation> and deletes it there
#     once nothing reads it. A replaced entry takes the same path, but in a
#     content-addressed store that is rare, so every such rename counts as
#     an eviction.
# An inotify watch therefore sees every insert (IN_MOVED_TO into
# cas-content/d2) and every eviction (IN_MOVED_TO into cas-tmp/d2), and the
# size is in the file name, so the counters are measured, not derived; no
# directory walk is needed. Cross-check:
#   increase(evicted_bytes) ~= increase(inserted_bytes) - delta(content bytes).
#
# Retention: the store's LRU is seeded at hub start from each file's atime
# (the store never sets atime itself; ext4 relatime updates it on a read at
# most once a day) and is then kept in memory, where reads *and* existence
# checks (FindMissingBlobs, the action cache's completeness check) promote an
# entry. That in-memory order cannot be read from outside, so the oldest
# atime on disk overstates how long entries survive: an entry kept alive only
# by existence checks keeps an old atime. The eviction stream gives the real
# horizon: right when an entry is evicted it is the least recently used, so
# its atime (stat'd in cas-tmp before the delete) marks the LRU frontier.
#   nativelink_hub_cas_oldest_entry_age_seconds =
#       now - median(atime of the last 1000 evicted entries)
# keeps rising while nothing is evicted, which is right: entries then live
# longer. A walk of cas-content/d2 every 15 min (thread at nice 19 and idle
# I/O priority; 9-20 s wall, 4-12 s CPU for 1.4M files) gives the entry count,
# the content bytes (kept current between walks from the counters; fill =
# content bytes / cap) and the oldest atime on disk.
# ---------------------------------------------------------------------------
import collections, ctypes, select, struct, threading, time

_DATA = os.environ.get("NL_HUB_DATA", "")
_CONTENT = f"{_DATA}/cas-content/d2"
_TMP = f"{_DATA}/cas-tmp/d2"
_IN_MOVED_TO, _IN_Q_OVERFLOW, _IN_IGNORED = 0x80, 0x4000, 0x8000
_lock = threading.Lock()
_st = {"ins_n": 0, "ins_b": 0, "ev_n": 0, "ev_b": 0, "overflows": 0, "watch_up": 0,
       "last_evict": 0.0, "scan": None}
_recent = collections.deque(maxlen=1000)  # (evicted at, atime, mtime)

def _size(name):
    try:
        return int(name.split("-")[1])
    except (IndexError, ValueError):
        return 0

def _watch():
    while True:
        try:
            _watch_once()
        except Exception:
            _st["watch_up"] = 0
            time.sleep(30)

def _watch_once():
    libc = ctypes.CDLL("libc.so.6", use_errno=True)
    while True:
        fd = libc.inotify_init1(os.O_CLOEXEC)
        wc = libc.inotify_add_watch(fd, _CONTENT.encode(), _IN_MOVED_TO)
        wt = libc.inotify_add_watch(fd, _TMP.encode(), _IN_MOVED_TO)
        if fd < 0 or wc < 0 or wt < 0:
            _st["watch_up"] = 0
            if fd >= 0: os.close(fd)
            time.sleep(30); continue
        inodes = (os.stat(_CONTENT).st_ino, os.stat(_TMP).st_ino)
        _st["watch_up"] = 1
        rearm, next_check = False, time.time() + 30
        while not rearm:
            if select.select([fd], [], [], 5)[0]:
                buf = os.read(fd, 1 << 20)
                i = 0
                while i < len(buf):
                    wd, mask, _cookie, ln = struct.unpack_from("iIII", buf, i)
                    name = buf[i + 16:i + 16 + ln].rstrip(b"\0").decode(errors="replace")
                    i += 16 + ln
                    if mask & _IN_Q_OVERFLOW:
                        with _lock: _st["overflows"] += 1
                    elif mask & _IN_IGNORED:
                        rearm = True
                    elif wd == wc:
                        with _lock: _st["ins_n"] += 1; _st["ins_b"] += _size(name)
                    elif wd == wt:
                        now = time.time()
                        try:
                            s = os.stat(f"{_TMP}/{name}")
                            _recent.append((now, s.st_atime, s.st_mtime))
                        except OSError:
                            pass
                        with _lock:
                            _st["ev_n"] += 1; _st["ev_b"] += _size(name); _st["last_evict"] = now
            if time.time() > next_check:  # hub restart may recreate the dirs
                next_check = time.time() + 30
                try:
                    rearm = rearm or inodes != (os.stat(_CONTENT).st_ino, os.stat(_TMP).st_ino)
                except OSError:
                    rearm = True
        _st["watch_up"] = 0
        os.close(fd)

def _scan_loop():
    tid = threading.get_native_id()
    try:
        os.setpriority(os.PRIO_PROCESS, tid, 19)
        ctypes.CDLL("libc.so.6").syscall(251, 1, tid, 3 << 13)  # ioprio_set(WHO_PROCESS, tid, IDLE)
    except Exception:
        pass
    while True:
        t0, c0 = time.time(), time.thread_time()
        with _lock:
            base = (_st["ins_n"] - _st["ev_n"], _st["ins_b"] - _st["ev_b"])
        n = b = 0; oldest = None
        try:
            with os.scandir(_CONTENT) as it:
                for e in it:
                    try:
                        s = e.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    n += 1; b += s.st_size
                    if oldest is None or s.st_atime < oldest: oldest = s.st_atime
            with _lock:
                _st["scan"] = (time.time(), n, b, oldest or 0, time.time() - t0, time.thread_time() - c0, base)
        except OSError:
            pass
        time.sleep(900)

def _median(xs):
    xs = sorted(xs)
    return xs[len(xs) // 2] if xs else None

def hub_cas_cap():
    """NL_HUB_CAS_MAX_BYTES of the *running* hub: hub.env can be ahead of it until the hub restarts."""
    try:
        pid = subprocess.run(["systemctl", "show", "-p", "MainPID", "--value",
                              "nativelink-macos-hub.service"], capture_output=True, text=True, timeout=5).stdout.strip()
        with open(f"/proc/{pid}/environ", "rb") as f:
            for kv in f.read().split(b"\0"):
                if kv.startswith(b"NL_HUB_CAS_MAX_BYTES="):
                    return int(kv.split(b"=", 1)[1])
    except Exception:
        pass
    return int(os.environ.get("NL_HUB_CAS_MAX_BYTES", "0") or 0)

def eviction():
    now = time.time()
    with _lock:
        st = dict(_st); recent = list(_recent)
    rows = [
        ("nativelink_hub_cas_watch_up", "gauge", "1 if the inotify watch on the CAS directories is running.", st["watch_up"]),
        ("nativelink_hub_cas_inotify_overflows_total", "counter", "inotify queue overflows (events lost; counters undercount).", st["overflows"]),
        ("nativelink_hub_cas_inserted_entries_total", "counter", "Entries written into the CAS (renames into cas-content/d2) since the exporter started.", st["ins_n"]),
        ("nativelink_hub_cas_inserted_bytes_total", "counter", "Bytes written into the CAS (size from the file name) since the exporter started.", st["ins_b"]),
        ("nativelink_hub_cas_evicted_entries_total", "counter", "Entries evicted from the CAS (renames into cas-tmp/d2) since the exporter started.", st["ev_n"]),
        ("nativelink_hub_cas_evicted_bytes_total", "counter", "Bytes evicted from the CAS (size from the file name) since the exporter started.", st["ev_b"]),
    ]
    if st["last_evict"]:
        rows.append(("nativelink_hub_cas_last_eviction_timestamp_seconds", "gauge", "Unix time of the last eviction seen.", st["last_evict"]))
    if recent:
        rows.append(("nativelink_hub_cas_oldest_entry_age_seconds", "gauge",
                     "now - median atime of the last 1000 evicted entries: how long an untouched entry survives (LRU frontier).",
                     round(now - _median(r[1] for r in recent), 1)))
        rows.append(("nativelink_hub_cas_evicted_lifetime_seconds", "gauge",
                     "Median time from write (mtime) to eviction over the last 1000 evicted entries.",
                     round(_median(r[0] - r[2] for r in recent), 1)))
    if st["scan"]:
        at, n, b, oldest, dur, cpu, base = st["scan"]
        # Walk result moved forward by the inserts and evictions seen since.
        n += (st["ins_n"] - st["ev_n"]) - base[0]
        b += (st["ins_b"] - st["ev_b"]) - base[1]
        rows += [
            ("nativelink_hub_cas_entries", "gauge", "Entries in the CAS: last walk of cas-content/d2 plus inserts minus evictions since.", n),
            ("nativelink_hub_cas_content_bytes", "gauge", "Bytes in the CAS (sum of entry sizes): last walk plus inserted minus evicted bytes since.", b),
            ("nativelink_hub_cas_oldest_atime_seconds", "gauge", "Oldest atime in cas-content/d2 at the last walk (overstates retention; see exporter comments).", oldest),
            ("nativelink_hub_cas_scan_timestamp_seconds", "gauge", "Unix time the last walk finished.", round(at)),
            ("nativelink_hub_cas_scan_duration_seconds", "gauge", "Wall time of the last walk.", round(dur, 2)),
            ("nativelink_hub_cas_scan_cpu_seconds", "gauge", "CPU time of the last walk.", round(cpu, 2)),
        ]
    out = []
    for metric, typ, help_, v in rows:
        out += [f"# HELP {metric} {help_}", f"# TYPE {metric} {typ}", f"{metric} {v}"]
    return out

# ---------------------------------------------------------------------------
# Hub-side /status health probe.
#
# NativeLink's /status handler runs FilesystemStore::check_health (a 2 s stat
# on cas-content) and returns 503 on timeout without logging the response code.
# Running this probe on a background thread keeps /metrics fast even when
# /status stalls, and records both the result and its wall latency.
# ---------------------------------------------------------------------------
_status_lock = threading.Lock()
_status_st = {
    "probed": False,
    "up": 0,
    "code": 0,
    "duration_s": 0.0,
    "last_ts": 0.0,
    "counts": {"ok": 0, "503": 0, "timeout": 0, "error": 0},
}

def probe_status_once(url=None, timeout=None):
    """Probe the hub's /status endpoint once. Returns (up, http_code, duration_s, outcome)."""
    target = url or STATUS_URL
    tmo = STATUS_PROBE_TIMEOUT_S if timeout is None else timeout
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(target, timeout=tmo) as r:
            code = getattr(r, "status", 200) or 200
            r.read()
        dur = time.monotonic() - t0
        if code == 200:
            return 1, 200, dur, "ok"
        return 0, code, dur, "503" if code == 503 else "error"
    except urllib.error.HTTPError as e:
        dur = time.monotonic() - t0
        try:
            e.read()
        except Exception:
            pass
        code = int(e.code)
        return 0, code, dur, "503" if code == 503 else "error"
    except (socket.timeout, TimeoutError):
        return 0, 0, time.monotonic() - t0, "timeout"
    except urllib.error.URLError as e:
        dur = time.monotonic() - t0
        reason = getattr(e, "reason", None)
        if isinstance(reason, (socket.timeout, TimeoutError)) or "timed out" in str(reason).lower():
            return 0, 0, dur, "timeout"
        return 0, 0, dur, "error"
    except Exception:
        return 0, 0, time.monotonic() - t0, "error"

def _record_status(up, code, dur, outcome, now=None):
    ts = time.time() if now is None else now
    with _status_lock:
        _status_st["probed"] = True
        _status_st["up"] = up
        _status_st["code"] = code
        _status_st["duration_s"] = dur
        _status_st["last_ts"] = ts
        _status_st["counts"][outcome] = _status_st["counts"].get(outcome, 0) + 1

def _status_probe_loop():
    while True:
        up, code, dur, outcome = probe_status_once()
        _record_status(up, code, dur, outcome)
        time.sleep(STATUS_PROBE_INTERVAL_S)

def status_metrics():
    with _status_lock:
        probed = _status_st["probed"]
        up = _status_st["up"]
        code = _status_st["code"]
        dur = _status_st["duration_s"]
        last_ts = _status_st["last_ts"]
        counts = dict(_status_st["counts"])
    out = []
    if probed:
        for metric, help_, v in (
            ("nativelink_hub_status_up", "1 if the hub-side /status health probe returned HTTP 200.", up),
            ("nativelink_hub_status_http_code", "HTTP status code from the last hub-side /status probe (0 on timeout or connection error).", code),
            ("nativelink_hub_status_duration_seconds", "Wall latency of the last hub-side /status probe in seconds.", round(dur, 6)),
            ("nativelink_hub_status_last_probe_timestamp_seconds", "Unix time of the last hub-side /status probe.", round(last_ts, 3)),
        ):
            out += [f"# HELP {metric} {help_}", f"# TYPE {metric} gauge", f"{metric} {v}"]
    out += [
        "# HELP nativelink_hub_status_probes_total Hub-side /status health probes by outcome since exporter start.",
        "# TYPE nativelink_hub_status_probes_total counter",
    ]
    for outcome in ("ok", "503", "timeout", "error"):
        out.append(f'nativelink_hub_status_probes_total{{outcome="{outcome}"}} {counts.get(outcome, 0)}')
    return out

# ---------------------------------------------------------------------------
# Hub journal event counters.
#
# Tails the hub's JSON journal (NL_LOG=json on nativelink-macos-hub.service)
# with cursor-based JSON journalctl output, resuming after reconnects. Every
# counter starts at 0 at exporter start so Prometheus increase() observes even
# the first occurrence of a rare fault without a cold-series blind spot.
# ---------------------------------------------------------------------------
JOURNAL_METRICS = (
    (
        "nativelink_hub_store_health_timeouts_total",
        "FilesystemStore::check_health stat timeouts logged by the hub.",
        re.compile(r"FilesystemStore::check_health: stat timed out"),
    ),
    (
        "nativelink_hub_worker_kill_ack_evictions_total",
        "Workers evicted by the hub for not acknowledging a kill in time.",
        re.compile(r'"message":"Worker did not acknowledge a kill in time, removing from pool"'),
    ),
    (
        "nativelink_hub_worker_silence_evictions_total",
        "Workers evicted by the hub for timing out (silence).",
        re.compile(r'"message":"Worker timed out, removing from pool"'),
    ),
    (
        "nativelink_hub_scheduler_action_timeouts_total",
        "Scheduler Action.timeout events ('Timing out operation') logged by the hub.",
        re.compile(r'"message":"Timing out operation"'),
    ),
    (
        "nativelink_hub_action_kills_total",
        "'Action ended by a kill' events logged by the hub.",
        re.compile(r'"message":"Action ended by a kill"'),
    ),
    (
        "nativelink_hub_command_timeouts_total",
        "'Command timed out on the worker' events logged by the hub.",
        re.compile(r'"message":"Command timed out on the worker'),
    ),
    (
        "nativelink_hub_compressed_upload_decoded_size_errors_total",
        "Compressed-upload decoded-size-exceeds-digest-size errors logged by the hub.",
        re.compile(r"Decoded compressed upload size [0-9]+ bytes exceeds digest size"),
    ),
)

_journal_lock = threading.Lock()
_journal_counts = {name: 0 for name, _, _ in JOURNAL_METRICS}
_journal_watch_up = 0

def classify_journal_line(line):
    """Return the list of journal counter metric names matched by one log line."""
    return [name for name, _, pat in JOURNAL_METRICS if pat.search(line)]

def record_journal_line(line):
    matched = classify_journal_line(line)
    if matched:
        with _journal_lock:
            for name in matched:
                _journal_counts[name] += 1
    return matched

def _journal_follow_once(cursor):
    """Confirm journal access and resume after the last counted entry."""
    global _journal_watch_up
    base = ["journalctl", "--unit", HUB_UNIT, "--output", "json", "--no-pager"]
    proc = None
    try:
        # A running journalctl alone does not prove access (it can just print
        # an access warning and wait forever). Require a readable unit entry.
        check = subprocess.run(base + ["--lines=1"], capture_output=True, text=True, timeout=10)
        if check.returncode or check.stderr.strip():
            return cursor
        entry = json.loads(check.stdout.strip())
        if not entry.get("__CURSOR") or "MESSAGE" not in entry:
            return cursor
        if cursor is None:
            cursor = entry["__CURSOR"]
        proc = subprocess.Popen(base + ["--follow", "--no-tail", "--after-cursor", cursor],
                                stdout=subprocess.PIPE, text=True, bufsize=1)
        with _journal_lock:
            _journal_watch_up = 1
        assert proc.stdout is not None
        for line in proc.stdout:
            entry = json.loads(line)
            next_cursor = entry["__CURSOR"]
            if next_cursor != cursor:
                record_journal_line(entry.get("MESSAGE", ""))
                cursor = next_cursor
    except Exception:
        pass
    finally:
        with _journal_lock:
            _journal_watch_up = 0
        if proc is not None:
            try:
                proc.terminate()
                proc.wait(timeout=2)
            except Exception:
                proc.kill()
                proc.wait()
    return cursor


def _journal_loop():
    cursor = None
    while True:
        cursor = _journal_follow_once(cursor)
        time.sleep(5)

def journal_metrics():
    with _journal_lock:
        up = _journal_watch_up
        counts = dict(_journal_counts)
    out = [
        "# HELP nativelink_hub_journal_watch_up 1 if the journalctl follower on the hub unit is running.",
        "# TYPE nativelink_hub_journal_watch_up gauge",
        f"nativelink_hub_journal_watch_up {up}",
    ]
    for name, help_, _ in JOURNAL_METRICS:
        out += [f"# HELP {name} {help_}", f"# TYPE {name} counter", f"{name} {counts[name]}"]
    return out

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        sections = cache() + eviction() + status_metrics() + journal_metrics()
        body = (render() + "\n".join(sections) + "\n").encode()
        self.send_response(200); self.send_header("Content-Type", "text/plain; version=0.0.4")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass

if __name__ == "__main__":
    threading.Thread(target=_watch, name="cas-watch", daemon=True).start()
    threading.Thread(target=_scan_loop, name="cas-scan", daemon=True).start()
    threading.Thread(target=_status_probe_loop, name="status-probe", daemon=True).start()
    threading.Thread(target=_journal_loop, name="journal-watch", daemon=True).start()
    HTTPServer(("127.0.0.1", 9108), H).serve_forever()
