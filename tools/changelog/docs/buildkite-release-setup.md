# Buildkite: changelog release setup

This document is the operator checklist for the **changelog release pipeline** — the Buildkite pipeline that builds prebuilt `changelog` binaries for Linux and macOS and publishes them as assets on a GitHub Release of `spinyfin/mono`. External repos consume these prebuilts instead of building changelog from source.

It is modeled directly on the checkleft release pipeline; for that reference see [`../../checkleft/docs/buildkite-release-setup.md`](../../checkleft/docs/buildkite-release-setup.md). Two differences worth knowing up front:

- **No musl phase.** changelog has no C dependencies (checkleft carries wasmtime and tree-sitter and needs a hermetic musl toolchain for a static Linux binary); a native Linux build is enough, so this pipeline has only `linux` and `darwin` build phases.
- **No version-embedding step.** changelog's CLI has no `--version` flag and never reads `CARGO_PKG_VERSION`, so unlike checkleft there is no Cargo.toml/Cargo.lock patch-and-restore step before building. The tag is the only place the version lives; a given commit produces byte-identical binaries regardless of which tag names it. The version _scheme_ also differs: changelog's `Cargo.toml` carries a plain `X.Y.Z` (no `-alpha.N` suffix), so this pipeline revs the **patch** counter within the Cargo.toml major/minor rather than an alpha counter — see `compute_next_version` in the release script.

Like checkleft, this runs as a **separate pipeline** with its own cron schedule and its own manual trigger, not a step inside the main `mono` pipeline. **Creating the in-repo pipeline file is not enough — the pipeline must be registered in Buildkite using the steps below.**

