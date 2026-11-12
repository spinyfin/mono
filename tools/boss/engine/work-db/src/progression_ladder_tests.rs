//! One test per transition of the escalation ladder (design 2.5), the
//! cross-fingerprint `worker_failed` regression with distinct terminal
//! execution ids, the three answers to the engine's question, and the report
//! buckets that count it.
use super::*;
use crate::test_support::{create_test_chore, create_test_chore_manual, create_test_product};
use boss_protocol::{
    CreateExecutionInput, ExecutionStatus, OperatorAnswer, OperatorQuestionRecord, OperatorQuestionStatus,
    ProposalKind, ProposalSubmissionError, StuckBucket,
};
use serde_json::json;

const HOUR: i64 = 3600;
const S5: &str = "S5_merge_conflict";
const S7: &str = "S7_worker_failed";

fn now() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_secs()
}

struct Fx {
    db: WorkDb,
    product: String,
    checker: FakePrStateChecker,
    keys: std::cell::Cell<u32>,
}

fn fx() -> Fx {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product(&db).id;
    Fx {
        db,
        product,
        checker: FakePrStateChecker::always(PrOpenState::Open),
        keys: std::cell::Cell::new(0),
    }
}

impl Fx {
    fn sql(&self, sql: &str) {
        self.db.connect().unwrap().execute_batch(sql).unwrap();
    }

    fn chore(&self, name: &str) -> String {
        create_test_chore_manual(&self.db, &self.product, name).id
    }

    fn age(&self, id: &str, secs: i64) {
        self.sql(&format!(
            "UPDATE task_status_audit SET changed_at = CAST(changed_at AS INTEGER) - {secs} WHERE work_item_id = '{id}'"
        ));
        self.sql(&format!(
            "UPDATE tasks SET progression_grace_waived_at = CAST(progression_grace_waived_at AS INTEGER) - {secs}
             WHERE id = '{id}' AND progression_grace_waived_at IS NOT NULL"
        ));
    }

    /// A chore in `blocked: merge_conflict` for three hours (past the grace).
    fn conflicted(&self, name: &str) -> String {
        let id = self.chore(name);
        self.sql(&format!(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict' WHERE id = '{id}'"
        ));
        self.age(&id, 3 * HOUR);
        id
    }

    /// Put `id` in `blocked: worker_failed` (produced by `basis`) for three
    /// hours.
    fn fail(&self, id: &str, basis: &str) {
        crate::task_status_audit::execute(
            &self.db.connect().unwrap(),
            basis,
            "engine",
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed', autostart = 0 WHERE id = ?1",
            [id],
        )
        .unwrap();
        self.age(id, 3 * HOUR);
    }

    fn task(&self, id: &str) -> Task {
        match self.db.get_work_item(id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            other => panic!("{id} is not a task: {other:?}"),
        }
    }

    fn fingerprint(&self, id: &str, kind: &str) -> String {
        self.db.current_progression_fingerprint(id, kind).unwrap()
    }

    /// Mint the pass's runs and return what it did.
    fn pass(&self) -> ProgressionMint {
        self.db.mint_progression_executions().unwrap()
    }

    fn progression_runs(&self, id: &str) -> Vec<WorkExecution> {
        self.db
            .list_executions(Some(id))
            .unwrap()
            .into_iter()
            .filter(|execution| execution.kind == ExecutionKind::Progression)
            .collect()
    }

    /// Close `run` the way a finished progression run ends, `ago` seconds ago,
    /// with the episode its brief would have recorded.
    fn finish(&self, run: &str, status: &str, disposition: Option<&str>, ago: i64, fingerprint: &str, kind: &str) {
        let outcome = match disposition {
            Some("no_action") => "'no_changes_needed'",
            Some(_) => "'delivered'",
            None => "NULL",
        };
        self.sql(&format!(
            "UPDATE work_executions
             SET status = '{status}', progression_disposition = {disposition}, run_done_outcome = {outcome},
                 created_at = {t}, started_at = {t}, finished_at = {t}, last_error = {error}
             WHERE id = '{run}'",
            disposition = disposition.map_or("NULL".to_owned(), |d| format!("'{d}'")),
            t = now() - ago,
            error = if status == "completed" {
                "NULL"
            } else {
                "'the pane died'"
            },
        ));
        self.db.record_progression_episode(run, fingerprint, kind).unwrap();
    }

    /// A finished progression run that was never minted by a pass.
    fn past_run(
        &self,
        id: &str,
        status: &str,
        disposition: Option<&str>,
        ago: i64,
        fingerprint: &str,
        kind: &str,
    ) -> String {
        let run = self
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(id)
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap()
            .id;
        self.finish(&run, status, disposition, ago, fingerprint, kind);
        run
    }

    fn open_question(&self, id: &str) -> Option<OperatorQuestionRecord> {
        self.db
            .list_operator_questions(id)
            .unwrap()
            .into_iter()
            .find(|question| question.status == OperatorQuestionStatus::Open)
    }

