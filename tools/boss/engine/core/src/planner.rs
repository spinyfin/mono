//! The Planner — a reusable LLM "mini-coordinator".
//!
//! Given a merged design doc plus project/product context, the Planner
//! proposes the project's implementation task graph: the tasks to create
//! (with effort levels and kinds) and the dependency edges that let work
//! proceed in parallel. It is the automated stand-in for a human
//! coordinator who would otherwise read the doc by hand and type out
//! `boss task create` / `boss task depend add` calls.
//!
//! See `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
//! (project P783) §2 "The Planner". This module is task 3 of that design.
//!
//! ## Pure transform, no writes
//!
//! [`Planner::plan`] takes the typed [`PlannerInput`] (defined in
//! `boss-protocol`) and returns a typed [`PlannerOutput`]. It performs no
//! writes and has no knowledge of the trigger that invoked it — the
//! deterministic *Materializer* (a sibling task) is the only thing that
//! writes rows. Keeping the Planner a pure prose-to-typed-graph transform
//! is what makes the auto-populate feature testable, idempotent, and safe.
//!
//! ## Rides the shared `claude_client` pipeline
//!
//! All Anthropic transport goes through [`crate::claude_client`] — the single
//! engine-wide Messages API pipeline (process-wide client, pinned API version,
//! retry/backoff). The Planner owns only its prompt and schema: it builds the
//! forced-tool-call body, sends it via
//! [`claude_client::send_messages_raw`](crate::claude_client::send_messages_raw),
//! and maps the shared [`ClaudeError`](crate::claude_client::ClaudeError) into a
//! typed [`PlannerOutcome`] rather than an `anyhow::Result` that would erase the
//! distinction the caller (the Populator, a sibling task) needs to record the
//! right `planner_runs` outcome. The design names the entry point
//! `Planner::plan(PlannerInput) -> Result<PlannerOutput>`; we return the richer
//! [`PlannerOutcome`] enum to honour the design's adjacent requirement to
//! "return typed outcomes (`NoApiKey`, `ApiError`, success)".
//!
//! ## Structured output is enforced, not requested
//!
//! The call forces a single tool call (`tool_choice: {type: "tool"}`) whose
//! `input_schema` is [`planner_output_schema`]. The model is therefore
//! obligated to emit the [`PlannerOutput`] shape, which we deserialise
//! directly into the Rust type — a deserialisation failure is a validation
//! failure ([`PlannerOutcome::InvalidOutput`]), never a parse-and-hope over
//! free-form markdown.
//!
//! A healthy proposal has nevertheless been observed to fail whole because an
//! array-typed field came back as a single JSON-encoded string rather than a
//! JSON array — observed on `effort_audit` in production, and guarded against
//! on the fields still deserialized from the model (`tasks`, `edges`) as a
//! precaution — model flakiness on an otherwise-valid result, not a prompt or
//! schema defect. Mitigations, in order: (1) the model is no longer asked to
//! emit `effort_audit` at all — [`planner_output_schema`] omits it from both
//! `required` and `properties`, and [`planner_output_from_response`]
//! overwrites the raw `effort_audit` key (if the model sends one anyway)
//! before deserialising, so [`PlannerOutput::effort_audit`] instead comes
//! from [`derive_effort_audit`], which reads it back out of each task's
//! `description` — the array was always redundant with data already present
//! there, so its shape can never fail deserialization at all (mirrors
//! `pr_review`'s `suspected_deletions` fix); (2)
//! [`coerce_stringified_array_fields`] rewrites a remaining known-array field
//! (`tasks`, `edges`) back into an array, when the string itself parses as
//! one, before schema validation runs; (3) if validation still fails,
//! [`plan_with_url`] retries (bounded by [`PLANNER_VALIDATION_ATTEMPTS`])
//! with the validation error fed back into the prompt; (4) if the *final*
//! attempt is still schema-invalid, it falls back to the last schema-valid
//! proposal an earlier attempt produced (if any) rather than discarding a
//! stageable plan — only when no attempt ever produced a schema-valid output
//! does the run fail whole ([`PlannerOutcome::InvalidOutput`]).
//!
//! ## Bounded model / effort / timeout
//!
//! Planning quality matters and the call is infrequent (once per project),
//! so the Planner defaults to a strong model (Opus) rather than the Haiku
//! that `live_status` uses for its cheap one-liner. The model, effort,
//! `max_tokens`, timeout, and retry count are all single constants, tunable
//! without a schema change (design R5).

use std::time::Duration;

use serde_json::{Value, json};

use boss_protocol::{PlannerInput, PlannerOutput, planner_output_schema};

use crate::claude_client::{self, CallConfig, ClaudeError, MessagesResponse, RetryPolicy};
use boss_engine_planner_validation::{OversizeFinding, detect_oversize_tasks};

/// The model the Planner runs on. A direct API call needs a concrete model
/// id (the `--model` family aliases used for worker dispatch are resolved by
/// the `claude` CLI, not the Messages API), so this is pinned rather than an
/// alias. Opus is deliberate: planning quality matters and the call is
/// infrequent (once per project), unlike the Haiku one-liner in
/// [`crate::live_status`]. Tunable here without a schema change (design R5).
pub const PLANNER_MODEL: &str = "claude-opus-4-8";

/// `output_config.effort` for the planning call (design "bound … effort").
/// `high` is the recommended minimum for intelligence-sensitive work;
/// extracting a typed task graph from prose is intelligence-sensitive but
/// bounded, so we do not spend up at `xhigh`/`max`.
pub const PLANNER_EFFORT: &str = "high";

/// Output ceiling. A breakdown of up to ~30 tasks — each with a description
/// plus its `[effort-classification]` line — plus the edge set and notes
/// fits comfortably here, and staying at/under ~16K keeps the non-streaming
/// request under the SDK/HTTP timeout envelope.
pub const PLANNER_MAX_TOKENS: u32 = 16_384;

/// Wall-clock budget for one planning round trip. A high-effort Opus call
/// over a full design doc is far slower than the `live_status` one-liner, so
/// this is generous — but still bounded so a wedged call cannot hang the
/// caller indefinitely (design "bound … timeout").
pub const PLANNER_TIMEOUT: Duration = Duration::from_secs(180);

/// Total attempts per Anthropic round trip: the design says "retry once,
/// then fail safe", i.e. two attempts. Only transient failures (429/5xx/
/// overloaded/transport) are retried at this layer — see
/// [`ClaudeError::is_retryable`]. This is independent of
/// [`PLANNER_VALIDATION_ATTEMPTS`], which bounds a *different* retry: a
/// schema-invalid response is not a transport failure, so it is not retried
/// here.
pub const PLANNER_ATTEMPTS: u32 = 2;

/// Backoff before the planning retry. A single retry of an infrequent,
/// high-effort call can afford a real pause before hammering the API again.
pub const PLANNER_BACKOFF: Duration = Duration::from_millis(500);

/// Total attempts across the outer output-acceptance retry loop in
/// [`Planner::plan`]. Two distinct rejection modes share this one bounded
/// loop, each re-prompting with the rejection reason fed back to the model:
///
/// 1. **Schema-invalid output** — a model occasionally emits a tool call that
///    violates [`planner_output_schema`] (observed: an array-typed field like
///    `edges` emitted as a single JSON-encoded string; historically also
///    `effort_audit`, before the model stopped being asked to emit it — see
///    the module doc). That is model flakiness, not a transient transport
///    error, so [`PLANNER_ATTEMPTS`]'s 429/5xx retry never sees it and a
///    single miss used to fail the whole proposal.
/// 2. **Oversize tasks (the decomposition gate)** — a schema-valid proposal
///    that packs a monolithic "project in disguise" task
///    ([`detect_oversize_tasks`]). The retry asks the model to decompose it
///    into dependency-ordered, single-subsystem, single-PR tasks.
///
/// Bounded at 2 (one retry): the retry re-sends the request with the
/// rejection reason appended to the prompt, so the model can see and correct
/// exactly what it got wrong. An oversize proposal that survives the budget
/// is accepted best-effort (the valid, operator-reviewed plan is not worth
/// discarding over an imperfect split). A schema failure that survives the
/// budget falls back to the last schema-valid proposal seen on an earlier
/// attempt, if any (same best-effort policy — a validation-clean, merely
/// oversize proposal from attempt 1 must not be thrown away just because
/// attempt 2 flaked on schema); only when NO attempt ever produced a
/// schema-valid output does the run fail ([`PlannerOutcome::InvalidOutput`]).
pub const PLANNER_VALIDATION_ATTEMPTS: u32 = 2;

