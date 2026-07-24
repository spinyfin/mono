//! Split out of `completion.rs`'s `#[cfg(test)] mod tests`.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).

use super::*;

#[test]
fn tail_snippet_collapses_whitespace_and_keeps_tail() {
    // Short text passes through, single-lined.
    assert_eq!(tail_snippet("hello world", 200), "hello world");
    assert_eq!(tail_snippet("a\n\nb   c", 200), "a b c");
    // Empty / whitespace-only → explicit marker, never a bare "".
    assert_eq!(tail_snippet("", 200), "(empty)");
    assert_eq!(tail_snippet("   \n  ", 200), "(empty)");
    // Over-length is truncated to the TAIL (the marker would be at the end
    // of a triage message) with a leading ellipsis.
    let long = "x".repeat(50);
    let snippet = tail_snippet(&long, 10);
    assert_eq!(snippet, format!("…{}", "x".repeat(10)));
    assert!(snippet.starts_with('…'));
}

#[test]
fn triage_no_decision_detail_distinguishes_transcript_states() {
    // The "spoke but no marker" case keeps the stable prefix (so existing
    // greps match) and appends the agent's actual final words.
    let spoke = triage_no_decision_detail(&TriageTranscript::FinalMessage(
        "I looked around and decided to open a PR instead.".to_owned(),
    ));
    assert!(spoke.starts_with("triage ended without a decision marker"));
    assert!(spoke.contains("open a PR instead"));

    // The other states each get their own actionable phrasing — and must
    // NOT masquerade as "ended without a decision marker".
    let no_path = triage_no_decision_detail(&TriageTranscript::NoPath);
    assert!(no_path.contains("no transcript"));
    assert!(!no_path.contains("without a decision marker"));

    let unreadable = triage_no_decision_detail(&TriageTranscript::Unreadable);
    assert!(unreadable.contains("could not be read"));

    // A transcript that never materialised any content vs. one that
    // recorded activity (tool calls / thinking) but no prose are
    // distinct conditions — the field incident's conflated string
    // ("contained no assistant message (worker emitted no prose before
    // stopping)") asserted the second even when the transcript hadn't
    // been fully flushed, so the two must now read differently.
    let no_events = triage_no_decision_detail(&TriageTranscript::NoAssistantText { event_count: 0 });
    assert!(no_events.contains("no events at all"));

    let no_prose = triage_no_decision_detail(&TriageTranscript::NoAssistantText { event_count: 12 });
    assert!(no_prose.contains("12 event"));
    assert!(no_prose.contains("no prose"));
    assert_ne!(no_events, no_prose);
}

#[test]
fn triage_transcript_into_message_only_yields_final_message() {
    assert_eq!(
        TriageTranscript::FinalMessage("hi".to_owned()).into_message(),
        Some("hi".to_owned())
    );
    assert_eq!(TriageTranscript::NoPath.into_message(), None);
    assert_eq!(TriageTranscript::Unreadable.into_message(), None);
    assert_eq!(
        TriageTranscript::NoAssistantText { event_count: 12 }.into_message(),
        None
    );
}

#[test]
fn recover_skip_reason_fires_only_on_a_clean_conclusion() {
    // A plain "nothing to do" conclusion with no deferral → recover as skip.
    let clean = TriageTranscript::FinalMessage(
        "I checked the crate: no compiler or clippy warnings. The code is clean, nothing to do.".to_owned(),
    );
    assert!(
        recover_skip_reason(&TriageDecision::NoDecision, &clean).is_some(),
        "a plain clean-repo conclusion should recover as a skip",
    );

    // The exact field-evidence mid-verification tails must NOT recover — the
    // worker never decided, so the run has to stay failed_will_retry.
    for tail in [
        "The authoritative checkleft run is in progress. Let me wait for it to complete.",
        "I'll wait for checkleft to finish before deciding.",
        "Let me broaden the check to the whole repo to be thorough.",
        "Let me do one confirming check that clippy genuinely ran.",
    ] {
        let t = TriageTranscript::FinalMessage(tail.to_owned());
        assert_eq!(
            recover_skip_reason(&TriageDecision::NoDecision, &t),
            None,
            "must not skip-recover a mid-verification tail: {tail}",
        );
    }

    // No affirmative no-work signal → no recovery (ambiguous silence, not a skip).
    let vague = TriageTranscript::FinalMessage("I looked around the repository.".to_owned());
    assert_eq!(recover_skip_reason(&TriageDecision::NoDecision, &vague), None);

    // Non-FinalMessage transcript states carry no prose to conclude from.
    assert_eq!(
        recover_skip_reason(&TriageDecision::NoDecision, &TriageTranscript::NoPath),
        None
    );
    assert_eq!(
        recover_skip_reason(&TriageDecision::NoDecision, &TriageTranscript::Unreadable),
        None
    );
    assert_eq!(
        recover_skip_reason(
            &TriageDecision::NoDecision,
            &TriageTranscript::NoAssistantText { event_count: 12 }
        ),
        None
    );
}

#[test]
fn skip_recovery_scans_only_the_message_tail() {
    // "no warnings found" appears only in the HEAD, past the tail window;
    // the worker then kept reviewing and was cut off without concluding. The
    // early clean-sounding line must not leak into a false skip.
    let mut msg = String::from("Early note: no warnings found in the parser module. ");
    msg.push_str(&"Continuing to review the remaining crates for context. ".repeat(12));
    let t = TriageTranscript::FinalMessage(msg);
    assert_eq!(
        recover_skip_reason(&TriageDecision::NoDecision, &t),
        None,
        "a clean line in the head (outside the tail window) must not trigger skip-recovery",
    );
}

#[tokio::test]
async fn on_stop_finalizes_triage_skip_when_final_message_lands_after_a_flush_race() {
    // Regression test for the field incident: the Stop hook fired and
    // triggered `read_final_triage_message`'s first read within
    // milliseconds of the triage worker writing its final assistant-text
    // line (carrying `automation: skip — …`) to the transcript — before
    // that write had been flushed to disk. Without the retry-with-backoff
    // fix, that first read would see only the 12 pre-final-message events
    // (the exact field-evidence count: 1 user + 6 thinking + 5 tool
    // events) and permanently mis-finalise a correct skip decision as
    // `failed_will_retry`. This asserts the retry recovers the marker
    // once it lands and the run finalises `skipped` — zero retries, not
    // the failed/marker-recovery fallback path.
    let workspace = tempdir().unwrap();
    let (_dir, db, _automation_id, execution_id) = automation_triage_fixture(workspace.path());

    let transcript_path = workspace.path().join(format!("transcript-{execution_id}.jsonl"));
    let mut partial = String::new();
    partial.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "text", "text": "triage this repo for dead code"}]}
        })
    ));
    for _ in 0..6 {
        partial.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "thinking", "thinking": "considering whether this is really dead..."}]}
            })
        ));
    }
    for _ in 0..5 {
        partial.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "grep -r dead_code"}}]}
            })
        ));
    }
    std::fs::write(&transcript_path, partial.as_bytes()).unwrap();
    db.set_run_transcript_path_if_unset(&execution_id, transcript_path.to_str().unwrap())
        .unwrap();

    // Land the final assistant-text line a few milliseconds after Stop
    // fires — inside the retry window's ~300ms budget but after the very
    // first (would-be-losing) read, reproducing the flush race.
    let flush_path = transcript_path.clone();
    let flush_handle = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let final_line = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": "automation: skip — only dead_code found is \
                    intentionally-annotated placeholder state for planned work; no cheaply-confirmable \
                    removable dead code"}]}
            })
        );
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&flush_path)
            .await
            .unwrap();
        file.write_all(final_line.as_bytes()).await.unwrap();
    });

    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    flush_handle.await.unwrap();

    match &outcome {
        StopOutcome::AutomationTriage { outcome } => assert_eq!(outcome, AUTOMATION_OUTCOME_SKIPPED),
        other => panic!("expected a clean skip outcome recovered from the flush race, got {other:?}"),
    }

    let run = db
        .automation_run_for_triage_execution(&execution_id)
        .unwrap()
        .expect("automation run row should exist");
    assert_eq!(run.outcome, AUTOMATION_OUTCOME_SKIPPED);
    let detail = run.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("only dead_code found"),
        "detail should carry the worker's actual skip reason, got {detail:?}",
    );
    // The parsed `automation: skip` marker path, not the
    // failed_will_retry / marker-recovery fallback.
    assert!(
        !detail.contains("marker-recovery"),
        "should finalise via the direct marker path, not recovery: {detail:?}",
    );
}

