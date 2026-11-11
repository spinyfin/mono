"""Owning-host process inspection, also shipped verbatim over legacy SSH.

Only exact inherited execution identifiers authorize signals. Cwd is diagnostic
evidence, never permission to kill. No command lines or environments are logged.
"""

import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import signal
import shlex
import struct
import sys
import time
import tempfile


def registry():
    default = str(Path(os.environ.get("TEST_TMPDIR", "/tmp")) / f"boss-worker-processes-{os.geteuid()}")
    return Path(os.environ.get("BOSS_PROCESS_ROOT", default))


def private_registry():
    root = registry()
    root.mkdir(mode=0o700, exist_ok=True)
    metadata = root.lstat()
    if root.is_symlink() or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
        raise PermissionError("worker process registry must be private to the owning user")
    return root


def registered_environment(pid, birth):
    try:
        record = json.loads((registry() / f"{pid}.json").read_text())
        expected = list(birth) if isinstance(birth, tuple) else birth
        if record["birth"] == expected:
            return [os.fsencode(key + "=" + value) for key, value in record["identity"].items()]
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        # Missing, truncated or foreign records mean "unregistered". One bad
        # file must not fail every cleanup on the host.
        pass
    return []


def register(pid):
    identity = {key: os.environ[key] for key in ("BOSS_RUN_ID", "BOSS_SPAWN_TOKEN") if os.environ.get(key)}
    if not identity:
        return
    private_registry()
    birth, _, _, _, _ = inspect(pid)
    record_identity(pid, birth, identity)


def record_identity(pid, birth, identity):
    root = private_registry()
    previous = dict(os.fsdecode(entry).split("=", 1) for entry in registered_environment(pid, birth))
    previous.update(identity)
    with tempfile.NamedTemporaryFile(mode="w", dir=root, delete=False) as output:
        json.dump({"birth": birth, "identity": previous}, output)
    os.replace(output.name, root / f"{pid}.json")


def original_startup(name):
    """The caller's value, ignoring one of our own generated hooks (nested install)."""
    value = os.environ.get(name, "")
    return "" if value.startswith(str(registry()) + os.sep) else value


def install(source):
    original_zdotdir, original_bash_env = original_startup("ZDOTDIR"), original_startup("BASH_ENV")
    key = "\0".join((source, original_zdotdir, original_bash_env))
    root = private_registry() / hashlib.sha256(key.encode()).hexdigest()
    root.mkdir(mode=0o700, exist_ok=True)
    script = root / "processes.py"
    if not script.exists():
        with tempfile.NamedTemporaryFile(mode="w", dir=root, delete=False) as output:
            output.write(source)
        os.replace(output.name, script)
    # Registration failure must never stop the shell: an unregistered process
    # inside the workspace is still an occupant that keeps its lease.
    register_line = (f'python3 {shlex.quote(str(script))} --register "$$" || '
                     'echo "boss: worker process registration failed; continuing unregistered" >&2\n')
    bash_env = register_line
    if original_bash_env:
        original = shlex.quote(original_bash_env)
        bash_env += f'if [ -r {original} ]; then . {original}; fi\n'
    # Hand startup back to the user's own zsh files: restore ZDOTDIR before zsh
    # looks for .zprofile/.zshrc/.zlogin, then run the user's .zshenv.
    zshenv = register_line
    zshenv += f'ZDOTDIR={shlex.quote(original_zdotdir)}\n' if original_zdotdir else 'unset ZDOTDIR\n'
    zshenv += 'if [ -r "${ZDOTDIR:-$HOME}/.zshenv" ]; then . "${ZDOTDIR:-$HOME}/.zshenv"; fi\n'
    for name, startup in (("bash-env", bash_env), (".zshenv", zshenv)):
        with tempfile.NamedTemporaryFile(mode="w", dir=root, delete=False) as output:
            output.write(startup)
        os.replace(output.name, root / name)
    return {"BASH_ENV": str(root / "bash-env"), "ZDOTDIR": str(root), "BOSS_PROCESS_SCRIPT": str(script)}


def darwin_call(name, *args):
    library = ctypes.CDLL(None, use_errno=True)
    result = getattr(library, name)(*args)
    if result <= 0:
        raise OSError(ctypes.get_errno(), name)
    return result


def pids():
    if sys.platform == "darwin":
        size = darwin_call("proc_listpids", 4, os.geteuid(), None, 0)
        buffer = ctypes.create_string_buffer(size + 4096)
        size = darwin_call("proc_listpids", 4, os.geteuid(), buffer, len(buffer))
        return [pid[0] for pid in struct.iter_unpack("i", buffer.raw[:size]) if pid[0] > 1]
    found = []
    for path in Path("/proc").iterdir():
        try:
            # The PID namespace's init predates launched workers and cannot be
            # one of their descendants. Like Darwin's scan, exclude it: Linux
            # containers can expose its uid while denying access to its cwd.
            if path.name.isdigit() and int(path.name) > 1 and path.stat().st_uid == os.geteuid():
                found.append(int(path.name))
        except FileNotFoundError:
            pass
    return found


