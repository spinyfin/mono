//! Three-way driver traffic allocation (experiment, operator-controlled).
//!
//! One persisted [`DriverTrafficSplit`] — three shares summing to exactly
//! 100 — decides how work that dispatches on its own row's driver is divided
//! between the `grok`, `claude`, and `codex` drivers. This generalises the
//! earlier single Codex percentage; see [`boss_protocol::DriverTrafficSplit`]
//! for why the split is modelled as three explicit integers with a hard sum
//! constraint rather than an implied remainder or normalised weights.
//!
//! **Which work that is, is mostly not decided here.** Allocation asks
//! [`boss_engine_driver::DriverRegistry::eligible_drivers_for_kind`] — i.e. the
//! dispatch capability gate, `CapabilityResolver::check_dispatch`, resolving
//! the row's `TaskKind` **and** the execution's own `ExecutionKind` (see
//! [`eligible_drivers_for`]) against each driver's declared `CapabilitySet` —
//! which of the three drivers may run a work item of that kind, and
//! renormalises the configured split over exactly that subset
//! ([`DriverTrafficSplit::driver_for_bucket_among`]). One source of truth: a
//! driver declared eligible for a kind can receive it, one that is not
//! cannot, and widening or narrowing eligibility is a `KindRequirements` /
//! `CapabilitySet` change that this path picks up with no edit of its own.
//! The earlier narrow slice (implementation kinds at `standard` reasoning
//! only) was an artifact of the original Codex experiment, not a statement
//! about capability, and is gone: `reasoning` still selects which *model* a
//! driver runs (`ModelMenu::model_for_reasoning`) but no longer decides
//! *which driver*. The one policy exception is the dedicated
//! design/investigation tier: original designs and `reasoning = investigation`
//! rows exclude Grok and are allocated only between Claude and Codex. A
//! `DesignPostmortem` is excluded by its distinct kind and remains ordinary
//! traffic-split work.
//!
//! The `ExecutionKind` dimension is why `conflict_resolution` and
//! `ci_remediation` do not silently become codex/grok-eligible just because
//! their underlying `tasks.kind` (an ordinary chore/task/revision) clears
//! every driver's gate on its own: `KindRequirements::for_kind` marks
//! `Capability::CommandOutcomeObservation` required-strict for those two
//! execution kinds — neither codex nor grok declares it — so the gate itself
//! refuses them until that capability is declared elsewhere (see the
//! Codex/Grok driver design docs' "review and conflict resolution" phase).
//! This is deliberately expressed at the gate, not as a second hardcoded
//! kind list here.
//!
//! **An execution whose driver is pinned by the pool it will run on** is
//! decided before allocation ever runs (`pr_review` and `automation_triage`):
//! those dispatch on
//! [`boss_engine_storage_values::pool_policy::pool_dispatch_policy_for_worker_id`]'s fixed
//! driver, so `decide_execution_driver` records that pool driver with
//! reason `pool` up front, rather than an allocation or a row/product pin.
//! See [`decide_execution_driver`] for the full reasoning.
//!
//! One thing allocation itself still declines, and it is not a claim about
//! capability:
//!
//! - **A row with an explicit pin** — see below. The dedicated
//!   design/investigation tier is also intentionally unpinned so it follows
//!   the Claude/Codex traffic split.
//!
//! Assignment is a deterministic hash of the work item's own id placed on
//! the split's `[0, 100)` bucket line — not a coin flip per dispatch
//! attempt — so the same row under the same split always lands on the same
//! driver across retries, redispatch, and recovery. Changing the split moves
//! the boundaries and therefore reallocates some rows that have not been
//! dispatched yet; that is deliberate, and it cannot make a row flip
//! *between attempts of one dispatch* because the decision is computed once,
//! at [`crate::revision_helpers::insert_execution`] time, and
//! persisted per-execution in `execution_driver_decisions` (see
//! [`record_execution_driver_decision`]).
//!
//! An explicit driver pin wins over allocation when the pin clears the
//! dispatch capability gate for the execution: the work item's own `driver`
//! column first, then its product's `default_driver`. A pin of a driver that
//! the gate refuses for this execution kind (today: codex/grok on
//! `conflict_resolution` / `ci_remediation`) is dropped so allocation can
//! place the row among the eligible drivers — otherwise the pin would stall
//! the worker at the spawn-time gate. Allocation only decides rows that
//! expressed no (honourable) preference at either level — which is also what
//! makes the shipped default behaviour-preserving for ordinary kinds (see
//! [`load_driver_traffic_split_conn`]).
//!
//! [`WorkDb::get_execution_driver_decision`] is the read side both real
//! dispatch paths ([`crate::driver_lookup`]'s events-socket resolver
//! and [`crate::runner::worker_spawn`]'s spawn-config resolver) consult, so
//! an allocated row is actually routed to its driver rather than the
//! recorded decision being an inert audit trail.

use boss_protocol::{DRIVER_SLUG_CLAUDE, DRIVER_SLUG_CODEX, DriverTrafficSplit};
use sha2::{Digest, Sha256};

use super::*;

/// Metadata KV key for the persisted split, stored as the JSON encoding of
/// [`DriverTrafficSplit`]. Same shape as `dispatch_concurrency_limit` (see
/// `app/handler_helpers.rs`): read fresh at each decision point rather than
/// cached, so a change takes effect on the very next execution created —
/// without disturbing any execution already dispatched (a decision already
/// recorded on an existing row is never recomputed).
///
/// Deliberately ONE row holding the whole triple rather than three rows: an
/// operator edit is a single atomic write, so a concurrent dispatch can
/// never read a half-applied edit that transits through an invalid split.
const METADATA_KEY_DRIVER_TRAFFIC_SPLIT: &str = "driver_traffic_split";