- Pipeline definition: [`../../../.buildkite/pipeline-changelog-release.yml`](../../../.buildkite/pipeline-changelog-release.yml)
- Release script: [`../../../.buildkite/steps/changelog-release.sh`](../../../.buildkite/steps/changelog-release.sh)
- Version source of truth: [`../Cargo.toml`](../Cargo.toml) (`version = "0.1.0"`)
- Registered pipeline: [`https://buildkite.com/flunge/mono-changelog-release`](https://buildkite.com/flunge/mono-changelog-release) — steps 1-5 below were already performed for this pipeline (cluster: Default cluster; push-triggered builds off; daily 08:00 UTC schedule). This section stays as the operator checklist for re-registration (e.g. after an accidental pipeline deletion), not a to-do.

---

## How releases are triggered

| Trigger                                                            | When       | Change-detection                                                           |
| ------------------------------------------------------------------ | ---------- | -------------------------------------------------------------------------- |
| Buildkite cron schedule                                            | e.g. daily | Skips if nothing under changelog changed since the last `changelog-v*` tag |
| Manual build (`bk build create`, BK UI **New Build**, or REST API) | On demand  | Always releases (skips change-detection)                                   |

The pipeline should **not** be wired to build on push. It pushes only a tag, never a commit to `main`, and an idempotency guard no-ops any run whose `HEAD` is already the latest release commit — but the cleanest configuration is push-builds disabled, schedule + manual only.

The org slug is `flunge`; the GitHub repo is `spinyfin/mono`. (checkleft's release build URLs look like `https://buildkite.com/flunge/mono/builds/N`.)

---

## One-time registration

All `bk` commands below assume the Buildkite CLI is authenticated. Verify with:

```sh
bk whoami
bk use flunge          # select the org these pipelines live in
```

### 1. Find the cluster the mono pipelines use

New pipelines must be created in the same cluster as the existing `mono` pipeline so they schedule onto the same agent fleet (the `bazel-any` and `macos-arm64` queues).

```sh
bk cluster list
```

Note the cluster name (or ID). It is passed as `-c` when creating the pipeline below.

### 2. Create the pipeline

```sh
bk pipeline create "mono-changelog-release" \
  --description "Release pipeline for the changelog prebuilt binaries" \
  --repository "git@github.com:spinyfin/mono.git" \
  --cluster-id "<cluster-name-or-id>"
```

This creates the pipeline and connects it to the GitHub repo (which provisions the webhook). Confirm with `bk pipeline view mono-changelog-release`.

### 3. Point the pipeline at the release YAML

`bk pipeline create` does not upload the steps; like every pipeline in this repo, the registered pipeline must run a single bootstrap step that uploads the in-repo definition. The bootstrap step **must target a queue** (`bazel-any`) — the Default cluster has no default queue, so an untargeted step fails with "no queue has been targeted". Set the pipeline's **Steps** (Buildkite UI → Pipeline → **Settings** → **Steps**, or via the REST API) to exactly:

```yaml
steps:
  - label: ":pipeline: upload"
    command: "buildkite-agent pipeline upload .buildkite/pipeline-changelog-release.yml"
    agents:
      queue: bazel-any
```

(The default pipeline-upload command reads `.buildkite/pipeline.yml`; the explicit path is what makes this pipeline use the changelog definition.)

To do it via the API instead of the UI:

```sh
bk api --method PATCH /pipelines/mono-changelog-release --data '{"configuration":"steps:\n  - label: \":pipeline:\"\n    command: buildkite-agent pipeline upload .buildkite/pipeline-changelog-release.yml\n    agents:\n      queue: bazel-any\n"}'
```

### 4. Disable push-triggered builds

In Pipeline **Settings** → **GitHub**, turn **off** "Trigger builds when branches are pushed" (and any PR triggers). Releases come only from the cron schedule and manual builds. The release pushes only a tag (never a commit to `main`), so there is no self-trigger to guard against — push-builds-off simply keeps the pipeline schedule/manual-only.

### 5. Create the cron schedule

In Pipeline **Settings** → **Schedules** → **New Schedule**:

- **Description:** `changelog release check`
- **Cron interval:** `0 8 * * *` (daily 08:00 UTC — offset an hour from checkleft's 07:00 so the two release pipelines don't contend for the same agent pool at the same instant; adjust to taste)
- **Branch:** `main`
- **Message:** `Scheduled changelog release check`
- **Commit:** `HEAD`

To do it via the API instead of the UI (the REST API has no schedule endpoint; use GraphQL's `pipelineScheduleCreate`, with the pipeline's `graphql_id` from `bk pipeline view mono-changelog-release --output json`):

```sh
bk api --file - << 'EOF'
mutation CreateChangelogReleaseSchedule {
  pipelineScheduleCreate(input: {
    pipelineID: "<pipeline-graphql-id>"
    label: "changelog release check"
    cronline: "0 8 * * *"
    branch: "main"
    message: "Scheduled changelog release check"
    commit: "HEAD"
    enabled: true
  }) {
    pipelineScheduleEdge { node { id uuid label cronline enabled } }
  }
}
EOF
```

If a scheduled run finds no changelog-affecting changes since the last `changelog-v*` tag, the build logs `release skipped: ...` and exits 0 without cutting a release.

### 6. GitHub authentication — nothing to provision

No release token or secret is needed. The release pushes the tag with `git push origin` and creates the GitHub Release with `gh`, both authenticating via the CI agents' **ambient credentials** — exactly like checkleft. Every CI worker already has push-capable git auth and `gh` access to `spinyfin/mono`, so the pipeline works without any pipeline-specific token.

No branch-protection bypass is involved either: the release only pushes a **tag** (which protected branches permit) and never a commit to `main`.

---

## Triggering a release manually

```sh
bk build create \
  --pipeline mono-changelog-release \
  --branch main \
  --message "Manual changelog release"
```

Because `BUILDKITE_SOURCE` is `api`/`ui`, change-detection is skipped and a release is always cut. The BK UI **New Build** button does the same.

---

## Verifying the setup

1. Trigger a manual build (above) and open the build URL.
2. The **prepare** step should compute the next version, push the tag, and create the GitHub Release as a **draft** (not yet visible to normal `gh release view` / `gh release list` consumers as a published release).
3. The **linux** and **darwin** steps then run in parallel, each building its binaries and uploading them to that draft release. The **publish** step runs last (`depends_on` both): it re-downloads and re-verifies every required asset's checksum, then flips the release from draft to published.
4. Confirm the release and its assets:

```sh
gh release view changelog-v0.1.1 --repo spinyfin/mono
```

Expected assets (named by Rust target triple, each with a `.sha256` sidecar):

- `changelog-aarch64-apple-darwin` — **required**: the concrete consumer that motivated this pipeline (flunge's macOS release agents, e.g. `appoint`, whose release step currently falls back to `gh release create --generate-notes` purely because `bin/changelog` does not exist outside mono).
- `changelog-x86_64-unknown-linux-gnu` — **required**: mono's own CI (and any Linux Buildkite consumer) needs a Linux binary, and a native Bazel build for it is essentially free alongside the macOS build.
- `changelog-x86_64-apple-darwin` — **optional**: verified if present, but does not block publish if the darwin x86_64 cross-build fails (best-effort cargo cross-build from the arm64 macOS agent, same pattern as checkleft's optional asset).

A missing or checksum-mismatched **required** asset fails the `publish` step and leaves the release as an unpublished draft; the required/optional split is declared explicitly as `REQUIRED_ASSETS`/`OPTIONAL_ASSETS` in `changelog-release.sh`.

---

## Recovering from a partial release

`prepare` creates the tag and the GitHub Release as a **draft** before any build runs, then the `linux` and `darwin` build steps attach their assets in parallel, and `publish` verifies + publishes at the end. If a build step fails, or `publish` finds a missing/mismatched asset, the release is left as a draft — never published — with whatever assets did upload still attached. To recover:

- **Re-run the failed build job** (`bk job retry <job-id>`) — it reads the tag from build meta-data and re-uploads (assets use `--clobber`), so it picks up where it left off. Re-run `publish` afterwards (or it re-runs automatically as part of a full pipeline retry) to verify and publish.
- **Or upload manually** from an agent of the right OS, checked out at the tag:

  ```sh
  CHANGELOG_RELEASE_TAG=changelog-v0.1.1 \
    .buildkite/steps/changelog-release.sh darwin   # or: linux
  ```

- **Or re-trigger the whole pipeline manually** (`bk build create` / BK UI **New Build**) on the same commit. Because the trigger is manual (`BUILDKITE_SOURCE` is `ui`/`api`), `prepare`'s resume-existing-draft check re-adopts the existing draft/tag instead of computing a new version, and re-uploads the build fragment (skipping the upload if the fragment is already present in this build, e.g. a retried `prepare` job) — so the leftover draft is not orphaned and the fan-out build phases attach the remaining assets. A **scheduled (cron)** trigger will refuse to auto-resume a stranded draft — see "Abandoning a draft release" below — to avoid silently retrying a stuck release forever.

(If `prepare` itself fails before the Release is created, its cleanup trap deletes any tag it pushed, so a fresh run starts clean.)

## Abandoning a draft release

If a draft release is stuck (e.g. a persistent agent-pool problem keeps failing the same build/publish phase) and you do not want to keep resuming it, delete the draft and its tag so the next run — scheduled or manual — computes a fresh version instead of finding the stranded draft:

```sh
gh release delete changelog-v0.1.1 --repo spinyfin/mono --yes
git push origin :refs/tags/changelog-v0.1.1
```

A scheduled build that finds a draft for the current `HEAD` refuses to resume it on its own (see above) and names this exact recovery in its failure message, so a cron tick never gets stuck retrying indefinitely. To force a scheduled build to resume a draft instead of abandoning it, set `CHANGELOG_RESUME_DRAFT=1` on that build.

---

## How it works (summary)

- **Version:** the `Cargo.toml` version is a plain `X.Y.Z` (no pre-release suffix), so this pipeline revs the **patch** counter within the Cargo.toml major/minor: the next patch is `max(Cargo.toml patch, highest published changelog-v<major>.<minor>.* patch) + 1`, so a stale Cargo.toml can never reuse a published patch. The release **commit** (`BUILDKITE_COMMIT`) is tagged `changelog-vX.Y.Z`. Unlike checkleft, the version is never patched into the checkout — changelog embeds no version string, so there is nothing to keep in sync between `Cargo.toml`/`Cargo.lock` and the built binary; `main`'s `Cargo.toml` stays at whatever version it last held and developer builds carry no meaningful version, which is fine since the CLI never reports one.
- **Build tool:** native binaries are built with `bazel build -c opt //tools/changelog:changelog` (matches how mono builds changelog and reuses the CI disk cache); the darwin x86_64 cross target is built with `cargo --target`, since that triple is not registered in mono's bazel toolchains. All binaries for a given commit are byte-identical regardless of the release tag, since nothing embeds a version string.
- **Structure:** a `prepare` step (skip-logic + version + tag + GitHub Release, created as a **draft**) fans out to the `linux` and `darwin` build steps, which depend only on `prepare` and run in **parallel** on separate agents. A `publish` step depends on both build steps, verifies every required asset's checksum, and flips the release from draft to published — wall-clock is `prepare + max(linux, darwin) + publish`. The `concurrency_group` lives on `prepare` so two release runs can't create tags at once.
- **Loop prevention:** no commit is pushed to `main` (only a tag), so there is no self-trigger; push-triggered builds are disabled; and the idempotency guard no-ops any run whose `HEAD` is already the latest release commit.

---

## Relationship to the unified release tool proposal

[`docs/designs/unified-release-tool-for-mono.md`](../../../docs/designs/unified-release-tool-for-mono.md) proposes a shared `//tools/release` binary that would eventually own the `prepare`/`upload`/`publish` logic behind a thin per-product shell step, absorbing this script's duplication with checkleft's. That design is **proposed, not implemented** (a partial `tools/release/` crate exists on `main` but is wired into no pipeline), and the decision has been made to add this pipeline now, by cloning checkleft's shape, rather than block on that design landing. This pipeline does not conflict with it: task 3 of that design's migration path already plans to rewrite `checkleft-release.sh` down to a thin build-only script once `//tools/release` exists, and `changelog-release.sh` would migrate the same way — its `prepare`/`linux`/`darwin`/`publish` phases map onto that design's `prepare`/`upload`/`publish` subcommands with no structural rework. If anything, this pipeline is a second concrete data point (alongside checkleft) for how much of the script is genuinely shared.

One second-order effect worth naming: the unified design's stated goal is to make `changelog`'s enriched release notes available to `appoint` by linking `git_changelog` into `//tools/release` as a library, so `appoint` gets notes via the shared tool rather than by shelling out to a separately-published `bin/changelog`. This pipeline solves the same near-term problem (`appoint`'s `bin/changelog` fallback to `--generate-notes`) more directly, by publishing `changelog` itself as a prebuilt. Both paths are legitimate and not mutually exclusive — a future `appoint` could consume either the prebuilt `changelog` binary or the unified `release` tool's notes subcommand — but it means this pipeline's existence slightly reduces the urgency (not the validity) of that part of the unified design.

---

## Related

- [`../../../.buildkite/pipeline-changelog-release.yml`](../../../.buildkite/pipeline-changelog-release.yml) — pipeline definition
- [`../../../.buildkite/steps/changelog-release.sh`](../../../.buildkite/steps/changelog-release.sh) — release script
- [`../../checkleft/docs/buildkite-release-setup.md`](../../checkleft/docs/buildkite-release-setup.md) — the checkleft release pipeline this is modeled on
- [`../../../docs/designs/unified-release-tool-for-mono.md`](../../../docs/designs/unified-release-tool-for-mono.md) — the proposed shared release tool this pipeline deliberately does not wait for
