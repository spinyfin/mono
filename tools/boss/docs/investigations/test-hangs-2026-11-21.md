# Engine and cube test hang samples, 2026-11-21

Read-only `ps` and `sample <pid> 1 1` found 48 orphaned test binaries on anaplian and skaffen and six on the developer Mac. The remote binaries had survived for two to four days. Forty-five remote binaries were blocked in `WorkDb::connect` acquiring the shared connection mutex. These were not evidence of slow compilation or healthy long tests: libtest was waiting for the remaining test thread, which was asleep in `__psynch_mutexwait`.

## The shared connection group

The common stack suffix was `WorkDb::connect → connection_wait::acquire → Mutex<rusqlite::Connection>::lock → _pthread_mutex_firstfit_lock_wait → __psynch_mutexwait`. The table gives the named test, the number of remote binaries containing it, and the call immediately above that suffix. Counts overlap for the two binaries containing three prerequisite-link tests.

| Test                                                                                                                 | Binaries | Caller above `connect`                               |
| -------------------------------------------------------------------------------------------------------------------- | -------: | ---------------------------------------------------- |
| `tests::t34::answering_a_gated_revision_records_the_answer_and_waits_for_its_gate`                                   |       11 | `create_execution` or `list_operator_questions`      |
| `merge_intent::tests::a_stale_owed_fix_ends_even_after_an_earlier_ci_attempt_finished`                               |        9 | `active_merge_intent`                                |
| `tests::revision_ai_review_tests::a_batch_created_before_the_implementation_is_finalized_still_counts`               |        4 | `list_revisions_under_ai_review`                     |
| `conflict_deferral::tests::single_item_projection_carries_the_deferral_flag`                                         |        4 | `get_work_item`                                      |
| `driver_allocation::dispatch_tests::attention_sweep_keeps_the_banner_of_a_pre_start_failure_until_a_real_run_starts` |        3 | `reconcile_stale_attention_signals`                  |
| `tests::task_status_audit_tests::abandon_history_rolls_back_with_its_transaction_and_leaves_no_context_behind`       |        3 | `list_task_field_audit`                              |
| `prerequisite_link::tests::answering_a_stored_link_rechecks_chain_and_owner_liveness_without_adding_edges`           |        3 | `answer_operator_question_from`                      |
| `prerequisite_link::tests::ambiguous_owners_reject_explicit_and_scanned_create_and_link_questions`                   |        2 | fixture setup                                        |
| `prerequisite_link::tests::bare_number_without_an_asking_repo_requires_a_unique_open_owner`                          |        2 | fixture setup                                        |
| `tests::startup_wait_tests::heartbeat_and_card_log_the_same_non_abandoned_execution`                                 |        2 | `get_task_runtime`                                   |
| `tests::startup_wait_tests::unexplained_wait_age_follows_the_execution_transition_not_the_task_status`               |        2 | `get_task_runtime`                                   |
| `merge_conflict_merging::tests::only_revisions_opened_after_the_request_stop_the_continuation`                       |        2 | `unconfirmed_merge_revisions → open_merge_revisions` |
| `coordinator_tests::recovery_fresh_start::legacy_source_transport_is_not_queried`                                    |        1 | `list_task_field_audit`                              |
| `coordinator_tests::recovery_fresh_start::full_remote_source_does_not_pin_dispatch_to_its_occupied_slot`             |        1 | `list_task_field_audit`                              |

Example: anaplian pid 2858 retained only the harness and the gated-revision test thread. Its stack was:

```text
answering_a_gated_revision_records_the_answer_and_waits_for_its_gate
  WorkDb::create_execution
    WorkDb::connect
      connection_wait::acquire
        Mutex<rusqlite::Connection>::lock
          __psynch_mutexwait
```

The cause is recursive acquisition of the one connection: a caller holds its connection guard while calling another `WorkDb` method. That method cannot complete until the caller drops the guard, and the caller cannot drop it until the method returns. This ordering is deterministic; load is not required. Individual fixture fixes already on main release guards before those calls (for example, `tests/t34.rs`, `tests/startup_wait_tests.rs`, and `tests/task_status_audit_tests.rs`). The samples are from old executables and do not establish that every sampled call site is still broken on current main.