/// `execution_driver_decisions.reason`: the row (or its product) pinned a
/// driver; the split was not consulted.
pub const REASON_EXPLICIT: &str = "explicit";
/// `execution_driver_decisions.reason`: the row was placed on the split's
/// bucket line, renormalised over the drivers eligible for its kind.
/// Recorded whichever driver it landed on, so "allocated to claude" stays
/// distinguishable from "allocation never ran".
pub const REASON_ALLOCATION: &str = "allocation";
/// `execution_driver_decisions.reason`: the execution dispatches on a
/// pool's fixed driver, which overrides every row and product pin.
pub const REASON_POOL: &str = "pool";
/// `execution_driver_decisions.reason`: allocation did not decide this
/// execution — it is not bound to a `tasks` row. No override; the row
/// resolves through the ordinary precedence chain.
pub const REASON_DEFAULT: &str = "default";

/// The reason value written by the superseded single-Codex-percentage
/// scheme. Never written any more; still recognised on read so decisions
/// recorded before this landed keep their meaning instead of silently
/// degrading to `default`.
pub const REASON_LEGACY_PERCENTAGE: &str = "percentage";

/// The routing decision for one execution. `driver` is `None` only for
/// [`REASON_DEFAULT`] — an ineligible row, which falls through to the normal
/// row → product → engine default resolution. [`REASON_POOL`] names the
/// fixed driver the execution will actually run on. `split_at_decision` is
/// set for [`REASON_ALLOCATION`] so a later analysis can tell which split a
/// given row was placed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverDecision {
    pub driver: Option<String>,
    pub reason: &'static str,
    pub split_at_decision: Option<DriverTrafficSplit>,
}

impl DriverDecision {
    fn explicit(driver: String) -> Self {
        Self {
            driver: Some(driver),
            reason: REASON_EXPLICIT,
            split_at_decision: None,
        }
    }

    fn pool(driver: &'static str) -> Self {
        Self {
            driver: Some(driver.to_owned()),
            reason: REASON_POOL,
            split_at_decision: None,
        }
    }

    fn default_no_override() -> Self {
        Self {
            driver: None,
            reason: REASON_DEFAULT,
            split_at_decision: None,
        }
    }

    fn allocated(driver: &'static str, split: DriverTrafficSplit) -> Self {
        Self {
            driver: Some(driver.to_owned()),
            reason: REASON_ALLOCATION,
            split_at_decision: Some(split),
        }
    }
}

/// The drivers that may take a work item of `kind`, resolved from the
/// dispatch capability gate and nothing else.
///
/// This is the whole of allocation's eligibility rule. It builds the same
/// [`boss_engine_driver::DriverRegistry`] `runner::worker_spawn` builds and asks
/// the same `check_dispatch` question the gate asks immediately before a
/// pane spawns, so allocation cannot choose a driver that the gate would
/// then refuse — the two agree by construction rather than by keeping two
/// lists in step.
///
/// The candidate set is [`DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER`]:
/// allocation can only choose between drivers the split actually holds a
/// share for, and a slug the registry does not recognise is not eligible
/// (fail closed).
///
/// `execution_kind` is threaded through to the gate alongside `kind` because
/// some escalations live on the execution rather than the underlying task
/// row — see [`boss_engine_driver::KindRequirements`]'s doc for
/// `ConflictResolution` / `CiRemediation`, which is exactly why allocation
/// must decline those two kinds rather than treating every `tasks`-bound
/// execution kind as equally eligible.
fn eligible_drivers_for(
    kind: &TaskKind,
    execution_kind: &ExecutionKind,
    model_override: Option<&str>,
    design_or_investigation_tier: bool,
) -> Vec<&'static str> {
    let registry = boss_engine_driver::DriverRegistry::default();
    registry
        .eligible_drivers_for_kind(kind, Some(execution_kind), &DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER)
        .into_iter()
        .filter(|slug| !design_or_investigation_tier || *slug == DRIVER_SLUG_CLAUDE || *slug == DRIVER_SLUG_CODEX)
        .filter(|slug| {
            model_override.is_none_or(|model| {
                registry
                    .get(slug)
                    .is_some_and(|driver| (driver.descriptor().model_menu.model_belongs_to_driver)(model))
            })
        })
        .collect()
}

/// Deterministic hash of `work_item_id` into `[0, 100)`. SHA-256 rather
/// than `std::hash::Hasher` so the mapping is stable across engine
/// versions/restarts, not just within one process — the whole point is
/// that the same row always gets the same answer. Truncation to the first
/// 8 bytes (big-endian) before `% 100` introduces negligible bias for this
/// non-adversarial use.
fn hash_bucket(work_item_id: &str) -> u8 {
    let digest = Sha256::digest(work_item_id.as_bytes());
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(first8) % 100) as u8
}

