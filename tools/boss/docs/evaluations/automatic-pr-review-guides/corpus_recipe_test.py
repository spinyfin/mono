"""Offline checks for evidence fidelity; no model calls or human grading."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("recipe", Path(__file__).with_name("run-corpus.py"))
recipe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recipe)
TOOL = Path(sys.argv.pop(1)).resolve()


class CorpusRecipeTests(unittest.TestCase):
    def test_saved_comparisons_and_current_prompt_budgets(self):
        self.assertEqual(len(recipe.MANIFEST["cases"]), 6)
        with tempfile.TemporaryDirectory() as temporary:
            for case in recipe.MANIFEST["cases"]:
                packet = recipe.HERE / "cases" / case["id"] / "packet.json"
                recipe.verify_packet(case, packet)
                for variant in ["baseline", "production"]:
                    prompt = Path(temporary) / f"{case['id']}-{variant}.txt"
                    argv = [TOOL, "prompt", "--packet", packet, "--git-dir", "/pinned/.git", "--out", prompt]
                    if variant == "baseline":
                        argv.append("--baseline")
                    self.assertNotIn("PROMPT_TOO_LARGE", recipe.output(argv))
                    self.assertIn(case["head_sha"], prompt.read_text())
                    self.assertIn("Registered screenshots", prompt.read_text())
                    body = prompt.read_text()
                    self.assertLess(body.index("### Changed files"), body.index("### Task context"))
                    self.assertLess(body.index("### Task context"), body.index("### Registered screenshots"))
                    if variant == "baseline":
                        self.assertIn("Check every step against the actual code.", body)
                        self.assertNotIn("Ground the guide in those revisions.", body)
                    else:
                        self.assertIn("Ground the guide in those revisions.", body)

    def test_external_guides_and_local_links_are_reviewable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            case = root / "flunge-1545"
            case.mkdir()
            guide = case / "guide-baseline.md"
            href = "/Users/example/checkouts/flunge-1545/src/main.ts:19"
            guide.write_text(f"## Fix\n[implementation]({href})\n")
            target = root / "template.md"
            recipe.template(TOOL, target, [root])
            body = target.read_text()
            self.assertIn(href, body)
            self.assertIn(guide.as_uri(), body)
            self.assertIn("non-GitHub link (not navigable)", body)
            self.assertIn("Reviewer: (name)", body)

    def test_human_review_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "template.md"
            for body in ["| [x] | case | source | kind | section | link | | |\n",
                         "| [ ] | case | source | kind | section | link | supported | |\n",
                         "| [ ] | case | source | kind | section | link | | checked source |\n",
                         "Reviewer: Alex  Date: 2026-11-18  Prompt acceptance: accepted\n"]:
                target.write_text(body)
                with self.assertRaises(ValueError):
                    recipe.template(TOOL, target, [])
                self.assertEqual(target.read_text(), body)
            recipe.template(TOOL, target, [], force=True)
            self.assertIn("Prompt acceptance: (pending)", target.read_text())

    def test_bazel_has_no_external_timeout(self):
        with patch.object(recipe.subprocess, "run") as run:
            recipe.command(["bazel", "build", recipe.TARGET])
            self.assertNotIn("timeout", run.call_args.kwargs)

    def test_prepare_only_preserves_committed_human_template(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            committed = root / "grounding-review-template.md"
            committed.write_text("Reviewer: Alex\n| [x] | accepted |\n")
            argv = ["run-corpus.py", "--prepare-only", "--run-dir", str(root / "new-run"),
                    "--work-dir", str(root / "work")]
            def fake_command(args, **kwargs):
                if "metadata" in args:
                    Path(args[args.index("--out") + 1]).write_text("{}")
            with patch.object(recipe, "HERE", root), patch.object(recipe, "ROOT", Path.cwd()), \
                 patch.object(recipe, "MANIFEST", {"cases": []}), patch.object(sys, "argv", argv), \
                 patch.object(recipe, "command", side_effect=fake_command), patch.object(recipe, "output", return_value=str(TOOL)):
                recipe.main()
            self.assertEqual(committed.read_text(), "Reviewer: Alex\n| [x] | accepted |\n")
            self.assertTrue((root / "new-run/grounding-review-template.md").exists())

    def test_publication_redaction_preserves_lines_and_is_idempotent(self):
        source = "prefix " + "T" + "123456" + "\nsecond line\n"
        published = recipe.publication_text(source)
        self.assertEqual(published, "prefix [internal-id-redacted]\nsecond line\n")
        self.assertEqual(recipe.publication_text(published), published)

    def test_published_sources_keep_original_hash_verification(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original = ("first\n" + "T" + "123456" + "\n").encode()
            packet = root / "packet.json"
            packet.write_text(json.dumps({"files": [{"after": {
                "content": original.decode(), "content_hash": recipe.hashlib.sha256(original).hexdigest(),
                "sha": "a" * 40, "path": "src/file.rs"}}]}))
            recipe.publish_tree(root)
            self.assertIn("[internal-id-redacted]", packet.read_text())
            first_manifest = (root / "publication.json").read_bytes()
            recipe.publish_tree(root)
            self.assertEqual((root / "publication.json").read_bytes(), first_manifest)
            with patch.object(recipe, "command", return_value=subprocess.CompletedProcess([], 0, stdout=original)):
                self.assertEqual(recipe.verify_sources(packet, root), 1)
            with patch.object(recipe, "command", return_value=subprocess.CompletedProcess([], 0, stdout=b"wrong\n")):
                with self.assertRaisesRegex(ValueError, "hash differs"):
                    recipe.verify_sources(packet, root)

    def test_template_covers_baseline_and_retries_without_verdicts(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "template.md"
            recipe.template(TOOL, target, [recipe.HERE / "cases"])
            body = target.read_text()
            for guide in (recipe.HERE / "cases").glob("*/guide-*.md"):
                refs = json.loads(recipe.output([TOOL, "refs", "--markdown", guide, "--out", "/dev/stdout"]))
                for reference in refs:
                    self.assertIn(reference["href"], body)
                self.assertIn(str(guide.relative_to(recipe.HERE)), body)
            rows = [line for line in body.splitlines() if line.startswith("| [")]
            self.assertGreater(len(rows), 100)
            self.assertEqual(body.count("mechanical failure: range exceeds"), 4)
            self.assertTrue(all(line.split("|")[1].strip() == "[ ]"
                                and not any(cell.strip() for cell in line.split("|")[-3:-1]) for line in rows))

    def test_production_uses_submission_not_final_acknowledgement(self):
        guide = "# Submitted guide\n\n## Core fix\nSource-grounded content."
        event = {"type": "item.completed", "item": {"type": "command_execution",
                 "aggregated_output": json.dumps({"corpus_submission": guide})}}
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "prompt-production.txt").write_text("prompt")

            def fake_run(argv, **kwargs):
                if argv[0] == "codex":
                    self.assertEqual(argv[argv.index("-m") + 1], "gpt-6-astra")
                    self.assertIn('model_reasoning_effort="high"', argv)
                    kwargs["stdout"].write(json.dumps(event) + "\n")
                    kwargs["stdout"].write(json.dumps({"type": "turn.completed", "usage": {"output_tokens": 23}}) + "\n")
                    Path(argv[argv.index("-o") + 1]).write_text("Submitted successfully.")
                return subprocess.CompletedProcess(argv, 0)

            with patch.object(recipe.subprocess, "run", side_effect=fake_run):
                self.assertTrue(recipe.run_variant(TOOL, {"id": "flunge-1545"}, directory, directory, "production"))
            self.assertEqual((directory / "guide-production.md").read_text(), guide)
            self.assertEqual(json.loads((directory / "usage-production.json").read_text())["usage"], {"output_tokens": 23})

    def test_timeout_records_failed_usage_and_does_not_invent_a_guide(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "prompt-baseline.txt").write_text("prompt")
            with patch.object(recipe.subprocess, "run", side_effect=subprocess.TimeoutExpired("codex", 7200)):
                self.assertFalse(recipe.run_variant(TOOL, {"id": "flunge-1545"}, directory, directory, "baseline"))
            usage = json.loads((directory / "usage-baseline.json").read_text())
            self.assertEqual(usage["exit_code"], 124)
            self.assertTrue(usage["turn_failed"])
            self.assertIsNone(usage["usage"])
            self.assertFalse((directory / "guide-baseline.md").exists())


if __name__ == "__main__":
    unittest.main()
