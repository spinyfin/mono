#!/usr/bin/env python3
"""Verify the published fixture inputs and the archive's canonical metadata."""

import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

from gc_store_archive import check_manifest, pack


MANIFEST, GENERATOR, JJ_PIN = map(Path, sys.argv[1:4])
del sys.argv[1:4]


class ArchiveTest(unittest.TestCase):
    def test_published_inputs_match(self):
        check_manifest(MANIFEST, GENERATOR, JJ_PIN)

    def test_drift_is_actionable(self):
        original = json.loads(MANIFEST.read_text())
        generator = GENERATOR.read_text()
        jj_pin = JJ_PIN.read_text()
        variants = [
            ("generator", generator + "\n# changed generator\n", jj_pin),
            ("jj_version", generator, jj_pin.replace('"' + original["jj_version"] + '"', '"999.0.0"')),
            ("jj_sha256", generator, jj_pin.replace(next(iter(original["jj_archives"].values())), "0" * 64)),
        ]
        for key, changed_generator, changed_jj in variants:
            with self.subTest(key=key), tempfile.TemporaryDirectory() as directory:
                script, pin = Path(directory) / "generator.sh", Path(directory) / "jj_repository.bzl"
                script.write_text(changed_generator)
                pin.write_text(changed_jj)
                with self.assertRaisesRegex(ValueError, "Run tools/cube/testdata/gc_store_archive.py"):
                    check_manifest(MANIFEST, script, pin)

    def test_pack_normalizes_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = root / "store"
            store.mkdir()
            # Deliberately create entries out of order.
            (store / "z").write_bytes(b"contents")
            (store / "a").mkdir()
            long_name = "0" * 128
            (store / "a" / long_name).write_bytes(b"more contents")
            first, second = root / "first.tar.gz", root / "second.tar.gz"
            pack(store, first)
            for path in store.rglob("*"):
                os.utime(path, (123456789, 123456789))
                path.chmod(0o700 if path.is_dir() else 0o600)
            pack(store, second)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with tarfile.open(first) as archive:
                self.assertEqual(archive.getnames(), ["a", "a/" + long_name, "z"])
                for entry in archive:
                    self.assertEqual((entry.uid, entry.gid, entry.mtime), (0, 0, 0))
                    self.assertEqual((entry.uname, entry.gname), ("", ""))
                    self.assertEqual(entry.mode, 0o755 if entry.isdir() else 0o644)


if __name__ == "__main__":
    unittest.main()
