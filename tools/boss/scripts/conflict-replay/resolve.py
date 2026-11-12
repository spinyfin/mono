#!/usr/bin/env python3
"""Run candidate resolvers over the replayed conflict corpus and report.

For every content-conflicted file in `states.jsonl` (see `corpus.py`):

  * the keep-both resolver, through the `keep_both_replay` binary built
    from the engine's own resolver crate, in both scopes: `items` (whole
    Rust items and imports, what the engine ships) and `lines` (any
    self-contained run of whole lines, the per-path rule scope);
  * Mergiraf (https://mergiraf.org), given the three versions of the file.

Each resolved Rust file is then checked two ways without building
anything: rustfmt must accept it, and where the pushed resolution of that
exact conflict is known, the candidate is compared with it.

Subcommands:

  run       Resolve every file; writes `results.jsonl` and the resolved
            files under `resolved/`.
  report    Print the share of files and of whole conflict states each
            candidate resolves, as Markdown.
  jj-check  Re-create a sample of the conflicts with jj, which is what
            the engine resolves in production, and check that the
            keep-both resolver decides and writes the same thing from
            jj's conflict markers as it did from git's.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import subprocess
import sys
import tempfile
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

# The sibling import below would otherwise leave a `__pycache__` directory
# in the checkout this script is run from.
sys.dont_write_bytecode = True

from corpus import git, is_content_conflict

# Mirrors `is_generated_lockfile` in the resolver crate: files the engine
# already regenerates instead of merging.
LOCKFILES = {
    "Cargo.lock",
    "MODULE.bazel.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "uv.lock",
    "poetry.lock",
}
CANDIDATES = ("items", "lines", "mergiraf")
RUSTFMT_ARGS = ["--edition", "2024", "--config", "max_width=120,skip_children=true"]


def is_lockfile(path: str) -> bool:
    return os.path.basename(path) in LOCKFILES


def load_states(work_dir: Path) -> list[dict]:
    with (work_dir / "states.jsonl").open() as handle:
        return [json.loads(line) for line in handle]


def blob(repo: Path, spec: str) -> bytes | None:
    proc = git(repo, "cat-file", "blob", spec, check=False)
    return proc.stdout if proc.returncode == 0 else None


def rustfmt(source: bytes) -> bytes | None:
    """`source` formatted, or `None` when rustfmt does not accept it."""
    with tempfile.TemporaryDirectory() as tmp:
        target = Path(tmp) / "candidate.rs"
        target.write_bytes(source)
        proc = subprocess.run(["rustfmt", *RUSTFMT_ARGS, str(target)], capture_output=True)
        return target.read_bytes() if proc.returncode == 0 else None


def compare_with_pushed(candidate: bytes, pushed: bytes, rust: bool) -> str:
    """How a candidate relates to the resolution that was actually pushed."""
    if rust:
        candidate = rustfmt(candidate) or candidate
        pushed = rustfmt(pushed) or pushed
    if candidate == pushed:
        return "identical"
    if sorted(candidate.splitlines()) == sorted(pushed.splitlines()):
        return "same_lines_reordered"
    return "different"


def evaluate(output: bytes, path: str, pushed: bytes | None) -> dict:
    rust = path.endswith(".rs")
    verdict: dict = {}
    if rust:
        verdict["parses"] = rustfmt(output) is not None
    if pushed is not None:
        verdict["pushed"] = compare_with_pushed(output, pushed, rust)
    return verdict


def resolve_file(opts, index: int, state: dict, entry: dict) -> dict:
    repo = opts.work_dir / "repo.git"
    path = entry["path"]
    record = {"state": index, "pr": state["pr"], "path": path, "lockfile": is_lockfile(path)}
    if record["lockfile"] or not is_content_conflict(entry):
        record["content_conflict"] = is_content_conflict(entry)
        return record
    record["content_conflict"] = True

    # The pushed resolution answers this exact conflict only when `main`
    # did not change the file between the commit the push landed on and
    # the one replayed against.
    pushed = None
    if state["main"] == state["landed_on"] or blob(repo, f"{state['landed_on']}:{path}") == blob(
        repo, f"{state['main']}:{path}"
    ):
        pushed = blob(repo, f"{state['after']}:{path}")

    out_root = opts.work_dir / "resolved" / str(index)
    suffix = "".join(Path(path).suffixes[-1:]) or ".txt"
    with tempfile.TemporaryDirectory() as tmp:
        tmp_path = Path(tmp)
        conflicted = tmp_path / f"conflicted{suffix}"
        conflicted.write_bytes(blob(repo, f"{state['tree']}:{path}") or b"")
        for scope in ("items", "lines"):
            out_file = tmp_path / f"{scope}{suffix}"
            proc = subprocess.run(
                [str(opts.union_bin), "--path", path, "--scope", scope, "--input", str(conflicted), "--output", str(out_file)],
                capture_output=True,
            )
            if proc.returncode != 0:
                record[scope] = {"resolved": False, "reason": proc.stderr.decode("utf-8", "replace").strip()}
                continue
            verdict = json.loads(proc.stdout)
            if verdict["resolved"]:
                output = out_file.read_bytes()
                verdict.update(evaluate(output, path, pushed))
                save(out_root / scope / path, output)
            record[scope] = verdict

        if opts.mergiraf:
            versions = {}
            for stage, name in (("1", "base"), ("2", "main"), ("3", "head")):
                versions[name] = tmp_path / f"{name}{suffix}"
                versions[name].write_bytes(blob(repo, entry["stages"][stage]) or b"")
            out_file = tmp_path / f"mergiraf{suffix}"
            proc = subprocess.run(
                [
                    str(opts.mergiraf),
                    "merge",
                    str(versions["base"]),
                    str(versions["main"]),
                    str(versions["head"]),
                    "--path-name",
                    path,
                    "--output",
                    str(out_file),
                    "--timeout",
                    "30000",
                ],
                capture_output=True,
                # Mergiraf keeps a review copy of every merge under the
                # data directory; keep it out of the user's home.
                env={**os.environ, "XDG_DATA_HOME": str(opts.work_dir / "mergiraf-data")},
            )
            verdict = {"resolved": proc.returncode == 0}
            if verdict["resolved"]:
                output = out_file.read_bytes()
                verdict.update(evaluate(output, path, pushed))
                save(out_root / "mergiraf" / path, output)
            record["mergiraf"] = verdict
    return record


def save(target: Path, data: bytes) -> None:
    """Writes atomically, so a gate run reading `resolved/` never sees half a file."""
    target.parent.mkdir(parents=True, exist_ok=True)
    scratch = target.with_name(target.name + ".tmp")
    scratch.write_bytes(data)
    os.replace(scratch, target)


def cmd_run(opts) -> None:
    states = load_states(opts.work_dir)
    jobs = [(index, state, entry) for index, state in enumerate(states) for entry in state["files"]]
    with ThreadPoolExecutor(max_workers=opts.jobs) as pool:
        records = list(pool.map(lambda job: resolve_file(opts, *job), jobs))
    out_path = opts.work_dir / "results.jsonl"
    with out_path.open("w") as out:
        for record in records:
            out.write(json.dumps(record) + "\n")
    print(f"wrote {out_path} ({len(records)} conflicted files in {len(states)} states)")


def load_results(work_dir: Path) -> list[dict]:
    with (work_dir / "results.jsonl").open() as handle:
        return [json.loads(line) for line in handle]


def pct(part: int, whole: int) -> str:
    return f"{part} ({100 * part / whole:.1f}%)" if whole else "0"


def resolved_by(record: dict, candidate: str) -> bool:
    return bool(record.get(candidate, {}).get("resolved"))


def cmd_report(opts) -> None:
    records = load_results(opts.work_dir)
    by_state: dict[int, list[dict]] = defaultdict(list)
    for record in records:
        by_state[record["state"]].append(record)
    content = [r for r in records if r["content_conflict"] and not r["lockfile"]]
    rust = [r for r in content if r["path"].endswith(".rs")]
    lock_rows = [r for r in records if r["lockfile"]]
    other_rows = [r for r in records if not r["content_conflict"] and not r["lockfile"]]

    print("### Corpus\n")
    print(f"- Conflict states: {len(by_state)}")
    print(f"- Conflicted file rows: {len(records)}")
    print(f"  - lockfiles the engine regenerates: {len(lock_rows)}")
    print(f"  - add/add, modify/delete and rename conflicts: {len(other_rows)}")
    print(f"  - content conflicts outside lockfiles: {len(content)}, of which Rust: {len(rust)}\n")

    labels = {
        "items": "Keep-both, whole items and imports (shipped)",
        "lines": "Keep-both, any whole lines (per-path rule scope)",
        "mergiraf": "Mergiraf",
    }
    candidates = [c for c in CANDIDATES if any(c in r for r in content)]

    print("### Files resolved\n")
    print("| Candidate | Content-conflicted files | Rust files | rustfmt accepts the Rust result |")
    print("| --- | ---: | ---: | ---: |")
    for candidate in candidates:
        done = [r for r in content if resolved_by(r, candidate)]
        done_rust = [r for r in done if r["path"].endswith(".rs")]
        parses = sum(1 for r in done_rust if r[candidate].get("parses"))
        print(
            f"| {labels[candidate]} | {pct(len(done), len(content))} | {pct(len(done_rust), len(rust))} "
            f"| {pct(parses, len(done_rust))} |"
        )

    print("\n### Whole conflict states resolved\n")
    print("A state counts only when every conflicted file in it is resolved.\n")
    print("| Candidate | Alone | With lockfile regeneration |")
    print("| --- | ---: | ---: |")
    total = len(by_state)
    lock_only = sum(1 for rows in by_state.values() if all(r["lockfile"] for r in rows))
    print(f"| Lockfile regeneration only (existing tier) | | {pct(lock_only, total)} |")
    state_sets: dict[str, set[int]] = {}
    for candidate in candidates:
        alone = {s for s, rows in by_state.items() if all(resolved_by(r, candidate) for r in rows)}
        with_locks = {
            s
            for s, rows in by_state.items()
            if all(r["lockfile"] or resolved_by(r, candidate) for r in rows) and not all(r["lockfile"] for r in rows)
        }
        state_sets[candidate] = with_locks
        print(f"| {labels[candidate]} | {pct(len(alone), total)} | {pct(len(with_locks), total)} |")
    if "items" in state_sets and "mergiraf" in state_sets:
        both = state_sets["items"] & state_sets["mergiraf"]
        only_union = state_sets["items"] - state_sets["mergiraf"]
        only_mergiraf = state_sets["mergiraf"] - state_sets["items"]
        print(
            f"\nKeep-both (items) and Mergiraf, with lockfile regeneration: {len(both)} states resolved by both, "
            f"{len(only_union)} only by keep-both, {len(only_mergiraf)} only by Mergiraf."
        )

    print("\n### Agreement with the resolution that was pushed\n")
    print(
        "Counted over resolved files whose pushed resolution answers the same conflict "
        "(`main` did not change the file between the replayed commit and the one the push landed on).\n"
    )
    print("| Candidate | Comparable files | Identical | Same lines, other order | Different |")
    print("| --- | ---: | ---: | ---: | ---: |")
    for candidate in candidates:
        outcomes = Counter(r[candidate]["pushed"] for r in content if resolved_by(r, candidate) and "pushed" in r[candidate])
        comparable = sum(outcomes.values())
        print(
            f"| {labels[candidate]} | {comparable} | {pct(outcomes['identical'], comparable)} "
            f"| {pct(outcomes['same_lines_reordered'], comparable)} | {pct(outcomes['different'], comparable)} |"
        )

    for scope in ("items", "lines"):
        kinds = Counter(kind for r in content if resolved_by(r, scope) for kind in r[scope]["insertions"])
        if kinds:
            print(f"\nConflicts resolved by keep-both ({scope}), by kind: " + ", ".join(f"{k} {v}" for k, v in kinds.most_common()) + ".")

    if opts.reasons:
        print("\n### Why keep-both (items) declined\n")
        reasons = Counter(classify_reason(r["items"]["reason"]) for r in rust if not resolved_by(r, "items"))
        for reason, count in reasons.most_common(opts.reasons):
            print(f"- {count}: {reason}")


def jj(repo_dir: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    # Run from the repository root: jj reads and prints paths relative to
    # the current directory.
    proc = subprocess.run(["jj", "--no-pager", *args], cwd=repo_dir, capture_output=True)
    if check and proc.returncode != 0:
        sys.stderr.write(proc.stderr.decode("utf-8", "replace"))
        raise SystemExit(f"jj {' '.join(args)} failed ({proc.returncode})")
    return proc


def cmd_jj_check(opts) -> None:
    repo = opts.work_dir / "repo.git"
    jj_dir = opts.work_dir / "jj"
    states = load_states(opts.work_dir)
    by_state: dict[int, list[dict]] = defaultdict(list)
    for record in load_results(opts.work_dir):
        if record["content_conflict"] and not record["lockfile"] and record["path"].endswith(".rs"):
            by_state[record["state"]].append(record)
    resolved_states = sorted(s for s, rows in by_state.items() if any(resolved_by(r, "items") for r in rows))
    others = sorted(set(by_state) - set(resolved_states))
    chosen = resolved_states + random.Random(opts.seed).sample(others, min(opts.declined, len(others)))

    # jj only sees commits reachable from a branch, tag or remote ref.
    for index in chosen:
        git(repo, "update-ref", f"refs/heads/jj-check/{index}", states[index]["head"])
    if not jj_dir.exists():
        subprocess.run(["jj", "git", "init", "--git-repo", str(repo), str(jj_dir)], check=True, capture_output=True)
    jj(jj_dir, "git", "import")

    counts: Counter = Counter()
    mismatches = []
    for index in chosen:
        state = states[index]
        # The merge commit holds the same three-way merge a rebase of the
        # PR onto `main` leaves in the working copy: side #1 is `main`.
        jj(jj_dir, "new", "--no-edit", "--ignore-working-copy", "-m", f"jj-check {index}", state["main"], state["head"])
        merge = f'description(exact:"jj-check {index}\n") & latest(description(exact:"jj-check {index}\n"))'
        listing = jj(jj_dir, "resolve", "--list", "--ignore-working-copy", "-r", merge, check=False).stdout.decode()
        # "<path>    2-sided conflict", padded to align.
        conflicted = {re.sub(r"\s+\d+-sided conflict.*$", "", line) for line in listing.splitlines() if line.strip()}
        for record in by_state[index]:
            path = record["path"]
            if path not in conflicted:
                counts["not conflicted under jj"] += 1
                continue
            content = jj(jj_dir, "file", "show", "--ignore-working-copy", "-r", merge, path).stdout
            with tempfile.TemporaryDirectory() as tmp:
                conflict_file = Path(tmp) / "conflicted.rs"
                out_file = Path(tmp) / "out.rs"
                conflict_file.write_bytes(content)
                proc = subprocess.run(
                    [str(opts.union_bin), "--path", path, "--scope", "items", "--input", str(conflict_file), "--output", str(out_file)],
                    capture_output=True,
                )
                verdict = json.loads(proc.stdout)
                from_git = resolved_by(record, "items")
                if verdict["resolved"] != from_git:
                    counts["verdict differs"] += 1
                    mismatches.append((index, path, from_git, verdict.get("reason", "")))
                elif not from_git:
                    counts["declined under both"] += 1
                elif out_file.read_bytes() == (opts.work_dir / "resolved" / str(index) / "items" / path).read_bytes():
                    counts["resolved under both, identical output"] += 1
                else:
                    counts["resolved under both, output differs"] += 1
                    mismatches.append((index, path, "output", ""))
    print(
        f"Rust content-conflicted files re-created with jj {opts.jj_version}: "
        f"{sum(counts.values())} in {len(chosen)} states "
        f"({len(resolved_states)} states with a file keep-both resolved, {len(chosen) - len(resolved_states)} sampled others).\n"
    )
    print("| Outcome | Files |")
    print("| --- | ---: |")
    for outcome, count in counts.most_common():
        print(f"| {outcome} | {count} |")
    for mismatch in mismatches:
        print(f"- mismatch: state {mismatch[0]} `{mismatch[1]}` git-markers resolved={mismatch[2]} {mismatch[3]}")


def classify_reason(reason: str) -> str:
    """Strips the line number and quoted source so equal causes group together."""
    reason = reason.split(": ", 1)[1] if reason.startswith("conflict at line ") else reason
    return reason.split(" (`")[0].split(" `")[0]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--work-dir", type=Path, required=True, help="the directory `corpus.py` filled")
    sub = parser.add_subparsers(dest="command", required=True)
    run_parser = sub.add_parser("run")
    run_parser.add_argument("--union-bin", type=Path, required=True, help="path to the built keep_both_replay binary")
    run_parser.add_argument("--mergiraf", type=Path, help="path to a mergiraf binary; omitted, Mergiraf is skipped")
    run_parser.add_argument("--jobs", type=int, default=8)
    run_parser.set_defaults(func=cmd_run)
    report = sub.add_parser("report")
    report.add_argument("--reasons", type=int, default=0, help="also list the top N decline reasons")
    report.set_defaults(func=cmd_report)
    jj_check = sub.add_parser("jj-check")
    jj_check.add_argument("--union-bin", type=Path, required=True, help="path to the built keep_both_replay binary")
    jj_check.add_argument("--declined", type=int, default=100, help="how many states keep-both declined to include")
    jj_check.add_argument("--seed", type=int, default=20261111)
    jj_check.set_defaults(func=cmd_jj_check)
    opts = parser.parse_args()
    opts.work_dir = opts.work_dir.resolve()
    if opts.command == "jj-check":
        opts.jj_version = subprocess.run(["jj", "--version"], capture_output=True, text=True).stdout.split()[-1]
    opts.func(opts)


if __name__ == "__main__":
    main()
