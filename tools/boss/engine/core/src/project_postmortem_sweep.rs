//! Periodic reconciler that auto-schedules a `design_postmortem` task when
//! a project's implementation work drains to zero.
//!
//! Completion signals are recorded transactionally when open work closes,
//! leaves a project, is deleted or archived, or the project is marked done.
//! The sweep reconciles those signals even across restarts. Existing databases
//! retain the timestamp watermark fallback for completions predating signals;
//! installation never backfills historical projects.
//!
//! Scheduling rechecks all live task kinds and active planning in an immediate
//! transaction shared with the operator command. Any prior postmortem, including
//! a completed or deleted one, makes scheduling a no-op.
//!
//! Archived projects and projects without design docs remain ineligible.
//! The project_postmortem_sweep feature flag gates automatic scheduling only.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::work::{Product, Project, ProjectStatus, TaskKind, TaskStatus, WorkDb};

/// Interval between sweep passes. Postmortem scheduling is not latency
/// sensitive (it fires once a whole wave of implementation work has
/// finished), so this runs far less often than the dependency/dispatch
/// safety-net sweeps.
pub const PROJECT_POSTMORTEM_SWEEP_INTERVAL_SECS: u64 = 300;

/// Counters from one sweep pass.
#[derive(Debug, Default)]
pub struct ProjectPostmortemSweepOutcome {
    /// Number of projects evaluated this pass (design doc set, live tasks
    /// exist).
    pub projects_evaluated: usize,
    /// Number of `design_postmortem` tasks created this pass.
    pub postmortems_created: usize,
}

impl crate::sweep_loop::SweepOutcome for ProjectPostmortemSweepOutcome {
    fn has_activity(&self) -> bool {
        self.postmortems_created > 0
    }

    fn log(&self) {
        tracing::info!(
            projects_evaluated = self.projects_evaluated,
            postmortems_created = self.postmortems_created,
            "project-postmortem sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a wave that finished while the engine was
/// down is reconciled at boot.
///
/// `kick_fn` is called whenever the pass creates at least one postmortem
/// task, so the coordinator scheduler picks up its (autostart) execution
/// immediately rather than waiting for the next dispatch-triggering event.
///
/// `feature_flags` is re-checked every pass (not just at spawn time) so
/// flipping the `project_postmortem_sweep` kill switch off takes effect
/// within one `interval` without restarting the engine.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    interval: Duration,
    kick_fn: Arc<dyn Fn() + Send + Sync>,
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let kick_fn = Arc::clone(&kick_fn);
        let feature_flags = Arc::clone(&feature_flags);
        async move {
            if !feature_flags.is_enabled("project_postmortem_sweep") {
                return ProjectPostmortemSweepOutcome::default();
            }
            let outcome = run_one_pass(work_db.as_ref()).await;
            if outcome.postmortems_created > 0 {
                kick_fn();
            }
            outcome
        }
    })
}

/// Metadata-table key (see `WorkDb::get_metadata`/`set_metadata`) holding
/// the sweep's high-water mark: the epoch-seconds instant this sweep first
/// ever ran against this database. See [`ensure_watermark`] for why this
/// exists.
const WATERMARK_METADATA_KEY: &str = "project_postmortem_sweep_watermark";

/// Establish (on first call, ever, against this database) or read back the
/// sweep's high-water mark, persisted via the engine's generic metadata KV.
///
/// The legacy completion timestamp fallback is bounded by this watermark rather
/// than by all of time, so a database that predates this feature is never
/// retroactively backfilled. Set once, the first time the sweep ever runs
/// against a given database, and never moves again: a wave that completes
/// while the engine is briefly down is still `> watermark` and reconciles
/// normally on the next boot (the behaviour the module doc's "fires
/// immediately on spawn" comment describes), while a project that drained
/// before the feature existed is `<= watermark` and is correctly never
/// backfilled. See incident postmortem-archived-fanout-2026-07-20.
fn ensure_watermark(work_db: &WorkDb) -> anyhow::Result<i64> {
    if let Some(existing) = work_db.get_metadata(WATERMARK_METADATA_KEY)? {
        return existing.parse::<i64>().with_context(|| {
            format!("project-postmortem sweep watermark is not a valid epoch-seconds integer: {existing}")
        });
    }
    let now: i64 = crate::work::now_string()
        .parse()
        .context("now_string() did not produce a valid epoch-seconds integer")?;
    work_db.set_metadata(WATERMARK_METADATA_KEY, &now.to_string())?;
    tracing::info!(
        watermark = now,
        "project-postmortem sweep: established backfill watermark on first-ever pass"
    );
    Ok(now)
}

