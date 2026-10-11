# Buildkite: repobin release setup

Operator checklist for the **repobin release pipeline**, which builds prebuilt `repobin` binaries for macOS arm64 and Linux x86_64 and publishes them as pinned, checksum-verified assets on a `repobin-vX.Y.Z` GitHub Release of `spinyfin/mono`. It exists so that other repos' CI (e.g. `brianduff/flunge`) can install a specific repobin version by download instead of building it from source.

The pipeline is a direct copy of the changelog release pipeline; the full operator walk-through (cluster lookup, bootstrap step, disabling push triggers, cron schedule, recovery from a partial release, abandoning a draft) is in [`../../changelog/docs/buildkite-release-setup.md`](../../changelog/docs/buildkite-release-setup.md). Substitute `repobin` for `changelog` throughout. Differences:

- Pipeline slug: `mono-repobin-release`. Suggested cron: `30 8 * * *` (offset from checkleft at 07:00 and changelog at 08:00).
- Tags: `repobin-vX.Y.Z`. `tools/repobin/Cargo.toml` carries `0.1.0-alpha.1`; the pre-release suffix is ignored, so the first release is `repobin-v0.1.1`. Bump major/minor in `Cargo.toml` by hand to move the series.
- Build target: `bazel build -c opt //tools/repobin:repobin`; the optional darwin x86_64 asset is a `cargo --target` cross-build (`-p repobin`).
- Change detection covers `tools/repobin/` and the pipeline/script files.

Registration is **not** automatic: the in-repo files are inert until the Buildkite pipeline `mono-repobin-release` is created and pointed at `.buildkite/pipeline-repobin-release.yml` (steps 1-5 of the changelog doc), and until someone triggers a first manual build.

- Pipeline definition: [`../../../.buildkite/pipeline-repobin-release.yml`](../../../.buildkite/pipeline-repobin-release.yml)
- Build fragment: [`../../../.buildkite/pipeline-repobin-release-builds.yml`](../../../.buildkite/pipeline-repobin-release-builds.yml)
- Release script: [`../../../.buildkite/steps/repobin-release.sh`](../../../.buildkite/steps/repobin-release.sh)

## Release assets

`prepare` creates the release as a **draft**, the `linux` and `darwin` steps upload their assets in parallel, and `publish` verifies every required asset against its sidecar and only then flips the release to published. A release is therefore never public with a missing or unverified asset.

| Asset                              | Status                             |
| ---------------------------------- | ---------------------------------- |
| `repobin-aarch64-apple-darwin`     | required                           |
| `repobin-x86_64-unknown-linux-gnu` | required                           |
| `repobin-x86_64-apple-darwin`      | optional (best-effort cross-build) |

Each asset has a `<name>.sha256` sidecar (`<hex>  <name>`).

## Installing in CI

Pin the version in the consuming repo and download by version, verifying the sha256 before installing:

```sh
set -euo pipefail
version=0.1.1   # pin this in the consuming repo
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  asset=repobin-aarch64-apple-darwin ;;
  Linux-x86_64)  asset=repobin-x86_64-unknown-linux-gnu ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac
dir="$(mktemp -d)"
gh release download "repobin-v${version}" --repo spinyfin/mono --dir "$dir" \
  --pattern "$asset" --pattern "$asset.sha256"
expected="$(awk '{print $1}' "$dir/$asset.sha256")"
if command -v sha256sum >/dev/null; then actual="$(sha256sum "$dir/$asset" | awk '{print $1}')"
else actual="$(shasum -a 256 "$dir/$asset" | awk '{print $1}')"; fi
[ "$expected" = "$actual" ] || { echo "sha256 mismatch for $asset" >&2; exit 1; }
install -m 0755 "$dir/$asset" "$HOME/bin/repobin"
```

Without `gh`, fetch `https://github.com/spinyfin/mono/releases/download/repobin-v${version}/<asset>` (and the `.sha256`) with `curl -fsSL`. For stronger pinning, record the expected sha256 in the consuming repo instead of trusting the sidecar from the same release.
