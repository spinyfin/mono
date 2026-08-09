//! Three-way driver traffic allocation (experiment, operator-controlled).
//!
//! One persisted [`DriverTrafficSplit`] — three shares summing to exactly
//! 100 — decides how work that dispatches on its own row's driver is divided
//! between the `grok`, `claude`, and `codex` drivers. This generalises the
//! earlier single Codex percentage; see [`boss_protocol::DriverTrafficSplit`]
//! for why the split is modelled as three explicit integers with a hard sum
//! constraint rather than an implied remainder or normalised weights.
//!
//! **Which work that is, is not decided here.** Allocation carries no list of
//! eligible kinds. For a given row it asks
//! [`crate::driver::DriverRegistry::eligible_drivers_for_kind`] — i.e. the
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
//! *which driver*.
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
//! decided before allocation ever runs (`pr_review`, `automation_triage`,
//! and any execution whose work item came from an automation): those
//! dispatch on
//! [`crate::coordinator::pool_dispatch_policy_for_worker_id`]'s fixed
//! driver, so `decide_execution_driver` records that pool driver with
//! reason `pool` up front, rather than an allocation or a row/product pin.
//! See [`decide_execution_driver`] for the full reasoning.
//!
//! One thing allocation itself still declines, and it is not a claim about
//! capability:
//!
//! - **A row with an explicit pin** — see below.
//!
//! Assignment is a deterministic hash of the work item's own id placed on
//! the split's `[0, 100)` bucket line — not a coin flip per dispatch
//! attempt — so the same row under the same split always lands on the same
//! driver across retries, redispatch, and recovery. Changing the split moves
//! the boundaries and therefore reallocates some rows that have not been
//! dispatched yet; that is deliberate, and it cannot make a row flip
//! *between attempts of one dispatch* because the decision is computed once,
//! at [`crate::work::revision_helpers::insert_execution`] time, and
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
//! dispatch paths ([`crate::work::driver_lookup`]'s events-socket resolver
//! and [`crate::runner::worker_spawn`]'s spawn-config resolver) consult, so
//! an allocated row is actually routed to its driver rather than the
//! recorded decision being an inert audit trail.

use boss_protocol::DriverTrafficSplit;
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

/// The superseded single-Codex-percentage key. Read only by
/// `migrations_b::migrate_driver_traffic_split_from_codex_percentage`, which
/// folds any persisted value into an equivalent split once and then removes
/// it, so this module has exactly one source of truth.
pub(crate) const METADATA_KEY_CODEX_DISPATCH_PERCENTAGE: &str = "codex_dispatch_percentage";

/// `execution_driver_decisions.reason`: the row (or its product) pinned a
/// driver; the split was not consulted.
pub(crate) const REASON_EXPLICIT: &str = "explicit";
/// `execution_driver_decisions.reason`: the row was placed on the split's
/// bucket line, renormalised over the drivers eligible for its kind.
/// Recorded whichever driver it landed on, so "allocated to claude" stays
/// distinguishable from "allocation never ran".
pub(crate) const REASON_ALLOCATION: &str = "allocation";
/// `execution_driver_decisions.reason`: the execution dispatches on a
/// pool's fixed driver, which overrides every row and product pin.
pub(crate) const REASON_POOL: &str = "pool";
/// `execution_driver_decisions.reason`: allocation did not decide this
/// execution — it is not bound to a `tasks` row. No override; the row
/// resolves through the ordinary precedence chain.
pub(crate) const REASON_DEFAULT: &str = "default";

/// The reason value written by the superseded single-Codex-percentage
/// scheme. Never written any more; still recognised on read so decisions
/// recorded before this landed keep their meaning instead of silently
/// degrading to `default`.
pub(crate) const REASON_LEGACY_PERCENTAGE: &str = "percentage";

/// The routing decision for one execution. `driver` is `None` only for
/// [`REASON_DEFAULT`] — an ineligible row, which falls through to the normal
/// row → product → engine default resolution. [`REASON_POOL`] names the
/// fixed driver the execution will actually run on. `split_at_decision` is
/// set for [`REASON_ALLOCATION`] so a later analysis can tell which split a
/// given row was placed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverDecision {
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
/// [`crate::driver::DriverRegistry`] `runner::worker_spawn` builds and asks
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
/// row — see [`crate::driver::KindRequirements`]'s doc for
/// `ConflictResolution` / `CiRemediation`, which is exactly why allocation
/// must decline those two kinds rather than treating every `tasks`-bound
/// execution kind as equally eligible.
fn eligible_drivers_for(
    kind: &TaskKind,
    execution_kind: &ExecutionKind,
    model_override: Option<&str>,
) -> Vec<&'static str> {
    let registry = crate::driver::DriverRegistry::default();
    registry
        .eligible_drivers_for_kind(kind, Some(execution_kind), &DriverTrafficSplit::DRIVERS_IN_BUCKET_ORDER)
        .into_iter()
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
pub(crate) fn load_driver_traffic_split_conn(conn: &Connection) -> Result<DriverTrafficSplit> {
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
    pub(crate) fn get_execution_driver_decision(&self, execution_id: &str) -> Result<Option<DriverDecision>> {
        let conn = self.connect()?;
        get_execution_driver_decision_conn(&conn, execution_id)
    }
}

