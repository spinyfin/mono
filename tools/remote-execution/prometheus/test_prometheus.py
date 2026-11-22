"""Tests for Prometheus config validation, publishing, and node_exporter setup."""

import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

import install
from validate import validate_config

HERE = Path(__file__).resolve().parent
NODE_EXPORTER_DIR = HERE.parent / "node_exporter"


class PrometheusConfigTest(unittest.TestCase):
    def setUp(self):
        self.valid_text = (HERE / "prometheus.yml").read_text()

    def test_versioned_config_is_valid(self):
        config = validate_config(self.valid_text)
        jobs = {job["job_name"]: job for job in config["scrape_configs"]}
        self.assertEqual({"prometheus", "nativelink", "nativelink-pool", "node"}, set(jobs))
        instances = [sc["labels"]["instance"] for sc in jobs["node"]["static_configs"]]
        self.assertEqual(
            ["headcrash", "anaplian", "arbitrary", "determinist", "rascal", "skaffen", "zakalwe"],
            instances,
        )

    def test_rejects_wildcard_and_non_loopback_hub_targets(self):
        bad_configs = [
            self.valid_text.replace("100.96.159.44:9100", "0.0.0.0:9100"),
            self.valid_text.replace("127.0.0.1:8889", "100.96.159.44:8889"),
            self.valid_text.replace("worker_id: anaplian, ", ""),
            self.valid_text.replace("pool: macos", "pool: linux", 1),
            self.valid_text.replace("job_name: nativelink-pool", "job_name: nativelink"),
            self.valid_text.replace("instance: zakalwe", "instance: anaplian"),
            self.valid_text.replace("100.97.39.14:9100", "192.168.1.50:9100"),
            "global:\n\tscrape_interval: 15s\n",
            "global:\n  scrape_interval: 15s\n  scrape_interval: 30s\n",
        ]
        for bad in bad_configs:
            with self.subTest(bad=bad[:60]):
                with self.assertRaises(ValueError):
                    validate_config(bad)

    def test_publish_validates_with_promtool_before_writing(self):
        with tempfile.TemporaryDirectory() as tmp:
            config_path = Path(tmp) / "prometheus.yml"
            config_path.write_text("original\n")
            check_config = Mock(side_effect=subprocess.CalledProcessError(1, "promtool"))
            reload_service = Mock()

            with patch.object(os, "fchown", lambda *_: None):
                with self.assertRaises(subprocess.CalledProcessError):
                    install.publish(self.valid_text, config_path, check_config, reload_service)

            self.assertEqual("original\n", config_path.read_text())
            reload_service.assert_not_called()

    def test_publish_reloads_on_change_and_skips_when_unchanged(self):
        with tempfile.TemporaryDirectory() as tmp:
            config_path = Path(tmp) / "prometheus.yml"
            check_config = Mock()
            reload_service = Mock()
            owner = (os.getuid(), os.getgid())

            reloaded = install.publish(self.valid_text, config_path, check_config, reload_service, owner=owner)
            self.assertTrue(reloaded)
            self.assertEqual(self.valid_text, config_path.read_text())
            reload_service.assert_called_once_with()
            self.assertFalse((Path(tmp) / ".reload-required").exists())

            mtime_before = config_path.stat().st_mtime_ns
            reloaded_again = install.publish(self.valid_text, config_path, check_config, reload_service, owner=owner)
            self.assertFalse(reloaded_again)
            self.assertEqual(mtime_before, config_path.stat().st_mtime_ns)
            reload_service.assert_called_once_with()

    def test_publish_retries_reload_after_failed_reload(self):
        with tempfile.TemporaryDirectory() as tmp:
            config_path = Path(tmp) / "prometheus.yml"
            check_config = Mock()
            reload_service = Mock(side_effect=[subprocess.CalledProcessError(1, "systemctl"), None])
            owner = (os.getuid(), os.getgid())

            with self.assertRaises(subprocess.CalledProcessError):
                install.publish(self.valid_text, config_path, check_config, reload_service, owner=owner)
            self.assertTrue((Path(tmp) / ".reload-required").exists())

            reloaded = install.publish(self.valid_text, config_path, check_config, reload_service, owner=owner)
            self.assertTrue(reloaded)
            self.assertEqual(2, reload_service.call_count)
            self.assertFalse((Path(tmp) / ".reload-required").exists())


class NodeExporterSetupTest(unittest.TestCase):
    def test_macos_plist_has_keepalive_and_throttle(self):
        plist_bytes = (NODE_EXPORTER_DIR / "com.brianduff.node_exporter.plist").read_bytes()
        plist = plistlib.loads(plist_bytes)
        self.assertEqual("com.brianduff.node_exporter", plist["Label"])
        self.assertIs(True, plist["RunAtLoad"])
        self.assertIs(True, plist["KeepAlive"])
        self.assertGreater(plist["ThrottleInterval"], 0)
        self.assertIn("USER_HOME", plist["ProgramArguments"][1])

    def test_scripts_never_bind_wildcard(self):
        for name in ("run-macos.sh", "setup-macos.sh", "setup-ubuntu.sh"):
            lines = [
                line for line in (NODE_EXPORTER_DIR / name).read_text().splitlines()
                if not line.lstrip().startswith("#")
            ]
            self.assertNotIn("0.0.0.0", "\n".join(lines), msg=f"{name} must not bind to 0.0.0.0")

    def test_macos_setup_migrates_homebrew_service(self):
        setup = (NODE_EXPORTER_DIR / "setup-macos.sh").read_text()
        self.assertIn("sh.brew.node_exporter", setup)
        self.assertIn("com.brianduff.node_exporter", setup)
        runner = (NODE_EXPORTER_DIR / "run-macos.sh").read_text()
        self.assertIn("tailscale", runner)
        self.assertIn(":9100", runner)

    def test_ubuntu_setup_supports_hub_and_worker_modes(self):
        setup = (NODE_EXPORTER_DIR / "setup-ubuntu.sh").read_text()
        self.assertIn("127.0.0.1:9100", setup)
        self.assertIn("prometheus-node-exporter", setup)
        self.assertIn("tailscale", setup)


if __name__ == "__main__":
    unittest.main()