#[tokio::test]
async fn on_stop_finalizes_answer_agent_with_no_reply_as_failed_and_answered() {
    // Regression test for the "stranded running run" edge case: the
    // agent's session ended (Stop fired) without ever calling
    // `CommentsPostAnswer` to post a reply. `finalize_answer_agent` must
    // mark the still-`running` run 'failed', post an apology thread
    // entry, and force the comment `answering -> answered` so it doesn't
    // sit unanswered forever.
    let workspace = tempdir().unwrap();
    let (_dir, db, comment_id, run_id, execution_id) = answer_agent_fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AnswerAgent { replied: false }),
        "expected AnswerAgent {{ replied: false }}, got {outcome:?}",
    );

    let run = db
        .get_answer_agent_run(&run_id)
        .unwrap()
        .expect("run should still exist");
    assert_eq!(run.status, ANSWER_AGENT_RUN_STATUS_FAILED);
    assert_eq!(run.error_kind.as_deref(), Some("no_reply_posted"));

    let comment = db
        .get_comment(&comment_id)
        .unwrap()
        .expect("comment should still exist");
    assert_eq!(comment.status, "answered");

    let entries = db.list_comment_thread_entries(&comment_id).unwrap();
    assert_eq!(entries.len(), 1, "an apology thread entry should have been posted");
    assert_eq!(entries[0].entry_kind, THREAD_ENTRY_KIND_ANSWER);

    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "the cube lease must still be released even on the no-reply path",
    );
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
}

#[tokio::test]
async fn on_stop_finalizes_answer_agent_already_replied_mid_session() {
    // Happy-path counterpart: the reply was already posted via
    // `CommentsPostAnswer` mid-session (run -> 'replied', comment ->
    // 'answered') before Stop ever fired. `finalize_answer_agent` must
    // detect there's no longer a `running` run and leave that state
    // alone — it should only finalise the execution/run rows and
    // release resources.
    let workspace = tempdir().unwrap();
    let (_dir, db, comment_id, run_id, execution_id) = answer_agent_fixture(workspace.path());
    db.complete_answer_agent_run(
        &run_id,
        boss_protocol::ANSWER_AGENT_RUN_STATUS_REPLIED,
        Some("here's the answer"),
        None,
    )
    .unwrap();
    db.transition_comment_to_answered(&comment_id).unwrap();

    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), detector);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AnswerAgent { replied: true }),
        "expected AnswerAgent {{ replied: true }}, got {outcome:?}",
    );

    let run = db
        .get_answer_agent_run(&run_id)
        .unwrap()
        .expect("run should still exist");
    assert_eq!(
        run.status,
        boss_protocol::ANSWER_AGENT_RUN_STATUS_REPLIED,
        "the already-replied run must not be touched by the finalizer",
    );
    let comment = db
        .get_comment(&comment_id)
        .unwrap()
        .expect("comment should still exist");
    assert_eq!(comment.status, "answered");

    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
}

#[tokio::test]
async fn pr_detected_moves_work_item_to_in_review_and_releases_lease() {
    let workspace = tempdir().unwrap();
    let (_dir, db, product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let outcome = handler.on_stop(&execution_id).await;

    // P992 task 7: chore_implementation now enqueues a reviewer and holds
    // the task in `active` until the reviewer resolves.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "expected ReviewerEnqueued; got {outcome:?}",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            // Task is held in `active` (not advanced to `in_review`) while
            // the independent reviewer pass runs.
            assert_eq!(t.status, TaskStatus::Active);
            // pr_url IS stamped so the reviewer can find the PR.
            assert_eq!(t.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.cube_lease_id.is_none());
    assert!(execution.workspace_path.is_none());
    assert!(execution.finished_at.is_some());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "the engine must release the cube lease so the next dispatch can take it",
    );
    let publisher_events = publisher.publish_calls.lock().await.clone();
    assert!(
        publisher_events
            .iter()
            .any(|(_, _, _, reason)| reason == "worker_pr_completed"),
        "expected worker_pr_completed execution event, got {publisher_events:?}",
    );
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events
            .iter()
            .any(|(p, w, reason)| p == &product_id && w == &chore_id && reason == "worker_pr_completed"),
        "expected work-item invalidation for the chore, got {work_events:?}",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "pane teardown must fire on PR completion so the libghostty slot returns to Free",
    );
    assert!(
        probes.snapshot().is_empty(),
        "fresh-PR completion must NOT queue a probe — the worker is done",
    );
}

#[tokio::test]
async fn on_stop_uses_staged_pr_url_and_skips_detector() {
    // Primary path: the worker ran `gh pr create` mid-run, the
    // events-socket dispatcher captured the URL into the staging
    // cache, the worker did more work, then stopped. On Stop the
    // handler must:
    //   1. read the staged URL,
    //   2. NOT invoke the detector (jj+gh reconstruction),
    //   3. transition the work item to `in_review` with the
    //      staged URL bound,
    //   4. release the lease + pane.
    //
    // The detector is wired with a deliberately-wrong URL so any
    // accidental fall-through to the cold path would be visible
    // as a wrong pr_url on the work item.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/458");

    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.on_stop(&execution_id).await;
    // P992 task 7: chore_implementation holds the task and enqueues reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url }
            if pr_url == "https://github.com/spinyfin/mono/pull/458"),
        "expected ReviewerEnqueued with staged URL, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "the staged-URL short-circuit must skip the detector entirely (this is the whole point — no jj log, no gh api commits/{{sha}}/pulls)",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            // Held in `active` while reviewer runs; pr_url is stamped.
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(
                t.pr_url.as_deref(),
                Some("https://github.com/spinyfin/mono/pull/458"),
                "the chore must bind to the STAGED URL, not the detector's wrong URL",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // Cache is cleared after a successful transition so a repeat
    // Stop on the same execution wouldn't re-fire transition logic
    // against a stale entry.
    assert!(
        staged_pr_urls.get(&execution_id).is_none(),
        "staging cache must be cleared after the successful transition",
    );
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "lease release must still fire on the primary path",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "pane teardown must still fire on the primary path",
    );
    assert!(
        probes.snapshot().is_empty(),
        "fresh-PR completion must not queue a probe",
    );
}

