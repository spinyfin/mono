//! Re-reading a few board cards without assembling the product board
//! (hub-and-spoke design §2.4, "separate runtime and structural
//! invalidations").
//!
//! [`WorkDb::get_work_tree`] loads every card of a product and then attaches
//! derived fields to them in one fixed sequence. A runtime-class
//! invalidation names a card whose runtime changed, so this module runs the
//! same row select and the same attach sequence over just that card and its
//! revision chain. The chain is the whole affected set for the derived
//! fields: a revision's status decides its root's `has_in_progress_revision`
//! and `ready_for_review`, and the root's PR decides the revisions' parent
//! link, so no other card reads them. Both paths share the column list, the
//! lane predicates and [`attach_board_derivations`], so a patched card is
//! the card the tree would have produced.

use std::collections::BTreeSet;
use std::time::Instant;

use super::board_review_cache::BoardReviewCache;
use super::*;
use crate::population_timing::{PopulationTrace, elapsed_ms, segment};
use boss_protocol::WorkRuntimePatchItem;

/// Columns the board reads for every task and chore row. Shared by the tree
/// queries and the patch query so a card cannot be read two different ways.
const BOARD_TASK_COLUMNS: &str = "id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, merge_queue_detail, driver, pr_mergeable_state, reasoning, review_cycle, last_reviewed_sha, external_ref_kind, external_ref_canonical_id, external_ref_raw, external_ref_synced_at, external_ref_unbound_at, parent_task_id, source_automation_id, origin_task_short_id, origin_pr_number, completed_at, dispatch_failed_reason, dispatch_failed_error, dispatch_failed_at, blocked_detail, deferred, tags, human_driven, completion_summary, effort_matched_rule, effort_reasons, (SELECT view_json FROM open_operator_questions WHERE work_item_id = tasks.id) AS operator_question";

/// The board's tasks lane: every project-member kind plus chores and
/// follow-ups carrying a project id.
const BOARD_TASKS_LANE: &str = "(kind IN ('project_task', 'design', 'investigation', 'revision', 'design_postmortem') OR (kind IN ('chore', 'followup') AND project_id IS NOT NULL))";

/// The board's chores lane: project-less chores and follow-ups only.
const BOARD_CHORES_LANE: &str = "(kind IN ('chore', 'followup') AND project_id IS NULL)";

/// Rows the board hides: tombstoned and archived leaf items stay queryable
/// elsewhere but never appear as cards.
const BOARD_LIVE: &str = "deleted_at IS NULL AND status != 'archived'";

/// The product's tasks-lane rows, in board order.
pub(super) fn tasks_lane_query() -> String {
    format!(
        "SELECT {BOARD_TASK_COLUMNS} FROM tasks \
         WHERE product_id = ?1 AND {BOARD_TASKS_LANE} AND {BOARD_LIVE} \
         ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC, id ASC"
    )
}

/// The product's chores-lane rows, in board order.
pub(super) fn chores_lane_query() -> String {
    format!(
        "SELECT {BOARD_TASK_COLUMNS} FROM tasks \
         WHERE product_id = ?1 AND {BOARD_CHORES_LANE} AND {BOARD_LIVE} \
         ORDER BY created_at ASC, id ASC"
    )
}

#[derive(Clone, Copy)]
pub(super) enum CardScope {
    WholeProduct,
    Subset,
}

