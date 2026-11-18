#!/bin/bash
# Per-action sandbox for a NativeLink worker on Linux. A worker installs this
# file and names it as `entrypoint` in its config, so every remote action runs
# as `<this script> <the action's command line>`. This copy is the one the
# workers must run, unmodified: `ci-remote-linux` sends tests to them on the
# strength of what it enforces. Installing it is in
# .buildkite/linux-agents-runbook.md, "Remote execution workers".
#
# NativeLink starts an action directly in its work directory, as the user the
# worker runs as, with that user's whole view of the host. Bazel's local
# `linux-sandbox` does better for an action that runs on an agent, and the
# repository's tests rely on what it provides. This script gives a remote
# action the same boundary with bubblewrap (unprivileged user namespaces, no
# root needed):
#
#   - The host filesystem is read-only. Only the action's own directory (its
#     inputs and outputs) and /tmp, /dev/shm are writable.
#   - /tmp is private to the action. For a test action (Bazel sets TEST_TMPDIR)
#     it is a fresh tmpfs, as `--sandbox_tmpfs_path=/tmp` gives a local test;
#     hermeticity_guard_test checks exactly that. For any other action it is a
#     directory inside the action directory, so that compilers' temporary
#     files land on disk and not in RAM. NativeLink deletes the action
#     directory when the action ends.
#   - No network: a new network namespace with loopback only. An action whose
#     environment carries MONO_TEST_ALLOW_NETWORK=1 keeps the host network.
#     That is the variable `network_enabled_rust_test` sets, and the one the
#     test wrapper reads for the same purpose on macOS.
#   - /home and /run are empty. /run holds the user's D-Bus and systemd
#     sockets, the ssh and gpg agents and other daemons' sockets; a read-only
#     mount does not stop a connect(), so they are removed from view. The DNS
#     stub configuration is put back for actions that have the network.
#   - New PID, IPC and UTS namespaces. When the action's command exits, every
#     process it left behind is killed with the namespace.
#
#   - umask 022, whatever the worker was started with. A login user's
#     session, which is what runs a worker under systemd --user, has 002 on a
#     distribution with per-user groups, and then every directory an action
#     creates is group-writable. Code that refuses a group-writable path, and
#     its tests, see something an agent never produces: Bazel's server, and
#     so every local action, runs with 022.
#
# It fails closed: if the sandbox cannot be set up the action fails, and
# nothing runs unsandboxed.
#
# Input, set by the worker (`additional_environment` in its config):
#   NL_ACTION_DIRECTORY  the action's root directory; its `work` subdirectory
#                        is the action's execution root.
#
# The worker clears the environment, so nothing here can rely on PATH.
set -u

die() {
  printf 'remote worker action sandbox: %s\n' "$*" >&2
  exit 125
}

BWRAP=/usr/bin/bwrap
[[ -x "$BWRAP" ]] || die "$BWRAP is not installed"

action_dir="${NL_ACTION_DIRECTORY:-}"
unset NL_ACTION_DIRECTORY
[[ "$action_dir" == /* && -d "$action_dir/work" ]] || die "the worker did not name an action directory"
[[ "$#" -gt 0 ]] || die "no command"

umask 022

net_args=(--unshare-net)
if [[ "${MONO_TEST_ALLOW_NETWORK:-}" == "1" ]]; then
  net_args=()
fi

if [[ -n "${TEST_TMPDIR:-}" ]]; then
  tmp_args=(--tmpfs /tmp)
else
  /bin/mkdir -p "$action_dir/tmp" || die "could not create the action's temporary directory"
  tmp_args=(--bind "$action_dir/tmp" /tmp)
fi

exec "$BWRAP" \
  --die-with-parent \
  --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup-try \
  --hostname localhost \
  "${net_args[@]}" \
  --ro-bind / / \
  --dev /dev \
  --proc /proc \
  --tmpfs /run \
  --ro-bind-try /run/systemd/resolve/stub-resolv.conf /run/systemd/resolve/stub-resolv.conf \
  --tmpfs /home \
  "${tmp_args[@]}" \
  --bind "$action_dir" "$action_dir" \
  --chdir "$PWD" \
  -- "$@"
