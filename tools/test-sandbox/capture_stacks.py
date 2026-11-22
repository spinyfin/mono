"""Capture a terminating test group's threads without signalling the test."""

import concurrent.futures
import fcntl
import os
from pathlib import Path
import subprocess
import sys
import tempfile


COMPLETE = "complete"
LOCK = "capture.lock"


def group_members(group, runtime):
    result = subprocess.run(
        [str(runtime / "ps"), "-axo", "pid=,pgid=,comm="],
        check=True, capture_output=True, text=True, timeout=5,
    )
    members = []
    for line in result.stdout.splitlines():
        fields = line.split(None, 2)
        if len(fields) == 3 and int(fields[1]) == group:
            members.append((int(fields[0]), fields[2]))
    return members


def summarize(pid, command, report):
    """Compact per-thread lines for test.log if undeclared outputs are dropped."""
    print(f"timeout stacks pid={pid} command={command}", file=sys.stderr, flush=True)
    try:
        text = report.read_text(errors="replace")
    except OSError:
        return
    printed = 0
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if printed == 0 and "Call graph:" not in stripped and "Thread_" not in stripped:
            continue
        if "Call graph:" in stripped or "Thread_" in stripped or printed:
            print(f"timeout stacks   {stripped}", file=sys.stderr, flush=True)
            printed += 1
            if printed >= 24:
                break


def capture(pid, command, output, sample_runtime):
    path = output / f"threads-{pid}.txt"
    with path.open("w") as report:
        report.write(f"pid={pid} command={command}\n")
        report.flush()
        # A blocked stack needs only a few observations, not a profiler's
        # 1 kHz sampling load multiplied by every timed-out test process.
        args = [str(sample_runtime / "sample"), str(pid), "1", "100", "-file", "/dev/stdout"]
        try:
            # Bound only the diagnostic tool, not the test. A broken sampler
            # must not consume Bazel's termination grace or prevent cleanup.
            result = subprocess.run(args, stdout=report, stderr=subprocess.STDOUT, timeout=5)
            report.write(f"\nstack capture exit={result.returncode}\n")
        except (OSError, subprocess.TimeoutExpired) as error:
            report.write(f"\nstack capture unavailable: {error}\n")
    print(f"test thread backtraces: {path}", file=sys.stderr, flush=True)
    summarize(pid, command, path)


def capture_members(members, output, sample_runtime, group):
    (output / "processes.txt").write_text(
        f"test={os.environ.get('TEST_TARGET', 'unknown')} group={group}\n"
        + "".join(f"{pid} {command}\n" for pid, command in members)
    )
    # Each sample captures every thread. Sample members concurrently so a
    # subprocess blocked below the test harness is included before cleanup.
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, len(members))) as pool:
        list(pool.map(lambda member: capture(*member, output, sample_runtime), members))


def undeclared_root():
    raw = os.environ.get("TEST_UNDECLARED_OUTPUTS_DIR")
    if not raw:
        return None
    path = Path(raw)
    return path if path.is_dir() else None


def begin_capture(output):
    """Exclusive in-progress lock. None if another live capturer owns it or it completed."""
    try:
        output.mkdir(parents=False, exist_ok=True)
    except FileNotFoundError:
        return None
    lock = (output / LOCK).open("a+")
    try:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        return None
    if (output / COMPLETE).exists():
        lock.close()
        return None
    return lock


def finish_capture(output, lock):
    (output / COMPLETE).write_text("1\n")
    lock.close()


def capture_to_stderr_only(group, ps_runtime, sample_runtime):
    try:
        members = group_members(group, ps_runtime)
    except (OSError, subprocess.SubprocessError) as error:
        print(f"could not enumerate test process group {group}: {error}", file=sys.stderr, flush=True)
        return
    with tempfile.TemporaryDirectory() as directory:
        capture_members(members, Path(directory), sample_runtime, group)


def main():
    group = int(sys.argv[1])
    ps_runtime = Path(sys.argv[2])
    sample_runtime = Path(sys.argv[3]) if len(sys.argv) > 3 else ps_runtime
    root = undeclared_root()
    if root is None:
        # Bazel may already have dropped the action outputs; keep a log line.
        capture_to_stderr_only(group, ps_runtime, sample_runtime)
        return
    output = root / f"timeout-{group}"
    lock = begin_capture(output)
    if lock is None:
        if not root.is_dir():
            capture_to_stderr_only(group, ps_runtime, sample_runtime)
        return
    try:
        members = group_members(group, ps_runtime)
        capture_members(members, output, sample_runtime, group)
        finish_capture(output, lock)
        lock = None
    except (OSError, subprocess.SubprocessError) as error:
        print(f"could not enumerate test process group {group}: {error}", file=sys.stderr, flush=True)
    finally:
        if lock is not None:
            lock.close()


if __name__ == "__main__":
    main()
