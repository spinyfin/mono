#!/usr/bin/python3 -I
"""Root-owned, fixed-destination Prometheus config publisher; accepts YAML on stdin."""

import fcntl
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import urllib.request

# Installed beside this file in a root-owned directory, never from a checkout.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from root_fs import replace, trusted_directory
from validate import CONFIG_PATH, run_promtool, validate_config

PROMTOOL_BIN = "/usr/local/bin/promtool"
METRICS_URL = "http://127.0.0.1:9090/metrics"


def ensure_can_reload(run=subprocess.run):
    result = run(
        ["/usr/bin/systemctl", "show", "-p", "CanReload", "--value", "prometheus.service"],
        check=True,
        capture_output=True,
        text=True,
    )
    if result.stdout.strip() != "yes":
        raise RuntimeError(
            "prometheus.service does not support reload (CanReload != yes); define ExecReload in the unit first"
        )


def parse_reload_metrics(text):
    successful = None
    timestamp = None
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) < 2:
            continue
        if parts[0] == "prometheus_config_last_reload_successful":
            successful = float(parts[1])
        elif parts[0] == "prometheus_config_last_reload_success_timestamp_seconds":
            timestamp = float(parts[1])
    return successful, timestamp


def read_reload_metrics(url=METRICS_URL):
    with urllib.request.urlopen(url, timeout=2) as response:
        return parse_reload_metrics(response.read().decode("utf-8"))


def reload_and_verify(
    run_reload,
    fetch_metrics=read_reload_metrics,
    timeout_seconds=10.0,
    poll_interval=0.2,
    sleep=time.sleep,
    monotonic=time.monotonic,
):
    try:
        _, before_ts = fetch_metrics()
    except OSError:
        before_ts = None
    run_reload()
    deadline = monotonic() + timeout_seconds
    last_successful = None
    last_ts = None
    while True:
        try:
            last_successful, last_ts = fetch_metrics()
        except OSError:
            last_successful, last_ts = None, None
        if last_successful == 1.0 and (before_ts is None or (last_ts is not None and last_ts > before_ts)):
            return
        if monotonic() >= deadline:
            raise RuntimeError(
                f"Prometheus config reload did not succeed (successful={last_successful}, timestamp={last_ts}, before={before_ts})"
            )
        sleep(poll_interval)


def publish(contents, config_path, check_config, reload_service, owner=(0, 0)):
    validate_config(contents)
    fd, candidate = tempfile.mkstemp(prefix=".promtool-check-", suffix=".yml", dir=config_path.parent)
    try:
        with os.fdopen(fd, "w") as output:
            output.write(contents)
            output.flush()
        os.chmod(candidate, 0o644)
        check_config(Path(candidate))
    finally:
        if os.path.exists(candidate):
            os.unlink(candidate)

    marker = config_path.parent / ".reload-required"
    if not config_path.exists() or config_path.read_text() != contents:
        # Survives a failed reload so retrying the same publication retries it.
        marker.touch(mode=0o600)
    replace(config_path, contents, owner=owner)
    if marker.exists():
        reload_service()
        marker.unlink()
        return True
    return False


def main():
    if sys.argv[1:] or os.geteuid() != 0:
        sys.exit("installer requires root and accepts no arguments")
    payload = sys.stdin.buffer.read(262_145)
    if len(payload) > 262_144:
        sys.exit("prometheus.yml exceeds 256 KB")
    contents = payload.decode("utf-8")
    validate_config(contents)
    trusted_directory(CONFIG_PATH.parent)
    ensure_can_reload()
    with (CONFIG_PATH.parent / ".publish.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        reloaded = publish(
            contents,
            CONFIG_PATH,
            lambda path: run_promtool(path, promtool_bin=PROMTOOL_BIN),
            lambda: reload_and_verify(
                lambda: subprocess.run(["/usr/bin/systemctl", "reload", "prometheus.service"], check=True)
            ),
        )
        subprocess.run(["/usr/bin/systemctl", "is-active", "--quiet", "prometheus.service"], check=True)
    if reloaded:
        print("Published and reloaded Prometheus configuration")
    else:
        print("Prometheus configuration unchanged; no reload needed")


if __name__ == "__main__":
    main()