#[tokio::test]
async fn on_stop_with_no_staged_url_still_falls_back_to_detector() {
    // Regression test for the cold path. After this PR ships,
    // the staged-URL shortcut handles 99% of cases, but the
    // detector path remains as engine-restart recovery (if the
    // engine restarted between `gh pr create` and Stop, the
    // staging cache is empty here and we must still find the
    // PR through the legacy jj+gh path).
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));

    // No `with_staged_pr_urls` call — handler uses the default
    // empty cache. The detector must be invoked.
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let outcome = handler.on_stop(&execution_id).await;
    // P992 task 7: chore_implementation holds the task and enqueues reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "expected ReviewerEnqueued; got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        1,
        "with no staged URL, the detector is the only way to bind — it must be called",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/12"));
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn recheck_for_pr_uses_staged_pr_url_and_skips_detector() {
    // Merge-poller mirror: if the on-Stop path missed staging
    // (e.g. PostToolUse arrived after Stop in the wrong order
    // because of socket reordering, or the engine restarted),
    // the merge poller's `recheck_for_pr` sweep is the second
    // chance to find the URL. Same shortcut applies — if the
    // dispatcher staged a URL between Stop and now, recheck
    // uses it without the detector.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::err("jj broken");

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/458");

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    // Detector intentionally returns Err — if recheck called it,
    // recheck would surface `DetectorFailed`. With the staged
    // shortcut, recheck must succeed without ever touching the
    // detector.
    let outcome = handler.recheck_for_pr(&execution_id).await;
    // P992 (regression fix): chore_implementation advances to in_review and
    // enqueues an async reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url }
            if pr_url == "https://github.com/spinyfin/mono/pull/458"),
        "expected ReviewerEnqueued from recheck via staged URL, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "recheck must skip the detector when a staged URL is present",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/458"),);
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn recheck_for_pr_staged_url_rejected_on_branch_mismatch() {
    // T520 / T523 regression: a staged URL whose PR belongs to a
    // different execution's branch must be silently dropped, and the
    // recheck must fall through to the cold-path detector (which
    // sees no PR for the correct branch) rather than incorrectly
    // advancing the work item to in_review.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // Detector returns None → this execution has no PR yet.
    let detector = StubPrDetector::ok(None);

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/579");

    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        // PR #579 belongs to a DIFFERENT execution's branch — simulate
        // the mismatch that killed T520's worker.
        .with_branch_verifier(StubBranchVerifier::ok("boss/exec_some_other_exec_id"));

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "branch mismatch must drop the staged URL and fall through to cold path; got {outcome:?}",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "branch-mismatched PR must NOT advance the chore to in_review",
            );
            assert!(t.pr_url.is_none(), "branch-mismatched PR must not bind pr_url");
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // Staged URL must be cleared after mismatch so the next sweep
    // doesn't re-evaluate the same wrong URL.
    assert!(
        staged_pr_urls.get(&execution_id).is_none(),
        "mismatched staged URL must be evicted from the cache",
    );
    assert_eq!(
        cube.release_calls.lock().await.len(),
        0,
        "branch mismatch must NOT release the cube lease",
    );
}

#[tokio::test]
async fn on_stop_staged_url_rejected_on_branch_mismatch() {
    // Defence-in-depth: the on_stop path applies the same branch
    // check as recheck_for_pr. A staged URL for a different execution's
    // PR must be dropped and fall through to the cold-path detector.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // Detector returns None → no real PR for this execution's branch.
    let detector = StubPrDetector::ok(None);

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/458");

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok("boss/exec_completely_different_id"));

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "branch-mismatched staged URL must not advance to in_review; got {outcome:?}",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "wrong-branch PR must NOT move chore to in_review",
            );
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(
        staged_pr_urls.get(&execution_id).is_none(),
        "mismatched staged URL must be cleared from the cache",
    );
}

/// Regression for the 2026-06-09 incident (exec_18b7882532b66c00_14b):
/// a chore worker pushed a branch, opened a PR, and ended its turn at a
/// clean Stop boundary. The engine processed none of it: the PR was never
/// detected, the row never left `active`, and the worker session was never
/// released.
///
/// Root cause: when the staged URL's branch verification call failed
/// transiently (GitHub API error), the staged URL was discarded. If the
/// cold-path detector also failed (same transient window), the engine
/// returned `DetectorFailed` with no probe, no attention item, and no
/// retry. Subsequent merge-poller `recheck_for_pr` sweeps had no staged
/// URL to retry and kept returning quietly if `detect_pr` also returned
/// `None`, leaving the worker at `waiting_for_input` indefinitely.
///
/// The fix: do NOT discard the staged URL on a transient verification
/// error — only on a definitive branch-name mismatch. This ensures the
/// URL survives for the next sweep to retry.
#[tokio::test]
async fn on_stop_staged_url_preserved_when_branch_verification_errors() {
    // Verifier returns a transient error; detector returns the correct
    // PR so we can confirm the cold-path ran this turn AND the staged URL
    // is still in the cache for the next sweep's retry.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/1449"));
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/1449");

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector.clone(),
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_staged_pr_urls(staged_pr_urls.clone())
    // Verification errors — simulates the transient GitHub API failure
    // that caused the 2026-06-09 incident.
    .with_branch_verifier(StubBranchVerifier::err("GitHub API timeout"))
    // Bypass the reviewer so on_stop goes straight to in_review (not
    // PendingReview), making the transition observable in one step.
    .with_max_review_cycles(0);

    let outcome = handler.on_stop(&execution_id).await;
    // The cold-path detector was able to find the PR this turn and
    // advanced the task even though branch verification errored.
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url }
            if pr_url == "https://github.com/spinyfin/mono/pull/1449"),
        "cold path must still run after a transient verification error; got {outcome:?}",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "chore must advance to in_review");
            assert_eq!(
                t.pr_url.as_deref(),
                Some("https://github.com/spinyfin/mono/pull/1449"),
                "pr_url must be bound",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // The staged URL is cleared once finalization succeeds (not
    // prematurely on verification error).
    assert!(
        staged_pr_urls.get(&execution_id).is_none(),
        "staged URL must be cleared after successful finalization",
    );
    // Slot must be freed.
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "pane must be torn down",
    );
}

/// Regression test for the happy path: worker opens a PR and stops →
/// chore advances to `in_review` with `pr_url` bound and the worker slot
/// freed. Covers the direct `in_review` transition (reviewer bypassed via
/// `max_review_cycles = 0`) to keep the assertion simple and focused on
/// the fundamental Stop → in_review invariant.
#[tokio::test]
async fn on_stop_chore_pr_opened_advances_to_in_review_slot_freed() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // Cold-path detector is not reached (staged URL takes the primary
    // path), but wire it with a wrong URL so any accidental fall-through
    // would produce an observable wrong `pr_url`.
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/1449");

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector.clone(),
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_staged_pr_urls(staged_pr_urls.clone())
    .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
        &execution_id,
        &BranchNaming::BossExecPrefix,
        None,
    )))
    // Bypass reviewer: go directly to in_review so the transition is
    // visible without a second execution.
    .with_max_review_cycles(0);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url }
            if pr_url == "https://github.com/spinyfin/mono/pull/1449"),
        "worker that opened a PR and stopped must produce PrDetected; got {outcome:?}",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "chore must be in_review");
            assert_eq!(
                t.pr_url.as_deref(),
                Some("https://github.com/spinyfin/mono/pull/1449"),
                "pr_url must match the staged URL",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // Staged URL is cleared once DB write succeeds.
    assert!(
        staged_pr_urls.get(&execution_id).is_none(),
        "staged URL must be cleared after successful finalization",
    );
    // Slot freed.
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube lease must be released on successful PR completion",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "pane must be torn down on successful PR completion",
    );
    // No probe queued — the PR was already detected.
    assert!(
        probes.snapshot().is_empty(),
        "no probe must be queued when the PR is already open",
    );
}