/// Name of the forced tool whose `input_schema` is [`planner_output_schema`].
/// The model must call exactly this tool; its `input` is the structured
/// [`PlannerOutput`].
pub const TOOL_NAME: &str = "emit_task_graph";

/// One-line tool description shown to the model alongside the schema.
const TOOL_DESCRIPTION: &str = "Emit the proposed implementation task graph extracted from the design \
     document: the tasks to create (with kind and effort), the dependency \
     edges between them by handle, the confidence, whether a breakdown was \
     found, the per-task [effort-classification] audit lines, and a notes \
     rationale.";

/// Distinguishable outcomes for one planning call. Mirrors
/// [`crate::live_status::SummarizerOutcome`]: the caller (the Populator)
/// needs to tell "no API key" from "model 429" from "succeeded" so it can
/// record the right `planner_runs.outcome` and surface the right attention
/// item. A bare `anyhow::Result<PlannerOutput>` would erase that.
#[derive(Debug, Clone)]
pub enum PlannerOutcome {
    /// The model returned a schema-valid [`PlannerOutput`].
    Success(PlannerOutput),
    /// No `ANTHROPIC_API_KEY` was configured on the engine. The feature
    /// degrades to "design pointer set, tasks not auto-created" and the
    /// caller surfaces an attention item asking the operator to configure
    /// the key — exactly as `live_status` degrades.
    NoApiKey,
    /// Anthropic returned a non-2xx response. `status` is the numeric code
    /// (e.g. 401, 429, 529); `snippet` is the first ~200 chars of the body.
    ApiError { status: u16, snippet: String },
    /// The HTTP client failed before/while getting a response (timeout, TLS,
    /// DNS, connection reset), or the response body could not be decoded.
    Transport(String),
    /// A response arrived but the model did not call [`TOOL_NAME`], or its
    /// tool input did not deserialise into [`PlannerOutput`]. Treated as a
    /// validation failure, not a transport error.
    InvalidOutput(String),
}

impl PlannerOutcome {
    /// Short tag for logs and the `planner_runs` audit row.
    pub fn tag(&self) -> &'static str {
        match self {
            PlannerOutcome::Success(_) => "success",
            PlannerOutcome::NoApiKey => "no_api_key",
            PlannerOutcome::ApiError { .. } => "api_error",
            PlannerOutcome::Transport(_) => "transport_error",
            PlannerOutcome::InvalidOutput(_) => "invalid_output",
        }
    }

    /// Human-readable detail for logs and the operator-facing audit record.
    pub fn detail(&self) -> String {
        match self {
            PlannerOutcome::Success(out) => {
                format!(
                    "{} task(s), {} edge(s), confidence={}, breakdown_found={}",
                    out.tasks.len(),
                    out.edges.len(),
                    out.confidence,
                    out.breakdown_found,
                )
            }
            PlannerOutcome::NoApiKey => "ANTHROPIC_API_KEY not configured on the engine".to_owned(),
            PlannerOutcome::ApiError { status, snippet } => {
                format!("anthropic returned {status}: {snippet}")
            }
            PlannerOutcome::Transport(err) => err.clone(),
            PlannerOutcome::InvalidOutput(err) => err.clone(),
        }
    }
}

/// Audit of the decomposition gate's activity during one [`Planner::plan`]
/// call, carried alongside [`PlannerOutcome`] so the caller (the Populator)
/// can surface it to the operator without re-deriving it from logs. Purely
/// observational — it does not change the gate's behaviour (design/T298
/// lineage), only exposes what [`plan_with_url`] already computes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecompositionAudit {
    /// Number of attempts on which [`detect_oversize_tasks`] found an
    /// oversize task (0 = the gate never triggered).
    pub oversize_attempts: u32,
    /// Tasks still oversize when a proposal was accepted best-effort after
    /// exhausting [`PLANNER_VALIDATION_ATTEMPTS`]. 0 if the gate never
    /// triggered, or triggered but a later retry resolved cleanly.
    pub oversize_remaining: usize,
}

/// The Planner. A zero-sized entry point so callers write the
/// `Planner::plan(..)` shape the design names; the Planner holds no state
/// (it is a pure transform).
pub struct Planner;

impl Planner {
    /// Plan the implementation task graph for one project from its merged
    /// design doc.
    ///
    /// `api_key` is passed in (not read from config here) so the Planner
    /// stays a pure transform with no config/DB dependency — the caller
    /// sources it from `Config::anthropic_api_key`, mirroring
    /// [`crate::live_status::summarize_transcript`]. A `None` key short-
    /// circuits to [`PlannerOutcome::NoApiKey`] without a network call.
    ///
    /// The shared [`crate::claude_client`] pipeline retries transient failures
    /// (transport errors and HTTP 429/5xx/overloaded) once before failing safe;
    /// a non-retryable 4xx, a decode failure, or output that fails schema
    /// validation is surfaced immediately, mapped into a [`PlannerOutcome`].
    /// The second element of the return tuple is the decomposition-gate audit
    /// ([`DecompositionAudit`]) for this call.
    pub async fn plan(api_key: Option<&str>, input: &PlannerInput) -> (PlannerOutcome, DecompositionAudit) {
        match api_key {
            None => {
                tracing::error!("planner: skipped — ANTHROPIC_API_KEY not configured",);
                (PlannerOutcome::NoApiKey, DecompositionAudit::default())
            }
            Some(key) => plan_with_url(claude_client::ANTHROPIC_MESSAGES_URL, key, input).await,
        }
    }
}

/// Why the previous attempt's output was rejected, carried into the next
/// attempt's prompt so the model can see and fix exactly what was wrong. The
/// two rejection modes share the one bounded acceptance loop
/// ([`PLANNER_VALIDATION_ATTEMPTS`]).
enum RetryFeedback {
    /// The tool call failed [`planner_output_schema`] validation.
    Schema(String),
    /// The proposal was schema-valid but one or more tasks tripped the
    /// decomposition gate ([`detect_oversize_tasks`]).
    Oversize(Vec<OversizeFinding>),
}

