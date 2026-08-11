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
//! [`plan_with_call`] retries (bounded by [`PLANNER_VALIDATION_ATTEMPTS`])
//! with the validation error fed back into the prompt; only when no attempt
//! ever produced a schema-valid output does the run fail whole
//! ([`PlannerOutcome::InvalidOutput`]).
//!
//! ## Breakdown authority (not a decomposition engine)
//!
//! The design doc's implementation-task breakdown is authoritative: entry
//! count in, row count out. The user prompt surfaces a discrete inventory of
//! parsed entries (`###` headings, or a numbered / bold-bullet fallback) so the
//! model cannot freely re-segment a
//! single Scope paragraph. Dependency edges come only from what each entry
//! declares (`Dependencies: none` → no edges). A prior "oversize task"
//! re-prompt that forced multi-layer / multi-clause entries into invented
//! per-subsystem DAGs is deliberately retired — that path undid design-time
//! breakdown honesty (one entry → ten invented rows).
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
use crate::utility_model::{UtilityCall, UtilityModel, UtilityTask};

/// The model the Planner runs on. A direct API call needs a concrete model
/// id (the `--model` family aliases used for worker dispatch are resolved by
/// the `claude` CLI, not the Messages API), so this is pinned rather than an
/// alias. Opus is deliberate: planning quality matters and the call is
/// infrequent (once per project), unlike the Haiku one-liner in
/// [`crate::live_status`]. Tunable here without a schema change (design R5).
///
/// This is the *default*; the model actually used comes from the
/// [`UtilityModel`] provider, whose per-task selection is exactly what keeps
/// the Planner on Opus while the cheap one-liner sits on Haiku.
pub const PLANNER_MODEL: &str = UtilityTask::Planner.default_model();

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
/// [`Planner::plan`]. The loop re-prompts only for **schema-invalid output** —
/// a model occasionally emits a tool call that violates
/// [`planner_output_schema`] (observed: an array-typed field like `edges`
/// emitted as a single JSON-encoded string; historically also `effort_audit`,
/// before the model stopped being asked to emit it — see the module doc).
/// That is model flakiness, not a transient transport error, so
/// [`PLANNER_ATTEMPTS`]'s 429/5xx retry never sees it and a single miss used
/// to fail the whole proposal.
///
/// Bounded at 2 (one retry): the retry re-sends the request with the
/// validation error appended to the prompt, so the model can see and correct
/// exactly what it got wrong. Only when no attempt produces a schema-valid
/// output does the run fail ([`PlannerOutcome::InvalidOutput`]).
///
/// **Not** a decomposition gate. A prior oversize re-prompt path forced the
/// model to invent tasks by splitting multi-clause / multi-layer design
/// entries the author had already sized; that undid design-time breakdown
/// honesty and is deliberately gone. The design doc's entry count is
/// authoritative — an over-large single row is cheap to fix by hand; ten
/// invented rows with a fabricated dependency graph are not.
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
            PlannerOutcome::NoApiKey => "no utility-model credential configured on the engine".to_owned(),
            PlannerOutcome::ApiError { status, snippet } => {
                format!("anthropic returned {status}: {snippet}")
            }
            PlannerOutcome::Transport(err) => err.clone(),
            PlannerOutcome::InvalidOutput(err) => err.clone(),
        }
    }
}

/// The Planner. A zero-sized entry point so callers write the
/// `Planner::plan(..)` shape the design names; the Planner holds no state
/// (it is a pure transform).
pub struct Planner;

impl Planner {
    /// Plan the implementation task graph for one project from its merged
    /// design doc.
    ///
    /// `utility` is passed in (not read from config here) so the Planner
    /// stays a pure transform with no config/DB dependency — the caller
    /// sources it from `RuntimeConfig::utility_model`, mirroring
    /// [`crate::live_status::summarize_transcript`]. A provider that cannot
    /// resolve a credential for [`UtilityTask::Planner`] short-circuits to
    /// [`PlannerOutcome::NoApiKey`] without a network call.
    ///
    /// The shared [`crate::claude_client`] pipeline retries transient failures
    /// (transport errors and HTTP 429/5xx/overloaded) once before failing safe;
    /// a non-retryable 4xx, a decode failure, or output that fails schema
    /// validation is surfaced immediately, mapped into a [`PlannerOutcome`].
    pub async fn plan(utility: &dyn UtilityModel, input: &PlannerInput) -> PlannerOutcome {
        match utility.resolve(UtilityTask::Planner) {
            Err(err) => {
                tracing::error!(%err, "planner: skipped — no utility-model credential");
                PlannerOutcome::NoApiKey
            }
            Ok(call) => plan_with_call(&call, input).await,
        }
    }
}

/// Why the previous attempt's output was rejected, carried into the next
/// attempt's prompt so the model can see and fix exactly what was wrong.
/// Only schema-invalid output is re-prompted ([`PLANNER_VALIDATION_ATTEMPTS`]).
enum RetryFeedback {
    /// The tool call failed [`planner_output_schema`] validation.
    Schema(String),
}