/// Mirror of `on_stop_staged_url_preserved_when_branch_verification_errors`
/// for the merge-poller `recheck_for_pr` path: a transient verification
/// error must not evict the staged URL between sweeps.
#[tokio::test]
async fn recheck_staged_url_preserved_when_branch_verification_errors() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/1449"));
    let cube = Arc::new(StubCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let pane = Arc::new(RecordingPaneReleaser::default());
    let probes = Arc::new(RecordingProbeQueuer::default());

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/1449");

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector.clone(),
        cube.clone(),
        publisher.clone(),
        pane.clone(),
        probes.clone(),
    )
    .with_staged_pr_urls(staged_pr_urls.clone())
    .with_branch_verifier(StubBranchVerifier::err("transient API failure"))
    .with_max_review_cycles(0);

    let outcome = handler.recheck_for_pr(&execution_id).await;
    // Cold path picked up the PR even though verification errored.
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url }
            if pr_url == "https://github.com/spinyfin/mono/pull/1449"),
        "recheck must advance on cold-path hit after transient verification error; got {outcome:?}",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "chore must be in_review after recheck");
            assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/1449"),);
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "recheck must release the cube lease on success",
    );
}

#[tokio::test]
async fn on_stop_staged_url_associates_prefix_divergent_branch() {
    // Issue #1145 regression: a worker that honoured a product
    // `worker_branch_prefix` (e.g. `bduff/`) opened its PR on
    // `bduff/<exec-id>`, while the engine reconstructs
    // `boss/<exec-id>` as the expected branch. The work-item suffix
    // (`exec_<id>`) is identical, so the staged URL MUST associate —
    // the whole point of the fix is that the worker no longer has to
    // close a compliant `bduff/` PR and recreate it under `boss/`.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // Cold-path detector wired with a wrong URL: any fall-through
    // would surface as a wrong pr_url, proving the staged URL was
    // (incorrectly) dropped.
    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));

    let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, "https://github.com/spinyfin/mono/pull/458");

    // The expected branch is `boss/<exec-id>` (BossExecPrefix), but
    // the PR's head branch is `bduff/<exec-id>` — same suffix, only
    // the prefix differs.
    let expected = expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None);
    let suffix = branch_work_item_suffix(&expected);
    let divergent_branch = format!("bduff/{suffix}");
    assert_ne!(
        divergent_branch, expected,
        "test must exercise a real prefix divergence"
    );

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&divergent_branch));

    let outcome = handler.on_stop(&execution_id).await;
    // P992 (regression fix): chore_implementation advances to in_review and
    // enqueues an async reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { ref pr_url }
            if pr_url == "https://github.com/spinyfin/mono/pull/458"),
        "prefix-divergent but suffix-matching PR must associate; got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "the staged URL must be accepted (suffix match) and the detector skipped",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(
                t.pr_url.as_deref(),
                Some("https://github.com/spinyfin/mono/pull/458"),
                "the chore must bind to the staged `bduff/` PR URL",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn pr_absent_publishes_awaiting_pr_and_queues_probe() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let outcome = handler.on_stop(&execution_id).await;

    assert_eq!(outcome, StopOutcome::AwaitingInput);
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active, "no PR must NOT move to in_review");
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
    assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no PR must NOT release the cube workspace",
    );
    let events = publisher.publish_calls.lock().await.clone();
    assert!(
        events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
        "expected worker_awaiting_pr event for the no-PR case, got {events:?}",
    );
    assert!(pane.calls.lock().await.is_empty(), "no PR must NOT release the pane",);
    let queued = probes.snapshot();
    assert_eq!(
        queued.len(),
        1,
        "exactly one probe must be queued when the worker stops without a PR, got {queued:?}",
    );
    assert_eq!(queued[0].0, execution_id);
    assert_eq!(queued[0].1, PROBE_NO_PR);
}

#[tokio::test]
async fn stale_pr_publishes_awaiting_pr_and_queues_push_probe() {
    // PR exists but local commits are ahead of the PR's head sha.
    // The work item must NOT move to in_review, the lease must
    // stay held, and the worker gets probed to push the missing
    // commits so the next Stop sees a fresh PR.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok_status(PrStatus::Stale {
        url: "https://github.com/foo/bar/pull/42".into(),
        reason: "local HEAD abcd1234 is ahead of PR head 9876fedc".into(),
    });

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let outcome = handler.on_stop(&execution_id).await;

    match outcome {
        StopOutcome::StalePr { pr_url, .. } => {
            assert_eq!(pr_url, "https://github.com/foo/bar/pull/42");
        }
        other => panic!("expected StalePr, got {other:?}"),
    }
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "stale PR must NOT move the work item to in_review",
            );
            assert!(t.pr_url.is_none(), "stale PR must NOT stamp pr_url yet");
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
    assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());

    let events = publisher.publish_calls.lock().await.clone();
    assert!(
        events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
        "stale PR must publish worker_awaiting_pr, got {events:?}",
    );
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1, "expected one probe, got {queued:?}");
    assert_eq!(queued[0].1, PROBE_STALE_PR);
}

#[tokio::test]
async fn detector_failure_does_not_probe_worker() {
    // A transient `gh`/network failure must NOT inject a probe into the
    // worker pane.  Probing on detector failure creates a re-entrancy
    // loop: worker responds → stops → detection fails again → probe
    // again → …  The merge-poller recheck recovers the transition once
    // the failure clears.
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::err("gh broken");

    let TestHarness {
        handler,
        cube,
        pane,
        probes,
        ..
    } = TestHarness::new(db, detector);
    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(outcome, StopOutcome::DetectorFailed);
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
    let queued = probes.snapshot();
    assert!(
        queued.is_empty(),
        "detector failure must NOT probe the worker, got {queued:?}",
    );
}

#[tokio::test]
async fn unknown_execution_is_a_noop() {
    let detector = StubPrDetector::ok(Some("https://github.com/x/y/pull/1"));
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db, detector);
    let outcome = handler.on_stop("not-an-execution").await;
    assert_eq!(outcome, StopOutcome::UnknownExecution);
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
    assert!(publisher.publish_calls.lock().await.is_empty());
    assert!(probes.snapshot().is_empty(), "unknown executions must NOT queue probes",);
}

#[tokio::test]
async fn force_release_releases_pane_and_cube_lease_then_idempotent() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());

    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));

    handler.force_release(&execution_id).await;

    // First call: pane fired, cube release fired exactly once.
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(execution.cube_lease_id.is_none());
    assert!(execution.workspace_path.is_none());

    // Second call: idempotent — no second cube release. The pane
    // releaser is invoked again here (the registry-level
    // idempotency lives in `WorkerRegistry::take_slot_for_run`),
    // but no extra cube release happens because the lease columns
    // are already cleared.
    handler.force_release(&execution_id).await;
    assert_eq!(
        cube.release_calls.lock().await.len(),
        1,
        "cube release must fire only once across duplicate force_release calls",
    );
}

#[tokio::test]
async fn force_release_no_lease_skips_cube_release() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());

    // Pre-clear the lease so force_release can confirm it skips
    // cube release when there's nothing to release.
    db.clear_execution_workspace(&execution_id).unwrap();

    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));

    handler.force_release(&execution_id).await;
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    assert!(cube.release_calls.lock().await.is_empty());
}

