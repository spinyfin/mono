"""Root-owned filesystem primitives shared by NativeLink publishers (stdlib only)."""

import os
import stat
import tempfile


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