/// Core of [`Planner::plan`] with the resolved [`UtilityCall`] injected so
/// tests can drive it against a mock server. Hands each attempt's request to the shared
/// [`crate::claude_client`] pipeline (which owns 429/5xx/transport
/// retry/backoff) and, on a schema-validation failure, rebuilds the request
/// with the reason fed back into the prompt and tries again, bounded by
/// [`PLANNER_VALIDATION_ATTEMPTS`], before failing safe.
///
/// Schema-valid proposals are accepted as-is. The planner does **not** re-prompt
/// to split multi-clause or multi-layer tasks — the design doc's breakdown is
/// authoritative (entry count in, row count out).
async fn plan_with_call(call: &UtilityCall, input: &PlannerInput) -> PlannerOutcome {
    let config = CallConfig::new(PLANNER_TIMEOUT)
        .with_retry(RetryPolicy::new(PLANNER_ATTEMPTS, PLANNER_BACKOFF, PLANNER_BACKOFF))
        .with_endpoint(call.endpoint.clone());

    let mut feedback: Option<RetryFeedback> = None;
    for attempt in 1..=PLANNER_VALIDATION_ATTEMPTS {
        let body = match &feedback {
            None => build_request_body(input, &call.model),
            Some(fb) => build_retry_request_body(input, fb, &call.model),
        };
        match claude_client::send_messages_raw(&call.api_key, &body, &config).await {
            Ok(response) => match planner_output_from_response(&response) {
                Ok(output) => {
                    return PlannerOutcome::Success(output);
                }
                Err(msg) => {
                    if attempt >= PLANNER_VALIDATION_ATTEMPTS {
                        return PlannerOutcome::InvalidOutput(msg);
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
            Err(err) => return outcome_from_error(err),
        }
    }
    // Unreachable in practice: the final iteration returns in every branch.
    // Kept as a fail-safe.
    PlannerOutcome::InvalidOutput("exhausted planner validation retries".to_owned())
}

/// Assemble the Anthropic Messages request body. Public so tests and future
/// callers can inspect the exact request shape.
pub fn build_request_body(input: &PlannerInput, model: &str) -> Value {
    json!({
        "model": model,
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
/// existing-task dedup hint, a structured inventory of breakdown entries
/// (when parseable), and the full design doc to read.
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

    // Discrete entry inventory: entry boundaries are a hard unit of work.
    // Passing only free prose let the model re-segment a single entry's Scope
    // paragraph into many invented tasks; listing the parsed entries first
    // makes the entry count and per-entry Dependencies field explicit.
    let entries = extract_breakdown_entries(&input.design_doc);
    if !entries.is_empty() {
        out.push_str(&format!(
            "--- AUTHORITATIVE BREAKDOWN ENTRIES ({N} entr{plural}) ---\n\
             These were parsed from the design doc's implementation task \
             breakdown. By default emit **exactly one task per entry** and \
             **only the dependency edges each entry states**. Do not split an \
             entry's Scope paragraph into multiple tasks. Do not invent edges \
             the entry does not declare (if it says `Dependencies: none`, emit \
             no edges for it).\n\n",
            N = entries.len(),
            plural = if entries.len() == 1 { "y" } else { "ies" },
        ));
        for (i, entry) in entries.iter().enumerate() {
            out.push_str(&format!("### Entry {} — {}\n", i + 1, entry.title));
            if !entry.body.trim().is_empty() {
                out.push_str(entry.body.trim());
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str("--- END AUTHORITATIVE BREAKDOWN ENTRIES ---\n\n");
    }

    out.push_str(
        "Below is the full merged design document. Read its implementation \
         breakdown and call the `emit_task_graph` tool with the proposed \
         task graph. Prefer the authoritative entry inventory above when it \
         is present; use the full doc for context and for any entries the \
         inventory could not parse.\n\n",
    );
    out.push_str(&format!("--- BEGIN DESIGN DOC ({}) ---\n", input.design_doc_ref.path));
    out.push_str(&input.design_doc);
    if !input.design_doc.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("--- END DESIGN DOC ---\n");
    out
}

/// One discrete unit from a design doc's implementation task breakdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakdownEntry {
    /// Entry title (`###` heading text, or the first line of a numbered item).
    pub title: String,
    /// Remainder of the entry (Scope, Effort hint, Dependencies, free prose).
    pub body: String,
}

/// Headings that open a design-doc implementation breakdown section.
const BREAKDOWN_HEADINGS: &[&str] = &[
    "proposed implementation task breakdown",
    "follow-up implementation chores",
    "implementation plan",
    "implementation task breakdown",
];

/// Locate the breakdown section body (text after the matching `##` heading
/// until the next `##` heading, or EOF). Returns `None` when no recognised
/// breakdown heading is present.
pub fn extract_breakdown_section(doc: &str) -> Option<&str> {
    // Walk line-by-line so we match only ATX headings, not prose mentions.
    let all_lines: Vec<&str> = doc.lines().collect();
    let mut section_start: Option<usize> = None;
    for (idx, line) in all_lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            let heading = rest.trim().to_ascii_lowercase();
            if BREAKDOWN_HEADINGS.iter().any(|h| heading == *h) {
                // Body starts on the next line.
                section_start = Some(idx + 1);
                break;
            }
        }
    }
    let start_line = section_start?;
    let start_byte = line_byte_offset(doc, start_line);
    // End at the next ## heading (not ###).
    let mut end_byte = doc.len();
    for (idx, line) in all_lines.iter().enumerate().skip(start_line) {
        let trimmed = line.trim();
        if trimmed.starts_with("## ") && !trimmed.starts_with("### ") {
            end_byte = line_byte_offset(doc, idx);
            break;
        }
    }
    if start_byte > end_byte {
        return None;
    }
    Some(doc.get(start_byte..end_byte).unwrap_or("").trim())
}

/// Byte offset of the start of line `line_idx` (0-based) in `doc`.
fn line_byte_offset(doc: &str, line_idx: usize) -> usize {
    if line_idx == 0 {
        return 0;
    }
    let mut seen = 0usize;
    for (byte_idx, ch) in doc.char_indices() {
        if ch == '\n' {
            seen += 1;
            if seen == line_idx {
                return byte_idx + 1;
            }
        }
    }
    doc.len()
}

/// Parse discrete breakdown entries from a design doc.
///
/// Prefers `###` ATX headings inside the breakdown section (the modern
/// design-directive shape). Falls back to top-level numbered list items
/// (`1. …`, optionally `**1. …**`) when the section has no `###` entries,
/// then to `- **Name.** …` bold-bullet entries. Returns an empty vec when no
/// breakdown section or no entries are found — the planner still receives
/// the full doc and can fall back to free-form extraction, but that fallback
/// is a silent degradation from the hard entry boundary this function
/// exists to provide, so callers that build the authoritative-entries prompt
/// block log when a recognised section yields zero entries.
pub fn extract_breakdown_entries(doc: &str) -> Vec<BreakdownEntry> {
    let Some(section) = extract_breakdown_section(doc) else {
        return Vec::new();
    };
    let heading_entries = parse_hash_entries(section);
    if !heading_entries.is_empty() {
        return heading_entries;
    }
    let numbered_entries = parse_numbered_entries(section);
    if !numbered_entries.is_empty() {
        return numbered_entries;
    }
    let bullet_entries = parse_bullet_entries(section);
    if bullet_entries.is_empty() {
        tracing::warn!(
            "planner: design doc has a recognised breakdown section but no entries parsed \
             from it (no ### headings, numbered items, or bold bullets) — falling back to \
             free-form extraction from prose"
        );
    }
    bullet_entries
}

/// True when `line`, trimmed, opens or closes a fenced code block (``` or
/// ~~~, at least 3 chars). Callers toggle an `in_fence` flag on this so a
/// `### `/numbered-looking line inside a fence is never mistaken for an
/// entry marker — mirrors the fence handling in
/// `boss_engine_editorial::split_markdown_segments`.
fn is_fence_delimiter(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Split a breakdown section on `###` headings into entries. Skips a leading
/// `Breakdown size: N entries …` prose line (not an entry), and ignores
/// anything inside a fenced code block.
fn parse_hash_entries(section: &str) -> Vec<BreakdownEntry> {
    let mut entries = Vec::new();
    let mut current_title: Option<String> = None;
    let mut body_lines: Vec<&str> = Vec::new();
    let mut in_fence = false;

    for line in section.lines() {
        if is_fence_delimiter(line) {
            in_fence = !in_fence;
            if current_title.is_some() {
                body_lines.push(line);
            }
            continue;
        }
        if in_fence {
            if current_title.is_some() {
                body_lines.push(line);
            }
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("### ") {
            if let Some(title) = current_title.take() {
                entries.push(BreakdownEntry {
                    title,
                    body: body_lines.join("\n"),
                });
                body_lines.clear();
            }
            current_title = Some(rest.trim().to_owned());
            continue;
        }
        if current_title.is_some() {
            body_lines.push(line);
        }
        // Lines before the first ### (e.g. Breakdown size: …) are ignored.
    }
    if let Some(title) = current_title {
        entries.push(BreakdownEntry {
            title,
            body: body_lines.join("\n"),
        });
    }
    entries
}

/// Fallback: numbered list items (`1. Title` / `1. Title — scope`, optionally
/// wrapped in `**…**`/`__…__` emphasis) as entries. An indented line (a
/// sub-step nested under a real entry) is never promoted to a top-level
/// entry; a fenced code block's contents are never scanned for markers.
fn parse_numbered_entries(section: &str) -> Vec<BreakdownEntry> {
    let mut entries = Vec::new();
    let mut current_title: Option<String> = None;
    let mut body_lines: Vec<&str> = Vec::new();
    let mut in_fence = false;

    for line in section.lines() {
        if is_fence_delimiter(line) {
            in_fence = !in_fence;
            if current_title.is_some() {
                body_lines.push(line);
            }
            continue;
        }
        if in_fence {
            if current_title.is_some() {
                body_lines.push(line);
            }
            continue;
        }
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if !is_indented && let Some(rest) = strip_numbered_item(trimmed) {
            if let Some(title) = current_title.take() {
                entries.push(BreakdownEntry {
                    title,
                    body: body_lines.join("\n"),
                });
                body_lines.clear();
            }
            current_title = Some(rest.to_owned());
            continue;
        }
        if current_title.is_some() {
            body_lines.push(line);
        }
    }
    if let Some(title) = current_title {
        entries.push(BreakdownEntry {
            title,
            body: body_lines.join("\n"),
        });
    }
    entries
}

/// Second fallback: `- **Name.** rest of line…` bullet entries, e.g.
/// `- **6f-4: protocol additions.** protocol additions are described...`.
/// The bold span is the title; text after the closing emphasis on the same
/// line, plus any indented continuation lines, becomes the body.
fn parse_bullet_entries(section: &str) -> Vec<BreakdownEntry> {
    let mut entries = Vec::new();
    let mut current_title: Option<String> = None;
    let mut body_lines: Vec<String> = Vec::new();
    let mut in_fence = false;

    for line in section.lines() {
        if is_fence_delimiter(line) {
            in_fence = !in_fence;
            if current_title.is_some() {
                body_lines.push(line.to_owned());
            }
            continue;
        }
        if in_fence {
            if current_title.is_some() {
                body_lines.push(line.to_owned());
            }
            continue;
        }
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if !is_indented && let Some((title, rest)) = strip_bullet_bold_item(trimmed) {
            if let Some(prev_title) = current_title.take() {
                entries.push(BreakdownEntry {
                    title: prev_title,
                    body: body_lines.join("\n"),
                });
                body_lines.clear();
            }
            current_title = Some(title.to_owned());
            if !rest.trim().is_empty() {
                body_lines.push(rest.trim().to_owned());
            }
            continue;
        }
        if current_title.is_some() && !trimmed.is_empty() {
            body_lines.push(trimmed.to_owned());
        }
    }
    if let Some(title) = current_title {
        entries.push(BreakdownEntry {
            title,
            body: body_lines.join("\n"),
        });
    }
    entries
}

/// If `line` is a `- **Bold title.** rest…` bullet, return `(title, rest)`.
fn strip_bullet_bold_item(line: &str) -> Option<(&str, &str)> {
    let after_dash = line.strip_prefix("- ")?;
    let after_open = after_dash.strip_prefix("**")?;
    let close_idx = after_open.find("**")?;
    let title = after_open[..close_idx].trim().trim_end_matches('.').trim();
    if title.is_empty() {
        return None;
    }
    let rest = &after_open[close_idx + 2..];
    Some((title, rest))
}

/// If `line` is a numbered list item (`1. foo` / `12. foo`), optionally
/// wrapped in leading `*`/`_` emphasis markers (`**1. foo**`), return the
/// text after the marker (with a matching trailing emphasis marker
/// stripped); otherwise `None`. Requires a literal space after the `.` so
/// prose like `2.5x faster than before` never parses as an entry.
fn strip_numbered_item(line: &str) -> Option<&str> {
    let mut rest_line = line;
    while let Some(stripped) = rest_line.strip_prefix('*').or_else(|| rest_line.strip_prefix('_')) {
        rest_line = stripped;
    }
    let bytes = rest_line.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return None;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i >= bytes.len() || bytes[i] != b'.' {
        return None;
    }
    if bytes.get(i + 1) != Some(&b' ') {
        return None;
    }
    let rest = rest_line[i + 1..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let rest = rest
        .strip_suffix("**")
        .or_else(|| rest.strip_suffix("__"))
        .unwrap_or(rest);
    let rest = rest.trim_end();
    if rest.is_empty() { None } else { Some(rest) }
}

/// Build the retry request body: identical to [`build_request_body`] except
/// the single user turn also carries the previous attempt's rejection reason,
/// so the model can see and correct exactly what it got wrong instead of
/// repeating the same mistake blind.
fn build_retry_request_body(input: &PlannerInput, feedback: &RetryFeedback, model: &str) -> Value {
    let mut body = build_request_body(input, model);
    if let Some(content) = body
        .get_mut("messages")
        .and_then(|messages| messages.get_mut(0))
        .and_then(|message| message.get_mut("content"))
    {
        *content = Value::String(retry_user_prompt(input, feedback));
    }
    body
}

/// The retry user turn: the normal prompt plus an explicit schema-rejection notice.
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
/// `tasks` by position stay aligned.
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
/// serde still rejects it, and the retry loop in [`plan_with_call`] then feeds
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
/// otherwise apply by hand: the design-doc breakdown is authoritative, the Q4
/// effort heuristic, the kind conventions, the doc-stated edge guidance, and
/// the `[effort-classification]` emission contract. See design §2 "Encodes
/// coordinator policy".
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
Implementation Chores\", or \"Implementation Plan\". Each entry is typically \
a `###` heading (or a numbered list item) with a short name, a Scope \
paragraph, an Effort hint, and a Dependencies line. **The design doc's \
breakdown is authoritative.** It has already been through design review; \
the author decided what the units of work are. Your job is to materialise \
that decision, not to re-litigate it. Entry count in, row count out.\n\
\n\
- If the doc contains such a breakdown, set `breakdown_found` to true and \
emit **exactly one task per enumerated entry** (each `###` heading or each \
top-level numbered item is one entry).\n\
- If the user message includes an \"AUTHORITATIVE BREAKDOWN ENTRIES\" \
inventory, treat that list as the discrete units of work: one task per \
inventory entry by default.\n\
- If the doc is pure design rationale with NO enumerable implementation \
breakdown, set `breakdown_found` to false, return an empty `tasks` array and \
empty `edges`, and explain in `notes`. This is a clean, valid result — not \
an error. Never invent tasks the doc does not describe.\n\
\n\
The breakdown section may open with a `Breakdown size: N entries (M \
in-scope, K deferred) — <rationale>` line before the first entry. That line \
is the design author's self-check on how many entries the problem warranted; \
it is prose about the section, NOT an entry. Never emit it as a task. Do use \
it as a cross-check: if the entries you extracted differ materially in count \
from N, say so in `notes`.\n\
\n\
Do NOT propose:\n\
- The design task itself (it already exists and its PR has already merged).\n\
- Any task whose name duplicates one already in the project (the existing \
names are listed in the user message).\n\
- More than the task cap stated in the user message.\n\
- Tasks invented by re-segmenting one entry's Scope paragraph into clauses, \
layers, files, or subsystems the author already chose to keep together.\n\
\n\
## scope tiers — in-scope vs deferred\n\
\n\
A breakdown entry may carry a `Scope:` tag (exactly `Scope: in-scope` or \
`Scope: deferred (future / not a v1 blocker)`) put there by the design \
directive. Older docs written before this convention existed will instead \
say so in prose — phrases like \"future / not a v1 blocker\", \"out of \
scope\", \"deferred\", \"stretch goal\", or \"not for v1\" attached to an \
item. Treat either signal the same way.\n\
\n\
- **Never silently drop a deferred item.** It still becomes a task — \
omitting it forces the coordinator to guess what you considered and \
rejected.\n\
- **Set `deferred: true` on the proposed task.** The boolean is the \
primary/canonical mechanism — set it explicitly; do not encode deferral \
only in name/description prose. A residual `[deferred]` token in name or \
description is a Materializer fallback for stale model output only (not \
something you should emit). Keep the doc's own reason for deferring the \
item in the task's `description` (why it is future / not a v1 blocker). \
The Materializer files `tasks.deferred = 1` from this flag, which is what \
suppresses automatic execution minting on every path (normal reconcile, \
dependency auto-unblock cascade, project chain). A deferred row stays \
visible on the board and still unblocks when its prerequisites land, but \
nothing dispatches until an operator explicitly approves it.\n\
- **Do not gate in-scope work on a deferred task.** Never add an `edges` \
entry whose `prerequisite` is a task with `deferred: true` — deferred \
work is explicitly not being built now, so nothing in-scope should wait \
on it. A deferred task may still depend on an in-scope one (e.g. it \
builds on a shared root), which is a normal `dependent` edge.\n\
- Classify a deferred task's `kind` and `effort` exactly as you would any \
other task — the deferral is a scheduling signal, not a different shape of \
work. For in-scope items set `deferred: false`.\n\
\n\
## coordinator-only landing site gate\n\
\n\
Every task you propose is dispatched to a cube worker, which leases a repo \
workspace. A cube worker cannot read or write coordinator-private state: the \
Boss coordinator's private memory store, the engine's runtime database, or \
any artifact under the coordinator's local Application Support directory. A \
task whose entire deliverable is an action on that state is unexecutable by \
any worker, no matter how clearly it is briefed.\n\
\n\
- **Never emit a `project_task` (or `investigation` task) whose landing \
site is coordinator-only state.** Recognize this shape: \"update the \
coordinator's memory/runbook store\", \"prune/mark memory notes for \
deletion\", \"write to the runtime database\", \"update engine taxonomy \
state directly\" — none of these land in a repo, so none are worker-filable, \
even when the surrounding design doc's rationale is entirely legitimate.\n\
- **Account for it visibly — never just leave it out.** Unlike a deferred \
item (which still becomes a task with `deferred: true`), a coordinator-only \
item must NOT be emitted as a task at all: omit it from `tasks` and add one \
line per omitted item to `notes` describing the item and stating that it is \
coordinator-only work the coordinator must perform directly in-session.\n\
- A design doc can correctly *describe* a coordinator-only constraint (e.g. \
\"the memory store must stop being a runbook\") without every task derived \
from it being coordinator-only — judge each breakdown item on where ITS OWN \
deliverable lands, not on whether the doc's subject matter mentions \
coordinator state.\n\
- When only PART of a breakdown item is coordinator-only (e.g. it also \
touches a repo file), still emit the task but scope its `description` to \
the repo-side portion only, and note in `notes` that the coordinator-only \
portion was excluded and why.\n

## breakdown authority — do not re-expand entries\n\
\n\
**Default: one row per breakdown entry, zero invented work.** The entry \
boundary is a hard boundary. Transcribe each entry as one task whose `name` \
comes from the entry title and whose `description` is the entry's Scope (plus \
the `[effort-classification]` line). A one-entry breakdown produces one row \
and an empty edge set when that entry declares no dependencies.\n\
\n\
The following are **not** reasons to split an entry into multiple tasks:\n\
- a Scope paragraph with several clauses, or that names several files, \
directories, or subsystems;\n\
- an entry that spans more than one layer (protocol, engine, CLI, app) — a \
thin change across layers is still one reviewable PR when the design author \
kept it as one entry;\n\
- an entry that mentions tests alongside the behaviour they cover;\n\
- an entry longer than some number of words or characters;\n\
- any comparison against a target or typical entry count.\n\
\n\
**The one permitted exception — a high bar.** You may split an entry only \
where there is a *strong, articulable* reason to believe that entry is \
genuinely too large to execute as a single task (for example: two unrelated \
deliverables that cannot ship or be reviewed together, with no shared PR \
boundary). Multi-clause Scope, multi-layer scope, and length alone never \
meet this bar. When you do split, you **must** say so explicitly in \
`notes` — name the entry, the reason the bar was met, and what you split it \
into — so the decision is visible and reviewable rather than silent. When \
in doubt, do **not** split: emit one row and raise the concern in `notes` \
instead. An over-large single row is cheap to fix; ten invented rows with a \
fabricated dependency graph are not.\n\
\n\
- **Unknown-format discovery may stay an `investigation` kind** when the \
entry itself is framed as research / reverse-engineering — that is a kind \
choice for that entry, not a licence to invent extra entries.\n\
- Do **not** invent sibling tasks for subsystems, phases, or test sweeps \
named inside one entry's Scope.\n\
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
## dependency edges — only what the doc states\n\
\n\
**Edges come only from what the design doc states.** Read each entry's \
`Dependencies:` line (or equivalent prose such as \"Depends on …\" / \
\"Depends on: none\").\n\
\n\
- If an entry says `Dependencies: none` (or equivalent), emit **no edges** \
for that entry. Do not invent ordering from Scope prose, layer names, or a \
\"healthy\" fan-out shape.\n\
- If an entry names other entries as prerequisites, emit exactly those edges \
(map prerequisite names to the handles you assigned those entries).\n\
- Do **not** synthesise a schema-root / fan-out / integration DAG the doc did \
not declare. Do **not** chain tasks into a line just because they are listed \
in order — `ordinal` already carries the soft ordering hint.\n\
- Edges MUST form a DAG — never introduce a cycle.\n\
\n\
When the doc *does* leave two tasks independently startable and they are \
**clearly and substantially** likely to co-edit the same file(s), you may \
add a `merge_order_hints` entry naming the pair and the file(s)/surface — \
not a hard edge. A little incidental overlap is not enough.\n\
\n\
**A `merge_order_hints` entry is NOT a dependency edge and must never gate \
dispatch.** Both tasks stay independently startable — the hint only lets a \
later merge-time step order the two PRs and require the later one to \
forward-port the sibling's changes preservingly (integrate, never delete). \
Never use a `blocks` edge for file overlap alone; `edges` is reserved for \
doc-stated functional prerequisites (design's \"Parallel throughput stays the \
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
Put a short free-text rationale in `notes`: which section you read, the \
entry count you materialised, how you chose the edges (must cite doc-stated \
dependencies), and anything a human reviewer should know. If you split an \
entry under the high-bar exception, name the entry, the reason, and the \
split results here. If you kept a large multi-layer entry as one row but \
worry it is oversized, say so here rather than inventing a split.\
";

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{Confidence, DocRef, ProductContext, ProjectContext, TaskBrief};
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A [`UtilityCall`] pointing at a `wiremock` server, standing in for what
    /// the engine's provider would resolve for [`UtilityTask::Planner`].
    fn mock_call(server_uri: &str) -> UtilityCall {
        UtilityCall {
            provider: "anthropic".to_owned(),
            endpoint: format!("{server_uri}/v1/messages"),
            model: PLANNER_MODEL.to_owned(),
            api_key: "test-key".to_owned(),
        }
    }

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
                            "ordinal": 0,
                            "deferred": false
                        }, {
                            "handle": "engine-handler",
                            "name": "Engine handler",
                            "description": "Wire the handler.\n\n[effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` reasons=\"engine + protocol\"",
                            "kind": "project_task",
                            "effort": "medium",
                            "ordinal": 1,
                            "deferred": false
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
        let body = build_request_body(&sample_input(), PLANNER_MODEL);
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
        assert!(SYSTEM_PROMPT.contains("only what the doc states"));
        assert!(SYSTEM_PROMPT.contains("breakdown_found"));
        assert!(SYSTEM_PROMPT.contains("DAG"));
        // File-overlap coupling must stay a soft merge_order_hints entry —
        // never a hard edge that gates dispatch.
        assert!(SYSTEM_PROMPT.contains("merge_order_hints"));
        assert!(SYSTEM_PROMPT.contains("forward-port the sibling's changes preservingly"));
        assert!(SYSTEM_PROMPT.contains("is NOT a dependency edge and must never gate"));
        assert!(SYSTEM_PROMPT.contains("Never use a `blocks` edge for file overlap alone"));
    }

    /// Breakdown authority: the design doc's entries are the units of work.
    /// The prompt must forbid re-expanding multi-clause / multi-layer Scope
    /// into invented tasks, and must require doc-stated dependencies only.
    #[test]
    fn system_prompt_encodes_breakdown_authority() {
        assert!(SYSTEM_PROMPT.contains("breakdown is authoritative"));
        assert!(SYSTEM_PROMPT.contains("exactly one task per enumerated entry"));
        assert!(SYSTEM_PROMPT.contains("not** reasons to split"));
        assert!(SYSTEM_PROMPT.contains("high bar"));
        assert!(SYSTEM_PROMPT.contains("Dependencies: none"));
        assert!(SYSTEM_PROMPT.contains("only what the doc states"));
        assert!(SYSTEM_PROMPT.contains("do not re-expand entries"));
        // The retired sizing contract must not reappear as split-forcing policy.
        assert!(
            !SYSTEM_PROMPT.contains("re-prompted to decompose"),
            "must not threaten decomposition re-prompts"
        );
        assert!(
            !SYSTEM_PROMPT.contains("If a breakdown item needs a paragraph to describe, it is almost"),
            "must not treat paragraph-length Scope as an automatic split signal"
        );
        assert!(
            !SYSTEM_PROMPT.contains("Multi-subsystem scope is several tasks"),
            "must not force multi-layer entries into per-subsystem tasks"
        );
    }

    /// The Planner must not mint fully-startable tasks for design-doc items
    /// the design directive tags `Scope: deferred (future / not a v1
    /// blocker)` — it must still emit them (never silently drop), but set
    /// `deferred: true` rather than proposing them as ordinary in-scope
    /// work, and never let an in-scope task depend on one.
    #[test]
    fn system_prompt_encodes_scope_tier_handling() {
        assert!(SYSTEM_PROMPT.contains("Scope: in-scope"));
        assert!(SYSTEM_PROMPT.contains("Scope: deferred (future / not a v1 blocker)"));
        assert!(SYSTEM_PROMPT.contains("Never silently drop a deferred item"));
        assert!(SYSTEM_PROMPT.contains("Set `deferred: true` on the proposed task"));
        assert!(SYSTEM_PROMPT.contains("Do not gate in-scope work on a deferred task"));
        assert!(SYSTEM_PROMPT.contains("stretch goal"));
        // Boolean is primary/canonical; residual prose is Materializer fallback only.
        assert!(SYSTEM_PROMPT.contains("primary/canonical mechanism"));
        assert!(SYSTEM_PROMPT.contains("Materializer fallback for stale model output only"));
        assert!(
            !SYSTEM_PROMPT.contains("only mechanism the engine reads"),
            "do not claim the boolean is the only path — residual prose is a Materializer fallback"
        );
        assert!(
            !SYSTEM_PROMPT.contains("autostart = false"),
            "autostart alone does not park cascade dispatch; do not claim it does"
        );
    }

    /// The coordinator-only landing site gate must survive edits to this
    /// 200+ line literal: a cube worker cannot land work in coordinator-only
    /// state, so such an item must never be emitted as a dispatchable task.
    #[test]
    fn system_prompt_encodes_coordinator_only_gate() {
        assert!(SYSTEM_PROMPT.contains("coordinator-only landing site gate"));
        assert!(SYSTEM_PROMPT.contains("whose landing site is coordinator-only state"));
        assert!(SYSTEM_PROMPT.contains("coordinator must perform directly in-session"));
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
                        "ordinal": 0,
                        "deferred": false
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
        // `tasks`.
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
                        "ordinal": 0,
                        "deferred": false
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
                        "ordinal": 0,
                        "deferred": false
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
        let keyless = crate::utility_model::AnthropicUtilityModel::from_lookup(None, |_| None);
        let outcome = Planner::plan(&keyless, &sample_input()).await;
        assert!(matches!(outcome, PlannerOutcome::NoApiKey));
        assert_eq!(outcome.tag(), "no_api_key");
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

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;

        match outcome {
            PlannerOutcome::Success(out) => {
                assert_eq!(out.tasks.len(), 2);
                assert_eq!(out.edges.len(), 1);
                assert_eq!(out.confidence, Confidence::High);
            }
            other => panic!("expected Success, got {other:?}"),
        }
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

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;
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
                        "ordinal": 0,
                        "deferred": false
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

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;
        match outcome {
            PlannerOutcome::Success(out) => assert_eq!(out.effort_audit.len(), 1),
            other => panic!("expected Success via derivation, got {other:?}"),
        }
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

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;
        assert!(
            matches!(outcome, PlannerOutcome::Success(_)),
            "expected success after the validation retry, got {outcome:?}",
        );
        // A schema-invalid retry never trips the oversize gate.

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

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;
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

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;
        match outcome {
            PlannerOutcome::ApiError { status, .. } => assert_eq!(status, 401),
            other => panic!("expected ApiError, got {other:?}"),
        }
        assert_eq!(outcome.tag(), "api_error");
    }

    /// Faithful one-row materialisation of a multi-clause, multi-layer Scope
    /// with `Dependencies: none` — the failure shape that used to be force-split
    /// by the oversize re-prompt. Must be accepted on the first attempt with
    /// zero edges and no second API call.
    fn single_multi_clause_entry_tool_use_response() -> Value {
        json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "pause-only-forced-dispatch",
                        "name": "Implement pause-only forced dispatch end to end",
                        "description": "Add the shared read-only admission evaluation, distinct pause-override \
                            intent, and entry-point provenance to the protocol and engine, with the evaluator \
                            and mutating request using the same reason-producing function. Add `--force` only \
                            to `bossctl work start`. Before a macOS drag-to-Doing requests execution, call the \
                            evaluator. Add Bazel test coverage for the protocol and engine admission behavior, \
                            bossctl parsing and engine boundary, and macOS confirmation.\n\n\
                            [effort-classification] level=`medium` matched-rule=`rule 3 (multi-subsystem)` \
                            reasons=\"names protocol + engine + bossctl + macOS surfaces\"",
                        "kind": "project_task",
                        "effort": "medium",
                        "ordinal": 0,
                        "deferred": false
                    }],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "One entry, Dependencies: none — materialised as one row, zero edges.",
                    "effort_audit": []
                }
            }]
        })
    }

    #[tokio::test]
    async fn multi_clause_single_entry_is_accepted_without_reprompt() {
        // The reproduced failure: a design doc with one long multi-clause
        // Scope (protocol + engine + bossctl + macOS) and Dependencies: none.
        // The planner must accept a faithful one-row, zero-edge emission and
        // must NOT re-prompt for decomposition.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(single_multi_clause_entry_tool_use_response()))
            .mount(&server)
            .await;

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;
        match outcome {
            PlannerOutcome::Success(out) => {
                assert_eq!(out.tasks.len(), 1, "one entry → one row");
                assert_eq!(out.edges.len(), 0, "Dependencies: none → zero edges");
                assert_eq!(out.tasks[0].handle, "pause-only-forced-dispatch");
            }
            other => panic!("expected Success with the single-entry plan, got {other:?}"),
        }
        assert_eq!(
            server.received_requests().await.expect("requests recorded").len(),
            1,
            "must not re-prompt to split a multi-clause single entry",
        );
    }

    #[tokio::test]
    async fn schema_invalid_then_valid_succeeds_without_oversize_path() {
        // Schema retry still works: first response missing required field,
        // second is valid. No oversize decomposition path involved.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(missing_confidence_tool_use_response()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(single_multi_clause_entry_tool_use_response()))
            .mount(&server)
            .await;

        let outcome = plan_with_call(&mock_call(&server.uri()), &sample_input()).await;
        match outcome {
            PlannerOutcome::Success(out) => {
                assert_eq!(out.tasks.len(), 1);
                assert!(out.edges.is_empty());
            }
            other => panic!("expected Success after schema retry, got {other:?}"),
        }
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 2);
        let retry_body: Value = requests[1].body_json().expect("retry body is JSON");
        let retry_prompt = retry_body["messages"][0]["content"]
            .as_str()
            .expect("retry content is a string");
        assert!(
            retry_prompt.contains("Schema validation error"),
            "retry must be schema feedback, not oversize: {retry_prompt}",
        );
        assert!(
            !retry_prompt.contains("OVERSIZE"),
            "must not re-introduce oversize decomposition feedback",
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

    /// Fixture matching the reproduced failure doc shape: one `###` entry with
    /// a long multi-clause Scope and `Dependencies: none`.
    const SINGLE_ENTRY_DOC: &str = "\
# Operator force bypasses only the observed global dispatch pause\n\
\n\
## Goals\n\
\n\
- Dispatch one item while global pause remains.\n\
\n\
## Proposed implementation task breakdown\n\
\n\
### Implement pause-only forced dispatch end to end\n\
\n\
Scope: Add the shared read-only admission evaluation, distinct pause-override intent, and \
entry-point provenance to the protocol and engine, with the evaluator and mutating request \
using the same reason-producing function. Add `--force` only to `bossctl work start`, map it \
to that pause-only intent with CLI provenance, and surface engine refusal messages and JSON \
results without mapping it to the existing pool-growth bit. Before a macOS drag-to-Doing \
requests execution, call the evaluator, render the pause reason and any non-overridable \
blockers, and send the confirmed pause generation with app-drag provenance through the \
existing board gesture. Add Bazel test coverage for the protocol and engine admission \
behavior, `bossctl` parsing and engine boundary, and macOS confirmation, cancellation, \
changed or lifted pause, blockers, and refusal bounce-back.\n\
\n\
Effort hint: medium\n\
\n\
Dependencies: none\n\
";

    #[test]
    fn extracts_one_hash_entry_from_single_entry_breakdown() {
        let entries = extract_breakdown_entries(SINGLE_ENTRY_DOC);
        assert_eq!(entries.len(), 1, "exactly one ### entry");
        assert_eq!(entries[0].title, "Implement pause-only forced dispatch end to end");
        assert!(
            entries[0].body.to_ascii_lowercase().contains("dependencies: none"),
            "body must carry Dependencies: none: {}",
            entries[0].body
        );
        assert!(
            entries[0].body.contains("protocol") && entries[0].body.contains("bossctl"),
            "long multi-clause Scope must stay on the single entry"
        );
    }

    #[test]
    fn extracts_n_hash_entries() {
        let doc = "\
## Proposed implementation task breakdown\n\
\n\
### Protocol types\n\
\n\
Scope: Add the contract.\n\
\n\
Dependencies: none\n\
\n\
### Engine handler\n\
\n\
Scope: Wire the handler.\n\
\n\
Dependencies: Protocol types\n\
\n\
### CLI flag\n\
\n\
Scope: Expose --force.\n\
\n\
Dependencies: Engine handler\n\
";
        let entries = extract_breakdown_entries(doc);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].title, "Protocol types");
        assert_eq!(entries[1].title, "Engine handler");
        assert_eq!(entries[2].title, "CLI flag");
        assert!(entries[1].body.contains("Dependencies: Protocol types"));
    }

    #[test]
    fn extracts_numbered_list_entries_when_no_hash_headings() {
        let doc = "\
## Proposed implementation task breakdown\n\
\n\
1. Protocol types. Add the contract.\n\
2. Engine handler. Depends on 1.\n\
3. CLI surface.\n\
";
        let entries = extract_breakdown_entries(doc);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].title.starts_with("Protocol types"));
        assert!(entries[1].title.starts_with("Engine handler"));
    }

    #[test]
    fn extracts_numbered_entries_wrapped_in_bold_emphasis() {
        // `**1. Title**` shape (e.g. the tmux-in-coordinator design doc):
        // strip_numbered_item must skip the leading emphasis marker before
        // the digit test, and drop the matching trailing marker from the
        // title.
        let doc = "\
## Proposed implementation task breakdown\n\
\n\
**1. `boss-tmux` control crate**\n\
\n\
Scope: new crate.\n\
\n\
**2. tmux preflight gate**\n\
\n\
Scope: hard-dependency check.\n\
";
        let entries = extract_breakdown_entries(doc);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "`boss-tmux` control crate");
        assert_eq!(entries[1].title, "tmux preflight gate");
    }

    #[test]
    fn extracts_bold_bullet_entries_when_no_hash_or_numbered_headings() {
        // `- **Name.** rest of line…` shape (e.g. engine-app-rpc.md's
        // "Implementation plan" section) — third fallback.
        let doc = "\
## Implementation plan\n\
\n\
- **6f-4: protocol additions.** Adds RegisterAppSession and friends.\n\
```rust\n\
struct RegisterAppSession;\n\
```\n\
- **6f-5: engine-side dispatch.** ServerState tracks sessions.\n\
";
        let entries = extract_breakdown_entries(doc);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "6f-4: protocol additions");
        assert!(entries[0].body.contains("Adds RegisterAppSession"));
        assert!(entries[0].body.contains("```rust"));
        assert!(entries[0].body.contains("struct RegisterAppSession;"));
        assert_eq!(entries[1].title, "6f-5: engine-side dispatch");
    }

    #[test]
    fn numbered_fallback_rejects_decimals_and_indented_subitems() {
        // A standalone `2.5x faster than before` line must not parse as a
        // new entry "5x faster than before" (no space after the dot), and
        // an indented sub-step nested under a real entry must not be
        // promoted to a top-level entry — both must stay part of the
        // preceding entry's body.
        // Not built with the file's usual `"\` line-continuation style: that
        // escape also strips leading whitespace from the continued line,
        // which would silently erase the indentation this test exists to
        // check.
        let doc = "## Proposed implementation task breakdown\n\n1. Protocol types.\n2.5x faster than before.\n   1. nested sub-step, not a top-level entry\n2. Engine handler.\n";
        let entries = extract_breakdown_entries(doc);
        assert_eq!(entries.len(), 2, "got {entries:?}");
        assert!(entries[0].title.starts_with("Protocol types"));
        assert!(
            entries[0].body.contains("2.5x faster than before"),
            "the decimal-numbered line must stay part of the preceding entry's body: {:?}",
            entries[0].body
        );
        assert!(
            entries[0].body.contains("nested sub-step"),
            "the indented sub-step must stay part of the preceding entry's body: {:?}",
            entries[0].body
        );
        assert!(entries[1].title.starts_with("Engine handler"));
    }

    #[test]
    fn hash_and_numbered_fallbacks_skip_fenced_code_blocks() {
        // A `### ` (or numbered) line inside a fenced code block must not be
        // mistaken for an entry marker.
        let doc = "\
## Proposed implementation task breakdown\n\
\n\
### Real entry\n\
\n\
Scope: does the thing.\n\
\n\
```text\n\
### not an entry\n\
1. also not an entry\n\
```\n\
\n\
More scope prose after the fence.\n\
";
        let entries = extract_breakdown_entries(doc);
        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert_eq!(entries[0].title, "Real entry");
        assert!(entries[0].body.contains("### not an entry"));
        assert!(entries[0].body.contains("More scope prose after the fence"));
    }

    #[test]
    fn recognised_section_with_no_entries_returns_empty() {
        // A recognised heading whose body has no ###, numbered, or bullet
        // entries must still return an empty vec (free-prose fallback) —
        // this doc's "breakdown" is just a paragraph, not a list. The
        // This test pins the empty-vec contract for a free-prose breakdown.
        let doc = "\
## Proposed implementation task breakdown\n\
\n\
Just do the thing, no explicit entries here.\n\
";
        let entries = extract_breakdown_entries(doc);
        assert!(entries.is_empty());
    }

    #[test]
    fn user_prompt_lists_authoritative_single_entry_inventory() {
        let mut input = sample_input();
        input.design_doc = SINGLE_ENTRY_DOC.to_owned();
        input.design_doc_ref.path =
            "tools/boss/docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md".to_owned();
        let prompt = build_user_prompt(&input);
        assert!(
            prompt.contains("AUTHORITATIVE BREAKDOWN ENTRIES (1 entry)"),
            "must surface the discrete entry count: {prompt}"
        );
        assert!(prompt.contains("Implement pause-only forced dispatch end to end"));
        assert!(prompt.contains("Dependencies: none"));
        assert!(prompt.contains("exactly one task per entry"));
        // Full doc still present for context.
        assert!(prompt.contains("--- BEGIN DESIGN DOC ("));
        assert!(prompt.contains("operator-forced-dispatch-while-dispatch-is-paused.md"));
    }

    #[test]
    fn real_operator_forced_dispatch_doc_has_no_breakdown_after_ship() {
        // Pin against the real design doc via
        // //tools/boss/docs:operator_forced_dispatch_design_doc (see BUILD.bazel
        // compile_data) — not a testdata copy, so this cannot silently drift
        // from the doc it claims to cover.
        //
        // After mono#2705 shipped, the as-built postmortem replaced the
        // pre-ship "Proposed implementation task breakdown" with an as-shipped
        // layer table and "Outstanding work: none". The planner must therefore
        // see zero breakdown entries (no further design-scoped tasks to emit).
        // The single-entry shape is still covered by SINGLE_ENTRY_DOC fixtures
        // above; this pin only guards the live doc's post-ship contract.
        let doc = include_str!(env!("BOSS_OPERATOR_FORCED_DISPATCH_DESIGN_DOC"));
        let entries = extract_breakdown_entries(doc);
        assert!(
            entries.is_empty(),
            "as-built postmortem must not reintroduce a task breakdown; got {}: {:?}",
            entries.len(),
            entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>()
        );
        assert!(
            extract_breakdown_section(doc).is_none(),
            "as-built postmortem must not keep a recognised breakdown heading"
        );
    }

    #[test]
    fn faithful_single_entry_output_validates_with_zero_edges() {
        // Ground-truth PlannerOutput for the single-entry doc: one row, zero
        // edges — what the materializer must receive after a correct plan.
        use boss_protocol::{EffortLevel, ProposedTask, TaskKind};
        let output = PlannerOutput {
            tasks: vec![ProposedTask {
                handle: "pause-only-forced-dispatch".to_owned(),
                name: "Implement pause-only forced dispatch end to end".to_owned(),
                description: format!(
                    "{}\n\n[effort-classification] level=`medium` matched-rule=`rule 3` reasons=\"multi-layer thin change\"",
                    extract_breakdown_entries(SINGLE_ENTRY_DOC)[0].body.trim()
                ),
                kind: TaskKind::ProjectTask,
                effort: EffortLevel::Medium,
                ordinal: 0,
                deferred: false,
            }],
            edges: vec![],
            merge_order_hints: vec![],
            confidence: Confidence::High,
            breakdown_found: true,
            notes: "1 entry, Dependencies: none.".to_owned(),
            effort_audit: vec![],
        };
        assert_eq!(output.tasks.len(), 1);
        assert!(output.edges.is_empty());
        match boss_engine_planner_validation::validate(&output, 30) {
            boss_engine_planner_validation::ValidationResult::Valid { .. } => {}
            other => panic!("faithful single-entry plan must validate: {other:?}"),
        }
    }
}
