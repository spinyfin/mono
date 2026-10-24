//! A runtime patch must be the card the board tree would have produced, for
//! runtime-only changes, and must say so loudly (absent, changed structural
//! fields) for the structural ones a patch cannot carry: deletes, archives,
//! product moves, project moves and revision-chain effects on a related card.
use super::*;
use std::collections::BTreeMap;

fn value<T: serde::Serialize>(item: &T) -> serde_json::Value {
    serde_json::to_value(item).unwrap()
}

/// Every card of the tree as `id -> (lane, card, runtime)`, in comparable JSON.
fn tree_cards(db: &WorkDb, product_id: &str) -> BTreeMap<String, (bool, serde_json::Value, serde_json::Value)> {
    let tree = db.get_work_tree(product_id).unwrap();
    let runtimes: BTreeMap<_, _> = tree
        .task_runtimes
        .iter()
        .map(|runtime| (runtime.work_item_id.clone(), value(runtime)))
        .collect();
    tree.tasks
        .iter()
        .map(|task| (false, task))
        .chain(tree.chores.iter().map(|task| (true, task)))
        .map(|(chore, task)| (task.id.clone(), (chore, value(task), runtimes[&task.id].clone())))
        .collect()
}

fn patch_cards(patch: &WorkRuntimePatch) -> BTreeMap<String, (bool, serde_json::Value, serde_json::Value)> {
    patch
        .items
        .iter()
        .map(|entry| {
            let (chore, task) = match &entry.item {
                WorkItem::Task(task) => (false, task),
                WorkItem::Chore(task) => (true, task),
                other => panic!("a patch carries only tasks and chores, got {other:?}"),
            };
            (task.id.clone(), (chore, value(task), value(&entry.runtime)))
        })
        .collect()
}

fn ids(cards: &BTreeMap<String, (bool, serde_json::Value, serde_json::Value)>) -> Vec<String> {
    cards.keys().cloned().collect()
}

/// A product with a chain (root chore in review plus two revisions), a plain
/// chore with a queued execution, a project task, and the cards the board
/// hides: one archived, one deleted, one in another product.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct Board {
    db: WorkDb,
    product: String,
    other_product: String,
    root: String,
    active_revision: String,
    done_revision: String,
    queued_chore: String,
    project_task: String,
    archived: String,
    deleted: String,
    foreign: String,
}

fn board() -> Board {
    let db = WorkDb::open(temp_db_path("board-patch")).unwrap();
    let product = make_revision_product(&db, "patch");
    let other_product = make_revision_product(&db, "other");
    let root = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/7");
    let active_revision = insert_revision_row(&db, &product, &root);
    let done_revision = insert_revision_row(&db, &product, &root);
    let queued_chore = create_test_chore_manual(&db, product.clone(), "Queued chore").id;
    let archived = create_test_chore_manual(&db, product.clone(), "Archived chore").id;
    let deleted = create_test_chore_manual(&db, product.clone(), "Deleted chore").id;
    let foreign = create_test_chore_manual(&db, other_product.clone(), "Foreign chore").id;
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.clone(),
            name: "Patch project".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let project_task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.clone())
                .project_id(project.id)
                .name("Project task")
                .build(),
        )
        .unwrap()
        .id;
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&active_revision])
            .unwrap();
        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&done_revision])
            .unwrap();
        conn.execute("UPDATE tasks SET status = 'archived' WHERE id = ?1", [&archived])
            .unwrap();
    }
    db.delete_work_item(&deleted).unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(queued_chore.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap();
    Board::builder()
        .db(db)
        .product(product)
        .other_product(other_product)
        .root(root)
        .active_revision(active_revision)
        .done_revision(done_revision)
        .queued_chore(queued_chore)
        .project_task(project_task)
        .archived(archived)
        .deleted(deleted)
        .foreign(foreign)
        .build()
}

fn all_ids(board: &Board) -> Vec<String> {
    ids(&tree_cards(&board.db, &board.product))
}

fn assert_patch_matches_tree(board: &Board, requested: &[String]) {
    let patch = board.db.get_work_runtime_patch(&board.product, requested).unwrap();
    assert!(patch.absent.is_empty(), "unexpected absent ids: {:?}", patch.absent);
    let tree = tree_cards(&board.db, &board.product);
    let cards = patch_cards(&patch);
    for requested in requested {
        assert!(cards.contains_key(requested), "{requested} missing from patch");
    }
    for (id, patched) in &cards {
        assert_eq!(patched, &tree[id], "patched card {id} differs from the tree's card");
    }
}

#[test]
fn every_board_card_patches_to_the_card_the_tree_produces() {
    let board = board();
    let all = all_ids(&board);
    assert!(all.len() >= 5, "fixture should put several cards on the board: {all:?}");
    for hidden in [&board.archived, &board.deleted, &board.foreign] {
        assert!(!all.contains(hidden), "{hidden} must not be on the board");
    }
    assert_patch_matches_tree(&board, &all);
    for id in &all {
        assert_patch_matches_tree(&board, std::slice::from_ref(id));
    }
}

