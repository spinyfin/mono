# Buildkite: repobin release setup

Operator checklist for the **repobin release pipeline**, which builds prebuilt `repobin` binaries for macOS arm64 and Linux x86_64 and publishes them as pinned, checksum-verified assets on a `repobin-vX.Y.Z-alpha.N` GitHub Release of `spinyfin/mono`. It exists so that other repos' CI (e.g. `brianduff/flunge`) can install a specific repobin version by download instead of building it from source.

The pipeline is a thin wrapper over the shared `bin/release` tool (`prepare` / `upload` / `publish`), configured by [`../release.toml`](../release.toml) and shaped like the `release` and `checkleft` pipelines. The full operator walk-through (cluster lookup, bootstrap step, disabling push triggers, cron schedule, recovery from a partial release, abandoning a draft) is in [`../../changelog/docs/buildkite-release-setup.md`](../../changelog/docs/buildkite-release-setup.md); substitute `repobin` for `changelog`. Differences:

- Pipeline slug: `mono-repobin-release`. Suggested cron: `30 8 * * *` (offset from checkleft at 07:00 and changelog at 08:00).
- Tags: `repobin-vX.Y.Z-alpha.N` (`alpha-counter` scheme against `tools/repobin/Cargo.toml`, which carries `0.1.0-alpha.1`). Bump the manifest by hand to move the series.
- Build target: `bazel build -c opt //tools/repobin:repobin` on each platform. No cargo cross-builds are used.
- Change detection covers `tools/repobin/` and the pipeline/script files (`change_paths` in `release.toml`).

Registration is **not** automatic and is an admin action outside this repo: the in-repo files take effect only once the Buildkite pipeline `mono-repobin-release` is created and pointed at `.buildkite/pipeline-repobin-release.yml` (steps 1-5 of the changelog doc) and someone triggers a first manual build. Until then no `repobin-v*` release exists.

- Pipeline definition: [`../../../.buildkite/pipeline-repobin-release.yml`](../../../.buildkite/pipeline-repobin-release.yml)
- Build fragment: [`../../../.buildkite/pipeline-repobin-release-builds.yml`](../../../.buildkite/pipeline-repobin-release-builds.yml)
- Release script: [`../../../.buildkite/steps/repobin-release.sh`](../../../.buildkite/steps/repobin-release.sh)

## Release assets

`prepare` creates the release as a **draft**, the `linux` and `darwin` steps upload their assets in parallel, and `publish` verifies every required asset against its sidecar and only then flips the release to published. A release is therefore never public with a missing or unverified asset.

| Asset                              | Status   |
| ---------------------------------- | -------- |
| `repobin-aarch64-apple-darwin`     | required |
| `repobin-x86_64-unknown-linux-gnu` | required |

Each asset has a `<name>.sha256` sidecar (`<hex>  <name>`).

## Installing in CI

Record the release version and the expected sha256 of each platform's asset in the consuming repo, and verify the download against those recorded values. The `.sha256` sidecar on the release is only a convenience for reading the hash when you bump the pin; do not verify against it at install time, since a replaced asset would come with a matching replaced sidecar.

```sh
set -euo pipefail
version=0.1.0-alpha.2   # pin in the consuming repo
sha256_darwin_arm64=<hex>   # pin in the consuming repo
sha256_linux_x86_64=<hex>   # pin in the consuming repo
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  asset=repobin-aarch64-apple-darwin; expected="$sha256_darwin_arm64" ;;
  Linux-x86_64)  asset=repobin-x86_64-unknown-linux-gnu; expected="$sha256_linux_x86_64" ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac
dir="$(mktemp -d)"
gh release download "repobin-v${version}" --repo spinyfin/mono --dir "$dir" --pattern "$asset"
if command -v sha256sum >/dev/null; then actual="$(sha256sum "$dir/$asset" | awk '{print $1}')"
else actual="$(shasum -a 256 "$dir/$asset" | awk '{print $1}')"; fi
[ "$expected" = "$actual" ] || { echo "sha256 mismatch for $asset" >&2; exit 1; }
mkdir -p "$HOME/bin"
install -m 0755 "$dir/$asset" "$HOME/bin/repobin"
export PATH="$HOME/bin:$PATH"   # for later steps, add this to the agent's environment hook
```

To read the hash when bumping the pin, download `<asset>.sha256` (`<hex>  <name>`) from the new release. Without `gh`, fetch `https://github.com/spinyfin/mono/releases/download/repobin-v${version}/<asset>` with `curl -fsSL`.