    fn answer(&self, question: &OperatorQuestionRecord, choice: &str) -> OperatorAnswerOutcome {
        self.db
            .answer_operator_question_with(
                &question.question.id,
                OperatorAnswer::MultipleChoice { value: choice.into() },
                &self.checker,
            )
            .unwrap()
    }

    fn is_candidate(&self, id: &str) -> bool {
        self.db
            .list_progression_candidates(None)
            .unwrap()
            .iter()
            .any(|candidate| candidate.work_item_id == id)
    }

    fn ladder_row(&self, id: &str) -> (i64, Option<String>, Option<String>) {
        self.db
            .connect()
            .unwrap()
            .query_row(
                "SELECT reset_at, question_id, question_reason FROM progression_ladder WHERE work_item_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    // ── the verbs, for the regression ────────────────────────────────────────

    /// A live progression run for `item`, spawned for the current graph, with
    /// its brief's episode recorded at the item's current fingerprint.
    fn spawn(&self, run: &str, item: &str, kind: &str) -> String {
        self.sql(&format!(
            "UPDATE work_executions SET status = 'running', started_at = strftime('%s','now') WHERE id = '{run}'"
        ));
        self.db.capture_progression_neighbourhood(run).unwrap();
        let fingerprint = self.fingerprint(item, kind);
        self.db.record_progression_episode(run, &fingerprint, kind).unwrap();
        fingerprint
    }

    fn submit(
        &self,
        run: &str,
        kind: ProposalKind,
        payload: serde_json::Value,
    ) -> std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError> {
        let n = self.keys.get() + 1;
        self.keys.set(n);
        let item = self.db.work_item_for_execution(run).unwrap().unwrap();
        let payload_json = boss_engine_proposal_validation::validate_payload(kind, &payload)
            .unwrap_or_else(|errors| panic!("{kind} payload invalid: {errors:?}"))
            .canonical_json;
        self.db
            .submit_progression_verb(
                SubmitWorkerProposalInput {
                    execution_id: run,
                    work_item_id: &item,
                    kind,
                    payload_json: &payload_json,
                    idempotency_key: &format!("key-{n}"),
                },
                &self.checker,
            )
            .unwrap()
    }

    fn work_execution(&self, item: &str, status: &str, age: i64) -> String {
        let id = self
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(item)
                    .kind(ExecutionKind::TaskImplementation)
                    .status(ExecutionStatus::Failed)
                    .build(),
            )
            .unwrap()
            .id;
        self.sql(&format!(
            "UPDATE work_executions SET status = '{status}', created_at = {t}, finished_at = {t} WHERE id = '{id}'",
            t = now() - age
        ));
        id
    }
}

fn run(
    id: &str,
    status: &str,
    disposition: Option<ProgressionDisposition>,
    fp: Option<&str>,
    kind: Option<&str>,
) -> LadderRun {
    LadderRun::builder()
        .execution_id(id)
        .status(status)
        .maybe_disposition(disposition)
        .maybe_fingerprint(fp.map(str::to_owned))
        .maybe_stuck_kind(kind.map(str::to_owned))
        .created_at(0)
        .build()
}

// ── counting ─────────────────────────────────────────────────────────────────

#[test]
fn only_runs_that_acted_count_toward_the_attempt_cap_and_a_run_with_no_episode_matches() {
    use ProgressionDisposition::{Asked, NoAction, Progressed, Withdrawn};
    let runs = vec![
        run("a", "completed", Some(Progressed), Some("fp-1"), Some(S5)),
        run("b", "completed", Some(NoAction), Some("fp-1"), Some(S5)),
        run("c", "completed", Some(Asked), Some("fp-1"), Some(S5)),
        run("d", "completed", Some(Withdrawn), Some("fp-1"), Some(S5)),
        run("e", "failed", None, Some("fp-1"), Some(S5)),
    ];
    let state = LadderState::count(runs, "fp-1");
    assert_eq!(
        state.progressed_at_fingerprint, 1,
        "asked, withdrawn, no_action and failed runs are not attempts"
    );
    assert_eq!(state.verdict(S5), None);

    // A run whose brief was never composed cannot be told apart from the
    // current fingerprint, so it escalates rather than loops.
    let state = LadderState::count(
        vec![
            run("a", "completed", Some(Progressed), None, None),
            run("b", "completed", Some(Progressed), Some("fp-1"), Some(S5)),
        ],
        "fp-1",
    );
    assert_eq!(state.verdict(S5), Some(LadderReason::LadderExhausted));

    // A different fingerprint is a different world.
    let state = LadderState::count(
        vec![
            run("a", "completed", Some(Progressed), Some("fp-0"), Some(S5)),
            run("b", "completed", Some(Progressed), Some("fp-1"), Some(S5)),
        ],
        "fp-1",
    );
    assert_eq!(state.verdict(S5), None);
    // ...except on `worker_failed`, where the count crosses fingerprints.
    let state = LadderState::count(
        vec![
            run("a", "completed", Some(Progressed), Some("fp-0"), Some(S7)),
            run("b", "completed", Some(Progressed), Some("fp-1"), Some(S7)),
        ],
        "fp-2",
    );
    assert_eq!(state.worker_failed_attempts, 2);
    assert_eq!(state.verdict(S7), Some(LadderReason::WorkerFailedCap));
    assert_eq!(
        state.verdict(S5),
        None,
        "the cross-fingerprint cap is `worker_failed`'s alone"
    );
}

#[test]
fn a_no_action_streak_is_broken_by_any_other_run() {
    use ProgressionDisposition::{NoAction, Progressed};
    let idle = |id: &str, fp: &str| run(id, "completed", Some(NoAction), Some(fp), Some(S5));
    let state = LadderState::count(vec![idle("a", "fp"), idle("b", "fp"), idle("c", "fp")], "fp");
    assert_eq!(
        (state.no_action_streak, state.verdict(S5)),
        (3, Some(LadderReason::NoActionStreak))
    );
    // Two is not three; the other fingerprint in the middle is a different world.
    assert_eq!(
        LadderState::count(vec![idle("a", "fp"), idle("b", "fp")], "fp").no_action_streak,
        2
    );
    assert_eq!(
        LadderState::count(vec![idle("a", "fp"), idle("b", "fp"), idle("c", "other")], "fp").no_action_streak,
        0
    );
    let acted = run("x", "completed", Some(Progressed), Some("fp"), Some(S5));
    assert_eq!(
        LadderState::count(vec![idle("a", "fp"), idle("b", "fp"), acted, idle("c", "fp")], "fp").no_action_streak,
        1
    );
}

#[test]
fn the_fingerprint_moves_with_the_world_and_not_with_a_progression_run() {
    let fx = fx();
    let id = fx.conflicted("Conflicted");
    let before = fx.fingerprint(&id, S5);
    assert_eq!(before.len(), 16);
    fx.past_run(&id, "completed", Some("progressed"), 8 * HOUR, &before, S5);
    assert_eq!(fx.fingerprint(&id, S5), before, "a progression run does not move it");

    fx.sql(&format!("UPDATE tasks SET pr_head_sha = 'abc123' WHERE id = '{id}'"));
    let new_head = fx.fingerprint(&id, S5);
    assert_ne!(new_head, before, "a new PR head does");
    fx.work_execution(&id, "failed", 100);
    assert_ne!(
        fx.fingerprint(&id, S5),
        new_head,
        "so does a new terminal work execution"
    );
    assert_ne!(
        fx.fingerprint(&id, S7),
        fx.fingerprint(&id, S5),
        "and so does the stuck kind"
    );
}

// ── the ladder's transitions ─────────────────────────────────────────────────

#[test]
fn run_one_acts_run_two_follows_on_the_unchanged_fingerprint_and_the_engine_asks_after_it() {
    let fx = fx();
    let id = fx.conflicted("Stays conflicted");

    // Run 1.
    let first = fx.pass();
    assert_eq!((first.minted.len(), first.asked.len()), (1, 0));
    let fingerprint = fx.fingerprint(&id, S5);
    let run1 = first.minted[0].1.id.clone();
    fx.finish(&run1, "completed", Some("progressed"), 9 * HOUR, &fingerprint, S5);

    // Run 2: the item is the same, so a second run is spawned; it is the
    // fingerprint refusal on the verbs that forces it onto another action.
    let second = fx.pass();
    assert_eq!((second.minted.len(), second.asked.len()), (1, 0), "{second:?}");
    assert_eq!(
        fx.db
            .progression_ladder_state(&id, S5)
            .unwrap()
            .progressed_at_fingerprint,
        1
    );
    let run2 = second.minted[0].1.id.clone();
    fx.finish(&run2, "completed", Some("progressed"), 7 * HOUR, &fingerprint, S5);

    // After run 2, with nothing changed, the engine asks and spends no run 3.
    let third = fx.pass();
    assert_eq!((third.minted.len(), third.asked.len()), (0, 1));
    assert_eq!(third.asked[0].work_item_id, id);
    assert_eq!(third.asked[0].reason, LadderReason::LadderExhausted);
    assert_eq!(fx.progression_runs(&id).len(), 2);

    let task = fx.task(&id);
    assert_eq!(
        (task.status, task.blocked_reason.as_deref()),
        (TaskStatus::Blocked, Some("awaiting_operator_answer"))
    );
    let question = fx.open_question(&id).expect("the engine filed a question");
    assert_eq!(question.question.text, QUESTION_TEXT);
    assert_eq!(
        question.question.answer_type,
        OperatorAnswerType::MultipleChoice {
            choices: vec![PARK_IT.into(), ARCHIVE_IT.into(), KEEP_TRYING.into()]
        }
    );
    let explanation = &question.question.explanation;
    for expected in [
        "Context:",
        "Recommendation:",
        "Details:",
        &run1,
        &run2,
        "ended progressed",
    ] {
        assert!(explanation.contains(expected), "{expected}: {explanation}");
    }
    assert_eq!(fx.ladder_row(&id).2.as_deref(), Some("ladder_exhausted"));
    assert_eq!(fx.ladder_row(&id).1.as_deref(), Some(question.question.id.as_str()));
    assert_eq!(question.question.execution_id, run2, "hangs off the latest run");

    // The question is the hold: nothing is asked or minted again while it is open.
    let again = fx.pass();
    assert_eq!((again.minted.len(), again.asked.len()), (0, 0));
    assert_eq!(fx.progression_runs(&id).len(), 2);
}

#[test]
fn a_changed_fingerprint_between_the_runs_is_not_an_unchanged_world() {
    let fx = fx();
    let id = fx.conflicted("Moves between runs");
    fx.past_run(&id, "completed", Some("progressed"), 20 * HOUR, "an-older-world", S5);
    fx.past_run(
        &id,
        "completed",
        Some("progressed"),
        10 * HOUR,
        "another-older-world",
        S5,
    );
    let pass = fx.pass();
    assert_eq!(
        (pass.minted.len(), pass.asked.len()),
        (1, 0),
        "neither run was spent at this fingerprint"
    );
}

#[test]
fn one_no_action_files_nothing_and_renominates_at_four_times_the_cooldown() {
    let fx = fx();
    let id = fx.conflicted("Waiting on an external fact");
    let fingerprint = fx.fingerprint(&id, S5);
    let run = fx.past_run(&id, "completed", Some("no_action"), 7 * HOUR, &fingerprint, S5);
    fx.sql(&format!(
        "INSERT INTO worker_proposals (id, execution_id, work_item_id, kind, payload_json, idempotency_key, state, created_at)
         VALUES ('prop-note', '{run}', '{id}', 'run_done',
                 '{{\"outcome\":\"no_changes_needed\",\"summary\":\"Waiting on the vendor to ship the fix\"}}',
                 'k', 'applied', '1')"
    ));

    // Past one cooldown (six hours) but not four: nothing is minted, nothing asked.
    let quiet = fx.pass();
    assert_eq!((quiet.minted.len(), quiet.asked.len()), (0, 0));
    assert!(fx.open_question(&id).is_none());
    assert_eq!(
        fx.task(&id).blocked_reason.as_deref(),
        Some("merge_conflict"),
        "the item is left as it was"
    );

    // The report shows it as handled while it waits.
    let report = fx.db.stuck_report(0, now(), &|_| false).unwrap();
    let row = report
        .rows
        .iter()
        .find(|row| row.work_item_id == id)
        .expect("in the report");
    assert_eq!(row.bucket, StuckBucket::ProgressionInFlight);
    assert_eq!(
        row.progression_note.as_deref(),
        Some("Waiting on the vendor to ship the fix"),
        "the run's stated reason is shown while the item waits"
    );

    // Past four cooldowns it is nominated again, and still nothing was filed.
    fx.finish(&run, "completed", Some("no_action"), 25 * HOUR, &fingerprint, S5);
    let again = fx.pass();
    assert_eq!((again.minted.len(), again.asked.len()), (1, 0));
    assert!(fx.open_question(&id).is_none());
}

#[test]
fn three_consecutive_no_action_runs_on_an_unchanged_fingerprint_file_the_question() {
    let fx = fx();
    let id = fx.conflicted("Never actionable");
    let fingerprint = fx.fingerprint(&id, S5);
    for ago in [100 * HOUR, 70 * HOUR] {
        fx.past_run(&id, "completed", Some("no_action"), ago, &fingerprint, S5);
    }
    let two = fx.pass();
    assert_eq!(
        (two.minted.len(), two.asked.len()),
        (1, 0),
        "two no_action runs still earn a third"
    );
    let third = two.minted[0].1.id.clone();
    fx.finish(&third, "completed", Some("no_action"), 30 * HOUR, &fingerprint, S5);

    let pass = fx.pass();
    assert_eq!((pass.minted.len(), pass.asked.len()), (0, 1));
    assert_eq!(pass.asked[0].reason, LadderReason::NoActionStreak);
    let question = fx.open_question(&id).unwrap();
    assert!(
        question.question.explanation.contains("three times in a row"),
        "{question:?}"
    );
    assert!(question.question.explanation.contains("ended no_action"));
}

#[test]
fn two_infrastructure_failures_stop_the_nominations_and_file_the_question_with_the_failure() {
    let fx = fx();
    let id = fx.conflicted("Progression keeps dying");
    fx.past_run(&id, "failed", None, 30 * HOUR, "fp", S5);
    let one = fx.pass();
    assert_eq!((one.minted.len(), one.asked.len()), (1, 0), "one failure is retried");
    let retry = one.minted[0].1.id.clone();
    fx.finish(&retry, "orphaned", None, 20 * HOUR, "fp", S5);

    let two = fx.pass();
    assert_eq!((two.minted.len(), two.asked.len()), (0, 1));
    assert_eq!(two.asked[0].reason, LadderReason::InfrastructureFailure);
    let question = fx.open_question(&id).expect("question filed");
    for expected in [
        "crashed or timed out twice",
        "ended failed: the pane died",
        "ended orphaned: the pane died",
    ] {
        assert!(
            question.question.explanation.contains(expected),
            "{expected}: {question:?}"
        );
    }
    assert_eq!(fx.task(&id).blocked_reason.as_deref(), Some("awaiting_operator_answer"));
    assert_eq!(fx.progression_runs(&id).len(), 2, "no third run");

    // An item that has stopped being stuck is not asked about.
    let healed = fx.conflicted("Healed meanwhile");
    fx.past_run(&healed, "failed", None, 30 * HOUR, "fp", S5);
    fx.past_run(&healed, "failed", None, 20 * HOUR, "fp", S5);
    fx.sql(&format!(
        "UPDATE tasks SET status = 'in_review', blocked_reason = NULL WHERE id = '{healed}'"
    ));
    assert_eq!(fx.pass().asked.len(), 0);
    assert!(fx.open_question(&healed).is_none());
}

// ── the answers ──────────────────────────────────────────────────────────────

/// An item the ladder has asked about, with its two finished runs.
fn exhausted(fx: &Fx, id: &str, kind: &str) -> OperatorQuestionRecord {
    let fingerprint = fx.fingerprint(id, kind);
    fx.past_run(id, "completed", Some("progressed"), 20 * HOUR, &fingerprint, kind);
    fx.past_run(id, "completed", Some("progressed"), 10 * HOUR, &fingerprint, kind);
    let pass = fx.pass();
    assert_eq!((pass.asked.len(), pass.minted.len()), (1, 0), "{pass:?}");
    fx.open_question(id).expect("question")
}

#[test]
fn keep_trying_resets_the_attempts_and_renominates_the_item_in_the_state_it_was_stuck_in() {
    let fx = fx();
    let id = fx.conflicted("Owner says keep trying");
    let question = exhausted(&fx, &id, S5);
    let spent_actions: i64 = fx
        .db
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM progression_actions", [], |row| row.get(0))
        .unwrap();

    let outcome = fx.answer(&question, KEEP_TRYING);

    assert_eq!(outcome.archive_work_item_id, None);
    assert!(
        !outcome.minted_execution,
        "keep trying starts no work: progression stays the actor"
    );
    let task = fx.task(&id);
    assert_eq!(
        (task.status, task.blocked_reason.as_deref()),
        (TaskStatus::Blocked, Some("merge_conflict")),
        "the item goes back to the state it was stuck in"
    );
    assert!(fx.open_question(&id).is_none());
    assert!(fx.ladder_row(&id).0 > 0, "the reset is recorded");
    let state = fx.db.progression_ladder_state(&id, S5).unwrap();
    assert_eq!(state.progressed_at_fingerprint, 0, "attempts are back to zero");
    assert!(state.runs.is_empty());

    // Nominated again, without waiting out the stuck-after grace a second time.
    assert!(fx.is_candidate(&id));
    let pass = fx.pass();
    assert_eq!((pass.minted.len(), pass.asked.len()), (1, 0));
    let still: i64 = fx
        .db
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM progression_actions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        still, spent_actions,
        "the verbs' action record, and so the redispatch bound, is untouched"
    );