#[test]
fn runtime_only_changes_patch_to_the_same_card_the_tree_now_produces() {
    let board = board();
    // The merge poller's write: CI and mergeability on the root card. The
    // connection is a mutex guard, so it is released before the next WorkDb
    // call takes its own.
    board
        .db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET ci_required_state = 'success', pr_mergeable_state = 'mergeable', \
             merge_queue_state = 'queued', merge_queue_detail = 'position 2' WHERE id = ?1",
            [&board.root],
        )
        .unwrap();
    // The scheduler's write: a wait reason on the queued execution.
    let execution = board.db.list_executions(Some(&board.queued_chore)).unwrap().remove(0);
    board
        .db
        .set_dispatch_wait_reason(&execution.id, "waiting for a free slot")
        .unwrap();
    let patch = board
        .db
        .get_work_runtime_patch(&board.product, std::slice::from_ref(&board.queued_chore))
        .unwrap();
    let cards = patch_cards(&patch);
    assert_eq!(
        cards[&board.queued_chore].2["dispatch_wait_reason"],
        "waiting for a free slot"
    );
    assert_patch_matches_tree(&board, std::slice::from_ref(&board.queued_chore));
    assert_patch_matches_tree(&board, std::slice::from_ref(&board.root));
    let root = patch_cards(
        &board
            .db
            .get_work_runtime_patch(&board.product, std::slice::from_ref(&board.root))
            .unwrap(),
    );
    assert_eq!(root[&board.root].1["merge_queue_state"], "queued");
}

#[test]
fn a_revision_patch_carries_its_chain_so_the_root_flags_are_current() {
    let board = board();
    // The active revision is what holds the root out of "ready for review".
    let patch = board
        .db
        .get_work_runtime_patch(&board.product, std::slice::from_ref(&board.active_revision))
        .unwrap();
    let cards = patch_cards(&patch);
    assert_eq!(
        ids(&cards),
        {
            let mut expected = vec![
                board.root.clone(),
                board.active_revision.clone(),
                board.done_revision.clone(),
            ];
            expected.sort();
            expected
        },
        "a revision's patch is its whole chain: root, itself and its siblings"
    );
    assert_eq!(cards[&board.root].1["has_in_progress_revision"], true);

    // The revision finishes: the ROOT card's derived flag must follow, and the
    // patch for the revision alone must carry that root.
    board
        .db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done' WHERE id = ?1",
            [&board.active_revision],
        )
        .unwrap();
    let cards = patch_cards(
        &board
            .db
            .get_work_runtime_patch(&board.product, std::slice::from_ref(&board.active_revision))
            .unwrap(),
    );
    // `false` is omitted from the wire form, so "not set" is what a cleared
    // flag looks like; the tree comparison below pins the exact card.
    assert_ne!(
        cards[&board.root].1["has_in_progress_revision"], true,
        "the root's flag must clear once its last open revision is done"
    );
    assert_patch_matches_tree(&board, std::slice::from_ref(&board.active_revision));
}

#[test]
fn cards_the_board_hides_or_that_are_not_cards_come_back_absent() {
    let board = board();
    let requested = vec![
        board.deleted.clone(),
        board.archived.clone(),
        board.foreign.clone(),
        "task_unknown".to_owned(),
        "prod_not_a_card".to_owned(),
        board.queued_chore.clone(),
    ];
    let patch = board.db.get_work_runtime_patch(&board.product, &requested).unwrap();
    assert_eq!(
        patch.absent,
        requested[..5].to_vec(),
        "deleted, archived, other-product, unknown and non-task ids are absent, in request order"
    );
    assert!(patch_cards(&patch).contains_key(&board.queued_chore));
    // The same ids are live cards of their own product, proving "absent" is
    // about membership of the requested board, not about the row.
    let foreign = board
        .db
        .get_work_runtime_patch(&board.other_product, std::slice::from_ref(&board.foreign))
        .unwrap();
    assert!(foreign.absent.is_empty());
}

#[test]
fn deleting_a_revision_reports_it_absent_while_its_root_still_patches() {
    let board = board();
    board.db.delete_work_item(&board.done_revision).unwrap();
    let patch = board
        .db
        .get_work_runtime_patch(&board.product, &[board.done_revision.clone(), board.root.clone()])
        .unwrap();
    assert_eq!(patch.absent, vec![board.done_revision.clone()]);
    assert!(patch_cards(&patch).contains_key(&board.root));
    assert_patch_matches_tree(&board, std::slice::from_ref(&board.root));
}

#[test]
fn a_moved_card_patches_with_its_new_structural_fields_and_leaves_the_old_board() {
    let board = board();
    let before = patch_cards(
        &board
            .db
            .get_work_runtime_patch(&board.product, std::slice::from_ref(&board.project_task))
            .unwrap(),
    );
    assert!(before[&board.project_task].1["project_id"].is_string());

    // Detach from its project: the card changes lane (tasks -> none/chores) and
    // its `project_id` differs from what a consumer holds.
    board
        .db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET project_id = NULL, kind = 'chore' WHERE id = ?1",
            [&board.project_task],
        )
        .unwrap();
    let after = board
        .db
        .get_work_runtime_patch(&board.product, std::slice::from_ref(&board.project_task))
        .unwrap();
    let after_cards = patch_cards(&after);
    assert!(after_cards[&board.project_task].1["project_id"].is_null());
    assert!(
        after_cards[&board.project_task].0,
        "the card now sits in the chores lane"
    );
    assert_patch_matches_tree(&board, std::slice::from_ref(&board.project_task));

    // Move it to another product: it is absent from the old board's patch.
    board
        .db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET product_id = ?2 WHERE id = ?1",
            params![board.project_task, board.other_product],
        )
        .unwrap();
    let moved = board
        .db
        .get_work_runtime_patch(&board.product, std::slice::from_ref(&board.project_task))
        .unwrap();
    assert_eq!(moved.absent, vec![board.project_task.clone()]);
}
