#!/usr/bin/python3 -I
"""Root-owned, fixed-destination publisher; accepts only JSON data on stdin."""

import fcntl
import os
from pathlib import Path
import pwd
import stat
import subprocess
import sys
import tempfile

# Installed beside this file in a root-owned directory, never from a checkout.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from validate import DASHBOARDS, DIRECTORY, PROVIDER, canonical, parse, validate_bundle


def trusted_directory(path):
    for parent in (path, *path.parents):
        info = parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError(f"directory must be root-owned and not group/world writable: {parent}")


def replace(path, contents, owner):
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


def publish(bundle, directory, provider, owner, restart):
    validate_bundle(bundle)  # Validate everything before any write.
    marker = directory / ".reload-required"
    for name in DASHBOARDS:
        replace(directory / name, canonical(bundle[name]), owner)
    desired = bundle[PROVIDER]
    if provider.is_symlink():
        raise ValueError("refusing provider symlink")
    if not provider.exists() or provider.read_text() != desired:
        # Survives a failed restart so retrying the same publication retries it.
        marker.touch(mode=0o600)
    replace(provider, desired, (0, owner[1]))
    if marker.exists():
        restart()
        marker.unlink()


def main():
    if sys.argv[1:] or os.geteuid() != 0:
        sys.exit("installer requires root and accepts no arguments")
    payload = sys.stdin.buffer.read(2_000_001)
    if len(payload) > 2_000_000:
        sys.exit("dashboard bundle exceeds 2 MB")
    bundle = parse(payload)
    validate_bundle(bundle)
    directory = Path(DIRECTORY)
    provider = Path("/etc/grafana/provisioning/dashboards/nativelink.yaml")
    trusted_directory(directory.parent)
    trusted_directory(provider.parent)
    directory.mkdir(mode=0o755, exist_ok=True)
    trusted_directory(directory)
    account = pwd.getpwnam("grafana")
    with (directory / ".publish.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        publish(bundle, directory, provider, (account.pw_uid, account.pw_gid),
                lambda: subprocess.run(["/usr/bin/systemctl", "restart", "grafana-server.service"], check=True))
        subprocess.run(["/usr/bin/systemctl", "is-active", "--quiet", "grafana-server.service"], check=True)
    print("Published NativeLink dashboards; Grafana polls them within 30 seconds")


if __name__ == "__main__":
    main()
