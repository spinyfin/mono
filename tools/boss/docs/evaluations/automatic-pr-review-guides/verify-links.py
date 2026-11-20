#!/usr/bin/env python3
"""Mechanical check of every pinned blob link in the grounding-review template.

Confirms only that the revision resolves in the pinned checkout, the file exists
at that revision, and the line range lies inside the file. It says nothing about
whether the link supports the claim next to it; that remains a human judgement.
Run from the repository root after `run-corpus.sh` has created the pinned clones.
"""

import argparse
import os
import collections
from pathlib import Path
import re
import subprocess
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]
BLOB = re.compile(r"https://github\.com/([^/]+/[^/]+)/blob/([0-9a-f]{40})/([^#]+)(?:#L(\d+)(?:-L(\d+))?)?$")


def file_lines(clone, sha, path, cache):
    key = (sha, path)
    if key not in cache:
        result = subprocess.run(["jj", "--ignore-working-copy", "-R", clone, "file", "show", "-r", sha,
                                 f'root-file:"{path}"'], capture_output=True)
        cache[key] = len(result.stdout.decode(errors="replace").splitlines()) if result.returncode == 0 else None
    return cache[key]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, default=Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "boss-review-guide-corpus")
    parser.add_argument("--template", type=Path, default=HERE / "grounding-review-template.md")
    args = parser.parse_args()
    clones = {repo: args.work_dir / "pinned-repos" / repo.replace("/", "-")
              for repo in ["brianduff/flunge", "spinyfin/mono"]}
    counts, problems, cache = collections.Counter(), [], {}
    for line in args.template.read_text().splitlines():
        if not line.startswith("| ["):
            continue
        columns = [column.strip() for column in re.split(r"(?<!\\)\|", line)]
        source, href = columns[3], columns[6]
        match = BLOB.match(href)
        if not match:
            counts["non-GitHub link (not navigable)" if not href.startswith("https://github.com/") else "not a blob link (not line-checked)"] += 1
            continue
        repository, sha, path, first, last = match.groups()
        if repository not in clones:
            counts["repository not pinned locally (not checked)"] += 1
            continue
        length = file_lines(clones[repository], sha, path, cache)
        if length is None:
            counts["revision or file missing"] += 1
            problems.append((source, href, "revision or file missing"))
        elif first and (int(first) > length or (last and (int(last) > length or int(last) < int(first)))):
            counts["line range outside file"] += 1
            problems.append((source, href, f"file has {length} lines"))
        else:
            counts["revision, file and lines exist"] += 1
    for name, count in sorted(counts.items()):
        print(f"{count:5d}  {name}")
    for problem in problems:
        print("PROBLEM", *problem)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