The developer Mac also retained pid 53161 with `progression_verbs::tests::an_answer_to_a_worker_question_does_n…` and `tests::progression_candidates_tests::the_calibrated_cooldown_is…` in the same suffix. Those test names no longer exist in this checkout; the thread names are truncated by macOS. The local app sample for `force_answer_behind_a_writer_dispatches_once_the_writer_finishes` is listed under separate groups: it is a two-worker Tokio test and the retained sample has no thread inventory proving a same-thread holder.

`connection_wait` now tracks live guards per thread and shared mutex identity. Same-thread re-entry returns an error naming the holder and attempted acquisition sites before waiting. Cloned `WorkDb` handles share an identity; independent connections and other threads remain permitted. The guard removes ownership on normal drop and panic unwind. Deterministic regressions exercise re-entry while the guard is held, reacquisition after release, independent connections released out of order, cross-thread contention, and poisoning after panic. This is a production precondition check, so an invalid call cannot silently deadlock outside tests either.

Before this change, `bazel test --config=dev-local --remote_download_outputs=minimal --jobs=32 --runs_per_test=10 //tools/boss/engine/work-db:work_db_test` passed all ten runs, each containing 2,718 tests (328.1–338.8 seconds). The gated-revision test also passed ten isolated runs (2.1–2.2 seconds). A sample of the long full run showed active scale-test work; it was not a recurrence of the old self-lock. These zero-hang baseline results are not a claimed improvement in historical hang rate.

With the ownership check and four regressions, the same ten-run workload finished every run without a hang (2,722 tests per run, 464.4–475.7 seconds). Seven runs passed; three failed assertions about progression launch-backoff windows, with no connection re-entry errors. Those tests seed timestamps from the wall clock and expect a 120-second backoff still to be active when later assertions run. They require separate clock-control work, rather than a relaxation of deadlines or assertions. In the same loaded batch, ten runs each of `cleanup_guard_test` and `capture_stacks_test` passed, including real all-thread samples retained after process-group cleanup.

After rebasing the change onto `5fc3fd46`, the `--test_filter=progression --runs_per_test=3` isolation run passed all 147 matching tests in each run (23.8–25.1 seconds per action). The failure classification is unrelated timing sensitivity: `progression_candidates.rs` compares the fixture's recorded failure time plus 120 seconds with SQLite's live `strftime('%s','now')`, and the ownership check changes neither timestamp nor query. The failed loaded runs remain part of the evidence; the isolated success does not erase them. The full CI target set (`bazel build` and `bazel test --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/...`, both with `--remote_download_outputs=minimal`) subsequently passed: 438 build targets and 222 test targets, including work-db.

## Separate groups to investigate

These samples have different waits and are not fixed by checking connection ownership:

- **Cube fork fetch and store-GC:** Mac pids 2407, 2409 and 23249, 13–22 days old: `concurrent_fork_fetches_use_distinct_remotes → ScopedJoinHandle::join`, with the sole remaining worker in `ConcurrentFetch::run → Barrier::wait → __psynch_cvwait`. The other participant was gone. Current main synchronizes the missing-head observation instead of remote addition and uses a condition-variable rendezvous; this sample describes the older barrier implementation. CI still has separate store-GC concurrency failures, including `Invalid hex operation id: "lock"`; those need their own cube investigation.
- **Completion:** anaplian pids 42587 and 85926: `tests::t16::stop_boundary::overlapping_stops_for_one_execution_are_serialized → Tokio current-thread runtime → IoStack::park → kevent`. Only the harness and that async test remained. The stack establishes an unfinished async wait, not which future lost progress.
- **App dispatch:** Mac pid 86048: `app::tests::proposals::force_launched_resume::unpaused_answer_uses_normal_dispatch → Tokio current-thread runtime → kevent`. The test awaits runner notifications; the sample alone does not establish whether notification or dispatch admission is responsible.
- **App writer wait:** Mac pid 99309 in `app::tests::mismatch_answer_dispatch::force_answer_behind_a_writer_dispatches_once_the_writer_finishes`, through `parked_mismatch_inner → WorkDb::connect → Mutex::lock`. The test is `tokio::test(flavor = "multi_thread", worker_threads = 2)`. The retained sample names the waiter and not the holder, so another worker thread may have owned the mutex; the same-thread re-entry check would not catch that.
- **Driver:** anaplian pid 43141: ten `agy::home`/`agy::hooks` test threads in `Mutex<()>::lock → __psynch_mutexwait`, with the mutex symbol instantiated in `signal_hook_registry`. They include `provisioning_refuses_aliases_before_writing_host_files`, `probe_kills_a_hung_forwarder_at_its_deadline`, and `probe_uses_a_private_events_socket`. A mutex's generic symbol is not proof of its owner; diagnosing the holder requires separate investigation.
- **CI-only TIMEOUTs:** spoke ingestion, spoke-preflight, and worker-setup-trust isolation jobs in the table below have target-level TIMEOUT lines and no sampled connection stack.
- **Clock-sensitive fixtures:** progression launch-backoff windows, spoke-adapter heartbeat/unconfirmed-stop bounds, and the inbox receipt handshake fail under load when they compare against wall clocks or delivery order rather than a controlled clock.
- **Watchdog startup ownership:** `cleanup_guard_test`'s startup-kill case can lose the launcher before it announces a child, so stack capture never runs. That is a launch/watchdog protocol gap, not a WorkDb lock.
- **UpdateCore observer ordering:** `RelaunchHelperTests.testEarlyExitIsRecordedAsProcessExited` can observe `observerUnavailable` instead of the helper's exit code. The Swift helper does not use WorkDb.
- **Linux timeout backtraces:** the hermetic wrapper still `exec`s the test on non-darwin hosts, so a Linux timeout has no stack dump. macOS capture is the delivered diagnostic.

## CI evidence window

Queried failed/failing mono builds created from 2026-11-07 through the 2026-11-21 investigation: 628 builds, 508 failed Bazel jobs, all logs retrieved successfully. Counted each job once per TIMEOUT target, not each repeated log line:

| Target                                                       | Jobs with TIMEOUT | Example build                                           |
| ------------------------------------------------------------ | ----------------: | ------------------------------------------------------- |
| `//tools/boss/engine/work-db:work_db_test`                   |                 1 | [24713](https://buildkite.com/flunge/mono/builds/24713) |
| `//tools/boss/engine/core:engine_lib_test`                   |                 3 | [27610](https://buildkite.com/flunge/mono/builds/27610) |
| `//tools/boss/engine/app:app_test`                           |                 3 | [25756](https://buildkite.com/flunge/mono/builds/25756) |
| `//tools/boss/engine/driver:driver_test`                     |                 1 | [26740](https://buildkite.com/flunge/mono/builds/26740) |
| `//tools/cube:cube_lib_test`                                 |                 1 | [25047](https://buildkite.com/flunge/mono/builds/25047) |
| `//tools/boss/engine/spoke:ingestion_test`                   |                 3 | [27384](https://buildkite.com/flunge/mono/builds/27384) |
| `//tools/boss/engine/spoke-preflight:spoke-preflight_test`   |                 9 | [26350](https://buildkite.com/flunge/mono/builds/26350) |
| `//tools/boss/engine/core:worker_setup_trust_isolation_test` |                 1 | [25701](https://buildkite.com/flunge/mono/builds/25701) |

The work-db log names migration/scale tests running over 60 seconds; it does not identify a deadlock. App builds 24859 and 25756 name `app::design_docs::tests::socket_tests::identical_cold_reads_start_without_waiting_for_the_first_response`. Cube build 25047 includes the fork-fetch test above and store-GC/push tests still running after 60 seconds. Other logs have only a target-level TIMEOUT. An elapsed-time notice is not a backtrace, and these failures cannot all be attributed to the sampled database locks.

Future macOS termination reports are retained as Bazel test outputs by `tools/test-sandbox/capture_stacks.py`; see the test-sandbox README for the artifact layout and capture limitations.
