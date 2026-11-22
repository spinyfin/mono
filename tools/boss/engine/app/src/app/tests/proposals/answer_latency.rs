//! A person's answer to a question card does not wait for the scheduler's
//! queued-display pass.
//!
//! The pass recomputes every queued card's label inside one read transaction
//! and holds its connection throughout. While it read through the connection
//! mutations share, an answer that arrived during a pass waited for the pass
//! to end: seconds, when each card's read was slow. The pass now has its own
//! read-only connection, so the answer's write goes straight through.

use super::*;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a person's own action may take to be recorded and replied to.
const ANSWER_BOUND: Duration = Duration::from_millis(300);

/// Longest a pass stays held if nothing releases it, so an answer that waits
/// for the pass reports how late it was instead of hanging the suite.
const PASS_HOLD_LIMIT: Duration = Duration::from_secs(2);

/// Holds a queued-display pass inside its first statement, on its connection.
#[derive(Default)]
struct PassGate {
    /// (the pass reached the gate, the pass may continue)
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl PassGate {
    /// Called from the pass's connection as its statements run.
    fn hold(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let deadline = Instant::now() + PASS_HOLD_LIMIT;
        while !state.1 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
        // A pass held once runs to completion.
        state.1 = true;
    }

    fn reached(&self) -> bool {
        self.state.lock().unwrap().0
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_is_not_queued_behind_a_slow_queued_display_pass() {
    let (state, _dir, asking, task_id) = parked_question_task().await;
    let coordinator = state.execution_coordinator.clone();

    // A slow pass: every statement it runs on its connection stops at the gate.
    let gate = Arc::new(PassGate::default());
    let display_db = coordinator.queued_display_db();
    display_db.connect().unwrap().progress_handler(
        1,
        Some({
            let gate = gate.clone();
            move || {
                gate.hold();
                false
            }
        }),
    );
    let pass = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            let mut displays = None;
            coordinator
                .refresh_queued_displays(&mut displays, boss_engine_utils::epoch_time::now_epoch_secs())
                .await;
            displays
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !gate.reached() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the queued-display pass must read through the connection it is given");

    // The click, while the pass is still holding its connection.
    let clicked = Instant::now();
    let reply = answer_question(&state, &task_id, true).await;
    let answered = clicked.elapsed();
    let pass_still_reading = !pass.is_finished();
    eprintln!(
        "answer latency during a held queued-display pass: {answered:?} (pass still reading: {pass_still_reading})"
    );

    let FrontendEvent::WorkItemUpdated { item } = reply else {
        panic!("expected the answered card, got {reply:?}");
    };
    let (boss_protocol::WorkItem::Task(card) | boss_protocol::WorkItem::Chore(card)) = item else {
        panic!("expected the answered task");
    };
    assert_eq!(card.id, task_id);
    assert!(card.operator_question.is_none(), "the reply clears the question card");
    assert!(
        answered < ANSWER_BOUND,
        "an answer must be recorded and replied to within {ANSWER_BOUND:?} while a queued-display pass is \
         reading; it took {answered:?}"
    );
    assert!(
        pass_still_reading,
        "the answer completed while the queued-display pass was still holding its connection"
    );
    assert_eq!(
        resume_after_answer(&state, &task_id, &asking),
        1,
        "the answer is durable: it minted the run that carries it"
    );

    gate.release();
    let displays = pass.await.unwrap();
    display_db.connect().unwrap().progress_handler(0, None::<fn() -> bool>);
    assert!(
        displays.is_some(),
        "the held pass still completes and seeds its snapshot"
    );
}

fn resume_after_answer(state: &ServerState, task_id: &str, asking: &str) -> usize {
    state
        .work_db
        .list_executions(Some(task_id))
        .unwrap()
        .into_iter()
        .filter(|execution| execution.id != asking)
        .count()
}