    // It takes a fresh set of attempts to run the ladder out again.
    let fingerprint = fx.fingerprint(&id, S5);
    fx.finish(
        &pass.minted[0].1.id,
        "completed",
        Some("progressed"),
        7 * HOUR,
        &fingerprint,
        S5,
    );
    assert_eq!(fx.pass().asked.len(), 0, "one attempt after the reset");
}

#[test]
fn keep_trying_restores_autostart_under_the_restoration_basis() {
    let fx = fx();
    let id = create_test_chore(&fx.db, &fx.product, "Keep trying restores autostart").id;
    crate::task_status_audit::execute(
        &fx.db.connect().unwrap(),
        "merge_conflict_observed",
        "engine",
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict',
             last_status_actor = 'engine' WHERE id = ?1",
        [&id],
    )
    .unwrap();
    fx.age(&id, 3 * HOUR);
    let question = exhausted(&fx, &id, S5);
    assert!(!fx.task(&id).autostart, "parking for the question clears autostart");
    fx.answer(&question, KEEP_TRYING);
    let mut autostart_rows: Vec<_> = fx
        .db
        .list_task_field_audit(&id)
        .unwrap()
        .into_iter()
        .filter(|row| row.property == "autostart")
        .collect();
    let restored = autostart_rows.pop().expect("autostart history");
    assert_eq!(restored.old_value.as_deref(), Some("0"));
    assert_eq!(restored.new_value.as_deref(), Some("1"));
    assert_eq!(restored.actor, "human");
    assert_eq!(restored.basis, "progression_ladder_restore_autostart");
}

