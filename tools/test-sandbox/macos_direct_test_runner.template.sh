#!/bin/bash

set -euo pipefail

if [[ "%(test_type)s" != "XCTEST" ]]; then
  printf '%s\n' "the direct macOS runner refuses UI test bundles" >&2
  exit 2
fi

test_host_path="%(test_host_path)s"
if [[ -n "${test_host_path}" ]]; then
  printf '%s\n' "the direct macOS runner refuses tests with an application host" >&2
  exit 2
fi

if [[ "${COVERAGE:-0}" == "1" ]]; then
  printf '%s\n' "coverage is not implemented by the direct macOS unit-test runner" >&2
  exit 2
fi

if [[ -n "${TEST_PREMATURE_EXIT_FILE:-}" ]]; then
  touch "${TEST_PREMATURE_EXIT_FILE}"
fi

test_tmp_dir="$(mktemp -d "${TEST_TMPDIR:?}/direct_xctest.XXXXXX")"
trap 'rm -rf "${test_tmp_dir}"' EXIT

test_bundle_path="%(test_bundle_path)s"
test_bundle_name="$(basename "${test_bundle_path}")"
test_bundle_name="${test_bundle_name%.*}"
if [[ "${test_bundle_path}" == *.xctest ]]; then
  cp -R "${test_bundle_path}" "${test_tmp_dir}"
else
  unzip -qq -d "${test_tmp_dir}" "${test_bundle_path}"
fi
chmod -R u+w "${test_tmp_dir}/${test_bundle_name}.xctest"

test_status=0
"${MONO_TEST_XCODE_DEVELOPER_DIR:?}/usr/bin/xctest" \
  "${test_tmp_dir}/${test_bundle_name}.xctest" || test_status=$?

if [[ "${test_status}" == "0" && -n "${TEST_PREMATURE_EXIT_FILE:-}" ]]; then
  rm -f "${TEST_PREMATURE_EXIT_FILE}"
fi
exit "${test_status}"
