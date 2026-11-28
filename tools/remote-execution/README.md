# Remote execution: the NativeLink hub and the macOS workers

This directory holds everything that is installed on the hosts that run mono's remote builds on macOS workers: the NativeLink configs, the units that start them, the patches the workers need, the hub's health check, the hub's restart and CAS exporters, and this runbook. Nothing here is built or read by a build. The files are copied to the hosts by hand, which is why they are in the repository: a change to how the scheduler behaves is a reviewed change, and `tests/config_test.sh` keeps the files consistent with each other. Copying a file whose content already matches the host is a no-op ("Installing or updating hub files").

What decides whether a build executes remotely is elsewhere: [`.buildkite/README.md`](../../.buildkite/README.md) for CI steps and [`tools/test-sandbox/README.md`](../test-sandbox/README.md) for a developer's Mac. The hub schedules the Linux workers too ("Routing" below). Their configs and units are not in this directory: [`.buildkite/linux-agents-runbook.md`](../../.buildkite/linux-agents-runbook.md), "Remote execution workers", describes them.

## What is here

| File                                                     | Installed as                                                 | What it is                                                                                                                                                               |
| -------------------------------------------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `nativelink/hub.json5`                                   | `~/nativelink-macos-hub/hub.json5` on the hub                | Scheduler, CAS and action cache. Every scheduler timeout, with its reason.                                                                                               |
| `nativelink/nativelink-macos-hub.service`                | `/etc/systemd/system/` on the hub (root)                     | Starts the hub, as a system unit (`User=bduff`).                                                                                                                         |
| `nativelink/hub-health.sh`                               | `~/nativelink-macos-hub/hub-health.sh` on the hub            | The health check.                                                                                                                                                        |
| `nativelink/nativelink-macos-hub-health.{service,timer}` | `~/.config/systemd/user/` on the hub                         | Runs the health check every minute.                                                                                                                                      |
| `nativelink/hub-textfile-metrics.sh`                     | `~/nativelink-macos-hub/hub-textfile-metrics.sh`             | Hub restart state for node_exporter's textfile collector.                                                                                                                |
| `nativelink/nativelink-hub-textfile.{service,timer}`     | `/etc/systemd/system/` on the hub (root)                     | Runs the restart exporter every 30 s.                                                                                                                                    |
| `nativelink/worker-slots-exporter.py`                    | `~/nativelink-macos-hub/worker-slots-exporter.py`            | Per-worker slots and CAS eviction figures, on 127.0.0.1:9108.                                                                                                            |
| `nativelink/nativelink-worker-slots-exporter.service`    | `~/.config/systemd/user/` on the hub                         | Starts the slots and CAS exporter.                                                                                                                                       |
| `nativelink/worker-macos.json5`                          | `~/nativelink/worker-macos.json5` on each Mac                | The worker. The same file on every Mac.                                                                                                                                  |
| `nativelink/com.brianduff.nativelink.plist`              | `~/Library/LaunchAgents/` on each Mac                        | Starts the worker and sets what differs per Mac.                                                                                                                         |
| `nativelink/action-env.sh`                               | `~/nativelink/action-env.sh` on each Mac                     | Wraps every action: picks the Xcode and SDK the action asks for.                                                                                                         |
| `nativelink/headcrash-worker/`                           | `~/nativelink/` and `~/.config/systemd/user/` on `headcrash` | The Linux worker on the hub's host: `worker.json5`, its user unit and the `memory.conf` and `60-v1.8.conf` drop-ins. Other Linux workers differ (see the Linux runbook). |
| `nativelink/build-worker.sh` and `nativelink/patches/`   | run on one Mac, and on the Linux hosts that need a binary    | Builds the worker binary, or with `--role hub` the hub's: the pinned release plus the patches.                                                                           |

## Hosts

The hub is a Linux host with the stores on an internal disk. It runs no actions. Its listen addresses, data directory and store size are in `~/nativelink-macos-hub/hub.env` on that host, which is not in the repository; `hub.json5` lists the variables. `NL_HUB_CAS_MAX_BYTES` is 95% of the filesystem that holds `NL_HUB_DATA`, minus 5 GB for the action cache (3734977866444 bytes on the current host).

The workers, as inspected on 2026-11-18:

| Mac           | Model    | Cores | RAM   | `NL_WORKER_SLOTS` | `NL_WORKER_STORE_MAX_BYTES` |
| ------------- | -------- | ----- | ----- | ----------------- | --------------------------- |
| `anaplian`    | Mac16,10 | 10    | 16 GB | 10                | 40000000000                 |
| `skaffen`     | Mac16,10 | 10    | 16 GB | 10                | 40000000000                 |
| `arbitrary`   | Mac18,5  | 12    | 16 GB | 12                | 25000000000                 |
| `determinist` | Mac18,5  | 12    | 16 GB | 12                | 25000000000                 |
| `rascal`      | Mac18,5  | 12    | 16 GB | 12                | 25000000000                 |

`zakalwe` (10 cores, 16 GB) was the hub and a ten-slot worker in one process, with its stores on an external disk. It stops being the hub at the cutover. If it stays in the pool it is a worker like the others, with `NL_WORKER_DATA` on its internal disk: stalls of the external disk were what the scheduler turned into the hang of 2026-11-18.

The Linux workers are the hosts in [`.buildkite/linux-agents-runbook.md`](../../.buildkite/linux-agents-runbook.md), "Remote execution workers".

The slot counts are the numbers the workers ran with before this change. They are sized by cores, and about four concurrent Rust compiles was the RAM-safe figure measured for a 16 GB host in September ([investigation](../../docs/investigations/nativelink-macos-remote-execution.md)), so expect memory pressure on a worker; the scheduler's `worker_timeout_s` is set with that in mind.

## Routing

One scheduler serves the macOS and the Linux workers, and one CAS and action cache serve both kinds of build. What keeps a macOS action off a Linux worker, and the reverse, is the `OSFamily` platform property:

- **Every worker declares one.** `OSFamily: macos` in `worker-macos.json5`, `OSFamily: linux` in a Linux worker's `worker.json5`.
- **The scheduler matches it exactly** (`OSFamily: "exact"` in `hub.json5`): an action is offered only to workers whose value equals the action's.
- **Every action has one by the time it is scheduled.** Linux builds send `OSFamily=linux` (`ci-remote-linux` in `.ci.bazelrc`). macOS builds send none, and an action without the property would be offered to every worker. The `property_modifier` layer in `hub.json5` therefore gives each arriving action a value: the one its client sent, or `macos` if it sent none.