/// Read the persisted driver traffic split using an already-open
/// `Connection`. Callers holding a `WorkDb::connect()` guard (e.g.
/// `insert_execution`, mid-transaction) MUST use this instead of
/// `WorkDb::get_metadata`, which opens its own guard on the same
/// non-reentrant mutex and would deadlock.
///
/// An absent value yields [`DriverTrafficSplit::default`] — `claude = 100`,
/// `grok = 0`, `codex = 0`. That is the behaviour-preserving state: `claude`
/// is `boss_engine_effort::ENGINE_DEFAULT_DRIVER`, and rows that pinned a
/// driver at row or product level never reach allocation at all, so an
/// engine that has never had a split configured routes exactly where it
/// always did. `grok` starting at 0 also means shipping this sends nothing
/// to the `grok` driver until an operator deliberately raises it.
///
/// A value that is present but does not parse, or parses to a split that
/// fails [`DriverTrafficSplit::validate`], can only come from a hand-edited
/// `state.db` — [`WorkDb::set_driver_traffic_split`] refuses to persist one.
/// It is logged at ERROR and the default is used. Note what this is NOT: the
/// corrupt value is discarded whole, never clamped or normalised into a
/// nearby valid-looking split. Failing the read outright would wedge every
/// subsequent execution insert, which is a far worse outcome than routing to
/// the default driver with a loud log line.
pub fn load_driver_traffic_split_conn(conn: &Connection) -> Result<DriverTrafficSplit> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![METADATA_KEY_DRIVER_TRAFFIC_SPLIT],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(DriverTrafficSplit::default());
    };
    match serde_json::from_str::<DriverTrafficSplit>(&raw) {
        Ok(split) => match split.validate() {
            Ok(()) => Ok(split),
            Err(err) => {
                tracing::error!(
                    raw = %raw,
                    %err,
                    "driver_traffic_split: persisted split is invalid — discarding it, allocating on the default split",
                );
                Ok(DriverTrafficSplit::default())
            }
        },
        Err(err) => {
            tracing::error!(
                raw = %raw,
                %err,
                "driver_traffic_split: persisted split is unparseable — discarding it, allocating on the default split",
            );
            Ok(DriverTrafficSplit::default())
        }
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// [`load_driver_traffic_split_conn`], but opens its own connection.
    /// For callers (RPC handlers, CLI) that are not already inside a
    /// `connect()` guard.
    pub fn get_driver_traffic_split(&self) -> Result<DriverTrafficSplit> {
        let conn = self.connect()?;
        load_driver_traffic_split_conn(&conn)
    }

    /// Persist `split`, or fail without writing anything when it does not
    /// sum to exactly 100. Nothing is clamped, redistributed, or normalised
    /// — an invalid split is the caller's error to surface, and
    /// all-three-zero is rejected by the same rule as every other bad total.
    ///
    /// Takes effect on the next execution created — never touches an
    /// execution already dispatched, exactly like the interactive
    /// concurrency cap. Written as one metadata row, so no concurrent
    /// dispatch can observe a partially-applied edit.
    pub fn set_driver_traffic_split(&self, split: DriverTrafficSplit) -> Result<DriverTrafficSplit> {
        split.validate()?;
        self.set_metadata(METADATA_KEY_DRIVER_TRAFFIC_SPLIT, &serde_json::to_string(&split)?)?;
        Ok(split)
    }

    /// Look up the recorded routing decision for `execution_id`, or `None`
    /// when no row exists (an execution created before this feature
    /// landed, or a kind this module never records for). Callers must not
    /// already hold a `connect()` guard.
    pub fn get_execution_driver_decision(&self, execution_id: &str) -> Result<Option<DriverDecision>> {
        let conn = self.connect()?;
        get_execution_driver_decision_conn(&conn, execution_id)
    }
}

