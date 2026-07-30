# Buildkite: checkleft release setup

This document is the operator checklist for the **checkleft release pipeline** — the Buildkite pipeline that builds prebuilt `checkleft` binaries for Linux and macOS and publishes them as assets on a GitHub Release of `spinyfin/mono`. External repos consume these prebuilts instead of building checkleft from source.

It is modeled on the Boss release pipeline; for that reference see [`../../boss/docs/buildkite-release-setup.md`](../../boss/docs/buildkite-release-setup.md). The two differ in one important way: Boss releases run as a _step inside the existing `mono` pipeline_, while checkleft releases run as a **separate pipeline** with its own cron schedule and its own manual trigger. **Creating the in-repo pipeline file is not enough — the pipeline must be registered in Buildkite using the steps below.**

- Pipeline definition: [`../../../.buildkite/pipeline-checkleft-release.yml`](../../../.buildkite/pipeline-checkleft-release.yml)
- Release script: [`../../../.buildkite/steps/checkleft-release.sh`](../../../.buildkite/steps/checkleft-release.sh)
- Version source of truth: [`../Cargo.toml`](../Cargo.toml) (`version = "0.1.0-alpha.N"`)

---

## How releases are triggered

| Trigger                                                            | When       | Change-detection                                                           |
| ------------------------------------------------------------------ | ---------- | -------------------------------------------------------------------------- |
| Buildkite cron schedule                                            | e.g. daily | Skips if nothing under checkleft changed since the last `checkleft-v*` tag |
| Manual build (`bk build create`, BK UI **New Build**, or REST API) | On demand  | Always releases (skips change-detection)                                   |

The pipeline should **not** be wired to build on push. It pushes only a tag, never a commit to `main` (the version bump is patched into the release checkout, never committed), and an idempotency guard no-ops any run whose `HEAD` is already the latest release commit — but the cleanest configuration is push-builds disabled, schedule + manual only.