#[test]
fn a_run_created_in_the_same_second_as_the_reset_belongs_to_the_new_episode() {
    let fx = fx();
    let id = fx.conflicted("Same-second reset");
    let question = exhausted(&fx, &id, S5);
    fx.answer(&question, KEEP_TRYING);
    // Pin the reset to one second and put the two old runs, then a new one, in it.
    fx.sql(&format!(
        "UPDATE progression_ladder SET reset_at = {} WHERE work_item_id = '{id}'",
        now() - HOUR
    ));
    fx.sql(&format!(
        "UPDATE work_executions SET created_at = {} WHERE work_item_id = '{id}' AND kind = 'progression'",
        now() - HOUR
    ));
    fx.sql(&format!(
        "UPDATE progression_ladder SET reset_rowid = (SELECT MAX(rowid) FROM work_executions) WHERE work_item_id = '{id}'"
    ));
    let fingerprint = fx.fingerprint(&id, S5);
    let first = fx.past_run(&id, "completed", Some("progressed"), HOUR, &fingerprint, S5);
    fx.sql(&format!(
        "UPDATE work_executions SET created_at = {} WHERE id = '{first}'",
        now() - HOUR
    ));
    let state = fx.db.progression_ladder_state(&id, S5).unwrap();
    assert_eq!(
        state
            .runs
            .iter()
            .map(|run| run.execution_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.as_str()],
        "the run after the reset counts; the two before it, in the same second, do not"
    );
}

