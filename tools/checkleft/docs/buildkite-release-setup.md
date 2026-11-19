# Buildkite: checkleft release setup

This is the operator guide for `mono-checkleft-release`, the Buildkite pipeline
that produces checkleft's Linux and macOS prebuilt binaries. External
repositories consume the GitHub Release assets and their `.sha256` sidecars
instead of building checkleft from a mono checkout.

The product-specific shell script builds binaries; the shared
[`//tools/release`](../../release/BUILD.bazel) tool owns the release record:
version resolution, change detection, tags, draft creation, release notes,
asset staging, checksums, retries, verification, and publication. Its
configuration for checkleft is [`../release.toml`](../release.toml).

- Pipeline definition: [`../../../.buildkite/pipeline-checkleft-release.yml`](../../../.buildkite/pipeline-checkleft-release.yml)
- Dynamically uploaded build fragment: [`../../../.buildkite/pipeline-checkleft-release-builds.yml`](../../../.buildkite/pipeline-checkleft-release-builds.yml)
- Product build step: [`../../../.buildkite/steps/checkleft-release.sh`](../../../.buildkite/steps/checkleft-release.sh)
- Shared release CLI: [`../../release/BUILD.bazel`](../../release/BUILD.bazel)

The checkleft pipeline remains separate from the main `mono` pipeline. Register
it in Buildkite; adding the files to the repository alone does not register a
pipeline.

## Triggering releases

| Trigger                  | When               | Behavior                                                                                              |
| ------------------------ | ------------------ | ----------------------------------------------------------------------------------------------------- |
| Buildkite schedule       | For example, daily | Skips unless a configured checkleft release path changed since the latest published tag.              |
| Manual build (UI or API) | On demand          | Cuts a release unless the current commit is already published; re-adopts a draft for the same commit. |

Do not configure push or pull-request builds. A release pushes a tag, never a
commit to `main`; schedule and manual triggers make that intent explicit.

`tools/checkleft/release.toml` is the single source for release policy:
`checkleft-v` tags, the alpha-counter version scheme, release-note paths, and
the required versus optional asset set. The shell step only maps product build
outputs to those declared asset names.

## One-time registration

All commands below assume the Buildkite CLI is authenticated.

```sh
bk whoami
bk use flunge
bk cluster list
```

Create the pipeline in the same cluster as `mono` so it can use the existing
`bazel-any` agents:

```sh
bk pipeline create "mono-checkleft-release" \
  --description "Release pipeline for the checkleft prebuilt binaries" \
  --repository "git@github.com:spinyfin/mono.git" \
  --cluster-id "<cluster-name-or-id>"
```

Set the pipeline's **Steps** configuration to this bootstrap step. The explicit
queue is required because the Default cluster has no default queue.

```yaml
steps:
  - label: ":pipeline: upload"
    command: "buildkite-agent pipeline upload .buildkite/pipeline-checkleft-release.yml"
    agents:
      queue: bazel-any
```

Then disable push and pull-request triggers in the pipeline's GitHub settings
and add the desired schedule, for example:

- Description: `checkleft release check`
- Cron: `0 7 * * *`
- Branch: `main`
- Commit: `HEAD`

No new credential is required. `release prepare` pushes the tag and uses `gh`
through the agents' existing ambient credentials. The pipeline never pushes a
commit to `main`, so it does not need a branch-protection bypass.

## Release flow

The static Linux `prepare` step invokes:

```sh
bin/release prepare --config tools/checkleft/release.toml
```

On a release, this creates (or manually resumes) a draft and records its tag in
Buildkite metadata. It then uploads the existing build fragment. A scheduled
no-op leaves no tag and uploads no build steps, so no Bazel builds run.

The `linux`, `musl`, and `darwin` steps each read the tag with `bin/release tag`,
build their local artifact, and pass it to `bin/release upload`. `publish` runs
after all three build steps and calls `bin/release publish`. It verifies the
required assets—and any optional asset that was uploaded—by downloading their
sidecars and hashing the downloaded files before publishing the draft.

Expected release assets are declared in `release.toml`:

- `checkleft-x86_64-unknown-linux-gnu` — required
- `checkleft-x86_64-unknown-linux-musl` — required
- `checkleft-aarch64-apple-darwin` — required
- `checkleft-x86_64-apple-darwin` — optional

Every listed binary has a `<name>.sha256` sidecar. A missing or corrupt required
asset fails publication and leaves the GitHub Release as a draft for recovery.

## Build behavior and the musl guarantee

All three build phases derive their version from the recorded `checkleft-v…`
tag. Before compiling, each phase stamps that version into its isolated CI
checkout's `tools/checkleft/Cargo.toml` and `Cargo.lock`; this is never
committed. checkleft's Bazel rules and Cargo builds read `CARGO_PKG_VERSION`
from that manifest, so the release binary reports the release version rather
than a generic development version.

The native Linux and Apple Silicon macOS artifacts are built through Bazel. The
static `x86_64-unknown-linux-musl` artifact is also built hermetically by Bazel
with `//tools/checkleft:checkleft_musl`; it is not a Cargo cross-build. After
building, the `musl` phase executes the binary and requires its reported version
to match the tag-derived version. Any Bazel, execution, or version-check failure
fails the phase and blocks publication.

The Darwin x86_64 artifact is the one exception: it remains a best-effort
`cargo build --target x86_64-apple-darwin` from the Apple Silicon agent. If
that Cargo cross-build fails, the phase logs a warning and uploads the required
Apple Silicon artifact alone. If present, its checksum is still verified before
publication.

