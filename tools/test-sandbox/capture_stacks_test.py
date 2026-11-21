"""Failure-path coverage for diagnostics that must never signal the test."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import capture_stacks


class CaptureTest(unittest.TestCase):
    def test_selects_only_the_test_process_group(self):
        result = subprocess.CompletedProcess([], 0, "11 11 test\n12 11 child\n13 13 other test\n")
        with patch.object(subprocess, "run", return_value=result):
            self.assertEqual(capture_stacks.group_members(11, Path("/runtime")), [(11, "test"), (12, "child")])

    def test_unavailable_sampler_keeps_a_named_failure_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(subprocess, "run", side_effect=FileNotFoundError("sample missing")):
                capture_stacks.capture(11, "work_db_test", Path(directory), Path("/runtime"))
            report = (Path(directory) / "threads-11.txt").read_text()
            self.assertIn("pid=11 command=work_db_test", report)
            self.assertIn("stack capture unavailable: sample missing", report)

    def test_stuck_sampler_keeps_partial_output_and_reports_its_failure(self):
        def stalled(args, **kwargs):
            kwargs["stdout"].write("partial thread backtrace\n")
            raise subprocess.TimeoutExpired(args, 5)

        with tempfile.TemporaryDirectory() as directory:
            with patch.object(subprocess, "run", side_effect=stalled):
                capture_stacks.capture(11, "work_db_test", Path(directory), Path("/runtime"))
            report = (Path(directory) / "threads-11.txt").read_text()
            self.assertIn("partial thread backtrace", report)
            self.assertIn("stack capture unavailable:", report)

    def test_signal_then_cleanup_capture_each_member_once(self):
        with tempfile.TemporaryDirectory() as directory:
            with (
                patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": directory, "TEST_TARGET": "//test"}),
                patch("sys.argv", ["capture_stacks.py", "11", "/runtime"]),
                patch.object(capture_stacks, "group_members", return_value=[(11, "test"), (12, "child")]),
                patch.object(capture_stacks, "capture") as capture,
            ):
                capture_stacks.main()
                capture_stacks.main()
            self.assertEqual(capture.call_count, 2)
            self.assertEqual({call.args[0] for call in capture.call_args_list}, {11, 12})
            manifest = (Path(directory) / "timeout-11" / "processes.txt").read_text()
            self.assertIn("test=//test group=11", manifest)
            self.assertIn("12 child", manifest)


if __name__ == "__main__":
    unittest.main()
