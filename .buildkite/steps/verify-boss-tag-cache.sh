#!/usr/bin/env bash
# Opt-in, sequential cross-agent probe using the normal CI cache configuration.
set -euo pipefail

probe_key="boss-tag-cache-${BUILDKITE_STEP_ID}"
role="${BUILDKITE_PARALLEL_JOB}"
peer=$((1 - role))
# Rendezvous before setup: both jobs must be running on different agents,
# even when an idle agent would otherwise pick up both jobs in sequence.
buildkite-agent meta-data set "${probe_key}-${role}-agent" "$BUILDKITE_AGENT_NAME"
until buildkite-agent meta-data exists "${probe_key}-${peer}-agent"; do
  sleep 5
done
peer_agent=$(buildkite-agent meta-data get "${probe_key}-${peer}-agent")
[[ "$peer_agent" != "$BUILDKITE_AGENT_NAME" ]]

if (( role == 0 )); then
  # Release the waiting peer even if setup or a build fails.
  trap 'buildkite-agent meta-data set "${probe_key}-result" failed' EXIT
else
  until buildkite-agent meta-data exists "${probe_key}-result"; do
    sleep 5
  done
  [[ "$(buildkite-agent meta-data get "${probe_key}-result")" == passed ]]
fi

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
if (( role == 0 )); then
  buildkite-agent meta-data set "${probe_key}-result" passed
  trap - EXIT
fi
