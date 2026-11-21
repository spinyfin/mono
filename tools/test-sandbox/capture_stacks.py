"""Capture a terminating test group's threads without signalling the test."""

import concurrent.futures
import os
from pathlib import Path
import subprocess
import sys


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


def capture(pid, command, output, runtime):
    path = output / f"threads-{pid}.txt"
    with path.open("w") as report:
        report.write(f"pid={pid} command={command}\n")
        report.flush()
        # A blocked stack needs only a few observations, not a profiler's
        # 1 kHz sampling load multiplied by every timed-out test process.
        args = [str(runtime / "sample"), str(pid), "1", "100", "-file", "/dev/stdout"]
        try:
            # Bound only the diagnostic tool, not the test. A broken sampler
            # must not consume Bazel's termination grace or prevent cleanup.
            result = subprocess.run(args, stdout=report, stderr=subprocess.STDOUT, timeout=5)
            report.write(f"\nstack capture exit={result.returncode}\n")
        except (OSError, subprocess.TimeoutExpired) as error:
            report.write(f"\nstack capture unavailable: {error}\n")
    print(f"test thread backtraces: {path}", file=sys.stderr, flush=True)


def main():
    group = int(sys.argv[1])
    runtime = Path(sys.argv[2])
    output = Path(os.environ["TEST_UNDECLARED_OUTPUTS_DIR"]) / f"timeout-{group}"
    # Atomic claim also prevents duplicate capture if cleanup follows a TERM
    # trap, or if a wrapper watchdog takes over during termination.
    try:
        output.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        return
    members = group_members(group, runtime)
    (output / "processes.txt").write_text(
        f"test={os.environ.get('TEST_TARGET', 'unknown')} group={group}\n"
        + "".join(f"{pid} {command}\n" for pid, command in members)
    )
    # Each sample captures every thread. Sample members concurrently so a
    # subprocess blocked below the test harness is included before cleanup.
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, len(members))) as pool:
        list(pool.map(lambda member: capture(*member, output, runtime), members))


if __name__ == "__main__":
    main()
