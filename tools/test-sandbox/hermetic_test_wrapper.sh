#!/bin/bash

set -euo pipefail

# macOS rejects nested Seatbelt profiles. Bazel may run this one dedicated
# process-lifecycle supervisor outside the profile so it can invoke and
# interrupt fully enforced wrapper processes. The exception is bound to the
# physical Bazel test executable passed by run_under; target-controlled
# environment variables cannot enable it for another test action.
cleanup_supervisor="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/tools/test-sandbox/cleanup_guard_test"
if [[ "$#" -gt 0 && -x "${cleanup_supervisor}" && "$1" -ef "${cleanup_supervisor}" ]]; then
  unset \
    ANTHROPIC_API_KEY \
    OPENAI_API_KEY \
    GITHUB_TOKEN \
    GH_TOKEN \
    BOSS_SHAKE_APP_ID \
    BOSS_SHAKE_INSTALLATION_ID \
    BOSS_SHAKE_PRIVATE_KEY_PEM
  exec "$@"
fi

runtime_root="${TEST_SRCDIR:?}/+test_runtime_repository+test_runtime_tools"
runtime_bin="${runtime_root}/bin"
runtime_manifest="${runtime_root}/manifest"
developer_dir_file="${runtime_root}/developer_dir"
main_runfiles="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
xcode_bin="${main_runfiles}/tools/test-sandbox"
host_home="${HOME:?}"
host_tmpdir="${TMPDIR:-}"
host_tmpdir_alias=""
host_user_home=""
if [[ -n "${USER:-}" && -d "/Users/${USER}" ]]; then
  host_user_home="/Users/${USER}"
fi

if [[ ! -d "${runtime_bin}" || ! -f "${runtime_manifest}" || ! -f "${developer_dir_file}" ]]; then
  printf '%s\n' "audited Bazel test runtime is unavailable" >&2
  exit 126
fi

if [[ "${OSTYPE:-}" == darwin* || "${OSTYPE:-}" == linux* ]]; then
  # On Linux, .bazelrc makes /tmp a per-action linux-sandbox tmpfs. This keeps
  # Unix-domain socket paths well below sockaddr_un.sun_path without exposing
  # a shared host temp directory. hermeticity_guard_test's
  # linux_private_temp_root_is_short_and_action_private fails the action if
  # /tmp is not actually mounted as tmpfs or contains entries from outside
  # this action. macOS protects the same private directory with the
  # generated Seatbelt profile below.
  test_tmpdir="$("${runtime_bin}/mktemp" -d /tmp/mono-test.XXXXXX)"
  owns_test_tmpdir=1
else
  printf 'unsupported test sandbox host: %s\n' "${OSTYPE:-unknown}" >&2
  exit 126
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

unset \
  ANTHROPIC_API_KEY \
  OPENAI_API_KEY \
  GITHUB_TOKEN \
  GH_TOKEN \
  BOSS_SHAKE_APP_ID \
  BOSS_SHAKE_INSTALLATION_ID \
  BOSS_SHAKE_PRIVATE_KEY_PEM

child_pid=""
watchdog_pid=""

process_group_is_alive() {
  [[ -n "${child_pid}" ]] && kill -0 -- "-${child_pid}" 2>/dev/null
}

terminate_child_group() {
  if ! process_group_is_alive; then
    return 0
  fi

  kill -TERM -- "-${child_pid}" 2>/dev/null || true
  for _ in {1..20}; do
    if ! process_group_is_alive; then
      wait "${child_pid}" 2>/dev/null || true
      return 0
    fi
    "${runtime_bin}/sleep" 0.05
  done

  kill -KILL -- "-${child_pid}" 2>/dev/null || true
  wait "${child_pid}" 2>/dev/null || true
  for _ in {1..20}; do
    if ! process_group_is_alive; then
      return 0
    fi
    "${runtime_bin}/sleep" 0.05
  done

  printf 'test process group survived SIGKILL: %s\n' "${child_pid}" >&2
  return 1
}