/// Run a single sweep pass over every product's projects. Returns per-pass
/// counters for the caller to log or assert in tests.
pub async fn run_one_pass(work_db: &WorkDb) -> ProjectPostmortemSweepOutcome {
    let mut outcome = ProjectPostmortemSweepOutcome::default();

    let watermark = match ensure_watermark(work_db) {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!(
                ?err,
                "project-postmortem sweep: failed to establish watermark; skipping pass"
            );
            return outcome;
        }
    };

    let products = match work_db.list_products() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(?err, "project-postmortem sweep: failed to list products; skipping pass");
            return outcome;
        }
    };

    for product in &products {
        let projects = match work_db.list_projects(&product.id, None) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    product_id = %product.id,
                    ?err,
                    "project-postmortem sweep: failed to list projects; skipping product",
                );
                continue;
            }
        };
        for project in &projects {
            if project.design_doc_path.as_deref().unwrap_or_default().is_empty() {
                // No design doc yet — precondition #4: skip rather than error.
                continue;
            }
            match evaluate_project(work_db, product, project, watermark).await {
                Ok(EvalOutcome::Skipped) => {}
                Ok(EvalOutcome::Evaluated) => outcome.projects_evaluated += 1,
                Ok(EvalOutcome::Scheduled) => {
                    outcome.projects_evaluated += 1;
                    outcome.postmortems_created += 1;
                }
                Err(err) => {
                    tracing::warn!(
                        project_id = %project.id,
                        ?err,
                        "project-postmortem sweep: failed to evaluate project; skipping",
                    );
                }
            }
        }
    }

    outcome
}

enum EvalOutcome {
    /// Design doc missing (already filtered before this point), trigger
    /// count non-zero, dedup gate hit, or zero net completions — no action
    /// taken, not counted as an evaluated candidate.
    Skipped,
    /// Trigger count was zero and the project was a genuine candidate, but
    /// the precondition (dedup gate / zero net completions) held off
    /// scheduling.
    Evaluated,
    /// A new `design_postmortem` task was created.
    Scheduled,
}

async fn evaluate_project(
    work_db: &WorkDb,
    product: &Product,
    project: &Project,
    watermark: i64,
) -> anyhow::Result<EvalOutcome> {
    if project.status == ProjectStatus::Archived {
        // The kanban board only ever shows a project-scoped task via its
        // parent project's lane, and archived projects are filtered out of
        // that lane by default (`ChatViewModel.projectsForSelectedProduct`).
        // A postmortem scheduled here would be a live work item with a
        // dispatched worker that is not visible or steerable on the board,
        // so skip rather than create work nobody can act on.
        return Ok(EvalOutcome::Skipped);
    }
    if work_db.last_design_postmortem_for_project(&project.id)?.is_some() {
        return Ok(EvalOutcome::Evaluated);
    }
    let signaled = work_db.has_project_postmortem_signal(&project.id)?;
    let tasks = work_db.list_project_trigger_tasks(&product.id, &project.id)?;
    let recent_completion = tasks.iter().any(|task| {
        matches!(task.kind, TaskKind::ProjectTask | TaskKind::Investigation)
            && task.status == TaskStatus::Done
            && epoch_secs(task.completed_at.as_deref()).is_some_and(|time| time > watermark)
    });
    if !signaled && !recent_completion {
        return Ok(EvalOutcome::Skipped);
    }
    // Recheck every task kind and planning, serializing with insertion.
    let (created, inserted) = match work_db.start_project_postmortem(&project.id) {
        Ok(result) => result,
        Err(err) => {
            tracing::debug!(project_id = %project.id, %err, "project postmortem not ready");
            return Ok(EvalOutcome::Evaluated);
        }
    };
    if !inserted {
        return Ok(EvalOutcome::Evaluated);
    }

    // The new task is `autostart = true, status = todo`; give it an
    // execution row now rather than waiting for the next unrelated
    // invalidation to trigger `reconcile_product_executions`.
    if let Err(err) = work_db.reconcile_product_executions(&product.id) {
        tracing::warn!(
            product_id = %product.id,
            task_id = %created.id,
            ?err,
            "project-postmortem sweep: failed to reconcile executions after scheduling postmortem",
        );
    }

    Ok(EvalOutcome::Scheduled)
}

