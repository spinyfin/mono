# NativeLink Grafana dashboards

These exports came from headcrash on 2026-11-22, including the overview's Hub health and Cache eviction rows, Hub restart / OOM-kill annotations, Queue time panel (p50/p90/p99/mean and oldest queued action), and workers host-resource queries that aggregate `by (instance)` so extra target labels do not split series. Dashboard UIDs, queries, panel IDs and positions are preserved. Dashboard-level `id` and `version` are omitted, object keys are sorted, and each panel has an explicit datasource. Text and row panels use Grafana's built-in datasource; metric panels retain the host's Prometheus UID `PBFA97CFB590B2093`.

## Publishing

The mono Buildkite pipeline selects publication after its build, test and check steps succeed on `main`. It uploads `pipeline.yml` with `--apply-if-changed --git-diff-base=HEAD^`, comparing the merged commit to its first parent. Changes under this directory or `tools/remote-execution/prometheus/` enqueue the publisher (`publish.sh` publishes both the Grafana dashboards and [the Prometheus config](../prometheus/README.md)). The dedicated `nativelink-grafana` queue runs on headcrash; publications are serialized. Before writing, the publisher fetches `main` and rejects a stale checkout whose Grafana or Prometheus sources differ from current main. Retry the latest main build after a stale-build failure. See [Buildkite change filtering](https://buildkite.com/docs/pipelines/configure/dynamic-pipelines/if-changed).

We use file provisioning because headcrash already provisions these dashboards from disk. This needs no Grafana API token or general remote shell access. The root-owned installer accepts a bounded JSON bundle on stdin and has fixed destinations:

| Source                     | Destination                                                   | Ownership / mode          |
| -------------------------- | ------------------------------------------------------------- | ------------------------- |
| `nativelink-overview.json` | `/etc/grafana/nativelink-dashboards/nativelink-overview.json` | `grafana:grafana`, `0644` |
| `nativelink-workers.json`  | `/etc/grafana/nativelink-dashboards/nativelink-workers.json`  | `grafana:grafana`, `0644` |
| `nativelink.yaml`          | `/etc/grafana/provisioning/dashboards/nativelink.yaml`        | `root:grafana`, `0644`    |

The new dashboard directory is `root:root`, `0755`. It is separate from `/var/lib/grafana/dashboards` so the provider cannot ingest unrelated files or old backups. The installer neither deletes nor rewrites the old files, other providers, or other dashboards. It accepts only the two fixed NativeLink UIDs and the exact provider policy in `validate.py`.

Files are replaced atomically, individually, after validating the whole bundle. Identical dashboard files retain their modification times. A partial write failure fails the job; retry reconciles the complete bundle. A provider change restarts `grafana-server.service` once to load provisioning, briefly interrupting Grafana, and leaves a retry marker if restart fails. Normal dashboard-only changes need no restart: Grafana polls every 30 seconds. `allowUiUpdates: false` prevents saving divergent UI changes. See [Grafana provisioning](https://grafana.com/docs/grafana/latest/administration/provisioning/).

## One-time enablement on headcrash

**Publishing is inactive until a host administrator installs the helper and enables the dedicated queue.** Publishing never runs from a developer laptop: `publish.sh` runs only in a main-branch Buildkite job on the `nativelink-grafana` queue on headcrash. Bootstrap only the publisher machinery on headcrash; the first successful main Buildkite job installs the dashboards and provider.

1. Create a dedicated unprivileged account, `nativelink-publisher`, with a Buildkite agent tagged `queue=nativelink-grafana`. Restrict that queue through Buildkite to the mono pipeline's trusted main publications; do not run PR builds or general jobs on it. It needs Python 3, Bash, Git, the agent binary, read access to the mono repository, and network access for Git/Buildkite. No Grafana token or SSH credential is needed. The agents uploading the pipeline need Buildkite agent 3.103.0 or newer for the explicit change-filter flags.
2. From a reviewed mono checkout **on headcrash**, install `install.py` (`0755`), `validate.py` (`0644`), and `../root_fs.py` (`0644`) into `/usr/local/libexec/nativelink-grafana/`. The directory and its ancestors must be root-owned and not writable by the publishing account. Set the directory to `0755`, all owned by `root:root`. The helper's isolated Python shebang ignores caller-supplied Python environment settings. Updates to these trusted helper files require the host administrator to review and reinstall them; the pipeline cannot upgrade its own privilege boundary.
3. Confirm `/etc/grafana`, `/etc/grafana/provisioning`, and `/etc/grafana/provisioning/dashboards` are root-owned and not group/world writable. The installer checks all ancestor directories and refuses symlinked destinations. The `grafana` account and `/usr/bin/systemctl` must exist.
4. Use `visudo` to install exactly this sudoers rule (the empty argument string forbids arguments):

   ```sudoers
   nativelink-publisher ALL=(root) NOPASSWD: /usr/local/libexec/nativelink-grafana/install.py ""
   ```

   Grant no sudo access to shells, Python interpreters, `install`, `cp`, `chown`, or `systemctl` themselves. The helper alone writes the three fixed files, its lock/retry marker, and restarts/checks the one Grafana service. It imports only its adjacent root-owned `validate.py` and `root_fs.py`.

5. Also install the Prometheus helper (`/usr/local/libexec/nativelink-prometheus/{install.py,validate.py,root_fs.py}`) and its sudoers rule per [the Prometheus README](../prometheus/README.md#one-time-enablement-on-headcrash): `publish.sh` invokes both helpers unconditionally under `pipefail`, so every publication fails until both are installed.

6. Run the mono main build containing this change through Buildkite. Confirm the publication log succeeds and both dashboard UIDs load in Grafana's NativeLink folder after 30 seconds. Confirm the Queue time panel has five series and a UI save is rejected. The initial restart is intentional; no other service is restarted.
7. After confirming both UIDs load from `/etc/grafana/nativelink-dashboards`, archive or remove the old copies (`/var/lib/grafana/dashboards/nativelink-overview.json`, `/var/lib/grafana/dashboards/nativelink-workers.json`, and `/etc/grafana/nativelink-overview.json.upstream`) so stale hand-edited files do not remain on disk, and confirm no other file provider under `/etc/grafana/provisioning/dashboards/` scans `/var/lib/grafana/dashboards` (which would cause duplicate-UID conflicts).

## Editing and rollback

Edit the dashboard in Grafana, then export its JSON without saving over the provisioned dashboard. Export for the same Grafana instance, not for external sharing (which substitutes datasource placeholders). Normalize, validate, and format the export:

```sh
python3 tools/remote-execution/grafana/validate.py --normalize nativelink-overview.json < export.json
bin/checkleft fix
```

(Pass `nativelink-workers.json` for the workers dashboard.) `validate.py --normalize` strips dashboard-level `id`/`version`, adds the built-in `-- Grafana --` datasource to any `row` or `text` panels that omit one, sorts keys recursively, validates the dashboard, and writes the target file in this directory; `bin/checkleft fix` applies the repo's JSON formatting (`oxfmt`) so `bin/checkleft run` passes. Do not edit files on headcrash.

Run `bazel build --remote_download_outputs=minimal //tools/remote-execution/grafana:grafana_test`, `bazel test --remote_download_outputs=minimal //tools/remote-execution/grafana:grafana_test`, and `bin/checkleft run`. Validation parses both real exports, checks unique panel IDs (including collapsed rows), grid bounds/non-overlap and datasources, and enforces publication scope. Tests also exercise repeated publishing, failure/retry behavior, preservation of unrelated files and CI guards without touching a live host.

Commit, open a PR, and merge to `main`; Buildkite publishes. If a `main` build that changed `tools/remote-execution/grafana/**` or `tools/remote-execution/prometheus/**` is cancelled, skipped, or fails before publication, trigger a new `main` build in Buildkite with `NATIVELINK_GRAFANA_PUBLISH=1` to run a catch-up publish (which uploads `pipeline-force.yml` without `if_changed`). To roll back, restore the last good dashboard contents in a new PR and merge it. Do not rerun an old build to roll back: the stale-source guard deliberately rejects it. The new main build publishes the restored content; Grafana ignores export version numbers for file provisioning. Roll back publisher code through a reviewed host-admin reinstall if the privileged helper itself needs reverting.