def birth_and_parent(pid):
    if sys.platform == "darwin":
        info = ctypes.create_string_buffer(136)
        darwin_call("proc_pidinfo", pid, 3, 0, info, len(info))
        if struct.unpack_from("I", info, 4)[0] == 5:  # SZOMB
            raise ProcessLookupError(errno.ESRCH, "zombie")
        birth = struct.unpack_from("QQ", info, 120)
        parent = struct.unpack_from("I", info, 16)[0]
    else:
        fields = (Path("/proc") / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()
        if fields[0] in ("Z", "X"):
            raise ProcessLookupError(errno.ESRCH, "zombie")
        birth, parent = (Path("/proc/sys/kernel/random/boot_id").read_text().strip(), fields[19]), int(fields[1])
    return birth, parent


def inspect(pid):
    birth, parent = birth_and_parent(pid)
    if sys.platform == "darwin":
        cwd_info = ctypes.create_string_buffer(2352)
        darwin_call("proc_pidinfo", pid, 9, 0, cwd_info, len(cwd_info))
        cwd = os.fsdecode(cwd_info.raw[152:1176].split(b"\0", 1)[0])
        mib = (ctypes.c_int * 2)(1, 8)  # CTL_KERN, KERN_ARGMAX
        capacity = ctypes.c_int()
        size = ctypes.c_size_t(ctypes.sizeof(capacity))
        library = ctypes.CDLL(None, use_errno=True)
        if library.sysctl(mib, 2, ctypes.byref(capacity), ctypes.byref(size), None, 0):
            raise OSError(ctypes.get_errno(), "KERN_ARGMAX")
        buffer = ctypes.create_string_buffer(capacity.value)
        size = ctypes.c_size_t(len(buffer))
        mib = (ctypes.c_int * 3)(1, 49, pid)  # KERN_PROCARGS2
        if library.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0):
            error = ctypes.get_errno()
            if error not in (errno.EPERM, errno.EACCES):
                raise OSError(error, "KERN_PROCARGS2")
            # Protected programs can expose cwd but hide their environment.
            # Registration still proves ownership; otherwise an in-workspace
            # process is an unowned occupant and prevents release. A known
            # cwd elsewhere does not make this workspace's occupancy unknown.
            argv, environment = [], []
        else:
            argc = struct.unpack_from("i", buffer)[0]
            strings = buffer.raw[4:size.value].split(b"\0", 1)[1].lstrip(b"\0").split(b"\0")
            argv, environment = strings[:argc], strings[argc:]
    else:
        root = Path("/proc") / str(pid)
        cwd = os.readlink(root / "cwd")
        argv = (root / "cmdline").read_bytes().split(b"\0")
        try:
            environment = (root / "environ").read_bytes().split(b"\0")
        except PermissionError:
            environment = []
    environment += registered_environment(pid, birth)
    return birth, cwd, argv, environment, parent


def resident_bazel(argv, workspace, pid):
    """Require Bazel's server jar AND its exact workspace startup argument."""
    args = [os.fsdecode(arg) for arg in argv]
    server = bool(args) and args[0].startswith("bazel(") and any(
        flag == "-jar" and Path(value).name == "A-server.jar" for flag, value in zip(args, args[1:]))
    roots = [arg.partition("=")[2] for arg in args if arg.startswith("--workspace_directory=")]
    bases = [arg.partition("=")[2] for arg in args if arg.startswith("--output_base=")]
    if not server or not any(os.path.realpath(root) == workspace for root in roots):
        return False
    try:
        return any((Path(base) / "server/server.pid.txt").read_text().strip() == str(pid) for base in bases)
    except OSError:
        return False


def snapshot(key, value, workspace):
    private_registry()
    owned, occupants, unknown, owners = {}, [], [], {}
    lineage = {}
    needle = os.fsencode(key + "=" + value) if value else None
    for pid in pids():
        if pid == os.getpid():
            continue
        try:
            birth, cwd, argv, environment, parent = inspect(pid)
        except OSError as error:
            if error.errno not in (errno.ESRCH, errno.ENOENT):
                # libproc can return zero without ESRCH when a process exits
                # between the birth, cwd and argv queries. Confirm disappearance
                # instead of retaining that stale PID as an unreadable occupant.
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    continue
                except PermissionError:
                    pass
                unknown.append(pid)
            continue
        root = os.path.realpath(cwd)
        inside = workspace and (root == workspace or root.startswith(workspace + os.sep))
        bazel_root = workspace or root
        if resident_bazel(argv, bazel_root, pid):
            continue
        lineage[pid] = (parent, birth)
        if needle and needle in environment:
            owned[pid] = birth
        if inside:
            occupants.append(pid)
            for entry in environment:
                if entry.startswith(b"BOSS_RUN_ID="):
                    owners.setdefault(os.fsdecode(entry.partition(b"=")[2]), []).append(pid)
    # Retain proven descendants before TERM reparents them. Platform helpers
    # may hide their environment; parentage plus birth time supplies ownership.
    changed = True
    while changed:
        changed = False
        for pid, (parent, birth) in lineage.items():
            if pid not in owned and parent in owned:
                try:
                    if birth_and_parent(parent)[0] != owned[parent] or birth_and_parent(pid) != (birth, parent):
                        continue
                except OSError:
                    continue
                owned[pid] = birth
                record_identity(pid, birth, {key: value})
                changed = True
    return owned, occupants, unknown, owners