| The client sends                       | The scheduler sees | Runs on                                                                 |
| -------------------------------------- | ------------------ | ----------------------------------------------------------------------- |
| no `OSFamily`                          | `OSFamily=macos`   | a macOS worker                                                          |
| `OSFamily=macos`                       | `OSFamily=macos`   | a macOS worker                                                          |
| `OSFamily=linux`                       | `OSFamily=linux`   | a Linux worker                                                          |
| `OSFamily=linux`, `worker-host=<name>` | the same           | that Linux worker only                                                  |
| any other `OSFamily`                   | the same           | nothing: it fails after `unsatisfiable_action_timeout_s` (five minutes) |

The default is filled in on the hub, after the client has computed the action's digest. So it changes no cache key, a macOS build needs no flag, and a checkout older than this arrangement routes correctly. `macos` is the default, and not `linux`, because the Linux builds were already tagged and the macOS ones were not.

Measured on 2026-11-22 against a hub with this `hub.json5` and three workers on loopback, one declaring `macos` and two `linux`, eight actions per row: the first four rows of the table ran where it says, and with the `macos` worker stopped an untagged action stayed queued (the hub's queue listing showed it as `OSFamily=macos`) while both `linux` workers sat idle.

**Naming one worker.** Every worker also declares `worker-host` with its own name. A build that adds `--remote_default_exec_properties=worker-host=<name>` runs only on that worker. The hub does not list the key, and a key it does not list restricts only the workers that declare it, so every worker on either operating system must declare one. For a Linux worker the build must also carry `OSFamily=linux`: without it the action is a macOS action and no macOS worker has that name, so it waits and fails.

**Adding a worker.** It must declare `OSFamily` with one of the two values, and `worker-host`. A worker that declares neither `macos` nor `linux` is never offered anything. A Linux worker must also run every action inside the sandbox the Linux runbook describes, because `ci-remote-linux` sends tests to whatever Linux worker is on this hub.

**A target with its own `exec_properties`** does not receive `--remote_default_exec_properties`. On a Linux build such a target would lose `OSFamily=linux` and be scheduled as a macOS action, so it has to carry the property itself.

**When one operating system's workers are all gone** and the other's are connected, that operating system's actions wait, and fail after `unsatisfiable_action_timeout_s`. With no worker connected at all they fail after `no_worker_action_timeout_s`.

## Version and patches

Every host runs NativeLink **1.8.6** (`37f3b206a8e6d55613abae2b18a0665fff24e096`), and the hub and the workers are upgraded together: an older binary rejects fields these configs use, and an older worker does not acknowledge a dispatch, which `dispatch_ack_timeout_s` relies on.

- **The hub** runs that release with one patch, `compressed-upload-restart.patch`, built by `build-worker.sh --role hub` on the hub's own host (the Linux binary links the host's libc). `nativelink --version` prints `nativelink 1.8.6+compressed-restart`. The workers' two patches are not in it: the hub stages no inputs and has no store to go stale. The upstream release the hub ran until 2026-11-23 is `nativelink-1.8.6-x86_64-unknown-linux-musl.tar.gz` from the [v1.8.6 release](https://github.com/TraceMachina/nativelink/releases/tag/v1.8.6), sha256 `9bad10aa1c11a00fb1544ddd28a5a5ab892cfa42817e96a6cf18f169df7f0472`, which prints `nativelink 1.8.6`; it is what to put back to take the patch out ("Updating the hub's binary").
- **The workers** run that release with the patches in [`nativelink/patches/`](nativelink/patches/), built from source by `build-worker.sh` on a host of the worker's platform. The script's `PATCHES` list says which patch goes into which platform's binary, and stamps the version from it: a macOS worker reports `nativelink 1.8.6+macos-clonefile.stale-entry` and a Linux worker `nativelink 1.8.6+stale-entry`, so a worker that was given an upstream binary by mistake, or one that is a patch behind, can be told apart. Every worker of a platform runs the same file: compare its sha256 after copying it.

| Patch                                                                                       | Workers         | Why                                                                                                                                       |
| ------------------------------------------------------------------------------------------- | --------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| [`macos-clonefile-executables.patch`](nativelink/patches/macos-clonefile-executables.patch) | macOS           | Upstream stages an executable as a hardlink out of the worker's store, and `rustc` then cannot load its own library.                      |
| [`stale-entry-recovery.patch`](nativelink/patches/stale-entry-recovery.patch)               | macOS and Linux | A store entry whose file is gone is never fetched again, and every action that needs the blob fails on that worker until it is restarted. |
| [`compressed-upload-restart.patch`](nativelink/patches/compressed-upload-restart.patch)     | the hub         | A compressed upload that is cut off fails the client's build instead of restarting.                                                       |

