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
//! A healthy ~13-task proposal has nevertheless been observed to fail whole
//! because one array-typed field (`effort_audit`) came back as a single
//! JSON-encoded string rather than a JSON array — model flakiness on an
//! otherwise-valid result, not a prompt or schema defect. Two mitigations, in
//! order: (1) [`coerce_stringified_array_fields`] rewrites a known-array field
//! back into an array, when the string itself parses as one, before schema
//! validation runs; (2) if validation still fails, [`plan_with_url`] retries
//! (bounded by [`PLANNER_VALIDATION_ATTEMPTS`]) with the validation error fed
//! back into the prompt, rather than failing the whole run on one bad field.
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

/// Total attempts across the outer schema-validation retry loop in
/// [`Planner::plan`]. A model occasionally emits a tool call that violates
/// [`planner_output_schema`] (observed: an array-typed field like
/// `effort_audit` emitted as a single JSON-encoded string). That is model
/// flakiness, not a transient transport error, so [`PLANNER_ATTEMPTS`]'s
/// 429/5xx retry never sees it and a single miss used to fail the whole
/// proposal. Bounded at 2 (one retry): the retry re-sends the request with the
/// validation error appended to the prompt, so the model can see and correct
/// exactly what it got wrong.
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
    pub async fn plan(api_key: Option<&str>, input: &PlannerInput) -> PlannerOutcome {
        match api_key {
            None => {
                tracing::error!("planner: skipped — ANTHROPIC_API_KEY not configured",);
                PlannerOutcome::NoApiKey
            }
            Some(key) => plan_with_url(claude_client::ANTHROPIC_MESSAGES_URL, key, input).await,
        }
    }
}

