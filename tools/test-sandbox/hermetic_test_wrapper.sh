#!/bin/bash
#
# Bazel's generated test wrapper needs /bin and /usr/bin while it prepares the
# test action. Once it delegates here, remove the host PATH before any
# repository-owned test code runs. Tests that need an executable must receive
# its path through a declared Bazel dependency.

set -euo pipefail

if [[ "${OSTYPE:-}" == darwin* ]]; then
  # Bazel's local TestRunner temp path is long enough to overflow SUN_LEN once
  # a test appends a tempfile component and a socket name. Give each action a
  # short, unique sandbox-owned root and remove it when the test exits.
  test_tmpdir="$(/usr/bin/mktemp -d /private/tmp/mono-test.XXXXXX)"
else
  test_tmpdir="${TEST_TMPDIR:?}"
fi
export TEST_TMPDIR="${test_tmpdir}"
export TMPDIR="${test_tmpdir}"
export TMP="${test_tmpdir}"
export TEMP="${test_tmpdir}"

# Some tests exercise shell quoting and subprocess behavior. Give them a fixed
# allowlist of OS runtime tools without exposing the developer's PATH (and in
# particular Homebrew/user-installed gh, bk, codex, claude, or cube).
test_bin="${test_tmpdir}/mono-test-bin"
mkdir -p "${test_bin}"
for tool in \
  awk basename bash cat chmod cp cut date dirname echo env false find git grep \
  head ln mkdir mkfifo mktemp mv od printf pwd python3 rm sed sh shasum sleep \
  sort tail tee touch tr true uname unzip wc xcodebuild xcrun; do
  candidates=("/bin/${tool}" "/usr/bin/${tool}")
  if [[ "${tool}" == "python3" && -x /opt/homebrew/bin/python3 ]]; then
    candidates=("/opt/homebrew/bin/python3" "${candidates[@]}")
  fi
  for candidate in "${candidates[@]}"; do
    if [[ -x "${candidate}" ]]; then
      /bin/ln -sf "${candidate}" "${test_bin}/${tool}"
      break
    fi
  done
done
export PATH="${test_bin}"

unset ANTHROPIC_API_KEY OPENAI_API_KEY GITHUB_TOKEN GH_TOKEN

if [[ "${OSTYPE:-}" == darwin* ]]; then
  profile="${TEST_TMPDIR:?}/mono-test.sb"

  {
    printf '%s\n' '(version 1)' '(allow default)'

    if [[ "${MONO_TEST_ALLOW_OUTSIDE_WRITES:-}" != "1" ]]; then
      printf '%s\n' \
        '(deny file-write*' \
        '  (require-any' \
        '    (vnode-type REGULAR-FILE)' \
        '    (vnode-type DIRECTORY)' \
        '    (vnode-type SYMLINK)))'
      for writable_path in \
        /dev \
        "${TEST_TMPDIR:-}" \
        "${TEST_UNDECLARED_OUTPUTS_DIR:-}" \
        "${TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR:-}" \
        "${COVERAGE_DIR:-}" \
        "${XML_OUTPUT_FILE:-}" \
        "${TEST_SHARD_STATUS_FILE:-}" \
        "${TEST_PREMATURE_EXIT_FILE:-}" \
        "${TEST_WARNINGS_OUTPUT_FILE:-}"; do
        if [[ -n "${writable_path}" ]]; then
          escaped="${writable_path//\\/\\\\}"
          escaped="${escaped//\"/\\\"}"
          printf '  (allow file-write* (subpath "%s"))\n' "${escaped}"
        fi
      done
    fi

    if [[ "${MONO_TEST_ALLOW_NETWORK:-}" != "1" ]]; then
      printf '%s\n' \
        '(deny network*)' \
        '(allow network-inbound (local ip "localhost:*"))' \
        '(allow network* (remote ip "localhost:*"))' \
        '(allow network* (remote unix-socket))'
    fi
  } >"${profile}"

  set +e
  /usr/bin/sandbox-exec -f "${profile}" "$@"
  test_status=$?
  set -e
  /bin/rm -rf "${test_tmpdir}"
  exit "${test_status}"
fi

exec "$@"