# Tells the watchdog this shell did its own cleanup, and reaps it. Called
# last, so that a SIGKILL part-way through cleanup still leaves the watchdog
# to finish the job.
release_watchdog() {
  if [[ -n "${watchdog_pid}" ]]; then
    printf 'done\n' >&8 2>/dev/null || true
    wait "${watchdog_pid}" 2>/dev/null || true
    watchdog_pid=""
  fi
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ -n "${child_pid}" ]] && ! terminate_child_group && [[ "${status}" == "0" ]]; then
    status=1
  fi
  if [[ "${owns_test_tmpdir}" == "1" ]]; then
    "${runtime_bin}/rm" -rf "${test_tmpdir}"
  fi
  release_watchdog
  exit "${status}"
}

forward_signal() {
  local signal="$1"
  if [[ -n "${child_pid}" ]]; then
    kill "-${signal}" -- "-${child_pid}" 2>/dev/null || true
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

# The manifest records where each audited tool lived on the machine that
# configured the runtime repository, which under remote execution is not the
# machine running this action. Seatbelt matches the final physical path, so
# resolve every entry here, on the executing host. bin/<name> is a declared
# symlink to the recorded path; it must reach the same file as the recorded
# path does (a shipped copy of the tool, or a host without it, does not), and
# that file is what the profile allows. A *-tree entry names a directory of
# helpers and is allowed at its resolved location. Anything missing or
# mismatched fails the action closed. On the machine that configured the
# repository the recorded paths are already final, so this yields exactly the
# manifest's own paths.
runtime_grants="${TEST_TMPDIR}/runtime-grants"
if ! "${runtime_bin}/python3" -c \
  'import os, sys
runtime_bin, manifest = sys.argv[1:3]
for line in open(manifest):
    name, _, recorded = line.rstrip("\n").partition("=")
    if not name or not os.path.isabs(recorded):
        sys.exit("malformed runtime manifest entry: " + line.rstrip("\n"))
    resolved = os.path.realpath(recorded)
    if "\n" in resolved or "\t" in resolved:
        sys.exit(name + ": unsupported path " + repr(resolved))
    if name.endswith("-tree"):
        if not os.path.isdir(resolved):
            sys.exit(name + ": " + recorded + " is not a directory on this host")
        print("subpath\t" + resolved)
        continue
    if not os.path.isfile(resolved):
        sys.exit(name + ": " + recorded + " does not exist on this host")
    tool = os.path.join(runtime_bin, name)
    if os.path.realpath(tool) != resolved:
        sys.exit(name + ": " + tool + " does not resolve to " + recorded)
    print("literal\t" + resolved)' \
  "${runtime_bin}" "${runtime_manifest}" >"${runtime_grants}"; then
  printf '%s\n' "audited Bazel test runtime does not match this host" >&2
  exit 126
fi

escape_profile_path() {
  local escaped="$1"
  escaped="${escaped//\\/\\\\}"
  escaped="${escaped//\"/\\\"}"
  printf '%s' "${escaped}"
}

emit_protected_root() {
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
  for writable_pattern in "${writable_patterns[@]-}"; do
    if [[ -n "${writable_pattern}" ]]; then
      printf '      (require-not (regex #"%s"))\n' "${writable_pattern}"
    fi
  done
  printf '%s\n' '    ))'
}

developer_dir=""
developer_dir_grant=""
if [[ "${MONO_TEST_XCODE_TOOLCHAIN:-}" == "1" ]]; then
  IFS= read -r developer_dir <"${developer_dir_file}" || true
  if [[ -z "${developer_dir}" || ! -d "${developer_dir}" ]]; then
    printf '%s\n' "configured Xcode developer root is unavailable" >&2
    exit 126
  fi
  # As with the manifest: the recorded root may be a symlink on this host.
  developer_dir_grant="$("${runtime_bin}/python3" -c \
    'import os, sys; print(os.path.realpath(sys.argv[1]))' "${developer_dir}")"
  # Foundation stages atomic writes in the per-user temp directory, wherever
  # TMPDIR points, and the write exception below is built from that directory.
  # Bazel gives a local action the invoking user's TMPDIR, which names it. A
  # remote action has only the environment it declares: no TMPDIR, or for an
  # Apple test target the unexpanded XCODE_TEST_ENV placeholder. Neither is a
  # path, so ask the host that is running the action. 65537 is
  # _CS_DARWIN_USER_TEMP_DIR in <unistd.h>, the value `getconf
  # DARWIN_USER_TEMP_DIR` prints; os.confstr accepts the number, not the name.
  if [[ "${host_tmpdir}" != /* ]]; then
    if ! host_tmpdir="$("${runtime_bin}/python3" -c \
      'import os; print(os.confstr(65537) or "")')" ||
      [[ "${host_tmpdir}" != /* || ! -d "${host_tmpdir}" ]]; then
      printf '%s\n' "the per-user temp directory of this host is unavailable" >&2
      exit 126
    fi
  fi
  if [[ "${host_tmpdir}" == /var/* ]]; then
    host_tmpdir_alias="/private${host_tmpdir}"
  fi
  export MONO_TEST_XCODE_DEVELOPER_DIR="${developer_dir}"
  export MONO_TEST_HOST_TMPDIR="${host_tmpdir}"
  export DEVELOPER_DIR="${developer_dir}"
fi

{
  printf '%s\n' \
    '(version 1)' \
    '(allow default)'

  writable_paths=(
    "${TEST_TMPDIR:-}"
    "${test_tmpdir_alias}"
    "${TEST_UNDECLARED_OUTPUTS_DIR:-}"
    "${TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR:-}"
    "${COVERAGE_DIR:-}"
    "${COVERAGE_OUTPUT_FILE:-}"
    "${XML_OUTPUT_FILE:-}"
    "${TEST_SHARD_STATUS_FILE:-}"
    "${TEST_PREMATURE_EXIT_FILE:-}"
    "${TEST_WARNINGS_OUTPUT_FILE:-}"
  )
  writable_patterns=()
  if [[ "${MONO_TEST_XCODE_TOOLCHAIN:-}" == "1" && -n "${host_tmpdir}" ]]; then
    host_tmpdir_regex="$("${runtime_bin}/python3" -c \
      'import re, sys; print(re.escape(sys.argv[1].rstrip("/")))' \
      "${host_tmpdir}")"
    writable_patterns+=(
      "^${host_tmpdir_regex}/TemporaryItems/NSIRD_xctest_[^/]+(/.*)?$"
    )
    if [[ -n "${host_tmpdir_alias}" ]]; then
      host_tmpdir_alias_regex="$("${runtime_bin}/python3" -c \
        'import re, sys; print(re.escape(sys.argv[1].rstrip("/")))' \
        "${host_tmpdir_alias}")"
      writable_patterns+=(
        "^${host_tmpdir_alias_regex}/TemporaryItems/NSIRD_xctest_[^/]+(/.*)?$"
      )
    fi
  fi

  # Enumerate every mounted/top-level macOS host filesystem root at action
  # startup. Path-bearing writes are denied in each root except the
  # action-private and Bazel-declared paths above. /dev is intentionally
  # absent: its device nodes are not host file storage.
  while IFS= read -r protected_path; do
    if [[ "${protected_path}" == "/dev" ]]; then
      continue
    fi
    emit_protected_root "${protected_path}" "${writable_paths[@]}"
  done < <("${runtime_bin}/find" / -mindepth 1 -maxdepth 1 -print)

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

  while IFS=$'\t' read -r grant_kind grant_path; do
    printf '  (allow process-exec (%s "%s"))\n' \
      "${grant_kind}" "$(escape_profile_path "${grant_path}")"
  done <"${runtime_grants}"

  # The runfiles tree and Bazel output tree both use symlinks on macOS, while
  # Seatbelt evaluates the final physical executable. Resolve every executable
  # from the current target's manifest to its final path before granting it.
  # A remote executor stages the runfiles tree itself and no manifest (whose
  # paths would name the client anyway), so there the staged tree is read
  # instead: the same declared inputs, resolved on the host that runs them.
  # Files staged inside the tree are already covered by the TEST_SRCDIR grant
  # above; what is left is a declared symlink that leaves the tree.
  canonical_runfile_execs="${TEST_TMPDIR}/runfile-executables"
  "${runtime_bin}/python3" -c \
    'import os, sys
manifest, runfiles = sys.argv[1:3]
if os.path.isfile(manifest):
    for line in open(manifest):
        fields = line.rstrip("\n").split(" ", 1)
        if len(fields) == 2 and os.access(fields[1], os.X_OK):
            print(os.path.realpath(fields[1]))
else:
    for root, dirs, files in os.walk(runfiles):
        dirs.sort()
        for name in sorted(files):
            path = os.path.join(root, name)
            resolved = os.path.realpath(path)
            if os.access(path, os.X_OK) and not resolved.startswith(runfiles + os.sep):
                print(resolved)' \
    "${RUNFILES_MANIFEST_FILE:-${TEST_SRCDIR}/MANIFEST}" \
    "${TEST_SRCDIR}" \
    >"${canonical_runfile_execs}"
  while IFS= read -r runfile_path; do
    printf '  (allow process-exec (literal "%s"))\n' \
      "$(escape_profile_path "${runfile_path}")"
  done <"${canonical_runfile_execs}"

  if [[ "${MONO_TEST_XCODE_TOOLCHAIN:-}" == "1" ]]; then
    printf '%s\n' \
      '  (allow process-exec (literal "/usr/bin/xcodebuild"))' \
      '  (allow process-exec (literal "/usr/bin/xcrun"))' \
      "  (allow process-exec (subpath \"$(escape_profile_path "${developer_dir_grant}")\"))"
  fi

  if [[ "${MONO_TEST_ALLOW_NETWORK:-}" != "1" ]]; then
    printf '%s\n' \
      '  (deny network*)' \
      '  (allow network-inbound (local ip "localhost:*"))' \
      '  (allow network* (remote ip "localhost:*"))' \
      '  (allow network* (remote unix-socket))'
  fi
} >"${profile}"

# The traps above cover every signal this shell can see. They do not cover
# SIGKILL, which is how a test action is abandoned rather than timed out:
# Bazel's process-wrapper kills test-setup.sh's group when a build is
# interrupted, and test-setup.sh's own helper then kills this shell's group.
# An executor that kills an action outright has the same effect. The test's
# process group below is in neither of those groups. With nothing else
# watching, it would outlive this shell with no parent: a hung test stays for
# good, a healthy one keeps running against a build that is over, and the
# private root is never removed.
#
# So a watchdog holds the read end of a FIFO whose only write end is this
# shell's descriptor 8. The kernel closes that descriptor however this shell
# dies. A line on the FIFO says cleanup already happened; end-of-file says the
# shell is gone, and the watchdog does the cleanup in its place. The FIFO is
# unlinked once both ends are open, so nothing else can open it.
lifeline="${TEST_TMPDIR}/wrapper-lifeline"
"${runtime_bin}/mkfifo" "${lifeline}"
exec 8<>"${lifeline}" 9<"${lifeline}"
"${runtime_bin}/rm" -f "${lifeline}"

# Monitor mode gives sandbox-exec and all descendants a dedicated process
# group, allowing timeout/interrupt traps to forward the original signal to
# the complete test tree before removing the private root. The watchdog gets
# a group of its own for the same reason the test does: a kill aimed at this
# shell's group must not take it too. Neither end of the FIFO reaches the
# test, where a surviving descendant would hold the write end open.
set -m
/usr/bin/sandbox-exec -f "${profile}" "$@" 8<&- 9<&- &
child_pid=$!
(
  set +m
  trap - EXIT INT TERM HUP
  exec 8<&- </dev/null >/dev/null 2>&1
  if IFS= read -r _ <&9; then
    exit 0
  fi
  terminate_child_group || true
  if [[ "${owns_test_tmpdir}" == "1" ]]; then
    "${runtime_bin}/rm" -rf "${test_tmpdir}"
  fi
) &
watchdog_pid=$!
set +m
exec 9<&-

set +e
wait "${child_pid}"
test_status=$?
set -e
if ! process_group_is_alive; then
  child_pid=""
fi
exit "${test_status}"
