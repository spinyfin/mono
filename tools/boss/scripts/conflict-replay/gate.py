#!/usr/bin/env python3
"""Build and test a sample of conflict states a candidate resolved.

`resolve.py run` leaves, for every state, the files each candidate
produced. This script takes a seeded random sample of the states in which
one candidate resolved every conflicted file and, for each one:

  1. checks out the merged tree `git merge-tree` wrote for that state and
     replaces the conflicted files with the candidate's output;
  2. formats the Rust files with the rustfmt invocation the engine's
     resolver would run there, obtained from `keep_both_replay
     --format-command` (a rejection here is a decline in production);
  3. finds the Bazel targets that own each resolved file, plus the unit
     test targets of those crates;
  4. `bazel build`s and `bazel test`s them.

Failing tests are run up to three times before they count. A state that
still fails is built again from the resolution that was actually pushed
for that conflict, on the same targets. A failure that the pushed
resolution shares is attributed to the environment (an old revision that
no longer builds on this host, a test that fails either way), not to the
candidate.

Old revisions pin whatever Rust release `main` used at the time. When
that is not the release the checkout this script lives in pins, the
historical tree is built with the current one instead, for the candidate's
tree and the pushed resolution alike: a host can only be relied on to run
the toolchain its repository uses today. The swap is recorded per state.

`--candidate` picks whose output is gated: `items` (the default, the
scope the engine ships), `lines` (the per-path rule scope) or `mergiraf`.
Only Rust targets are built, so a state with no Rust file is reported as
`no_targets`.

Everything happens in a `git worktree` under `--work-dir`, shared by all
candidates so they share one Bazel output base. Runs are resumable: a
state already present in the candidate's `results.jsonl` is skipped, and
the latest record for a state is the one that counts.

Subcommands:

  sample   Pick the states (writes `sample.json`).
  run      Build and test them (appends to `results.jsonl`).
  report   Print the outcome table as Markdown.

The files of `items` are directly under `gate/`; another candidate's are
under `gate/<candidate>/`.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

# The sibling imports below would otherwise leave a `__pycache__`
# directory in the checkout this script is run from.
sys.dont_write_bytecode = True

from corpus import git
from resolve import load_results, load_states


TEST_ATTEMPTS = 3

CANDIDATES = {
    "items": "keep-both (items)",
    "lines": "keep-both (whole lines)",
    "mergiraf": "Mergiraf",
}


def gate_dir(opts) -> Path:
    path = opts.work_dir / "gate"
    path.mkdir(parents=True, exist_ok=True)
    return path


def candidate_dir(opts) -> Path:
    """Where one candidate keeps its sample, results and logs."""
    path = gate_dir(opts) if opts.candidate == "items" else gate_dir(opts) / opts.candidate
    path.mkdir(parents=True, exist_ok=True)
    return path


def cmd_sample(opts) -> None:
    by_state: dict[int, list[dict]] = defaultdict(list)
    for record in load_results(opts.work_dir):
        by_state[record["state"]].append(record)
    eligible = sorted(
        state for state, rows in by_state.items() if all(r.get(opts.candidate, {}).get("resolved") for r in rows)
    )
    chosen = sorted(random.Random(opts.seed).sample(eligible, min(opts.count, len(eligible))))
    out = candidate_dir(opts) / "sample.json"
    out.write_text(json.dumps({"seed": opts.seed, "eligible": len(eligible), "states": chosen}))
    print(f"sampled {len(chosen)} of {len(eligible)} fully resolved states (seed {opts.seed}) -> {out}")


def bazel(worktree: Path, *args: str, log: Path | None = None) -> tuple[int, str, float]:
    started = time.time()
    proc = subprocess.run(["bazel", *args], cwd=worktree, capture_output=True, text=True)
    output = proc.stdout + proc.stderr
    if log is not None:
        log.write_text(output)
    return proc.returncode, proc.stdout, time.time() - started


def query(worktree: Path, expression: str) -> list[str]:
    code, stdout, _ = bazel(worktree, "query", "--noshow_progress", "--keep_going", expression)
    return sorted(line for line in stdout.splitlines() if line.startswith("//")) if code in (0, 3) else []


def owning_package(worktree: Path, path: str) -> str | None:
    directory = (worktree / path).parent
    while directory != worktree.parent:
        if (directory / "BUILD.bazel").exists() or (directory / "BUILD").exists():
            return str(directory.relative_to(worktree))
        directory = directory.parent
    return None


def targets_for(worktree: Path, paths: list[str]) -> tuple[list[str], list[str]]:
    """The Rust targets that compile `paths`, and the tests to run for them.

    Tests are the owning targets that are themselves tests plus every
    `rust_test` whose `crate` is an owning library (its unit tests).
    Targets tagged `manual` are left out, as `bazel test //...` leaves
    them out.
    """
    build: set[str] = set()
    tests: set[str] = set()
    for path in paths:
        package = owning_package(worktree, path)
        if package is None:
            continue
        label = f"//{package}:{Path(path).relative_to(package)}"
        scope = f"//{package}:all"
        owners = query(worktree, f'kind("rust_.* rule", rdeps({scope}, {label}, 1)) except attr(tags, "manual", {scope})')
        build.update(owners)
        tests.update(query(worktree, f'kind("rust_test rule", set({" ".join(owners)}))') if owners else [])
        for owner in owners:
            name = owner.rsplit(":", 1)[1]
            tests.update(
                query(
                    worktree,
                    f'kind("rust_test rule", attr(crate, ":{name}$", {scope})) except attr(tags, "manual", {scope})',
                )
            )
    return sorted(build | tests), sorted(tests)


def pinned_rust(checkout_root: Path) -> str | None:
    toolchain = checkout_root / "rust-toolchain.toml"
    if not toolchain.exists():
        return None
    match = re.search(r'^channel\s*=\s*"([^"]+)"', toolchain.read_text(), re.MULTILINE)
    return match.group(1) if match else None


HOST_RUST = pinned_rust(Path(__file__).resolve().parents[4])


def checkout(worktree: Path, tree: str) -> str | None:
    """Checks `tree` out and moves its Rust pin to the host's.

    Returns "<old> -> <new>" when the pin was moved.
    """
    proc = subprocess.run(["git", "-C", str(worktree), "read-tree", "-u", "--reset", tree], capture_output=True, text=True)
    if proc.returncode != 0:
        raise SystemExit(f"cannot check out {tree}: {proc.stderr}")
    old = pinned_rust(worktree)
    if not old or not HOST_RUST or old == HOST_RUST:
        return None
    for name in ("rust-toolchain.toml", "MODULE.bazel"):
        target = worktree / name
        if target.exists():
            target.write_text(target.read_text().replace(f'"{old}"', f'"{HOST_RUST}"'))
    return f"{old} -> {HOST_RUST}"


def build_and_test(worktree: Path, paths: list[str], logs: Path, tag: str) -> dict:
    build, tests = targets_for(worktree, paths)
    result: dict = {"build_targets": build, "test_targets": tests}
    if not build:
        result["outcome"] = "no_targets"
        return result
    code, _, seconds = bazel(worktree, "build", "--keep_going", *build, log=logs / f"{tag}-build.log")
    result["build_seconds"] = round(seconds)
    if code != 0:
        result["outcome"] = "build_failed"
        return result
    if tests:
        # A shard that times out on a loaded host is not a verdict. Bazel
        # keeps the shards that passed, so a retry reruns only the rest.
        for attempt in range(1, TEST_ATTEMPTS + 1):
            log = logs / f"{tag}-test-{attempt}.log"
            code, _, seconds = bazel(worktree, "test", "--keep_going", "--test_output=errors", *tests, log=log)
            result["test_seconds"] = result.get("test_seconds", 0) + round(seconds)
            result["test_attempts"] = attempt
            if code == 0:
                break
        if code != 0:
            result["outcome"] = "test_failed"
            return result
    result["outcome"] = "passed"
    return result


def cmd_run(opts) -> None:
    root = candidate_dir(opts)
    repo = opts.work_dir / "repo.git"
    worktree = gate_dir(opts) / "worktree"
    logs = root / "logs"
    logs.mkdir(exist_ok=True)
    states = load_states(opts.work_dir)
    sample = json.loads((root / "sample.json").read_text())["states"]
    results_path = root / "results.jsonl"
    latest = latest_records(results_path)
    if not worktree.exists():
        git(repo, "worktree", "add", "--detach", str(worktree), states[sample[0]]["main"])

    # Newest first: neighbouring revisions share the most build outputs.
    pending = [
        i
        for i in sorted(sample, key=lambda i: states[i]["pushed_at"], reverse=True)
        if i not in latest or (opts.retry_unjudged and not judged(latest[i]))
    ]
    if opts.only:
        pending = [i for i in pending if i in opts.only]
    for index in pending[: opts.limit]:
        state = states[index]
        paths = [entry["path"] for entry in state["files"]]
        record = {"state": index, "pr": state["pr"], "pushed_at": state["pushed_at"], "files": paths}
        swapped = checkout(worktree, state["tree"])
        if swapped:
            record["rust_toolchain"] = swapped
        formatted = True
        for path in paths:
            resolved = opts.work_dir / "resolved" / str(index) / opts.candidate / path
            (worktree / path).write_bytes(resolved.read_bytes())
            if not path.endswith(".rs"):
                continue
            command = json.loads(
                subprocess.run(
                    [str(opts.union_bin), "--path", path, "--format-command", str(worktree)],
                    capture_output=True,
                    check=True,
                ).stdout
            )
            proc = subprocess.run(command, cwd=worktree, capture_output=True, text=True)
            if proc.returncode != 0:
                formatted = False
                record["rustfmt_error"] = proc.stderr.strip().splitlines()[:3]
        if not formatted:
            record["outcome"] = "rustfmt_rejected"
        else:
            record.update(build_and_test(worktree, paths, logs, f"{index}-{opts.candidate}"))
            if record["outcome"] in ("build_failed", "test_failed"):
                # Would the resolution that was actually pushed have fared
                # any better on this host today?
                checkout(worktree, f"{state['after']}^{{tree}}")
                control = build_and_test(worktree, paths, logs, f"{index}-pushed")
                record["pushed_resolution"] = control
        with results_path.open("a") as out:
            out.write(json.dumps(record) + "\n")
        print(f"state {index} (PR {state['pr']}): {record['outcome']}", file=sys.stderr, flush=True)


def latest_records(results_path: Path) -> dict[int, dict]:
    latest: dict[int, dict] = {}
    if results_path.exists():
        for line in results_path.read_text().splitlines():
            record = json.loads(line)
            latest[record["state"]] = record
    return latest


def judged(record: dict) -> bool:
    """Whether the record says something about the candidate rather than about the host."""
    if record["outcome"] in ("passed", "rustfmt_rejected"):
        return True
    return record.get("pushed_resolution", {}).get("outcome") == "passed"


def verdict(record: dict) -> str:
    outcome = record["outcome"]
    if outcome in ("passed", "rustfmt_rejected", "no_targets"):
        return outcome
    control = record.get("pushed_resolution", {}).get("outcome")
    if control == "passed":
        return f"{outcome}, pushed resolution passes"
    return f"{outcome}, pushed resolution also fails"


def cmd_report(opts) -> None:
    root = candidate_dir(opts)
    sample = json.loads((root / "sample.json").read_text())
    records = list(latest_records(root / "results.jsonl").values())
    print(
        f"Sample: {len(sample['states'])} of the {sample['eligible']} states in which {CANDIDATES[opts.candidate]} "
        f"resolved every conflicted file, seed {sample['seed']}; {len(records)} built so far.\n"
    )
    print("| Outcome | States |")
    print("| --- | ---: |")
    for outcome, count in Counter(verdict(r) for r in records).most_common():
        print(f"| {outcome} | {count} |")
    passed = sum(1 for r in records if r["outcome"] == "passed")
    with_verdict = sum(1 for r in records if judged(r))
    if with_verdict:
        print(f"\nGated pass rate: {passed} of {with_verdict} ({100 * passed / with_verdict:.0f}%) states with a verdict.")
    swapped = sum(1 for r in records if "rust_toolchain" in r)
    if swapped:
        print(f"\n{swapped} states were built with the host's Rust release in place of the one their revision pinned.")
    if opts.details:
        print("\n| State | PR | Pushed | Rust | Outcome | Files |")
        print("| ---: | ---: | --- | --- | --- | --- |")
        for r in sorted(records, key=lambda r: r["pushed_at"]):
            files = ", ".join(f"`{path}`" for path in r["files"])
            rust = r.get("rust_toolchain", "as pinned")
            print(f"| {r['state']} | #{r['pr']} | {r['pushed_at'][:10]} | {rust} | {verdict(r)} | {files} |")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--work-dir", type=Path, required=True, help="the directory `corpus.py` and `resolve.py` filled")
    parser.add_argument(
        "--candidate",
        choices=sorted(CANDIDATES),
        default="items",
        help="whose resolutions to gate (default: items, the scope the engine ships)",
    )
    sub = parser.add_subparsers(dest="command", required=True)
    sample = sub.add_parser("sample")
    sample.add_argument("--count", type=int, default=50)
    sample.add_argument("--seed", type=int, default=20261111)
    sample.set_defaults(func=cmd_sample)
    run_parser = sub.add_parser("run")
    run_parser.add_argument("--union-bin", type=Path, required=True, help="path to the built keep_both_replay binary")
    run_parser.add_argument("--limit", type=int, default=None, help="stop after this many states (the rest resume later)")
    run_parser.add_argument(
        "--retry-unjudged",
        action="store_true",
        help="build again the states whose last run failed for the pushed resolution too",
    )
    run_parser.add_argument("--only", type=int, nargs="*", help="restrict the run to these state numbers")
    run_parser.set_defaults(func=cmd_run)
    report = sub.add_parser("report")
    report.add_argument("--details", action="store_true", help="also list every state")
    report.set_defaults(func=cmd_report)
    opts = parser.parse_args()
    opts.work_dir = opts.work_dir.resolve()
    opts.func(opts)


if __name__ == "__main__":
    main()