/// Core of [`Planner::plan`] with the endpoint URL injected so tests can
/// drive it against a mock server. Hands each attempt's request to the shared
/// [`crate::claude_client`] pipeline (which owns 429/5xx/transport
/// retry/backoff) and, on a schema-validation failure, rebuilds the request
/// with the error fed back into the prompt and tries again — bounded by
/// [`PLANNER_VALIDATION_ATTEMPTS`] — before failing safe.
async fn plan_with_url(url: &str, api_key: &str, input: &PlannerInput) -> PlannerOutcome {
    let config = CallConfig::new(PLANNER_TIMEOUT)
        .with_retry(RetryPolicy::new(PLANNER_ATTEMPTS, PLANNER_BACKOFF))
        .with_endpoint(url);

    let mut validation_error: Option<String> = None;
    for attempt in 1..=PLANNER_VALIDATION_ATTEMPTS {
        let body = match &validation_error {
            None => build_request_body(input),
            Some(err) => build_retry_request_body(input, err),
        };
        match claude_client::send_messages_raw(api_key, &body, &config).await {
            Ok(response) => match planner_output_from_response(&response) {
                Ok(output) => return PlannerOutcome::Success(output),
                Err(msg) => {
                    tracing::warn!(
                        attempt,
                        max_attempts = PLANNER_VALIDATION_ATTEMPTS,
                        err = %msg,
                        "planner: schema-invalid output; retrying with validation feedback",
                    );
                    validation_error = Some(msg);
                }
            },
            Err(err) => return outcome_from_error(err),
        }
    }
    // Exhausted the validation-retry budget; `validation_error` is always
    // `Some` here (the loop only continues past attempt 1 by setting it).
    PlannerOutcome::InvalidOutput(validation_error.unwrap_or_else(|| "exhausted planner validation retries".to_owned()))
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
/// the single user turn also carries the previous attempt's schema-validation
/// error, so the model can see and correct exactly what it got wrong instead
/// of repeating the same mistake blind.
fn build_retry_request_body(input: &PlannerInput, validation_error: &str) -> Value {
    let mut body = build_request_body(input);
    if let Some(content) = body
        .get_mut("messages")
        .and_then(|messages| messages.get_mut(0))
        .and_then(|message| message.get_mut("content"))
    {
        *content = Value::String(retry_user_prompt(input, validation_error));
    }
    body
}

/// The retry user turn: the normal prompt plus an explicit rejection notice
/// naming the schema-validation error from the previous attempt.
fn retry_user_prompt(input: &PlannerInput, validation_error: &str) -> String {
    let mut out = build_user_prompt(input);
    out.push_str(&format!(
        "\n--- YOUR PREVIOUS emit_task_graph CALL WAS REJECTED ---\n\
         Schema validation error: {validation_error}\n\
         Every field must have exactly the JSON type the schema declares — in \
         particular, array fields (`tasks`, `edges`, `effort_audit`) must be \
         emitted as a JSON array, never as a single JSON-encoded string \
         containing one. Call `emit_task_graph` again with a schema-valid \
         payload that fixes this.\n\
         --- END REJECTION NOTICE ---\n"
    ));
    out
}

/// Map a shared [`ClaudeError`] into the matching [`PlannerOutcome`]. Transport
/// and decode failures are both "we couldn't get usable bytes back", so they
/// bucket together.
fn outcome_from_error(err: ClaudeError) -> PlannerOutcome {
    match err {
        ClaudeError::Api { status, body } => PlannerOutcome::ApiError {
            status,
            snippet: clip(&body, 200),
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
    let mut output = serde_json::from_value::<PlannerOutput>(input)
        .map_err(|err| format!("tool input did not match the PlannerOutput schema: {err}"))?;
    normalize_output_text(&mut output);
    Ok(output)
}

/// Top-level [`PlannerOutput`] fields the schema requires to be a JSON array.
const ARRAY_TYPED_FIELDS: &[&str] = &["tasks", "edges", "effort_audit"];

/// Undo a model slip observed in production (T-planner-string-array): an
/// array-typed field emitted as a single JSON-encoded string instead of an
/// actual JSON array — e.g. `"effort_audit": "[\"[effort-classification] …\"]"`
/// rather than `"effort_audit": ["[effort-classification] …"]`. The model's
/// *content* is fine; only the outer JSON type is wrong. When one of
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
/// too many). Applied once here, right after deserialisation, so every
/// downstream consumer (the `planner_runs` audit row, the Materializer, the
/// app UI) sees clean text instead of each display site having to band-aid
/// around it.
fn normalize_output_text(output: &mut PlannerOutput) {
    output.notes = unescape_over_escaped(&output.notes);
    for line in &mut output.effort_audit {
        *line = unescape_over_escaped(line);
    }
    for task in &mut output.tasks {
        task.name = unescape_over_escaped(&task.name);
        task.description = unescape_over_escaped(&task.description);
    }
}

/// Replace literal `\"` and `\n` (backslash followed by a literal character,
/// as opposed to an actual quote/newline) with the character they were
/// meant to represent.
fn unescape_over_escaped(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\\"", "\"")
}

/// Clip a string to `max` bytes on a char boundary, appending an ellipsis if
/// truncated. Used to bound the error snippet stored in [`PlannerOutcome`].
fn clip(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
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
- ALSO add the identical line to the `effort_audit` array — one entry per \
task, in the same order as `tasks`.\n\
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
co-edit the same file(s), add a `blocks` edge so they land in a defined order \
and note in the dependent task's `description` that it must **forward-port the \
sibling's changes preservingly** (integrate, never delete). Do NOT over-index \
on this — a little incidental overlap is not enough; if you serialise every \
pair that shares a file, every project becomes linear. Parallel throughput \
stays the default; sequence only on clear, substantial overlap.\n\
\n\
Each edge is { \"dependent\": <handle that waits>, \"prerequisite\": <handle \
that must land first> }. Both endpoints must be handles you emitted.\n\
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
        // P5-lite (incident-002): the planner must weigh file overlap, but
        // only serialise on clear/substantial overlap so throughput stays the
        // default.
        assert!(SYSTEM_PROMPT.contains("file** overlap"));
        assert!(SYSTEM_PROMPT.contains("forward-port the sibling's changes preservingly"));
        assert!(SYSTEM_PROMPT.contains("Parallel throughput stays the default"));
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
    fn coerces_a_stringified_effort_audit_array_before_validation() {
        // Reproduces the production failure: the model emitted `effort_audit`
        // as a single JSON-encoded string (`"[\"[effort-classification] …\"]"`)
        // instead of a JSON array, which used to fail the whole otherwise-valid
        // proposal with a serde type-mismatch error.
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "h",
                        "name": "Task",
                        "description": "Do the thing.",
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
        }));
        let out = planner_output_from_response(&response)
            .expect("stringified-array effort_audit must be coerced and accepted");
        assert_eq!(out.effort_audit.len(), 1);
        assert_eq!(
            out.effort_audit[0],
            "[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\""
        );
    }

    #[test]
    fn does_not_coerce_a_non_json_string_field() {
        // A field that is a string but does not itself parse as a JSON array
        // (e.g. a genuine free-text mistake, not the known stringified-array
        // slip) must still be rejected by schema validation rather than
        // silently accepted.
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": false,
                    "notes": "n",
                    "effort_audit": "not an array at all"
                }
            }]
        }));
        assert!(
            planner_output_from_response(&response).is_err(),
            "a non-JSON-array string must still fail validation",
        );
    }

    #[test]
    fn normalizes_over_escaped_notes_and_effort_audit() {
        // Guards against a model that over-escapes its JSON tool-call
        // arguments (writes `\\"` / `\\n` where a single JSON escape level
        // was meant), which otherwise survives the JSON decode as literal
        // backslash-quote and backslash-n sequences.
        let response = response_from(json!({
            "content": [{
                "type": "tool_use",
                "name": TOOL_NAME,
                "input": {
                    "tasks": [{
                        "handle": "h",
                        "name": "Over-escaped \\\"name\\\"",
                        "description": "First paragraph.\\n\\nSecond paragraph with a \\\"quote\\\".",
                        "kind": "project_task",
                        "effort": "small",
                        "ordinal": 0
                    }],
                    "edges": [],
                    "confidence": "high",
                    "breakdown_found": true,
                    "notes": "the doc's \\\"Proposed implementation task breakdown\\\" section\\n\\nmore prose",
                    "effort_audit": ["[effort-classification] level=`small` matched-rule=`rule 5` reasons=\\\"x\\\""]
                }
            }]
        }));
        let out = planner_output_from_response(&response).expect("valid tool_use response parses");
        assert_eq!(
            out.notes,
            "the doc's \"Proposed implementation task breakdown\" section\n\nmore prose"
        );
        assert_eq!(
            out.effort_audit[0],
            "[effort-classification] level=`small` matched-rule=`rule 5` reasons=\"x\""
        );
        assert_eq!(out.tasks[0].name, "Over-escaped \"name\"");
        assert_eq!(
            out.tasks[0].description,
            "First paragraph.\n\nSecond paragraph with a \"quote\"."
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
        let outcome = Planner::plan(None, &sample_input()).await;
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

        let outcome = plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;

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

        let outcome = plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
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
                        "description": "Do the thing.",
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
    async fn end_to_end_stringified_array_is_coerced_without_a_retry_round_trip() {
        // Regression test for the production incident: the model's array
        // field arrived as a JSON-encoded string. This must succeed off the
        // *first* HTTP call via coercion — no second round trip needed. Only
        // one response is mounted (`up_to_n_times(1)`); if the code
        // mistakenly retried, the second call would get no matching mock.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(stringified_effort_audit_tool_use_response()))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let outcome = plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        match outcome {
            PlannerOutcome::Success(out) => assert_eq!(out.effort_audit.len(), 1),
            other => panic!("expected Success via coercion, got {other:?}"),
        }
        assert_eq!(
            server.received_requests().await.expect("requests recorded").len(),
            1,
            "coercion must resolve the stringified array on the first call, with no retry round trip",
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

        let outcome = plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        assert!(
            matches!(outcome, PlannerOutcome::Success(_)),
            "expected success after the validation retry, got {outcome:?}",
        );

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

        let outcome = plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
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

        let outcome = plan_with_url(&format!("{}/v1/messages", server.uri()), "test-key", &sample_input()).await;
        match outcome {
            PlannerOutcome::ApiError { status, .. } => assert_eq!(status, 401),
            other => panic!("expected ApiError, got {other:?}"),
        }
        assert_eq!(outcome.tag(), "api_error");
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
