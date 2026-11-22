#!/usr/bin/env bash
# Opt-in, sequential cross-agent probe using the normal CI cache configuration.
set -euo pipefail

CI_REMOTE_EXECUTION_ALLOWED=1
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

evidence_dir=$(mktemp -d "${TMPDIR:-/tmp}/boss-tag-cache.XXXXXX")
echo "Agent: ${BUILDKITE_AGENT_NAME}"
echo "Commit: ${BUILDKITE_COMMIT}"
bash tools/boss/installer/workspace-status.sh | tee "${evidence_dir}/workspace-status.txt"

# boss and boss_test compile the generated build_info_rs, so this checks both
# the version-dependent compile and test cache, rather than only the tag list.
bazel_verify_only build --execution_log_json_file="${evidence_dir}/build-execution.json" \
  --profile="${evidence_dir}/build-profile.json.gz" \
  //tools/boss/cli:boss //tools/boss/build-info:build-info
bazel_verify_only test --test_output=errors \
  --execution_log_json_file="${evidence_dir}/test-execution.json" \
  --profile="${evidence_dir}/test-profile.json.gz" \
  //tools/boss/cli:boss_test //tools/boss/build-info:build-info_test
buildkite-agent artifact upload "${evidence_dir}/*"
