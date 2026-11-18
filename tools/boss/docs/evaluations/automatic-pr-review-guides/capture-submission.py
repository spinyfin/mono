#!/usr/bin/env python3
"""Capture the production submission contract without contacting a Boss engine."""

import json
import os
import subprocess
import sys

if len(sys.argv) != 5 or sys.argv[1:4] != ["propose", "review-guide", "--body"]:
    sys.exit("Only propose review-guide --body <literal Markdown> is supported")

result = subprocess.run(
    [
        os.environ["CORPUS_TOOL"], "validate",
        "--packet", os.environ["CORPUS_PACKET"],
        "--workspace", os.environ["CORPUS_WORKSPACE"],
        "--body", sys.argv[4],
    ],
    capture_output=True, text=True, timeout=120,
)
if result.returncode:
    sys.exit(result.stderr)
print(json.dumps({"corpus_submission": sys.argv[4]}))
