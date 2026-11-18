# Postmortem: remote-execution outage on 2026-11-18

- **Incident date:** 2026-11-18. All times are America/Chicago (CST, UTC−06:00).
- **Status:** service restored at 15:58:07 by rebooting the scheduler host. The trigger is not identified. Two after-effects were visible in CI between 16:07 and 16:23; the evidence for this document was last read at 16:33.
- **Format:** blameless. Hosts and systems are named; people are not.
- **Provenance:** an incident record supplied by the infrastructure owner (kernel, scheduler and host logs, a shutdown spindump, shell diagnosis), which this investigation could not re-read; plus evidence checked here: Buildkite build records and job logs, GitHub pull-request and release metadata, the NativeLink fork's source at the deployed commit, and this repository's Bazel and CI configuration at `22edd856`.
- **Related:** [NativeLink remote execution: findings and a small macOS worker pool](nativelink-macos-remote-execution.md), which describes the fork and the deployment this incident happened to.

For 58 minutes on the afternoon of 2026-11-18 no Bazel client could get a remote action executed. The APFS volume that holds the single NativeLink scheduler's stores stopped committing transactions, and the scheduler turned that into a permanent wedge. Clients hung instead of failing, nobody was alerted, and the merge queue landed nothing for 70 minutes.

## Verdict

- **What failed.** From 15:00:08 to 15:58:07 remote execution was unavailable to every client: CI on six Macs, and developer and agent builds on the laptop.
- **Trigger: not known.** APFS on the scheduler's external SSD stopped committing transactions at 15:00:08 with no device, link or memory error logged. A filesystem-level stall is the most likely explanation, a hardware or link fault is less likely but not excluded, and heavy file churn is a possible, unproven contributor. No stackshot was taken during the stall, so the thread that held the transaction path cannot be named.
- **Amplifier: known, and already fixed upstream.** NativeLink 1.6.6 turns slow workers into an eviction loop that leaves actions `Executing` forever. Six upstream fixes released between v1.7.3 and v1.7.7 describe this behaviour. The deployment was 16 releases behind.
- **The trouble was wider than the stall.** CI logs show remote actions failing from 14:13, 47 minutes before the stall, mostly with a different signature: one worker could not stage one executable. That signature also occurred in the morning and came back at 16:07, after the reboot. It accounts for most of the afternoon's failed merge-queue builds. The reboot ended the wedge; it did not end the day's remote-execution failures.
- **Detection failed at every layer.** Nothing alerted. A hung remote action has no client-side bound, CI's health probe only checks that the port accepts a connection, and an agent read a hung build as a slow one for more than four hours.
- **Recovery.** Rebooting the host worked. Restarting the scheduler and force-unmounting the volume did not.

## How the evidence is labelled

Claims below carry one of three labels, either in a table's source column or in the sentence itself.

| Label     | Meaning                                                                                                                                    |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Record    | Stated in the supplied incident record. Not re-verified: this investigation had no access to the hosts, their logs or the spindump.        |
| Verified  | Checked in this investigation against a named source: a CI job log, the Buildkite or GitHub API, the fork's source, or this repo's config. |
| Inference | Reasoning from the two above. It could be wrong, and the text says what would confirm it.                                                  |

Verified sources, read on 2026-11-18 between 16:10 and 16:33:

- **Buildkite:** all 367 builds of the mono pipeline created between 00:00 and 16:25 (27004 to 27370), and the full log of every failed job in them (132 logs). Job logs carry a timestamp per line.
- **GitHub:** the six upstream NativeLink pull requests named below, upstream issue 3137 and its two pull requests, the upstream release list, the fork comparison, and the merge times of today's mono pull requests.
- **Fork source:** `nativelink-store/src/filesystem_store.rs` and `nativelink-worker/src/running_actions_manager.rs` at commit `3125a4fd`.
- **This repo:** [`.bazelrc`](../../.bazelrc), [`.ci.bazelrc`](../../.ci.bazelrc), [`.buildkite/steps/remote-execution.sh`](../../.buildkite/steps/remote-execution.sh) and `.buildkite/pipeline.yml` at `22edd856`; `bazel help` from Bazel 9.1.0 for flag defaults.

## Setup at the time

