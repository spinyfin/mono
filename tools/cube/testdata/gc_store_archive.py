#!/usr/bin/env python3
"""Generate the shared GC fixture with Bazel's pinned jj, then pack and pin it.

Run from the checkout:
  python3 tools/cube/testdata/gc_store_archive.py --output-dir /tmp/cube-gc-data \
      --release-tag cube-test-data-gc-store-v1
Upload gc-store.tar.gz to that dedicated GitHub release, and copy the generated
gc_store.json beside this script. Never commit the archive or generated store.
"""

import argparse
import ast
import gzip
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile
import time


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inputs(generator, jj_pin):
    text = jj_pin.read_text()
    default = re.search(r'_DEFAULT_VERSION = "([\d.]+)"', text)
    releases = re.search(r"_JJ_RELEASES = (\{.*?\n\})", text, re.S)
    if not default or not releases:
        raise ValueError("Cannot read the pinned jj version and archive checksums")
    version = default[1]
    archives = ast.literal_eval(releases[1]).get(version)
    if archives is None:
        raise ValueError(f"No jj archive checksums are pinned for version {version}")
    return {
        "generator_sha256": sha256(generator),
        "jj_version": version,
        "jj_archives": archives,
    }


def check_manifest(manifest, generator, jj_pin):
    recorded = json.loads(manifest.read_text())
    for key, value in inputs(generator, jj_pin).items():
        if recorded.get(key) != value:
            raise ValueError(
                f"GC fixture {key} drifted. Run tools/cube/testdata/gc_store_archive.py "
                "with a new --release-tag, upload the archive, and update the "
                "gc_store.json pin."
            )


def pack(store, archive):
    """Canonical tar/gzip metadata; jj's store contents are intentionally real."""
    with archive.open("wb") as raw:
        with gzip.GzipFile(filename="", fileobj=raw, mode="wb", compresslevel=9, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path in sorted(store.rglob("*")):
                    entry = tar.gettarinfo(str(path), arcname=path.relative_to(store).as_posix())
                    entry.uid = entry.gid = entry.mtime = 0
                    entry.uname = entry.gname = ""
                    entry.mode = 0o755 if entry.isdir() else 0o644
                    if entry.isfile():
                        with path.open("rb") as contents:
                            tar.addfile(entry, contents)
                    else:
                        tar.addfile(entry)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--release-tag", required=True)
    args = parser.parse_args()
    if not args.release_tag.startswith("cube-test-data-"):
        parser.error("use a dedicated cube-test-data-* release, separate from Boss releases")
    repo = Path(__file__).resolve().parents[3]
    generator = repo / "tools/cube/testdata/gc_store.sh"
    jj_pin = repo / "tools/boss/engine/recovery/jj_repository.bzl"
    manifest = inputs(generator, jj_pin)
    subprocess.run(["bazel", "build", "@jj_test_tools//:jj"], cwd=repo, check=True)
    binary = subprocess.check_output(
        ["bazel", "cquery", "@jj_test_tools//:jj", "--output=files"], cwd=repo, text=True
    ).strip()
    execution_root = subprocess.check_output(
        ["bazel", "info", "execution_root"], cwd=repo, text=True
    ).strip()
    jj = (Path(execution_root) / binary).resolve()
    version = subprocess.check_output([str(jj), "--version"], text=True).strip()
    if version.split("-")[0] != "jj " + manifest["jj_version"]:
        raise ValueError(f"Unexpected pinned binary: {version}")
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    store = output / "store"
    if store.exists():
        parser.error(f"{store} already exists; choose a new output directory")
    started = time.monotonic()
    subprocess.run(["bash", str(generator), str(jj), str(store)], cwd=repo, check=True)
    print(f"Generated store in {time.monotonic() - started:.2f}s", flush=True)
    archive = output / "gc-store.tar.gz"
    started = time.monotonic()
    pack(store, archive)
    print(f"Packed store in {time.monotonic() - started:.2f}s", flush=True)
    manifest.update({
        "url": f"https://github.com/spinyfin/mono/releases/download/{args.release_tag}/{archive.name}",
        "sha256": sha256(archive),
    })
    pin = output / "gc_store.json"
    pin.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"Created {archive} ({archive.stat().st_size} bytes) and {pin}")


if __name__ == "__main__":
    main()