#[test]
fn keep_trying_forgives_the_infrastructure_failures_that_filed_the_question() {
    let fx = fx();
    let id = fx.conflicted("Died, then the owner said keep trying");
    fx.past_run(&id, "failed", None, 30 * HOUR, "fp", S5);
    fx.past_run(&id, "orphaned", None, 20 * HOUR, "fp", S5);
    assert_eq!(fx.pass().asked.len(), 1);
    let question = fx.open_question(&id).unwrap();
    assert!(!fx.is_candidate(&id));

    fx.answer(&question, KEEP_TRYING);

    let clauses = fx.db.progression_clauses(&id).unwrap().unwrap();
    assert!(!clauses.infrastructure_failing, "{clauses:?}");
    assert!(fx.is_candidate(&id), "{clauses:?}");
    assert_eq!(fx.pass().minted.len(), 1);
}

#[test]
fn park_it_parks_through_the_human_path_and_leaves_the_item_in_its_stuck_state_for_un_park() {
    let fx = fx();
    let id = fx.conflicted("Owner says park");
    let question = exhausted(&fx, &id, S5);

    fx.answer(&question, PARK_IT);

    assert!(fx.db.is_parked(&id).unwrap());
    let task = fx.task(&id);
    assert_eq!(task.parked_by.as_deref(), Some("human"));
    assert_eq!(
        task.blocked_reason.as_deref(),
        Some("merge_conflict"),
        "restored for the un-park reconcile"
    );
    assert!(fx.open_question(&id).is_none());
    assert!(!fx.is_candidate(&id), "a parked item atrophies: no progression");
    assert_eq!(fx.pass().minted.len(), 0);
}

