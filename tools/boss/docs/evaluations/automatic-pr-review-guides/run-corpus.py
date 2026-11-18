#!/usr/bin/env python3
"""Paired prompt evaluation using the production collector, renderer and validator."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]
MANIFEST = json.loads((HERE / "corpus-manifest.json").read_text())
TARGET = "//tools/boss/engine/review-guide:corpus-tool"


def command(args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, timeout=1800, **kwargs)


def output(args):
    try:
        return command(args, capture_output=True, text=True).stdout.strip()
    except subprocess.CalledProcessError as error:
        raise RuntimeError(error.stderr) from error


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def verify_packet(case, path):
    packet = json.loads(path.read_text())
    for saved, collected in [("base_tip_sha", "observed_base_sha"),
                             ("merge_base_sha", "merge_base_sha"), ("head_sha", "head_sha")]:
        if case[saved] != packet[collected]:
            raise ValueError(f"{case['id']}: {saved} differs from the saved comparison")
    if packet["canonical_pr_url"] != case["pr_url"]:
        raise ValueError(f"{case['id']}: PR identity mismatch")
    return packet


def checkout(case, work):
    repository = case["pr_url"].split("github.com/", 1)[1].split("/pull/", 1)[0]
    clone = work / "pinned-repos" / repository.replace("/", "-")
    if not clone.exists():
        clone.parent.mkdir(parents=True, exist_ok=True)
        # Squash-merged heads need GitHub's retained pull refs as well as main.
        # gh is the authenticated transport; all subsequent VCS operations use jj.
        argv = ["gh", "repo", "clone", repository, clone, "--"]
        for comparison in MANIFEST["cases"]:
            if comparison["pr_url"].startswith(f"https://github.com/{repository}/pull/"):
                number = comparison["pr_url"].rsplit("/", 1)[1]
                argv.extend(["--config", f"remote.origin.fetch=+refs/pull/{number}/head:refs/remotes/origin/corpus-{number}"])
        command(argv)
    if not (clone / ".jj").exists():
        command(["jj", "git", "init", "--colocate", clone])
    for field in ["base_tip_sha", "merge_base_sha", "head_sha"]:
        actual = output(["jj", "--ignore-working-copy", "-R", clone, "log", "--no-graph",
                         "-r", case[field], "-T", "commit_id"])
        if actual != case[field]:
            raise ValueError(f"{case['id']}: cannot resolve pinned {field}")
    workspace = work / "checkouts" / case["id"]
    if not workspace.exists():
        workspace.parent.mkdir(parents=True, exist_ok=True)
        command(["jj", "-R", clone, "workspace", "add", "--name", case["id"],
                 "--revision", case["head_sha"], workspace])
    # jj workspace add creates an empty child of the requested immutable head.
    parent = output(["jj", "--ignore-working-copy", "-R", workspace, "log", "--no-graph",
                     "-r", "@-", "-T", "commit_id"])
    delta = output(["jj", "-R", workspace, "diff", "--from", case["head_sha"], "--summary"])
    if parent != case["head_sha"] or delta:
        raise ValueError(f"{case['id']}: checkout differs from pinned head")
    return workspace, clone / ".git"


def verify_sources(packet_path, workspace):
    packet = json.loads(packet_path.read_text())
    checked = 0
    for file in packet["files"]:
        for side in ["before", "after"]:
            source = file.get(side)
            if not source or source.get("content") is None:
                continue
            result = command(["jj", "--ignore-working-copy", "-R", workspace, "file", "show",
                              "-r", source["sha"], workspace / source["path"]], capture_output=True)
            if result.stdout != source["content"].encode():
                raise ValueError(f"Saved source differs from pinned GitHub object: {source['sha']}:{source['path']}")
            checked += 1
    return checked


def run_variant(tool, case, directory, workspace, variant):
    prompt = directory / f"prompt-{variant}.txt"
    events = directory / f"events-{variant}.jsonl"
    stderr = directory / f"stderr-{variant}.txt"
    final = directory / f"final-{variant}.md"
    # No live engine binding is inherited. BOSS_BIN is a read-only capture
    # adapter: it validates with corpus-tool and prints the submitted Markdown
    # into the Codex command event. It cannot create/modify a real review guide.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("BOSS_", "CUBE_"))}
    env.update(BOSS_BIN=str(HERE / "capture-submission.py"), CORPUS_TOOL=str(tool),
               CORPUS_PACKET=str(HERE / "cases" / case["id"] / "packet.json"),
               CORPUS_WORKSPACE=str(workspace))
    argv = ["codex", "exec", "--ignore-user-config", "--json", "-s", "read-only",
            "--skip-git-repo-check", "-C", str(workspace), "-m", "gpt-6-astra",
            "-c", 'model_reasoning_effort="high"', "-c", "project_doc_max_bytes=0",
            "-c", 'web_search="disabled"', "-o", str(final), "-"]
    started = time.time()
    print(f"{case['id']}/{variant}: gpt-6-astra high", flush=True)
    with prompt.open() as source, events.open("w") as event_file, stderr.open("w") as errors:
        try:
            result = subprocess.run(argv, stdin=source, stdout=event_file, stderr=errors,
                                    env=env, timeout=900)
            exit_code = result.returncode
        except subprocess.TimeoutExpired:
            exit_code = 124
    ended = time.time()
    records = [json.loads(line) for line in events.read_text().splitlines() if line.strip()]
    turns = [record for record in records if record.get("type") == "turn.completed"]
    submissions = []
    for record in records:
        item = record.get("item", {})
        if record.get("type") != "item.completed" or item.get("type") != "command_execution":
            continue
        for line in item.get("aggregated_output", "").splitlines():
            try:
                submission = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(submission, dict) and isinstance(submission.get("corpus_submission"), str):
                submissions.append(submission["corpus_submission"])
    guide = directory / f"guide-{variant}.md"
    if variant == "production" and submissions:
        guide.write_text(submissions[-1])
    elif variant == "baseline" and final.exists():
        guide.write_text(final.read_text())
    success = exit_code == 0 and bool(turns) and guide.exists() and guide.stat().st_size > 0
    write_json(directory / f"usage-{variant}.json", {
        "case_id": case["id"], "variant": variant, "model": "gpt-6-astra", "effort": "high",
        "started_at_epoch": started, "ended_at_epoch": ended, "wall_seconds": ended - started,
        "exit_code": exit_code, "turn_failed": not success,
        "usage": turns[-1].get("usage") if turns else None,
        "accepted_submissions": len(submissions),
    })
    if variant == "production" and guide.exists():
        with (directory / "validation-production.txt").open("w") as validation:
            result = subprocess.run([str(tool), "validate", "--packet", env["CORPUS_PACKET"],
                                     "--workspace", str(workspace), "--markdown", str(guide)],
                                    stdout=validation, stderr=subprocess.STDOUT, timeout=120)
        success = success and result.returncode == 0
    print(f"{case['id']}/{variant}: {'completed' if success else 'FAILED'} ({ended-started:.1f}s)", flush=True)
    return success


def reference_kind(section, href):
    # Positional labels help humans navigate; these are not grounding verdicts.
    if re.search(r"example|before.after|scenario|trace|worked", section, re.I):
        return "example (heading)"
    if re.search(r"test|coverage|assert", section, re.I):
        return "test (heading)"
    if re.search(r"fix|implementation|walkthrough|architecture|logic", section, re.I):
        return "core fix (heading)"
    return "source/context" if "/blob/" in href else "PR/diff/context"


def template(tool, output_path, guide_roots):
    lines = ["# Human navigation and grounding review", "",
             "Every extracted reference is unchecked. Verdict and notes are reserved for a human.",
             "Open each generated link in GitHub and check the intended file, revision, side, lines, and supporting claim.",
             "Kind is a mechanical heading-based navigation hint, not a judgement of support. Repeated links in different sections remain separate rows.",
             "The source column distinguishes saved evidence from fresh runs. Missing outputs have no invented references.", "",
             "Reviewer: __________  Date: __________  Prompt acceptance: __________", ""]
    count = 0
    for case in MANIFEST["cases"]:
        lines += [f"## {case['id']}", "",
                  "| Checked | Case | Source | Kind | Section | Link as generated | Verdict | Notes |",
                  "| --- | --- | --- | --- | --- | --- | --- | --- |"]
        case_count = 0
        for root in guide_roots:
            for guide in sorted((root / case["id"]).glob("guide-*.md")):
                refs = json.loads(output([tool, "refs", "--markdown", guide, "--out", "/dev/stdout"]))
                source = str(guide.relative_to(HERE))
                for ref in refs:
                    section = ref["section"].replace("|", "\\|")
                    kind = reference_kind(section, ref["href"])
                    lines.append(f"| [ ] | {case['id']} | `{source}` | {kind} | {section} | {ref['href']} | | |")
                    count += 1
                    case_count += 1
        if not case_count:
            lines += ["", "No guide output exists for this case; generation is still required."]
        lines.append("")
    output_path.write_text("\n".join(lines))
    print(f"Wrote {count} unchecked reference rows to {output_path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cases", nargs="*")
    parser.add_argument("--run-dir", type=Path, default=HERE / "runs" / datetime.date.today().isoformat())
    parser.add_argument("--work-dir", type=Path, default=ROOT / ".codex/corpus-work")
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--template-only", action="store_true")
    parser.add_argument("--recollect", action="store_true", help="Collect a separate fresh packet from pinned GitHub revisions")
    args = parser.parse_args()
    ids = {case["id"] for case in MANIFEST["cases"]}
    if set(args.cases) - ids:
        parser.error("Unknown case id")
    args.run_dir = args.run_dir.resolve()
    args.work_dir = args.work_dir.resolve()
    if args.run_dir == HERE / "cases" or HERE / "cases" in args.run_dir.parents:
        parser.error("The saved cases are immutable; choose a separate run directory")
    if Path.cwd().resolve() != ROOT:
        parser.error(f"Invoke from the Bazel workspace {ROOT}")
    command(["bazel", "build", TARGET])
    tool = ROOT / output(["bazel", "cquery", TARGET, "--output=files"]).splitlines()[-1]
    if args.template_only:
        template(tool, HERE / "grounding-review-template.md", [HERE / "cases", args.run_dir])
        return
    if not args.prepare_only:
        command(["codex", "login", "status"])
    args.run_dir.mkdir(parents=True, exist_ok=True)
    command([tool, "metadata", "--out", args.run_dir / "generator.json"])
    failures = []
    for case in MANIFEST["cases"]:
        if args.cases and case["id"] not in args.cases:
            continue
        packet = HERE / "cases" / case["id"] / "packet.json"
        verify_packet(case, packet)
        directory = args.run_dir / case["id"]
        directory.mkdir(parents=True, exist_ok=True)
        if list(directory.glob("events-*.jsonl")):
            raise ValueError(f"{directory}: prior attempts exist; choose a new run directory")
        workspace, git_dir = checkout(case, args.work_dir)
        source_count = verify_sources(packet, workspace)
        write_json(directory / "provenance.json", {
            "comparison": case, "packet_sha256": hashlib.sha256(packet.read_bytes()).hexdigest(),
            "codex_version": output(["codex", "--version"]),
            "renderer_source_sha256": hashlib.sha256((ROOT / "tools/boss/engine/review-guide/src/lib.rs").read_bytes()).hexdigest(),
            "model": "gpt-6-astra", "effort": "high",
            "saved_sources_matching_pinned_objects": source_count,
            "checkout": str(workspace), "git_dir": str(git_dir),
            "submission": "local stdout capture with production validation; no live engine binding",
        })
        if args.recollect:
            command([tool, "collect", "--pr-url", case["pr_url"], "--base-sha", case["base_tip_sha"],
                     "--head-sha", case["head_sha"], "--out", directory / "packet-recollected.json"])
            verify_packet(case, directory / "packet-recollected.json")
        for variant in ["baseline", "production"]:
            prompt = directory / f"prompt-{variant}.txt"
            argv = [tool, "prompt", "--packet", packet, "--git-dir", git_dir, "--out", prompt]
            if variant == "baseline":
                argv.append("--baseline")
            rendered = output(argv)
            if "PROMPT_TOO_LARGE" in rendered:
                raise ValueError(f"{case['id']}/{variant}: {rendered}")
            if not args.prepare_only and not run_variant(tool, case, directory, workspace, variant):
                failures.append(f"{case['id']}/{variant}")
    template(tool, HERE / "grounding-review-template.md", [HERE / "cases", args.run_dir])
    if failures:
        raise SystemExit("Generation failed: " + ", ".join(failures))


if __name__ == "__main__":
    main()