/// Attach every derived per-card field the board carries, in the order the
/// fields depend on each other, and return the tasks lane (the revision
/// projection rebuilds it). `tasks` and `chores` must contain whole revision
/// chains. `scope` selects bulk or per-card queries; `caller` only labels warnings.
// The scope and the optional review cache are independent per-caller choices;
// bundling them into a struct would only rename the two call sites.
#[allow(clippy::too_many_arguments)]
pub(super) fn attach_board_derivations(
    conn: &Connection,
    tasks: Vec<Task>,
    chores: &mut [Task],
    product_id: &str,
    scope: CardScope,
    caller: &str,
    trace: &mut PopulationTrace,
    review_cache: Option<&BoardReviewCache>,
) -> Vec<Task> {
    // Compute revision projections (revision_seq, revision_parent_pr_url)
    // for every `kind = 'revision'` task. These are derived fields —
    // not stored columns — so they are calculated fresh here.
    trace.enter_stage(segment::ASSEMBLE);
    let t = Instant::now();
    let mut tasks = attach_revision_projections(tasks, chores);
    // Compute has_in_progress_revision for every chain-root task that
    // has at least one descendant revision that
    // `TaskStatus::can_still_change_pr`.
    attach_in_progress_revision_flag(&mut tasks, chores);
    // Compute ready_for_review for every Review-lane task that is
    // waiting on the operator and nothing else (no block, no
    // in-progress revision, CI green, no merge conflict). Must run
    // after attach_in_progress_revision_flag, which it reads.
    attach_ready_for_review_flag(&mut tasks, chores);
    trace.record_plain(segment::ASSEMBLE, elapsed_ms(t));

    // Compute ai_reviewing for tasks held in Doing while a pr_review
    // execution is in flight. Surfaces the "Reviewing (AI)" badge on
    // the kanban card. Errors are non-fatal — log and continue with
    // the field defaulting to false. Single batched `IN (...)` query.
    trace.enter_stage(segment::DB_AI_REVIEWING);
    let t = Instant::now();
    if let Err(err) = attach_ai_reviewing_flag(conn, &mut tasks, chores) {
        tracing::warn!(?err, "{caller}: failed to attach ai_reviewing flag; ignoring");
    }
    trace.record_plain(segment::DB_AI_REVIEWING, elapsed_ms(t));

    // Resolve the single AI-review-state badge (reviewing / reviewed
    // with findings / reviewed all clear / review not required / no
    // badge) for every task and chore. Must run after the ai_reviewing
    // attach above (reads it) and after attach_revision_projections
    // above (reads revision_seq). Errors are non-fatal — log and leave
    // the field at its default (no badge). Resolved per PR owner, from
    // the review cache when the caller has one; the segment records the
    // cards resolved and the statements that took.
    trace.enter_stage(segment::DB_AI_REVIEW_STATE);
    let t = Instant::now();
    match attach_ai_review_state_with(conn, &mut tasks, chores, review_cache.map(|cache| (cache, product_id))) {
        Ok((stats, timings)) => record_ai_review_state(trace, elapsed_ms(t), stats, timings),
        Err(err) => {
            tracing::warn!(?err, "{caller}: failed to attach ai_review_state; ignoring");
            trace.record_plain(segment::DB_AI_REVIEW_STATE, elapsed_ms(t));
        }
    }

    // Resolve which labelled Review-column section each card is grouped
    // under. Reads ready_for_review, has_in_progress_revision and
    // ai_review_state, so it must run after all three attaches above.
    attach_review_section(&mut tasks, chores);
    if let Err(err) = attach_pr_updated_at(conn, &mut tasks, chores) {
        tracing::warn!(?err, "{caller}: failed to attach pr_updated_at; ignoring");
    }

    // Resolve has_attachments for every task/chore so the kanban card can
    // gate its screenshot-viewer affordance without a per-card query.
    // Errors are non-fatal — log and leave the field at its default
    // (affordance hidden). Single batched `IN (...)` query.
    trace.enter_stage(segment::DB_HAS_ATTACHMENTS);
    let t = Instant::now();
    let attachments = if matches!(scope, CardScope::WholeProduct) {
        attach_has_attachments_flag(conn, &mut tasks, chores)
    } else {
        revision_helpers::attach_has_attachments_flag_for_ids(conn, &mut tasks, chores)
    };
    if let Err(err) = attachments {
        tracing::warn!(?err, "{caller}: failed to attach has_attachments flag; ignoring");
    }
    trace.record_plain(segment::DB_HAS_ATTACHMENTS, elapsed_ms(t));

    // Resolve review-guide lifecycle/readable-version for every PR-bearing
    // task/chore so the Review-card guide affordance renders without a
    // per-card query. Errors are non-fatal — log and leave the fields at
    // their default (affordance hidden). Board-sized sets join series by
    // product; single-item chains keep a root-id list.
    trace.enter_stage(segment::DB_REVIEW_GUIDE_STATE);
    let t = Instant::now();
    if let Err(err) = attach_review_guide_state(conn, &mut tasks, chores) {
        tracing::warn!(?err, "{caller}: failed to attach review_guide_state; ignoring");
    }
    trace.record_plain(segment::DB_REVIEW_GUIDE_STATE, elapsed_ms(t));

    // Resolve the per-task doc-link state for every work item so a
    // card with `tasks.doc_*` set renders the Review-lane doc-link
    // icon — independent of `kind`. Design cards with a project also
    // overlay the parent project's `design_doc_*` state in the app.
    // Pass `|_| None` for the workspace lookup: cube is not consulted
    // in get_work_tree, and the app prefers the GitHub raw-content URL
    // for in-review docs on the PR head branch anyway. Errors per
    // task are non-fatal — log and leave the field None (affordance
    // hidden).
    //
    // Whole-product reads prefilter pointer-bearing rows; subset reads probe
    // each requested card. `resolved` counts pointer-bearing items (including
    // broken links), not all probed cards. `doc_pointer_queries` counts all
    // statements, including probes without pointers.
    trace.enter_stage(segment::DB_DOC_POINTERS);
    let t = Instant::now();
    let (resolved, doc_pointer_queries) = if matches!(scope, CardScope::WholeProduct) {
        attach_task_doc_link_states_for_groups(conn, &mut [&mut tasks[..], &mut chores[..]], caller, product_id)
    } else {
        let mut queries = 0;
        let mut resolved = 0;
        for task in tasks.iter_mut().chain(chores.iter_mut()) {
            attach_task_doc_link_state(conn, task, caller, &mut queries);
            resolved += usize::from(task.doc_link_state.is_some());
        }
        (resolved, queries)
    };
    trace.record_nplus1(segment::DB_DOC_POINTERS, elapsed_ms(t), resolved, doc_pointer_queries);
    super::revision_review_card::attach(&mut tasks, chores);
    tasks
}

