#!/usr/bin/env bash
# Run the production corpus recipe from the Bazel workspace root.
set -euo pipefail
exec python3 "$(dirname "${BASH_SOURCE[0]}")/run-corpus.py" "$@"
