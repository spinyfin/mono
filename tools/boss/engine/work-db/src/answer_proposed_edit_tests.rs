//! Durable storage for answer-agent proposed edits: the reply transaction,
//! the migration, and approval through the same-PR revision path.

use crate::test_support::{add_guide_comment, open_db, seed_published_guide};
use crate::{FakePrStateChecker, PrOpenState, ReviseDocOutcome, SpokeWriteApplication, WorkDb};
use boss_protocol::{
    AnswerProposedEdit, COMMENT_STATUS_RESOLVED, ProposedEditInput, ProposedEditState, ProposedTextReplacement,
    WorkItem, WorkItemPatch,
};
use rusqlite::Connection;
use std::sync::Arc;

const REPO: &str = "https://example.test/repo.git";
const ACTOR: &str = "user";

#[path = "answer_proposed_edit_replay_tests.rs"]
mod replay_tests;

fn proposal() -> ProposedEditInput {
    ProposedEditInput {
        version: 1,
        title: "Clarify maintenance cost".into(),
        instruction: "Update docs/design.md to describe change-proportional maintenance.".into(),
        replacements: vec![ProposedTextReplacement {
            path: "docs/design.md".into(),
            old_text: "delta-only".into(),
            new_text: "change-proportional".into(),
        }],
    }
}

/// An `answering` guide question with a running run bound to an execution.
fn asking(db: &WorkDb, series: &str) -> (String, String, String) {
    let comment = add_guide_comment(db, series, "Could this explanation be clearer?");
    db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(&comment.id, "pr_review_guide", series, "v0", 0)
        .unwrap();
    let execution = db.create_answer_agent_execution(&comment.id, REPO).unwrap();
    db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();
    (comment.id, run.id, execution.id)
}

/// A guide question answered with a proposal; returns (comment, entry).
fn answered(db: &WorkDb, series: &str) -> (String, String) {
    let (comment, _, execution) = asking(db, series);
    db.apply_answer_agent_reply(
        &execution,
        "Use change-proportional maintenance.",
        Some(proposal()),
        None,
    )
    .unwrap();
    let entries = db.list_comment_thread_entries(&comment).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].proposed_edit.as_ref().unwrap().state,
        ProposedEditState::Proposed
    );
    (comment, entries[0].id.clone())
}

fn open() -> FakePrStateChecker {
    FakePrStateChecker::always(PrOpenState::Open)
}

fn task_count(db: &WorkDb) -> i64 {
    db.connect()
        .unwrap()
        .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
        .unwrap()
}

fn stored(db: &WorkDb, comment: &str) -> AnswerProposedEdit {
    db.list_comment_thread_entries(comment).unwrap()[0]
        .proposed_edit
        .clone()
        .unwrap()
}

#[test]
fn reply_stores_the_proposal_with_run_completion_and_answered_status() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, run, execution) = asking(&db, &series);
    let answered = db
        .apply_answer_agent_reply(&execution, "Prose answer.", Some(proposal()), None)
        .unwrap();
    assert_eq!(answered.status, "answered");
    let run = db.get_answer_agent_run(&run).unwrap().unwrap();
    assert_eq!(run.status, "replied");
    let proposal_after = stored(&db, &comment);
    assert_eq!(proposal_after.proposal, proposal());
    assert_eq!(proposal_after.state, ProposedEditState::Proposed);
    assert_eq!(proposal_after.revision_id, None);
}

#[test]
fn invalid_or_misplaced_proposal_refuses_the_reply_and_leaves_the_run_running() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, run, execution) = asking(&db, &series);
    for bad in [
        ProposedEditInput {
            version: 99,
            ..proposal()
        },
        ProposedEditInput {
            instruction: "  ".into(),
            ..proposal()
        },
        ProposedEditInput {
            replacements: vec![ProposedTextReplacement {
                path: "".into(),
                old_text: "a".into(),
                new_text: "b".into(),
            }],
            ..proposal()
        },
    ] {
        assert!(
            db.apply_answer_agent_reply(&execution, "Answer", Some(bad), None)
                .is_err()
        );
    }
    assert_eq!(db.get_answer_agent_run(&run).unwrap().unwrap().status, "running");
    assert!(db.list_comment_thread_entries(&comment).unwrap().is_empty());
    assert_eq!(db.get_comment(&comment).unwrap().unwrap().status, "answering");

    // A proposal on a non-guide question is refused too.
    let other = db
        .create_comment(boss_protocol::CreateCommentInput {
            artifact_kind: "work_item".into(),
            artifact_id: "t1".into(),
            doc_version: "v0".into(),
            anchor: boss_protocol::CommentAnchor {
                exact: "alpha".into(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "why?".into(),
            author: "operator".into(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.transition_comment_to_answering(&other.id).unwrap();
    let run = db
        .create_answer_agent_run(&other.id, "work_item", "t1", "v0", 0)
        .unwrap()
        .id;
    let execution = db.create_answer_agent_execution(&other.id, REPO).unwrap();
    db.bind_answer_agent_run_execution(&run, &execution.id).unwrap();
    assert!(
        db.apply_answer_agent_reply(&execution.id, "Answer", Some(proposal()), None)
            .is_err()
    );
    assert_eq!(db.get_answer_agent_run(&run).unwrap().unwrap().status, "running");
}

#[test]
fn remote_replay_keeps_one_entry_and_one_proposal() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, _, execution) = asking(&db, &series);
    let application = || SpokeWriteApplication {
        key: "key-1",
        host_id: "host-1",
        contract: "answer-reply",
    };
    db.apply_answer_agent_reply(&execution, "Answer", Some(proposal()), Some(application()))
        .unwrap();
    db.apply_answer_agent_reply(&execution, "Answer", Some(proposal()), Some(application()))
        .unwrap();
    let entries = db.list_comment_thread_entries(&comment).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].proposed_edit.as_ref().unwrap().state,
        ProposedEditState::Proposed
    );
    // A local duplicate is still refused.
    assert!(
        db.apply_answer_agent_reply(&execution, "Again", Some(proposal()), None)
            .is_err()
    );
}