/// T981 regression — the lease-release gate. When the pane releaser
/// reports `NoLiveWorker` (the worker is still mid-spawn: no slot
/// mapped, no pid to reap), `force_release` must NOT free the cube
/// lease. Freeing it would hand a workspace the worker is about to
/// occupy back to cube, which re-leases it into a same-workspace
/// collision. The lease stays held until the in-flight run reaps the
/// worker and releases it.
#[tokio::test]
async fn force_release_mid_spawn_holds_cube_lease() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());
    let pane = Arc::new(RecordingPaneReleaser::with_outcome(PaneReleaseOutcome::NoLiveWorker));
    let TestHarness { handler, cube, .. } = TestHarness::with_pane(db.clone(), StubPrDetector::ok(None), pane.clone());

    handler.force_release(&execution_id).await;

    // Pane release was attempted...
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    // ...but the cube lease was NOT released, and the row still
    // carries it — the still-occupied workspace stays leased.
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "mid-spawn force_release must not release the cube lease",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.cube_lease_id.as_deref(),
        Some("lease-1"),
        "lease columns must stay set so the in-flight run owns the eventual release",
    );
    assert_eq!(execution.workspace_path.as_deref(), workspace.path().to_str());
}

/// T981 regression — `cancel_and_release` racing the spawn window.
/// It cancels the execution row (so the reconciler won't redispatch)
/// but, with the worker still mid-spawn, must leave the lease held —
/// the in-flight run reaps + releases once its spawn settles.
#[tokio::test]
async fn cancel_and_release_mid_spawn_cancels_row_but_holds_lease() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let pane = Arc::new(RecordingPaneReleaser::with_outcome(PaneReleaseOutcome::NoLiveWorker));
    let TestHarness { handler, cube, .. } = TestHarness::with_pane(db.clone(), StubPrDetector::ok(None), pane.clone());

    handler
        .cancel_and_release(&chore_id, &execution_id, "test: mid-spawn cancel")
        .await;

    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Cancelled,
        "the execution row must be cancelled so the reconciler won't redispatch it",
    );
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "the lease must stay held while the worker is still occupying the workspace",
    );
    assert_eq!(
        execution.cube_lease_id.as_deref(),
        Some("lease-1"),
        "lease columns must remain so the in-flight run can release after reaping",
    );
}

/// Companion to the gate test: when a live worker WAS reaped
/// (`Reaped`), `cancel_and_release` releases the lease as before — the
/// gate only defers on the mid-spawn case.
#[tokio::test]
async fn cancel_and_release_with_live_worker_releases_lease() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());

    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));

    handler
        .cancel_and_release(&chore_id, &execution_id, "test: live worker cancel")
        .await;

    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Cancelled);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert!(execution.cube_lease_id.is_none());
}

/// Regression guard for the "delete-while-doing" lifecycle bug: deleting a
/// work item while its worker is actively running must tear down the worker
/// fully — process kill, pool slot release, and cube lease release — not
/// just mark the DB row terminal (the T1089 cancel-without-reap failure
/// mode).
///
/// `handle_delete_work_item` detects a live execution and spawns
/// `cancel_and_release` on the completion handler. This test drives that
/// call directly and verifies all three observable side effects:
///
/// 1. `pane.calls` contains the execution id — `release_pane` was invoked,
///    which is the proxy for "OS process tree was signalled and the pool
///    slot was freed" (the production `release_worker_pane` calls
///    `reap_worker_process_tree` + `release_worker_and_kick` once
///    `release_pane` returns `Reaped`).
/// 2. `cube.release_calls` contains the lease id — the cube workspace
///    lease was freed so the workspace slot returns to the pool.
/// 3. `execution.status` == `Cancelled` — the DB row is terminal so the
///    orphan sweep and reconciler will not re-dispatch it.
#[tokio::test]
async fn delete_while_doing_teardown_releases_pane_frees_lease_and_cancels_execution() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture_running(workspace.path());

    let TestHarness {
        handler, cube, pane, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));

    // This is the call that handle_delete_work_item spawns when it detects
    // a live execution for a work item being deleted.
    handler
        .cancel_and_release(&chore_id, &execution_id, "work item deleted while a worker was active")
        .await;

    // 1. Pane releaser was invoked — the production implementation
    //    signals the OS process tree (SIGTERM → SIGKILL) and frees the
    //    worker pool slot via release_worker_and_kick.
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "release_pane must be called with the execution id so the OS process tree is signalled and the pool slot freed",
    );

    // 2. Cube workspace lease was released — the workspace is back in
    //    the pool for future dispatches and the slot is not permanently
    //    occupied.
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "cube workspace lease must be released so the pool slot is not permanently occupied",
    );

    // 3. Execution row is terminal — the orphan sweep and reconciler
    //    will not re-dispatch the deleted work item.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Cancelled,
        "execution must be marked Cancelled so the reconciler never re-dispatches the deleted item",
    );
    assert!(
        execution.cube_lease_id.is_none(),
        "cube_lease_id must be cleared from the execution row after the lease is released",
    );
}

/// Regression guard for the "delete-while-dispatching" lifecycle bug:
/// a delete that lands while the row's execution is still mid-dispatch
/// (workspace leased, worker pane not yet up) must not let that dispatch
/// complete into a zombie. `handle_delete_work_item` still spawns
/// `cancel_and_release` here exactly as it does for the delete-while-doing
/// case above; the pane releaser reports `NoLiveWorker` because no slot
/// is mapped yet, which is the T981 gate: releasing the cube lease now
/// would hand a workspace the in-flight spawn is about to occupy back to
/// cube, causing a same-workspace collision the moment it lands. So the
/// execution row is cancelled immediately (the reconciler will never
/// re-dispatch it) while the lease is deliberately left held for the
/// in-flight `run_execution` to reap and release once its spawn settles
/// — proven end-to-end by
/// `runner::tests::run_execution_reaps_and_signals_when_cancelled_mid_spawn`,
/// which shows that same cancelled row's spawn coming back reaped with
/// its pool slot freed, and by the coordinator's `CancelledDuringSpawn`
/// branch, which releases the lease this test leaves held. Together the
/// three cover the full delete-while-dispatching path ending with no
/// live process, no held lease, and no held slot.
#[tokio::test]
async fn delete_while_dispatching_cancels_row_but_holds_lease_for_inflight_spawn() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture_running(workspace.path());
    let pane = Arc::new(RecordingPaneReleaser::with_outcome(PaneReleaseOutcome::NoLiveWorker));
    let TestHarness { handler, cube, .. } = TestHarness::with_pane(db.clone(), StubPrDetector::ok(None), pane.clone());

    // This is the call that handle_delete_work_item spawns when it
    // detects a live (here: mid-dispatch) execution for a work item
    // being deleted.
    handler
        .cancel_and_release(&chore_id, &execution_id, "work item deleted while a worker was active")
        .await;

    // The row is terminal immediately — the orphan sweep and
    // reconciler will not re-dispatch the deleted work item no matter
    // how long the in-flight spawn takes to settle.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        ExecutionStatus::Cancelled,
        "execution must be marked Cancelled so the reconciler never re-dispatches the deleted item",
    );

    // Pane release was attempted (it is what would reap a worker that
    // had already come up)...
    assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    // ...but with no live worker found yet, the cube lease must stay
    // held rather than being handed back to the pool out from under
    // the in-flight spawn.
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "a delete landing mid-dispatch must not release the lease before the in-flight spawn settles",
    );
    assert_eq!(
        execution.cube_lease_id.as_deref(),
        Some("lease-1"),
        "lease columns must remain so the in-flight run can reap the worker and release the lease itself",
    );
}