The org slug is `flunge`; the GitHub repo is `spinyfin/mono`. (Boss's release build URLs look like `https://buildkite.com/flunge/mono/builds/N`.)

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
bk pipeline create "mono-checkleft-release" \
  --description "Release pipeline for the checkleft prebuilt binaries" \
  --repository "git@github.com:spinyfin/mono.git" \
  --cluster-id "<cluster-name-or-id>"
```

This creates the pipeline and connects it to the GitHub repo (which provisions the webhook). Confirm with `bk pipeline view mono-checkleft-release`.

### 3. Point the pipeline at the release YAML

`bk pipeline create` does not upload the steps; like every pipeline in this repo, the registered pipeline must run a single bootstrap step that uploads the in-repo definition. The bootstrap step **must target a queue** (`bazel-any`) — the Default cluster has no default queue, so an untargeted step fails with "no queue has been targeted". Set the pipeline's **Steps** (Buildkite UI → Pipeline → **Settings** → **Steps**, or via the REST API) to exactly:

```yaml
steps:
  - label: ":pipeline: upload"
    command: "buildkite-agent pipeline upload .buildkite/pipeline-checkleft-release.yml"
    agents:
      queue: bazel-any
```

(The default pipeline-upload command reads `.buildkite/pipeline.yml`; the explicit path is what makes this pipeline use the checkleft definition.)

To do it via the API instead of the UI:

```sh
bk api --method PATCH /pipelines/mono-checkleft-release --data '{"configuration":"steps:\n  - label: \":pipeline:\"\n    command: buildkite-agent pipeline upload .buildkite/pipeline-checkleft-release.yml\n    agents:\n      queue: bazel-any\n"}'
```

### 4. Disable push-triggered builds

In Pipeline **Settings** → **GitHub**, turn **off** "Trigger builds when branches are pushed" (and any PR triggers). Releases come only from the cron schedule and manual builds. The release pushes only a tag (never a commit to `main`), so there is no self-trigger to guard against — push-builds-off simply keeps the pipeline schedule/manual-only.

### 5. Create the cron schedule

In Pipeline **Settings** → **Schedules** → **New Schedule**:

- **Description:** `checkleft release check`
- **Cron interval:** `0 7 * * *` (daily 07:00 UTC — adjust to taste)
- **Branch:** `main`
- **Message:** `Scheduled checkleft release check`
- **Commit:** `HEAD`

If a scheduled run finds no checkleft-affecting changes since the last `checkleft-v*` tag, the build logs `release skipped: ...` and exits 0 without cutting a release.

### 6. GitHub authentication — nothing to provision

No release token or secret is needed. The release pushes the tag with `git push origin` and creates the GitHub Release with `gh`, both authenticating via the CI agents' **ambient credentials** — exactly like the boss release step in the `mono` pipeline. Every CI worker already has push-capable git auth and `gh` access to `spinyfin/mono`, so the pipeline works without any pipeline-specific token.

No branch-protection bypass is involved either: the release only pushes a **tag** (which protected branches permit) and never a commit to `main`.

### 7. Ensure the Linux agents have musl tooling

The static `x86_64-unknown-linux-musl` asset is built hermetically via Bazel (`//tools/checkleft:checkleft_musl`) by its own `musl` build phase, and it is **release-blocking**: it is a `REQUIRED_ASSETS` entry, so a failed musl build fails the whole release and leaves it as an unpublished draft rather than shipping without it. The Bazel hermetic toolchain means no host-side musl provisioning is required beyond what Bazel already fetches.

---

## Triggering a release manually

```sh
bk build create \
  --pipeline mono-checkleft-release \
  --branch main \
  --message "Manual checkleft release"
```

Because `BUILDKITE_SOURCE` is `api`/`ui`, change-detection is skipped and a release is always cut. The BK UI **New Build** button does the same.

---

## Verifying the setup

1. Trigger a manual build (above) and open the build URL.
2. The **prepare** step should compute the next version, push the tag, and create the GitHub Release as a **draft** (not yet visible to normal `gh release view` / `gh release list` consumers as a published release).
3. The **linux**, **musl**, and **darwin** steps then run in parallel, each building its binaries and uploading them to that draft release. The **publish** step runs last (`depends_on` all three): it re-downloads and re-verifies every required asset's checksum, then flips the release from draft to published.
4. Confirm the release and its assets:

```sh
gh release view checkleft-v0.1.0-alpha.9 --repo spinyfin/mono
```

Expected assets (named by Rust target triple, each with a `.sha256` sidecar):

- `checkleft-x86_64-unknown-linux-gnu` — **required**
- `checkleft-x86_64-unknown-linux-musl` — **required**
- `checkleft-aarch64-apple-darwin` — **required**
- `checkleft-x86_64-apple-darwin` — **optional**: verified if present, but does not block publish if the darwin x86_64 cross-build fails (see `phase_darwin`'s warning-only fallback)

A missing or checksum-mismatched **required** asset fails the `publish` step and leaves the release as an unpublished draft; the required/optional split is declared explicitly as `REQUIRED_ASSETS`/`OPTIONAL_ASSETS` in `checkleft-release.sh`.

---

## Recovering from a partial release

`prepare` creates the tag and the GitHub Release as a **draft** before any build runs, then the `linux`, `musl`, and `darwin` build steps attach their assets in parallel, and `publish` verifies + publishes at the end. If a build step fails, or `publish` finds a missing/mismatched asset, the release is left as a draft — never published — with whatever assets did upload still attached. To recover:

- **Re-run the failed build job** (`bk job retry <job-id>`) — it reads the tag from build meta-data and re-uploads (assets use `--clobber`), so it picks up where it left off. Re-run `publish` afterwards (or it re-runs automatically as part of a full pipeline retry) to verify and publish.
- **Or upload manually** from an agent of the right OS, checked out at the tag:

  ```sh
  CHECKLEFT_RELEASE_TAG=checkleft-v0.1.0-alpha.9 \
    .buildkite/steps/checkleft-release.sh darwin   # or: linux / musl
  ```

- **Or re-trigger the whole pipeline manually** (`bk build create` / BK UI **New Build**) on the same commit. Because the trigger is manual (`BUILDKITE_SOURCE` is `ui`/`api`), `prepare`'s resume-existing-draft check re-adopts the existing draft/tag instead of computing a new version, and re-uploads the build fragment (skipping the upload if the fragment is already present in this build, e.g. a retried `prepare` job) — so the leftover draft is not orphaned and the fan-out build phases attach the remaining assets. A **scheduled (cron)** trigger will refuse to auto-resume a stranded draft — see "Abandoning a draft release" below — to avoid silently retrying a stuck release forever.

(If `prepare` itself fails before the Release is created, its cleanup trap deletes any tag it pushed, so a fresh run starts clean.)

## Abandoning a draft release

If a draft release is stuck (e.g. a persistent agent-pool problem keeps failing the same build/publish phase) and you do not want to keep resuming it, delete the draft and its tag so the next run — scheduled or manual — computes a fresh version instead of finding the stranded draft:

```sh
gh release delete checkleft-v0.1.0-alpha.9 --repo spinyfin/mono --yes
git push origin :refs/tags/checkleft-v0.1.0-alpha.9
```

A scheduled build that finds a draft for the current `HEAD` refuses to resume it on its own (see above) and names this exact recovery in its failure message, so a cron tick never gets stuck retrying indefinitely. To force a scheduled build to resume a draft instead of abandoning it, set `CHECKLEFT_RESUME_DRAFT=1` on that build.

---

## How it works (summary)

- **Version:** only the `-alpha.N` counter is revved (the base `X.Y.Z` is carried through). The next N is `max(Cargo.toml alpha, highest published checkleft-v* alpha) + 1`, so a stale Cargo.toml can never reuse a published alpha — which is exactly why the bump never has to be committed back to `main`. The new version is patched into `tools/checkleft/Cargo.toml` + `Cargo.lock` in the release checkout (never committed) and the release **commit** (`BUILDKITE_COMMIT`) is tagged `checkleft-vX.Y.Z-alpha.N`. `main`'s `Cargo.toml` stays at whatever version it last held, so developer builds carry a non-meaningful version — intentional and harmless (see Build tool).
- **Build tool:** native binaries are built with `bazel build -c opt //tools/checkleft:checkleft` (matches how mono builds checkleft and reuses the CI disk cache); the cross targets (`x86_64-apple-darwin`, `x86_64-unknown-linux-musl`) are built with `cargo --target`, since those triples are not registered in mono's bazel toolchains. checkleft's CLI does not embed `CARGO_PKG_VERSION`, so all binaries are byte-identical regardless of the version string — the patched-in version is for tree-consistency, not the artifact bytes; all phases build at `BUILDKITE_COMMIT`.
- **Structure:** a `prepare` step (skip-logic + version + tag + GitHub Release, created as a **draft**) fans out to the `linux`, `musl`, and `darwin` build steps, which depend only on `prepare` and run in **parallel** on separate agents. A `publish` step depends on all three build steps, verifies every required asset's checksum, and flips the release from draft to published — wall-clock is `prepare + max(linux, musl, darwin) + publish`. The `concurrency_group` lives on `prepare` so two release runs can't create tags at once.
- **Loop prevention:** no commit is pushed to `main` (only a tag), so there is no self-trigger; push-triggered builds are disabled; and the idempotency guard no-ops any run whose `HEAD` is already the latest release commit.

---

## Related

- [`../../../.buildkite/pipeline-checkleft-release.yml`](../../../.buildkite/pipeline-checkleft-release.yml) — pipeline definition
- [`../../../.buildkite/steps/checkleft-release.sh`](../../../.buildkite/steps/checkleft-release.sh) — release script
- [`../../boss/docs/buildkite-release-setup.md`](../../boss/docs/buildkite-release-setup.md) — the Boss release pipeline this is modeled on