#[test]
fn park_it_on_a_revision_parks_the_parent_pr_it_follows() {
    let fx = fx();
    let parent = fx.chore("Parent PR");
    let revision = fx.chore("Revision");
    fx.sql(&format!(
        "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}' WHERE id = '{revision}'"
    ));
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict' WHERE id = '{revision}'"
    ));
    fx.age(&revision, 3 * HOUR);
    let question = exhausted(&fx, &revision, S5);

    fx.answer(&question, PARK_IT);

    assert!(fx.db.is_parked(&parent).unwrap(), "a revision is never parked alone");
    assert!(fx.db.is_parked(&revision).unwrap());
    assert_eq!(fx.task(&revision).blocked_reason.as_deref(), Some("merge_conflict"));
}

#[test]
fn archive_it_restores_the_item_and_hands_the_archive_to_the_caller() {
    let fx = fx();
    let id = fx.conflicted("Owner says archive");
    let question = exhausted(&fx, &id, S5);

    let outcome = fx.answer(&question, ARCHIVE_IT);

    assert_eq!(outcome.archive_work_item_id.as_deref(), Some(id.as_str()));
    assert_eq!(
        fx.task(&id).blocked_reason.as_deref(),
        Some("merge_conflict"),
        "not left waiting on an answer"
    );
    // The caller archives through the ordinary update path, as the owner.
    let archived = fx
        .db
        .update_work_item(
            &id,
            boss_protocol::WorkItemPatch::builder()
                .status("archived")
                .archived_reason("progression could not unstick it; the owner chose to archive it")
                .build(),
        )
        .unwrap();
    assert_eq!(fx.task(&id).status, TaskStatus::Archived);
    drop(archived);
}

