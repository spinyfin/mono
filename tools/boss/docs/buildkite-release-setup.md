# Buildkite: Boss release setup

This document is the operator checklist for the **Boss release step** — the `boss-release` step in the existing `mono` pipeline that builds `Boss.app` and publishes it as `Boss-1.0.N.zip` on a GitHub Release of `spinyfin/mono` tagged `boss-v1.0.N`. Unlike checkleft, Boss is **not** a separately registered pipeline: it is one step inside [`.buildkite/pipeline.yml`](../../../.buildkite/pipeline.yml), gated on `main` and on the three PR steps going green.

Version resolution, skip/idempotency, notes, the draft GitHub Release, the `.sha256` sidecar, verification, and publish are owned by the shared `//tools/release` tool. This step keeps only what is unique to Boss: loading `BOSS_SHAKE_*` compile secrets, the GhosttyKit stub, `bazel build -c opt --define=...`, and `.zip` `cquery` discovery.

The pipeline also needs the shake credentials (for embedding GitHub App credentials into the release binary). Those are documented separately in [`buildkite-shake-secrets-setup.md`](buildkite-shake-secrets-setup.md).

- Pipeline step: [`.buildkite/pipeline.yml`](../../../.buildkite/pipeline.yml) (`boss-release`, lines 39–53)
- Release script: [`.buildkite/steps/boss-release.sh`](../../../.buildkite/steps/boss-release.sh)
- Product record: [`../release.toml`](../release.toml)
- Shared tool: [`../../release/`](../../release/) (invoked as `bin/release` via [REPOBIN.toml](../../../REPOBIN.toml))

---

## How releases are triggered

The `boss-release` step in `.buildkite/pipeline.yml` only fires when
`BUILDKITE_SOURCE` is `schedule`, `ui`, or `api`. It does **not** fire on every
merge to main. There are two normal trigger paths:

| Trigger                           | When              | Change-detection                                            |
| --------------------------------- | ----------------- | ----------------------------------------------------------- |
| Hourly cron schedule              | Every hour at :00 | Skips if no Boss-affecting changes since last `boss-v*` tag |
| `boss release` CLI (or BK UI/API) | On demand         | Always releases (skips change-detection)                    |