/// `WorkDb::get_execution_driver_decision`, but reusing an already-open
/// `Connection` (see `load_driver_traffic_split_conn`'s doc for why this
/// split exists).
pub(crate) fn get_execution_driver_decision_conn(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<DriverDecision>> {
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
    source_automation_id: Option<String>,
    product_default_driver: Option<String>,
}

/// Whether `driver_slug` clears [`crate::driver::CapabilityResolver::check_dispatch`]
/// for `(task_kind, execution_kind)`.
///
/// Unregistered slugs return `true` so the existing `UnknownDriver` failure
/// path still owns them — this helper only yields pins for *known* drivers
/// that the gate refuses (e.g. codex/grok on `ConflictResolution`). Used by
/// allocation, spawn composition, and the events-socket driver lookup so a
/// pin the gate will refuse never outranks a runnable substitute.
pub(crate) fn driver_clears_dispatch_gate(
    driver_slug: &str,
    task_kind: &TaskKind,
    execution_kind: &ExecutionKind,
) -> bool {
    let registry = crate::driver::DriverRegistry::default();
    match registry.resolver(driver_slug) {
        Some(resolver) => resolver.check_dispatch(task_kind, Some(execution_kind)).is_ok(),
        None => true,
    }
}

/// Decide which driver governs a newly-created execution row and why.
/// Called once, from `insert_execution`, using the same open `Connection`
/// (never `WorkDb::connect()` — see `load_driver_traffic_split_conn`).
///
/// Precedence:
/// 1. If the execution dispatches on a pool with a fixed driver, record that
///    pool driver. It overrides all row/product pins, matching dispatch.
///    Pool-bound executions take precedence because recording a row pin
///    would name a driver the worker never ran on. Two shapes:
///    [`crate::coordinator::kind_always_dispatches_on_pool_driver`] covers
///    `pr_review` and `automation_triage`; `tasks.source_automation_id`
///    covers ordinary implementation work that came from an automation and
///    therefore runs on the automation pool
///    (`ClaudeCoordinator::execution_targets_automation_pool`). Both pools
///    pin `pool_dispatch_policy_for_worker_id`'s driver, which overrides the
///    row's. Their `pool` decision names the driver the worker actually ran
///    on, matching what the events-socket resolver
///    ([`crate::work::driver_lookup::WorkDb::get_execution_driver_slug`])
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
pub(crate) fn decide_execution_driver(
    conn: &Connection,
    work_item_id: &str,
    kind: ExecutionKind,
) -> Result<DriverDecision> {
    let row = conn
        .query_row(
            "SELECT t.driver, t.model_override, t.kind, t.source_automation_id, p.default_driver
               FROM tasks t
               LEFT JOIN products p ON p.id = t.product_id
              WHERE t.id = ?1",
            params![work_item_id],
            |row| {
                Ok(AllocationRow {
                    explicit_driver: row.get(0)?,
                    model_override: row.get(1)?,
                    task_kind: row.get(2)?,
                    source_automation_id: row.get(3)?,
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
    let automation_sourced = row.source_automation_id.is_some_and(|id| !id.trim().is_empty());
    // Pool dispatch overrides row/product pins. Persist its fixed driver
    // rather than the pin, so this durable record always names the driver
    // the worker actually runs on. Routed through the same named accessors
    // `crate::work::driver_lookup`'s events-socket resolver uses, so there is
    // one resolution point per pool-bound shape rather than a re-derived
    // constant here.
    if let Some(pool_driver) = crate::coordinator::pool_driver_slug_for_execution_kind(&kind) {
        return Ok(DriverDecision::pool(pool_driver));
    }
    if automation_sourced {
        return Ok(DriverDecision::pool(crate::coordinator::automation_pool_driver_slug()));
    }
    let pinned = row
        .explicit_driver
        .filter(|s| !s.trim().is_empty())
        .or_else(|| row.product_default_driver.filter(|s| !s.trim().is_empty()));
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
        if pin_ok {
            return Ok(DriverDecision::explicit(driver));
        }
        tracing::info!(
            work_item_id = %work_item_id,
            execution_kind = %kind,
            pinned_driver = %driver,
            "dropping driver pin that fails the capability gate for this execution kind; \
             allocating among eligible drivers instead",
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
    let eligible = eligible_drivers_for(&task_kind, &kind, model_override);
    if eligible.is_empty() && model_override.is_some() {
        return Ok(DriverDecision::default_no_override());
    }
    let split = load_driver_traffic_split_conn(conn)?;
    let driver = match allocate_among(split, work_item_id, &task_kind, &eligible) {
        Ok(driver) => driver,
        Err(_) if model_override.is_some() => return Ok(DriverDecision::default_no_override()),
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
/// eligible set, which no real work item can currently produce — all three
/// built-in drivers clear every kind's gate today.
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
pub(crate) fn record_execution_driver_decision(
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

#[cfg(test)]
mod tests;