/// Core of [`Planner::plan`] with the endpoint URL injected so tests can
/// drive it against a mock server. Hands each attempt's request to the shared
/// [`crate::claude_client`] pipeline (which owns 429/5xx/transport
/// retry/backoff) and, on a rejection — a schema-validation failure OR an
/// oversize proposal the decomposition gate catches — rebuilds the request
/// with the reason fed back into the prompt and tries again, bounded by
/// [`PLANNER_VALIDATION_ATTEMPTS`], before failing safe / accepting
/// best-effort.
async fn plan_with_url(url: &str, api_key: &str, input: &PlannerInput) -> (PlannerOutcome, DecompositionAudit) {
    let config = CallConfig::new(PLANNER_TIMEOUT)
        .with_retry(RetryPolicy::new(PLANNER_ATTEMPTS, PLANNER_BACKOFF))
        .with_endpoint(url);

    let mut feedback: Option<RetryFeedback> = None;
    let mut audit = DecompositionAudit::default();
    // The last schema-valid proposal seen on an earlier attempt (with its
    // oversize-finding count), kept so a *later* attempt's schema failure can
    // fall back to it instead of discarding a stageable plan — see the
    // fallback branch below and [`PLANNER_VALIDATION_ATTEMPTS`]'s doc comment.
    let mut last_valid: Option<(PlannerOutput, usize)> = None;
    for attempt in 1..=PLANNER_VALIDATION_ATTEMPTS {
        let body = match &feedback {
            None => build_request_body(input),
            Some(fb) => build_retry_request_body(input, fb),
        };
        match claude_client::send_messages_raw(api_key, &body, &config).await {
            Ok(response) => match planner_output_from_response(&response) {
                Ok(output) => {
                    // Decomposition gate (design brief deliverable 1): reject a
                    // schema-valid proposal that ships a monolithic
                    // "project in disguise" task and re-prompt for a split,
                    // bounded by the same retry budget. On the final attempt
                    // accept the best-effort output rather than discard a valid
                    // (if imperfectly-decomposed) plan — the staged tasks are
                    // still operator-reviewed before dispatch, and the prompt's
                    // sizing contract already pushed hard for the split.
                    let findings = detect_oversize_tasks(&output);
                    if findings.is_empty() {
                        return (PlannerOutcome::Success(output), audit);
                    }
                    audit.oversize_attempts += 1;
                    if attempt >= PLANNER_VALIDATION_ATTEMPTS {
                        audit.oversize_remaining = findings.len();
                        tracing::warn!(
                            attempt,
                            oversize = findings.len(),
                            "planner: oversize task(s) remain after decomposition retries; accepting best-effort proposal",
                        );
                        return (PlannerOutcome::Success(output), audit);
                    }
                    tracing::warn!(
                        attempt,
                        oversize = findings.len(),
                        "planner: proposal packs oversize task(s); re-prompting for decomposition",
                    );
                    last_valid = Some((output, findings.len()));
                    feedback = Some(RetryFeedback::Oversize(findings));
                }
                Err(msg) => {
                    if attempt >= PLANNER_VALIDATION_ATTEMPTS {
                        // Final attempt is schema-invalid. Rather than discard
                        // the run outright, fall back to the last schema-valid
                        // proposal an earlier attempt already produced (if
                        // any) and accept it best-effort — symmetric with the
                        // oversize best-effort acceptance above. This is the
                        // exact incident this budget failed to handle: attempt
                        // 1 produced a valid-but-oversize proposal, attempt
                        // 2's only retry was already spent on the oversize
                        // re-prompt, and the model's final response flaked on
                        // schema (`effort_audit` as a JSON-encoded string) —
                        // the valid attempt-1 plan was discarded instead of
                        // staged.
                        if let Some((output, oversize_remaining)) = last_valid {
                            audit.oversize_remaining = oversize_remaining;
                            tracing::warn!(
                                attempt,
                                err = %msg,
                                "planner: final attempt schema-invalid; falling back to last schema-valid proposal",
                            );
                            return (PlannerOutcome::Success(output), audit);
                        }
                        return (PlannerOutcome::InvalidOutput(msg), audit);
                    }
                    tracing::warn!(
                        attempt,
                        max_attempts = PLANNER_VALIDATION_ATTEMPTS,
                        err = %msg,
                        "planner: schema-invalid output; retrying with validation feedback",
                    );
                    feedback = Some(RetryFeedback::Schema(msg));
                }
            },
            Err(err) => return (outcome_from_error(err), audit),
        }
    }
    // Unreachable in practice: the final iteration returns in every branch
    // (Ok → Success/best-effort, Err → InvalidOutput/fallback-Success). Kept
    // as a fail-safe.
    (
        PlannerOutcome::InvalidOutput("exhausted planner validation retries".to_owned()),
        audit,
    )
}

/// Assemble the Anthropic Messages request body. Public so tests and future
/// callers can inspect the exact request shape.
pub fn build_request_body(input: &PlannerInput) -> Value {
    json!({
        "model": PLANNER_MODEL,
        "max_tokens": PLANNER_MAX_TOKENS,
        // Bound the reasoning/token spend (design "bound … effort"). Effort
        // lives inside `output_config`, not at the top level.
        "output_config": { "effort": PLANNER_EFFORT },
        "system": SYSTEM_PROMPT,
        // A single forced tool call IS the structured-output mechanism: the
        // model must call `emit_task_graph`, whose `input` is a PlannerOutput.
        "tools": [{
            "name": TOOL_NAME,
            "description": TOOL_DESCRIPTION,
            "input_schema": planner_output_schema(),
        }],
        "tool_choice": { "type": "tool", "name": TOOL_NAME },
        "messages": [{ "role": "user", "content": build_user_prompt(input) }],
    })
}

/// Build the user message: project/product context, the task cap, the
/// existing-task dedup hint, and the full design doc to read.
pub fn build_user_prompt(input: &PlannerInput) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Project: {} (slug: {})\n",
        input.project.name, input.project.slug
    ));
    if !input.project.description.trim().is_empty() {
        out.push_str(&format!("Project description: {}\n", input.project.description));
    }
    if !input.project.goal.trim().is_empty() {
        out.push_str(&format!("Project goal: {}\n", input.project.goal));
    }
    out.push_str(&format!(
        "Product: {} (slug: {})\n\n",
        input.product.name, input.product.slug
    ));

    out.push_str(&format!(
        "Task cap: do NOT propose more than {} task(s). If the doc genuinely \
         describes more, propose the most important up to the cap and say so \
         in `notes`.\n\n",
        input.max_tasks
    ));

    out.push_str(
        "Existing task names already in this project (do NOT propose a task \
         that duplicates one of these; skip any breakdown item whose work \
         they already cover):\n",
    );
    if input.existing_tasks.is_empty() {
        out.push_str("(none)\n\n");
    } else {
        for task in &input.existing_tasks {
            out.push_str(&format!("- {}\n", task.name));
        }
        out.push('\n');
    }

    out.push_str(
        "Below is the full merged design document. Read its implementation \
         breakdown and call the `emit_task_graph` tool with the proposed \
         task graph.\n\n",
    );
    out.push_str(&format!("--- BEGIN DESIGN DOC ({}) ---\n", input.design_doc_ref.path));
    out.push_str(&input.design_doc);
    if !input.design_doc.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("--- END DESIGN DOC ---\n");
    out
}

/// Build the retry request body: identical to [`build_request_body`] except
/// the single user turn also carries the previous attempt's rejection reason,
/// so the model can see and correct exactly what it got wrong instead of
/// repeating the same mistake blind.
fn build_retry_request_body(input: &PlannerInput, feedback: &RetryFeedback) -> Value {
    let mut body = build_request_body(input);
    if let Some(content) = body
        .get_mut("messages")
        .and_then(|messages| messages.get_mut(0))
        .and_then(|message| message.get_mut("content"))
    {
        *content = Value::String(retry_user_prompt(input, feedback));
    }
    body
}

/// The retry user turn: the normal prompt plus an explicit rejection notice.
/// The notice differs by rejection mode — a schema-type error, or the
/// decomposition gate's list of oversize tasks to split.
fn retry_user_prompt(input: &PlannerInput, feedback: &RetryFeedback) -> String {
    let mut out = build_user_prompt(input);
    match feedback {
        RetryFeedback::Schema(validation_error) => {
            out.push_str(&format!(
                "\n--- YOUR PREVIOUS emit_task_graph CALL WAS REJECTED ---\n\
                 Schema validation error: {validation_error}\n\
                 Every field must have exactly the JSON type the schema declares — in \
                 particular, array fields (`tasks`, `edges`) must be emitted as a JSON \
                 array, never as a single JSON-encoded string containing one. Call \
                 `emit_task_graph` again with a schema-valid payload that fixes this.\n\
                 --- END REJECTION NOTICE ---\n"
            ));
        }
        RetryFeedback::Oversize(findings) => {
            let list = findings
                .iter()
                .map(|f| format!("- {}", f.describe()))
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&format!(
                "\n--- YOUR PREVIOUS emit_task_graph CALL WAS REJECTED: OVERSIZE TASK(S) ---\n\
                 One or more proposed tasks are too large for a single worker session / a \
                 single reviewable PR. Decompose EACH of the following into dependency-ordered \
                 tasks — single-subsystem, single-PR each — and wire the `edges` between them. \
                 Lift any embedded fan-out (\"validate/sweep/migrate all N X\", an all-lists \
                 reconciliation, a corpus-wide sweep) into its OWN dependent task. When a task \
                 embeds unknown-format discovery, emit a separate `investigation` task before \
                 the implementation task that consumes it:\n\
                 {list}\n\
                 Call `emit_task_graph` again with the decomposed graph.\n\
                 --- END REJECTION NOTICE ---\n"
            ));
        }
    }
    out
}

/// Map a shared [`ClaudeError`] into the matching [`PlannerOutcome`]. Transport
/// and decode failures are both "we couldn't get usable bytes back", so they
/// bucket together.
fn outcome_from_error(err: ClaudeError) -> PlannerOutcome {
    match err {
        ClaudeError::Api { status, body } => PlannerOutcome::ApiError {
            status,
            snippet: boss_engine_utils::string_clip::clip_to_bytes(&body, 200),
        },
        ClaudeError::Transport(msg) | ClaudeError::Decode(msg) => PlannerOutcome::Transport(msg),
    }
}

