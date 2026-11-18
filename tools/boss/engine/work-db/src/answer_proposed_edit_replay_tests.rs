//! Replay and malformed-data regression coverage for proposed answers.

use super::*;

fn reply_twice(db: &WorkDb, comment: &str, run: &str, execution: &str, status: &str) {
    for _ in 0..2 {
        let result = db
            .apply_answer_agent_reply(
                execution,
                "Retained answer",
                Some(proposal()),
                Some(SpokeWriteApplication {
                    key: "repair-key",
                    host_id: "host-1",
                    contract: "answer-reply",
                }),
            )
            .unwrap();
        assert_eq!(result.status, status);
    }
    let run = db.get_answer_agent_run(run).unwrap().unwrap();
    assert_eq!(run.status, "replied");
    assert_eq!(run.reply_body.as_deref(), Some("Retained answer"));
    let entries = db.list_comment_thread_entries(comment).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "Retained answer");
    assert_eq!(entries[0].answer_agent_run_id.as_deref(), Some(run.id.as_str()));
    assert_eq!(stored(db, comment).proposal, proposal());
    assert_eq!(stored(db, comment).state, ProposedEditState::Proposed);
    assert_eq!(db.get_comment(comment).unwrap().unwrap().status, status);
    assert!(db.get_spoke_applied_write("repair-key").unwrap().unwrap().accepted);
}

#[test]
fn late_replies_preserve_resolved_and_dismissed_questions() {
    for status in ["resolved", "dismissed"] {
        let (_dir, db) = open_db();
        let (_, series) = seed_published_guide(&db, 9);
        let (comment, run, execution) = asking(&db, &series);
        db.set_comment_status(&comment, status, Some("user")).unwrap();
        reply_twice(&db, &comment, &run, &execution, status);
    }
}

#[test]
fn remote_replay_repairs_a_completed_run_without_an_entry() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, run, execution) = asking(&db, &series);
    db.complete_answer_agent_run(&run, "replied", Some("Retained answer"), None)
        .unwrap();
    assert!(db.list_comment_thread_entries(&comment).unwrap().is_empty());
    reply_twice(&db, &comment, &run, &execution, "answered");
}

#[test]
fn malformed_proposal_keeps_the_answer_and_other_entries_readable() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, entry) = answered(&db, &series);
    db.create_comment_thread_entry(&comment, "answer", "engine", "Another answer", None, None)
        .unwrap();
    for raw in ["{bad json", "{\"state\":\"proposed\"}"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE comment_thread_entries SET proposed_edit = ?1 WHERE id = ?2",
                rusqlite::params![raw, entry],
            )
            .unwrap();
        let entries = db.list_comment_thread_entries(&comment).unwrap();
        assert_eq!(entries.len(), 2);
        let answer = entries.iter().find(|e| e.id == entry).unwrap();
        assert_eq!(answer.body, "Use change-proportional maintenance.");
        assert!(answer.proposed_edit.is_none());
        assert!(db.approve_proposed_edit(&entry, None, &open(), ACTOR).is_err());
    }
}

#[test]
fn proposal_shape_and_size_limits_refuse_without_writing() {
    let (_dir, db) = open_db();
    let (_, series) = seed_published_guide(&db, 9);
    let (comment, run, execution) = asking(&db, &series);
    let mut invalid = Vec::new();
    for path in [
        "/absolute",
        "../outside",
        "docs/../outside",
        "C:\\outside",
        "docs//file",
    ] {
        let mut bad = proposal();
        bad.replacements[0].path = path.into();
        invalid.push(bad);
    }
    let mut empty_old = proposal();
    empty_old.replacements[0].old_text.clear();
    invalid.push(empty_old);
    let mut duplicate = proposal();
    duplicate.replacements.push(duplicate.replacements[0].clone());
    invalid.push(duplicate);
    invalid.push(ProposedEditInput {
        title: "x".repeat(4097),
        ..proposal()
    });
    invalid.push(ProposedEditInput {
        instruction: "x".repeat(65537),
        ..proposal()
    });
    let mut oversized = proposal();
    oversized.replacements[0].new_text = "x".repeat(65537);
    invalid.push(oversized);
    let mut too_many = proposal();
    too_many.replacements = vec![too_many.replacements[0].clone(); 101];
    invalid.push(too_many);
    let mut total_size = proposal();
    total_size.replacements = (0..20)
        .map(|i| ProposedTextReplacement {
            path: format!("docs/{i}.md"),
            old_text: "x".repeat(65536),
            new_text: "y".into(),
        })
        .collect();
    invalid.push(total_size);
    for bad in invalid {
        assert!(matches!(
            db.apply_answer_agent_reply(&execution, "Answer", Some(bad), None),
            Err(crate::SpokeWriteError::Refused(_))
        ));
    }
    assert!(db.list_comment_thread_entries(&comment).unwrap().is_empty());
    assert_eq!(db.get_answer_agent_run(&run).unwrap().unwrap().status, "running");
}
