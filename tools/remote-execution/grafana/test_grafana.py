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

HERE = Path(__file__).resolve().parent
sys.path.append(str(HERE.parent))

import install
from validate import DASHBOARDS, PROVIDER, canonical, load_bundle, parse, resolve_normalize_target, validate_bundle


class DashboardsTest(unittest.TestCase):
    def setUp(self):
        self.bundle = load_bundle(HERE)

    def test_live_exports_and_queue_time(self):
        overview, workers = (self.bundle[name]["panels"] for name in DASHBOARDS)
        self.assertEqual([37, 54], [len(overview), len(workers)])
        titles = [p.get("title") for p in overview]
        self.assertIn("Hub health", titles)
        self.assertIn("Cache eviction", titles)
        self.assertEqual(
            ["Hub uptime", "Hub restarts (24h)", "OOM kills on headcrash (24h)", "Hub unit state"],
            [p.get("title") for p in overview if p.get("title") in {
                "Hub uptime", "Hub restarts (24h)", "OOM kills on headcrash (24h)", "Hub unit state",
            }],
        )
        annotations = [a.get("name") for a in self.bundle[DASHBOARDS[0]].get("annotations", {}).get("list", [])]
        self.assertIn("Hub restart", annotations)
        self.assertIn("OOM kill on headcrash", annotations)
        panel = next(p for p in overview if p.get("title") == "Queue time")
        self.assertEqual(["p50", "p90", "p99", "mean", "oldest queued"], [t["legendFormat"] for t in panel["targets"]])
        self.assertIn("nativelink_execution_queue_time_seconds", panel["targets"][0]["expr"])
        self.assertEqual("max(nativelink_scheduler_queue_oldest_age_seconds)", panel["targets"][-1]["expr"])
        eviction = next(p for p in overview if p.get("title") == "Evicted in the last hour")
        self.assertTrue(any("nativelink_hub_cas_evicted_bytes_total" in t["expr"] for t in eviction["targets"]))
        node_exprs = [
            t["expr"]
            for p in workers
            for t in p.get("targets") or []
            if isinstance(t, dict) and "node_" in t.get("expr", "")
        ]
        self.assertEqual(15, len(node_exprs))
        for expr in node_exprs:
            self.assertIn("by (instance)", expr)

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
            self.assertIn(str(target.resolve()), result.stdout)
            self.assertIn("bin/checkleft fix", result.stdout)
            normalized = parse(target.read_text())
            builtin = {"type": "grafana", "uid": "-- Grafana --"}
            expected = copy.deepcopy(raw)
            expected.pop("id")
            expected.pop("version")
            expected["panels"][0]["datasource"] = builtin
            expected["panels"][0]["panels"][0]["datasource"] = builtin
            self.assertEqual(expected, normalized)

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

    def test_normalize_target_resolution(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            dashboards_dir = root / "dashboards"
            dashboards_dir.mkdir()
            cwd_dir = root / "cwd"
            cwd_dir.mkdir()
            (cwd_dir / DASHBOARDS[0]).write_text("{}")

            old_cwd = Path.cwd()
            os.chdir(cwd_dir)
            try:
                resolved = resolve_normalize_target(DASHBOARDS[0], dashboards_dir=dashboards_dir)
                self.assertEqual((dashboards_dir / DASHBOARDS[0]).resolve(), resolved)
                explicit = cwd_dir / DASHBOARDS[0]
                self.assertEqual(explicit.resolve(), resolve_normalize_target(str(explicit), dashboards_dir=dashboards_dir))
            finally:
                os.chdir(old_cwd)

            with self.assertRaises(ValueError):
                resolve_normalize_target("other.json", dashboards_dir=dashboards_dir)

    def test_pipeline_force_matches_pipeline_without_if_changed(self):
        pipeline_lines = (HERE / "pipeline.yml").read_text().splitlines()
        force_text = (HERE / "pipeline-force.yml").read_text()
        filtered = []
        skip_block = False
        for line in pipeline_lines:
            if line.startswith("    if_changed:"):
                skip_block = True
                continue
            if skip_block:
                if line.startswith("      ") or not line.strip():
                    continue
                skip_block = False
            filtered.append(line)
        self.assertEqual("\n".join(filtered) + "\n", force_text)
        self.assertNotIn("if_changed", force_text)
        self.assertIn('queue: "nativelink-grafana"', force_text)
        self.assertIn("concurrency: 1", force_text)
        self.assertIn('if: build.branch == "main" && build.pull_request.id == null', force_text)


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