#[tokio::test]
async fn duplicate_stop_after_pr_detection_is_idempotent() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector);

    // P992 task 7: first Stop enqueues reviewer and holds task in active.
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::ReviewerEnqueued { .. }
    ));
    // A second Stop event for the same execution must NOT
    // duplicate work — release is called once, work item stays
    // pinned at `active` (pending review). The pane releaser is
    // invoked again here; production releasers must be idempotent
    // on their own (see `WorkerRegistry::take_slot_for_run`).
    assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::AlreadyTerminal,);
    assert_eq!(cube.release_calls.lock().await.len(), 1);
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn merged_pr_skips_in_review_and_moves_chore_to_done() {
    // The Stop arrives after the worker pushed AND the PR was
    // merged (e.g. fast-merge during the run). The detector
    // reports `Merged`; the chore must move directly to `done`
    // instead of `in_review`, the cube lease is released, and
    // the publish reason is `worker_pr_merged` so the frontend
    // can paint the right activity.
    let workspace = tempdir().unwrap();
    let (_dir, db, product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok_status(PrStatus::Merged {
        url: "https://github.com/foo/bar/pull/42".into(),
    });

    let TestHarness {
        handler,
        cube,
        publisher,
        probes,
        ..
    } = TestHarness::new(db.clone(), detector);
    let outcome = handler.on_stop(&execution_id).await;
    match outcome {
        StopOutcome::PrMerged { pr_url } => {
            assert_eq!(pr_url, "https://github.com/foo/bar/pull/42");
        }
        other => panic!("expected PrMerged, got {other:?}"),
    }
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Done, "merged-at-stop must skip in_review");
            assert_eq!(t.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"),);
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.cube_lease_id.is_none());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "merged-at-stop must still release the cube lease",
    );
    let publisher_events = publisher.publish_calls.lock().await.clone();
    assert!(
        publisher_events
            .iter()
            .any(|(_, _, _, reason)| reason == "worker_pr_merged"),
        "expected worker_pr_merged execution event, got {publisher_events:?}",
    );
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events
            .iter()
            .any(|(p, w, reason)| p == &product_id && w == &chore_id && reason == "worker_pr_merged"),
        "expected work-item invalidation tagged worker_pr_merged, got {work_events:?}",
    );
    assert!(
        probes.snapshot().is_empty(),
        "merged-at-stop must NOT queue a probe — the worker is done",
    );
}

#[tokio::test]
async fn closed_unmerged_pr_treated_as_no_pr() {
    // PR was closed without merging — work shouldn't advance to
    // `in_review` or `done`. Behave like the no-PR case so the
    // worker is asked to confirm what they want.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok_status(PrStatus::Closed {
        url: "https://github.com/foo/bar/pull/9".into(),
    });
    let TestHarness {
        handler, cube, probes, ..
    } = TestHarness::new(db.clone(), detector);
    assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput);
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(cube.release_calls.lock().await.is_empty());
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].1, PROBE_NO_PR);
}

/// Build a `kind = 'conflict_resolution'` execution against a chore
/// that is currently `blocked: merge_conflict`. Also inserts the
/// matching `conflict_resolutions` row in `running` so the
/// completion finalizer has something to look up. Mirrors the
#[tokio::test]
async fn non_conflict_kind_execution_does_not_invoke_finalizer() {
    // The standard chore_implementation kind must NOT trip the
    // conflict-resolution finalizer even if a conflict_resolutions
    // row happens to exist for the same work item (e.g. a prior
    // attempt was archived).
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    // Pre-existing failed attempt unrelated to this execution.
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: "any".into(),
            work_item_id: chore_id.clone(),
            pr_url: "https://github.com/foo/bar/pull/99".into(),
            pr_number: 99,
            head_branch: "x".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("bsha".into()),
            head_sha_before: None,
        })
        .unwrap()
        .unwrap();
    db.mark_conflict_resolution_running(&attempt.id, "lease-x", "ws-x", "worker-x")
        .unwrap();
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let _ = handler.on_stop(&execution_id).await;

    // The chore_implementation execution must not touch the
    // sibling conflict_resolutions row; if it did, the attempt
    // would now be `failed` instead of `running`.
    let after = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(
        after.status, "running",
        "non-conflict-kind executions must not trip the conflict-resolution finalizer",
    );
}

#[tokio::test]
async fn cross_execution_attribution_uses_per_execution_branch_name() {
    let alice_ws = tempdir().unwrap();
    let bob_ws = tempdir().unwrap();
    let (_dir, db, _alice_product, _alice_chore, alice_exec) = fixture(alice_ws.path());
    // Fresh DB for Bob so the two executions are independent —
    // we're modelling them as living in different cube
    // workspaces, not contending for the same chore.
    let (_dir, bob_db, _bob_product, _bob_chore, bob_exec) = fixture(bob_ws.path());

    // Detector returns Fresh URLs unique per branch — the
    // production behaviour of `gh pr list --head <branch>` once
    // each worker has pushed.
    struct PerBranchDetector;
    #[async_trait]
    impl PrDetector for PerBranchDetector {
        async fn detect_pr(&self, _repo_remote_url: &str, expected_branch: &str) -> Result<PrStatus> {
            Ok(PrStatus::Fresh {
                url: format!("https://github.com/spinyfin/mono/pull/PR-for-{expected_branch}"),
            })
        }
    }
    let detector = Arc::new(PerBranchDetector);

    let alice_handler = TestHarness::new(db.clone(), detector.clone()).handler;
    let bob_handler = TestHarness::new(bob_db.clone(), detector).handler;

    let alice_outcome = alice_handler.on_stop(&alice_exec).await;
    let bob_outcome = bob_handler.on_stop(&bob_exec).await;

    // P992 task 7: chore_implementation holds task and enqueues reviewer.
    let alice_url = match alice_outcome {
        StopOutcome::ReviewerEnqueued { pr_url } => pr_url,
        other => panic!("alice expected ReviewerEnqueued, got {other:?}"),
    };
    let bob_url = match bob_outcome {
        StopOutcome::ReviewerEnqueued { pr_url } => pr_url,
        other => panic!("bob expected ReviewerEnqueued, got {other:?}"),
    };
    assert_ne!(
        alice_url, bob_url,
        "two concurrent workers in different workspaces must bind to different PRs — \
         the fan-out bug from incident 001 was exactly the case where they got the same one",
    );
    assert!(
        alice_url.contains(&expected_branch_name(&alice_exec, &BranchNaming::BossExecPrefix, None)),
        "alice's bound URL must derive from her own execution id, got {alice_url}",
    );
    assert!(
        bob_url.contains(&expected_branch_name(&bob_exec, &BranchNaming::BossExecPrefix, None)),
        "bob's bound URL must derive from his own execution id, got {bob_url}",
    );
}

