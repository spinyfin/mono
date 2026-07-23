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
//! `remote_pid`, and `shell_pid`. Only the first is on the wire today: the
//! other three are `work_executions` / `work_runs` **columns that
//! `mappers.rs` never maps into [`WorkExecution`] or [`WorkRun`]**. Stripping
//! a field that does not exist is not something the type system can express,
//! so [`SANITIZED_EXECUTION_FIELDS`] is asserted against the *serialized
//! JSON* in tests instead. If someone later adds `host_id` to
//! `WorkExecution`, that test fails and this module has to grow a line —
//! which is the outcome we want, rather than a silent leak.

use boss_protocol::{FrontendEvent, WorkExecution, WorkRun};

/// The JSON keys that must never appear in an execution or run row sent to a
/// worker. Asserted against serialized rows in this crate's tests, so a
/// future field addition that reintroduces one of these fails loudly.
pub const SANITIZED_EXECUTION_FIELDS: &[&str] = &["transcript_path", "host_id", "remote_pid", "shell_pid"];

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
/// `transcript_path` is the live one: it is an absolute path into the
/// engine's transcript store, and handing it to a worker would turn a
/// taxonomy read into a filesystem-level route to another execution's
/// transcript — the exact thing `TailRunTranscript` is denied for.
fn sanitize_run(mut run: WorkRun) -> WorkRun {
    run.transcript_path = None;
    run
}

/// Sanitize one outbound event for a worker-tier connection.
///
/// Every other variant passes through untouched: they either carry no rows
/// that straddle the boundary, or they are replies to verbs a worker cannot
/// call in the first place (which is belt-and-braces — the verb gate already
/// stopped those, so this pass only has to be correct for the events a
/// worker can actually elicit).
pub fn sanitize_event_for_worker(event: FrontendEvent) -> FrontendEvent {
    match event {
        FrontendEvent::ExecutionResult { execution } => FrontendEvent::ExecutionResult {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::ExecutionsList {
            work_item_id,
            executions,
        } => FrontendEvent::ExecutionsList {
            work_item_id,
            executions: executions.into_iter().map(sanitize_execution).collect(),
        },
        FrontendEvent::ExecutionCreated { execution } => FrontendEvent::ExecutionCreated {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::ExecutionRequested { execution } => FrontendEvent::ExecutionRequested {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::ExecutionCancelled { execution } => FrontendEvent::ExecutionCancelled {
            execution: sanitize_execution(execution),
        },
        FrontendEvent::PrReviewTriggered {
            execution,
            work_item_id,
            pr_url,
        } => FrontendEvent::PrReviewTriggered {
            execution: sanitize_execution(execution),
            work_item_id,
            pr_url,
        },
        FrontendEvent::RunReaped { run_id, execution } => FrontendEvent::RunReaped {
            run_id,
            execution: sanitize_execution(execution),
        },
        FrontendEvent::RunResult { run } => FrontendEvent::RunResult { run: sanitize_run(run) },
        FrontendEvent::RunCreated { run } => FrontendEvent::RunCreated { run: sanitize_run(run) },
        FrontendEvent::RunsList { execution_id, runs } => FrontendEvent::RunsList {
            execution_id,
            runs: runs.into_iter().map(sanitize_run).collect(),
        },
        other => other,
    }
}
