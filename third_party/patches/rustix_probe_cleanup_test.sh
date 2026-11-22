#!/usr/bin/env bash
# Inspect the real build-script output, not the text of the patch.
set -euo pipefail
out_dir="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/${1:?}"
if [[ ! -d "$out_dir" ]]; then
  echo "Missing rustix build-script output directory: $out_dir" >&2
  exit 1
fi
if [[ -e "${out_dir}/rustix_test_can_compile" ]]; then
  echo "rustix leaked temporary compiler metadata into downstream action inputs" >&2
  exit 1
fi