/// Reused-workspace stale-Stop guard (the bug this change fixes):
/// two executions occupy the same warm-cached cube workspace. The
/// older is a stale prior occupant whose `boss-event` Stop hook
/// leaked from a settings.json left in the re-leased tree. Its Stop
/// must be ignored — not finalized, no lease released — while the
/// live (newest) execution's own Stop still completes it.
#[tokio::test]
async fn stale_stop_from_superseded_workspace_occupant_is_ignored() {
    let ws = tempdir().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);

    let ws_path = ws.path().to_str().unwrap();
    let (stale_chore, stale_exec) =
        seed_workspace_occupant(&db, &product.id, "stale", "lease-stale", "mono-agent-shared", ws_path);
    let (_live_chore, live_exec) =
        seed_workspace_occupant(&db, &product.id, "live", "lease-live", "mono-agent-shared", ws_path);

    // Force deterministic recency: the stale occupant created
    // earlier. (`created_at` is second-granularity, so two rows
    // created in the same test tick would otherwise tie.)
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '100' WHERE id = ?1",
            rusqlite::params![stale_exec],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '200' WHERE id = ?1",
            rusqlite::params![live_exec],
        )
        .unwrap();
    }

    let TestHarness { handler, cube, .. } = TestHarness::new(
        db.clone(),
        StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/910")),
    );

    // The stale occupant's leaked Stop must be ignored.
    assert_eq!(
        handler.on_stop(&stale_exec).await,
        StopOutcome::SupersededInWorkspace,
        "a stale Stop from a superseded reused-workspace occupant must be ignored",
    );
    // Its chore is not pushed to in_review and no lease is released.
    match db.get_work_item(&stale_chore).unwrap() {
        WorkItem::Chore(t) => assert_ne!(
            t.status,
            TaskStatus::InReview,
            "the stale occupant's task must not transition on a leaked Stop",
        ),
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "a leaked stale Stop must not release any cube lease",
    );

    // P992 task 7: the live execution's Stop enqueues a reviewer and
    // holds the live chore in active.
    assert!(
        matches!(handler.on_stop(&live_exec).await, StopOutcome::ReviewerEnqueued { .. }),
        "the live execution's own Stop must still complete it",
    );
}

#[tokio::test]
async fn running_status_short_circuits_without_calling_detector() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture_running(workspace.path());

    let detector = StubPrDetector::ok(Some("https://github.com/should/not/pull/999"));

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert_eq!(
        outcome,
        StopOutcome::RunningNoStagedPr,
        "running execution with no staged URL must short-circuit, not invoke the detector",
    );
    assert_eq!(detector.call_count(), 0, "running-status gate must not call detect_pr",);
    // Chore stays put, no probe queued, no publish.
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
    assert!(probes.snapshot().is_empty());
    assert!(publisher.publish_calls.lock().await.is_empty());
}

/// Companion to the running-status gate test: when the execution
/// IS in `waiting_human` (worker has paused and is awaiting human
/// review), the fallback fires. This is the only state in which
/// the cold path is allowed to run, per the incident-001 fix.
#[tokio::test]
async fn waiting_human_status_invokes_detector_with_expected_branch() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    // Fixture leaves the execution in `waiting_human`; the on-Stop
    // handler should fall through to the detector.
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/501"));

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let outcome = handler.on_stop(&execution_id).await;
    // P992 task 7: chore_implementation holds task and enqueues reviewer.
    assert!(
        matches!(outcome, StopOutcome::ReviewerEnqueued { .. }),
        "expected ReviewerEnqueued; got {outcome:?}",
    );
    assert_eq!(detector.call_count(), 1);
    let calls = detector.calls_snapshot();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].expected_branch,
        expected_branch_name(&execution_id, &BranchNaming::BossExecPrefix, None),
        "detect_pr must be invoked with the execution's deterministic branch name",
    );
}

/// Integration: the handler must publish `awaiting_pr` and queue
/// `PROBE_EMPTY_PR` when the detector reports `EmptyDiff`. The
/// work item must stay in `active` and the cube lease must NOT
/// be released — the worker is alive and must fix its PR first.
#[tokio::test]
async fn empty_diff_pr_publishes_awaiting_pr_and_queues_empty_pr_probe() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok_status(PrStatus::EmptyDiff {
        url: "https://github.com/foo/bar/pull/77".into(),
    });

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let outcome = handler.on_stop(&execution_id).await;

    match outcome {
        StopOutcome::EmptyDiffPr { pr_url } => {
            assert_eq!(pr_url, "https://github.com/foo/bar/pull/77");
        }
        other => panic!("expected EmptyDiffPr, got {other:?}"),
    }
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "empty-diff PR must NOT move the work item to in_review",
            );
            assert!(t.pr_url.is_none(), "empty-diff PR must NOT stamp pr_url");
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
    assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "empty-diff PR must NOT release the cube workspace",
    );
    assert!(
        pane.calls.lock().await.is_empty(),
        "empty-diff PR must NOT release the pane"
    );

    let events = publisher.publish_calls.lock().await.clone();
    assert!(
        events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
        "empty-diff PR must publish worker_awaiting_pr, got {events:?}",
    );
    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1, "expected one probe, got {queued:?}");
    assert_eq!(queued[0].0, execution_id);
    assert_eq!(queued[0].1, PROBE_EMPTY_PR);
}

/// End-to-end regression mirror of the user-reported symptom:
/// the chore description references multiple historical merged
/// PRs in narrative text, and the worker exits without pushing.
/// The detector returns `None` (because the structural head-sha
/// gate in `classify_pr` rejects the parent-on-main false
/// positive), and the on-Stop handler must therefore leave the
/// chore in `active` with `pr_url` unset — NOT transition it to
/// `done` against one of the PRs mentioned in the description.
///
/// We can't drive `classify_pr` from end-to-end here without a
/// real `gh`/`jj` install in the test harness, so we stub the
/// detector with `PrStatus::None` (the exact value the fixed
/// `classify_pr` now returns for the bug scenario) and assert on
/// the chore-and-execution state the handler is supposed to land
/// in. The description-text storm is preserved to make the
/// intent clear if this test ever has to be revisited.
#[tokio::test]
async fn chore_with_pr_references_in_description_stays_active_when_worker_exits_without_pr() {
    let workspace = tempdir().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let description_with_pr_refs = "\
This is a follow-up to PR #379 (the auto-bind safety net work). \
See #379 for context. We also referenced #379 in the design doc. \
PR #379 was reverted in #381; the structural fix from PR #379 should \
not be reintroduced as-is. Out-of-scope section of prior PR #379 \
applies. Discussion in PR #379 still relevant. PR #379. PR #379. \
PR #379. PR #379.";
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Engine PR-auto-bind regression returned")
                .description(description_with_pr_refs)
                .build(),
        )
        .unwrap();
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &execution.id, &run.id, Some("spawned worker pane"));

    // Worker exited without pushing — the (now-fixed) detector
    // returns `None` rather than misbinding to one of the PRs
    // mentioned in the description text.
    let detector = StubPrDetector::ok(None);

    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), detector);
    let outcome = handler.on_stop(&execution.id).await;
    assert_eq!(outcome, StopOutcome::AwaitingInput);

    let item = db.get_work_item(&chore.id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Active,
                "chore with PR refs in description must stay active when the worker exits without a PR",
            );
            assert!(
                t.pr_url.is_none(),
                "pr_url must NOT be stamped from description text — got {:?}",
                t.pr_url,
            );
            assert_ne!(
                t.last_status_actor, "engine",
                "engine must NOT be the last status actor when no PR was bound",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }

    let exec_after = db.get_execution(&execution.id).unwrap();
    assert_eq!(
        exec_after.status,
        ExecutionStatus::WaitingHuman,
        "execution must stay in waiting_human so a follow-up Stop can re-check",
    );
    assert_eq!(
        exec_after.cube_lease_id.as_deref(),
        Some("lease-1"),
        "cube lease must NOT be released when no PR was bound",
    );

    let queued = probes.snapshot();
    assert_eq!(queued.len(), 1, "expected one probe, got {queued:?}");
    assert_eq!(queued[0].1, PROBE_NO_PR);
}

