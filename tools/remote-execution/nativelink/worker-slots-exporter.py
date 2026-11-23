#!/usr/bin/env python3
"""Exposes each worker's slot count and slots in use to Prometheus.

The NativeLink hub's own metrics do not say how many slots a worker offers or
how many are busy; its admin API does. On every scrape this asks the hub's
admin API (loopback, NL_HUB_ADMIN_LISTEN from hub.env) for the worker list and
prints it in Prometheus text format on 127.0.0.1:9108/metrics.
It also exports the hub's cache size and CAS eviction figures (see below).

Started by the user unit nativelink-worker-slots-exporter.service (the hub it
watches is a system unit). It only
reads; it never changes anything on the hub.
"""
import json, os, re, subprocess, urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

ADMIN = os.environ.get("NL_HUB_ADMIN_LISTEN", "127.0.0.1:50071")
URL = f"http://{ADMIN}/admin/scheduler/MAIN_SCHEDULER/workers"

def render():
    out = ["# HELP nativelink_pool_admin_up 1 if the hub's admin API answered.",
           "# TYPE nativelink_pool_admin_up gauge"]
    try:
        with urllib.request.urlopen(URL, timeout=5) as r:
            workers = json.load(r)
    except Exception:
        return "\n".join(out + ["nativelink_pool_admin_up 0", ""])
    out.append("nativelink_pool_admin_up 1")
    rows = {"slots": [], "running": [], "paused": [], "draining": []}
    for w in workers:
        m = re.match(r"^([a-z]+)-", w.get("id", ""))
        host = m.group(1) if m else "unknown"
        # The operating system the worker declares (OSFamily). The one hub
        # serves macOS and Linux workers and routes on it, so the two are
        # separate pools as far as capacity goes.
        osf = (w.get("platform_properties") or {}).get("OSFamily") or "unknown"
        osf = osf if re.fullmatch(r"[a-z0-9_-]+", osf) else "unknown"
        key = (host, osf)
        rows["slots"].append((key, w.get("max_inflight_tasks", 0)))
        rows["running"].append((key, w.get("running_actions", 0)))
        rows["paused"].append((key, 1 if w.get("is_paused") else 0))
        rows["draining"].append((key, 1 if w.get("is_draining") else 0))
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

threading.Thread(target=_watch, name="cas-watch", daemon=True).start()
threading.Thread(target=_scan_loop, name="cas-scan", daemon=True).start()

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = (render() + "\n".join(cache() + eviction()) + "\n").encode()
        self.send_response(200); self.send_header("Content-Type", "text/plain; version=0.0.4")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass

HTTPServer(("127.0.0.1", 9108), H).serve_forever()
