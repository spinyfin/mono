#!/bin/bash

set -euo pipefail

if [[ "${MONO_TEST_XCODE_TOOLCHAIN:-}" != "1" ]]; then
  printf '%s\n' "xcodebuild is only available to audited Apple test targets" >&2
  exit 126
fi

derived_data="${TEST_TMPDIR:?}/xcode-derived-data"
mkdir -p "${derived_data}"

previous=""
for argument in "$@"; do
  if [[ "${previous}" == "-xctestrun" ]]; then
    sed \
      -e "s#__MONO_TEST_TMPDIR__#${TEST_TMPDIR}#g" \
      -e "s#__MONO_TEST_PROCESS_TMPDIR__#${TMPDIR}#g" \
      -e "s#__MONO_TEST_HOME__#${HOME}#g" \
      -e "s#__MONO_TEST_HOST_TMPDIR__#${MONO_TEST_HOST_TMPDIR:-}#g" \
      -e "s#__MONO_TEST_UNDECLARED_OUTPUTS_DIR__#${TEST_UNDECLARED_OUTPUTS_DIR:?}#g" \
      -e "s#__MONO_TEST_XCODE_DEVELOPER_DIR__#${MONO_TEST_XCODE_DEVELOPER_DIR:?}#g" \
      -i "" \
      "${argument}"
    break
  fi
  previous="${argument}"
done

exec /usr/bin/xcodebuild \
  -derivedDataPath "${derived_data}" \
  -collect-test-diagnostics never \
  "$@"
