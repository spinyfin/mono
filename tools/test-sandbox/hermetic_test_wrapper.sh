#!/bin/bash

set -euo pipefail

# macOS rejects nested Seatbelt profiles. This single meta-test therefore runs
# as the outer supervisor and invokes a normal, fully enforced wrapper itself.
if [[ "${MONO_TEST_WRAPPER_META_TEST:-}" == "1" ]]; then
  unset MONO_TEST_WRAPPER_META_TEST
  exec "$@"
fi

runtime_root="${TEST_SRCDIR:?}/+test_runtime_repository+test_runtime_tools"
runtime_bin="${runtime_root}/bin"
runtime_manifest="${runtime_root}/manifest"
main_runfiles="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
xcode_bin="${main_runfiles}/tools/test-sandbox"
host_home="${HOME:?}"
host_user_home=""
if [[ -n "${USER:-}" && -d "/Users/${USER}" ]]; then
  host_user_home="/Users/${USER}"
fi

if [[ ! -d "${runtime_bin}" || ! -f "${runtime_manifest}" ]]; then
  printf '%s\n' "audited Bazel test runtime is unavailable" >&2
  exit 126
fi

if [[ "${OSTYPE:-}" == darwin* ]]; then
  test_tmpdir="$("${runtime_bin}/mktemp" -d /tmp/mono-test.XXXXXX)"
  owns_test_tmpdir=1
else
  test_tmpdir="${TEST_TMPDIR:?}"
  owns_test_tmpdir=0
fi

