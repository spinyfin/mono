"""Keep the Rust roster and the crew portrait grid in step.

The engine persona roster (Rust `ROSTER`) is, by design, the 56-character
portrait grid in reading order minus the coordinator, Picard. This test parses
the real sources, so a hand edit to any one of them that forgets the others
fails here rather than at runtime, where a persona would show the wrong face.
"""

import os
from pathlib import Path
import re
import struct
import sys

COORDINATOR = "Picard"
GRID_ROWS, GRID_COLUMNS = 7, 8

ROOT = Path(os.environ["TEST_SRCDIR"]) / os.environ.get("TEST_WORKSPACE", "_main") / "tools/boss"
RUST_NAMES = ROOT / "protocol/src/worker_names.rs"
SWIFT_GRID = ROOT / "app-macos/Sources/TrekPortraitGrid.swift"
GRID_PNG = ROOT / "app-macos/Sources/Resources/TrekIcons/trek-crew-grid.png"

QUOTED = re.compile(r'"((?:[^"\\]|\\.)*)"')


def block(text, opener, closer, label):
    """Text between `opener` and the first following `closer`."""
    start = text.find(opener)
    if start < 0:
        sys.exit(f"cannot find `{opener}` in {label}")
    start += len(opener)
    end = text.find(closer, start)
    if end < 0:
        sys.exit(f"cannot find the end of `{opener}` in {label}")
    return text[start:end]


def strip_comments(text):
    return re.sub(r"//[^\n]*", "", text)


def rust_roster():
    body = block(RUST_NAMES.read_text(), "pub const ROSTER: &[&str] = &[", "];", "worker_names.rs")
    return QUOTED.findall(strip_comments(body))


def swift_grid_rows():
    body = block(SWIFT_GRID.read_text(), "static let names: [[String]] = [", "\n    ]\n", "TrekPortraitGrid.swift")
    return [QUOTED.findall(line) for line in strip_comments(body).splitlines() if QUOTED.search(line)]


def swift_grid_constants(text):
    return (
        int(re.search(r"static let imageWidth = (\d+)", text).group(1)),
        int(re.search(r"static let imageHeight = (\d+)", text).group(1)),
    )


def swift_crop_rects():
    body = block(SWIFT_GRID.read_text(), "static let cropRects: [[PixelRect]] = [", "\n    ]\n", "TrekPortraitGrid.swift")
    return [tuple(map(int, m)) for m in re.findall(r"rect\((\d+), (\d+), (\d+), (\d+)\)", body)]


def png_size(path):
    header = path.read_bytes()[:24]
    if header[:8] != b"\x89PNG\r\n\x1a\n" or header[12:16] != b"IHDR":
        sys.exit(f"{path} is not a PNG")
    return struct.unpack(">II", header[16:24])


def main():
    failures = []

    rows = swift_grid_rows()
    flat = [name for row in rows for name in row]
    if len(rows) != GRID_ROWS or any(len(row) != GRID_COLUMNS for row in rows):
        failures.append(f"grid must be {GRID_ROWS}x{GRID_COLUMNS}; got rows of {[len(r) for r in rows]}")
    if len(set(flat)) != len(flat):
        dupes = sorted({n for n in flat if flat.count(n) > 1})
        failures.append(f"grid names must be unique; duplicated: {dupes}")
    if COORDINATOR not in flat:
        failures.append(f"{COORDINATOR} must have a grid portrait")

    expected_roster = [name for name in flat if name != COORDINATOR]
    rust = rust_roster()
    if rust != expected_roster:
        failures.append(
            "Rust ROSTER differs from the grid in reading order minus "
            f"{COORDINATOR}.\n  rust only: {[n for n in rust if n not in expected_roster]}"
            f"\n  grid only: {[n for n in expected_roster if n not in rust]}"
            f"\n  rust:  {rust}\n  grid:  {expected_roster}"
        )

    width, height = swift_grid_constants(SWIFT_GRID.read_text())
    if png_size(GRID_PNG) != (width, height):
        failures.append(
            f"{GRID_PNG.name} is {png_size(GRID_PNG)} but TrekPortraitGrid.imageWidth/Height "
            f"say {(width, height)}"
        )

    rects = swift_crop_rects()
    if len(rects) != GRID_ROWS * GRID_COLUMNS:
        failures.append(f"expected {GRID_ROWS * GRID_COLUMNS} crop rects, found {len(rects)}")
    for index, (x, y, w, h) in enumerate(rects):
        if x < 0 or y < 0 or x + w > width or y + h > height:
            failures.append(f"crop rect #{index} {(x, y, w, h)} leaves the {width}x{height} sheet")

    if failures:
        sys.exit("\n".join(failures))
    print(f"ok: {len(rust)} worker names, {len(flat)} grid names, {len(rects)} crop rects")


if __name__ == "__main__":
    main()