/// `WorkDb::get_execution_driver_decision`, but reusing an already-open
/// `Connection` (see `load_driver_traffic_split_conn`'s doc for why this
/// split exists).
pub fn get_execution_driver_decision_conn(conn: &Connection, execution_id: &str) -> Result<Option<DriverDecision>> {
    conn.query_row(
        "SELECT driver, reason, split_at_decision
           FROM execution_driver_decisions
          WHERE execution_id = ?1",
        params![execution_id],
        |row| {
            let reason_raw: String = row.get(1)?;
            let reason = match reason_raw.as_str() {
                REASON_EXPLICIT => REASON_EXPLICIT,
                REASON_ALLOCATION => REASON_ALLOCATION,
                REASON_POOL => REASON_POOL,
                REASON_LEGACY_PERCENTAGE => REASON_LEGACY_PERCENTAGE,
                _ => REASON_DEFAULT,
            };
            let split_raw: Option<String> = row.get(2)?;
            Ok(DriverDecision {
                driver: row.get(0)?,
                reason,
                split_at_decision: split_raw.and_then(|raw| serde_json::from_str(&raw).ok()),
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// The work-item columns [`decide_execution_driver`] reads: the two pin
/// levels, the kind whose capability requirements decide eligibility, and
/// the automation provenance that says the driver comes from a pool instead.
/// A named row rather than a four-wide tuple so each column is read back by
/// name at the point it is used.
struct AllocationRow {
    explicit_driver: Option<String>,
    model_override: Option<String>,
    task_kind: String,
    reasoning: Option<String>,
    product_default_driver: Option<String>,
}

/// Whether `driver_slug` clears [`boss_engine_driver::CapabilityResolver::check_dispatch`]
/// for `(task_kind, execution_kind)`.
///
/// Unregistered slugs return `true` so the existing `UnknownDriver` failure
/// path still owns them — this helper only yields pins for *known* drivers
/// that the gate refuses (e.g. codex/grok on `ConflictResolution`). Used by
/// allocation, spawn composition, and the events-socket driver lookup so a
/// pin the gate will refuse never outranks a runnable substitute.
pub fn driver_clears_dispatch_gate(driver_slug: &str, task_kind: &TaskKind, execution_kind: &ExecutionKind) -> bool {
    let registry = boss_engine_driver::DriverRegistry::default();
    match registry.resolver(driver_slug) {
        Some(resolver) => resolver.check_dispatch(task_kind, Some(execution_kind)).is_ok(),
        None => true,
    }
}

/// Whether `driver_slug`'s [`boss_engine_driver::ModelMenu`] declares a dedicated
/// design/investigation-tier model. `grok` does not; `claude` and `codex` do.
/// An unregistered slug returns `false` (fail closed) — the same "unknown
/// driver" case is caught elsewhere with a loud `UnknownDriver` error.
///
/// Used both to decide whether a *pinned* driver on a design/investigation
/// row can be honoured without wedging spawn (see [`decide_execution_driver`]'s
/// pinned branch), and to decide whether a *recorded* traffic-allocation
/// decision must be substituted with [`boss_engine_effort::ENGINE_DEFAULT_DRIVER`]
/// at read time (see [`crate::driver_lookup::WorkDb::get_execution_driver_slug`]),
/// mirroring the substitution [`boss_engine_effort::resolve_spawn_config_in`]
/// applies at spawn time for a `TrafficAllocation`/`EngineDefault`-sourced
/// driver.
pub fn driver_has_design_investigation_model(driver_slug: &str) -> bool {
    let registry = boss_engine_driver::DriverRegistry::default();
    registry
        .get(driver_slug)
        .is_some_and(|driver| driver.descriptor().model_menu.design_investigation_model.is_some())
}

/// Decide which driver governs a newly-created execution row and why.
/// Called once, from `insert_execution`, using the same open `Connection`
/// (never `WorkDb::connect()` — see `load_driver_traffic_split_conn`).
///
/// Precedence:
/// 1. If the execution dispatches on a pool with a fixed driver, record that
///    pool driver. It overrides all row/product pins, matching dispatch.
///    Pool-bound executions take precedence because recording a row pin
///    would name a driver the worker never ran on:
///    [`boss_engine_storage_values::pool_policy::kind_always_dispatches_on_pool_driver`] covers
///    `pr_review` and `automation_triage`. Their `pool` decision names the
///    driver the worker actually ran on, matching what the events-socket resolver
///    ([`crate::driver_lookup::WorkDb::get_execution_driver_slug`])
///    independently returns for these kinds — see that function's doc.
///
///    **This is why PR review does not participate in the split.** It is a
///    decision, not an omission: reviews dispatch on the review pool's fixed
///    driver at a fixed strong model tier precisely so that who authored a
///    change cannot determine who reviews it (see
///    `pool_dispatch_policy_for_worker_id`'s doc). Putting reviews in the
///    split would either be inert (spawn ignores the allocation) or, if the
///    pool pin were removed to make it bite, would undo that independence
///    and the reviewer-model choice with it. Reviews participating is a
///    change to reviewer dispatch policy — one function, deliberately — not
///    a change to allocation.
/// 2. Otherwise the work item's own explicit `driver` column wins **when
///    that driver clears the capability gate for this execution kind**.
/// 3. Otherwise the product's `default_driver` wins under the same gate
///    check. Both of these are an operator saying "this work goes there";
///    allocation only decides rows that expressed no honourable preference.
///    A pin the gate refuses for this execution (e.g. `tasks.driver =
///    codex` on `ConflictResolution`) is dropped — not recorded as
///    `explicit` — so the row can still dispatch on an eligible driver
///    instead of stalling at the spawn-time gate. Respecting a pin that
///    *does* clear the gate is also what makes the shipped default split
///    byte-for-byte behaviour-preserving for ordinary kinds.
/// 4. Otherwise, place the work item's own id on the split's bucket line,
///    renormalised over exactly the drivers eligible for the row's
///    [`TaskKind`] ([`eligible_drivers_for`]).
/// 5. Not a `tasks` row at all — no override.
///
/// Errors (failing the execution insert) when the row's kind is
/// unrecognised, or when the eligible drivers hold no share between them.
/// Both are loud on purpose: the alternative is dispatching to the engine
/// default driver, which for an eligibility failure means handing the row to
/// a driver whose gate refuses it, and for a zero-share failure means
/// overruling an operator who set that driver to 0. Zero means zero.
pub fn decide_execution_driver(conn: &Connection, work_item_id: &str, kind: ExecutionKind) -> Result<DriverDecision> {
    let row = conn
        .query_row(
            "SELECT t.driver, t.model_override, t.kind, t.reasoning, p.default_driver
               FROM tasks t
               LEFT JOIN products p ON p.id = t.product_id
              WHERE t.id = ?1",
            params![work_item_id],
            |row| {
                Ok(AllocationRow {
                    explicit_driver: row.get(0)?,
                    model_override: row.get(1)?,
                    task_kind: row.get(2)?,
                    reasoning: row.get(3)?,
                    product_default_driver: row.get(4)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else {
        // Not a `tasks` row at all (e.g. an `answer_agent` execution bound
        // to a comment id) — nothing to route.
        return Ok(DriverDecision::default_no_override());
    };
    let task_kind_raw = row.task_kind;
    let task_kind: Result<TaskKind, _> = task_kind_raw.parse();
    let reasoning: Option<ReasoningMode> = row
        .reasoning
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::parse)
        .transpose()
        .map_err(|err| {
            anyhow::anyhow!("driver_traffic_split: work item {work_item_id} has invalid reasoning: {err}")
        })?;
    let design_or_investigation_tier =
        boss_engine_effort::is_design_or_investigation_tier(task_kind.as_ref().ok(), reasoning);
    // Pool dispatch overrides row/product pins. Persist its fixed driver
    // rather than the pin, so this durable record always names the driver
    // the worker actually runs on. Routed through the same named accessors
    // `crate::driver_lookup`'s events-socket resolver uses, so there is
    // one resolution point per pool-bound shape rather than a re-derived
    // constant here.
    if let Some(pool_driver) = boss_engine_storage_values::pool_policy::pool_driver_slug_for_execution_kind(&kind) {
        return Ok(DriverDecision::pool(pool_driver));
    }
    let explicit_driver = row.explicit_driver.filter(|s| !s.trim().is_empty());
    let from_task_pin = explicit_driver.is_some();
    let pinned = explicit_driver.or_else(|| row.product_default_driver.filter(|s| !s.trim().is_empty()));
    if let Some(driver) = pinned {
        // Honour the pin only when it clears the capability gate for this
        // execution kind. A product default (or `--driver`) of codex/grok
        // must not stall ConflictResolution / CiRemediation — those kinds
        // require CommandOutcomeObservation, which only claude declares
        // today. Drop the pin and fall through to allocation among the
        // eligible set instead of recording an explicit decision the spawn
        // path would then hard-fail on.
        let pin_ok = match &task_kind {
            Ok(tk) => driver_clears_dispatch_gate(&driver, tk, &kind),
            // Unrecognised task kind is handled below as a hard error once
            // we reach allocation; keep the pin so the error surface does
            // not change for that data-integrity case.
            Err(_) => true,
        };
        // A tier row's pin also needs a dedicated design/investigation-tier
        // model (`ModelMenu::design_investigation_model`) or spawn hard-fails
        // with `IneligibleDesignInvestigationDriver` instead of ever running.
        // A `products.default_driver` is a default, not a row-level pin, so
        // it yields to allocation the same way a gate-failing pin does; a
        // `tasks.driver` pin is an explicit human decision and stays loud —
        // its failure at spawn is the intended signal, not silently routed
        // around.
        let tier_ok = from_task_pin || !design_or_investigation_tier || driver_has_design_investigation_model(&driver);
        if pin_ok && tier_ok {
            return Ok(DriverDecision::explicit(driver));
        }
        tracing::info!(
            work_item_id = %work_item_id,
            execution_kind = %kind,
            pinned_driver = %driver,
            "dropping driver pin that fails the capability gate or design/investigation tier model \
             requirement for this execution kind; allocating among eligible drivers instead",
        );
    }
    let task_kind: TaskKind = task_kind.map_err(|err| {
        anyhow::anyhow!(
            "driver_traffic_split: work item {work_item_id} has unrecognised kind {task_kind_raw:?} ({err}); \
             refusing to allocate a driver for a kind whose capability requirements this engine cannot resolve",
        )
    })?;
    let model_override = row
        .model_override
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let eligible = eligible_drivers_for(&task_kind, &kind, model_override, design_or_investigation_tier);
    if eligible.is_empty() && model_override.is_some() {
        return Ok(DriverDecision::default_no_override());
    }
    let split = load_driver_traffic_split_conn(conn)?;
    let driver = match allocate_among(split, work_item_id, &task_kind, &eligible) {
        Ok(driver) => driver,
        Err(_) if model_override.is_some() => return Ok(DriverDecision::default_no_override()),
        Err(_) if design_or_investigation_tier => return Ok(DriverDecision::default_no_override()),
        Err(err) => return Err(err),
    };
    Ok(DriverDecision::allocated(driver, split))
}

/// Place `work_item_id` on `split`'s bucket line restricted to `eligible`,
/// or fail loudly.
///
/// `hash_bucket` is in `[0, 100)`, and
/// [`DriverTrafficSplit::driver_for_bucket_among`] partitions that space
/// into half-open intervals sized by the shares the eligible drivers hold
/// *relative to each other* — so an eligible driver at 0 owns an empty
/// interval and is literally unreachable, an ineligible one is not on the
/// line at all, and there is no rounding gap or off-by-one leakage between
/// them.
///
/// Split out from [`decide_execution_driver`] so the failure paths (nothing
/// eligible; everything eligible at 0) are exercisable with a restricted
/// eligible set. The dedicated design/investigation tier is one real producer:
/// it excludes Grok even though Grok clears the capability gate.
fn allocate_among(
    split: DriverTrafficSplit,
    work_item_id: &str,
    kind: &TaskKind,
    eligible: &[&'static str],
) -> Result<&'static str> {
    split
        .driver_for_bucket_among(hash_bucket(work_item_id), eligible)
        .map_err(|err| {
            tracing::error!(
                work_item_id,
                %kind,
                ?eligible,
                ?split,
                %err,
                "driver_traffic_split: cannot allocate a driver for this work item",
            );
            anyhow::anyhow!(
                "driver_traffic_split: cannot allocate a driver for work item {work_item_id} (kind {kind}): {err}"
            )
        })
}

/// Persist `decision` for `execution_id`. Called once, right after the
/// `work_executions` row is inserted, in the same transaction — this is
/// the durable, queryable record the task requires ("how much went to each
/// driver", "did one driver's rows fail more"), not just a log line.
pub fn record_execution_driver_decision(
    conn: &Connection,
    execution_id: &str,
    work_item_id: &str,
    decision: &DriverDecision,
    now: &str,
) -> Result<()> {
    let split_json = decision
        .split_at_decision
        .map(|split| serde_json::to_string(&split))
        .transpose()?;
    conn.execute(
        "INSERT INTO execution_driver_decisions
            (execution_id, work_item_id, driver, reason, split_at_decision, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(execution_id) DO UPDATE SET
            work_item_id = excluded.work_item_id,
            driver = excluded.driver,
            reason = excluded.reason,
            split_at_decision = excluded.split_at_decision,
            created_at = excluded.created_at",
        params![
            execution_id,
            work_item_id,
            decision.driver,
            decision.reason,
            split_json,
            now,
        ],
    )?;
    Ok(())
}

pub(super) mod sol_selection {
    //! Durable automatic model decisions; independent of mutable task classification.
    use crate::*;
    use boss_engine_effort::sol::{DEFAULT_SOL_PERCENTAGE, SOL_MODEL, use_sol};

    /// Statuses that mean the worker itself failed. Cancellation, supersession
    /// and post-crash orphan recovery are operator or engine events, so they
    /// never push work off Sol.
    const FAILURE_STATUSES: &str = "'failed','abandoned'";

    /// Sol became the standard model for eligible work (default share 100%).
    /// The share row is only written by an explicit set, but a stored 50 cannot
    /// be told apart from the former 50% default being stepped away from and
    /// back, so a stored 50 moves to 100. Any other stored value was
    /// deliberately chosen and is kept.
    pub fn migrate_default_full_share(conn: &Connection) -> Result<()> {
        conn.execute(
            "UPDATE metadata SET value='100' WHERE key='codex_sol_percentage' AND value='50'",
            [],
        )?;
        Ok(())
    }

    pub fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS execution_sol_decisions (
            execution_id TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE,
            eligible INTEGER NOT NULL, bucket INTEGER NOT NULL,
            percentage INTEGER NOT NULL, escalated INTEGER NOT NULL,
            arm TEXT NOT NULL, selected_model TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sol_pr_outcomes (
            work_item_id TEXT PRIMARY KEY REFERENCES tasks(id),
            baseline_head TEXT, first_head TEXT, first_ci TEXT, merged_at TEXT
         );
         CREATE TABLE IF NOT EXISTS sol_failed_work_items (work_item_id TEXT PRIMARY KEY);
         CREATE TRIGGER IF NOT EXISTS sol_failure_execution AFTER UPDATE OF status,model,run_undeclared_at,run_done_outcome ON work_executions
         WHEN NEW.model='{SOL_MODEL}' AND (NEW.status IN ({FAILURE_STATUSES})
             OR NEW.run_undeclared_at IS NOT NULL OR NEW.run_done_outcome='blocked')
         BEGIN INSERT OR IGNORE INTO sol_failed_work_items VALUES (NEW.work_item_id); END;
         CREATE TRIGGER IF NOT EXISTS sol_failure_run AFTER UPDATE OF status ON work_runs
         WHEN NEW.status IN ({FAILURE_STATUSES})
         BEGIN INSERT OR IGNORE INTO sol_failed_work_items
             SELECT work_item_id FROM work_executions WHERE id=NEW.execution_id
             AND (model='{SOL_MODEL}' OR NEW.model='{SOL_MODEL}'); END;"
        ))?;
        Ok(())
    }

    #[work_db_boundary::instrument]
    impl WorkDb {
        pub fn get_codex_sol_percentage(&self) -> Result<u8> {
            let conn = self.connect()?;
            percentage(&conn)
        }

        pub fn set_codex_sol_percentage(&self, value: u8) -> Result<u8> {
            anyhow::ensure!(value <= 100, "Codex Sol percentage must be between 0 and 100");
            self.connect()?.execute(
                "INSERT INTO metadata(key,value) VALUES ('codex_sol_percentage',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [value.to_string()],
            )?;
            Ok(value)
        }

        /// Idempotent across compose retries and restarts. The draw is namespaced
        /// separately from the work-item driver split and stored with its threshold.
        ///
        /// A stored decision is only reused where it still describes this
        /// launch: an `ineligible` row is redrawn once the work becomes
        /// eligible, and a Sol draw for an execution that never launched is
        /// redrawn so a lowered share (or a failed preflight followed by a 0%
        /// setting) takes effect on pre-start retries.
        pub fn select_codex_sol_model(
            &self,
            execution_id: &str,
            eligible: bool,
            current_model: &str,
        ) -> Result<String> {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
            let percentage = percentage(&tx)?;
            // A failed run also counts: some recovery paths reuse an execution row.
            // A durable prior failure never clears when a later Astra retry succeeds.
            let escalated: bool = tx.query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM sol_failed_work_items WHERE work_item_id=?1) OR EXISTS (
                SELECT 1 FROM work_executions e
                WHERE e.work_item_id=?1 AND e.model=?2
                  AND (e.status IN ({FAILURE_STATUSES})
                       OR e.run_undeclared_at IS NOT NULL
                       OR e.run_done_outcome='blocked'
                       OR EXISTS (SELECT 1 FROM work_runs r WHERE r.execution_id=e.id
                           AND r.status IN ({FAILURE_STATUSES})))
            )"
                ),
                params![execution.work_item_id, SOL_MODEL],
                |row| row.get(0),
            )?;
            let stored = tx
                .query_row(
                    "SELECT arm, selected_model FROM execution_sol_decisions WHERE execution_id=?1",
                    [execution_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((arm, model)) = stored {
                let stale_ineligible = arm == "ineligible" && eligible;
                let unlaunched_sol = model == SOL_MODEL && execution.model.is_none();
                if !stale_ineligible && !unlaunched_sol {
                    // A stored model is meaningless once the work is ineligible
                    // (e.g. an override was added or cleared since).
                    if !eligible {
                        return Ok(current_model.to_owned());
                    }
                    if model == SOL_MODEL && escalated {
                        tx.execute(
                            "UPDATE execution_sol_decisions SET selected_model=?2,escalated=1 WHERE execution_id=?1",
                            params![execution_id, current_model],
                        )?;
                        tx.commit()?;
                        return Ok(current_model.to_owned());
                    }
                    return Ok(model);
                }
                tx.execute(
                    "DELETE FROM execution_sol_decisions WHERE execution_id=?1",
                    [execution_id],
                )?;
            }
            let bucket = super::hash_bucket(&format!("codex-sol:{execution_id}"));
            let sol = use_sol(eligible, escalated, bucket, percentage);
            let selected = if sol { SOL_MODEL } else { current_model };
            let arm = if !eligible {
                "ineligible"
            } else if sol {
                "sol"
            } else {
                "astra"
            };
            tx.execute(
                "INSERT INTO execution_sol_decisions VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![execution_id, eligible, bucket, percentage, escalated, arm, selected],
            )?;
            if eligible {
                enroll(&tx, &execution.work_item_id)?;
            }
            tx.commit()?;
            Ok(selected.to_owned())
        }
    }

    /// Open the family's outcome row and remember the PR head already
    /// published, so the first head pushed afterwards (while the enrolled
    /// worker is still running, or later) is the one CI is judged on.
    fn enroll(conn: &Connection, work_item_id: &str) -> Result<()> {
        let root = crate::chain_helpers::chain_root(conn, work_item_id)?;
        let head: Option<String> = conn
            .query_row("SELECT pr_head_sha FROM tasks WHERE id=?1", [&root], |r| r.get(0))
            .optional()?
            .flatten();
        conn.execute(
            "INSERT INTO sol_pr_outcomes(work_item_id,baseline_head) VALUES (?1,?2)
             ON CONFLICT(work_item_id) DO UPDATE SET baseline_head=excluded.baseline_head
             WHERE first_head IS NULL",
            params![root, head],
        )?;
        Ok(())
    }

    /// Idempotently stamp the family's merge time; a no-op for families that
    /// never enrolled. Shared by the merge poller and the worker-completion
    /// path that finds its PR already merged.
    pub fn record_merge(conn: &Connection, work_item_id: &str, now: &str) -> Result<()> {
        let root = crate::chain_helpers::chain_root(conn, work_item_id)?;
        conn.execute(
            "UPDATE sol_pr_outcomes SET merged_at=COALESCE(merged_at,?2) WHERE work_item_id=?1",
            params![root, now],
        )?;
        Ok(())
    }

    fn percentage(conn: &Connection) -> Result<u8> {
        let raw: Option<String> = conn
            .query_row("SELECT value FROM metadata WHERE key='codex_sol_percentage'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let value = raw
            .map(|v| v.parse::<u8>())
            .transpose()?
            .unwrap_or(DEFAULT_SOL_PERCENTAGE);
        anyhow::ensure!(value <= 100, "persisted Codex Sol percentage must be between 0 and 100");
        Ok(value)
    }

    /// Record only the first head published after enrollment and that head's
    /// first terminal CI result, independent of whether the enrolled worker has
    /// finished. A later head or CI rerun cannot turn a failed first attempt
    /// into a success, and the head that existed at enrollment never counts.
    /// Families that never enrolled have no outcome row, so this is two cheap
    /// primary-key updates for them.
    pub fn observe_ci(conn: &Connection, item: &str, head: Option<&str>, state: &str) -> Result<()> {
        let Some(head) = head else { return Ok(()) };
        let root = crate::chain_helpers::chain_root(conn, item)?;
        conn.execute(
            "UPDATE sol_pr_outcomes
             SET first_head=COALESCE(first_head, CASE WHEN ?2 IS NOT baseline_head THEN ?2 END)
             WHERE work_item_id=?1 AND first_ci IS NULL",
            params![root, head],
        )?;
        conn.execute(
            "UPDATE sol_pr_outcomes SET first_ci=?3
             WHERE work_item_id=?1 AND first_ci IS NULL AND first_head=?2 AND ?3 IN ('success','fail')",
            params![root, head, state],
        )?;
        Ok(())
    }

    /// Reuse the indexed review-family walk, including tombstoned revisions.
    pub fn family_cte() -> String {
        format!(
            "{}, roots AS (SELECT DISTINCT id AS task_id,root AS cycle_root_id FROM family)",
            review_badge::review_family_cte("?1", review_badge::ReviewKey::Owner)
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::test_support::{create_product, create_ready_chore_execution, create_test_chore, open_db};
        use boss_engine_effort::sol::ASTRA_MODEL;

        #[test]
        fn default_share_migration_moves_stored_fifty_only() {
            for (stored, expected) in [("50", 100), ("35", 35), ("0", 0), ("100", 100)] {
                let (_dir, db) = open_db();
                db.connect()
                    .unwrap()
                    .execute(
                        "INSERT INTO metadata(key,value) VALUES ('codex_sol_percentage',?1)",
                        [stored],
                    )
                    .unwrap();
                migrate_default_full_share(&db.connect().unwrap()).unwrap();
                assert_eq!(db.get_codex_sol_percentage().unwrap(), expected, "stored {stored}");
            }
        }

        #[test]
        fn setting_and_draw_are_durable_and_zero_preserves_the_existing_model() {
            let (_dir, db) = open_db();
            assert_eq!(db.get_codex_sol_percentage().unwrap(), 100);
            assert!(db.set_codex_sol_percentage(101).is_err());
            let product = create_product(&db);
            let task = create_test_chore(&db, &product, "selection");
            let execution = create_ready_chore_execution(&db, &task.id);
            db.set_codex_sol_percentage(0).unwrap();
            assert_eq!(
                db.select_codex_sol_model(&execution.id, true, ASTRA_MODEL).unwrap(),
                ASTRA_MODEL
            );
            db.set_codex_sol_percentage(100).unwrap();
            assert_eq!(
                db.select_codex_sol_model(&execution.id, true, ASTRA_MODEL).unwrap(),
                ASTRA_MODEL
            );
            let conn = db.connect().unwrap();
            let (eligible, threshold, bucket, arm): (bool, u8, u8, String) = conn
                .query_row(
                    "SELECT eligible,percentage,bucket,arm FROM execution_sol_decisions WHERE execution_id=?1",
                    [&execution.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert!(eligible);
            assert_eq!(threshold, 0);
            assert_eq!(
                bucket,
                crate::driver_allocation::hash_bucket(&format!("codex-sol:{}", execution.id))
            );
            assert_eq!(arm, "astra");
            drop(conn);
            let other = create_test_chore(&db, &product, "other driver");
            let other_execution = create_ready_chore_execution(&db, &other.id);
            assert_eq!(
                db.select_codex_sol_model(&other_execution.id, false, "opus").unwrap(),
                "opus"
            );
        }

        #[test]
        fn worker_failure_states_escalate_permanently_for_the_same_item() {
            for status in ["failed", "abandoned"] {
                let (_dir, db) = open_db();
                let product = create_product(&db);
                let task = create_test_chore(&db, &product, status);
                db.set_codex_sol_percentage(100).unwrap();
                let first = create_ready_chore_execution(&db, &task.id);
                assert_eq!(
                    db.select_codex_sol_model(&first.id, true, ASTRA_MODEL).unwrap(),
                    SOL_MODEL
                );
                db.record_execution_launch_config(&first.id, "codex", SOL_MODEL, None)
                    .unwrap();
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE work_executions SET status=?2 WHERE id=?1",
                        params![first.id, status],
                    )
                    .unwrap();
                assert_eq!(
                    db.select_codex_sol_model(&first.id, true, ASTRA_MODEL).unwrap(),
                    ASTRA_MODEL
                );
                // Rearming the original row must not erase its failure history.
                db.connect()
                    .unwrap()
                    .execute("UPDATE work_executions SET status='completed' WHERE id=?1", [&first.id])
                    .unwrap();
                let second = create_ready_chore_execution(&db, &task.id);
                assert_eq!(
                    db.select_codex_sol_model(&second.id, true, ASTRA_MODEL).unwrap(),
                    ASTRA_MODEL,
                    "{status}"
                );
                db.record_execution_launch_config(&second.id, "codex", ASTRA_MODEL, None)
                    .unwrap();
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE work_executions SET status='completed' WHERE id=?1",
                        [&second.id],
                    )
                    .unwrap();
                let third = create_ready_chore_execution(&db, &task.id);
                assert_eq!(
                    db.select_codex_sol_model(&third.id, true, ASTRA_MODEL).unwrap(),
                    ASTRA_MODEL
                );
                let unrelated = create_test_chore(&db, &product, "unrelated");
                let other = create_ready_chore_execution(&db, &unrelated.id);
                assert_eq!(
                    db.select_codex_sol_model(&other.id, true, ASTRA_MODEL).unwrap(),
                    SOL_MODEL
                );
            }
        }

        #[test]
        fn operator_and_recovery_states_do_not_escalate() {
            for status in ["cancelled", "orphaned"] {
                let (_dir, db) = open_db();
                let task = create_test_chore(&db, create_product(&db), status);
                db.set_codex_sol_percentage(100).unwrap();
                let first = create_ready_chore_execution(&db, &task.id);
                db.select_codex_sol_model(&first.id, true, ASTRA_MODEL).unwrap();
                db.record_execution_launch_config(&first.id, "codex", SOL_MODEL, None)
                    .unwrap();
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE work_executions SET status=?2 WHERE id=?1",
                        params![first.id, status],
                    )
                    .unwrap();
                let second = create_ready_chore_execution(&db, &task.id);
                assert_eq!(
                    db.select_codex_sol_model(&second.id, true, ASTRA_MODEL).unwrap(),
                    SOL_MODEL,
                    "{status}"
                );
            }
        }

        #[test]
        fn stale_ineligible_decision_is_redrawn_not_reapplied() {
            let (_dir, db) = open_db();
            let task = create_test_chore(&db, create_product(&db), "override cleared");
            db.set_codex_sol_percentage(0).unwrap();
            let execution = create_ready_chore_execution(&db, &task.id);
            assert_eq!(
                db.select_codex_sol_model(&execution.id, false, "custom-override")
                    .unwrap(),
                "custom-override"
            );
            // The override is gone: the stored override model must not come back.
            assert_eq!(
                db.select_codex_sol_model(&execution.id, true, ASTRA_MODEL).unwrap(),
                ASTRA_MODEL
            );
            let arm: String = db
                .connect()
                .unwrap()
                .query_row(
                    "SELECT arm FROM execution_sol_decisions WHERE execution_id=?1",
                    [&execution.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(arm, "astra");
            // And an eligible row never leaks its model into a now-ineligible launch.
            assert_eq!(
                db.select_codex_sol_model(&execution.id, false, "custom-override")
                    .unwrap(),
                "custom-override"
            );
        }

        #[test]
        fn unlaunched_sol_draw_follows_a_lowered_share_but_a_launched_one_is_durable() {
            let (_dir, db) = open_db();
            let task = create_test_chore(&db, create_product(&db), "preflight failed");
            db.set_codex_sol_percentage(100).unwrap();
            let execution = create_ready_chore_execution(&db, &task.id);
            assert_eq!(
                db.select_codex_sol_model(&execution.id, true, ASTRA_MODEL).unwrap(),
                SOL_MODEL
            );
            // The launch failed before a model was recorded; the operator sets 0%.
            db.set_codex_sol_percentage(0).unwrap();
            assert_eq!(
                db.select_codex_sol_model(&execution.id, true, ASTRA_MODEL).unwrap(),
                ASTRA_MODEL
            );

            let launched = create_ready_chore_execution(&db, &task.id);
            db.set_codex_sol_percentage(100).unwrap();
            assert_eq!(
                db.select_codex_sol_model(&launched.id, true, ASTRA_MODEL).unwrap(),
                SOL_MODEL
            );
            db.record_execution_launch_config(&launched.id, "codex", SOL_MODEL, None)
                .unwrap();
            db.set_codex_sol_percentage(0).unwrap();
            assert_eq!(
                db.select_codex_sol_model(&launched.id, true, ASTRA_MODEL).unwrap(),
                SOL_MODEL
            );
        }

        fn set_head_and_state(db: &WorkDb, task: &str, head: &str) {
            db.connect()
                .unwrap()
                .execute("UPDATE tasks SET pr_head_sha=?2 WHERE id=?1", params![task, head])
                .unwrap();
        }

        fn first_ci(db: &WorkDb, task: &str) -> Option<String> {
            db.connect()
                .unwrap()
                .query_row(
                    "SELECT first_ci FROM sol_pr_outcomes WHERE work_item_id=?1",
                    [task],
                    |r| r.get(0),
                )
                .unwrap()
        }

        #[test]
        fn first_ci_failure_is_kept_while_the_worker_is_live_and_after_reruns() {
            let (_dir, db) = open_db();
            let task = create_test_chore(&db, create_product(&db), "live ci");
            db.set_codex_sol_percentage(100).unwrap();
            set_head_and_state(&db, &task.id, "before");
            let execution = create_ready_chore_execution(&db, &task.id);
            db.select_codex_sol_model(&execution.id, true, ASTRA_MODEL).unwrap();
            db.record_execution_launch_config(&execution.id, "codex", SOL_MODEL, None)
                .unwrap();
            let observe = |head: &str, state: &str| {
                let conn = db.connect().unwrap();
                observe_ci(&conn, &task.id, Some(head), state).unwrap();
            };
            // The execution is still running (no pr_head_after yet); the head
            // that predates enrollment never counts.
            observe("before", "success");
            assert_eq!(first_ci(&db, &task.id), None);
            observe("first", "in_progress");
            observe("first", "fail");
            // A rerun on the same head, or a fix on a new head, cannot rewrite it.
            observe("first", "success");
            observe("fix", "success");
            assert_eq!(first_ci(&db, &task.id).as_deref(), Some("fail"));
        }

        #[test]
        fn families_that_never_enrolled_record_nothing() {
            let (_dir, db) = open_db();
            let task = create_test_chore(&db, create_product(&db), "ineligible");
            let execution = create_ready_chore_execution(&db, &task.id);
            db.select_codex_sol_model(&execution.id, false, ASTRA_MODEL).unwrap();
            let conn = db.connect().unwrap();
            observe_ci(&conn, &task.id, Some("head"), "fail").unwrap();
            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM sol_pr_outcomes", [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 0);
        }

        #[test]
        fn undeclared_exit_escalates_but_a_successful_sol_run_does_not() {
            for undeclared in [false, true] {
                let (_dir, db) = open_db();
                let task = create_test_chore(&db, create_product(&db), "exit");
                db.set_codex_sol_percentage(100).unwrap();
                let first = create_ready_chore_execution(&db, &task.id);
                db.select_codex_sol_model(&first.id, true, ASTRA_MODEL).unwrap();
                db.record_execution_launch_config(&first.id, "codex", SOL_MODEL, None)
                    .unwrap();
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE work_executions SET status='completed',run_undeclared_at=?2 WHERE id=?1",
                        params![first.id, undeclared.then_some("1")],
                    )
                    .unwrap();
                let second = create_ready_chore_execution(&db, &task.id);
                assert_eq!(
                    db.select_codex_sol_model(&second.id, true, ASTRA_MODEL).unwrap(),
                    if undeclared { ASTRA_MODEL } else { SOL_MODEL }
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
