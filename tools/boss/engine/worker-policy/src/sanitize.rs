//! Field-level sanitization of rows on their way out to a worker.
//!
//! ## Why a whole pass instead of edits in four handlers
//!
//! Execution and run rows are the only rows that straddle the isolation
//! boundary: their taxonomy columns (status, PR binding, timestamps) are
//! exactly what a worker needs to see, while `transcript_path` and the
//! host/pid columns belong to the runtime half that stays closed (design
//! §"Read-only model access and the exposure boundary": "Sanitization is
//! field-level where a row mixes halves").
//!
//! Applying that in each read handler would work today and rot tomorrow —
//! the next verb to return a `WorkExecution` would have to remember. So
//! sanitization runs once per outbound event, at the connection's single
//! write choke point, and therefore covers responses and topic pushes alike
//! without any handler opting in.
//!
//! ## The forbidden-key test is the real guard
//!
//! The four fields the design names are `transcript_path`, `host_id`,
//! `remote_pid`, and `shell_pid`. `transcript_path` and `artifacts_path` are
//! on the wire today: the other two are `work_executions` / `work_runs`
//! **columns that `mappers.rs` never maps into [`WorkExecution`] or
//! [`WorkRun`]**. Stripping a field that does not exist is not something the
//! type system can express, so [`SANITIZED_EXECUTION_FIELDS`] is asserted
//! against the *serialized JSON* in tests instead. If someone later adds
//! `host_id` to `WorkExecution`, that test fails and this module has to grow
//! a line — which is the outcome we want, rather than a silent leak.

use boss_protocol::{
    Attention, AttentionGroup, FrontendEvent, FrontendEventWorkerVisitor, Idea, PlannerRunSummary, Product, Project,
    Task, TaskRuntime, WorkAttentionItem, WorkExecution, WorkItemDependency, WorkRun, work_tree_delta::WorkTreeUpdate,
};

/// The JSON keys that must never appear in an execution or run row sent to a
/// worker. Asserted against serialized rows in this crate's tests, so a
/// future field addition that reintroduces one of these fails loudly.
pub const SANITIZED_EXECUTION_FIELDS: &[&str] = &[
    "transcript_path",
    "artifacts_path",
    "host_id",
    "remote_pid",
    "shell_pid",
];

/// Strip the runtime-half fields from one execution row.
///
/// A no-op today — [`WorkExecution`] carries none of
/// [`SANITIZED_EXECUTION_FIELDS`] on the wire — but it exists so the
/// sanitizing pass has an obvious place to grow when it does, and so the
/// wiring is already correct at every call site.
fn sanitize_execution(execution: WorkExecution) -> WorkExecution {
    execution
}

/// Strip the runtime-half fields from one run row.
///
/// `transcript_path` is an absolute path into the engine's transcript store,
/// and handing it to a worker would turn a taxonomy read into a
/// filesystem-level route to another execution's transcript — the exact
/// thing `TailRunTranscript` is denied for. `artifacts_path` is the same
/// class of leak: it is where `$BOSS_STRUCTURED_OUTPUT` files (review
/// verdicts, followup manifests) for a run live, so it is stripped for the
/// same reason even though no production writer populates it today.
fn sanitize_run(mut run: WorkRun) -> WorkRun {
    run.transcript_path = None;
    run.artifacts_path = None;
    run
}

/// Sanitize one outbound event at the shared response/push write boundary.
/// Every event declaration explicitly selects pass-through or a required hook;
/// the generated traversal only moves typed payloads, while policy stays here.
pub fn sanitize_event_for_worker(event: FrontendEvent) -> FrontendEvent {
    event.map_for_worker(&mut WorkerSanitizer)
}

struct WorkerSanitizer;

impl FrontendEventWorkerVisitor for WorkerSanitizer {
    fn work_tree_update(&mut self, (mut update,): (WorkTreeUpdate,)) -> (WorkTreeUpdate,) {
        if let Some(changes) = update.changes.as_object_mut() {
            if update.base_sequence.is_none() {
                changes.insert("ideas".into(), serde_json::json!([]));
            } else {
                changes.remove("ideas");
            }
        }
        (update,)
    }

    fn execution_result(&mut self, (execution,): (WorkExecution,)) -> (WorkExecution,) {
        (sanitize_execution(execution),)
    }

    fn executions_list(
        &mut self,
        (work_item_id, executions): (Option<String>, Vec<WorkExecution>),
    ) -> (Option<String>, Vec<WorkExecution>) {
        (work_item_id, executions.into_iter().map(sanitize_execution).collect())
    }

    fn execution_created(&mut self, (execution,): (WorkExecution,)) -> (WorkExecution,) {
        (sanitize_execution(execution),)
    }

    fn execution_requested(&mut self, (execution,): (WorkExecution,)) -> (WorkExecution,) {
        (sanitize_execution(execution),)
    }

    fn execution_cancelled(&mut self, (execution,): (WorkExecution,)) -> (WorkExecution,) {
        (sanitize_execution(execution),)
    }

    fn pr_review_triggered(
        &mut self,
        (execution, work_item_id, pr_url, batch_id, batch_generation, batch_execution_ids, already_active): (
            WorkExecution,
            String,
            String,
            Option<String>,
            Option<i64>,
            Vec<String>,
            bool,
        ),
    ) -> (
        WorkExecution,
        String,
        String,
        Option<String>,
        Option<i64>,
        Vec<String>,
        bool,
    ) {
        (
            sanitize_execution(execution),
            work_item_id,
            pr_url,
            batch_id,
            batch_generation,
            batch_execution_ids,
            already_active,
        )
    }

    fn run_reaped(&mut self, (run_id, execution): (String, WorkExecution)) -> (String, WorkExecution) {
        (run_id, sanitize_execution(execution))
    }

    fn run_result(&mut self, (run,): (WorkRun,)) -> (WorkRun,) {
        (sanitize_run(run),)
    }

    fn run_created(&mut self, (run,): (WorkRun,)) -> (WorkRun,) {
        (sanitize_run(run),)
    }

    fn runs_list(&mut self, (execution_id, runs): (String, Vec<WorkRun>)) -> (String, Vec<WorkRun>) {
        (execution_id, runs.into_iter().map(sanitize_run).collect())
    }

    fn work_tree(
        &mut self,
        (
            product,
            projects,
            tasks,
            chores,
            task_runtimes,
            dependencies,
            _ideas,
            attention_items,
            attention_groups,
            attention_members,
            planner_runs,
        ): (
            Product,
            Vec<Project>,
            Vec<Task>,
            Vec<Task>,
            Vec<TaskRuntime>,
            Vec<WorkItemDependency>,
            Vec<Idea>,
            Vec<WorkAttentionItem>,
            Vec<AttentionGroup>,
            Vec<Attention>,
            Vec<PlannerRunSummary>,
        ),
    ) -> (
        Product,
        Vec<Project>,
        Vec<Task>,
        Vec<Task>,
        Vec<TaskRuntime>,
        Vec<WorkItemDependency>,
        Vec<Idea>,
        Vec<WorkAttentionItem>,
        Vec<AttentionGroup>,
        Vec<Attention>,
        Vec<PlannerRunSummary>,
    ) {
        // GetWorkTree is a taxonomy read, but idea bodies are coordinator-only.
        (
            product,
            projects,
            tasks,
            chores,
            task_runtimes,
            dependencies,
            vec![],
            attention_items,
            attention_groups,
            attention_members,
            planner_runs,
        )
    }
}
