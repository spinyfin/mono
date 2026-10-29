//! `CreateRevision` records the caller's actor (not `engine`) on the new
//! revision's ledger insert row, via the real RPC handler.

use super::*;
use crate::app::work_items;
use crate::test_support::{create_test_chore, create_test_product};
use crate::work::{FakePrStateChecker, PrOpenState};
use boss_protocol::CreateRevisionInput;
use rusqlite::params;

#[tokio::test]
async fn create_revision_handler_records_the_callers_actor_on_the_insert_row() {
    let (state, _dir) =
        test_server_state_with_pr_state_checker(Arc::new(FakePrStateChecker::always(PrOpenState::Open)));
    let db = &state.work_db;
    let product = create_test_product(db);
    let parent = create_test_chore(db, product.id, "Parent");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            params![parent.id, "https://github.com/spinyfin/mono/pull/42"],
        )
        .unwrap();

    let sink = make_session_sink();
    let ctx = Dispatch::builder()
        .server_state(state.clone())
        .work_db(db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build();
    work_items::handle_create_revision(
        ctx,
        FrontendRequest::CreateRevision {
            input: CreateRevisionInput::builder()
                .parent_task_id(&parent.id)
                .description("Human-filed revision")
                .build(),
        },
    )
    .await;

    let conn = db.connect().unwrap();
    let revision_id: String = conn
        .query_row(
            "SELECT id FROM tasks WHERE kind = 'revision' AND description = 'Human-filed revision'",
            [],
            |r| r.get(0),
        )
        .expect("handler must create the revision");
    let (basis, actor): (String, String) = conn
        .query_row(
            "SELECT basis, actor FROM task_status_audit
             WHERE work_item_id = ?1 AND property = 'status' AND old_value IS NULL
             ORDER BY id ASC LIMIT 1",
            [&revision_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(basis, "insert_revision_in_tx");
    assert_eq!(actor, boss_protocol::LAST_STATUS_ACTOR_HUMAN);
}