#[test]
fn proposal_on_a_revision_classified_question_is_stored_superseded() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, _, execution) = asking(&db, &series);
    // Reclassified mid-flight: the comment is still `answering`, but its intent
    // is now a revision, so the reply lands it on `active`.
    db.connect()
        .unwrap()
        .execute("UPDATE work_comments SET intent = 'revision' WHERE id = ?1", [&comment])
        .unwrap();
    db.apply_answer_agent_reply(&execution, "Answer", Some(proposal()), None)
        .unwrap();
    assert_eq!(stored(&db, &comment).state, ProposedEditState::Superseded);
    assert!(
        db.approve_proposed_edit(
            &db.list_comment_thread_entries(&comment).unwrap()[0].id,
            None,
            &open(),
            ACTOR
        )
        .is_err()
    );
}

#[test]
fn approval_creates_one_revision_carrying_the_edited_instruction_and_is_idempotent() {
    let (_dir, db) = open_db();
    let (root, series) = seed_published_guide(&db, 9);
    let (comment, entry) = answered(&db, &series);
    let ReviseDocOutcome::Created { task_id, .. } = db
        .approve_proposed_edit(
            &entry,
            Some("Change docs/design.md and explain assembly cost.".into()),
            &open(),
            ACTOR,
        )
        .unwrap()
    else {
        panic!("expected a revision");
    };
    let (WorkItem::Task(task) | WorkItem::Chore(task)) = db.get_work_item(&task_id).unwrap() else {
        panic!("expected task");
    };
    for needle in [
        "Change docs/design.md and explain assembly cost.",
        comment.as_str(),
        entry.as_str(),
        "delta-only",
        "change-proportional",
    ] {
        assert!(task.description.contains(needle), "missing {needle}");
    }
    let approved = stored(&db, &comment);
    assert_eq!(approved.state, ProposedEditState::Approved);
    assert_eq!(approved.revision_id.as_deref(), Some(task_id.as_str()));
    assert!(approved.revision_status.is_some());
    assert_eq!(
        db.get_comment(&comment).unwrap().unwrap().revise_task_id.as_deref(),
        Some(task_id.as_str())
    );

    // Even after the PR closes, repeating the approval returns the same revision.
    let before = task_count(&db);
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let closed = FakePrStateChecker::always(PrOpenState::Merged);
    assert!(matches!(
        db.approve_proposed_edit(&entry, None, &closed, ACTOR).unwrap(),
        ReviseDocOutcome::AlreadyInFlight { task_id: id } if id == task_id
    ));
    assert_eq!(task_count(&db), before);
}

#[test]
fn closed_pr_rejects_approval_without_touching_the_proposal_or_creating_work() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, entry) = answered(&db, &series);
    let before = task_count(&db);
    let merged = FakePrStateChecker::always(PrOpenState::Merged);
    assert!(matches!(
        db.approve_proposed_edit(&entry, None, &merged, ACTOR).unwrap(),
        ReviseDocOutcome::PrClosed { .. }
    ));
    assert_eq!(task_count(&db), before);
    assert_eq!(db.get_comment(&comment).unwrap().unwrap().status, "answered");
    assert_eq!(stored(&db, &comment).state, ProposedEditState::Proposed);
}