/// Second half of the required coverage: a chore whose
/// description references multiple historical PRs, but the
/// worker actually pushes and creates a real PR. The detector
/// reports `Fresh { url }` for the worker's PR. The chore must
/// bind to *that* PR (the one the worker actually created), not
/// to any of the PRs mentioned in the description text.
#[tokio::test]
async fn chore_with_pr_references_in_description_binds_to_worker_created_pr_not_description_pr() {
    let workspace = tempdir().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    // Description points at PR #379 repeatedly as prior art. The
    // worker is going to actually create PR #500.
    let description_with_pr_refs = "\
Follow-up to PR #379. See PR #379. Reverted in #381. PR #379. PR #379. \
PR #379. PR #379. PR #379. PR #379. PR #379.";
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Engine PR-auto-bind regression returned")
                .description(description_with_pr_refs)
                .build(),
        )
        .unwrap();
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let (execution, run) = db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_waiting_human(&db, &execution.id, &run.id, Some("spawned worker pane"));

    // The worker DID create a real PR — number 500, freshly
    // opened. The (fixed) detector reports that fresh PR's url,
    // NOT any of the description-mentioned PRs.
    let workers_actual_pr = "https://github.com/spinyfin/mono/pull/500";
    let description_mentioned_pr = "https://github.com/spinyfin/mono/pull/379";
    let detector = StubPrDetector::ok(Some(workers_actual_pr));

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector);
    let outcome = handler.on_stop(&execution.id).await;
    // P992 task 7: chore_implementation holds task and enqueues reviewer.
    match outcome {
        StopOutcome::ReviewerEnqueued { pr_url } => {
            assert_eq!(
                pr_url, workers_actual_pr,
                "must bind to the worker-created PR, not the description-mentioned one",
            );
        }
        other => panic!("expected ReviewerEnqueued, got {other:?}"),
    }

    let item = db.get_work_item(&chore.id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(
                t.pr_url.as_deref(),
                Some(workers_actual_pr),
                "pr_url must be the worker's actual PR",
            );
            assert_ne!(
                t.pr_url.as_deref(),
                Some(description_mentioned_pr),
                "pr_url MUST NOT be one of the historical PRs the description mentions",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Regression for the missed PR-open detection that left chore
/// `task_18aefd1f955e5348_e` (PR #415) stuck in `active` with
/// `pr_url=NULL`. The on-Stop hook can miss a freshly-opened PR
/// when GitHub's `commits/{sha}/pulls` index hasn't caught up yet
/// (PR #415 was created 7s before the Stop fired). When that
/// happens the chore stays `active`, the merge poller's primary
/// query (`list_chores_pending_merge_check`) never picks it up
/// (that query gates on `status='in_review'`), and the chore is
/// stuck. The fix routes `waiting_human` executions with no
/// `pr_url` through `WorkerCompletionHandler::recheck_for_pr` on
/// every merge-poller pass, so a delayed GitHub-side propagation
/// recovers on the next 60s sweep.
#[tokio::test]
async fn merge_poller_recovers_missed_pr_open_for_waiting_human_execution() {
    use crate::merge_poller::{MergeProbe, OpenPrStatus, PrLifecycleProbe, PrLifecycleState};

    // Fixture leaves the chore in `active` and the execution in
    // `waiting_human` with a workspace_path — exactly the state
    // PR #415 was in after its on-Stop hook missed.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    // Simulate "on-Stop already ran and saw no PR" by leaving
    // the chore's pr_url unset. The recheck path is what we're
    // testing — it must see PrStatus::Fresh on this pass and
    // promote the chore.
    let workers_pr = "https://github.com/foo/bar/pull/415";
    let detector = StubPrDetector::ok(Some(workers_pr));
    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let handler = Arc::new(handler);

    // Wire a no-op MergeProbe — the test exercises only the
    // pending-PR-detection arm of the sweep, not the in-review
    // merge path.
    struct NoOpProbe;
    #[async_trait]
    impl MergeProbe for NoOpProbe {
        async fn probe(&self, _: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(String::new())
                .state(PrLifecycleState::Open(OpenPrStatus::clean()))
                .labels(Vec::new())
                .review(crate::merge_poller::PrReviewState::Unknown)
                .build())
        }
    }
    let probe = NoOpProbe;

    let outcome =
        crate::merge_poller::run_one_pass(db.as_ref(), &probe, publisher.as_ref(), None, Some(handler.as_ref())).await;

    assert_eq!(
        outcome.pr_recheck_recovered, 1,
        "the sweep must recover exactly one missed PR-open transition, got {outcome:?}",
    );

    // P992 task 7: chore held in `active` with pr_url stamped
    // (reviewer is enqueued to run the review pass).
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(t.pr_url.as_deref(), Some(workers_pr));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // Execution finalised — lease released, pane torn down.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.cube_lease_id.is_none());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "the recovery must release the cube lease just like the on-Stop path does",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "the recovery must tear down the pane just like the on-Stop path does",
    );
    // Crucially: NO probe was queued. Periodic polling must not
    // spam the worker's probe FIFO.
    assert!(
        probes.snapshot().is_empty(),
        "merge-poller recovery must NOT queue a probe — that's a Stop-event side effect only",
    );
    // Publish reason distinguishes recheck from on-Stop so
    // operators can see which path closed the chore.
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events.iter().any(|(_, _, r)| r == "worker_pr_completed_recheck"),
        "expected worker_pr_completed_recheck publish reason, got {work_events:?}",
    );
}

/// Periodic polling must NOT queue probes when the detector
/// still sees no PR — that's the side effect that makes the
/// no-PR branch of `on_stop` correct but the no-PR branch of a
/// 60s poll wrong (a Stop event happens once; a poll happens
/// every minute).
#[tokio::test]
async fn recheck_for_pr_is_quiet_when_detector_still_reports_no_pr() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db.clone(), detector);
    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert_eq!(outcome, StopOutcome::AwaitingInput);

    // Chore stays where it was.
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert!(t.pr_url.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // No probe queued, no awaiting-input event published, no
    // lease released, no pane torn down.
    assert!(
        probes.snapshot().is_empty(),
        "recheck must NOT queue probes on the no-PR branch",
    );
    assert!(
        publisher.publish_calls.lock().await.is_empty(),
        "recheck must NOT publish awaiting-input events on the no-PR branch",
    );
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
}

/// Sibling regression: when the detector returns PrStatus::Stale
/// (PR exists but local commits are ahead), the recheck must
/// also stay silent — the worker has stopped, so probing it
/// every 60s with PROBE_STALE_PR would spam its input FIFO.
#[tokio::test]
async fn recheck_for_pr_is_quiet_on_stale_pr() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok_status(PrStatus::Stale {
        url: "https://github.com/foo/bar/pull/42".into(),
        reason: "local HEAD ahead of PR head".into(),
    });

    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = TestHarness::new(db, detector);
    let outcome = handler.recheck_for_pr(&execution_id).await;
    match outcome {
        StopOutcome::StalePr { .. } => {}
        other => panic!("expected StalePr, got {other:?}"),
    }
    assert!(
        probes.snapshot().is_empty(),
        "recheck must NOT queue probes on the stale-PR branch",
    );
    assert!(publisher.publish_calls.lock().await.is_empty());
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
}
