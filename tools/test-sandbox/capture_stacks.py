"""Capture a terminating test group's threads without signalling the test."""

import concurrent.futures
import fcntl
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import time


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
    prefix = f"timeout stacks pid={pid}"
    lines = [f"{prefix} command={command}"]
    try:
        text = report.read_text(errors="replace")
    except OSError:
        return
    threads = []
    for line in text.splitlines():
        stripped = line.strip()
        if re.search(r"\bThread_\w+", stripped):
            threads.append([stripped])
        elif stripped.startswith(("Total number", "Sort by", "Binary Images")):
            break
        elif threads and stripped:
            threads[-1].append(stripped)
    # sample prints callers before leaf frames. Bound each thread separately
    # so a deep harness wait does not hide the blocked test's name.
    for thread in threads[:32]:
        lines.append(f"{prefix} {thread[0]}")
        lines.extend(f"{prefix}   {frame}" for frame in thread[1:][-4:])
    sys.stderr.write("\n".join(lines) + "\n")
    sys.stderr.flush()


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
        # The wrapper binds this one-shot hook to the supervisor's private
        # directory; a successor captures normally after this holder dies.
        hold = os.environ.get("MONO_CAPTURE_TEST_HOLD")
        if hold and Path(hold).exists():
            Path(hold).unlink()
            Path(hold + ".paused").write_text(f"{os.getpid()}\n")
            while True:
                time.sleep(0.05)
        members = group_members(group, ps_runtime)
        capture_members(members, output, sample_runtime, group)
        finish_capture(output, lock)
        lock = None
    except (OSError, subprocess.SubprocessError) as error:
        print(f"could not enumerate test process group {group}: {error}", file=sys.stderr, flush=True)
    finally:
        if lock is not None:
            lock.close()


def wait_until_deadline_or_wrapper_exit(delay, wrapper_pid):
    """True if the wrapper is still alive at the deadline; False if it died first."""
    deadline = time.monotonic() + delay
    while time.monotonic() < deadline:
        try:
            os.kill(wrapper_pid, 0)
        except OSError:
            return False
        time.sleep(0.2)
    try:
        os.kill(wrapper_pid, 0)
    except OSError:
        return False
    return True


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "--before-deadline":
        delay = int(sys.argv[2])
        wrapper_pid = int(sys.argv[3])
        sys.argv = [sys.argv[0]] + sys.argv[4:]
        if not wait_until_deadline_or_wrapper_exit(delay, wrapper_pid):
            sys.exit(0)
        print("capturing test thread backtraces before deadline", file=sys.stderr, flush=True)
    main()
