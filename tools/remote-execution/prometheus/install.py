#!/usr/bin/python3 -I
"""Root-owned, fixed-destination Prometheus config publisher; accepts YAML on stdin."""

import fcntl
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile

# Installed beside this file in a root-owned directory, never from a checkout.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from validate import CONFIG_PATH, run_promtool, validate_config

PROMTOOL_BIN = "/usr/local/bin/promtool"


def trusted_directory(path):
    for parent in (path, *path.parents):
        info = parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError(f"directory must be root-owned and not group/world writable: {parent}")


def replace(path, contents, owner=(0, 0)):
    if path.is_symlink():
        raise ValueError(f"refusing symlink: {path}")
    if path.exists() and path.read_text() == contents:
        info = path.stat()
        if (info.st_uid, info.st_gid, stat.S_IMODE(info.st_mode)) == (*owner, 0o644):
            return False
    fd, temporary = tempfile.mkstemp(prefix=".publish-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as output:
            output.write(contents)
            output.flush()
            os.fchown(output.fileno(), *owner)
            os.fchmod(output.fileno(), 0o644)
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return True


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
    with (CONFIG_PATH.parent / ".publish.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        reloaded = publish(
            contents,
            CONFIG_PATH,
            lambda path: run_promtool(path, promtool_bin=PROMTOOL_BIN),
            lambda: subprocess.run(["/usr/bin/systemctl", "reload", "prometheus.service"], check=True),
        )
        subprocess.run(["/usr/bin/systemctl", "is-active", "--quiet", "prometheus.service"], check=True)
    if reloaded:
        print("Published and reloaded Prometheus configuration")
    else:
        print("Prometheus configuration unchanged; no reload needed")


if __name__ == "__main__":
    main()
