#!/bin/bash

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
bazelrc="${workspace}/.bazelrc"
defs="${workspace}/tools/test-sandbox/defs.bzl"
wrapper="${workspace}/tools/test-sandbox/hermetic_test_wrapper.sh"

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

for key in \
  BOSS_SHAKE_APP_ID \
  BOSS_SHAKE_INSTALLATION_ID \
  BOSS_SHAKE_PRIVATE_KEY_PEM; do
  grep -Fqx "test --test_env=${key}=" "${bazelrc}"
  grep -Fq "${key}" "${wrapper}"
done
