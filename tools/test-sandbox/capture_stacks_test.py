"""Failure-path coverage for diagnostics that must never signal the test."""

import os
from pathlib import Path
import subprocess
import sys
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

    def run_main(self, directory, capture, members=None):
        if members is None:
            members = [(11, "test"), (12, "child")]
        with (
            patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": directory, "TEST_TARGET": "//test"}),
            patch("sys.argv", ["capture_stacks.py", "11", "/runtime"]),
            patch.object(capture_stacks, "group_members", return_value=members),
            patch.object(capture_stacks, "capture", capture),
        ):
            capture_stacks.main()

    def test_signal_then_cleanup_capture_each_member_once(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(capture_stacks, "capture") as capture:
                self.run_main(directory, capture)
                self.run_main(directory, capture)
            self.assertEqual(capture.call_count, 2)
            self.assertEqual({call.args[0] for call in capture.call_args_list}, {11, 12})
            output = Path(directory) / "timeout-11"
            manifest = (output / "processes.txt").read_text()
            self.assertIn("test=//test group=11", manifest)
            self.assertIn("12 child", manifest)
            self.assertTrue((output / capture_stacks.COMPLETE).exists())

    def test_abandoned_claim_without_complete_is_recaptured(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "timeout-11"
            output.mkdir()
            (output / capture_stacks.LOCK).write_text("dead capturer")
            with patch.object(capture_stacks, "capture") as capture:
                self.run_main(directory, capture)
            self.assertEqual(capture.call_count, 2)
            self.assertTrue((output / capture_stacks.COMPLETE).exists())

    def test_enumeration_failure_lets_a_successor_capture(self):
        with tempfile.TemporaryDirectory() as directory:
            with (
                patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": directory, "TEST_TARGET": "//test"}),
                patch("sys.argv", ["capture_stacks.py", "11", "/runtime"]),
                patch.object(
                    capture_stacks,
                    "group_members",
                    side_effect=subprocess.CalledProcessError(1, ["ps"]),
                ),
                patch.object(capture_stacks, "capture") as capture,
            ):
                capture_stacks.main()
            output = Path(directory) / "timeout-11"
            self.assertTrue(output.is_dir())
            self.assertFalse((output / capture_stacks.COMPLETE).exists())
            self.assertEqual(capture.call_count, 0)
            with patch.object(capture_stacks, "capture") as capture:
                self.run_main(directory, capture)
            self.assertEqual(capture.call_count, 2)
            self.assertTrue((output / capture_stacks.COMPLETE).exists())

    def test_live_holder_blocks_successor_until_it_exits(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "timeout-11"
            output.mkdir()
            holder = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    "import fcntl, sys, time\n"
                    "from pathlib import Path\n"
                    "lock = (Path(sys.argv[1]) / sys.argv[2]).open('a+')\n"
                    "fcntl.flock(lock.fileno(), fcntl.LOCK_EX)\n"
                    "sys.stdout.write('held\\n')\n"
                    "sys.stdout.flush()\n"
                    "time.sleep(30)\n",
                    str(output),
                    capture_stacks.LOCK,
                ],
                stdout=subprocess.PIPE,
            )
            try:
                self.assertEqual(holder.stdout.readline(), b"held\n")
                with patch.object(capture_stacks, "capture") as capture:
                    self.run_main(directory, capture)
                self.assertEqual(capture.call_count, 0)
                self.assertFalse((output / capture_stacks.COMPLETE).exists())
            finally:
                holder.kill()
                holder.wait()

    def test_missing_undeclared_dir_is_not_recreated(self):
        missing = Path(tempfile.mkdtemp()) / "gone"
        with (
            patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": str(missing)}),
            patch("sys.argv", ["capture_stacks.py", "11", "/runtime"]),
            patch.object(capture_stacks, "group_members", return_value=[(11, "test")]),
            patch.object(capture_stacks, "capture") as capture,
        ):
            capture_stacks.main()
        self.assertFalse(missing.exists())
        self.assertEqual(capture.call_count, 1)
        self.assertEqual(capture.call_args.args[0], 11)


if __name__ == "__main__":
    unittest.main()