/// Pull the forced tool call's `input` out of the response and deserialise it
/// into a [`PlannerOutput`]. Uses the shared
/// [`MessagesResponse::tool_use_input`] extractor; a missing tool call or a
/// schema mismatch is a validation failure (`Err`), which the caller records as
/// [`PlannerOutcome::InvalidOutput`].
fn planner_output_from_response(response: &MessagesResponse) -> Result<PlannerOutput, String> {
    let input = response
        .tool_use_input(TOOL_NAME)
        .ok_or_else(|| format!("model did not call the {TOOL_NAME} tool"))?;
    let mut input = input.clone();
    coerce_stringified_array_fields(&mut input);
    // `effort_audit` is no longer part of the model contract (see the module
    // doc), but `PlannerOutput::effort_audit` deserialises normally now that
    // it is a bidirectional wire field — so overwrite whatever the raw JSON
    // holds here (present or not, well-formed or not) with an empty array
    // before deserialising. This confines tolerance for any shape the model
    // might still emit to this one call site, then [`derive_effort_audit`]
    // fills in the real values from each task's `description`.
    if let Some(obj) = input.as_object_mut() {
        obj.insert("effort_audit".to_owned(), json!([]));
    }
    let mut output = serde_json::from_value::<PlannerOutput>(input)
        .map_err(|err| format!("tool input did not match the PlannerOutput schema: {err}"))?;
    normalize_output_text(&mut output);
    derive_effort_audit(&mut output);
    Ok(output)
}

/// Top-level [`PlannerOutput`] fields the schema requires to be a JSON array
/// and that are still deserialized from the model's JSON (unlike
/// `effort_audit`, which the model is no longer asked to emit at all and
/// which [`planner_output_from_response`] overwrites before deserialising —
/// see [`derive_effort_audit`]).
const ARRAY_TYPED_FIELDS: &[&str] = &["tasks", "edges"];

/// Prefix marking the audit line the system prompt requires at the end of
/// every task's `description` (see [`SYSTEM_PROMPT`] "`[effort-classification]`
/// audit line").
const EFFORT_AUDIT_PREFIX: &str = "[effort-classification]";

/// Derive [`PlannerOutput::effort_audit`] from each task's `description`
/// instead of trusting a separately-emitted array. The system prompt requires
/// the model to write a `[effort-classification]` line at the end of every
/// task's `description`; the model is no longer asked to also duplicate that
/// line into an `effort_audit` array — that array was redundant with data
/// already present in `description`, and duplicating it invited a whole
/// class of failure (observed in production: `effort_audit` emitted as a
/// single JSON-encoded string rather than a JSON array, which used to fail
/// deserialization of an otherwise fully valid proposal). Deriving the field
/// here instead removes that failure mode entirely: `effort_audit` no longer
/// depends on anything the model puts in its own JSON —
/// [`planner_output_from_response`] overwrites the raw key unconditionally
/// before deserialising — mirrors `pr_review::types::
/// RegressionCheck::suspected_deletions`, which is derived from `findings`
/// for the same reason.
///
/// One entry per task, same order as `tasks` (an empty string for a task
/// whose description has no audit line), so callers that index it against
/// `tasks` by position (e.g. [`detect_oversize_tasks`]) stay aligned.
fn derive_effort_audit(output: &mut PlannerOutput) {
    output.effort_audit = output
        .tasks
        .iter()
        .map(|task| {
            task.description
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| line.starts_with(EFFORT_AUDIT_PREFIX))
                .unwrap_or("")
                .to_owned()
        })
        .collect();
}

/// Undo a model slip observed in production (T-planner-string-array): an
/// array-typed field emitted as a single JSON-encoded string instead of an
/// actual JSON array — e.g. `"edges": "[{\"dependent\": …}]"` rather than
/// `"edges": [{"dependent": …}]`. The model's *content* is fine; only the
/// outer JSON type is wrong. When one of
/// [`ARRAY_TYPED_FIELDS`] is a string that itself parses as a JSON array, this
/// swaps it in place before schema validation, logging a warning so the slip
/// stays visible rather than being silently masked. A string that fails to
/// parse (or parses to something other than an array) is left untouched —
/// serde still rejects it, and the retry loop in [`plan_with_url`] then feeds
/// the resulting schema error back to the model.
fn coerce_stringified_array_fields(input: &mut Value) {
    let Some(obj) = input.as_object_mut() else {
        return;
    };
    for field in ARRAY_TYPED_FIELDS {
        let raw = match obj.get(*field) {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        if let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(&raw) {
            tracing::warn!(
                field = *field,
                "planner: coerced a stringified JSON array back into an array before schema validation",
            );
            obj.insert((*field).to_owned(), parsed);
        }
    }
}

/// Undo over-escaping the model occasionally introduces in free-text fields
/// (observed as literal `\"` and `\n` sequences surviving the JSON decode —
/// the model wrote `\\"` / `\\n` inside its tool-call JSON, one escape level
/// too many). Applied once here, right after deserialisation and before
/// [`derive_effort_audit`] extracts the audit line out of each (now clean)
/// `description`, so every downstream consumer (the `planner_runs` audit
/// row, the Materializer, the app UI) sees clean text instead of each
/// display site having to band-aid around it. `effort_audit` itself needs no
/// entry here — [`planner_output_from_response`] always resets it to an
/// empty array before this runs, and [`derive_effort_audit`] populates it
/// afterwards from the (now-clean) task descriptions.
fn normalize_output_text(output: &mut PlannerOutput) {
    output.notes = unescape_over_escaped(&output.notes);
    for task in &mut output.tasks {
        task.name = unescape_over_escaped(&task.name);
        task.description = unescape_over_escaped(&task.description);
    }
    for hint in &mut output.merge_order_hints {
        hint.reason = unescape_over_escaped(&hint.reason);
    }
}

/// Replace literal `\"` and `\n` (backslash followed by a literal character,
/// as opposed to an actual quote/newline) with the character they were
/// meant to represent.
fn unescape_over_escaped(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\\"", "\"")
}

