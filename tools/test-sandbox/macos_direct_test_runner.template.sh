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
test_binary="${test_tmp_dir}/${test_bundle_name}.xctest/Contents/MacOS/${test_bundle_name}"

coverage_profraw="${test_tmp_dir}/coverage.profraw"
if [[ "${COVERAGE:-0}" == "1" ]]; then
  export LLVM_PROFILE_FILE="${coverage_profraw}"
fi

test_status=0
"${MONO_TEST_XCODE_DEVELOPER_DIR:?}/usr/bin/xctest" \
  "${test_tmp_dir}/${test_bundle_name}.xctest" || test_status=$?

if [[ "${test_status}" != "0" ]]; then
  exit "${test_status}"
fi

if [[ "${COVERAGE:-0}" == "1" ]]; then
  coverage_manifest="${COVERAGE_MANIFEST:?}"
  provided_coverage_manifest="%(test_coverage_manifest)s"
  if [[ -s "${provided_coverage_manifest:-}" ]]; then
    coverage_manifest="${provided_coverage_manifest}"
  fi

  coverage_toolchain="${MONO_TEST_XCODE_DEVELOPER_DIR:?}/Toolchains/XcodeDefault.xctoolchain/usr/bin"
  llvm_profdata="${coverage_toolchain}/llvm-profdata"
  llvm_cov="${coverage_toolchain}/llvm-cov"
  if [[ ! -x "${llvm_profdata}" || ! -x "${llvm_cov}" ]]; then
    printf '%s\n' "configured Xcode coverage tools are unavailable" >&2
    exit 126
  fi

  coverage_profdata="${test_tmp_dir}/coverage.profdata"
  "${llvm_profdata}" merge \
    "${coverage_profraw}" \
    --output "${coverage_profdata}"

  coverage_export_errors="${test_tmp_dir}/llvm-cov-export-error.txt"
  coverage_args=(
    -instr-profile "${coverage_profdata}"
    -ignore-filename-regex='.*external/.+'
    -path-equivalence=".,${PWD}"
  )
  coverage_status=0
  "${llvm_cov}" export \
    -format lcov \
    "${coverage_args[@]}" \
    "${test_binary}" \
    @"${coverage_manifest}" \
    >"${COVERAGE_OUTPUT_FILE:?}" \
    2>"${coverage_export_errors}" || coverage_status=$?
  if [[ -s "${coverage_export_errors}" || "${coverage_status}" != "0" ]]; then
    printf '%s\n' "error: while exporting coverage report" >&2
    cat "${coverage_export_errors}" >&2
    exit 1
  fi

  if [[ -n "${COVERAGE_PRODUCE_JSON:-}" ]]; then
    coverage_status=0
    "${llvm_cov}" export \
      -format text \
      "${coverage_args[@]}" \
      "${test_binary}" \
      @"${coverage_manifest}" \
      >"${TEST_UNDECLARED_OUTPUTS_DIR:?}/coverage.json" \
      2>"${coverage_export_errors}" || coverage_status=$?
    if [[ -s "${coverage_export_errors}" || "${coverage_status}" != "0" ]]; then
      printf '%s\n' "error: while exporting JSON coverage report" >&2
      cat "${coverage_export_errors}" >&2
      exit 1
    fi
  fi
fi

if [[ -n "${TEST_PREMATURE_EXIT_FILE:-}" ]]; then
  rm -f "${TEST_PREMATURE_EXIT_FILE}"
fi
