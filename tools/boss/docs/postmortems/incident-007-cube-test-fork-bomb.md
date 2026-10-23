# Incident 007 — Cube test fork bomb and false update rollback

- **Date:** 2026-10-23; all timeline times are CDT (UTC−05:00).
- **Impact:** Developer laptop unusable for about 15 minutes; forced power-off; interrupted agent work; a release with no demonstrated defect rolled back and blocklisted.
- **Status:** Host recovered after reboot. Self-exec fix is in progress separately in the jj bookmark / shared-store scaling work. This document changes no code.
- **Provenance:** Incident-day investigation notes supplied with the writeup request, read-only macOS diagnostic reports, and source inspection.
- **Source baseline:** `main` at `8c9ee8eefa7db8d581c5ce077f502d492e8c32e5` (mono#3265). All main-source `path:line` references below refer to this revision.

## 1. Summary

An unmerged cube store-GC change recursively launched the Rust test binary as if it were the cube CLI. Detached descendants survived the test timeout and overwhelmed the host with filesystem activity; during the resulting starvation, Boss's update helper incorrectly concluded that version 1.0.795 had not launched, restored 1.0.794, and blocklisted 1.0.795.

### Verdict

The initiating failure was recursive self-execution in `cube_lib_test`, followed by system-wide kernel lock and I/O starvation. The update rollback was a separate detection defect exposed by that load, not evidence that 1.0.795 was defective. The concurrent, normal engine test run was initially blamed incorrectly.

## 2. Evidence and method

The supplied notes are the source for workspace attribution, the unmerged patch, test-log excerpts, update-event timestamps, process limits, and post-reboot observations. This writeup did not read production Boss state, query the running engine for incident evidence, inspect another worker's checkout, or reproduce the unsafe test. Workflow commands used to publish this document are not incident evidence.

The following reports were read under `/Library/Logs/DiagnosticReports/`:

- `WindowServer_2026-10-23-152434_Brians-Laptop.userspace_watchdog_timeout.spin`: header, process inventory, and lock-wait/thread counts. Despite its filename, the captured interval is **15:16:49.910–15:16:55.480**; its reason names logd's firehose queue.
- `logd-2026-10-23-151719.ips`: watchdog stackshot at 15:17:05.491, unresponsive firehose I/O queue, 40 seconds without a successful check-in.
- `WindowServer-2026-10-23-152448.ips`: WindowServer main-thread watchdog, 40 seconds without a successful check-in.
- `ResetCounter-2026-10-23-153255.diag`: `Boot faults: btn_rst,finger_reset force_off`.
- `disk writes_2026-10-21-153508_Brians-Laptop.diag` and `disk writes_2026-10-21-153559_Brians-Laptop.diag`: earlier `cube_lib_test` powerstats, discussed in §5.3.

These are local diagnostic artifacts, not publicly attached evidence. Update-log observations below are attributed to the supplied investigation notes rather than presented as independently reread logs. No kernel panic or incident-time JetsamEvent was reported; the directory does contain a **13:04** JetsamEvent, outside this incident window.

## 3. Timeline

| Time                | Observation                                                                                                                                                                                                                                                                                                     |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 14:31               | Earlier update from 1.0.789 to 1.0.794 staged in about 1.5 seconds, providing a normal-latency comparison.                                                                                                                                                                                                      |
| 15:11–15:15         | Boss downloads/stages 1.0.795. Staging takes 204 seconds (`totalDurationMs=204429`).                                                                                                                                                                                                                            |
| ~15:14              | Supplied attribution places `bazel test //tools/cube/...` in workspace `mono-agent-538`, on the unmerged jj store change. Its output-base log says `Test timed out at 20:14:16 UTC`, with GC tests running for over 60 seconds. These approximate times do not establish the exact start of recursive spawning. |
| 15:16:49            | Stackshot records logd's firehose I/O queue unresponsive.                                                                                                                                                                                                                                                       |
| 15:17:05            | logd watchdog stackshot is captured.                                                                                                                                                                                                                                                                            |
| 15:17:26–29         | Boss 1.0.794 loses its WindowServer connection: `Failed to look up the port for com.apple.windowserver.active`, then `Connection interrupted`.                                                                                                                                                                  |
| 15:19:40            | Boss relaunch begins; reaching `startup.will-finish-launching` takes 98 seconds.                                                                                                                                                                                                                                |
| 15:22:31–39         | Startup discovers staged 1.0.795 and automatically swaps it in: `swap.requested userInitiated=false`, then `swap.committed installedVersion=1.0.795`. The app arms the helper with a 30-second watchdog and 15-second launch timeout, then exits.                                                               |
| 15:23:01            | Helper bundle assessment passes: codesign OK, version 1.0.795.                                                                                                                                                                                                                                                  |
| 15:23:05            | runningboardd launches Boss 1.0.795 as PID 12124.                                                                                                                                                                                                                                                               |
| 15:23:13–17         | `open` returns exit 0 after 11 seconds, versus normally about zero. The helper's diagnostic scan logs `appExecutablesRunning: 12124 /Applications/Boss.app/Contents/MacOS/Boss`.                                                                                                                                |
| 15:23:19            | Helper decides `reason=launch-not-started`, claiming no process was observed within 15 seconds. Its own scan contradicts that conclusion.                                                                                                                                                                       |
| 15:24:34 / 15:24:48 | WindowServer watchdog reporting: main thread unresponsive, 40 seconds since last successful check-in. Reported thermal pressure is nominal.                                                                                                                                                                     |
| 15:24:47–15:26:36   | Helper logs `removed-failed-bundle`, `restored-backup restoredVersion=1.0.794`, then `marker-written`; individual steps take 40–90 seconds. No `reopened-previous` or `completed` event is recorded.                                                                                                            |
| ~15:32              | Developer holds the power button to force power-off. Reset report confirms forced-off boot faults; recorded boot time is 15:32:17.                                                                                                                                                                              |
| 15:33:38            | Boss 1.0.794 starts and displays the rollback notice.                                                                                                                                                                                                                                                           |
| 15:35:58            | `stage.skipped-blocklisted`: 1.0.795 will not auto-install.                                                                                                                                                                                                                                                     |
| 15:36               | Workers have immediately re-dispatched. Load is 129 during an unrelated, normal 11-shard engine test run.                                                                                                                                                                                                       |
| 15:39               | Another workspace's main-revision `cube_lib_test` passes 503 tests in 9 seconds, according to the incident notes.                                                                                                                                                                                               |
| 15:41               | Load is 27, with zero `cube_lib_test` processes.                                                                                                                                                                                                                                                                |

## 4. Impact

The developer's laptop was unusable for approximately 15 minutes and required a forced power-off. In-flight agent work was interrupted; the available evidence does not establish the extent of any unsaved work loss. Logging, the window server, process discovery, and filesystem operations all suffered substantial delays.

The rollback restored the previous app bundle but did not finish its recorded relaunch sequence before power-off. A release with no demonstrated defect was classified as failed and excluded from automatic installation. Version 1.0.795's only notable change in the supplied notes was mono#3265; nothing in these observations implicates that change.

## 5. Investigation and root causes

### 5.1 Recursive self-execution in the unmerged store-GC change

The supplied patch evidence describes `tools/cube/src/app/store_gc.rs:332` (approximately, **unmerged patch, not main**): `spawn_detached_store_gc` constructs `Command::new(std::env::current_exe()?)`, passes `repo gc <repo>`, closes stdio, uses `.process_group(0)`, spawns, and drops the child handle. A new pool-GC call near `tools/cube/src/app/gc.rs:583` (**unmerged patch**) invokes it during workspace leasing when `store_gc_due` is true. A fresh test database has no timestamp, so this condition holds.

Inside `cube_lib_test`, `current_exe()` identifies the test executable. Consequently, the child runs `cube_lib_test repo gc mono`; libtest interprets the positional arguments as test-name filters, and `gc` selects GC tests. Those tests lease workspaces and spawn more copies. Multiple matching tests per generation create exponential growth instead of a single background maintenance process.

The child starts a new process group, escaping the group that the harness can terminate. The timeout kills the harness-managed group but does not end the detached recursion. Dropping handles and closing stdio further remove direct lifecycle control and useful child output.

This code never reached main. At the inspected baseline, `store_gc.rs` is absent; the existing pool-GC entry is `tools/cube/src/app/gc.rs:361`. It would be misleading to cite the unmerged mechanism as current-main code or to infer that main reproduces it.

Independent recounting of the spin report finds **1,978 process headings**, **325 `cube_lib_test` process headings**, **276 `/bin/sh` paths**, **2,358 lines naming `app::tests::gc_tests`**, and **6,084 lock-wait entries containing both `blocked by krwlock` and `owned by cube_lib_test`**. These are report entries, not 6,084 distinct locks. The supplied analysis additionally identifies a common test-binary UUID, roughly 13 threads per test process, process ages of 345–455 seconds, and 103 `repo_tests` thread entries.

The lock waits span jj, Python, rustc, java, mds, and fseventsd. Together with the GC-heavy stacks, this supports a system-wide kernel lock convoy caused by mass filesystem deletion. The roughly 2,000 processes were below the reported limits (`kern.maxproc=16000`, per-user limit 10666): process-table exhaustion is not the supported explanation. Nominal thermal pressure and the watchdog evidence also distinguish this incident from a thermal shutdown.

### 5.2 False-positive launch failure and destructive rollback

In `tools/boss/app-macos/Sources/Resources/RelaunchHelper/relaunch-helper.sh:319`, the helper backgrounds `open`; line 322 records `launch_started` immediately afterward. Thus the 15-second wall-clock window includes the wait for `open` to finish, rather than starting after its completion. This refines the incident notes' description that the timer starts “before open.” The relevant helper change is attributed by those notes to mono#3147 (2026-10-11).

The loop at `relaunch-helper.sh:336` polls `find_running_pid`, then collects the `open` result and evaluates elapsed time. Those operations can themselves stall. Once the deadline has passed, lines 350–354 perform a diagnostic `ps` scan, log it, and unconditionally call `rollback launch-not-started`. There is no final adoption/re-check of the candidate PID before rollback. The diagnostic result is not used in the decision.

PID 12124 was launched and appeared in that diagnostic scan, yet `new_pid` remained empty. Rollback's termination guard at `relaunch-helper.sh:178` therefore skipped terminating it. The removal and backup restoration at `relaunch-helper.sh:192` replaced the bundle from which that process was running. The evidence establishes a false “not started” decision; it does not establish that the new app would have completed its readiness handshake while the host remained starved.

The failed-version marker at `relaunch-helper.sh:202` persists the classification beyond the host incident. Automatic installation then remains blocked even after load subsides.

**Recovery qualification:** the incident notes describe the blocklist as having no recovery path. Current main does have an explicit recovery mechanism: `tools/boss/app-macos/Sources/UpdateCore/UpdateInstaller.swift:536` removes a version from the blocklist, `tools/boss/app-macos/Sources/UpdateLifecycle.swift:346` allows deliberate retry, and `tools/boss/app-macos/Sources/Update/UpdateRollbackBanner.swift:38` exposes “Retry Update.” What is demonstrated here is lack of automatic recovery and no recorded successful use of manual recovery during the incident, not absence of all recovery code. Whether that path was available and effective in the restored 1.0.794 bundle is unverified. Follow-up should verify and improve the existing path rather than duplicate it.

### 5.3 Did earlier orphaned runs also trigger fork bombs?

The supplied history records nine orphaned endings since October 21, with no PR from the jj store work. That is a repeated lifecycle failure worth investigating, but it does not establish nine fork bombs.

A search of the available top-level October 21–22 diagnostic reports found `cube_lib_test` powerstats in two October 21 disk-write reports (at lines 703083 and 1309041 respectively). Their sections contain 11 and 17 samples in the Boss resource coalition. Those samples establish earlier test execution, not recursive parentage, detached descendants, the exact patch revision, or a connection to an orphaned ending. No earlier recurrence is confirmed by this evidence. Resolving it requires preserved per-attempt test logs, revisions, process ancestry/counts, and orphan-transition reasons; production state was not accessed to obtain them.

### 5.4 Alternatives ruled out

The first diagnosis blamed the concurrent `engine_lib_test` run. Its 11-way sharding is long-standing (`tools/boss/engine/core/BUILD.bazel:270`), as is the high job ceiling (`.bazelrc:1`, `--jobs=200`; no `--local_test_jobs` setting). The reported run completed normally. High load alone does not distinguish normal parallel tests from recursive spawning; the cube process inventory and lock ownership do.

The wrapper does not fan out test shards. `tools/test-sandbox/hermetic_test_wrapper.sh:315` launches one sandboxed command; Bazel supplies sharding. Main's reported 503-test, 9-second cube result is further evidence against treating all cube test executions as faulty, though it is not a replay of the unmerged patch.

## 6. Contributing factors

- **Real detached children inside tests:** self-exec assumed a production executable identity. Fresh databases repeatedly activated the maintenance path, without a test seam controlling the spawn.
- **Containment did not cover detachment or child count:** the wrapper allows the test executable and declared runfiles (`tools/test-sandbox/hermetic_test_wrapper.sh:281`) but supplies no bound on recursive child creation or prevention of new process groups. Filesystem/exec restrictions are not a resource quota.
- **Timeout cleanup had a narrower reach than the process tree:** group-directed termination at `hermetic_test_wrapper.sh:98` and signal forwarding at line 132 cannot reach descendants that create separate groups. The group established at line 314 is insufficient for that case.
- **Launch detection used elapsed time as evidence of failure:** under load, the polling and logging machinery consumed the deadline. The contradictory late scan was diagnostic only.
- **Blocklisting outlived the transient host condition:** no automatic recovery cleared the false classification. Existing manual retry needs incident-shaped verification, including redownload after bundle restoration.
- **Recovery immediately reintroduced work:** workers re-dispatched into a heavily loaded host after reboot. This did not initiate the pre-reboot failure, but reduced recovery headroom.
- **Repeated orphaning did not produce an effective intervention:** nine attempts ended without a PR. The notes do not explain why the retry history failed to surface the underlying problem.

## 7. Detection: what went well, what went poorly, and lessons

**What went well:** macOS retained watchdog and forced-reset evidence. The spin report preserved process identities, ages, stacks, and lock ownership across the reboot. The helper's event trail recorded both the rollback decision and the contradictory live PID, allowing the rollback to be identified as false rather than blaming the release. A normal main cube test result helped isolate the unmerged change.

**What went poorly:** the first diagnosis attributed the outage to normal engine test fan-out. That attribution relied on conspicuous concurrent load rather than the recursive process lineage and lock evidence. Test timeout was mistaken for containment even though descendants escaped its process group. Update diagnostics captured the decisive information without consulting it before deleting the installed bundle. The repeated orphan history had not led to an explanation before this incident.

**Lessons:** diagnose using the offending invocation's logs and historical process evidence, not global process-name matches or load alone. Treat background process creation as a lifecycle boundary that tests must control. An overloaded observer cannot turn failure to observe promptly into proof that an application never started. Recovery should restore both service and the ability to accept a known-good update.

## 8. Action items

These are separate, reviewable follow-ups, with no owners assigned and no implementation in this PR. They preserve the investigation's requested scope while incorporating the source findings above.

1. **Fix the self-exec (already in progress):** complete the jj store change's executable-selection/spawn boundary so library tests cannot recursively invoke themselves as cube. Add a regression that observes the intended command through a controlled runner and proves fresh-database leasing cannot spawn recursive test processes.
2. **Re-check before rollback:** after deadline handling and diagnostics, perform authoritative process discovery again, adopt a valid newly launched PID, and ensure rollback accounts for any such process before replacing its bundle. Test a PID appearing between the initial poll and the timeout decision, including exclusions for pre-existing processes.
3. **Make launch detection tolerate load:** separate launch-request progress from readiness, use bounded timing that tolerates delayed probes, and distinguish observer failure from application failure. Test delayed `open`, delayed process scans, and successful launch near the deadline without running a host-wide stressor.
4. **Recover falsely blocklisted releases:** verify the existing “Retry Update” flow after rollback has consumed the staged bundle, including redownload, unblock, and successful install. Make that recovery discoverable and preserve the failure reason; explicitly decide whether transient host failures warrant a bounded automatic retry. Do not introduce an unreviewed infinite retry policy.
5. **Contain test descendants:** evaluate and implement a supported way to prevent tests escaping harness containment or spawning unbounded children. Cover new process groups and sessions, enforce a per-test resource boundary where supported, and add a small bounded regression proving timeout leaves no descendants. Document platform limits rather than assuming filesystem sandboxing contains forks.
6. **Explain and surface repeated orphaning:** reconstruct the nine attempts from preserved revisions and lifecycle evidence, distinguish worker loss from test timeout or host failure, and add a visible escalation for repeated orphaning without progress. Verify with a deterministic lifecycle test; do not assume every orphan was a fork bomb.
7. **Pace post-reboot dispatch:** add a bounded recovery ramp or admission check so redispatch does not immediately saturate a recovering host. Define the recovery signal and maximum delay explicitly, and test eventual resumption as well as suppression under load.

## 9. Open questions

- Did any of the earlier nine orphaned attempts run the same recursive-spawn code? The available October 21 samples are insufficient; the evidence needed is listed in §5.3.
- Was manual update retry available and effective in restored 1.0.794, and why was it not used successfully in the recorded recovery? Current-main source alone cannot answer this.
- **Unverified observation:** between 15:13 and 15:26, short-lived Claude coordinator sessions were reportedly started roughly every two minutes in the production Boss session directory, each against a throwaway engine whose only products were named “Audit 0” through “Audit 4.” Whether this relates to the incident is unknown. This writeup did not inspect that directory and makes no causal inference from the observation.
