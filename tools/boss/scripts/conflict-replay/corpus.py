#!/usr/bin/env python3
"""Rebuild the merge-conflict corpus from GitHub force-push history.

Every time a PR branch is force-pushed onto a newer `main`, the commit it
replaced (the "before" commit) is the head that had to be reconciled with
`main`. This script recovers those heads and replays each one against the
`main` commit that preceded the push, using `git merge-tree`, to recover
the conflicted files and the three versions of each one.

Subcommands (run in order; every one is resumable and offline after the
first two):

  fetch-prs   One paginated GraphQL pass over the repository's PRs, with
              their force-push events.
  clone       Bare clone into the work directory, plus every PR head and
              every force-push "before"/"after" commit, fetched by SHA.
  replay      `git merge-tree` of each pre-push head against `main`.

All state lives under `--work-dir`, which must be outside any checkout.
Nothing here writes to the repository it is run from.
"""

from __future__ import annotations

import argparse
import bisect
import json
import subprocess
import sys
from datetime import datetime
from pathlib import Path

PR_QUERY = """
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 40, after: $cursor, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number state createdAt mergedAt closedAt baseRefName headRefOid changedFiles
        mergeCommit { oid }
        timelineItems(first: 100, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT]) {
          pageInfo { hasNextPage endCursor }
          nodes { ... on HeadRefForcePushedEvent { createdAt beforeCommit { oid } afterCommit { oid } } }
        }
      }
    }
  }
}
"""

