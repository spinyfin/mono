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

for key in \
  BOSS_SHAKE_APP_ID \
  BOSS_SHAKE_INSTALLATION_ID \
  BOSS_SHAKE_PRIVATE_KEY_PEM; do
  grep -Fqx "test --test_env==${key}" "${bazelrc}"
  grep -Fq "${key}" "${wrapper}"
done
