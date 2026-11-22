#!/usr/bin/env bash
set -euo pipefail

[[ "${BUILDKITE:-}" == true && "${BUILDKITE_BRANCH:-}" == main && "${BUILDKITE_PULL_REQUEST:-false}" == false ]] || {
  echo 'NativeLink publishing requires a main-branch Buildkite build, not a PR or local invocation' >&2
  exit 1
}
[[ "$(hostname -s)" == headcrash ]] || {
  echo 'NativeLink publishing must run on headcrash' >&2
  exit 1
}
here="$(dirname "${BASH_SOURCE[0]}")"
# A slow older build must not undo a newer publication. Unrelated main
# changes are fine; changed dashboard or Prometheus sources require the newer build.
git fetch origin main
git diff --exit-code HEAD FETCH_HEAD -- tools/remote-execution/grafana tools/remote-execution/prometheus
# pipefail propagates validation, sudo, write and service failures to Buildkite.
python3 "${here}/validate.py" --bundle |
  sudo -n /usr/local/libexec/nativelink-grafana/install.py
python3 "${here}/../prometheus/validate.py" --print |
  sudo -n /usr/local/libexec/nativelink-prometheus/install.py