| Component         | State                                                                                                                                                                                                                                                               | Source   |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| Scheduler host    | `zakalwe`, a Mac mini with 16 GB and 10 cores, running the NativeLink scheduler, the CAS and one worker (`max_inflight_tasks: 10`) in one process.                                                                                                                  | Record   |
| Store volume      | `/Volumes/ssd`, APFS volume disk5s1 on an external Thunderbolt NVMe enclosure (Express 1M2, firmware 54.17, USB4 40 Gb/s, Samsung controller, SMART Verified). It holds the NativeLink stores, the worker's work directory and the CI Bazel disk cache.             | Record   |
| Also on that host | Four Buildkite agent slots. CI on macOS sets `--disk_cache=/Volumes/ssd/bazel/disk_cache` and `--repository_cache=/Volumes/ssd/bazel/repo_cache`, and Bazel 9.1 puts its repo contents cache under the repository cache by default, so all three are on the volume. | Verified |
| Remote workers    | `anaplian`, `arbitrary`, `determinist`, `rascal`, `skaffen`, all macOS. Each also runs Buildkite agents (verified).                                                                                                                                                 | Record   |
| Linux CI          | Linux agents send their actions to a separate scheduler with its own cache. They were not affected, which is why some builds in the window passed.                                                                                                                  | Verified |
| NativeLink build  | Every host runs `brianduff/nativelink` branch `fix/macos-clonefile-executables` at `3125a4fd`.                                                                                                                                                                      | Record   |
| Fork contents     | That commit is upstream v1.6.6 (published 2026-08-22) plus exactly one commit. Upstream has not merged an equivalent: issue 3137 and pull requests 3138 and 3191 are open. The latest upstream release is v1.8.6, published 2026-11-17: 16 releases newer.          | Verified |
| Clients           | CI steps and developer or agent builds send actions to `grpc://zakalwe:50051`.                                                                                                                                                                                      | Record   |
| Redundancy        | One scheduler. This is a known single point of failure; the owner has deferred redundancy.                                                                                                                                                                          | Record   |

One detail of the record needs a qualifier. It says clients run with `--remote_local_fallback`. In this repo that is true of the developer config (`dev-remote`) only. CI's `ci-remote` config sets `--noremote_local_fallback` and instead decides once per step: if the endpoint does not accept a TCP connection within 5 seconds, the whole step builds on the agent (verified in `.ci.bazelrc` and `remote-execution.sh`). Neither mechanism reacts to an action the scheduler accepted and never finished, which is what this incident produced.

## Impact