// ── the cross-fingerprint worker_failed episode ──────────────────────────────

#[test]
fn worker_failed_across_new_fingerprints_files_the_question_and_never_a_second_redispatch() {
    let fx = fx();
    let item = fx.chore("Keeps failing");
    fx.fail(&item, "delivered_without_pr");

    // Run 1 redispatches; the restart fails and the item is worker_failed again.
    let pass = fx.pass();
    assert_eq!((pass.minted.len(), pass.asked.len()), (1, 0));
    let run1 = pass.minted[0].1.id.clone();
    let f1 = fx.spawn(&run1, &item, S7);
    let w1 = fx
        .submit(&run1, ProposalKind::Redispatch, json!({}))
        .expect("the first redispatch is allowed")
        .proposal
        .applied_ref
        .unwrap();
    fx.finish(&run1, "completed", Some("progressed"), 9 * HOUR, &f1, S7);
    fx.sql(&format!(
        "UPDATE work_executions SET status = 'failed', created_at = {t}, finished_at = {t} WHERE id = '{w1}'",
        t = now() - 100
    ));
    fx.fail(&item, "delivered_without_pr");

    // Run 2 sees a new fingerprint (a new terminal execution id), so the
    // fingerprint alone would not stop it; the redispatch bound does.
    let f2 = fx.fingerprint(&item, S7);
    assert_ne!(f1, f2);
    let pass = fx.pass();
    assert_eq!(
        (pass.minted.len(), pass.asked.len()),
        (1, 0),
        "one attempt is not the cap"
    );
    let run2 = pass.minted[0].1.id.clone();
    assert_eq!(fx.spawn(&run2, &item, S7), f2);
    let bound = fx
        .submit(&run2, ProposalKind::Redispatch, json!({}))
        .expect_err("a second redispatch is refused");
    assert!(
        bound.message.contains("redispatch bound") && bound.message.contains(&run1),
        "{bound:?}"
    );
    fx.submit(
        &run2,
        ProposalKind::Precursor,
        json!({"name": "Fix the cause", "brief": "It fails on main too."}),
    )
    .expect("a different action is allowed");
    fx.finish(&run2, "completed", Some("progressed"), 7 * HOUR, &f2, S7);

    // The precursor's work fails too: another terminal execution, another
    // fingerprint, `worker_failed` again.
    fx.sql(&format!(
        "DELETE FROM work_item_dependencies WHERE dependent_id = '{item}'"
    ));
    fx.work_execution(&item, "failed", 50);
    fx.fail(&item, "delivered_without_pr");
    let f3 = fx.fingerprint(&item, S7);
    assert!(f3 != f2 && f3 != f1, "three distinct fingerprints: {f1} {f2} {f3}");

    // Parking and un-parking, and the runs ending, reset nothing.
    fx.db.park_work_item_as_actor(&item, true, None, "human").unwrap();
    fx.db.park_work_item_as_actor(&item, false, None, "human").unwrap();
    let state = fx.db.progression_ladder_state(&item, S7).unwrap();
    assert_eq!(
        (state.progressed_at_fingerprint, state.worker_failed_attempts),
        (0, 2),
        "no run was spent at this fingerprint, but two were spent on worker_failed"
    );

    // No third run: the engine asks.
    fx.age(&item, 3 * HOUR);
    let pass = fx.pass();
    assert_eq!((pass.minted.len(), pass.asked.len()), (0, 1), "{pass:?}");
    assert_eq!(pass.asked[0].reason, LadderReason::WorkerFailedCap);
    assert_eq!(fx.progression_runs(&item).len(), 2);
    let redispatches: i64 = fx
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM progression_actions WHERE verb = 'redispatch'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(redispatches, 1, "exactly one redispatch was ever applied");
    let question = fx.open_question(&item).unwrap();
    assert!(
        question
            .question
            .explanation
            .contains("failed again after each of Boss's two attempts")
    );

    // "Keep trying" resets the ladder's counts and not the redispatch bound.
    fx.answer(&question, KEEP_TRYING);
    assert_eq!(
        fx.db
            .progression_ladder_state(&item, S7)
            .unwrap()
            .worker_failed_attempts,
        0
    );
    let pass = fx.pass();
    assert_eq!((pass.minted.len(), pass.asked.len()), (1, 0));
    let run3 = pass.minted[0].1.id.clone();
    fx.spawn(&run3, &item, S7);
    let bound = fx
        .submit(&run3, ProposalKind::Redispatch, json!({}))
        .expect_err("the bound outlives the reset");
    assert!(bound.message.contains("redispatch bound"), "{bound:?}");
    fx.finish(&run3, "completed", Some("progressed"), 7 * HOUR, &f3, S7);
}