/// The Planner's system prompt. Encodes the coordinator policy a human would
/// otherwise apply by hand: the Q4 effort heuristic, the kind conventions,
/// the parallelism-maximising edge guidance, and the `[effort-classification]`
/// emission contract. See design §2 "Encodes coordinator policy".
const SYSTEM_PROMPT: &str = "\
You are the Boss Planner — a mini-coordinator. You read a merged software \
design document and propose the project's implementation task graph: the \
tasks to create, their effort levels and kinds, and the dependency edges \
between them. You are the automated stand-in for a human coordinator who \
would otherwise read the doc by hand and type out `boss task create` / \
`boss task depend add` calls.\n\
\n\
You write no code and create no rows. Your entire job is the prose-to-typed- \
graph transform: read the doc, then make exactly one `emit_task_graph` tool \
call with the proposed graph. Do not call any other tool.\n\
\n\
## What to extract\n\
\n\
Most design docs end with a section enumerating the implementation work — \
headings like \"Proposed implementation task breakdown\", \"Follow-up \
Implementation Chores\", or \"Implementation Plan\", usually a numbered or \
bulleted list where each item is one bite-sized unit of work (roughly one PR \
each). Extract those items as `tasks`.\n\
\n\
- If the doc contains such a breakdown, set `breakdown_found` to true and \
emit one task per enumerated item.\n\
- If the doc is pure design rationale with NO enumerable implementation \
breakdown, set `breakdown_found` to false, return an empty `tasks` array and \
empty `edges`, and explain in `notes`. This is a clean, valid result — not \
an error. Never invent tasks the doc does not describe.\n\
\n\
Do NOT propose:\n\
- The design task itself (it already exists and its PR has already merged).\n\
- Any task whose name duplicates one already in the project (the existing \
names are listed in the user message).\n\
- More than the task cap stated in the user message.\n\
\n\
## task sizing contract — one reviewable PR per task\n\
\n\
Every task you propose must be single-subsystem, single-PR, and completable \
by one worker in roughly one session. Size each item down to that \
granularity: when a doc's own breakdown item is bigger than that, SPLIT it \
into several dependency-ordered tasks rather than transcribing it whole. A \
single monolithic \"project in disguise\" task is exactly the failure mode \
this guards against.\n\
\n\
- **Multi-subsystem scope is several tasks.** A task that spans multiple \
subsystems (engine + cli + protocol + app + …) must be emitted as one task \
per subsystem with dependency edges — never one task that touches them all.\n\
- **Multi-phase scope is several tasks.** \"parse (i)… and (ii)… and emit… \
and validate…\" is a chain of phases: emit each phase as its own task and \
wire the `edges` between them. Never pack the phases into one task.\n\
- **Embedded fan-out is its own dependent task.** \"validate/sweep/migrate \
all N X\", an all-lists reconciliation, or a corpus-wide sweep is a separate \
task that depends on the implementation it validates. Do not fold the sweep \
into the implementer.\n\
- **If a breakdown item needs a paragraph to describe, it is almost \
certainly several tasks** — decompose it.\n\
- **Unknown-format discovery becomes an INVESTIGATION task.** When an item \
embeds format discovery / reverse-engineering (verbs like study, dump, \
reverse-engineer, characterise, reconcile-against-source), emit a separate \
`investigation` task for that discovery, sequenced (via an edge) BEFORE the \
implementation task that consumes its findings. This is the T298 lesson: the \
single biggest chunk of that run was format discovery, not implementation — \
ideal investigation-task shape.\n\
\n\
A proposal that ships an oversize task will be rejected and you will be \
re-prompted to decompose it, so split it up front.\n\
\n\
## handles\n\
\n\
Each task carries a `handle`: a short, stable, kebab-case proposal-local id \
(e.g. `protocol-types`, `engine-rpc-handler`, `cli-surface`). Handles are \
how edges reference tasks, so make them unique and descriptive. They are not \
shown to users; they exist only to wire the graph.\n\
\n\
## kind conventions\n\
\n\
- Default every task to `project_task`. These belong to a project and map to \
roughly one PR each.\n\
- Use `investigation` for a task framed as research, audit, or diagnosis \
(\"investigate …\", \"audit …\", \"diagnose …\", \"root-cause …\").\n\
- Never emit any other kind. In particular never emit `design` (a project \
has exactly one design task and it already exists) or `chore` (chores are \
product-direct, not project-scoped).\n\
\n\
## effort heuristic (apply per task; first matching rule wins)\n\
\n\
Classify each task into exactly one of `trivial | small | medium | large`. \
Never emit `max` — that level is reserved for explicit human override. \
Evaluate top to bottom and take the first rule that matches:\n\
\n\
1. The task is an investigation / design-flavoured unit (kind = \
investigation, or framed as investigate / audit / instrument / diagnose / \
end-to-end / root cause / architect / redesign / migrate / rearchitect) → \
`large`.\n\
2. The task has very long, substantive scope (a paragraph or more) → \
`large`. Long scope is almost always a project in disguise.\n\
3. The task spans multiple subsystems or names multiple module surfaces \
(\"engine + protocol\", \"across cli and app\", or two or more of: engine, \
cli, protocol, app-macos, cube, bossctl) → `medium`.\n\
4. The task is a near-mechanical single-surface edit (rename / apply / \
revert / bump / move / delete / remove / hide / show / pad / align / \
re-export, a one-line tweak, a cursor / badge / tooltip / gap fix) → \
`trivial`.\n\
5. The task is small and self-contained (one to a few files, no \
architectural judgement) → `small`.\n\
6. Anything else → `medium`.\n\
\n\
As calibration: a schema / protocol / contract task that others build on is \
typically `small`; a single-subsystem feature is `small` or `medium`; an \
integration task that wires several pieces together is `medium`; an \
investigation or multi-subsystem rearchitecture is `large`.\n\
\n\
## [effort-classification] audit line\n\
\n\
For every task produce one `[effort-classification]` line in EXACTLY this \
format (backticks around the level and the rule; double-quoted reasons):\n\
\n\
[effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` reasons=\"names engine + protocol surfaces\"\n\
\n\
- Put this line at the END of the task's `description`, separated from the \
rest of the description by a blank line.\n\
- The `level` in the line MUST equal the task's `effort`.\n\
\n\
## dependency edges — maximise safe parallelism\n\
\n\
Add an edge ONLY for a true prerequisite: B genuinely cannot start until A \
has landed (e.g. \"engine RPC handler\" depends on \"protocol types\"). \
Leave independently-startable tasks unedged so they dispatch in parallel. Do \
NOT chain tasks into a single line just because they are listed in order — \
`ordinal` already carries the soft ordering hint, and over-edging serializes \
work that could run concurrently.\n\
\n\
The common healthy shape is: a shared schema / protocol / contract task as \
the root, then a fan-out of independent consumer tasks that each depend only \
on that root, then an integration / end-to-end task that depends on the \
fan-out. Edges MUST form a DAG — never introduce a cycle.\n\
\n\
When you decide parallelism, weigh not just **functional** independence but \
also **file** overlap. Two tasks can be independent in design yet edit the \
same files — e.g. a compact-view task and a detail-view task that both edit \
the same component/container, or two tasks that both touch one shared route / \
config / module. Parallelising edit-overlapping siblings schedules a \
forward-port conflict, and each such conflict is a chance for the later \
resolution to silently drop the earlier one's work. So: when — and only when \
— two otherwise-parallel tasks are **clearly and substantially** likely to \
co-edit the same file(s), add an entry to `merge_order_hints` naming the pair \
and the file(s)/surface you expect them to co-edit. Do NOT over-index on \
this — a little incidental overlap is not enough; if you flag every pair that \
shares a file, the hint stops being useful. Emit a hint only on clear, \
substantial overlap.\n\
\n\
**A `merge_order_hints` entry is NOT a dependency edge and must never gate \
dispatch.** Both tasks stay independently startable — the hint only lets a \
later merge-time step order the two PRs and require the later one to \
forward-port the sibling's changes preservingly (integrate, never delete). \
Never use a `blocks` edge for file overlap alone; `edges` is reserved for \
true functional prerequisites (design's \"Parallel throughput stays the \
default\").\n\
\n\
Each edge is { \"dependent\": <handle that waits>, \"prerequisite\": <handle \
that must land first> }. Both endpoints must be handles you emitted.\n\
\n\
Each `merge_order_hints` entry is { \"task_a\": <handle>, \"task_b\": <handle>, \
\"reason\": <which file(s)/surface they co-edit> }. Both handles must be \
handles you emitted, and must be two DIFFERENT tasks with no `edges` \
relationship between them (if one already depends on the other via an edge, \
their landing order is already fixed — do not also emit a hint for that \
pair).\n\
\n\
## ordinal\n\
\n\
`ordinal` is a soft ordering hint (0, 1, 2, …) suggesting reading order. It \
does NOT gate dispatch — edges do.\n\
\n\
## confidence\n\
\n\
- `high`: the doc has a clear, explicit, well-structured breakdown you \
transcribed with little inference.\n\
- `medium`: you inferred some structure or interpreted an unconventional \
layout.\n\
- `low`: the breakdown was ambiguous or buried, or you are unsure the graph \
is right. (Low blocks nothing downstream — tasks are staged for a human to \
review regardless — but it flags the result for scrutiny.)\n\
\n\
## notes\n\
\n\
Put a short free-text rationale in `notes`: which section you read, how you \
chose the edges, and anything a human reviewer should know.\
";

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{Confidence, DocRef, ProductContext, ProjectContext, TaskBrief};
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_input() -> PlannerInput {
        PlannerInput::builder()
            .design_doc("# Design\n\n## Proposed implementation task breakdown\n1. Protocol types.\n2. Engine handler. Depends on 1.\n")
            .design_doc_ref(DocRef {
                repo_remote_url: "https://github.com/owner/repo".to_owned(),
                git_ref: "main".to_owned(),
                path: "tools/boss/docs/designs/foo.md".to_owned(),
            })
            .project(ProjectContext {
                id: "proj_1".to_owned(),
                name: "My Project".to_owned(),
                slug: "my-project".to_owned(),
                description: "Do a thing.".to_owned(),
                goal: "Ship the thing.".to_owned(),
            })
            .product(ProductContext {
                id: "prod_1".to_owned(),
                slug: "boss".to_owned(),
                name: "Boss".to_owned(),
                repo_remote_url: "https://github.com/owner/repo".to_owned(),
            })
            .existing_tasks(vec![TaskBrief {
                id: "task_existing".to_owned(),
                name: "Already here".to_owned(),
            }])
            .max_tasks(30)
            .build()
    }

    /// A well-formed `tool_use` response body mirroring what Anthropic
    /// returns for a forced tool call.
    fn tool_use_response() -> Value {
        json!({
            "content": [
                { "type": "text", "text": "" },
                {
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": TOOL_NAME,
                    "input": {
                        "tasks": [{
                            "handle": "protocol-types",
                            "name": "Add protocol types",
                            "description": "Add the contract types.\n\n[effort-classification] level=`small` matched-rule=`rule 5 (self-contained)` reasons=\"protocol types\"",
                            "kind": "project_task",
                            "effort": "small",
                            "ordinal": 0
                        }, {
                            "handle": "engine-handler",
                            "name": "Engine handler",
                            "description": "Wire the handler.\n\n[effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` reasons=\"engine + protocol\"",
                            "kind": "project_task",
                            "effort": "medium",
                            "ordinal": 1
                        }],
                        "edges": [
                            { "dependent": "engine-handler", "prerequisite": "protocol-types" }
                        ],
                        "confidence": "high",
                        "breakdown_found": true,
                        "notes": "Clear two-item breakdown.",
                        "effort_audit": [
                            "[effort-classification] level=`small` matched-rule=`rule 5 (self-contained)` reasons=\"protocol types\"",
                            "[effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` reasons=\"engine + protocol\""
                        ]
                    }
                }
            ]
        })
    }

    #[test]
    fn build_request_body_forces_the_planner_tool() {
        let body = build_request_body(&sample_input());
        assert_eq!(body["model"], PLANNER_MODEL);
        assert_eq!(body["max_tokens"], PLANNER_MAX_TOKENS);
        assert_eq!(body["output_config"]["effort"], PLANNER_EFFORT);
        // Structured output is enforced via a forced tool call.
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], TOOL_NAME);
        assert_eq!(body["tools"][0]["name"], TOOL_NAME);
        // The forced tool's input_schema is the contract schema.
        assert_eq!(body["tools"][0]["input_schema"], planner_output_schema(),);
        // System prompt + a single user turn.
        assert!(body["system"].as_str().unwrap().contains("Boss Planner"));
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn build_user_prompt_carries_doc_and_context() {
        let prompt = build_user_prompt(&sample_input());
        assert!(prompt.contains("My Project"));
        assert!(prompt.contains("Boss"));
        // Task cap surfaced to the model.
        assert!(prompt.contains("more than 30"));
        // Existing-task dedup hint.
        assert!(prompt.contains("Already here"));
        // The full doc is included, fenced by the begin/end markers.
        assert!(prompt.contains("Proposed implementation task breakdown"));
        assert!(prompt.contains("--- BEGIN DESIGN DOC (tools/boss/docs/designs/foo.md) ---"));
        assert!(prompt.contains("--- END DESIGN DOC ---"));
    }

    #[test]
    fn build_user_prompt_handles_no_existing_tasks() {
        let mut input = sample_input();
        input.existing_tasks.clear();
        let prompt = build_user_prompt(&input);
        assert!(prompt.contains("(none)"));
    }

    #[test]
    fn system_prompt_encodes_the_required_policy() {
        // Effort heuristic, kind conventions, parallelism guidance, and the
        // audit-line contract must all be present.
        assert!(SYSTEM_PROMPT.contains("[effort-classification]"));
        assert!(SYSTEM_PROMPT.contains("project_task"));
        assert!(SYSTEM_PROMPT.contains("investigation"));
        assert!(SYSTEM_PROMPT.contains("first matching rule wins"));
        assert!(SYSTEM_PROMPT.contains("Never emit `max`"));
        assert!(SYSTEM_PROMPT.contains("maximise safe parallelism"));
        assert!(SYSTEM_PROMPT.contains("breakdown_found"));
        assert!(SYSTEM_PROMPT.contains("DAG"));
        // P5-lite (incident-002, reconciled with T2253): the planner must
        // weigh file overlap, but only emit a soft merge_order_hints entry —
        // never a `blocks` edge — on clear/substantial overlap, so throughput
        // stays the default and dispatch is never gated by file overlap
        // alone.
        assert!(SYSTEM_PROMPT.contains("file** overlap"));
        assert!(SYSTEM_PROMPT.contains("merge_order_hints"));
        assert!(SYSTEM_PROMPT.contains("forward-port the sibling's changes preservingly"));
        assert!(SYSTEM_PROMPT.contains("is NOT a dependency edge and must never gate"));
        assert!(SYSTEM_PROMPT.contains("Never use a `blocks` edge for file overlap alone"));
    }

    /// The decomposition gate's prompt half (design brief deliverable 1): the
    /// sizing contract must instruct the model to split multi-subsystem /
    /// multi-phase / fan-out scope and to emit investigation tasks for
    /// embedded discovery, so breakdowns arrive pre-split.
    #[test]
    fn system_prompt_encodes_the_sizing_contract() {
        assert!(SYSTEM_PROMPT.contains("task sizing contract"));
        assert!(SYSTEM_PROMPT.contains("single-subsystem, single-PR"));
        assert!(SYSTEM_PROMPT.contains("project in disguise"));
        assert!(SYSTEM_PROMPT.contains("Multi-phase scope is several tasks"));
        assert!(SYSTEM_PROMPT.contains("Embedded fan-out is its own dependent task"));
        assert!(SYSTEM_PROMPT.contains("INVESTIGATION task"));
        assert!(SYSTEM_PROMPT.contains("re-prompted to decompose"));
    }

    fn response_from(value: Value) -> MessagesResponse {
        serde_json::from_value(value).expect("valid MessagesResponse")
    }

    #[test]
    fn parses_a_well_formed_tool_use_response() {
        let out =
            planner_output_from_response(&response_from(tool_use_response())).expect("valid tool_use response parses");
        assert_eq!(out.tasks.len(), 2);
        assert_eq!(out.tasks[0].handle, "protocol-types");
        assert_eq!(out.tasks[0].effort, boss_protocol::EffortLevel::Small);
        assert_eq!(out.tasks[1].kind, boss_protocol::TaskKind::ProjectTask);
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].dependent, "engine-handler");
        assert_eq!(out.edges[0].prerequisite, "protocol-types");
        assert_eq!(out.confidence, Confidence::High);
        assert!(out.breakdown_found);
        assert_eq!(out.effort_audit.len(), 2);
    }

    #[test]
    fn effort_audit_is_derived_from_task_descriptions_ignoring_a_malformed_raw_field() {
        // Reproduces the production failure: the model emitted `effort_audit`
        // as a single JSON-encoded string (`"[\"[effort-classification] …\"]"`)
        // instead of a JSON array, which used to fail the whole otherwise-valid
        // proposal with a serde type-mismatch error. `effort_audit` is now
        // `#[serde(skip_deserializing)]`, so the raw field — whatever shape it
        // took — is never even looked at; the real value is derived from the
        // task's `description`, which the system prompt already requires to
        // carry the identical audit line.
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "h",
                        "name": "Task",
                        "description": "Do the thing.\n\n[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\"",
                        "kind": "project_task",
                        "effort": "small",
                        "ordinal": 0
                    }],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "n",
                    "effort_audit": "not even a JSON array, complete garbage {{{"
                }
            }]
        }));
        let out = planner_output_from_response(&response)
            .expect("a malformed raw effort_audit field must never fail deserialization");
        assert_eq!(out.effort_audit.len(), 1);
        assert_eq!(
            out.effort_audit[0],
            "[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\""
        );
    }

    #[test]
    fn effort_audit_is_empty_for_a_task_with_no_audit_line() {
        // A task description with no `[effort-classification]` line derives
        // an empty entry rather than failing, and stays index-aligned with
        // `tasks` (relied on by `detect_oversize_tasks`).
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "h",
                        "name": "Task",
                        "description": "Do the thing, no audit line here.",
                        "kind": "project_task",
                        "effort": "small",
                        "ordinal": 0
                    }],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "n",
                    "effort_audit": []
                }
            }]
        }));
        let out = planner_output_from_response(&response).expect("valid tool_use response parses");
        assert_eq!(out.effort_audit, vec!["".to_owned()]);
    }

    #[test]
    fn does_not_coerce_a_non_json_string_field() {
        // A field that is a string but does not itself parse as a JSON array
        // (e.g. a genuine free-text mistake, not the known stringified-array
        // slip) must still be rejected by schema validation rather than
        // silently accepted. `edges` is still schema-validated (unlike
        // `effort_audit`, which is derived and never validated).
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": "not an array at all",
                    "confidence": "high",
                    "breakdown_found": false,
                    "notes": "n",
                    "effort_audit": []
                }
            }]
        }));
        assert!(
            planner_output_from_response(&response).is_err(),
            "a non-JSON-array string must still fail validation",
        );
    }

    #[test]
    fn coerces_a_stringified_edges_array_before_validation() {
        // `edges` is still schema-validated and deserialized from the model's
        // JSON (unlike `effort_audit`, which is now derived and never
        // validated), so it still needs the pre-validation coercion —
        // guarding against the same class of slip observed on
        // `effort_audit`, even though `edges` itself has not been observed to
        // flake this way in production.
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": "[{\"dependent\": \"b\", \"prerequisite\": \"a\"}]",
                    "confidence": "high",
                    "breakdown_found": false,
                    "notes": "n",
                    "effort_audit": []
                }
            }]
        }));
        let out =
            planner_output_from_response(&response).expect("stringified-array edges must be coerced and accepted");
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].dependent, "b");
        assert_eq!(out.edges[0].prerequisite, "a");
    }

    #[test]
    fn normalizes_over_escaped_notes_and_task_descriptions() {
        // Guards against a model that over-escapes its JSON tool-call
        // arguments (writes `\\"` / `\\n` where a single JSON escape level
        // was meant), which otherwise survives the JSON decode as literal
        // backslash-quote and backslash-n sequences. `effort_audit` is
        // derived from `description` *after* it is unescaped, so the derived
        // audit line must come out clean too.
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "h",
                        "name": "Over-escaped \\\"name\\\"",
                        "description": "First paragraph.\\n\\nSecond paragraph with a \\\"quote\\\".\\n\\n[effort-classification] level=`small` matched-rule=`rule 5` reasons=\\\"x\\\"",
                        "kind": "project_task",
                        "effort": "small",
                        "ordinal": 0
                    }],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "the doc's \\\"Proposed implementation task breakdown\\\" section\\n\\nmore prose",
                    "effort_audit": []
                }
            }]
        }));
        let out = planner_output_from_response(&response).expect("valid tool_use response parses");
        assert_eq!(
            out.notes,
            "the doc's \"Proposed implementation task breakdown\" section\n\nmore prose"
        );
        assert_eq!(out.tasks[0].name, "Over-escaped \"name\"");
        assert_eq!(
            out.tasks[0].description,
            "First paragraph.\n\nSecond paragraph with a \"quote\".\n\n[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\""
        );
        assert_eq!(
            out.effort_audit[0],
            "[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\""
        );
    }

    #[test]
    fn rejects_response_with_no_tool_call() {
        let response = response_from(json!({
            "content": [{ "type": "text", "text": "I could not find a breakdown." }]
        }));
        assert!(
            planner_output_from_response(&response).is_err(),
            "a response with no tool call must be rejected",
        );
    }

    #[test]
    fn rejects_tool_input_that_violates_the_schema() {
        // Missing the required `confidence` field.
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": [],
                    "breakdown_found": false,
                    "notes": "",
                    "effort_audit": []
                }
            }]
        }));
        assert!(
            planner_output_from_response(&response).is_err(),
            "tool input missing a required field must be rejected",
        );
    }

    #[test]
    fn no_breakdown_response_is_a_valid_empty_plan() {
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": false,
                    "notes": "Pure design rationale; no task breakdown.",
                    "effort_audit": []
                }
            }]
        }));
        let out = planner_output_from_response(&response).expect("empty plan is valid");
        assert!(out.tasks.is_empty());
        assert!(!out.breakdown_found);
    }

    #[tokio::test]
    async fn plan_returns_no_api_key_when_key_missing() {
        let (outcome, audit) = Planner::plan(None, &sample_input()).await;
        assert!(matches!(outcome, PlannerOutcome::NoApiKey));
        assert_eq!(outcome.tag(), "no_api_key");
        assert_eq!(audit, DecompositionAudit::default());
    }

    #[tokio::test]
    async fn end_to_end_success_against_mock_anthropic() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", claude_client::ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(tool_use_response()))
            .mount(&server)
            .await;

        let (outcome, audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;

        match outcome {
            PlannerOutcome::Success(out) => {
                assert_eq!(out.tasks.len(), 2);
                assert_eq!(out.edges.len(), 1);
                assert_eq!(out.confidence, Confidence::High);
            }
            other => panic!("expected Success, got {other:?}"),
        }
        assert_eq!(audit, DecompositionAudit::default());
    }

    #[tokio::test]
    async fn retries_once_then_succeeds() {
        let server = MockServer::start().await;
        // First call: a transient 503 (consumed once). Second call: success.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tool_use_response()))
            .mount(&server)
            .await;

        let (outcome, _audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        assert!(
            matches!(outcome, PlannerOutcome::Success(_)),
            "expected success after one retry, got {outcome:?}",
        );
    }

    /// A tool-use response body whose `input` is missing the required
    /// `confidence` field — invalid in a way the stringified-array coercion
    /// cannot fix, so it only succeeds via the validation-retry loop.
    fn missing_confidence_tool_use_response() -> Value {
        json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": [],
                    "breakdown_found": false,
                    "notes": "",
                    "effort_audit": []
                }
            }]
        })
    }

    /// A tool-use response body mirroring the production incident: an
    /// otherwise well-formed proposal where `effort_audit` is a single
    /// JSON-encoded string instead of a JSON array.
    fn stringified_effort_audit_tool_use_response() -> Value {
        json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "h",
                        "name": "Task",
                        "description": "Do the thing.\n\n[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\"",
                        "kind": "project_task",
                        "effort": "small",
                        "ordinal": 0
                    }],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "n",
                    "effort_audit": "[\"[effort-classification] level=`small` matched-rule=`rule 5` reasons=\\\"x\\\"\"]"
                }
            }]
        })
    }

    #[tokio::test]
    async fn end_to_end_stringified_effort_audit_never_causes_a_retry() {
        // Regression test for the production incident: the model's
        // `effort_audit` field arrived as a JSON-encoded string. This must
        // succeed off the *first* HTTP call — `effort_audit` is
        // `#[serde(skip_deserializing)]` and derived from `tasks[].description`
        // instead, so a malformed raw value can never trigger a validation
        // retry at all (stronger than the old coercion fix, which only
        // tolerated a string that itself parsed as a JSON array). Only one
        // response is mounted (`up_to_n_times(1)`); if the code mistakenly
        // retried, the second call would get no matching mock.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(stringified_effort_audit_tool_use_response()))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let (outcome, audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        match outcome {
            PlannerOutcome::Success(out) => assert_eq!(out.effort_audit.len(), 1),
            other => panic!("expected Success via derivation, got {other:?}"),
        }
        assert_eq!(audit, DecompositionAudit::default());
        assert_eq!(
            server.received_requests().await.expect("requests recorded").len(),
            1,
            "a malformed effort_audit field must never trigger a retry round trip",
        );
    }

    #[tokio::test]
    async fn retries_with_validation_feedback_after_uncoercible_invalid_output() {
        // First attempt is schema-invalid in a way coercion cannot fix
        // (missing required field); the second attempt is well-formed. The
        // run must succeed via the validation-retry loop, and the retry
        // request must carry the previous validation error back to the model.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(missing_confidence_tool_use_response()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tool_use_response()))
            .mount(&server)
            .await;

        let (outcome, audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        assert!(
            matches!(outcome, PlannerOutcome::Success(_)),
            "expected success after the validation retry, got {outcome:?}",
        );
        // A schema-invalid retry never trips the oversize gate.
        assert_eq!(audit, DecompositionAudit::default());

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 2, "expected exactly one validation retry");
        let retry_body: Value = requests[1].body_json().expect("retry body is JSON");
        let retry_prompt = retry_body["messages"][0]["content"]
            .as_str()
            .expect("retry content is a string");
        assert!(
            retry_prompt.contains("YOUR PREVIOUS emit_task_graph CALL WAS REJECTED"),
            "retry prompt must feed the validation failure back to the model",
        );
        assert!(
            retry_prompt.contains("confidence"),
            "retry prompt must mention the actual validation error",
        );
    }

    #[tokio::test]
    async fn fails_after_exhausting_validation_retries() {
        // Every attempt is schema-invalid; the run must fail safe (not hang
        // or retry unboundedly) after exactly PLANNER_VALIDATION_ATTEMPTS
        // calls.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(missing_confidence_tool_use_response()))
            .mount(&server)
            .await;

        let (outcome, _audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        assert!(
            matches!(outcome, PlannerOutcome::InvalidOutput(_)),
            "expected InvalidOutput after exhausting retries, got {outcome:?}",
        );
        assert_eq!(outcome.tag(), "invalid_output");
        assert_eq!(
            server.received_requests().await.expect("requests recorded").len(),
            PLANNER_VALIDATION_ATTEMPTS as usize,
            "must stop after exactly PLANNER_VALIDATION_ATTEMPTS calls",
        );
    }

    #[tokio::test]
    async fn api_error_after_exhausting_retries() {
        let server = MockServer::start().await;
        // 401 is a non-retryable client error: the pipeline fails fast (no
        // retry) and we map it to the typed ApiError outcome.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let (outcome, _audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        match outcome {
            PlannerOutcome::ApiError { status, .. } => assert_eq!(status, 401),
            other => panic!("expected ApiError, got {other:?}"),
        }
        assert_eq!(outcome.tag(), "api_error");
    }

    /// A schema-valid tool_use response whose single task is T298-shaped: a
    /// paragraph of multi-table parsing across sections/slots, an emit step,
    /// a projected_impact seed, and an all-lists validation sweep, with an
    /// effort_audit line that literally calls it "a project in disguise". This
    /// trips the decomposition gate ([`detect_oversize_tasks`]).
    fn oversize_tool_use_response() -> Value {
        json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "national-rolling-points",
                        "name": "Full national rolling-points PDF detail parse",
                        "description": "Parse the multi-table PDF across sections and slots, emit the \
                            event-type slot mapping, seed the projected_impact path, and validate every \
                            fixture in the all-lists reconciliation sweep.\n\n\
                            [effort-classification] level=`large` matched-rule=`rule 2` reasons=\"a project in disguise\"",
                        "kind": "project_task",
                        "effort": "large",
                        "ordinal": 0
                    }],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "One big task.",
                    "effort_audit": [
                        "[effort-classification] level=`large` matched-rule=`rule 2` reasons=\"a project in disguise\""
                    ]
                }
            }]
        })
    }

    /// The decomposed answer a re-prompted model returns: an investigation
    /// task, a parser task, and a validation-sweep task — each well-sized,
    /// wired with dependency edges. Passes the gate cleanly.
    fn decomposed_tool_use_response() -> Value {
        json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "format-investigation",
                        "name": "Reverse-engineer the detail-table format",
                        "description": "Document the closed vocabulary of the detail tables.",
                        "kind": "investigation",
                        "effort": "large",
                        "ordinal": 0
                    }, {
                        "handle": "detail-parser",
                        "name": "Implement the detail-table parser",
                        "description": "Implement the parser against the documented format.",
                        "kind": "project_task",
                        "effort": "medium",
                        "ordinal": 1
                    }, {
                        "handle": "fixture-sweep",
                        "name": "Add the fixture reconciliation test",
                        "description": "Add the reconciliation test over the committed fixtures.",
                        "kind": "project_task",
                        "effort": "small",
                        "ordinal": 2
                    }],
                    "edges": [
                        { "dependent": "detail-parser", "prerequisite": "format-investigation" },
                        { "dependent": "fixture-sweep", "prerequisite": "detail-parser" }
                    ],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "Split into investigation, parser, and validation.",
                    "effort_audit": [
                        "[effort-classification] level=`large` matched-rule=`rule 1 (investigation)` reasons=\"format discovery\"",
                        "[effort-classification] level=`medium` matched-rule=`rule 6` reasons=\"single-subsystem parser\"",
                        "[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"one test\""
                    ]
                }
            }]
        })
    }

    #[tokio::test]
    async fn oversize_proposal_triggers_decomposition_reprompt() {
        // The T298 case end to end: the first tool call ships one monolithic
        // task; the gate rejects it and re-prompts; the model returns the
        // decomposed graph, which is staged. The plan visible to the caller
        // is the split (multiple dependency-ordered tasks), not the monolith.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(oversize_tool_use_response()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(decomposed_tool_use_response()))
            .mount(&server)
            .await;

        let (outcome, audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        match outcome {
            PlannerOutcome::Success(out) => {
                assert_eq!(out.tasks.len(), 3, "the decomposed plan must replace the monolith");
                assert_eq!(out.edges.len(), 2, "decomposed tasks must be dependency-ordered");
                assert!(
                    out.tasks
                        .iter()
                        .any(|t| t.kind == boss_protocol::TaskKind::Investigation)
                );
            }
            other => panic!("expected Success with the decomposed plan, got {other:?}"),
        }
        // The gate triggered once and resolved cleanly on retry — the audit
        // must reflect that even though the run succeeded outright.
        assert_eq!(
            audit,
            DecompositionAudit {
                oversize_attempts: 1,
                oversize_remaining: 0,
            }
        );

        // Exactly one re-prompt, and it must feed the oversize rejection back
        // to the model.
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 2, "expected exactly one decomposition retry");
        let retry_body: Value = requests[1].body_json().expect("retry body is JSON");
        let retry_prompt = retry_body["messages"][0]["content"]
            .as_str()
            .expect("retry content is a string");
        assert!(
            retry_prompt.contains("REJECTED: OVERSIZE TASK(S)"),
            "retry prompt must name the decomposition rejection: {retry_prompt}",
        );
        assert!(
            retry_prompt.contains("national-rolling-points"),
            "retry prompt must name the offending task handle: {retry_prompt}",
        );
    }

    #[tokio::test]
    async fn oversize_proposal_accepted_best_effort_after_exhausting_retries() {
        // If the model keeps returning an oversize task, the run does NOT
        // fail — the valid (if imperfectly-split) plan is staged best-effort
        // for operator review, after exactly PLANNER_VALIDATION_ATTEMPTS calls.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(oversize_tool_use_response()))
            .mount(&server)
            .await;

        let (outcome, audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        assert!(
            matches!(outcome, PlannerOutcome::Success(_)),
            "an unsplittable oversize proposal is accepted best-effort, not failed: {outcome:?}",
        );
        assert_eq!(
            server.received_requests().await.expect("requests recorded").len(),
            PLANNER_VALIDATION_ATTEMPTS as usize,
            "must stop after exactly PLANNER_VALIDATION_ATTEMPTS calls",
        );
        // The best-effort acceptance must be visible to the caller so it can
        // surface it to the operator, not just logged.
        assert_eq!(
            audit,
            DecompositionAudit {
                oversize_attempts: PLANNER_VALIDATION_ATTEMPTS,
                oversize_remaining: 1,
            }
        );
    }

    #[tokio::test]
    async fn final_attempt_schema_failure_falls_back_to_earlier_valid_oversize_proposal() {
        // Regression test for the live incident this fix addresses: attempt 1
        // produces a fully schema-valid, merely-oversize proposal (consuming
        // the only retry to re-prompt for a split); attempt 2 — the final
        // attempt — comes back schema-invalid (the model's `effort_audit`
        // stringified-array flake, or any other schema miss). The run must
        // NOT discard attempt 1's stageable plan: it falls back and accepts
        // it best-effort, exactly like the existing oversize best-effort
        // path, with the oversize attention item (`oversize_remaining`)
        // still reported.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(oversize_tool_use_response()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(missing_confidence_tool_use_response()))
            .mount(&server)
            .await;

        let (outcome, audit) =
            plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        match outcome {
            PlannerOutcome::Success(out) => {
                assert_eq!(
                    out.tasks.len(),
                    1,
                    "must stage attempt 1's oversize-but-valid proposal, not discard it"
                );
                assert_eq!(out.tasks[0].handle, "national-rolling-points");
            }
            other => panic!("expected best-effort fallback to attempt 1's proposal, got {other:?}"),
        }
        assert_eq!(
            audit,
            DecompositionAudit {
                oversize_attempts: 1,
                oversize_remaining: 1,
            },
            "the fallback must still surface the oversize attention item",
        );
        assert_eq!(
            server.received_requests().await.expect("requests recorded").len(),
            PLANNER_VALIDATION_ATTEMPTS as usize,
            "must stop after exactly PLANNER_VALIDATION_ATTEMPTS calls",
        );
    }

    #[test]
    fn outcome_tags_are_stable() {
        assert_eq!(PlannerOutcome::NoApiKey.tag(), "no_api_key");
        assert_eq!(
            PlannerOutcome::ApiError {
                status: 429,
                snippet: "x".into()
            }
            .tag(),
            "api_error",
        );
        assert_eq!(PlannerOutcome::Transport("boom".into()).tag(), "transport_error",);
        assert_eq!(PlannerOutcome::InvalidOutput("nope".into()).tag(), "invalid_output",);
    }
}
