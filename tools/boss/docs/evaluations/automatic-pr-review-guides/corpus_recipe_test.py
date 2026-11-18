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
            self.assertTrue(all(line.startswith("| [ ] |") and line.endswith("| | |") for line in rows))

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
            with patch.object(recipe.subprocess, "run", side_effect=subprocess.TimeoutExpired("codex", 900)):
                self.assertFalse(recipe.run_variant(TOOL, {"id": "flunge-1545"}, directory, directory, "baseline"))
            usage = json.loads((directory / "usage-baseline.json").read_text())
            self.assertEqual(usage["exit_code"], 124)
            self.assertTrue(usage["turn_failed"])
            self.assertIsNone(usage["usage"])
            self.assertFalse((directory / "guide-baseline.md").exists())


if __name__ == "__main__":
    unittest.main()