export TEST_TMPDIR="${test_tmpdir}"
export TMPDIR="${test_tmpdir}/tmp"
export TMP="${TMPDIR}"
export TEMP="${TMPDIR}"
export HOME="${test_tmpdir}/home"
export CFFIXED_USER_HOME="${HOME}"
"${runtime_bin}/mkdir" -p "${HOME}" "${TMPDIR}"
if [[ "${TEST_TMPDIR}" == /tmp/* ]]; then
  test_tmpdir_alias="/private${TEST_TMPDIR}"
else
  test_tmpdir_alias="/tmp/${TEST_TMPDIR#/private/tmp/}"
fi

# Only Bazel-declared/audited runtime inputs are searchable. The xcodebuild
# shim additionally requires an explicit target marker before it delegates to
# the registered local Apple toolchain.
export PATH="${xcode_bin}:${runtime_bin}"

unset ANTHROPIC_API_KEY OPENAI_API_KEY GITHUB_TOKEN GH_TOKEN

child_pid=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ -n "${child_pid}" ]] && kill -0 "${child_pid}" 2>/dev/null; then
    kill -TERM "-${child_pid}" 2>/dev/null || true
    wait "${child_pid}" 2>/dev/null || true
  fi
  if [[ "${owns_test_tmpdir}" == "1" ]]; then
    "${runtime_bin}/rm" -rf "${test_tmpdir}"
  fi
  exit "${status}"
}

forward_signal() {
  local signal="$1"
  if [[ -n "${child_pid}" ]]; then
    kill "-${signal}" "-${child_pid}" 2>/dev/null || true
  fi
}

trap cleanup EXIT
trap 'forward_signal INT' INT
trap 'forward_signal TERM' TERM
trap 'forward_signal HUP' HUP

if [[ "${OSTYPE:-}" != darwin* ]]; then
  trap - EXIT INT TERM HUP
  exec "$@"
fi

profile="${TEST_TMPDIR:?}/mono-test.sb"
test_command="$("${runtime_bin}/python3" -c \
  'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1")"

escape_profile_path() {
  local escaped="$1"
  escaped="${escaped//\\/\\\\}"
  escaped="${escaped//\"/\\\"}"
  printf '%s' "${escaped}"
}

emit_write_deny_except() {
  local protected_path="$1"
  shift
  printf '%s\n' \
    '  (deny file-write*' \
    '    (require-all' \
    "      (subpath \"$(escape_profile_path "${protected_path}")\")"
  for writable_path in "$@"; do
    if [[ -n "${writable_path}" ]]; then
      printf '      (require-not (subpath "%s"))\n' \
        "$(escape_profile_path "${writable_path}")"
    fi
  done
  printf '%s\n' '    ))'
}

{
  printf '%s\n' \
    '(version 1)' \
    '(allow default)'

  if [[ "${MONO_TEST_XCODE_TOOLCHAIN:-}" == "1" ]]; then
    # XCTest processes are launched by testmanagerd, which does not preserve
    # per-action Seatbelt path extensions. Deny writes to the host user tree
    # except Bazel-owned result paths, and to shared /tmp except this action's
    # private root. Remaining system-service writes are constrained by explicit
    # protected roots rather than a broad outside-write bypass.
    emit_write_deny_except \
      /Users \
      "${TEST_UNDECLARED_OUTPUTS_DIR:-}" \
      "${TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR:-}" \
      "${COVERAGE_DIR:-}" \
      "${XML_OUTPUT_FILE:-}" \
      "${TEST_SHARD_STATUS_FILE:-}" \
      "${TEST_PREMATURE_EXIT_FILE:-}" \
      "${TEST_WARNINGS_OUTPUT_FILE:-}"
    emit_write_deny_except /private/tmp "${test_tmpdir_alias}"
    for protected_path in \
      /Applications \
      /cores \
      /Library \
      /opt \
      /System \
      /private/var/tmp \
      /usr/local \
      /var/tmp \
      /Volumes; do
      printf '  (deny file-write* (subpath "%s"))\n' \
        "$(escape_profile_path "${protected_path}")"
    done
  else
    printf '%s\n' \
      '(deny file-write*' \
      '  (require-all' \
      '    (require-any' \
      '      (vnode-type REGULAR-FILE)' \
      '      (vnode-type DIRECTORY)' \
      '      (vnode-type SYMLINK))'

    for writable_path in \
      /dev \
      "${TEST_TMPDIR:-}" \
      "${test_tmpdir_alias}" \
      "${TEST_UNDECLARED_OUTPUTS_DIR:-}" \
      "${TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR:-}" \
      "${COVERAGE_DIR:-}" \
      "${XML_OUTPUT_FILE:-}" \
      "${TEST_SHARD_STATUS_FILE:-}" \
      "${TEST_PREMATURE_EXIT_FILE:-}" \
      "${TEST_WARNINGS_OUTPUT_FILE:-}"; do
      if [[ -n "${writable_path}" ]]; then
        printf '    (require-not (subpath "%s"))\n' \
          "$(escape_profile_path "${writable_path}")"
      fi
    done
    printf '%s\n' '  ))'
  fi

  # Keychain contents and their broker IPC remain outside every repository
  # test, including Xcode-backed tests.
  printf '%s\n' \
    '  (deny file-read* (subpath "/Library/Keychains"))'
  for keychain_root in \
    "${host_home}/Library/Keychains" \
    "${host_user_home:+${host_user_home}/Library/Keychains}"; do
    if [[ -n "${keychain_root}" ]]; then
      printf '  (deny file-read* (subpath "%s"))\n' \
        "$(escape_profile_path "${keychain_root}")"
    fi
  done
  printf '%s\n' \
    '  (deny mach-lookup' \
    '    (global-name "com.apple.securityd")' \
    '    (global-name "com.apple.securityd.general")' \
    '    (global-name "com.apple.securityd.xpc")' \
    '    (global-name "com.apple.SecurityServer"))'

  # Executable provenance is enforced independently of PATH. Bazel runfiles,
  # the private action root, the test entry point, and the audited runtime
  # manifest are the only default executable sources.
  printf '%s\n' \
    '  (deny process-exec)' \
    "  (allow process-exec (subpath \"$(escape_profile_path "${TEST_SRCDIR}")\"))" \
    "  (allow process-exec (subpath \"$(escape_profile_path "${TEST_TMPDIR}")\"))" \
    "  (allow process-exec (subpath \"$(escape_profile_path "${test_tmpdir_alias}")\"))" \
    "  (allow process-exec (literal \"$(escape_profile_path "${test_command}")\"))"

  while IFS='=' read -r runtime_name runtime_path; do
    if [[ "${runtime_name}" == *-tree ]]; then
      printf '  (allow process-exec (subpath "%s"))\n' \
        "$(escape_profile_path "${runtime_path}")"
    else
      printf '  (allow process-exec (literal "%s"))\n' \
        "$(escape_profile_path "${runtime_path}")"
    fi
  done <"${runtime_manifest}"

  # The runfiles tree and Bazel output tree both use symlinks on macOS, while
  # Seatbelt evaluates the final physical executable. Resolve every executable
  # from the current target's manifest to its final path before granting it.
  canonical_runfile_execs="${TEST_TMPDIR}/runfile-executables"
  "${runtime_bin}/python3" -c \
    'import os, sys
for line in open(sys.argv[1]):
    fields = line.rstrip("\n").split(" ", 1)
    if len(fields) == 2 and os.access(fields[1], os.X_OK):
        print(os.path.realpath(fields[1]))' \
    "${RUNFILES_MANIFEST_FILE:-${TEST_SRCDIR}/MANIFEST}" \
    >"${canonical_runfile_execs}"
  while IFS= read -r runfile_path; do
    printf '  (allow process-exec (literal "%s"))\n' \
      "$(escape_profile_path "${runfile_path}")"
  done <"${canonical_runfile_execs}"

  if [[ "${MONO_TEST_XCODE_TOOLCHAIN:-}" == "1" ]]; then
    printf '%s\n' \
      '  (allow process-exec (literal "/usr/bin/xcodebuild"))' \
      '  (allow process-exec (literal "/usr/bin/xcrun"))' \
      '  (allow process-exec (subpath "/Applications/Xcode.app"))'
  fi

  if [[ "${MONO_TEST_ALLOW_NETWORK:-}" != "1" ]]; then
    printf '%s\n' \
      '  (deny network*)' \
      '  (allow network-inbound (local ip "localhost:*"))' \
      '  (allow network* (remote ip "localhost:*"))' \
      '  (allow network* (remote unix-socket))'
  fi
} >"${profile}"

# Monitor mode gives sandbox-exec and all descendants a dedicated process
# group, allowing timeout/interrupt traps to forward the original signal to
# the complete test tree before removing the private root.
set -m
/usr/bin/sandbox-exec -f "${profile}" "$@" &
child_pid=$!
set +m

set +e
wait "${child_pid}"
test_status=$?
set -e
child_pid=""
exit "${test_status}"