/// The cards a runtime patch re-read.
#[derive(Debug, Default)]
pub struct WorkRuntimePatch {
    /// Each requested card still on the board, plus the other members of its
    /// revision chain, projected as the tree projects them.
    pub items: Vec<WorkRuntimePatchItem>,
    /// Requested ids that are not live cards of the product: deleted,
    /// archived, moved to another product, or not tasks at all. A consumer
    /// holding any of them must treat that as a structural change.
    pub absent: Vec<String>,
    pub(super) validity: HashMap<String, (bool, Option<i64>)>,
}

impl WorkDb {
    /// Re-read `item_ids` (and their revision-chain relatives) of
    /// `product_id` from one database snapshot, without loading the rest of
    /// the product. Cost is proportional to the chains named, not to the
    /// product.
    pub fn get_work_runtime_patch(&self, product_id: &str, item_ids: &[String]) -> Result<WorkRuntimePatch> {
        let mut connection = self.connect()?;
        // Same consistent-snapshot discipline as the tree.
        let conn = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;

        let mut requested: Vec<&str> = Vec::new();
        let mut closure: BTreeSet<String> = BTreeSet::new();
        for id in item_ids {
            if requested.contains(&id.as_str()) {
                continue;
            }
            requested.push(id);
            // Only task ids have chains; anything else stays in the closure as
            // itself and falls out as absent when no board row matches.
            let root = chain_root(&conn, id)?;
            closure.extend(collect_chain_revision_ids(&conn, &root)?);
            closure.insert(root);
            closure.insert(id.clone());
        }

        read_cards(
            &conn,
            product_id,
            &closure,
            &requested,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
    }
}

pub(super) fn read_cards(
    conn: &Connection,
    product_id: &str,
    closure: &BTreeSet<String>,
    requested: &[&str],
    now: i64,
) -> Result<WorkRuntimePatch> {
    let select = format!(
        "SELECT {BOARD_TASK_COLUMNS} FROM tasks \
             WHERE product_id = ?1 AND id = ?2 AND ({BOARD_TASKS_LANE} OR {BOARD_CHORES_LANE}) AND {BOARD_LIVE}"
    );
    let mut tasks = Vec::new();
    let mut chores = Vec::new();
    {
        let mut stmt = conn.prepare(&select)?;
        for id in closure {
            let row = stmt
                .query_row(
                    params![product_id, id],
                    map_task_with_external_ref_parent_source_and_provenance,
                )
                .optional()?;
            match row {
                Some(task) if boss_board_projection::is_chore(&task) => chores.push(task),
                Some(task) => tasks.push(task),
                None => {}
            }
        }
    }

    tasks.sort_by(boss_board_projection::task_order);
    chores.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    let mut queries = 0u64;
    let mut runtimes = HashMap::new();
    let mut validity = HashMap::new();
    for task in tasks.iter().chain(chores.iter()) {
        let computed = dispatch_helpers::compute_task_runtime(conn, &task.id, &mut queries, now)?;
        validity.insert(task.id.clone(), (computed.cacheable, computed.expires_at));
        runtimes.insert(task.id.clone(), computed.runtime);
    }

    let mut trace = PopulationTrace::disabled();
    // A patch names a few chains; recomputing their owners is cheaper
    // than a cache pass, and keeps the patch a pure function of the
    // snapshot.
    let tasks = attach_board_derivations(
        conn,
        tasks,
        &mut chores,
        product_id,
        CardScope::Subset,
        "get_work_runtime_patch",
        &mut trace,
        None,
    );

    let present: HashSet<&str> = tasks.iter().chain(chores.iter()).map(|t| t.id.as_str()).collect();
    let absent = requested
        .iter()
        .filter(|id| !present.contains(*id))
        .map(|id| (*id).to_owned())
        .collect();

    let mut items = Vec::with_capacity(tasks.len() + chores.len());
    for (task, chore) in tasks
        .into_iter()
        .map(|task| (task, false))
        .chain(chores.into_iter().map(|task| (task, true)))
    {
        let runtime = runtimes
            .remove(&task.id)
            .expect("a runtime is read for every board row");
        let item = if chore {
            WorkItem::Chore(task)
        } else {
            WorkItem::Task(task)
        };
        items.push(WorkRuntimePatchItem { item, runtime });
    }
    Ok(WorkRuntimePatch {
        items,
        absent,
        validity,
    })
}