#[test]
fn a_completed_work_run_starts_a_fresh_episode_and_a_failed_one_does_not() {
    let fx = fx();
    let item = fx.chore("Fails, then works");
    fx.fail(&item, "delivered_without_pr");
    fx.past_run(&item, "completed", Some("progressed"), 30 * HOUR, "fp-a", S7);
    fx.past_run(&item, "completed", Some("progressed"), 20 * HOUR, "fp-b", S7);
    assert_eq!(
        fx.db
            .progression_ladder_state(&item, S7)
            .unwrap()
            .worker_failed_attempts,
        2
    );

    // A work run that failed, or was cancelled, is not a reset.
    fx.work_execution(&item, "failed", 10 * HOUR);
    fx.work_execution(&item, "cancelled", 9 * HOUR);
    assert_eq!(
        fx.db
            .progression_ladder_state(&item, S7)
            .unwrap()
            .worker_failed_attempts,
        2
    );

    // A completed one is: the item got somewhere, so the two earlier runs are history.
    fx.work_execution(&item, "completed", 8 * HOUR);
    let state = fx.db.progression_ladder_state(&item, S7).unwrap();
    assert_eq!((state.worker_failed_attempts, state.runs.len()), (0, 0), "{state:?}");
    let pass = fx.pass();
    assert_eq!(
        (pass.minted.len(), pass.asked.len()),
        (1, 0),
        "a fresh set of attempts, not a question"
    );
}

#[test]
fn a_sibling_revision_completing_does_not_start_a_fresh_episode() {
    let fx = fx();
    let parent = fx.chore("Parent PR");
    let revision = fx.chore("Its revision");
    fx.sql(&format!(
        "UPDATE tasks SET kind = 'revision', parent_task_id = '{parent}' WHERE id = '{revision}'"
    ));
    fx.fail(&revision, "delivered_without_pr");
    fx.past_run(&revision, "completed", Some("progressed"), 30 * HOUR, "fp-a", S7);
    fx.past_run(&revision, "completed", Some("progressed"), 20 * HOUR, "fp-b", S7);

    // The root completes a run: the redispatch bound looks only at the item, so
    // the ladder's episode must not restart either.
    fx.work_execution(&parent, "completed", 8 * HOUR);
    let state = fx.db.progression_ladder_state(&revision, S7).unwrap();
    assert_eq!(state.worker_failed_attempts, 2, "{state:?}");

    // The item's own completed run does.
    fx.work_execution(&revision, "completed", 4 * HOUR);
    let state = fx.db.progression_ladder_state(&revision, S7).unwrap();
    assert_eq!(state.worker_failed_attempts, 0, "{state:?}");
}

// ── the report ───────────────────────────────────────────────────────────────

#[test]
fn the_progression_failing_bucket_counts_both_a_question_filed_and_a_failure_not_yet_asked() {
    let fx = fx();
    let bucket = |id: &str| {
        fx.db
            .stuck_report(0, now(), &|_| false)
            .unwrap()
            .rows
            .into_iter()
            .find(|row| row.work_item_id == id)
            .map(|row| row.bucket)
    };

    // Ladder exhausted and the question filed: waiting on a human, reported as
    // progression having failed.
    let asked = fx.conflicted("Ladder ran out");
    exhausted(&fx, &asked, S5);
    assert_eq!(bucket(&asked), Some(StuckBucket::ProgressionFailing));

    // Two infrastructure failures, the question not yet filed (it is the next
    // pass's work): the same bucket.
    let dying = fx.conflicted("Progression dies");
    fx.past_run(&dying, "failed", None, 30 * HOUR, "fp", S5);
    fx.past_run(&dying, "orphaned", None, 20 * HOUR, "fp", S5);
    assert_eq!(bucket(&dying), Some(StuckBucket::ProgressionFailing));
    // ...and still that bucket once the pass has filed it.
    assert_eq!(fx.pass().asked.len(), 1);
    assert_eq!(bucket(&dying), Some(StuckBucket::ProgressionFailing));

    // A question a progression agent asked, or any other human hold, is plain
    // waiting on a human: the ladder did not file it.
    let worker_question = fx.conflicted("Agent asked");
    fx.sql(&format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = '{worker_question}'"
    ));
    assert_eq!(bucket(&worker_question), Some(StuckBucket::WaitingOnHuman));

    // After the owner answers, the item leaves the bucket: parked is the owner's call.
    let question = fx.open_question(&asked).unwrap();
    fx.answer(&question, PARK_IT);
    assert_eq!(bucket(&asked), Some(StuckBucket::Parked));
}