def signal_owned(owned, sig):
    signalled = []
    for pid, birth in owned.items():
        try:
            current, _ = birth_and_parent(pid)
            if current == birth:
                os.kill(pid, sig)
                signalled.append(pid)
        except OSError as error:
            if error.errno not in (errno.ESRCH, errno.ENOENT):
                raise
    return signalled


def shell_alive(key, value, pid):
    """True unless `pid` is provably not the shell that carried `key=value`.

    A bare kill(0) cannot tell a dead shell from an unrelated process that
    reused its PID. The exact identity (environment or birth-pinned
    registration) can: a process without it is not that shell.
    """
    needle = os.fsencode(key + "=" + value)
    try:
        environment = [entry for entry in inspect(pid)[3] if entry]
        # Platform binaries hide their environment: an empty one proves nothing.
        return needle in environment or not environment
    except OSError as error:
        if error.errno in (errno.ESRCH, errno.ENOENT):
            return False
        try:
            os.kill(pid, 0)
        except (ProcessLookupError, PermissionError):
            return False  # gone, or another user's process: never our shell
        return True  # present but unreadable: not proven dead


def settled_snapshot(key, value, workspace):
    result = snapshot(key, value, workspace)
    # Match the existing host occupancy retry cadence for fork/exit races.
    for _ in range(4):
        if not result[2]:
            break
        time.sleep(0.02)
        result = snapshot(key, value, workspace)
    return result


def run(key, value, workspace, grace=5.0):
    if key not in ("BOSS_RUN_ID", "BOSS_SPAWN_TOKEN"):
        raise ValueError("unsupported ownership key")
    workspace = os.path.realpath(workspace) if workspace else None
    started = time.monotonic()
    owned, occupants, unknown, owners = settled_snapshot(key, value, workspace)
    terminated, killed = set(), set()
    if value:
        deadline = time.monotonic() + grace
        while owned and time.monotonic() < deadline:
            terminated.update(signal_owned(owned, signal.SIGTERM))
            time.sleep(0.02)
            owned, occupants, unknown, owners = snapshot(key, value, workspace)
        if owned:
            killed.update(signal_owned(owned, signal.SIGKILL))
            # Confirm exit, including children forked during TERM handling.
            deadline = time.monotonic() + grace
            while owned and time.monotonic() < deadline:
                killed.update(signal_owned(owned, signal.SIGKILL))
                time.sleep(0.02)
                owned, occupants, unknown, owners = snapshot(key, value, workspace)
    if unknown:
        owned, occupants, unknown, owners = settled_snapshot(key, value, workspace)
    return {"terminated": sorted(terminated), "killed": sorted(killed),
            "remaining": sorted(owned), "occupants": occupants, "unknown": unknown,
            "owners": owners, "elapsed_ms": int((time.monotonic() - started) * 1000)}


if __name__ == "__main__":
    if sys.argv[1] == "--register":
        try:
            register(int(sys.argv[2]))
        except (OSError, ValueError) as error:
            print(f"boss: worker process registration failed: {error}", file=sys.stderr)
            sys.exit(1)
    elif sys.argv[1] == "--install":
        print(json.dumps(install(sys.argv[2])))
    elif sys.argv[1] in ("--exec", "--exec-file"):
        source, command = (sys.argv[2], sys.argv[3]) if sys.argv[1] == "--exec" else (Path(__file__).read_text(), sys.argv[2])
        try:
            os.environ.update(install(source))
            register(os.getpid())
        except OSError as error:
            # Launch without hooks rather than not at all (read-only sandbox, full /tmp).
            print(f"boss: worker process registration unavailable: {error}", file=sys.stderr)
        os.execvpe("/bin/sh", ["/bin/sh", "-c", command], os.environ)
    elif sys.argv[1] == "--alive":
        print(json.dumps(shell_alive(sys.argv[2], sys.argv[3], int(sys.argv[4]))))
    elif sys.argv[1] == "--capture":
        snapshot(sys.argv[2], sys.argv[3], None)
    else:
        print(json.dumps(run(*sys.argv[1:])))