**The clone patch.** Upstream stages an executable input as a hardlink out of the worker's store. On macOS dyld resolves `@loader_path` from whichever name of that inode the kernel reports, finds the store, and `rustc` fails with `Library not loaded: @rpath/librustc_driver-<hash>.dylib`. The patch stages executables with `clonefile(2)` instead, which gives each its own inode. It is the commit the workers already ran on 1.6.6 (`brianduff/nativelink` `3125a4fd`), carried forward; the patch file's header lists what changed on the way. It is not upstream: [issue #3137](https://github.com/TraceMachina/nativelink/issues/3137) is open, and so are the two pull requests that address it differently (#3138, #3191). Do not turn on `directory_cache` in the worker config: it stages inputs by another path, which the patch does not touch.

**The stale-entry patch.** A worker's store answers "do you have this blob" from its index. If an entry's file is no longer on disk, the store still says yes, nothing fetches the blob, and every action that stages it fails: `executable-variant copy failed: … NotFound … Resolving executable variant for <digest>` for an executable input, `Could not make hardlink … file was likely evicted from cache` for a plain one. The failure is immediate, so the worker is free again at once and is handed the retries; one such entry for an input every test shares fails every test in the pool. This happened on a Linux worker on 2026-11-20, on 1.6.6, where a race in eviction left the entry behind. 1.8.6 closes that race and drops a stale entry when a blob is read through the store, but not on the two paths a worker stages inputs by; the patch covers those, so the blob is fetched again and the action runs. It is not upstream.

**The compressed-upload patch.** With `--remote_cache_compression` Bazel uploads a large blob as a compressed ByteStream write. When that stream is cut off (the likely cause is the `--remote_timeout` deadline on a big upload), Bazel resumes the standard way: `QueryWriteStatus`, then a new `Write` at `write_offset = committed_size`. 1.8.6 answers with the compressed bytes received so far, and then refuses any compressed `Write` that does not start at 0: `INVALID_ARGUMENT: Received out of order compressed data. Got N, expected 0 : In ByteStreamServer::write_compressed`. Bazel does not retry that, the upload fails, and the action result that needed it is never cached. The remote execution API makes the first offset of a compressed write an uncompressed one, so a non-zero resume is legal, and a server that cannot resume should answer 0 so that the client sends the blob again. The patch does that for an upload that is still in progress; an interrupted compressed upload costs a resend of the whole blob. It is [pull request #2891](https://github.com/TraceMachina/nativelink/pull/2891), unmerged, carried unchanged, and `main` and 1.8.6 have the same code, so a later release does not help until it is merged. The same bug in uncompressed uploads was fixed in [#2889](https://github.com/TraceMachina/nativelink/pull/2889) (1.8.0). Compression stays on: turning it off is not the remedy. The pull request's own test is the regression test: `build-worker.sh --role hub` runs it, and it fails on 1.8.6 at the `QueryWriteStatus` check.

Why 1.8.6 and not the 1.6.6 it replaces: on 2026-11-18 the 1.6.6 scheduler timed out operations on live workers after 5 s, evicted the workers, and went round again for hours while every client waited. The fixes, by the release that first had them:

| Fix                                                           | Release | What it changes                                                                               |
| ------------------------------------------------------------- | ------- | --------------------------------------------------------------------------------------------- |
| [#2797](https://github.com/TraceMachina/nativelink/pull/2797) | 1.7.3   | Any message from a worker counts as a sign of life, not only a keepalive; default 5 s → 10 s. |
| [#2826](https://github.com/TraceMachina/nativelink/pull/2826) | 1.7.3   | A live worker's action is no longer timed out over the worker's head.                         |
| [#2846](https://github.com/TraceMachina/nativelink/pull/2846) | 1.7.3   | A kill reaches an action that is fetching inputs or uploading, so it is acknowledged.         |
| [#2856](https://github.com/TraceMachina/nativelink/pull/2856) | 1.7.3   | A worker whose scheduler connection stalls reconnects instead of sitting idle.                |
| [#2900](https://github.com/TraceMachina/nativelink/pull/2900) | 1.7.6   | A clock step backwards no longer drops results and leaves actions executing.                  |
| [#2923](https://github.com/TraceMachina/nativelink/pull/2923) | 1.7.7   | Executing actions are swept on the scheduler's own clock, with or without a client.           |
| [#2925](https://github.com/TraceMachina/nativelink/pull/2925) | 1.7.7   | Keepalives report what a worker runs; what it no longer runs is queued again.                 |

The "Worker ids do not match" error that ended each round of the loop is gone as well: evicting a worker now skips an action that has already moved to another.

To move to a later release: change the pins at the top of `build-worker.sh`, refresh each patch against the new tag (or delete one, and its line in `PATCHES`, once a release does what it does), run `build-worker.sh` on a Mac, on a Linux worker host and, with `--role hub`, on the hub's host, which runs the tests that cover the patches, and update the version in this file and in the two configs. `config_test` fails until they agree.

## Timeouts

Every value is written out in the config, including the ones that equal NativeLink's default, with its reason next to it. This is the same list in one place, with what a Bazel client is told when a limit is what ends its action.

| Setting                          | Value           | In     | What it bounds                                                                                     | What the client sees                                                                  |
| -------------------------------- | --------------- | ------ | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `worker_timeout_s`               | 30 s            | hub    | Silence from a worker before it is evicted and its actions queued again. Twelve missed keepalives. | Nothing: the action runs elsewhere.                                                   |
| `max_worker_loss_retries`        | 6               | hub    | Times one action may lose its worker. As many as the pool has workers.                             | `FAILED_PRECONDITION`: `The worker running this action was lost 7 times`              |
| `max_job_retries`                | 3               | hub    | Retries of an action after an internal error or a scheduler timeout.                               | `FAILED_PRECONDITION`: `Job cancelled because it attempted to execute too many times` |
| `max_timeout_retries`            | 1               | hub    | A command that timed out on one worker is tried once on another.                                   | `DEADLINE_EXCEEDED`: `Command … timed out after N seconds`                            |
| `no_worker_action_timeout_s`     | 120 s           | hub    | Wait of a queued action while no worker at all is connected.                                       | `FAILED_PRECONDITION`: `Action cannot be scheduled, no worker is connected`           |
| `unsatisfiable_action_timeout_s` | 300 s           | hub    | Wait of a queued action that no connected worker could run even when idle.                         | `FAILED_PRECONDITION`: `Action cannot be scheduled`                                   |
| `max_action_executing_timeout_s` | 2700 s (45 min) | hub    | An action executing with no change of state, on a worker that still reports it.                    | `FAILED_PRECONDITION`: `Job cancelled …`, ending with the timeout that caused it      |
| `executing_sweep_interval_s`     | 30 s            | hub    | How late the limits on executing actions are noticed when no client is attached.                   | Nothing by itself.                                                                    |
| `unacknowledged_kill_timeout_s`  | 60 s            | hub    | Silence from a worker about an action it was told to kill, before the worker is evicted.           | Nothing: its other actions run elsewhere.                                             |
| `dispatch_ack_timeout_s`         | 20 s            | hub    | An action handed to a worker that never acknowledged it is queued again, untried.                  | Nothing.                                                                              |
| `client_action_timeout_s`        | 60 s            | hub    | How long a queued action outlives a client that has gone.                                          | Nothing: the client has gone.                                                         |
| `execute_stream_heartbeat_s`     | 30 s            | hub    | Quiet on an execution stream before the hub repeats the action's state.                            | Nothing.                                                                              |
| `max_download_timeout_s`         | 600 s           | worker | Fetching one action's inputs.                                                                      | `FAILED_PRECONDITION`: `Job cancelled …`, ending with the timeout that caused it      |
| `max_action_timeout_s`           | 1200 s          | worker | One command. Applies to an action with no timeout of its own, and caps one that has a longer one.  | `DEADLINE_EXCEEDED`, after `max_timeout_retries`                                      |
| `max_upload_timeout_s`           | 600 s           | worker | Uploading one action's outputs.                                                                    | `FAILED_PRECONDITION`: `Job cancelled …`, ending with the timeout that caused it      |
| `http2_keepalive_interval_s`     | 10 s            | worker | How often the worker pings the hub on its connection.                                              | Nothing.                                                                              |
| `http2_keepalive_timeout_s`      | 20 s            | worker | An unanswered ping before the worker reconnects. With the interval, 30 s: `worker_timeout_s`.      | Nothing.                                                                              |

Three of these depend on each other, and `config_test` checks the arithmetic: the hub's 45-minute ceiling is the sum of the worker's three limits plus five minutes, so that a worker's own, more specific error arrives first; a worker notices a dead connection (10 s + 20 s) no later than the hub evicts a silent worker (30 s); and `worker_timeout_s` is several keepalive intervals (a keepalive goes out every 2.5 s).

The worker's three limits are NativeLink's defaults, kept because nothing has measured this pool's longest fetch, command or upload. Lowering them, and the ceiling with them, needs that measurement first.

## What bounds a stuck action

A remote action that cannot make progress ends in one of the ways below. The times and the CI column are measured, with Bazel 9.1.0 against a 1.8.6 hub and two workers running these configs on one machine, unless a time is marked as derived from the settings. The developer column is measured for every worker freezing and for a hub that stops answering **before** a build; the during-a-build developer bound is derived from the watchdog's interval and probe timeout (it has not been measured against a frozen hub). The other rows follow from the rule Bazel applies, which is to run an action on the Mac when its remote execution failed for any reason but the action's own timeout.

| What happens                                                 | What ends it                                                                                                                                            | How long                                       | CI step (`ci-remote`)                                                                                                                                    | Developer's Mac (`dev-remote`)                                                                                                                                                                                   |
| ------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| The worker running an action freezes or dies                 | `worker_timeout_s`; the action runs on another worker                                                                                                   | Evicted after 30 s                             | Succeeds                                                                                                                                                 | Succeeds                                                                                                                                                                                                         |
| Every worker freezes or is gone                              | `worker_timeout_s`, then `no_worker_action_timeout_s`                                                                                                   | Failed 151 s after the freeze                  | Fails: `Action cannot be scheduled, no worker is connected to the scheduler.`                                                                            | The action runs on the Mac                                                                                                                                                                                       |
| Workers are evicted again and again (the loop of 2026-11-18) | `max_worker_loss_retries`                                                                                                                               | 3 to 6 min (derived: six losses of 30–60 s)    | Fails: `The worker running this action was lost 7 times`                                                                                                 | The action runs on the Mac                                                                                                                                                                                       |
| A command hangs on a worker                                  | The test's own timeout, or `max_action_timeout_s`; then one more worker                                                                                 | At most 2 × 20 min (derived)                   | Fails with the timeout                                                                                                                                   | Fails with the timeout                                                                                                                                                                                           |
| A worker reports an action as running and never finishes it  | `max_action_executing_timeout_s`                                                                                                                        | 45 min per attempt (derived)                   | Fails after the retries                                                                                                                                  | The action runs on the Mac                                                                                                                                                                                       |
| The hub's process dies                                       | The connection breaks                                                                                                                                   | 4 s                                            | Fails: `(Exit 34): UNAVAILABLE: io exception`                                                                                                            | The action runs on the Mac                                                                                                                                                                                       |
| The hub stops answering **before** a build starts            | CI: the status probe. Bazel: its first call, six tries of 60 s each                                                                                     | CI probe: 10 s. Bazel: 366 s and 369 s         | The step builds on its agent, with a warning. Without the probe: `Failed to query remote execution capabilities: DEADLINE_EXCEEDED`                      | The build runs on the Mac                                                                                                                                                                                        |
| The hub stops answering **during** a build                   | Client status watchdog (same `/status` request as the CI start-of-step probe), repeated while bazel runs; stops the command after 8 bad checks in a row | ~2 to 3.3 min (derived: 8 bad checks in a row) | The command is stopped. An error annotation names the executor and the probe result. `timeout_in_minutes` is the last resort if the watchdog cannot run. | The in-flight bazel command is stopped and a line is written to that command's terminal (or `$TMPDIR/dev-remote-watchdog-<pid>.reason` if it has no tty). Re-run with `--config=dev-local` to finish on the Mac. |

A scheduler that still answers its status page but stops completing actions is a different row: NativeLink 1.8.6 fails those actions itself (`no_worker_action_timeout_s`, `max_worker_loss_retries`, `max_action_executing_timeout_s`), and the developer fallback or the CI annotation then applies. The last row is only the hub that goes silent.

Bazel 9.1 puts no deadline on an execution call: `--remote_timeout` covers cache calls only, and the flag that used to cover execution (`--experimental_remote_execution_keepalive`) is a no-op since 9.0. `--grpc_keepalive_time` **is** on by default in 9.1, at 60 s (pings after that much quiet, while a call is pending; 0 disables them). `dev-remote`, `ci-remote` and `ci-remote-linux` set `--grpc_keepalive_time=30s --grpc_keepalive_timeout=20s` so those values live in the rc files, and so a future Bazel that defaults them to 0 still pings. They are not the bound for a stopped hub. Measured with keepalive on at the 60 s default: with the hub's process stopped in the middle of an action, the build was still waiting, with no error, 17 minutes later. When the hub was allowed to run again the build carried on and finished by itself, which is also what a restart of the hub does to builds that are waiting on it. The 30 s / 20 s values were not re-measured; the 60 s measurement is why the bound is the status watchdog instead.

So, for a hub that stops answering:

- **A CI step that starts** asks the hub for its status page first (`.buildkite/steps/remote-execution.sh`) and builds on its agent if there is no answer within 10 s (curl exit 28) or the answer is 503. A TCP connection is not enough: the kernel accepts one for a process that has stopped. Any other probe failure (reset, TLS, no route) is treated as "no status page" and the step still executes remotely.
- **A CI step that is running** is bounded by the status watchdog in `ci-env.sh`: every 15 s it repeats that same request for every bazel client the step started, including ones repobin and checkleft launch themselves. If the hub accepted a connection and then does not answer (or answers 503) on **8 checks in a row** (see "How long the watchdog waits"), it stops those bazel clients and annotates the build with the executor's host, port and probe result. Every bad check is logged to the step's output with its count. An unreachable hub is not treated as stuck. `timeout_in_minutes` on every remote-enabled step (`pipeline.yml` and the integrity pipeline) is the last resort if the watchdog cannot run (no curl); a Buildkite SIGTERM then names the executor immediately and runs a short (3 s) probe to amend the annotation.
- **A developer's build that is running** is bounded by `dev-remote`'s workspace-status command (`tools/remote-execution/dev-remote-workspace-status.sh`): the same status request, against the endpoint in `~/.bazelrc`. If the hub accepted a connection and then does not answer (or answers 503) on 8 checks in a row, it writes why to the in-flight bazel command's terminal and stops that command. Every bad check, with its count, is appended to `$TMPDIR/dev-remote-watchdog.log`, so a near-miss is visible afterwards. `--config=dev-local` restores the default stamp script, so the watchdog does not run. An unreachable hub is not treated as stuck. Re-run with `--config=dev-local` to finish on the Mac.
- The hub's health check reports it within a minute (next section), and restarting the hub releases every waiting build.

### How long the watchdog waits

The watchdog does not stop a build on one bad check. It stops it after `N = 8` consecutive bad checks (`503` or `timeout`, counted together); any other answer, including a good `200` or an unreachable hub, resets the count. `N` is `REMOTE_STATUS_STUCK_AFTER_DEFAULT` in `remote-status.sh`, shared by the developer watchdog (override: `DEV_REMOTE_WATCHDOG_CONSECUTIVE_BAD`) and the CI watchdog (`CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD`).

- **What a brief stall looks like.** The hub's 503 comes from one indicator: the CAS filesystem store's own `stat` probe, which gives up after a hard-coded 2 s, so one slow `stat` is enough. On 2026-11-27, 4 probe timeouts between 22:22:32 and 22:23:41 UTC stopped 3 developer builds on the old stop-on-first-bad-check rule. The hub's once-a-minute health check saw no 503 on either side, so the stall lasted seconds; the longest span was under about 70 s. Those 4 are the only probe timeouts in the hub's journal from 2026-11-20 onward.
- **Tolerance.** At a 15 s interval, 70 s of continuous 503s is at most 5 bad checks in a row, so 8 rides it out with margin: a stall of up to 7 bad checks, about 105 s of 503s, never stops a build.
- **Worst case for a hub that stays stuck.** A stopped hub process (the row above) answers every check with `timeout`, each a 15 s wait plus the 10 s probe, so the 8th bad check is about 8 × 25 s = 200 s (3.3 min) after the hub stalled. A hub that keeps answering 503 returns promptly, so about 8 × 17 s = 2.3 min. Both are within the few minutes the build would otherwise lose, against the 17 minutes or more measured with no watchdog.
- The one-shot start-of-step probe in CI and the developer's before-a-build check are unchanged: they decide once, before any work is in flight, and a hub that is not answering then is still not used.

A per-RPC deadline in the client would fail the Execute call itself, so a developer build could fall back instead of stopping. That needs Bazel 9.3 or later (`--remote_grpc_service_config`). The status watchdog is the bound on 9.1.

## Health check

`hub-health.sh` runs on the hub every minute and prints one line:

```
healthy: 6/6 workers, 18 of 66 slots busy, 0 queued; last 600s: 0 forced evictions, 0 operation timeouts, 0 lost results, 412 log lines
UNHEALTHY: no worker is connected; 4 workers were evicted for being silent or for not acknowledging a kill in the last 600s (limit 2) | 0/6 workers, …
```

It exits non-zero while the hub is unhealthy, so the unit's state is the signal:

```sh
systemctl --user is-failed nativelink-macos-hub-health.service   # "failed" while unhealthy
journalctl --user --unit nativelink-macos-hub-health.service --since -1h --output cat
```

Because `nativelink-macos-hub-health.service` runs as a `bduff` user unit while reading the system hub unit's journal with `journalctl --quiet --unit nativelink-macos-hub.service`, `journald` must keep its default `SplitMode=uid` (so entries from `User=bduff` land in a journal `bduff` can read) or `bduff` must be in the `systemd-journal` group; otherwise `--quiet` hides permission errors as an idle window.

What makes it unhealthy, and why each limit is where it is:

| Condition                                                                     | Limit           | Reason                                                                                                                                                                                                                                                                                                                                      |
| ----------------------------------------------------------------------------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| The status page does not answer in 10 s, or answers 503                       | —               | For the filesystem CAS store, the hub's own `stat` probe gives up after 2 s (hard-coded in NativeLink, `nativelink-store/src/filesystem_store.rs:2377`) and the status page answers 503. The outer per-indicator limit is 5 s (`health_server.rs:40`), which that probe does not reach. No answer in 10 s is a process that is not serving. |
| Fewer workers connected than `NL_HEALTH_EXPECTED_WORKERS`                     | the pool's size | A worker that lost its connection and cannot end an action it was running does not come back by itself (see "Recovery").                                                                                                                                                                                                                    |
| Workers evicted for silence or for ignoring a kill, in the last 10 min        | more than 2     | One is a Mac that rebooted. On 2026-11-18 it was 41 closed worker connections in the hour. A worker that disconnects by itself (a restart) is not counted.                                                                                                                                                                                  |
| Actions the scheduler timed out, in the last 10 min                           | more than 4     | In 1.8.6 a live worker's action is not timed out, so each of these is an action whose worker went silent or that hit a ceiling.                                                                                                                                                                                                             |
| Actions queued again because their worker no longer had them, in 10 min       | more than 4     | A result lost between a worker and the hub. One is a blip; a run of them is a connection that keeps breaking.                                                                                                                                                                                                                               |
| The oldest queued action is older than 5 min **and** a worker has a free slot | 300 s           | Work, capacity, and nothing moving. With every slot busy a long queue is only a busy pool, and is not reported.                                                                                                                                                                                                                             |

These limits have not yet run against a day of real traffic on 1.8.6. Expect to adjust them in the first week; they are variables in `hub.env` (the script's header lists them), and a changed default belongs in the script with its reason.

**The alert.** Set `NL_HEALTH_ALERT_CMD` in `hub.env` to whatever should tell a person. It runs when the hub becomes unhealthy, again when it recovers, and when the check cannot run, with the line in `NL_HEALTH_LINE` and `unhealthy`, `healthy` or `error` in `NL_HEALTH_STATE`. A repeating configuration error (a missing `NL_HUB_ADMIN_LISTEN`, a bad argument) alerts once: the check keeps its last state in `NL_HEALTH_STATE_FILE` before it validates anything. Until the command is set, the signal is the unit's state and nothing pushes it anywhere.

**In CI.** A step that fails because the executor failed says so on its build page: `ci-env.sh` recognises the errors in the table above in the log of a failed bazel command and adds an annotation that names the reason and points here. A step that could not use the executor at all, and built on its agent, has a warning annotation.

The check only reports. It does not restart the hub, and nothing else does on a schedule or on a failed check: a scheduler that is running and not scheduling is a fault to look at, and a restart destroys what there is to look at.

## Hub metrics

Two exporters run on the hub so Prometheus can see what NativeLink's own metrics omit: whether the hub has restarted (including an OOM kill), each worker's slots, and how the CAS is filling and evicting.

**Restarts.** The hub is a system unit (`User=bduff`), so `systemctl show` reads it without `--user`. `hub-textfile-metrics.sh` runs as root every 30 s (`nativelink-hub-textfile.timer`) and writes `/var/lib/prometheus/node-exporter/nativelink_hub.prom` atomically. The figures are systemd's `NRestarts`, the current main process start time, whether the unit is active, the last `Result` (so an `oom-kill` is visible), and the last main-process exit status. The script only reads the unit; it never restarts it.

**Slots and CAS.** `worker-slots-exporter.py` listens on `127.0.0.1:9108/metrics`. On each scrape it asks the hub's admin API (loopback, `NL_HUB_ADMIN_LISTEN`) for each worker's slots and running actions. NativeLink 1.8.6 has no eviction counter, so inserts and evictions are counted from inotify: a rename into `cas-content/d2` is an insert, a rename into `cas-tmp/d2` is an eviction, and the size is in the file name. A walk of `cas-content/d2` every 15 minutes, at nice 19 and idle I/O priority, gives the entry count and content bytes; between walks those are kept current from the counters. The CAS cap it exports (`nativelink_hub_cache_cap_bytes`) is `NL_HUB_CAS_MAX_BYTES` from the _running_ hub process, so an edit of `hub.env` is not reported until the hub itself is restarted. The exporter only reads; it never changes the hub.

Neither exporter binds anything but loopback. Copying the files onto the host is in "Installing or updating hub files"; identical content is a no-op and does not restart the hub.

## Recovery

Start from the health line. Its problems map to these.

**`N of M workers are connected`.** On the missing Mac:

```sh
launchctl print gui/$(id -u)/com.brianduff.nativelink | grep -E 'state =|pid =|last exit'
tail -n 50 ~/Library/Logs/nativelink/nativelink.log
```

A worker that lost its connection kills what it was running before it reconnects, and waits for those actions to end without a limit. If one cannot be killed, the worker never returns: the log ends with `Worker disconnected from scheduler, reconnecting` and no later `Worker registered with scheduler`. Restart that worker. It costs nothing the hub has not already queued again:

```sh
launchctl kickstart -k gui/$(id -u)/com.brianduff.nativelink
```

**`workers were evicted for being silent …`, `the scheduler timed out … actions`.** Find which workers; the worker id starts with the Mac's name:

```sh
journalctl --unit nativelink-macos-hub.service --since -15min --output cat |
  grep -E 'Worker timed out|did not acknowledge a kill|Timing out operation'
```

If it is one Mac, the fault is on it (memory pressure, a disk, a hung action): restart its worker, or take it out of the pool with `launchctl bootout gui/$(id -u)/com.brianduff.nativelink` until it is understood. If it is every Mac, look at what they share: the hub's host (`uptime`, the disk under `NL_HUB_DATA`) and the network between them.

**`the hub did not answer a status request`, `reports a failed or stalled store`, `actions are queued … while N slots are free`.** The hub itself. First keep what happened, then restart it:

```sh
set -a
. ~/nativelink-macos-hub/hub.env
set +a
journalctl --unit nativelink-macos-hub.service --since -1h --output cat > ~/hub-$(date +%Y%m%dT%H%M).log
curl -s --connect-timeout 10 --max-time 10 http://$NL_HUB_ADMIN_LISTEN/admin/scheduler/MAIN_SCHEDULER/workers || true
curl -s --connect-timeout 10 --max-time 10 http://$NL_HUB_ADMIN_LISTEN/admin/scheduler/MAIN_SCHEDULER/demand || true
sudo systemctl restart nativelink-macos-hub.service
```

A restart ends every action in flight. The queue is in memory and is lost; the workers kill what they were running and reconnect within half a minute. Each CI step that was executing remotely fails with `UNAVAILABLE` and is annotated as an executor failure; retry those steps. A developer's build falls back to the Mac for the actions that were in flight. Builds that were waiting on a hub that had stopped answering are released the same way.

A hub that needed a restart to schedule again has a bug. With the saved log, open an issue upstream; that the restart worked is not the end of it.

**Taking the pool out of use** needs no merge. Remove `CI_BAZEL_REMOTE_EXECUTOR` from an agent's `environment` hook and that agent builds on itself from its next job; comment the `dev-remote-endpoint` lines out of a Mac's `~/.bazelrc`.

## Installing or updating hub files

The files in `nativelink/` are copied onto the hub by hand. `install` always writes, which changes mtime and can make systemd treat a unit as changed; a re-run whose bytes already match the host must leave the host alone. `copy_if_changed` does that: identical content is a no-op.

Do not restart the hub as part of a copy. A changed `hub.json5` is picked up only at the next hub start, which is a person following "Recovery". Restarting the slots exporter (not the hub) is needed only when that script's bytes changed.

```sh
copy_if_changed() {
  src=$1 dest=$2 mode=$3
  if [[ -e "$dest" ]] && cmp -s "$src" "$dest"; then
    return 0
  fi
  install -m "$mode" "$src" "$dest"
}

sudo_copy_if_changed() {
  src=$1 dest=$2 mode=$3
  if sudo test -e "$dest" && sudo cmp -s "$src" "$dest"; then
    return 0
  fi
  sudo install -m "$mode" "$src" "$dest"
}

NL=~/nativelink-macos-hub
USER_UNIT=~/.config/systemd/user
REPO=<checkout>/tools/remote-execution/nativelink

copy_if_changed "$REPO/hub.json5" "$NL/hub.json5" 644
copy_if_changed "$REPO/hub-health.sh" "$NL/hub-health.sh" 755
copy_if_changed "$REPO/hub-textfile-metrics.sh" "$NL/hub-textfile-metrics.sh" 755
copy_if_changed "$REPO/nativelink-macos-hub-health.service" "$USER_UNIT/nativelink-macos-hub-health.service" 644
copy_if_changed "$REPO/nativelink-macos-hub-health.timer" "$USER_UNIT/nativelink-macos-hub-health.timer" 644
copy_if_changed "$REPO/nativelink-worker-slots-exporter.service" "$USER_UNIT/nativelink-worker-slots-exporter.service" 644
sudo_copy_if_changed "$REPO/nativelink-macos-hub.service" /etc/systemd/system/nativelink-macos-hub.service 644
sudo_copy_if_changed "$REPO/nativelink-hub-textfile.service" /etc/systemd/system/nativelink-hub-textfile.service 644
sudo_copy_if_changed "$REPO/nativelink-hub-textfile.timer" /etc/systemd/system/nativelink-hub-textfile.timer 644

if [[ -e "$NL/worker-slots-exporter.py" ]] && cmp -s "$REPO/worker-slots-exporter.py" "$NL/worker-slots-exporter.py"; then
  :
else
  install -m 755 "$REPO/worker-slots-exporter.py" "$NL/worker-slots-exporter.py"
  systemctl --user restart nativelink-worker-slots-exporter.service
fi

systemctl --user daemon-reload
sudo systemctl daemon-reload
systemctl --user enable --now nativelink-macos-hub-health.timer
systemctl --user enable --now nativelink-worker-slots-exporter.service
sudo systemctl enable --now nativelink-hub-textfile.timer
```

`hub.env` stays on the host: it names listen addresses. Do not copy it from the repository.

On a Mac worker the same helper applies to files that are installed unmodified (`worker-macos.json5`, `action-env.sh`). The launchd plist is edited after the first copy (placeholders for this Mac), so a later `cmp` against the repository file will differ; do not overwrite it from the repository.

### Updating the hub's binary

The hub runs `~/nativelink-macos-hub/nativelink`, built from source with its patch ("Version and patches"). Build it on the hub's host, in a directory of its own, and never in the hub's data directory:

```sh
BUILD=<a directory with room for a bazel build>
nice -n 15 ionice -c3 $REPO/build-worker.sh --role hub --src $BUILD/src --out $BUILD/out
```

It fetches the pinned commit, applies the patch, runs the ByteStream server's tests, and prints the binary's version and sha256. A reused `--src` is refused unless it is that commit plus exactly that patch. The install is a restart, so it is a deliberate step, not part of a copy: do it when no CI build is running (`bk build list --state running`), because the restart ends the cache traffic and every action in flight ("Recovery" says what the clients see).

```sh
cd ~/nativelink-macos-hub
backup=nativelink.bak-$(date +%Y%m%d-%H%M)
cp -p nativelink "$backup"
install -m 755 $BUILD/out/nativelink nativelink.new
mv nativelink.new nativelink    # a rename: the running process keeps its old file
./nativelink --version          # nativelink 1.8.6+compressed-restart
sudo systemctl restart nativelink-macos-hub.service
sleep 60 && ./hub-health.sh --journal-unit nativelink-macos-hub.service
```

The health check must report a healthy hub with every worker connected. If it does not, put the backup back and restart once more: `mv "$backup" nativelink && sudo systemctl restart nativelink-macos-hub.service`. Do not change `hub.json5`, `hub.env` or the stores in the same step. The upstream release is in `~/nativelink-macos-hub/nativelink-1.8.6-x86_64-unknown-linux-musl.tar.gz`; `tar xzf` it to get the unpatched binary back.

### The hub unit and the host's memory

The hub is a system unit with `OOMScoreAdjust=-900`, `MemoryLow=4G` and `MemorySwapMax=0`. Under the user manager it ran at `oom_score_adj` 200, and the kernel's OOM killer took it first when the Linux worker's compiles and links filled RAM. The Linux worker on the same host (`headcrash-worker/`) is capped inside its own cgroup (`MemoryHigh=16G`, `MemoryMax=18G`, `OOMScoreAdjust=300`), so a burst of compiles is killed there, not the hub.

The old user unit stays at `~/.config/systemd/user/nativelink-macos-hub.service`, disabled, for rollback only: disable the system unit (`sudo systemctl disable --now nativelink-macos-hub.service`), then enable the user one as `bduff`; the system unit's header comment has the exact commands. Never enable both.

The Linux worker's files on `headcrash` install the same way (a changed file is a no-op when identical; a restart is a separate, deliberate step: see the Linux runbook, "Restarting"):

```sh
W=$REPO/headcrash-worker
copy_if_changed "$W/worker.json5" ~/nativelink/worker.json5 644
copy_if_changed "$W/nativelink-worker.service" ~/.config/systemd/user/nativelink-worker.service 644
mkdir -p ~/.config/systemd/user/nativelink-worker.service.d
copy_if_changed "$W/nativelink-worker.service.d/60-v1.8.conf" ~/.config/systemd/user/nativelink-worker.service.d/60-v1.8.conf 644
copy_if_changed "$W/nativelink-worker.service.d/memory.conf" ~/.config/systemd/user/nativelink-worker.service.d/memory.conf 644
systemctl --user daemon-reload
```

Swap on `headcrash` is two files, both in `/etc/fstab` with the default priority: `/swap.img` (8G) and `/swap2.img` (24G), root-owned, mode 0600, 32G in all. The hub never uses it (`MemorySwapMax=0`); it is there for the worker's actions and the rest of the host. The repository provisions no host settings, so this is the record. To recreate it on a new host (idempotent: each file is made only when missing, an existing non-swap file fails with a repair instruction rather than being registered, and `/etc/fstab` is updated only after `swapon` succeeds):

```sh
add_swap() {
  local file=$1 size=$2
  if sudo test -e "$file"; then
    if [ "$(sudo blkid -p -o value -s TYPE "$file" 2>/dev/null)" != swap ]; then
      echo "error: $file exists but is not a valid swap file; inspect and remove it (sudo rm -- '$file') before re-running" >&2
      return 1
    fi
  else
    sudo fallocate -l "$size" "$file" || return 1
    sudo chmod 600 "$file" || return 1
    sudo mkswap "$file" || return 1
  fi
  swapon --show=NAME --noheadings | grep -qxF "$file" || sudo swapon "$file" || return 1
  awk -v f="$file" '$1 == f { found = 1 } END { exit !found }' /etc/fstab ||
    echo "$file none swap sw 0 0" | sudo tee -a /etc/fstab >/dev/null || return 1
}
add_swap /swap.img 8G && add_swap /swap2.img 24G
```

## Cutover

From NativeLink 1.6.6 with the hub on `zakalwe` to 1.8.6 with the hub on the Linux host. Everything moves in one window, because the hub and the workers have to change together. The stores start empty: 1.7.0 changed the on-disk layout of a store one way, and starting clean on the new host avoids converting anything. The first builds afterwards are cold.

**1. Build the worker binary**, on any of the Macs, ahead of time:

```sh
tools/remote-execution/nativelink/build-worker.sh --src ~/nativelink-src --out ~/nativelink-1.8.6
```

It fetches the pinned commit, applies the patches, runs the tests that cover them and prints the binary's sha256. Copy `~/nativelink-1.8.6/nativelink` to every Mac as `~/nativelink/nativelink-1.8.6` and check the digest there. Leave the running `~/nativelink/nativelink` alone for now.

**2. Set up the hub**, on the Linux host, ahead of time. It has no clients until step 5.

```sh
mkdir -p ~/nativelink-macos-hub && cd ~/nativelink-macos-hub
# the release tarball named under "Version and patches"; check its sha256 before unpacking
tar xzf nativelink-1.8.6-x86_64-unknown-linux-musl.tar.gz nativelink   # since 2026-11-23 the hub runs its patched build instead ("Updating the hub's binary")
# copy_if_changed: identical content is a no-op (see "Installing or updating hub files")
REPO=<checkout>/tools/remote-execution/nativelink
copy_if_changed() {
  src=$1 dest=$2 mode=$3
  if [[ -e "$dest" ]] && cmp -s "$src" "$dest"; then
    return 0
  fi
  install -m "$mode" "$src" "$dest"
}
copy_if_changed "$REPO/hub.json5" hub.json5 644
copy_if_changed "$REPO/hub-health.sh" hub-health.sh 755
copy_if_changed "$REPO/hub-textfile-metrics.sh" hub-textfile-metrics.sh 755
copy_if_changed "$REPO/worker-slots-exporter.py" worker-slots-exporter.py 755
copy_if_changed "$REPO/nativelink-macos-hub-health.service" ~/.config/systemd/user/nativelink-macos-hub-health.service 644
copy_if_changed "$REPO/nativelink-macos-hub-health.timer" ~/.config/systemd/user/nativelink-macos-hub-health.timer 644
copy_if_changed "$REPO/nativelink-worker-slots-exporter.service" ~/.config/systemd/user/nativelink-worker-slots-exporter.service 644
```

The hub itself and the restart-state exporter are system units (the exporter writes to node_exporter's textfile directory, which is root-owned; the hub runs as `bduff` with an OOM score and memory protection a user manager cannot give it). The health timer and the slots exporter stay user units:

```sh
sudo_copy_if_changed() {
  src=$1 dest=$2 mode=$3
  if sudo test -e "$dest" && sudo cmp -s "$src" "$dest"; then
    return 0
  fi
  sudo install -m "$mode" "$src" "$dest"
}
sudo_copy_if_changed "$REPO/nativelink-macos-hub.service" /etc/systemd/system/nativelink-macos-hub.service 644
sudo_copy_if_changed "$REPO/nativelink-hub-textfile.service" /etc/systemd/system/nativelink-hub-textfile.service 644
sudo_copy_if_changed "$REPO/nativelink-hub-textfile.timer" /etc/systemd/system/nativelink-hub-textfile.timer 644
```

Write `~/nativelink-macos-hub/hub.env`. The addresses are this host's own and stay on the host:

```sh
NL_HUB_DATA=<a directory on the internal disk>
NL_HUB_CAS_MAX_BYTES=<95% of that filesystem, minus 5 GB, in bytes>
NL_HUB_CLIENT_LISTEN=<address>:50051
NL_HUB_WORKER_LISTEN=<address>:50061
NL_HUB_ADMIN_LISTEN=127.0.0.1:50071
NL_HEALTH_EXPECTED_WORKERS=<the number of workers, macOS and Linux>
```

Then:

```sh
loginctl enable-linger "$USER"
systemctl --user daemon-reload
sudo systemctl daemon-reload
sudo systemctl enable --now nativelink-macos-hub.service
systemctl --user enable --now nativelink-worker-slots-exporter.service
sudo systemctl enable --now nativelink-hub-textfile.timer
journalctl --unit nativelink-macos-hub.service --output cat | head     # "Ready, listening on …" three times
curl -s --connect-timeout 10 --max-time 10 -o /dev/null -w '%{http_code}\n' http://<address>:50051/status         # 200
```

A variable missing from `hub.env` fails the start with an error that names it.

**3. Stop new work.** Pause the Mac agents (`bk agent pause <agent-id> --timeout-in-minutes 60`, as in the Linux runbook's "Safe maintenance procedure") and wait for running jobs to end. Developers' builds that are in flight fall back to their Macs when the old hub goes.

**4. Switch each worker.** On each Mac:

```sh
launchctl bootout gui/$(id -u)/com.brianduff.nativelink
cd ~/nativelink
mv nativelink nativelink-1.6.6 && mv nativelink-1.8.6 nativelink
./nativelink --version                                    # nativelink 1.8.6+macos-clonefile.stale-entry
REPO=<checkout>/tools/remote-execution/nativelink
copy_if_changed() {
  src=$1 dest=$2 mode=$3
  if [[ -e "$dest" ]] && cmp -s "$src" "$dest"; then
    return 0
  fi
  install -m "$mode" "$src" "$dest"
}
copy_if_changed "$REPO/worker-macos.json5" worker-macos.json5 644
copy_if_changed "$REPO/action-env.sh" action-env.sh 755
cp ~/Library/LaunchAgents/com.brianduff.nativelink.plist com.brianduff.nativelink.plist.1.6.6
# The plist is edited after this copy (placeholders for this Mac); later updates
# must not overwrite it from the repository.
install -m 644 "$REPO/com.brianduff.nativelink.plist" ~/Library/LaunchAgents/
```

Edit the placeholders in the installed plist: `USER_HOME`, `WORKER_NAME`, `HUB_HOST` (and the ports, if the hub's are not 50051 and 50061), and the slots and store size from the table under "Hosts". `NL_WORKER_DATA` is a new directory, `~/nativelink/v1.8`, so the 1.6.6 store in `~/nativelink/data` is untouched and a rollback finds it as it left it. Then:

```sh
plutil -lint ~/Library/LaunchAgents/com.brianduff.nativelink.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.brianduff.nativelink.plist
grep 'Worker registered with scheduler' ~/Library/Logs/nativelink/nativelink.log | tail -1
```

macOS ties "Developer Tools" and "Local Network" (Privacy & Security) to the binary, so the new one has to be allowed again, at the Mac's screen. Until "Local Network" is allowed the worker cannot reach the hub and never registers.

`zakalwe` is the same, with one more decision: its old process was the hub as well, and its job is `rbe.json5`, not a worker config. Booting it out stops the old hub, which is step 5. Bring it back as a worker with the plist from this directory, or leave it out.

**5. Check the pool, then point the clients at it.** On the hub:

```sh
set -a
. ~/nativelink-macos-hub/hub.env
set +a
curl -s --connect-timeout 10 --max-time 10 http://$NL_HUB_ADMIN_LISTEN/admin/scheduler/MAIN_SCHEDULER/workers | grep -o '"id":"[a-z]*' | sort
systemctl --user enable --now nativelink-macos-hub-health.timer
~/nativelink-macos-hub/hub-health.sh --journal-unit nativelink-macos-hub.service      # healthy: 6/6 workers …
```

Change `CI_BAZEL_REMOTE_EXECUTOR` in each Mac agent's `environment` hook, and the three `dev-remote-endpoint` lines in each developer's `~/.bazelrc`, to the new hub's client address.

**6. Prove it before the agents come back.** From a checkout of `main` on a Mac that points at the new hub:

```sh
bazel test //tools/test-sandbox/... --nocache_test_results     # the gate in tools/test-sandbox/README.md
bazel build //tools/boss/engine/core:engine                    # Rust compiles on the workers
```

The summary must report the actions as `remote`, and no worker's log may contain `Library not loaded` or `clonefile unavailable for executables`. Then run `bazel build //... && bazel test //...` once to fill the cache, so that the first CI steps do not start cold against a 60-minute limit, and resume the agents (`bk agent resume <agent-id>`).

A checkout that built against the old hub may log `Lost inputs no longer available remotely` on its first build: it remembers outputs that only the old cache had. Bazel starts the build again by itself and re-runs those actions.

**Rolling back.** The old state is intact on every Mac. Boot the worker out, put `nativelink-1.6.6` and the saved plist back, bootstrap it, and on `zakalwe` the old hub starts with it. Point the clients back. Do not start a 1.6.6 binary on a `v1.8` store or the 1.8.6 hub's stores: it cannot read them.

## Not covered here

- **The Linux workers' configs and units.** They register with this hub and run the Linux build from `build-worker.sh`, but `worker.json5`, the action sandbox and the systemd unit are installed per host and described in `.buildkite/linux-agents-runbook.md`, "Remote execution workers"; no copy is in this directory, so `tests/config_test.sh` does not cover them. The health check counts workers of both operating systems together: it reports a missing worker, and does not say which pool is short.
- **A per-RPC deadline in the Bazel client** (`--remote_grpc_service_config` in Bazel 9.3 or later). That would fail the Execute call itself so a developer build could fall back instead of stopping. The status watchdog is the bound on Bazel 9.1.