/// Parse a `completed_at`-shaped column (decimal epoch-seconds string) into
/// an `i64` for numeric (not lexical) comparison.
fn epoch_secs(value: Option<&str>) -> Option<i64> {
    value.and_then(|s| s.parse::<i64>().ok())
}

/// Compose the postmortem task's `description` — the remit brief that
/// surfaces to the worker via `runner::work_item_details`'s `- details:`
/// block. Lists every PR the review must cover so the worker doesn't have
/// to rediscover the wave from scratch.
pub(crate) fn compose_postmortem_brief(project: &Project, merged_prs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Design postmortem for project \"{}\": review the project's merged PRs and update the project's design doc to reflect what actually shipped — decisions that diverged, scope added or dropped, and contracts that evolved during implementation. Also flag any uncompleted work the review surfaces (see the required structured-output section in your instructions below) so the engine can schedule it.\n\n",
        project.name
    ));
    if let Some(path) = project.design_doc_path.as_deref().filter(|p| !p.is_empty()) {
        out.push_str(&format!("Design doc: `{path}`\n"));
    }
    out.push_str("Merged PRs to review:\n");
    for (name, url) in merged_prs {
        out.push_str(&format!("- {name}: {url}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::*;
    use crate::work::{CreateProjectInput, CreateTaskInput, SetProjectDesignDocInput, Task, WorkItemPatch};

    fn set_design_doc(db: &WorkDb, project_id: &str) {
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project_id.to_owned(),
            design_doc_path: Some("docs/designs/alpha.md".to_owned()),
            ..Default::default()
        })
        .unwrap();
    }

    fn create_project_no_seed(db: &WorkDb, product_id: &str, name: &str) -> boss_protocol::Project {
        db.create_project(
            CreateProjectInput::builder()
                .product_id(product_id)
                .name(name)
                .no_design_task(true)
                .build(),
        )
        .unwrap()
    }

    /// Seed the sweep's boot-time-backfill watermark (see
    /// [`ensure_watermark`]) to `value` before the first [`run_one_pass`]
    /// call in a test. Tests that exercise "no prior postmortem" with
    /// small, deterministic forced timestamps (via
    /// `force_completed_at_for_test`) need this — otherwise the watermark
    /// establishes itself at the real wall-clock "now" on first run, which
    /// dwarfs any small forced value and makes the trigger-task look like
    /// it predates the watermark, exactly as intended for a *genuinely*
    /// old completion but wrongly for a test's synthetic one.
    fn seed_watermark(db: &WorkDb, value: i64) {
        db.set_metadata(WATERMARK_METADATA_KEY, &value.to_string()).unwrap();
    }

    fn create_done_project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str, pr_url: &str) -> Task {
        let task = db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product_id)
                    .project_id(project_id)
                    .name(name)
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                pr_url: Some(pr_url.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        match db.get_work_item(&task.id).unwrap() {
            boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    /// No design doc set → skipped even though every task is terminal.
    #[tokio::test]
    async fn skips_project_without_design_doc() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.projects_evaluated, 0);
        assert_eq!(outcome.postmortems_created, 0);
    }

    /// First wave: design doc set, one project_task done with a PR, no
    /// prior postmortem → schedules one.
    #[tokio::test]
    async fn schedules_postmortem_on_first_completed_wave() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        // Watermark seeded in the deep past so this "first ever wave"
        // reads as genuinely new work, not pre-existing history — see
        // `seed_watermark`'s doc comment.
        seed_watermark(&db, 0);
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 1);

        let trigger_tasks = db.list_project_trigger_tasks(&product_id, &project.id).unwrap();
        // The trigger-task query only returns project_task/design/
        // investigation — the postmortem itself must never appear here
        // (rule 2), regardless of what `list_tasks` (the general
        // CLI-listing surface, which now also returns design_postmortem
        // rows — see the workitems.rs allowlist fix) returns.
        assert!(trigger_tasks.iter().all(|t| t.kind != TaskKind::DesignPostmortem));

        let last = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert_eq!(last.status, TaskStatus::Todo);
        assert!(last.description.contains("pull/1"));
    }

    /// Non-terminal task present → trigger count is non-zero, no schedule.
    #[tokio::test]
    async fn does_not_schedule_while_tasks_are_open() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        db.create_task(
            CreateTaskInput::builder()
                .product_id(&product_id)
                .project_id(&project.id)
                .name("still open")
                .build(),
        )
        .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 0);
    }

    fn project_status(db: &WorkDb, project_id: &str) -> ProjectStatus {
        match db.get_work_item(project_id).unwrap() {
            boss_protocol::WorkItem::Project(p) => p.status,
            other => panic!("expected a project, got {other:?}"),
        }
    }

    /// Trigger-task drain schedules a postmortem but never certifies the
    /// project complete. In production a planner can be between claiming its
    /// run and materializing open tasks at this exact point; the trigger query
    /// also deliberately excludes revision and postmortem rows.
    #[tokio::test]
    async fn trigger_task_drain_does_not_mark_project_done() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        seed_watermark(&db, 0);
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");
        assert_eq!(project_status(&db, &project.id), ProjectStatus::Planned);

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 1);
        assert_eq!(
            project_status(db.as_ref(), &project.id),
            ProjectStatus::Planned,
            "postmortem scheduling is not authority to certify project completion"
        );
    }

    /// A single non-terminal task keeps the project `planned` — the drain
    /// predicate has not fired.
    #[tokio::test]
    async fn does_not_advance_project_while_any_task_is_open() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        seed_watermark(&db, 0);
        create_done_project_task(
            &db,
            &product_id,
            &project.id,
            "impl done",
            "https://github.com/o/r/pull/1",
        );
        db.create_task(
            CreateTaskInput::builder()
                .product_id(&product_id)
                .project_id(&project.id)
                .name("still open")
                .build(),
        )
        .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 0);
        assert_eq!(
            project_status(db.as_ref(), &project.id),
            ProjectStatus::Planned,
            "open work must leave the project planned"
        );
    }

    /// Operator-driven `blocked` is not changed by the scheduling sweep.
    #[tokio::test]
    async fn does_not_overwrite_operator_blocked_status() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        seed_watermark(&db, 0);
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");
        db.update_work_item(
            &project.id,
            WorkItemPatch {
                status: Some("blocked".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        assert_eq!(project_status(&db, &project.id), ProjectStatus::Blocked);

        let db = Arc::new(db);
        let _ = run_one_pass(db.as_ref()).await;
        assert_eq!(
            project_status(db.as_ref(), &project.id),
            ProjectStatus::Blocked,
            "postmortem scheduling must not change project status"
        );
    }

    /// Dedup gate: an open postmortem already exists → skip even though
    /// the trigger count is zero.
    #[tokio::test]
    async fn dedup_gate_skips_while_postmortem_open() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");

        db.create_design_postmortem(&product_id, &project.id, &project.name, "existing".to_owned())
            .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "must not double-schedule while one is open"
        );
    }

    /// The project has one postmortem even if more work is completed later.
    #[tokio::test]
    async fn completed_postmortem_is_not_recreated_after_new_work() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        // Watermark seeded in the deep past (see `seed_watermark`) so the
        // forced `1_000`-epoch timestamps below read as after it, not
        // as pre-existing history the boot-time-backfill bound excludes.
        seed_watermark(&db, 0);
        let task1 = create_done_project_task(&db, &product_id, &project.id, "impl 1", "https://github.com/o/r/pull/1");
        // `completed_at` is epoch-seconds resolution, and this test's two
        // task completions and one postmortem completion all happen
        // in-process with no real elapsed time between them — force
        // strictly increasing timestamps rather than relying on real
        // wall-clock ordering, which would make the "since last postmortem"
        // cutoff comparison racy (see `force_completed_at_for_test`'s doc
        // comment).
        db.force_completed_at_for_test(&task1.id, 1_000).unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 1);
        let first_pm = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();

        // Complete the postmortem itself, strictly after task1.
        db.update_work_item(
            &first_pm.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                pr_url: Some("https://github.com/o/r/pull/2".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.force_completed_at_for_test(&first_pm.id, 2_000).unwrap();

        // No new work: must not re-fire.
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 0, "zero net new work must not re-trigger");

        // A fresh wave lands and drains, strictly after the postmortem.
        let task2 = create_done_project_task(&db, &product_id, &project.id, "impl 2", "https://github.com/o/r/pull/3");
        db.force_completed_at_for_test(&task2.id, 3_000).unwrap();
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "a completed postmortem must still prevent a duplicate"
        );
        let existing = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert_eq!(existing.id, first_pm.id);
    }

    /// The postmortem task's own completion must never count as
    /// "implementation work" that re-arms the trigger, and the seed
    /// `design` task's completion must not either (rule 2 / precondition).
    #[tokio::test]
    async fn design_and_postmortem_kinds_never_feed_the_precondition() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        // Real design task this time (not the no-seed helper) so its
        // completion is on the record as `kind = design`.
        let project = db
            .create_project(
                CreateProjectInput::builder()
                    .product_id(&product_id)
                    .name("Alpha")
                    .build(),
            )
            .unwrap();
        set_design_doc(&db, &project.id);
        let design_task = db
            .list_tasks(&product_id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .unwrap();
        db.update_work_item(
            &design_task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                pr_url: Some("https://github.com/o/r/pull/1".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "design task completing alone (no project_task/investigation work) must not trigger a postmortem"
        );
    }

    /// Regression test for incident postmortem-archived-fanout-2026-07-20:
    /// the engine's first sweep pass after a restart found many already-
    /// drained, archived projects (accumulated from before this feature
    /// existed) and fanned out a postmortem to every one of them at once.
    /// This is the verification bar from that incident's chore: boot
    /// against a database with many archived, fully-drained projects and
    /// confirm zero postmortems are created.
    #[tokio::test]
    async fn boot_creates_zero_postmortems_for_many_archived_drained_projects() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        for i in 0..8 {
            let project = create_project_no_seed(&db, &product_id, &format!("Archived {i}"));
            set_design_doc(&db, &project.id);
            let task = create_done_project_task(
                &db,
                &product_id,
                &project.id,
                &format!("impl {i}"),
                &format!("https://github.com/o/r/pull/{i}"),
            );
            db.force_completed_at_for_test(&task.id, 1_000 + i).unwrap();
            db.update_work_item(
                &project.id,
                WorkItemPatch {
                    status: Some("archived".to_owned()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        }

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "archived projects must never be backfilled with a postmortem, however many are drained at once"
        );
    }

    /// Defect 2 (boot-time backfill is unbounded): a project that fully
    /// drained long before this sweep ever ran must not retroactively get
    /// a postmortem the first time the sweep executes against the
    /// database — only work that completes after the watermark should
    /// ever count for a project with no postmortem history.
    #[tokio::test]
    async fn first_pass_does_not_backfill_postmortem_for_project_drained_before_sweep_existed() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        // No `seed_watermark` call here — this test deliberately lets the
        // watermark establish itself at the real "now" of the first
        // `run_one_pass` call below, and forces this task's completion far
        // in the past relative to that, simulating a project that drained
        // long before the sweep code ever ran.
        let old_task = create_done_project_task(
            &db,
            &product_id,
            &project.id,
            "ancient impl",
            "https://github.com/o/r/pull/1",
        );
        db.force_completed_at_for_test(&old_task.id, 1_000).unwrap();
        // Model history from before completion signals were installed.
        db.connect()
            .unwrap()
            .execute("DELETE FROM project_postmortem_signals", [])
            .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "a project that drained before the sweep's first-ever pass must not be backfilled"
        );

        // Read the watermark the first pass just established, and place a
        // second, genuinely-new wave deterministically after it (avoids
        // racing real wall-clock time against the one-second resolution of
        // `completed_at`).
        let watermark: i64 = db
            .get_metadata(WATERMARK_METADATA_KEY)
            .unwrap()
            .expect("watermark must be persisted after the first pass")
            .parse()
            .unwrap();

        let new_task = create_done_project_task(
            &db,
            &product_id,
            &project.id,
            "new impl",
            "https://github.com/o/r/pull/2",
        );
        db.force_completed_at_for_test(&new_task.id, watermark + 10).unwrap();

        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 1,
            "work completed after the watermark must still trigger normally — the bound must not be permanent"
        );
        let pm = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert!(pm.description.contains("pull/2"));
        assert!(
            pm.description.contains("pull/1"),
            "the project postmortem must cover its earlier completed work too"
        );
    }

    /// Defect 4 (delete re-arms the trigger): soft-deleting the project's
    /// only postmortem must not make the next pass treat the already-
    /// reviewed wave of work as new and re-schedule against it.
    #[tokio::test]
    async fn deleting_postmortem_does_not_recreate_on_next_pass() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        seed_watermark(&db, 0);
        let task = create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");
        db.force_completed_at_for_test(&task.id, 1_000).unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 1);
        let first_pm = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();

        db.delete_work_item(&first_pm.id).unwrap();

        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "deleting the only postmortem must not re-arm the trigger for already-reviewed work"
        );
    }

    /// Deletion is a durable dismissal, including after new work completes.
    #[tokio::test]
    async fn deleted_postmortem_is_not_recreated_after_new_work() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        seed_watermark(&db, 0);
        let task1 = create_done_project_task(&db, &product_id, &project.id, "impl 1", "https://github.com/o/r/pull/1");
        db.force_completed_at_for_test(&task1.id, 1_000).unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.postmortems_created, 1);
        let first_pm = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        db.delete_work_item(&first_pm.id).unwrap();

        let cutoff: i64 = first_pm.created_at.parse().unwrap();
        let task2 = create_done_project_task(&db, &product_id, &project.id, "impl 2", "https://github.com/o/r/pull/2");
        db.force_completed_at_for_test(&task2.id, cutoff + 10).unwrap();

        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(
            outcome.postmortems_created, 0,
            "a deleted postmortem must still prevent a duplicate"
        );
        let existing = db.last_design_postmortem_for_project(&project.id).unwrap().unwrap();
        assert_eq!(existing.id, first_pm.id);
    }

    /// Defect 3 (kill switch): with `project_postmortem_sweep` disabled via
    /// [`crate::feature_flags::FeatureFlagsStore`], [`spawn_loop`] must not
    /// run a pass or invoke `kick_fn`, even though the project is a genuine
    /// trigger candidate.
    #[tokio::test(start_paused = true)]
    async fn spawn_loop_flag_disabled_skips_pass_and_never_kicks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::feature_flags::FeatureFlagsStore;

        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let project = create_project_no_seed(&db, &product_id, "Alpha");
        set_design_doc(&db, &project.id);
        seed_watermark(&db, 0);
        let task = create_done_project_task(&db, &product_id, &project.id, "impl", "https://github.com/o/r/pull/1");
        db.force_completed_at_for_test(&task.id, 1_000).unwrap();
        let db = Arc::new(db);

        let flags_dir = tempfile::TempDir::new().unwrap();
        let feature_flags = Arc::new(FeatureFlagsStore::new(flags_dir.path().join("feature-flags.toml")));
        feature_flags.set("project_postmortem_sweep", false).unwrap();

        let kick_calls = Arc::new(AtomicUsize::new(0));
        let kick_calls_c = Arc::clone(&kick_calls);
        let kick_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            kick_calls_c.fetch_add(1, Ordering::SeqCst);
        });

        let handle = spawn_loop(Arc::clone(&db), Duration::from_secs(300), kick_fn, feature_flags);

        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        assert!(
            db.last_design_postmortem_for_project(&project.id).unwrap().is_none(),
            "kill switch: no postmortem must be created while the flag is disabled"
        );
        assert_eq!(
            kick_calls.load(Ordering::SeqCst),
            0,
            "kick_fn must never fire while the sweep is disabled"
        );

        handle.abort();
    }
}