#[test]
fn dismissed_proposals_cannot_be_approved_and_dismissal_is_repeatable() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, entry) = answered(&db, &series);
    db.dismiss_proposed_edit(&entry).unwrap();
    db.dismiss_proposed_edit(&entry).unwrap();
    assert_eq!(stored(&db, &comment).state, ProposedEditState::Dismissed);
    assert!(db.approve_proposed_edit(&entry, None, &open(), ACTOR).is_err());
    assert_eq!(db.comment_for_proposed_edit(&entry).unwrap().id, comment);
}

#[test]
fn only_follow_up_or_reclassification_supersedes_a_proposal() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);

    let (resolved, resolved_entry) = answered(&db, &series);
    db.set_comment_status(&resolved, COMMENT_STATUS_RESOLVED, Some("user"))
        .unwrap();
    assert_eq!(stored(&db, &resolved).state, ProposedEditState::Proposed);
    assert!(db.approve_proposed_edit(&resolved_entry, None, &open(), ACTOR).is_err());
    assert_eq!(stored(&db, &resolved).state, ProposedEditState::Proposed);

    let (followed_up, _) = answered(&db, &series);
    db.transition_comment_to_awaiting_followup(&followed_up).unwrap();
    assert_eq!(stored(&db, &followed_up).state, ProposedEditState::Superseded);

    let (reclassified, entry) = answered(&db, &series);
    db.override_comment_intent(&reclassified, "revision").unwrap();
    assert_eq!(stored(&db, &reclassified).state, ProposedEditState::Superseded);
    assert!(db.approve_proposed_edit(&entry, None, &open(), ACTOR).is_err());
}

#[test]
fn approval_does_not_replay_an_earlier_approved_instruction() {
    let (_dir, db) = open_db();
    let (root, series) = seed_published_guide(&db, 9);
    let (comment, entry) = answered(&db, &series);
    let earlier = db
        .create_comment_thread_entry(&comment, "answer", "engine", "Earlier answer", None, None)
        .unwrap();
    crate::comment_thread_entries::save_proposal(
        &db.connect().unwrap(),
        &earlier.id,
        &AnswerProposedEdit {
            proposal: ProposedEditInput {
                instruction: "An earlier instruction that must not be replayed".into(),
                ..proposal()
            },
            state: ProposedEditState::Approved,
            revision_id: Some(root),
            revision_status: None,
        },
    )
    .unwrap();
    let ReviseDocOutcome::Created { task_id, .. } = db.approve_proposed_edit(&entry, None, &open(), ACTOR).unwrap()
    else {
        panic!("expected a revision");
    };
    let (WorkItem::Task(task) | WorkItem::Chore(task)) = db.get_work_item(&task_id).unwrap() else {
        panic!("expected task");
    };
    assert!(task.description.contains("Update docs/design.md"));
    assert!(!task.description.contains("must not be replayed"));
}

#[test]
fn proposal_survives_reopen_and_concurrent_approvals_create_one_revision() {
    let (dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, entry) = answered(&db, &series);
    drop(db);
    let db = Arc::new(WorkDb::open(dir.path().join("state.db")).unwrap());
    assert_eq!(stored(&db, &comment).state, ProposedEditState::Proposed);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (db, entry, barrier) = (db.clone(), entry.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                db.approve_proposed_edit(&entry, None, &FakePrStateChecker::always(PrOpenState::Open), ACTOR)
                    .unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, ReviseDocOutcome::Created { .. }))
            .count(),
        1
    );
    let ids: Vec<_> = outcomes
        .into_iter()
        .map(|o| match o {
            ReviseDocOutcome::Created { task_id, .. } | ReviseDocOutcome::AlreadyInFlight { task_id } => task_id,
            other => panic!("unexpected approval result: {other:?}"),
        })
        .collect();
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn migration_adds_the_column_once_and_leaves_existing_entries_without_proposals() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE work_comments (id TEXT PRIMARY KEY, status TEXT);
         CREATE TABLE comment_thread_entries (id TEXT PRIMARY KEY, comment_id TEXT);
         INSERT INTO work_comments VALUES ('c', 'answered');
         INSERT INTO comment_thread_entries VALUES ('old', 'c');",
    )
    .unwrap();
    for _ in 0..2 {
        crate::comment_thread_entries::migrate_proposed_edits(&conn).unwrap();
    }
    let old: Option<String> = conn
        .query_row(
            "SELECT proposed_edit FROM comment_thread_entries WHERE id = 'old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old, None);
    conn.execute(
        "UPDATE comment_thread_entries SET proposed_edit = '{\"state\":\"proposed\"}' WHERE id = 'old'",
        [],
    )
    .unwrap();
    conn.execute("UPDATE work_comments SET status = 'awaiting_followup'", [])
        .unwrap();
    let state: String = conn
        .query_row(
            "SELECT json_extract(proposed_edit, '$.state') FROM comment_thread_entries WHERE id = 'old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "superseded");
}