TIMELINE_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      timelineItems(first: 100, after: $cursor, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT]) {
        pageInfo { hasNextPage endCursor }
        nodes { ... on HeadRefForcePushedEvent { createdAt beforeCommit { oid } afterCommit { oid } } }
      }
    }
  }
}
"""


def run(args: list[str], *, cwd: Path | None = None, check: bool = True, input_bytes: bytes | None = None):
    proc = subprocess.run(args, cwd=cwd, input=input_bytes, capture_output=True)
    if check and proc.returncode != 0:
        sys.stderr.write(proc.stderr.decode("utf-8", "replace"))
        raise SystemExit(f"command failed ({proc.returncode}): {' '.join(args)}")
    return proc


def git(repo: Path, *args: str, check: bool = True, input_bytes: bytes | None = None):
    return run(["git", "-C", str(repo), *args], check=check, input_bytes=input_bytes)


def graphql(query: str, **variables) -> dict:
    args = ["gh", "api", "graphql", "-f", f"query={query}"]
    for key, value in variables.items():
        if value is None:
            continue
        flag = "-F" if isinstance(value, int) else "-f"
        args += [flag, f"{key}={value}"]
    return json.loads(run(args).stdout)["data"]


def push_events(nodes: list[dict]) -> list[dict]:
    events = []
    for node in nodes:
        # GitHub drops the commit object once it is garbage collected; such
        # an event cannot be replayed.
        if not node or not node.get("beforeCommit") or not node.get("afterCommit"):
            continue
        events.append(
            {
                "at": node["createdAt"],
                "before": node["beforeCommit"]["oid"],
                "after": node["afterCommit"]["oid"],
            }
        )
    return events


def cmd_fetch_prs(opts) -> None:
    owner, name = opts.repo.split("/")
    out_path = opts.work_dir / "prs.jsonl"
    cursor = None
    count = 0
    with out_path.open("w") as out:
        while True:
            page = graphql(PR_QUERY, owner=owner, name=name, cursor=cursor)["repository"]["pullRequests"]
            for pr in page["nodes"]:
                timeline = pr.pop("timelineItems")
                events = push_events(timeline["nodes"])
                inner = timeline["pageInfo"]
                while inner["hasNextPage"]:
                    more = graphql(
                        TIMELINE_QUERY, owner=owner, name=name, number=pr["number"], cursor=inner["endCursor"]
                    )["repository"]["pullRequest"]["timelineItems"]
                    events += push_events(more["nodes"])
                    inner = more["pageInfo"]
                pr["mergeCommit"] = (pr.get("mergeCommit") or {}).get("oid")
                pr["forcePushes"] = events
                out.write(json.dumps(pr) + "\n")
                count += 1
            print(f"fetched {count} PRs", file=sys.stderr)
            if not page["pageInfo"]["hasNextPage"]:
                break
            cursor = page["pageInfo"]["endCursor"]
    print(f"wrote {out_path} ({count} PRs)")


def load_prs(work_dir: Path) -> list[dict]:
    with (work_dir / "prs.jsonl").open() as handle:
        return [json.loads(line) for line in handle]


def cmd_clone(opts) -> None:
    repo = opts.work_dir / "repo.git"
    url = f"https://github.com/{opts.repo}.git"
    if not repo.exists():
        run(["git", "clone", "--bare", "--quiet", url, str(repo)])
        # Fetched-by-SHA commits are reachable only through the refs created
        # below; never let an automatic gc race with the fetches.
        git(repo, "config", "gc.auto", "0")
    git(repo, "fetch", "--quiet", "--no-tags", "origin", "+refs/heads/main:refs/heads/main")
    git(repo, "fetch", "--quiet", "--no-tags", "origin", "+refs/pull/*/head:refs/pull/*/head")

    wanted: list[str] = []
    for pr in load_prs(opts.work_dir):
        for event in pr["forcePushes"]:
            wanted += [event["before"], event["after"]]
    wanted = sorted(set(wanted))
    missing = [sha for sha in wanted if git(repo, "cat-file", "-e", f"{sha}^{{commit}}", check=False).returncode != 0]
    print(f"{len(wanted)} force-push commits referenced, {len(missing)} to fetch", file=sys.stderr)
    unavailable = 0
    for start in range(0, len(missing), 40):
        batch = missing[start : start + 40]
        refspecs = [f"{sha}:refs/replay/{sha}" for sha in batch]
        if git(repo, "fetch", "--quiet", "--no-tags", "origin", *refspecs, check=False).returncode != 0:
            # One unretrievable SHA fails the whole batch; retry one by one.
            for sha in batch:
                single = git(repo, "fetch", "--quiet", "--no-tags", "origin", f"{sha}:refs/replay/{sha}", check=False)
                unavailable += single.returncode != 0
        print(f"fetched {min(start + 40, len(missing))}/{len(missing)}", file=sys.stderr)
    print(f"clone ready at {repo}; {unavailable} commits no longer retrievable")


class MainHistory:
    """First-parent history of `main`, oldest first, for time lookups."""

    def __init__(self, repo: Path):
        out = git(repo, "log", "--first-parent", "--reverse", "--format=%H %ct", "main").stdout.decode()
        self.shas: list[str] = []
        self.times: list[int] = []
        for line in out.splitlines():
            sha, ts = line.split()
            self.shas.append(sha)
            self.times.append(int(ts))
        self.index = {sha: i for i, sha in enumerate(self.shas)}

    def at(self, iso: str) -> str | None:
        """The `main` commit that preceded `iso` (GitHub's event time)."""
        ts = int(datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp())
        pos = bisect.bisect_right(self.times, ts)
        return self.shas[pos - 1] if pos else None


def merge_tree(repo: Path, ours: str, theirs: str) -> dict:
    """`git merge-tree --write-tree -z`, parsed.

    Returns the written tree and, for a conflicted merge, one entry per
    conflicted path with its stage blobs and the conflict kinds git
    reported for it.
    """
    proc = git(
        repo, "-c", "merge.conflictStyle=diff3", "merge-tree", "--write-tree", "-z", ours, theirs, check=False
    )
    if proc.returncode not in (0, 1):
        return {"error": proc.stderr.decode("utf-8", "replace").strip()}
    fields = proc.stdout.split(b"\0")
    result = {"tree": fields[0].decode(), "conflict": proc.returncode == 1, "files": []}
    if proc.returncode == 0:
        return result
    files: dict[str, dict] = {}
    pos = 1
    # Conflicted file info: "<mode> <oid> <stage>\t<path>", ended by an
    # empty field.
    while pos < len(fields) and fields[pos]:
        meta, path = fields[pos].split(b"\t", 1)
        _mode, oid, stage = meta.decode().split()
        entry = files.setdefault(path.decode("utf-8", "replace"), {"stages": {}, "kinds": []})
        entry["stages"][stage] = oid
        pos += 1
    pos += 1
    # Informational messages: "<n>", n paths, "<kind>", "<message>".
    while pos < len(fields) and fields[pos]:
        count = int(fields[pos])
        paths = [p.decode("utf-8", "replace") for p in fields[pos + 1 : pos + 1 + count]]
        kind = fields[pos + 1 + count].decode()
        pos += count + 3
        if not kind.startswith("CONFLICT"):
            continue
        for path in paths:
            if path in files and kind not in files[path]["kinds"]:
                files[path]["kinds"].append(kind)
    result["files"] = [{"path": path, **entry} for path, entry in sorted(files.items())]
    return result


def is_content_conflict(entry: dict) -> bool:
    """Both sides edited a file that exists in the base: a three-way text conflict."""
    return entry["kinds"] == ["CONFLICT (contents)"] and set(entry["stages"]) == {"1", "2", "3"}


def cmd_replay(opts) -> None:
    repo = opts.work_dir / "repo.git"
    history = MainHistory(repo)
    since = datetime.fromisoformat(opts.since + "T00:00:00+00:00")
    out_path = opts.work_dir / "states.jsonl"
    counts = {"pushes": 0, "onto_newer_main": 0, "conflicted": 0, "states": 0, "branch_point_not_on_main": 0}
    seen: set[tuple[int, str]] = set()
    with out_path.open("w") as out:
        for pr in load_prs(opts.work_dir):
            if pr["baseRefName"] != "main":
                continue
            for event in pr["forcePushes"]:
                if datetime.fromisoformat(event["at"].replace("Z", "+00:00")) < since:
                    continue
                counts["pushes"] += 1
                bases = []
                for sha in (event["before"], event["after"]):
                    proc = git(repo, "merge-base", sha, "main", check=False)
                    bases.append(proc.stdout.decode().strip() if proc.returncode == 0 else None)
                # A branch point that is not on `main`'s first-parent line
                # belongs to a branch stacked on another PR that later
                # landed as a merge commit. Restacking it is not a
                # reconciliation with `main`.
                if None in bases or bases[0] not in history.index or bases[1] not in history.index:
                    counts["branch_point_not_on_main"] += 1
                    continue
                # A push that left the branch point alone (an amend) or
                # moved it backwards did not reconcile the PR with `main`.
                if history.index[bases[1]] <= history.index[bases[0]]:
                    continue
                counts["onto_newer_main"] += 1
                main = history.at(event["at"])
                # GitHub's event time can trail the push by a few seconds;
                # never replay against a `main` older than the one the push
                # actually landed on.
                if main is None or history.index[main] < history.index[bases[1]]:
                    main = bases[1]
                merged = merge_tree(repo, main, event["before"])
                if "error" in merged:
                    raise SystemExit(f"merge-tree failed for PR {pr['number']} {event['before']}: {merged['error']}")
                if not merged["conflict"]:
                    continue
                counts["conflicted"] += 1
                key = (pr["number"], event["before"])
                if key in seen:
                    continue
                seen.add(key)
                counts["states"] += 1
                out.write(
                    json.dumps(
                        {
                            "pr": pr["number"],
                            "head": event["before"],
                            "after": event["after"],
                            "pushed_at": event["at"],
                            "main": main,
                            "landed_on": bases[1],
                            "branch_point": bases[0],
                            "tree": merged["tree"],
                            "files": merged["files"],
                        }
                    )
                    + "\n"
                )
    print(json.dumps(counts))
    print(f"wrote {out_path}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--work-dir", type=Path, required=True, help="scratch directory outside any checkout")
    parser.add_argument("--repo", default="spinyfin/mono", help="GitHub owner/name")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("fetch-prs").set_defaults(func=cmd_fetch_prs)
    sub.add_parser("clone").set_defaults(func=cmd_clone)
    replay = sub.add_parser("replay")
    replay.add_argument("--since", default="2026-05-01", help="ignore pushes before this UTC date")
    replay.set_defaults(func=cmd_replay)
    opts = parser.parse_args()
    opts.work_dir.mkdir(parents=True, exist_ok=True)
    opts.func(opts)


if __name__ == "__main__":
    main()