## Verifying a release

Trigger a manual build:

```sh
bk build create \
  --pipeline mono-checkleft-release \
  --branch main \
  --message "Manual checkleft release"
```

The prepare step should create a draft, the three build phases should run in
parallel, and publish should make the release visible only after verification.
For example:

```sh
gh release view checkleft-v0.1.0-alpha.9 --repo spinyfin/mono
```

## Recovering from a draft

When a build or verification fails, the tag, draft, and any uploaded assets are
preserved deliberately. A failed `release upload` or `release publish` adds an
error annotation to the build with the error, whether the release is still a
draft, and how many scheduled attempts it has left.

### What the next run does

The shared tool handles a draft an earlier run left behind. These rules apply to
every product released with `bin/release`:

- **A draft for the commit being released is resumed.** A scheduled prepare run
  re-adopts that commit's draft and fans out the build steps again, with a
  warning annotation naming the draft, the attempt number and the run that left
  it. `release upload` uses `--clobber`, and `publish` verifies every asset
  before publishing, exactly as for a new release.
- **The attempts are limited.** Scheduled runs resume one draft at most
  `scheduled_resume_attempts` times (default 3). The count is stored on the
  draft. After that, scheduled prepare runs fail with an error annotation that
  says the release is stranded and how to retry or abandon it. They do not
  retry, so a release that cannot be published is not rebuilt on every cron
  tick forever.
- **A draft that may still be building is left alone.** The build steps are not
  serialized with `prepare`, so a second run's `prepare` can start while the
  first run is still compiling. For `in_flight_minutes` (default 60) after a
  run last started `prepare`, `upload` or `publish` on a draft, a scheduled run
  treats it as in progress: it reports that in an info annotation, exits 0 and
  uploads no build phases. The value must be longer than the longest gap
  between two phases, which is the slowest build including time its job spends
  waiting for an agent.
- **A draft for an older commit is abandoned.** Once the branch has moved, no
  run would ever resume the old draft. The run that releases the newer commit
  deletes each older unpublished draft and its tag after creating its own, and
  lists what it deleted in a warning annotation. Drafts a run worked on within
  `in_flight_minutes` are skipped. The new release takes the next number, so
  abandoning a draft never frees its number for a different commit.
- **A draft with no claim is given time first.** A draft created before claims
  existed carries no record of when a run last worked on it. The first run to
  see it records the time and otherwise leaves it alone; it is resumed or
  abandoned once `in_flight_minutes` have passed. An old leftover draft is
  therefore abandoned by the second release after this behaviour arrives, not
  the first.

Both limits are optional keys in the product's `release.toml`:

```toml
[recovery]
scheduled_resume_attempts = 3
in_flight_minutes = 60
```

A run records its claim (when it last worked on the draft, scheduled resumes so
far, and its build URL) as an HTML comment in the draft's notes. A run that
cannot write its claim does not resume the draft. `publish` removes the comment
in the edit that publishes.

Because runs can overlap here, two more checks apply. `release upload` refuses a
release that is already published, so a second run cannot replace verified
assets. `release publish` reads the release again after verifying and stops if
an asset was replaced in the meantime.

### Recovering by hand

- Retry the failed Buildkite job. `bin/release tag` reads the tag recorded by
  prepare, and `release upload` uses `--clobber`, so the job can replace its
  assets safely. Retry `publish` after the build jobs complete.
- Re-run the whole pipeline manually on the same commit. A manual prepare run
  always re-adopts that commit's draft, whatever its claim says, and does not
  use up a scheduled attempt. Setting `RELEASE_RESUME_DRAFT=1` on a scheduled
  build does the same.
- To repair an asset outside Buildkite, build it from the tagged checkout and
  invoke the shared tool directly, for example:

  ```sh
  bin/release upload --config tools/checkleft/release.toml \
    --tag checkleft-v0.1.0-alpha.9 \
    --asset checkleft-x86_64-unknown-linux-gnu=<path-to-binary>
  ```

  The command writes and uploads the checksum sidecar with the asset.

- To abandon the draft for the current commit instead of resuming it, delete
  both the release and its tag before starting a fresh release:

  ```sh
  gh release delete checkleft-v0.1.0-alpha.9 --repo spinyfin/mono --yes
  git push origin :refs/tags/checkleft-v0.1.0-alpha.9
  ```

  Deleting the newest tag by hand frees its number. If the branch moves before
  the next release, that number goes to a different commit. An agent checkout
  that still holds the old tag then fails `git fetch --tags`, reporting
  `would clobber existing tag`, until the stale tag is deleted there.

## The shared tool's own release

`//tools/release` is released by its own separate `mono-release` pipeline. It
uses the same prepare → parallel native builds → verify → publish structure,
but builds `bin/release` from the mono source checkout and first runs
`//tools/release:release_lib_test`. It publishes
`release-<target-triple>` assets and `.sha256` sidecars under `release-v*` tags.

That source-built path is the bootstrap answer: mono never needs a published
`release` binary in order to release a corrected one. External repositories
pin the published assets and their checksums. If such a pin is bad, the
consumer recovers by restoring its previous known-good pin; mono and the
checkleft pipeline remain able to cut a replacement from source.

## Related

- [`../../../.buildkite/pipeline-checkleft-release.yml`](../../../.buildkite/pipeline-checkleft-release.yml)
- [`../../../.buildkite/steps/checkleft-release.sh`](../../../.buildkite/steps/checkleft-release.sh)
- [`../release.toml`](../release.toml)
- [`../../release/BUILD.bazel`](../../release/BUILD.bazel)
