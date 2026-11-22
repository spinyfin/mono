"""Validate the real exports and exercise publishing without host access."""

import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

import install
from validate import DASHBOARDS, PROVIDER, canonical, load_bundle, parse, validate_bundle

HERE = Path(__file__).resolve().parent


class DashboardsTest(unittest.TestCase):
    def setUp(self):
        self.bundle = load_bundle(HERE)

    def test_live_exports_and_queue_time(self):
        self.assertEqual([24, 42], [len(self.bundle[name]["panels"]) for name in DASHBOARDS])
        panel = next(p for p in self.bundle[DASHBOARDS[0]]["panels"] if p.get("title") == "Queue time")
        self.assertEqual(["p50", "p90", "p99", "mean", "oldest queued"], [t["legendFormat"] for t in panel["targets"]])
        self.assertIn("nativelink_execution_queue_time_seconds", panel["targets"][0]["expr"])
        self.assertEqual("max(nativelink_scheduler_queue_oldest_age_seconds)", panel["targets"][-1]["expr"])

    def test_bad_panels(self):
        mutations = {
            "duplicate id": lambda p: p[1].update(id=p[0]["id"]),
            "overlap": lambda p: p[1].update(gridPos=p[0]["gridPos"]),
            "missing datasource": lambda p: p[0].pop("datasource"),
            "empty datasource": lambda p: p[0].update(datasource={}),
            "outside grid": lambda p: p[0]["gridPos"].update(w=25),
            "negative position": lambda p: p[0]["gridPos"].update(y=-1),
            "zero height": lambda p: p[0]["gridPos"].update(h=0),
        }
        for name, mutation in mutations.items():
            with self.subTest(name=name):
                bundle = copy.deepcopy(self.bundle)
                mutation(bundle[DASHBOARDS[0]]["panels"])
                with self.assertRaises(ValueError):
                    validate_bundle(bundle)

    def test_nested_ids_and_overlaps(self):
        panels = self.bundle[DASHBOARDS[0]]["panels"]
        row = next(p for p in panels if p["type"] == "row")
        row["panels"] = [copy.deepcopy(panels[0])]
        with self.assertRaisesRegex(ValueError, "duplicate panel"):
            validate_bundle(self.bundle)
        row["panels"][0]["id"] = 1000
        row["panels"].append(copy.deepcopy(row["panels"][0]))
        row["panels"][1]["id"] = 1001
        with self.assertRaisesRegex(ValueError, "overlap"):
            validate_bundle(self.bundle)

    def test_publication_boundary(self):
        mutations = [
            lambda b: b.update({"../outside.json": {}}),
            lambda b: b.pop(DASHBOARDS[1]),
            lambda b: b[DASHBOARDS[0]].update(uid="another-dashboard"),
            lambda b: b[DASHBOARDS[0]].update(id=1),
            lambda b: b[DASHBOARDS[0]].update(version=12),
            lambda b: b.update({PROVIDER: b[PROVIDER].replace("allowUiUpdates: false", "allowUiUpdates: true")}),
            lambda b: b.update({PROVIDER: b[PROVIDER].replace('folder: "NativeLink"', 'folder: "Other"')}),
            lambda b: b.update({PROVIDER: b[PROVIDER].replace("/etc/grafana/nativelink-dashboards", "/var/lib/grafana/dashboards")}),
        ]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                bundle = copy.deepcopy(self.bundle)
                mutation(bundle)
                with self.assertRaises(ValueError):
                    validate_bundle(bundle)

    def test_json_and_canonical_format(self):
        for text in ('{"broken":', '{"a": 1, "a": 2}'):
            with self.assertRaises(ValueError):
                parse(text)
        with self.assertRaises(ValueError):
            canonical({"number": float("nan")})

    def test_normalize_raw_export(self):
        raw = {
            "version": 42,
            "id": 7,
            "title": "NativeLink Overview",
            "uid": "nativelink-overview",
            "panels": [
                {
                    "type": "row",
                    "title": "Section",
                    "id": 1,
                    "gridPos": {"x": 0, "y": 0, "w": 24, "h": 1},
                    "panels": [
                        {
                            "type": "text",
                            "title": "Notes",
                            "id": 2,
                            "gridPos": {"x": 0, "y": 1, "w": 12, "h": 2},
                        }
                    ],
                },
                {
                    "type": "timeseries",
                    "title": "Queue time",
                    "id": 3,
                    "datasource": {"type": "prometheus", "uid": "PBFA97CFB590B2093"},
                    "gridPos": {"x": 0, "y": 3, "w": 12, "h": 8},
                },
            ],
        }
        # Dump with sort_keys=False to simulate an unsorted Grafana export.
        raw_text = json.dumps(raw, indent=2, sort_keys=False)
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / DASHBOARDS[0]
            result = subprocess.run(
                [sys.executable, str(HERE / "validate.py"), "--normalize", str(target)],
                input=raw_text,
                text=True,
                capture_output=True,
            )
            self.assertEqual(0, result.returncode, result.stderr)
            normalized = parse(target.read_text())
            self.assertNotIn("id", normalized)
            self.assertNotIn("version", normalized)
            row = normalized["panels"][0]
            builtin = {"type": "grafana", "uid": "-- Grafana --"}
            self.assertEqual(builtin, row["datasource"])
            self.assertEqual(builtin, row["panels"][0]["datasource"])
            self.assertEqual(canonical(normalized), target.read_text())

            # Metric panels without a datasource must still fail without writing.
            bad = copy.deepcopy(raw)
            bad["panels"][1].pop("datasource")
            bad_target = Path(tmp) / DASHBOARDS[1]
            bad_result = subprocess.run(
                [sys.executable, str(HERE / "validate.py"), "--normalize", str(bad_target)],
                input=json.dumps(bad),
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(0, bad_result.returncode)
            self.assertFalse(bad_target.exists())


class PublisherTest(unittest.TestCase):
    def setUp(self):
        self.bundle = load_bundle(HERE)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.directory = self.root / "dashboards"
        self.directory.mkdir()
        self.provider = self.root / PROVIDER
        # macOS temporary files inherit the directory's group, which can differ
        # from the process's primary group. Match it while chown is mocked.
        self.owner = (os.getuid(), self.directory.stat().st_gid)
        self.restart = Mock()
        # The test is unprivileged; record ownership requests instead of chown.
        self.chown = patch("install.os.fchown").start()
        self.addCleanup(patch.stopall)

    def publish(self):
        install.publish(self.bundle, self.directory, self.provider, self.owner, self.restart)

    def test_repeat_publish_preserves_unrelated_files_and_dashboard_mtime(self):
        unrelated = self.directory / "unrelated.json"
        unrelated.write_text("untouched")
        self.publish()
        mtimes = [(self.directory / name).stat().st_mtime_ns for name in DASHBOARDS]
        self.publish()
        self.assertEqual(mtimes, [(self.directory / name).stat().st_mtime_ns for name in DASHBOARDS])
        self.restart.assert_called_once_with()
        self.assertEqual("untouched", unrelated.read_text())
        for name in DASHBOARDS:
            self.assertEqual(self.bundle[name], parse((self.directory / name).read_text()))
            self.assertEqual(0o644, (self.directory / name).stat().st_mode & 0o777)
        self.chown.assert_any_call(unittest.mock.ANY, *self.owner)
        self.chown.assert_any_call(unittest.mock.ANY, 0, self.owner[1])
        self.bundle[DASHBOARDS[0]]["title"] = "Updated title"
        self.publish()
        self.restart.assert_called_once_with()

    def test_failed_restart_is_retried(self):
        self.restart.side_effect = OSError("service failed")
        with self.assertRaises(OSError):
            self.publish()
        self.assertTrue((self.directory / ".reload-required").exists())
        self.restart.side_effect = None
        self.publish()
        self.assertEqual(2, self.restart.call_count)
        self.assertFalse((self.directory / ".reload-required").exists())

    def test_invalid_bundle_writes_nothing(self):
        self.bundle["outside.json"] = {}
        with self.assertRaises(ValueError):
            self.publish()
        self.assertEqual([], list(self.directory.iterdir()))
        self.assertFalse(self.provider.exists())
        self.restart.assert_not_called()

    def test_symlink_and_write_failure(self):
        target = self.root / "outside"
        target.write_text("untouched")
        (self.directory / DASHBOARDS[0]).symlink_to(target)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.publish()
        self.assertEqual("untouched", target.read_text())
        (self.directory / DASHBOARDS[0]).unlink()
        with patch("install.os.replace", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.publish()
        self.assertEqual([], list(self.directory.iterdir()))
        self.restart.assert_not_called()

    def test_ci_guards_and_error_propagation(self):
        def stub(name, body):
            path = self.root / name
            path.write_text("#!/bin/bash\n" + body + "\n")
            path.chmod(0o755)

        stub("hostname", "echo headcrash")
        stub("git", "exit 0")
        stub("sudo", 'cat >/dev/null; echo "sudo called"; exit 23')
        env = dict(os.environ, PATH=f"{self.root}:{os.environ['PATH']}", BUILDKITE="true", BUILDKITE_BRANCH="main", BUILDKITE_PULL_REQUEST="false")

        def run(overrides):
            return subprocess.run(["bash", str(HERE / "publish.sh")], env=dict(env, **overrides), text=True, capture_output=True)

        for overrides in ({"BUILDKITE": ""}, {"BUILDKITE_BRANCH": "feature"}, {"BUILDKITE_PULL_REQUEST": "42"}):
            result = run(overrides)
            self.assertNotEqual(0, result.returncode)
            self.assertNotIn("sudo called", result.stdout)
        result = run({})
        self.assertEqual(23, result.returncode)
        self.assertIn("sudo called", result.stdout)
        stub("git", "exit 1")
        result = run({})
        self.assertNotEqual(0, result.returncode)
        self.assertNotIn("sudo called", result.stdout)


if __name__ == "__main__":
    unittest.main()