| Measure                                 | Value                                                                                                                                                                                                                  | Source           |
| --------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------- |
| Remote execution unavailable            | 58 minutes, 15:00:08 to 15:58:07.                                                                                                                                                                                      | Record           |
| Remote execution degraded               | 10:43 to 11:17, and again from 14:13: actions failing rather than hanging.                                                                                                                                             | Verified         |
| Merge queue                             | No pull request merged between 14:55:14 (#3733) and 16:05:00 (#3714): 70 minutes.                                                                                                                                      | Verified         |
| Merge-queue builds, created 14:00–16:10 | 49 builds for 8 pull requests. 10 passed. 35 failed with an infrastructure signature in at least one job; 6 of those also had an unrelated failure. 4 failed without one: three on unit tests and one on a lost agent. | Verified         |
| All CI builds, created 14:00–16:10      | 69 builds, 50 failed, 46 of them with an infrastructure signature. For comparison, the 13:00 hour had 6 builds and no failures.                                                                                        | Verified         |
| Pull requests still unmerged at 16:33   | Five of the eight: #3247, #3693, #3726, #3730, #3738. #3218 merged at 16:25. #3738 had dropped out of the queue and needed a manual re-queue.                                                                          | Verified, Record |
| Hung CI jobs                            | Three jobs on `zakalwe` sat silent for 64 to 75 minutes. One job on `anaplian` waited 52 minutes on remote actions.                                                                                                    | Verified         |
| Agent time                              | One developer-agent build hung for more than 4.5 hours from 11:05. Other builds hung until killed. The total is not counted.                                                                                           | Record           |
| False follow-up work                    | Automation created "fix failing CI" work for failures that were infrastructure. The number of such items is not counted.                                                                                               | Record           |
| Laptop load, morning                    | Until about 09:27 laptop builds ran locally without saying so, because the scheduler's host name did not resolve.                                                                                                      | Record           |

Not every red build in the window was infrastructure. PR #3247 failed its own commit-message check in four queue builds, and four jobs on Linux agents failed unit tests with no remote-execution error (verified). Any rule that classifies failures has to keep those red.

## Timeline

| Time              | Event                                                                                                                                                                                                                                      | Source   |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------- |
| Morning, to 09:27 | Tailscale is off on the laptop, so `zakalwe` does not resolve (`Unable to resolve host zakalwe`). Laptop builds fall back to local execution silently and load the laptop. Fixed by turning Tailscale on. A separate issue, not the cause. | Record   |
| All day           | NativeLink logs operation timeouts at 6 to 20 per hour, with occasional worker disconnects.                                                                                                                                                | Record   |
| 10:43:18          | First CI job fails with `Already Exists: Action with operation_id … is already running … attempted to execute too many times 4 > 3`.                                                                                                       | Verified |
| 10:44:27–11:17:31 | First episode of `executable-variant copy failed … NotFound` for one executable digest: 16 errors in 10 builds.                                                                                                                            | Verified |
| 11:05             | A developer-agent Bazel server on the laptop stops logging; its last lines are `RemoteExecutionCache … Failed to download …`. The `bazel build` then hangs for more than 4.5 hours at 0% CPU while the agent keeps renewing its wait.      | Record   |
| 14:10:41          | First merge-queue build of the afternoon. Ten queue builds for six pull requests start within four minutes.                                                                                                                                | Verified |
| 14:10:51          | A `checks` job starts a Bazel-backed lint that does not finish; it is cut off by the lint's own 600-second limit at 14:20:51. A second one does the same from 14:11:27.                                                                    | Verified |
| 14:13:18          | A CI agent reports `UNAVAILABLE: Unable to resolve host zakalwe`. A second agent reports it more than a hundred times between 14:16:48 and 14:16:55.                                                                                       | Verified |
| 14:14:29          | On `determinist`, the logs of three CI jobs stop between 14:14:29 and 14:15:19. Buildkite marks the jobs lost (exit status −1) at 14:19:00 and 14:20:00. They are the only lost-agent jobs of the day.                                     | Verified |
| 14:14:53          | `executable-variant copy failed` returns. From 14:21:13 to 14:54:11 it appears 108 times in 28 builds, always naming a worker registration created at 14:19:07.                                                                            | Verified |
| 14:15–15:00       | Very heavy load on the volume: APFS transaction rate about 5 times noon's, flush latency roughly doubled (average 3.7 to 7 ms, maximum 38 ms). fseventsd logs about 17,000 lines in the 14:15 bucket against about 25 normally.            | Record   |
| 14:37:14          | Last CI job on `zakalwe` to run to an ordinary end.                                                                                                                                                                                        | Verified |
| 14:41:18          | A `checks` job on `zakalwe` prints `installing repobin tools into bin/`, its first Bazel invocation, and then nothing for 74 minutes.                                                                                                      | Verified |
| 14:52:14          | Two more jobs on `zakalwe` go silent at the same step: `bazel-build-test` of queue build 27336 (PR #3738, normal runtime about 11 minutes) and `mac-app-build` of queue build 27337 (PR #3714).                                            | Verified |
| 14:55:14          | PR #3733 merges. It is the last merge for 70 minutes.                                                                                                                                                                                      | Verified |
| About 14:59:58    | On `anaplian`, a test with a 60-second timeout is sent for remote execution. It is still shown as running remotely 47 minutes later.                                                                                                       | Verified |
| **15:00:08**      | **APFS on disk5 stops committing.** Last kernel line: `15:00:08.054 (apfs) tx_flush: disk5 xid 2020366 … flush 11568us`. No further `tx_flush` line for disk5 until 15:58:06.                                                              | Record   |
| 15:00 onward      | NativeLink enters an eviction loop. Counts for the 15:00 hour: 120 operation timeouts, 41 worker connections closed, 32 failed cleanups. Actions stay `stage=Executing`. Clients hang at 0% CPU.                                           | Record   |
| 15:06:10          | First `DEADLINE_EXCEEDED … after 59.99s` from a cache transfer. Builds started between 15:00 and 15:31 on other agents each fail after about 6.1 minutes.                                                                                  | Verified |
| About 15:45       | Diagnosis. Network healthy (0% loss, 1 to 4 ms RTT, all workers connected). `ls /Volumes/ssd` blocks for about 20 s and fails with EINTR; creating and removing a file at the volume root takes 5.6 s; `stat` of a single path is instant. | Record   |
| 15:47:36          | NativeLink is restarted. The new process hangs in `std::fs::remove_dir_all` → `openat` on the volume and never listens.                                                                                                                    | Record   |
| 15:47:39          | Clients waiting on remote actions get `UNAVAILABLE: io exception`. The 52-minute job on `anaplian` ends.                                                                                                                                   | Verified |
| 15:49:11          | First CI step logs `remote execution off: the remote executor did not accept a connection within 5s` and builds on its agent. That build passes.                                                                                           | Verified |
| 15:55:35–15:55:58 | `diskutil unmount`: `open()` returns EINTR at 15:55:40, `diskarbitrationd … not responding` at 15:55:51, unmount succeeds at 15:55:58 and the volume remounts in 0.3 s. `ls` still hangs.                                                  | Record   |
| 15:55:39          | Four queue builds start for re-queued pull requests. Their macOS steps log `remote execution off` and build on the agents. Three of the four pass.                                                                                         | Verified |
| 15:55:52          | The three silent jobs on `zakalwe` each print `ERROR: could not acquire lock on repo contents cache`, then `Server terminated abruptly`, and fail.                                                                                         | Verified |
| 15:56:46          | Clean, software-initiated reboot. A shutdown-stall spindump is written at 15:56:48. The host is up at 15:57:07. There is no panic report.                                                                                                  | Record   |
| 15:58:07          | NativeLink is ready, all six workers reconnect and `/Volumes/ssd` is healthy. At remount: `nx_mount: disk5 checkpoint: largest xid 2020381`, about 15 transactions in 56 minutes.                                                          | Record   |
| 16:00:19          | First CI step to execute remotely again; its remote actions succeed.                                                                                                                                                                       | Verified |
| 16:05:00          | PR #3714 merges. Its passing queue build had run entirely on the agents.                                                                                                                                                                   | Verified |
| 16:07:09–16:07:27 | `executable-variant copy failed` again, for the same digest, naming a worker registration created at 15:58:11. Three builds fail.                                                                                                          | Verified |
| 16:11:24–16:22:32 | Three `mac-app-build` jobs on one agent fail with `Lost inputs no longer available remotely`, each after Bazel's five automatic retries of the build.                                                                                      | Verified |

The record also establishes what did **not** happen between 12:00 and 16:00. There is no NVMe timeout or reset, no Thunderbolt or USB4 link-down and no IOStorage error for disk4 or disk5, in a unified log with no dropped messages; at 15:54:04 the controller reported its PCIe link up. Available memory never fell below about 4.5 GB, there is no JetsamEvent, and swap is on the internal disk.

## What the CI logs add to the record

The record was written from the scheduler host's point of view. The CI logs are an independent view from the clients, and they change the picture in four ways.

### 1. Remote actions were failing well before the stall, for a different reason

From 14:14:53 the dominant CI error is not a hang. It is a fast failure:

```
Remote Execution Failure:
Internal: executable-variant copy failed: Os { code: 2, kind: NotFound, message: "No such file or directory" } : Resolving executable variant for 5fdf67f7…0d13-4987 : --- : Job cancelled because it attempted to execute too many times 4 > 3 times for operation_id: …, maybe_worker_id: Some(1f1e38d4-e784-6954-ac34-13e5f0c79dec)
```

Verified facts about it:

- It names the same 4,987-byte executable digest every time: 140 error lines in 42 builds across the day.
- It names only three worker ids. The ids are version-6 UUIDs, which embed their creation time: 10:08:05, 14:19:07 and 15:58:11. The first covers the morning episode, the second the afternoon one, and the third was created four seconds after NativeLink came back from the reboot.
- It occurred in three episodes: 10:44 to 11:17, 14:14 to 14:54, and 16:07. Nothing in between.
- It is the signature in 32 of the 46 infrastructure-failed builds of the afternoon window. The stall itself (`DEADLINE_EXCEEDED`, the silent jobs, the restart) accounts for 11.

In the fork's source the message comes from the step that builds a worker's private executable copy of a CAS blob: `std::fs::copy` from the blob's file to a temporary file returned `ENOENT`, after the store's in-memory index had already confirmed the blob exists. A missing destination directory would return the same error, but it would fail every executable, and only one digest ever appears.

**Inference:** one worker holds an index entry for that blob whose file is missing or unreachable on disk, so every action that needs the executable fails there. A worker that fails instantly is also the first to be free again, so it attracts retries; that would explain how one bad entry failed four attempts in a row. The CI logs do not say which host those worker ids belong to, whether all four attempts landed on it, or why the index and the disk disagree. The scheduler log maps worker ids to hosts and would settle the first question in minutes.

There is one circumstantial lead. `determinist` dropped out at about 14:14: it failed to resolve the scheduler's host name at 14:13:18, and three Buildkite agents on it were lost. The registration named in every afternoon error was created at 14:19:07, which fits that host's worker reconnecting. **Inference:** `determinist` is the first host to check. A second agent host also failed to resolve the name at 14:16, so this is a lead and not an identification.

This failure is not explained by the volume stall: it happened hours earlier and again after the reboot. Whether it is related to the volume at all is unknown.

### 2. Jobs on the scheduler host hung in Bazel start-up, on the volume, from 14:41

The three CI jobs that hung for 64 to 75 minutes ran on `zakalwe` itself. Their logs show they never reached remote execution. Each one printed the step header for its first Bazel invocation and then nothing, not even Bazel's invocation ID, until the volume was force-unmounted:

```
[14:52:14] +++ installing repobin tools into bin/
[15:55:52] INFO: Invocation ID: 06e75cdd-dd50-4cec-904f-7141b430659a
[15:55:52] ERROR: could not acquire lock on repo contents cache
[15:55:54] Server terminated abruptly (error code: 14, error message: 'Socket closed', …)
```

The first of them went silent at 14:41:18, nineteen minutes before the kernel's last commit line. The last CI job on that host to behave normally ended at 14:37:14.

**Inference:** these jobs were waiting on `/Volumes/ssd`, not on the scheduler, and something on the volume was already blocking at 14:41. The logs cannot say whether another process held that lock or the filesystem call itself blocked. Either way the stall has an onset that predates 15:00:08, inside the window the record describes as very heavy load. The Bazel server logs for those three output bases on `zakalwe` would show what the servers were doing.

### 3. During the stall, cache calls were bounded and execution waits were not

Clients on the other agents did not all hang the same way.

- **Cache transfers failed after a bound.** Uploads and downloads failed with `DEADLINE_EXCEEDED: CallOptions deadline exceeded after 59.99s`. Jobs that needed one failed after about 6.1 minutes, which matches Bazel 9.1's defaults of a 60-second remote call timeout and five retries. Nothing in the repo's config overrides either.
- **Waits for an action's result had no bound.** On `anaplian`, `storage_values_test`, a `size = "small"` test with a 60-second timeout, was reported as `2840s remote` at 15:47:18. **Inference:** the client leaves a remote test's timeout to the remote side to enforce, and the remote side was wedged. The job ended only because the scheduler process was killed.

So "every client hung silently" is accurate for a client waiting on an executing action, and that is the common case. It is the execution wait specifically that needs a bound.

### 4. A dead scheduler was survivable; a wedged one was not

CI decides per step whether to execute remotely by opening a TCP connection to the endpoint. For the 47 minutes the scheduler was wedged but still listening (15:00:08 to 15:47:36), that probe passed and every step went remote. Within two minutes of the scheduler process going away, steps logged `remote execution off` and built on their agents. PR #3714's queue build started that way at 15:55:39, before the reboot, ran entirely on its agents and merged at 16:05:00.

The fallback path works. The probe that guards it measures the wrong thing.

## Root cause and contributing factors

### Trigger: the volume stopped committing, cause unknown

What is established (record): APFS committed transaction 2020366 on disk5 at 15:00:08.054 with an unremarkable 11.6 ms flush and then logged no commit for 58 minutes. Operations that needed the volume's metadata blocked or crawled; a `stat` of one path stayed instant. Nothing in a complete kernel log points at the device or the link. A volume-level unmount and remount did not clear it. A reboot did.

What is not established: why. No stackshot or spindump exists from 15:00 to 15:55, so the thread that held the APFS transaction path cannot be identified. The only spindump is from shutdown, after the remount.

| Candidate                                                          | Confidence                | For                                                                                                                                                                                                                                                                                                                                                                  | Against, or missing                                                                                                                                                                |
| ------------------------------------------------------------------ | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| APFS-level stall: filesystem or container state stuck              | Most likely               | Commits stop at one instant with normal latency. No device or link error in a complete log. Reads still completed: in the shutdown spindump fseventsd was progressing through `lstat → apfs_vnop_lookup → buf_biowait`.                                                                                                                                              | Nothing identifies the stuck thread or lock. "No error logged" is absence of evidence.                                                                                             |
| Hardware or link fault: enclosure, controller, Thunderbolt         | Less likely, not excluded | The volume is on an external enclosure. The 15:54:04 link-up report has no matching link-down and is unexplained.                                                                                                                                                                                                                                                    | No NVMe timeout or reset, no link-down, no IOStorage error. SMART is Verified. Reads completed during shutdown.                                                                    |
| Heavy file churn, including fseventsd, pushing APFS into the stall | Possible, unproven        | Transaction rate 5 times noon's and doubled flush latency for the 45 minutes before. fseventsd logged about 17,000 lines in one bucket against 25. `/Volumes/ssd/.fseventsd` holds about 219,390 files and 12 GB, all since Nov 14. fseventsd already had a disk-writes diagnostic on Nov 15 and a CPU report on Nov 16, and was at 189.83 MB against a 30 MB limit. | Correlation only. fseventsd was still making progress, so it may be a victim rather than a cause. It is not known whether this level of churn had occurred before without a stall. |

Memory pressure is ruled out by the record. Spotlight is disabled on the volume. That the stall survived a volume-level remount and cleared on reboot fits both of the first two candidates: it shows only that the stuck state lived below the mounted volume.

The CI logs give the churn a visible source. The heavy-load window opens with the afternoon's first merge-queue builds at 14:10, and the staging failure above kept it going: every failed build ejected a pull request and made the queue rebuild the rest, producing 49 queue builds for eight pull requests in two hours. **Inference:** the build storm, itself driven by infrastructure failures, is at least part of the load the volume was under when it stalled. Bazel's idle-time garbage collection of the disk cache and the repo contents cache, both on that volume, is a further possible source of mass deletions that nobody has examined.

Evidence that would distinguish the candidates:

1. **A stackshot or spindump taken during a stall.** A kernel thread blocked in block I/O under the NVMe driver points at the device. One waiting on an APFS lock, or busy in APFS itself, points at the filesystem. This is the single most valuable missing artifact.
2. **Device-side counters before and after:** the NVMe error log and SMART counters (media errors, controller busy time, unsafe shutdowns), Thunderbolt link statistics and retrain counts.
3. **Block-level throughput during a stall.** Pending I/O with zero completions means the device. An idle device under a stuck filesystem means APFS.
4. **The per-minute APFS flush-latency series from 14:37 to 15:00**, set against the onset seen in CI at 14:41, and the Bazel server logs from `zakalwe` for the same minutes.
5. **Recurrence after the planned changes.** The scheduler is moving to a Linux host, but `zakalwe` and the other Macs keep the same volume layout as workers. A stall on a Mac under worker-only load would clear NativeLink's scheduler of any role in the trigger. A stall that stops recurring once `.fseventsd` is trimmed would support the churn hypothesis.

### Amplifier: the scheduler turned slow workers into a permanent wedge

The scheduler log shows this loop (record):

```
WARN simple_scheduler_state_manager: OperationId … timed out after 5 seconds issuing a retry
WARN api_worker_scheduler: Worker did not acknowledge a kill in time, removing from pool, unacknowledged_kill_timeout_s: 60
INFO api_worker_scheduler: Evicting worker from pool, is_disconnect: false, running_actions: 12
ERROR worker_api_server: Failed to remove_timedout_workers, err: … "Worker ids do not match …"
WARN worker_api_server: UpdateForWorker channel was closed, thus closing connection to worker node
```

Workers logged `Worker disconnected from scheduler, reconnecting … "UpdateForWorker stream closed early"`, and actions stayed `stage=Executing`.

The likely mechanism is an inference: with every store on a volume that had stopped committing, every worker became slow at once, because inputs could not be fetched and results could not be stored. NativeLink 1.6.6 handles a slow worker by timing its operations out after 5 seconds, killing and evicting it, and requeueing its work, and operations can then be left `Executing` with nothing to finish them. Six upstream fixes describe these behaviours. Their merge states and dates were verified for this document; the release each first shipped in is taken from the release dates.

| Upstream fix                                                  | First release | What it changes, from its description                                                                                                                                                  | What it matches here                                                                                             |
| ------------------------------------------------------------- | ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| [#2797](https://github.com/TraceMachina/nativelink/pull/2797) | v1.7.3        | A worker was evicted, and all its work requeued, after five seconds without a keepalive. Any message now refreshes liveness and the default timeout is 10 seconds.                     | `timed out after 5 seconds issuing a retry`, then eviction with 12 running actions.                              |
| [#2826](https://github.com/TraceMachina/nativelink/pull/2826) | v1.7.3        | The scheduler requeued an action onto the worker still running it, which refused with AlreadyExists; retries ran out, and the worker was then disconnected with all its other actions. | `Already Exists … is already running … attempted to execute too many times 4 > 3` in CI logs at 10:43 and 14:17. |
| [#2856](https://github.com/TraceMachina/nativelink/pull/2856) | v1.7.3        | A stalled scheduler connection left a worker evicted and idle. The worker now sees the stream close and reconnects, and the scheduler no longer reads the stream inline.               | The disconnect and reconnect churn: 41 connections closed in one hour.                                           |
| [#2900](https://github.com/TraceMachina/nativelink/pull/2900) | v1.7.6        | A failed liveness refresh discarded the worker's finished result and stranded the operation in `Executing`. Its case is a backwards clock step; it also names a just-evicted worker.   | Actions `Executing` forever after evictions.                                                                     |
| [#2923](https://github.com/TraceMachina/nativelink/pull/2923) | v1.7.7        | The executing timeout ran only inside a client's call, and a result the store refused left the record on a worker that had moved on. A periodic sweep now retires or requeues these.   | Actions `Executing` forever while the store could not accept writes.                                             |
| [#2925](https://github.com/TraceMachina/nativelink/pull/2925) | v1.7.7        | The scheduler never heard what a worker was actually running. Keepalives now carry running operations, and what the worker no longer runs is requeued.                                 | Slots held by operations no worker was running.                                                                  |

`Worker ids do not match` appears to be a side effect, not the cause: it is the cleanup after an eviction failing, and the loop proceeds the same way without it (record's assessment).

**Confidence:** medium to high that these defects made the stall total and kept it from clearing. The match is between the logged loop and the upstream descriptions, plus the #2826 signature seen directly in CI. It has not been confirmed by reproducing the loop on 1.6.6 or by showing that 1.8.6 survives the same fault. The same low-rate symptoms were present all day (6 to 20 operation timeouts per hour), and the laptop build that hung at 11:05 did so four hours before the volume stalled, which suggests the defects were already costing work before the trigger made them fatal.

### A separate failure: one executable a worker could not stage

Described in [finding 1](#1-remote-actions-were-failing-well-before-the-stall-for-a-different-reason). It is a distinct defect with its own unknown cause, and it produced most of the afternoon's failed builds. **Confidence:** high that it is separate from the stall, because it occurred before and after it. Low on its cause.

The fork's source carries a comment on this code path saying that `NotFound` during staging can follow an eviction, and that the mitigations are `experimental_active_input_leases` or a larger store. Whether that race explains a failure that repeated for 33 minutes on one registration is not established; a persistent failure looks more like a lasting disagreement between index and disk than a race.

### Contributing factors

- **Everything on one volume on one host.** The scheduler's stores, a worker's work directory, four CI agents' disk cache and repository cache, and the fseventsd journal all write to `/Volumes/ssd` on the host that is also the only scheduler. Load from any of them lands on all of them, and a stall takes down the service and the CI jobs on that host together (verified for the CI paths; record for the rest).
- **A single scheduler.** A known gap. Redundancy is deferred by the owner and is not an action item here.
- **Version lag behind a private fork.** The fixes had been available since October. The fork exists because macOS workers need a change upstream has not merged, so every upgrade is a rebase and a fleet-wide rebuild. That cost is a plausible reason the deployment was 16 releases behind; the record does not say.
- **Retry amplification in the merge queue.** A queue build that fails for an infrastructure reason ejects its pull request and restarts the builds behind it. Under a persistent infrastructure fault the queue generates load instead of shedding it.
- **An unbounded fseventsd journal.** 219,390 files in four days on a volume whose content is disposable build output.
- **Startup that deletes before it listens.** The restarted scheduler never opened its port because a directory removal blocked on the volume. The source at the deployed commit clears the executable-copy directory with a blocking `std::fs::remove_dir_all` while constructing the store. **Inference:** that is one candidate for the call in the record; the worker's work-directory cleanup is another.

## Detection gaps

Nobody was alerted. The first diagnosis began around 15:45: 45 minutes into the stall and about 90 minutes after CI's first remote-execution failures.

| Gap                                                                                                                          | Evidence                                                                                                          |
| ---------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| A client waiting on an executing remote action waits forever.                                                                | 47-minute wait on a 60-second test (verified). Builds at 0% CPU for hours (record).                               |
| CI's health probe is a TCP connect, which a wedged scheduler passes.                                                         | Steps went remote throughout the wedge and fell back within two minutes of the process dying (verified).          |
| Local fallback is silent.                                                                                                    | The morning's Tailscale episode (record).                                                                         |
| No alert on the scheduler's own distress signals.                                                                            | 120 timeouts, 41 closed connections and 32 failed cleanups in an hour, against 6 to 20 timeouts earlier (record). |
| No alert on the volume.                                                                                                      | 58 minutes without a commit (record).                                                                             |
| A CI step with no output is not stopped. `pipeline.yml` sets no step time limit.                                             | Three jobs silent for 64 to 75 minutes (verified).                                                                |
| The merge queue reports each failure as that pull request's failure.                                                         | 35 infrastructure-failed queue builds, each ejecting a pull request (verified).                                   |
| A wait on a build can be renewed without evidence of progress. A live process with no errors looks the same as a slow build. | The 11:05 build, renewed for more than 4.5 hours as "still active, no errors" (record).                           |
| Automation does not tell infrastructure failures from code failures.                                                         | "Fix failing CI" work created for infrastructure failures (record).                                               |
| The morning episode of the staging failure went unnoticed as a pattern.                                                      | Ten builds failed with one identical error between 10:44 and 11:17, three hours before it recurred (verified).    |

## Recovery

| Action                              | Result                                                                                                                                                                | Source           |
| ----------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------- |
| Restart NativeLink, 15:47:36        | Did not work. The new process blocked on the volume during startup and never listened. It did release the hung CI clients, and CI began falling back to local builds. | Record, Verified |
| Force unmount and remount, 15:55:35 | Did not work. The unmount took 23 seconds and the remount 0.3 seconds, but `ls` still hung. It did end the three silent CI jobs on the host.                          | Record, Verified |
| Reboot, 15:56:46                    | Worked. NativeLink was ready and all six workers reconnected 81 seconds later.                                                                                        | Record           |

Recovery was not clean:

- The staging failure returned at 16:07 and failed three builds. It was not seen in builds created between 16:10 and 16:25, several of which executed remotely and passed.
- Three `mac-app-build` jobs on one agent failed between 16:11 and 16:22 with `Lost inputs no longer available remotely`. That host had built the same targets locally during the fallback window. **Inference:** its disk cache holds outputs the remote cache never received, or the remote cache lost blobs written around the stall. The same message appeared twice earlier in the day on Linux agents, so it is not proof of store damage on its own. The store's consistency after the stall has not been checked.
- PR #3738 had to be re-queued by hand.

## What went well

- The diagnosis at 15:45 separated network, memory and disk within minutes and tested the volume directly.
- The kernel and unified logs were complete, which is why the stall can be timed to the millisecond and why hardware errors can be said to be absent from the log.
- CI's per-step fallback did what it was designed to do once the scheduler was unreachable, and a queue build that started before the reboot went on to merge.
- The lint step's own 600-second limit turned two hangs into failures at 14:20.
- The reboot was clean, and the service came back without manual repair.
- The remedy for the amplifier already exists upstream, and the move off the external volume was already planned.

## What went poorly

- Failures were visible in CI from 10:44 and again from 14:13, and nothing connected them until a person looked at the host at 15:45.
- The failure mode for clients was a silent hang, the hardest one to notice.
- Two of three recovery attempts did not work, and the one that did also erased the in-memory state that could have identified the trigger.
- The merge queue amplified the fault and attributed it to pull requests.
- Automation spent effort on the symptoms: waiting on a dead build and opening fix work for infrastructure failures.
- The scheduler ran a version with known, fixed defects in exactly this area.

## Action items

Decided before this document and in progress elsewhere. They are listed for reference, not implemented here.

| Item                                                                                                       | Addresses                                    |
| ---------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| Move the scheduler, in about two days, to a new Linux host with an internal SSD.                           | Trigger exposure; one host doing everything. |
| Upgrade NativeLink to v1.8.6 with the clonefile commit rebased, on the scheduler and all workers together. | Amplifier.                                   |
| Make a stuck remote action fail Bazel within a bounded time.                                               | Detection: unbounded execution wait.         |
| Alert on the eviction loop.                                                                                | Detection: scheduler distress.               |
| Put the NativeLink configs and the runbook under version control.                                          | Recovery speed and repeatability.            |

Proposed by this postmortem. Owners are proposals, not assignments.

| Proposed item                                                                                                                                                                                                                                                                                         | Proposed owner                 | Addresses                                 |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------ | ----------------------------------------- |
| Find the worker that cannot stage executable `5fdf67f7…-4987`: map the three worker ids to a host in the scheduler log, inspect that store entry, and repair or rebuild the store. `determinist` is the first candidate. Do this before the upgrade, so the cause is not lost with the old processes. | Infrastructure owner           | The separate staging failure; still live. |
| Capture a spindump and stackshot automatically when the store volume or the scheduler stalls, and keep them off the affected volume.                                                                                                                                                                  | Infrastructure owner           | Trigger: the missing evidence.            |
| Watch the store volume directly: a small write-and-sync probe on an interval, or APFS commit progress. Alert, and mark the scheduler unhealthy, when it stops. Keep an equivalent probe on the new Linux host.                                                                                        | Infrastructure owner           | Detection: volume.                        |
| Keep `.fseventsd` in check on every build volume, including the five workers', for example with the volume's `no_log` marker after confirming it on the current macOS. Record its size as a metric.                                                                                                   | Infrastructure owner           | Possible contributor to the trigger.      |
| Probe the remote executor with a real request and a deadline before a CI step goes remote, instead of a TCP connect.                                                                                                                                                                                  | CI maintainers                 | Detection: wedged but listening.          |
| Recognise infrastructure failures in CI by signature (the ones in this document are a starting list), mark the build as such, and keep automation from opening code-fix work for them. Failures without such a signature must stay red.                                                               | CI and Boss engine maintainers | False follow-up work; queue ejections.    |
| Stop the merge queue from re-running everything while remote execution is known to be unhealthy.                                                                                                                                                                                                      | CI maintainers                 | Retry amplification.                      |
| Fail a CI step that has produced no output for a bounded time. This is a no-progress bound, not a wall-clock cap on a build that is still working.                                                                                                                                                    | CI maintainers                 | Detection: silent CI jobs.                |
| Say so when a developer or agent build runs locally because the endpoint was unreachable or did not resolve.                                                                                                                                                                                          | Build tooling maintainers      | Silent fallback.                          |
| Require evidence of progress before an agent's wait on a build is renewed: CPU time, output growth or Bazel progress.                                                                                                                                                                                 | Boss engine maintainers        | Hung build read as progress.              |
| Rehearse this failure on the new stack: stall the store volume or pause a worker, then confirm that clients fail within the bound, the alert fires, and the scheduler recovers without a reboot. Confirm that startup no longer blocks before listening.                                              | Infrastructure owner           | Confidence in the amplifier fix.          |
| Add a runbook step for after a remote-cache restart, once the cause of the `Lost inputs` failures is known.                                                                                                                                                                                           | Infrastructure owner           | Unclean recovery.                         |
| Help the macOS staging change land upstream (issue 3137, pull requests 3138 and 3191) so the fork can be retired.                                                                                                                                                                                     | Infrastructure owner           | Version lag.                              |

Known gap, not a commitment: scheduler redundancy. The owner has deferred it. After the move there will still be one scheduler.

## Open questions

- Why did APFS stop committing at 15:00:08? Unanswerable from the evidence that exists.
- What was already blocking on the volume at 14:41, and is it the same event?
- Which host is the worker that cannot stage the executable, and why do its index and disk disagree? Is it on a volume with the same symptoms?
- Why did two CI agent hosts fail to resolve the scheduler's host name at 14:13 and 14:16, hours after the laptop's Tailscale problem was fixed, and what took `determinist`'s agents offline at 14:14?
- Was the laptop build that hung at 11:05 stranded by the scheduler defects alone? Its timing fits the morning episode, and nothing else is known about it.
- Did the remote cache lose content during the stall?
- Does v1.8.6 actually survive a stalled store volume? The upstream descriptions say it should. Nobody has tried.

## Method

The CI findings can be reproduced from the Buildkite API: list the pipeline's builds for the day with their jobs, fetch the log of each failed job, and search the logs for the signatures quoted above. Worker registration times come from decoding the 60-bit timestamp of each version-6 UUID. Upstream pull-request states, merge dates and release dates come from the GitHub API. Flag defaults come from `bazel help build --long` on Bazel 9.1.0. Build and job counts are as of 16:26 on 2026-11-18, pull-request states as of 16:33. The counts classify a build as an infrastructure failure when any failed job's log contains one of these: `executable-variant copy failed`, `Unable to resolve host`, `DEADLINE_EXCEEDED`, `UNAVAILABLE: io exception`, `could not acquire lock on repo contents cache`, `is already running`, `Lost inputs no longer available remotely`, or the lint's wall-clock limit message.
