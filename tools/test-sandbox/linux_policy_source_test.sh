#!/bin/bash

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
bazelrc="${workspace}/.bazelrc"
defs="${workspace}/tools/test-sandbox/defs.bzl"
wrapper="${workspace}/tools/test-sandbox/hermetic_test_wrapper.sh"
remote_sandbox="${workspace}/tools/test-sandbox/linux_remote_action_sandbox.sh"

grep -Fqx 'test:linux --strategy=TestRunner=linux-sandbox' "${bazelrc}"
grep -Fqx 'test:linux --sandbox_tmpfs_path=/tmp' "${bazelrc}"
grep -Fqx 'test --sandbox_default_allow_network=false' "${bazelrc}"
grep -Fq 'network_tags.append("requires-network")' "${defs}"
grep -Fq 'mktemp" -d /tmp/mono-test.XXXXXX' "${wrapper}"

# The remote opt-in names the remote strategy alone. A comma fallback, or a
# second strategy line on the same config, would let a test that cannot run
# remotely run under some local strategy instead and still report a result.
grep -Fqx 'test:remote-tests --strategy=TestRunner=remote' "${bazelrc}"
remote_strategy_lines="$(grep -c -E '^[a-z]+:remote-tests .*strategy' "${bazelrc}" || true)"
[[ "${remote_strategy_lines}" == "1" ]]
# Remote and local results must not share a cache key.
grep -Fqx 'test:remote-tests --test_env=MONO_TEST_REMOTE_EXECUTION=1' "${bazelrc}"

# A Linux test that executes remotely has no linux-sandbox around it. The
# boundary is the worker's per-action sandbox, and these are the parts of it
# the guards depend on: a read-only host, new namespaces, no network unless the
# action carries the opt-in variable, a tmpfs /tmp for a test action, and no
# view of /home or of the sockets under /run.
grep -Fq -- '--ro-bind / /' "${remote_sandbox}"
grep -Fq -- '--unshare-pid --unshare-ipc --unshare-uts' "${remote_sandbox}"
grep -Fqx 'net_args=(--unshare-net)' "${remote_sandbox}"
grep -Fqx 'if [[ "${MONO_TEST_ALLOW_NETWORK:-}" == "1" ]]; then' "${remote_sandbox}"
grep -Fqx '  tmp_args=(--tmpfs /tmp)' "${remote_sandbox}"
grep -Fq -- '--tmpfs /run' "${remote_sandbox}"
grep -Fq -- '--tmpfs /home' "${remote_sandbox}"
# An action gets the umask a local action has, not the worker's.
grep -Fqx 'umask 022' "${remote_sandbox}"
# The opt-in variable the sandbox reads is the one the macro sets.
grep -Fq 'network_env["MONO_TEST_ALLOW_NETWORK"] = "1"' "${defs}"

for key in \
  BOSS_SHAKE_APP_ID \
  BOSS_SHAKE_INSTALLATION_ID \
  BOSS_SHAKE_PRIVATE_KEY_PEM; do
  grep -Fqx "test --test_env=${key}=" "${bazelrc}"
  grep -Fq "${key}" "${wrapper}"
done