A push or pull-request build never reaches this step (`pipeline.yml`'s `if:`), and the shared tool also refuses those trigger sources as defence in depth.

---

## Shared release tool

Inside mono the tool is always **built from source** and never consumed as a prebuilt. `ci-env.sh` runs `repobin install`, which puts `bin/release` on the step's path as a shim for `//tools/release:release`. There is no chicken-and-egg: the tool at HEAD releases Boss at HEAD. A broken published `release-v*` tag cannot block a Boss release.

`tools/boss/release.toml` is the only per-product input. It names the repo, the `boss-v` tag prefix, the `1.0` patch-counter, changelog notes from `tools/boss/PROJECT.yaml`, the change-detection paths, the required asset `Boss-{version}.zip` (`{version}` expands to the version encoded in the tag, so the published name stays `Boss-1.0.N.zip`), and the `[recovery]` limits described under [Recovering from a partial release](#recovering-from-a-partial-release).

The script makes three calls in one Buildkite step — the tool does not care that they are not three jobs:

1. `release prepare` — skip or proceed, compute `boss-v1.0.N`, tag HEAD, generate notes, create the GitHub Release as a **draft**, print the tag.
2. `release upload` — stage `Boss-1.0.N.zip` plus `Boss-1.0.N.zip.sha256`, upload with `--clobber`.
3. `release publish` — re-download the zip and sidecar from GitHub, re-hash, then flip the draft to published. Each download attempt is given two minutes plus the time the asset needs at 1 Mbit/s (about ten and a half minutes for a 64 MB zip); an attempt that fails or runs out of time is retried, three attempts in all.

A published release now means "the zip is present and checksum-correct". The previous published-first ordering could advertise a resolvable-but-assetless tag (`boss-v1.0.21`); draft-then-publish closes that. Anything polling for the newest _published_ release will see it appear at the end of the step rather than at tag-creation time. The auto-update design already skips assetless releases, so the change is strictly an improvement.

The asset name `Boss-1.0.N.zip` and the `boss-v1.0.N` tag scheme are unchanged, so [`automatic-boss-updates.md`](designs/automatic-boss-updates.md)'s resolution logic is unaffected. The new `Boss-1.0.N.zip.sha256` sidecar is extra; UpdateChecker does not require it.

---

## One-time setup

The `boss-release` step is already registered as part of the `mono` pipeline. The remaining operator setup is the hourly schedule, the `boss release` CLI token, and the shake secrets.

### 1. Configure the hourly cron schedule in Buildkite

This is a one-time setup in the Buildkite web UI.

1. Go to the `spinyfin/mono` pipeline in the Buildkite dashboard.
2. Click **Settings** → **Schedules** → **New Schedule**.
3. Fill in the fields:
   - **Description:** `Boss hourly release`
   - **Cron interval:** `0 * * * *` (every hour on the hour)
   - **Branch:** `main`
   - **Message:** `Hourly Boss release check` (shown in the BK build list)
   - **Commit:** `HEAD`
4. Click **Create Schedule**.

The schedule fires every hour. If no Boss-affecting files have changed since the last published `boss-v*` tag, `release prepare` logs `release skipped: ...` and the step exits 0 without creating a release.

### 2. Provision `BK_API_TOKEN` for `boss release`

The `boss release` CLI subcommand calls the Buildkite REST API to trigger a
build. It reads the token from the `BK_API_TOKEN` environment variable.

#### Create an API token

1. In the Buildkite dashboard, go to your **Personal Settings** → **API Access
   Tokens** → **New API Access Token**.
2. Give it a description: `boss release CLI`.
3. Grant the **Write Builds** scope on the `spinyfin` organization (or
   narrower: just the `mono` pipeline if Buildkite supports pipeline-scoped
   tokens in your plan).
4. Click **Create Token** and copy the value — it is only shown once.

#### Set the env var

Add it to your shell profile (e.g. `~/.zshrc` or `~/.bashrc`):

```sh
export BK_API_TOKEN="your-token-here"
```

Reload your shell or run `source ~/.zshrc`.

### 3. Shake credentials

Required for embedding GitHub App credentials into the release binary. Follow [`buildkite-shake-secrets-setup.md`](buildkite-shake-secrets-setup.md). `BOSS_SHAKE_*` stay in this step; the shared tool never sees them.

### 4. GitHub authentication — nothing to provision

No release token is needed. The tool pushes the tag with `git push origin` and creates/uploads/publishes the GitHub Release with `gh`, both authenticating via the CI agents' **ambient credentials**. Every CI worker already has push-capable git auth and `gh` access to `spinyfin/mono`.

No branch-protection bypass is involved: the release only pushes a **tag** (which protected branches permit) and never a commit to `main`.

---

## Triggering a release manually

```sh
boss release
```

Expected output (on success):

```
triggered release build #42: https://buildkite.com/flunge/mono/builds/42
```

Open the URL and confirm the `boss-release` step appears and runs.

If `BK_API_TOKEN` is missing:

```
error: BK_API_TOKEN is not set. See tools/boss/docs/buildkite-release-setup.md ...
```

You can also trigger a release directly from the Buildkite UI:

1. Go to the `spinyfin/mono` pipeline.
2. Click **New Build**.
3. Set **Branch** to `main` and add a message, then click **Create Build**.

Because `BUILDKITE_SOURCE` will be `ui`/`api`, change-detection is skipped and a release is always cut (unless `HEAD` is already the latest published `boss-v*` commit — that path is a no-op on every trigger).

---

## Verifying the setup

1. Trigger a manual build (above) and open the build URL.
2. `release prepare` should compute the next `boss-v1.0.N`, push the tag, and create the GitHub Release as a **draft**.
3. The same step then builds `Boss.app`, uploads `Boss-1.0.N.zip` and `Boss-1.0.N.zip.sha256` to that draft, re-downloads them, verifies the checksum, and flips the release from draft to published.
4. Confirm the release and its assets:

```sh
gh release view boss-v1.0.N --repo spinyfin/mono
```

Expected assets:

- `Boss-1.0.N.zip` — **required**; the name UpdateChecker resolves by exact string
- `Boss-1.0.N.zip.sha256` — **required**; `sha256sum -c` sidecar written by `release upload`

A missing or checksum-mismatched required asset fails `release publish` and leaves the release as an unpublished draft. So does a verification download that fails three times. [Recovering from a partial release](#recovering-from-a-partial-release) covers what happens to that draft.

After the next top-of-hour fires, check the BK builds list for a build with message `Hourly Boss release check`. If Boss-affecting files changed since the last published tag, a new `boss-v1.0.N` release will appear on GitHub. If not, the build will show a line like:

```
release skipped: no release-affecting changes since boss-v1.0.N
```

---

## Recovering from a partial release

`prepare` creates the tag and the GitHub Release as a **draft** before the zip is built, then `upload` attaches the zip and sidecar, and `publish` verifies and publishes at the end. If the build, upload, or publish fails, the release is left as a draft, never published, with whatever assets did upload still attached. When `upload` or `publish` is the step that failed, it adds an error annotation to the build page with the error, whether the release is still a draft, and how many scheduled attempts it has left.

A draft left behind is handled by the next run. Nobody has to notice it first.

### What the next run does

**`main` has not moved: the draft is resumed.** A scheduled run that finds an unpublished draft for the commit it is releasing picks it up. It keeps the tag, rebuilds the zip, uploads it with `--clobber`, and runs the full download-and-checksum verification before publishing, so a resumed release is verified exactly like a new one. The run adds a warning annotation naming the draft, the attempt number, and the run that left the draft behind.

**The attempts are limited.** Scheduled runs resume one draft at most three times (`scheduled_resume_attempts` in `tools/boss/release.toml`, default 3). After the third, further scheduled runs fail at `prepare` with an error annotation: the release is stranded, which run tried last, and the commands under [Recovering by hand](#recovering-by-hand). They do not try again. A release that has failed four times in a row on one commit needs a person, and `boss-release` stays red until someone acts or `main` moves. This is what stops a release that cannot be published from being retried on every schedule tick forever.

**`main` has moved: the draft is abandoned.** A run only ever resumes a draft for its own commit, so a draft for an older commit would otherwise sit there for good. The run that cuts the release for the newer commit deletes each older unpublished draft and its tag, and adds a warning annotation listing what it deleted. It does this after creating its own tag and draft, and the new release takes the next number, so abandoning a draft never frees its number for a different commit. A draft is only deleted if its commit is an ancestor of the commit being released, no published release uses its tag, and GitHub still reports it as a draft when it is read again just before the delete. A draft that could not be deleted is listed in the same annotation with the reason, and a later run tries again. If a draft is deleted but its tag is not, the annotation gives the command that deletes the tag.

A run that skips (nothing to release) does not look at older drafts. They wait for the next run that cuts or resumes a release.

### The claim on a draft

Each run records a claim in the notes of the draft it is working on, as one HTML comment that the release page does not render:

```
<!-- release-tool: claimed-at=1795059287 scheduled-resumes=1 build=https://buildkite.com/flunge/mono/builds/27488 -->
```

`claimed-at` is when a run last started `prepare`, `upload` or `publish` on the draft, `scheduled-resumes` counts the scheduled resumes so far, and `build` is the run that created or resumed it. Because the count is stored on the draft, it holds across builds and agents. A run that cannot write its claim does not resume the draft, whether it was scheduled or started by hand. `publish` removes the comment in the same edit that publishes, so it never reaches published notes.

### Drafts that are still being built

A draft is not always a failure: it also exists while a release is in progress. `in_flight_minutes` says how long after a run last worked on a draft other runs must assume it is still at it. During that time a scheduled run leaves a draft for its commit alone, says so in an info annotation and exits 0, and no run abandons the draft. A draft with no claim at all, such as one created before claims existed, has an unknown age: the first run to see it records the time and waits out the same period before anything is done to it.

Boss sets `in_flight_minutes = 0`. The whole release is one `boss-release` job, and that step has `concurrency: 1` in `mono-boss-release`, so a second release job cannot start until the first has ended. Any draft a job finds was left behind, and the next scheduled run can resume or abandon it at once. Pipelines that run `prepare`, the builds and `publish` as separate jobs keep the default of 60 minutes, because there a second build's `prepare` can start while the first build is still compiling. If `boss-release` is ever split across jobs, raise the value above the longest gap between two phases, which is the slowest build including time spent waiting for an agent.

Two checks back this up for pipelines where runs can overlap. `upload` refuses a release that is already published, so a second run cannot replace verified assets. `publish` reads the release again after verifying and stops if an asset was replaced in the meantime, so what it publishes is what it verified.

### Recovering by hand

- **Start a build by hand** on the same commit (`boss release`, or New Build in the Buildkite UI). A manual run (`BUILDKITE_SOURCE` is `ui`/`api`) always resumes the draft for its commit, whatever the claim says, and does not use up a scheduled attempt.
- **Force a scheduled build to resume** by setting `RELEASE_RESUME_DRAFT=1` on it. This also ignores the claim and the attempts already used.
- **Retry the failed `boss-release` job** (`bk job retry <job-id>`). In a scheduled build this counts as one of the scheduled attempts.
- **Upload manually** from a macOS agent checked out at the tag, after building the zip with the same `-c opt --define=BOSS_SHAKE_*` flags, then:

  ```sh
  bin/release upload --config tools/boss/release.toml --tag boss-v1.0.N \
    --asset "Boss-1.0.N.zip=<path-to-zip>"
  bin/release publish --config tools/boss/release.toml --tag boss-v1.0.N
  ```

(If `prepare` itself fails before the Release is created, the tool deletes any tag it pushed, so a fresh run starts clean.)

## Abandoning a draft release

Drafts for commits that `main` has moved past are abandoned automatically, as described above. To abandon the draft for the current commit instead of resuming it (for example a persistent agent or secrets problem keeps failing the build), delete the draft and its tag:

```sh
gh release delete boss-v1.0.N --repo spinyfin/mono --yes
git push origin :refs/tags/boss-v1.0.N
```

The next run, scheduled or manual, then finds no draft and computes a version as usual.

Deleting the newest tag by hand frees its number, which the automatic path never does. If `main` moves before the next release, that number goes to a different commit, and an agent checkout that still holds the old tag then fails the tool's `git fetch --tags` with `would clobber existing tag` until the stale tag is removed there (`git tag -d boss-v1.0.N`). Abandon by hand while `main` is still at the draft's commit, or leave the draft for the next release to abandon.

---

## How it works (summary)

- **Version:** `patch-counter` with literal `major_minor = "1.0"`. The next N is `highest published (or tagged) boss-v1.0.* + 1`, starting at `boss-v1.0.0` when none exist. The bump is never committed to `main`; it lives only in the tag and the GitHub Release. The release **commit** (`HEAD`) is tagged `boss-v1.0.N` _before_ Bazel runs, so `workspace-status.sh` can `git describe --exact-match` and stamp the binary with `1.0.N`.
- **Build tool:** `bazel build -c opt --define=BOSS_SHAKE_*=... //tools/boss/app-macos:Boss`. The `.zip` path is discovered with the same flag set via `cquery` (a different configuration would resolve the credential-free `fastbuild` zip left by `mac-app-build`). `BOSS_SHAKE_*` are compile inputs and never enter the shared tool.
- **Structure:** one Buildkite step, three tool calls (`prepare` → build → `upload` → `publish`). The release is created as a **draft** and published only after the required zip and sidecar re-download and checksum-verify.
- **Draft recovery:** a run that fails after `prepare` leaves a draft. The next scheduled run on the same commit resumes it, at most three times, and counts the attempts on the draft; after that scheduled runs fail until a person steps in. A run on a newer commit deletes the older draft and its tag once its own exist. Each of these is reported in a build annotation.
- **Loop prevention:** `pipeline.yml` only schedules the step on `main` + schedule/ui/api; the tool also refuses any other `BUILDKITE_SOURCE`; no commit is pushed to `main` (only a tag); and the idempotency guard no-ops any run whose `HEAD` is already the latest published release commit.

---

## Related

- [`.buildkite/pipeline.yml`](../../../.buildkite/pipeline.yml) — `boss-release` step definition
- [`.buildkite/steps/boss-release.sh`](../../../.buildkite/steps/boss-release.sh) — secrets, stub, build, cquery, then three `bin/release` calls
- [`../release.toml`](../release.toml) — Boss's product record for `//tools/release`
- [`../../release/`](../../release/) — shared prepare / upload / publish tool
- [`buildkite-shake-secrets-setup.md`](buildkite-shake-secrets-setup.md) — shake credential setup (required for embedding GitHub App credentials into the release binary)
- [`designs/automatic-boss-updates.md`](designs/automatic-boss-updates.md) — consumes `boss-v1.0.N` + `Boss-1.0.N.zip` by exact name
- [`../../checkleft/docs/buildkite-release-setup.md`](../../checkleft/docs/buildkite-release-setup.md) — sibling product on the same shared tool, as a separately registered pipeline
