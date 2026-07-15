//! The single choke point for every Boothby mutation.
//!
//! Boothby is an LLM with coordinator privileges acting unattended. The
//! safety argument for that cannot rest on its prompt, because a prompt is
//! advice: it can be misread, argued out of, or simply lost in a long
//! context. So every limit that matters is enforced here, in code Boothby
//! calls but cannot route around — the catalogue is closed, the caps are
//! counted from the journal, the guards re-read the row at the moment of
//! mutation, and an irreversible verb has to be asked for twice.
//!
//! Design: `tools/boss/docs/designs/boothby.md` §"Chosen approach" (the
//! guarded action executor) and §"Safety rails".
//!
//! ## The order of the rails, and why it is this order
//!
//! [`BoothbyExecutor::act`] runs its checks cheapest-and-most-final first:
//!
//! 1. **Catalogue lookup** — an unknown verb never reaches anything else.
//! 2. **Fingerprint refusal** — a human already vetoed this; no amount of
//!    budget or confirmation makes it OK, so it is settled before either is
//!    consulted.
//! 3. **Autonomy / mode** — needs an operator, so nothing below matters.
//! 4. **Caps** — the budget is spent; stop before touching the database.
//! 5. **Guards** — the expensive part (queries per target), and the only
//!    check that can change between two identical calls seconds apart.
//! 6. **Two-pass confirmation** — last, because nominating a target is a
//!    *side effect* on the gate. Running it earlier would let a request that
//!    a guard was about to refuse still bank a nomination, and the same
//!    request next pass would then find itself "confirmed" on the strength
//!    of a pass where it never actually cleared. See
//!    `a_guard_refusal_does_not_bank_a_two_pass_nomination`.
//!
//! ## What this module deliberately does not do
//!
//! It does not *implement* the verbs. The catalogue's effects are tasks 8
//! (taxonomy) and 9 (operational) of the design's breakdown; they arrive as
//! [`VerbHandler`] implementations registered on the executor. Until they
//! land the registry is empty and every call refuses with
//! `no handler is registered`, which is why this ships dark — same as task
//! 1's migration.
//!
//! It also does not own the undo engine, the pass lifecycle, the settings
//! keys, or the conversion of a proposal into an attention group. Those are
//! separate entries in the breakdown; [`BoothbyPolicy`] is the seam the
//! scheduler (task 3) hands its settings through, and
//! [`BoothbyActOutcome::Proposed`] is the seam the proposal flow (task 10)
//! picks up.

use std::collections::HashMap;

use anyhow::{Context, Result};
use boss_protocol::{BoothbyActInput, BoothbyActOutcome, LAST_STATUS_ACTOR_BOOTHBY};

use super::catalogue::{self, Autonomy, JournalMode, Reversibility, VerbSpec};
use super::guards::{self, Confirmation, GuardVerdict, PassSpend, TwoPassGate};
use crate::work::{BoothbyActionContext, WorkDb};

/// The global per-pass ceiling on mutations, whatever the per-verb budgets
/// allow. `boothby.max_actions_per_pass` in the design; default 15.
///
/// A backstop against the per-verb caps summing to something nobody
/// intended: the catalogue's individual budgets total far more than 15, and
/// this is the number that says what a single unattended pass may do to an
/// install overall.
pub const DEFAULT_MAX_ACTIONS_PER_PASS: u32 = 15;

/// How much autonomy the install grants Boothby. `boothby.mode` in the
/// design.
///
/// Ships defaulting to [`BoothbyMode::Propose`] — see the `Default` impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoothbyMode {
    /// Boothby is disabled; the executor refuses everything. This is the
    /// kill switch, and it works even mid-pass because it is checked per
    /// call rather than at spawn.
    Off,
    /// Nothing mutates. Every intended action becomes a proposal for an
    /// operator to approve. The shipping default: an install's first
    /// experience of Boothby should be a list of things it *would* have
    /// done, not a list of things it did.
    #[default]
    Propose,
    /// Boothby may act unattended — but only on the verbs the catalogue
    /// marks [`Autonomy::Auto`]. `auto` cannot promote a `propose` verb;
    /// the mode loosens nothing the catalogue closed.
    Auto,
}

/// The knobs the executor reads, injected rather than read from settings.
///
/// The design keys these off `boothby.mode` / `boothby.max_actions_per_pass`
/// / `boothby.human_touch_cooldown` in the settings registry, but the
/// registry is boolean-valued today and the Boothby keys are an enum and two
/// integers. Widening it is the scheduler's entry (task 3) in the breakdown,
/// so this struct is the seam: the executor takes its policy as a value and
/// stays testable at every setting, and task 3 populates it from the
/// registry without touching this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoothbyPolicy {
    pub mode: BoothbyMode,
    /// Global per-pass mutation ceiling.
    pub max_actions_per_pass: u32,
    /// Hours after a human touch during which a row is off limits.
    pub human_touch_cooldown_hours: i64,
}

impl Default for BoothbyPolicy {
    fn default() -> Self {
        Self {
            mode: BoothbyMode::default(),
            max_actions_per_pass: DEFAULT_MAX_ACTIONS_PER_PASS,
            human_touch_cooldown_hours: guards::DEFAULT_HUMAN_TOUCH_COOLDOWN_HOURS,
        }
    }
}

/// What a catalogue verb actually does.
///
/// Implemented by the verb sets in tasks 8 and 9. A handler is called *only*
/// after every rail has cleared and — for a [`JournalMode::WorkDbCapture`]
/// verb — with the journal context already armed, so an implementation's
/// whole job is the effect itself: no cap checking, no guard checking, no
/// journalling.
///
/// The `Result` is the effect's own failure (the row vanished, cube said no).
/// A *refusal* is not an error and never comes from here — by the time a
/// handler runs, nothing is left to refuse.
pub trait VerbHandler: Send + Sync {
    /// Perform the effect.
    ///
    /// Returns the `(pre_image, post_image)` JSON pair for a
    /// [`JournalMode::ExecutorWritten`] verb, or `None` when the verb has no
    /// restorable image (every I-class verb, which journals `params` and
    /// evidence instead). A [`JournalMode::WorkDbCapture`] verb returns
    /// `None`: its images come from the column diff the mutation layer takes
    /// inside the write's own transaction.
    fn apply(&self, db: &WorkDb, input: &BoothbyActInput) -> Result<Option<(String, String)>>;
}

/// The guarded action executor.
///
/// One instance per engine, holding the handler registry and the two-pass
/// gate (whose state must outlive any single pass, which is exactly why the
/// gate lives here and not on the pass).
pub struct BoothbyExecutor {
    policy: BoothbyPolicy,
    handlers: HashMap<&'static str, Box<dyn VerbHandler>>,
    gate: TwoPassGate,
    /// Serializes [`BoothbyExecutor::act`]. See the comment at the top of
    /// that method for what breaks without it.
    act_lock: std::sync::Mutex<()>,
}

impl BoothbyExecutor {
    pub fn new(policy: BoothbyPolicy) -> Self {
        Self {
            policy,
            handlers: HashMap::new(),
            gate: TwoPassGate::new(),
            act_lock: std::sync::Mutex::new(()),
        }
    }

    /// Register the handler for a catalogue verb.
    ///
    /// Panics if `slug` is not in the catalogue, or if it already has a
    /// handler. Both are wiring mistakes made at startup with a literal
    /// slug, so failing loudly at boot beats a verb that silently refuses
    /// forever in production because of a typo.
    pub fn register(&mut self, slug: &'static str, handler: Box<dyn VerbHandler>) {
        assert!(
            catalogue::lookup(slug).is_some(),
            "{slug} is not a Boothby catalogue verb",
        );
        assert!(
            self.handlers.insert(slug, handler).is_none(),
            "{slug} already has a registered handler",
        );
    }

    pub fn policy(&self) -> &BoothbyPolicy {
        &self.policy
    }

    /// Run one catalogue verb against one target, or explain why not.
    ///
    /// `pass_id` is the open pass the action belongs to; the caller resolves
    /// it (the scheduler owns the pass lifecycle). It is used for two-pass
    /// bookkeeping — the journal resolves the owning pass itself, from the
    /// database, in the write's transaction.
    ///
    /// An `Err` here means the machinery broke (a query failed, an effect
    /// blew up). Every *refusal* is an `Ok` carrying the reason, because a
    /// refusal is a normal answer that Boothby must be able to tell apart
    /// from a transient failure worth retrying.
    pub fn act(&self, db: &WorkDb, pass_id: &str, input: &BoothbyActInput) -> Result<BoothbyActOutcome> {
        // Serialize the whole call. Two rails are read-then-act sequences that
        // are only sound if nothing interleaves: the caps read the journal and
        // write to it later (concurrent calls would each see budget remaining
        // and jointly overshoot), and `execute` arms a *process-wide* action
        // context that the mutation layer reads back on its way past — so an
        // interleaved second call would re-arm the slot and the first call's
        // mutation would be journalled under the second's verb and rationale.
        //
        // Contention is not a concern: at most one pass runs at a time, and
        // its one agent issues a handful of calls per half hour.
        let _serialized = self.act_lock.lock().expect("boothby executor lock poisoned");

        // 1. The catalogue is closed: an unknown verb is not a thing Boothby
        //    may attempt, however well it argues for it.
        let Some(spec) = catalogue::lookup(&input.verb) else {
            return Ok(refused(format!(
                "{} is not a verb in the Boothby catalogue; the v1 catalogue is fixed",
                input.verb,
            )));
        };

        if input.rationale.trim().is_empty() {
            return Ok(refused(format!(
                "{} needs a rationale: an unexplained autonomous mutation is what the journal exists to prevent",
                input.verb,
            )));
        }

        if self.policy.mode == BoothbyMode::Off {
            return Ok(refused("Boothby is disabled (boothby.mode = off)".to_owned()));
        }

        // 2. Every action belongs to a pass, and this rail is what makes the
        //    caps mean anything: `boothby_pass_spend` reports an empty tally
        //    when no pass is open, so without this check every blast-radius
        //    budget would read as fully unspent, the caps rail would pass
        //    unconditionally, and an irreversible effect would run before the
        //    journal write failed on the very same missing pass. It lives here
        //    rather than only in the RPC handler because the scheduler calls
        //    `act` directly — a rail only the socket enforces is not a rail.
        let open_pass = db.open_boothby_pass_id()?;
        if open_pass.as_deref() != Some(pass_id) {
            return Ok(refused(match open_pass {
                Some(open) => format!(
                    "{pass_id} is not the open Boothby pass ({open} is); an action belongs to the \
                     pass that is actually running"
                ),
                None => format!("no Boothby pass is open, so {pass_id} cannot be acted on"),
            }));
        }

        // 3. Human veto is permanent and beats everything below it.
        if let Some(reason) = self.fingerprint_refusal(db, spec, &input.target_id)? {
            return Ok(refused(reason));
        }

        // 4. Needs an operator? Then nothing below is worth computing.
        if let Some(reason) = self.proposal_reason(spec) {
            return Ok(BoothbyActOutcome::Proposed { reason });
        }

        // Tell the gate which pass we are in on EVERY call, not just the
        // irreversible ones. The gate advances its generation when the pass
        // changes, so if only I-class requests rolled it, a pass that did
        // nothing but taxonomy work would be invisible to it — and a
        // nomination from an arbitrarily old pass would still be sitting in
        // the current generation, ready to confirm the next time anyone asked.
        // See `a_nomination_survives_passes_that_did_other_work`.
        self.gate.observe_pass(pass_id);

        // 5. Blast-radius caps, counted from the journal.
        if let Some(reason) = self.cap_refusal(db, spec)? {
            return Ok(BoothbyActOutcome::Capped { reason });
        }

        // 6. Guards: the only check whose answer can change between two
        //    identical calls a second apart.
        if let GuardVerdict::Refused(reason) = self.guard_target(db, spec, &input.target_id)? {
            return Ok(refused(reason));
        }

        // 7. Two-pass confirmation, last: nominating mutates the gate, and a
        //    request the guards would have refused must not bank one.
        if spec.reversibility.needs_two_pass_confirmation()
            && self.gate.nominate(pass_id, spec.slug, &input.target_id) == Confirmation::Deferred
        {
            return Ok(BoothbyActOutcome::Deferred {
                reason: format!(
                    "{} is irreversible; noted {} for confirmation and will act if the next pass \
                     reaches the same conclusion",
                    spec.slug, input.target_id,
                ),
            });
        }

        self.execute(db, spec, input)
    }

    /// Apply the effect and make sure it is journalled.
    ///
    /// The two journal modes differ in who writes the row, not in whether one
    /// is written: a `WorkDbCapture` verb has its row appended by the
    /// mutation layer inside the write's transaction (arming the context is
    /// what tells that layer the verb and rationale), while an
    /// `ExecutorWritten` verb is journalled here, after its effect returns.
    fn execute(&self, db: &WorkDb, spec: &VerbSpec, input: &BoothbyActInput) -> Result<BoothbyActOutcome> {
        let handler = match self.handlers.get(spec.slug) {
            Some(handler) => handler,
            // Ships dark: the catalogue is complete but its effects arrive in
            // tasks 8 and 9. Refusing (rather than erroring) keeps this on
            // the same footing as every other "no" Boothby can get.
            None => {
                return Ok(refused(format!(
                    "{} has no handler registered in this engine build",
                    spec.slug,
                )));
            }
        };

        let context = BoothbyActionContext::builder()
            .verb(spec.slug)
            .rationale(input.rationale.clone())
            .reversibility(spec.reversibility.as_str())
            .maybe_params(input.params.clone())
            .build();
        let _armed = db.arm_boothby_action(context)?;

        // Watermark taken before the effect: the capture layer's row is
        // whatever lands past it, which is how this call's action is told
        // apart from an identical earlier one in the same pass.
        let high_seq = db.boothby_pass_high_seq()?;

        let images = handler
            .apply(db, input)
            .with_context(|| format!("boothby: {} on {} failed", spec.slug, input.target_id))?;

        match spec.journal {
            JournalMode::WorkDbCapture => {
                // The mutation layer already appended the row from the column
                // delta, in the same transaction as the write. Re-journalling
                // here would double-count it against the caps.
                let action_id = db.boothby_action_after_seq(high_seq)?.with_context(|| {
                    format!(
                        "boothby: {} on {} reported success but journalled no action row; \
                         the mutation layer only journals a column delta, so the handler \
                         probably changed nothing",
                        spec.slug, input.target_id,
                    )
                })?;
                Ok(BoothbyActOutcome::Executed { action_id })
            }
            JournalMode::ExecutorWritten => {
                let (pre_image, post_image) = match (spec.reversibility, images) {
                    // I-class journals params + evidence, never a pre-image —
                    // there is nothing a restore could do with one.
                    (Reversibility::Irreversible, _) => (None, None),
                    (_, Some((pre, post))) => (Some(pre), Some(post)),
                    (_, None) => (None, None),
                };
                let action_id = db.record_boothby_effect(
                    spec.slug,
                    spec.target_kind,
                    &input.target_id,
                    pre_image.as_deref(),
                    post_image.as_deref(),
                )?;
                Ok(BoothbyActOutcome::Executed { action_id })
            }
        }
    }

    /// `Some(reason)` when a human has already vetoed this exact action.
    ///
    /// Two sources, one meaning. A prior `undone`/`conflicted` action row is
    /// a human having reversed this verb on this target. A suppressed
    /// fingerprint in the findings ledger is the same veto, made durable and
    /// operator-clearable. Either way Boothby does not get to re-decide.
    fn fingerprint_refusal(&self, db: &WorkDb, spec: &VerbSpec, target_id: &str) -> Result<Option<String>> {
        if db.boothby_action_was_reversed(spec.slug, spec.target_kind, target_id)? {
            return Ok(Some(format!(
                "a human already reversed {} on {target_id}; the veto is permanent until an \
                 operator clears the suppression",
                spec.slug,
            )));
        }
        let fingerprint = action_fingerprint(spec.slug, spec.target_kind, target_id);
        if db.boothby_fingerprint_is_suppressed(&fingerprint)? {
            return Ok(Some(format!("{fingerprint} is suppressed in the findings ledger")));
        }
        Ok(None)
    }

    /// `Some(reason)` when this verb needs an operator's approval first.
    fn proposal_reason(&self, spec: &VerbSpec) -> Option<String> {
        match (self.policy.mode, spec.autonomy) {
            (BoothbyMode::Propose, _) => Some(format!(
                "boothby.mode is propose; {} needs operator approval before it runs",
                spec.slug,
            )),
            // The catalogue outranks the mode: `auto` grants autonomy over the
            // verbs the design marked auto, and no others.
            (BoothbyMode::Auto, Autonomy::Propose) => Some(format!(
                "{} is propose-gated in the catalogue and stays so under boothby.mode = auto",
                spec.slug,
            )),
            (BoothbyMode::Auto, Autonomy::Auto) => None,
            // Handled before this is reached; folded in so the match stays
            // exhaustive rather than leaning on a catch-all that would
            // silently absorb a future mode.
            (BoothbyMode::Off, _) => Some("Boothby is disabled (boothby.mode = off)".to_owned()),
        }
    }

    /// `Some(reason)` when a blast-radius budget is spent for this pass.
    fn cap_refusal(&self, db: &WorkDb, spec: &VerbSpec) -> Result<Option<String>> {
        let spend = PassSpend::new(db.boothby_pass_spend()?.into_iter().collect());
        let total = spend.total();

        if total >= self.policy.max_actions_per_pass {
            return Ok(Some(format!(
                "this pass has spent its global budget ({total}/{} actions)",
                self.policy.max_actions_per_pass,
            )));
        }

        let group = spec.cap_group;
        let spent = spend.spent_in_group(group);
        if spent >= group.cap() {
            let sharers: Vec<_> = catalogue::verbs_in_group(group).collect();
            return Ok(Some(format!(
                "this pass has spent the {spent}/{} budget shared by {}",
                group.cap(),
                sharers.join(", "),
            )));
        }
        Ok(None)
    }

    /// Run the guards that apply to this verb's target.
    ///
    /// The three live-work rails are about *work items*: they ask whether
    /// something else is mid-flight on this row. A verb targeting an
    /// execution, a lease, a workspace or a file has no work item to
    /// interrogate — and, more to the point, those verbs exist precisely to
    /// clean up after work that is already broken, so a live-work guard
    /// would refuse exactly the cases they are for. Their protection is
    /// two-pass confirmation and (for the widest ones) propose-gating
    /// instead.
    fn guard_target(&self, db: &WorkDb, spec: &VerbSpec, target_id: &str) -> Result<GuardVerdict> {
        // Only `WorkDbCapture` verbs are gated here, and the journal mode is
        // the right discriminator rather than a coincidence: that mode means
        // "the mutation layer will diff this row before and after", which is
        // possible only for a row that already exists — exactly the verbs
        // whose target something else could be holding.
        //
        // Keying on `target_kind` alone would be wrong in a way that silently
        // breaks a verb: `file_chore` is task-targeted but *creates* its
        // task, so its target cannot exist when the guards run. Reading it
        // would refuse every `file_chore` call before its handler ever ran,
        // and Boothby could never file a finding.
        if spec.journal != JournalMode::WorkDbCapture {
            return Ok(GuardVerdict::Clear);
        }
        let is_task = spec.target_kind == boss_protocol::BOOTHBY_TARGET_TASK;
        let is_project = spec.target_kind == boss_protocol::BOOTHBY_TARGET_PROJECT;
        if !is_task && !is_project {
            return Ok(GuardVerdict::Clear);
        }

        // Live-work and lease are about executions, which hang off work items
        // rather than projects — a project has no execution of its own.
        if is_task {
            let verdict = guards::live_work_guard(db, target_id)?;
            if !verdict.is_clear() {
                return Ok(verdict);
            }
            let verdict = guards::lease_guard(db, target_id)?;
            if !verdict.is_clear() {
                return Ok(verdict);
            }
        }

        // The human-touch cooldown applies to both: the design scopes it to
        // any row carrying `updated_at` + `last_status_actor`, and archiving
        // a project a human was editing an hour ago is exactly as unwelcome
        // as closing their task.
        //
        // `get_work_item` errors rather than returning None for an unknown
        // id, and a target that has vanished mid-pass is an ordinary race
        // (a human deleted it), not a machinery fault — so it becomes a
        // refusal like any other guard verdict.
        let item = match db.get_work_item(target_id) {
            Ok(item) => item,
            Err(err) => {
                return Ok(GuardVerdict::Refused(format!("cannot read {target_id}: {err}")));
            }
        };
        let touch = match &item {
            crate::work::WorkItem::Task(task) | crate::work::WorkItem::Chore(task) if is_task => {
                (&task.last_status_actor, &task.updated_at)
            }
            crate::work::WorkItem::Project(project) if is_project => (&project.last_status_actor, &project.updated_at),
            // The id resolved to a different kind than the catalogue says the
            // verb targets — refuse rather than guess which guards apply.
            _ => {
                return Ok(GuardVerdict::Refused(format!(
                    "{} targets a {}, but {target_id} is not one",
                    spec.slug, spec.target_kind,
                )));
            }
        };
        Ok(guards::human_touch_guard(
            touch.0,
            touch.1,
            guards::now_epoch_secs(),
            self.policy.human_touch_cooldown_hours,
        ))
    }
}

/// The stable identity of "this verb, against this target".
///
/// `boothby_actions` has no fingerprint column, and deliberately needs none:
/// the triple `(verb, target_kind, target_id)` *is* the identity of an
/// action, it is already indexed by `boothby_actions_by_target`, and any
/// stored hash of the same three columns could only drift from them. This
/// composite is what the findings ledger's `fingerprint` holds for an
/// action-shaped suppression, so a human veto and a ledger entry meet on the
/// same key.
///
/// Readable rather than hashed on purpose: it appears in refusal messages
/// and in `boss boothby suppressions`, where `action:close_stale_task:task:T12`
/// tells an operator what they are clearing and a hex digest does not.
pub fn action_fingerprint(verb: &str, target_kind: &str, target_id: &str) -> String {
    format!("action:{verb}:{target_kind}:{target_id}")
}

/// Actor string for every Boothby mutation. Re-exported so the verb sets in
/// tasks 8/9 attribute their writes without importing the protocol directly.
pub const BOOTHBY_ACTOR: &str = LAST_STATUS_ACTOR_BOOTHBY;

fn refused(reason: String) -> BoothbyActOutcome {
    BoothbyActOutcome::Refused { reason }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::test_support::{create_test_chore_manual, create_test_product_named, open_db};
    use crate::work::{ExecutionStatus, WorkItemPatch};

    /// A handler that records its calls instead of doing anything.
    ///
    /// The counter is shared with the test rather than owned by the executor,
    /// because the property a guard test actually cares about is *the effect
    /// never ran* — not merely that the reply said no. Those come apart in
    /// the failure mode that matters most: a rail that mutates first and
    /// reports a refusal afterwards would satisfy an outcome-only assertion
    /// while doing exactly the damage the rail exists to prevent.
    #[derive(Clone, Default)]
    struct SpyHandler {
        calls: Arc<AtomicU32>,
        images: Option<(String, String)>,
    }

    impl SpyHandler {
        fn with_images(pre: &str, post: &str) -> Self {
            Self {
                calls: Arc::new(AtomicU32::new(0)),
                images: Some((pre.to_owned(), post.to_owned())),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }

        /// The assertion every guard test wants: the effect did not happen.
        fn assert_never_ran(&self) {
            assert_eq!(self.calls(), 0, "a refused action must never reach its handler");
        }
    }

    impl VerbHandler for SpyHandler {
        fn apply(&self, _db: &WorkDb, _input: &BoothbyActInput) -> Result<Option<(String, String)>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.images.clone())
        }
    }

    /// A handler that performs the real archive a taxonomy verb would, so
    /// the `WorkDbCapture` journal path has an actual column delta to notice.
    struct ArchiveHandler;

    impl VerbHandler for ArchiveHandler {
        fn apply(&self, db: &WorkDb, input: &BoothbyActInput) -> Result<Option<(String, String)>> {
            db.update_work_item_as_actor(
                &input.target_id,
                WorkItemPatch {
                    status: Some("archived".to_owned()),
                    ..Default::default()
                },
                BOOTHBY_ACTOR,
            )?;
            Ok(None)
        }
    }

    /// Backdate a task's `updated_at` past the human-touch cooldown.
    ///
    /// Needed by nearly every test below, and that is itself the guard
    /// working: creating a chore stamps `updated_at = now` with
    /// `last_status_actor = human`, so a brand-new row is — correctly — one a
    /// human touched seconds ago. Boothby only ever sees rows that have sat
    /// untouched for days, which no test wants to simulate by waiting.
    fn age_past_human_cooldown(db: &WorkDb, task_id: &str) {
        let conn = db.connect().unwrap();
        let long_ago = (guards::now_epoch_secs() - 90 * 24 * 3600).to_string();
        let changed = conn
            .execute(
                "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
                rusqlite::params![task_id, long_ago],
            )
            .unwrap();
        assert_eq!(changed, 1, "expected to age exactly one task");
    }

    /// A chore Boothby could plausibly act on: aged past the human-touch
    /// cooldown, no execution.
    fn stale_chore(db: &WorkDb, product_id: &str, name: impl Into<String>) -> crate::work::Task {
        let chore = create_test_chore_manual(db, product_id, name);
        age_past_human_cooldown(db, &chore.id);
        chore
    }

    /// Finish the open pass and open `next` — what the scheduler does between
    /// passes, and what the executor's open-pass rail requires before an
    /// action can name a new one.
    fn roll_pass(db: &WorkDb, next: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE boothby_passes SET outcome = 'completed', finished_at = '1700000001'
             WHERE finished_at IS NULL",
            [],
        )
        .unwrap();
        drop(conn);
        open_pass(db, next);
    }

    /// Run an I-class verb through the two passes it requires, asserting the
    /// first defers, and return the second pass's outcome. Rolls the pass for
    /// real between the two, since an action may only name the open pass.
    fn confirm_across_two_passes(
        executor: &BoothbyExecutor,
        db: &WorkDb,
        verb: &str,
        target_id: &str,
    ) -> BoothbyActOutcome {
        let first = executor.act(db, "bp_1", &input(verb, target_id)).unwrap();
        assert!(
            matches!(first, BoothbyActOutcome::Deferred { .. }),
            "an I-class verb must defer on its first pass, got {first:?}",
        );
        roll_pass(db, "bp_2");
        executor.act(db, "bp_2", &input(verb, target_id)).unwrap()
    }

    /// Put `work_item_id`'s execution into a state that holds a cube lease
    /// but is *not* live by `ExecutionStatus::is_live` (which is only
    /// `running` / `waiting_human`).
    ///
    /// `waiting_review` is the real shape of this: the worker has finished
    /// and its PR is up, so the live-work guard lets the row through, but the
    /// workspace is still leased. That gap is exactly what the lease guard
    /// exists for — without it a `waiting_review` row looks idle.
    fn hold_lease_while_waiting_review(db: &WorkDb, work_item_id: &str, lease_id: &str) {
        let conn = db.connect().unwrap();
        let changed = conn
            .execute(
                "UPDATE work_executions SET status = 'waiting_review', cube_lease_id = ?2
                 WHERE work_item_id = ?1",
                rusqlite::params![work_item_id, lease_id],
            )
            .unwrap();
        assert_eq!(changed, 1, "expected exactly one execution to lease");
    }

    /// The project counterpart of [`ArchiveHandler`], so the `WorkDbCapture`
    /// path has a real column delta to journal.
    struct ArchiveProjectHandler;

    impl VerbHandler for ArchiveProjectHandler {
        fn apply(&self, db: &WorkDb, input: &BoothbyActInput) -> Result<Option<(String, String)>> {
            db.update_project(
                &input.target_id,
                WorkItemPatch {
                    status: Some("archived".to_owned()),
                    ..Default::default()
                },
                BOOTHBY_ACTOR,
            )?;
            Ok(None)
        }
    }

    fn open_pass(db: &WorkDb, id: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO boothby_passes (id, trigger, started_at) VALUES (?1, 'schedule', '1700000000')",
            rusqlite::params![id],
        )
        .unwrap();
    }

    fn input(verb: &str, target_id: &str) -> BoothbyActInput {
        BoothbyActInput {
            verb: verb.to_owned(),
            target_id: target_id.to_owned(),
            rationale: "no activity in 90 days and no PR".to_owned(),
            params: None,
        }
    }

    /// `auto` mode with generous budgets — the permissive baseline, so that a
    /// refusal in any test below is attributable to the rail under test and
    /// not to the mode or the caps.
    fn auto_policy() -> BoothbyPolicy {
        BoothbyPolicy {
            mode: BoothbyMode::Auto,
            ..BoothbyPolicy::default()
        }
    }

    fn executor_with(policy: BoothbyPolicy, slug: &'static str, handler: Box<dyn VerbHandler>) -> BoothbyExecutor {
        let mut executor = BoothbyExecutor::new(policy);
        executor.register(slug, handler);
        executor
    }

    fn reason_of(outcome: &BoothbyActOutcome) -> &str {
        match outcome {
            BoothbyActOutcome::Executed { .. } => panic!("expected a refusal, got Executed"),
            BoothbyActOutcome::Proposed { reason }
            | BoothbyActOutcome::Deferred { reason }
            | BoothbyActOutcome::Capped { reason }
            | BoothbyActOutcome::Refused { reason } => reason,
        }
    }

    // ── the closed catalogue ─────────────────────────────────────────────

    #[test]
    fn an_unknown_verb_is_refused() {
        let (_dir, db) = open_db();
        open_pass(&db, "bp_1");
        let executor = BoothbyExecutor::new(auto_policy());

        let outcome = executor
            .act(&db, "bp_1", &input("delete_the_database", "task_1"))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }));
        assert!(reason_of(&outcome).contains("not a verb in the Boothby catalogue"));
    }

    /// The journal's whole purpose is that no autonomous mutation is
    /// unexplained, so an empty rationale must not be able to buy one.
    #[test]
    fn an_empty_rationale_is_refused() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let mut req = input("close_stale_task", &chore.id);
        req.rationale = "   ".to_owned();
        let outcome = executor.act(&db, "bp_1", &req).unwrap();
        assert!(reason_of(&outcome).contains("rationale"));
        handler.assert_never_ran();
    }

    #[test]
    fn a_verb_with_no_registered_handler_refuses_rather_than_erroring() {
        // The shipping-dark state: the catalogue is complete, its effects are
        // not. That has to be an ordinary "no", not a machinery fault.
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let executor = BoothbyExecutor::new(auto_policy());

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(reason_of(&outcome).contains("no handler registered"));
    }

    /// Without this rail every cap silently reads as zero: `boothby_pass_spend`
    /// reports an empty tally when no pass is open, so the budget check passes
    /// unconditionally, the effect runs, and only *then* does the journal write
    /// fail — leaving an irreversible action with no audit row.
    #[test]
    fn acting_with_no_open_pass_is_refused_before_the_effect_runs() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        // Deliberately no open_pass().
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("no Boothby pass is open"));
        handler.assert_never_ran();
    }

    /// An action must belong to the pass that is actually running, not to
    /// whatever id the caller passed.
    #[test]
    fn acting_against_a_pass_that_is_not_the_open_one_is_refused() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_stale", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("not the open Boothby pass"));
        handler.assert_never_ran();
    }

    // ── mode and autonomy ────────────────────────────────────────────────

    #[test]
    fn off_mode_refuses_everything() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(
            BoothbyPolicy {
                mode: BoothbyMode::Off,
                ..BoothbyPolicy::default()
            },
            "close_stale_task",
            Box::new(handler.clone()),
        );

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }));
        assert!(reason_of(&outcome).contains("disabled"));
        handler.assert_never_ran();
    }

    /// The shipping default. An install's first experience of Boothby is a
    /// list of things it *would* have done.
    #[test]
    fn the_default_policy_proposes_rather_than_acting() {
        assert_eq!(BoothbyPolicy::default().mode, BoothbyMode::Propose);

        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(BoothbyPolicy::default(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Proposed { .. }));

        let conn = db.connect().unwrap();
        let journalled: i64 = conn
            .query_row("SELECT count(*) FROM boothby_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journalled, 0, "propose mode must journal no action");
        handler.assert_never_ran();
    }

    /// The mode can loosen nothing the catalogue closed: `auto` grants
    /// autonomy over the auto verbs and no others.
    #[test]
    fn auto_mode_does_not_promote_a_propose_gated_verb() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a drifted chore");
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "reconcile_pr_task_drift", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("reconcile_pr_task_drift", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Proposed { .. }));
        assert!(reason_of(&outcome).contains("propose-gated in the catalogue"));
        handler.assert_never_ran();
    }

    // ── the journal ──────────────────────────────────────────────────────

    /// The `WorkDbCapture` path end to end: the mutation layer journals the
    /// column delta in its own transaction, and the executor reports the row
    /// it wrote.
    #[test]
    fn a_taxonomy_verb_executes_and_is_journalled_once() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(ArchiveHandler));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        let BoothbyActOutcome::Executed { action_id } = outcome else {
            panic!("expected Executed, got {outcome:?}");
        };

        let conn = db.connect().unwrap();
        let (verb, target, rationale, reversibility, pre, post): (String, String, String, String, String, String) =
            conn.query_row(
                "SELECT verb, target_id, rationale, reversibility, pre_image, post_image
                 FROM boothby_actions WHERE id = ?1",
                [&action_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(verb, "close_stale_task");
        assert_eq!(target, chore.id);
        assert_eq!(rationale, "no activity in 90 days and no PR");
        assert_eq!(reversibility, "reversible");
        assert!(pre.contains(r#""status":"todo""#), "pre_image was {pre}");
        assert!(post.contains(r#""status":"archived""#), "post_image was {post}");

        // Exactly one row: the executor must not journal a second time on
        // top of the capture layer's, or every capped budget would be half
        // what the design says.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM boothby_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// The non-WorkDb path — the half the capture layer structurally cannot
    /// see, since redispatching work moves no column on the target row.
    ///
    /// `redispatch_stuck_work` rather than an I-class verb so the journal is
    /// the only thing under test: it is S-class, so it needs no two-pass
    /// confirmation, and it carries images (unlike the I-class verbs).
    #[test]
    fn an_operational_verb_is_journalled_by_the_executor() {
        let (_dir, db) = open_db();
        open_pass(&db, "bp_1");
        let handler = Box::new(SpyHandler::with_images(
            r#"{"status":"waiting_human"}"#,
            r#"{"status":"ready"}"#,
        ));
        let executor = executor_with(auto_policy(), "redispatch_stuck_work", handler);

        let outcome = executor
            .act(&db, "bp_1", &input("redispatch_stuck_work", "exec_parked"))
            .unwrap();
        let BoothbyActOutcome::Executed { action_id } = outcome else {
            panic!("expected Executed, got {outcome:?}");
        };

        let conn = db.connect().unwrap();
        let (kind, target, reversibility, pre, post): (String, String, String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT target_kind, target_id, reversibility, pre_image, post_image
                 FROM boothby_actions WHERE id = ?1",
                [&action_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(kind, "execution");
        assert_eq!(target, "exec_parked");
        assert_eq!(reversibility, "semi");
        assert_eq!(pre.as_deref(), Some(r#"{"status":"waiting_human"}"#));
        assert_eq!(post.as_deref(), Some(r#"{"status":"ready"}"#));
    }

    /// Even if a handler hands back images, an I-class verb must journal
    /// none: a pre-image implies a restore that cannot exist, and undo would
    /// try it.
    #[test]
    fn an_irreversible_verb_discards_any_pre_image_its_handler_offers() {
        let (_dir, db) = open_db();
        open_pass(&db, "bp_1");
        let handler = Box::new(SpyHandler::with_images(r#"{"held":"yes"}"#, r#"{"held":"no"}"#));
        let executor = executor_with(auto_policy(), "gc_recovery_patches", handler);

        let outcome = confirm_across_two_passes(&executor, &db, "gc_recovery_patches", "/state/recovery/x.patch");
        let BoothbyActOutcome::Executed { action_id } = outcome else {
            panic!("expected Executed, got {outcome:?}");
        };
        let conn = db.connect().unwrap();
        let (reversibility, pre): (String, Option<String>) = conn
            .query_row(
                "SELECT reversibility, pre_image FROM boothby_actions WHERE id = ?1",
                [&action_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reversibility, "irreversible");
        assert_eq!(pre, None, "an I-class action journals no pre-image to restore");
    }

    /// A no-op re-run must not report success with an earlier call's action
    /// id. Boothby retrying a verb (an LLM re-deciding, a duplicate in the
    /// brief) finds the row already archived, so the capture layer journals
    /// nothing — and a read-back keyed on `(verb, target)` rather than on
    /// *this call* would hand back run one's row and call it Executed.
    #[test]
    fn re_running_a_taxonomy_verb_that_changes_nothing_does_not_report_a_stale_action() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(ArchiveHandler));

        let first = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        let BoothbyActOutcome::Executed { action_id } = first else {
            panic!("expected Executed, got {first:?}");
        };

        // Second call: every rail clears (the human-touch guard sees actor
        // `boothby` now, not `human`), the handler re-archives an already-
        // archived row, and no column moves.
        let second = executor.act(&db, "bp_1", &input("close_stale_task", &chore.id));
        let err = second.expect_err("a mutation that moved nothing must not report Executed");
        assert!(
            format!("{err:#}").contains("journalled no action row"),
            "unexpected error: {err:#}",
        );

        let conn = db.connect().unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM boothby_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "the no-op must not have journalled a second row");
        let only: String = conn
            .query_row("SELECT id FROM boothby_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(only, action_id);
    }

    /// `file_chore` is task-targeted but *creates* its task, so its target
    /// cannot exist when the guards run. Gating on `target_kind` alone would
    /// read the row, fail, and refuse every call — Boothby could never file a
    /// finding.
    #[test]
    fn file_chore_is_not_refused_for_a_target_that_does_not_exist_yet() {
        let (_dir, db) = open_db();
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "file_chore", Box::new(handler.clone()));

        // The target names a chore the handler would create; nothing has it.
        let outcome = executor.act(&db, "bp_1", &input("file_chore", "bf_1")).unwrap();
        assert!(
            matches!(outcome, BoothbyActOutcome::Executed { .. }),
            "file_chore must reach its handler, got {outcome:?}",
        );
        assert_eq!(handler.calls(), 1);
    }

    // ── caps ─────────────────────────────────────────────────────────────

    /// The shared budget is real: 3 stale closes + 2 duplicate closes spends
    /// the group's 5, and the sixth close is capped whichever verb asks.
    #[test]
    fn the_shared_close_budget_is_spent_across_both_close_verbs() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        open_pass(&db, "bp_1");
        let mut executor = BoothbyExecutor::new(auto_policy());
        executor.register("close_stale_task", Box::new(ArchiveHandler));
        executor.register("close_duplicate_task", Box::new(ArchiveHandler));

        for i in 0..3 {
            let chore = stale_chore(&db, &product.id, format!("stale {i}"));
            let outcome = executor
                .act(&db, "bp_1", &input("close_stale_task", &chore.id))
                .unwrap();
            assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
        }
        for i in 0..2 {
            let chore = stale_chore(&db, &product.id, format!("dupe {i}"));
            let outcome = executor
                .act(&db, "bp_1", &input("close_duplicate_task", &chore.id))
                .unwrap();
            assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
        }

        // Sixth close: the group budget of 5 is gone, via either verb.
        let chore = stale_chore(&db, &product.id, "one too many");
        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Capped { .. }), "{outcome:?}");
        assert!(
            reason_of(&outcome).contains("close_duplicate_task"),
            "the reason should name the sharers"
        );
    }

    #[test]
    fn the_global_cap_bounds_a_pass_regardless_of_per_verb_budgets() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        open_pass(&db, "bp_1");
        // rerun_effort_heuristic's own budget is 10; the global cap is 2 here.
        let executor = executor_with(
            BoothbyPolicy {
                max_actions_per_pass: 2,
                ..auto_policy()
            },
            "rerun_effort_heuristic",
            Box::new(ArchiveHandler),
        );

        for i in 0..2 {
            let chore = stale_chore(&db, &product.id, format!("drifted {i}"));
            let outcome = executor
                .act(&db, "bp_1", &input("rerun_effort_heuristic", &chore.id))
                .unwrap();
            assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
        }
        let chore = stale_chore(&db, &product.id, "drifted 3");
        let outcome = executor
            .act(&db, "bp_1", &input("rerun_effort_heuristic", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Capped { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("global budget"));
    }

    /// Budgets are per pass, so a new pass restarts them — otherwise Boothby
    /// would go permanently quiet after its first busy half hour.
    #[test]
    fn caps_are_per_pass_and_a_new_pass_restores_the_budget() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        open_pass(&db, "bp_1");
        let executor = executor_with(
            BoothbyPolicy {
                max_actions_per_pass: 1,
                ..auto_policy()
            },
            "close_stale_task",
            Box::new(ArchiveHandler),
        );

        let first = stale_chore(&db, &product.id, "first");
        assert!(matches!(
            executor
                .act(&db, "bp_1", &input("close_stale_task", &first.id))
                .unwrap(),
            BoothbyActOutcome::Executed { .. },
        ));
        let second = stale_chore(&db, &product.id, "second");
        assert!(matches!(
            executor
                .act(&db, "bp_1", &input("close_stale_task", &second.id))
                .unwrap(),
            BoothbyActOutcome::Capped { .. },
        ));

        // Close the pass and open a fresh one.
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE boothby_passes SET outcome = 'capped', finished_at = '2' WHERE id = 'bp_1'",
            [],
        )
        .unwrap();
        drop(conn);
        open_pass(&db, "bp_2");

        assert!(matches!(
            executor
                .act(&db, "bp_2", &input("close_stale_task", &second.id))
                .unwrap(),
            BoothbyActOutcome::Executed { .. },
        ));
    }

    /// A human undoing Boothby's work mid-pass must not hand it a fresh
    /// budget. That would invert the signal: the strongest available evidence
    /// that a pass is going badly would become the thing that buys it room to
    /// do more.
    #[test]
    fn an_undone_action_does_not_refund_its_budget() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        open_pass(&db, "bp_1");
        let executor = executor_with(
            BoothbyPolicy {
                max_actions_per_pass: 1,
                ..auto_policy()
            },
            "close_stale_task",
            Box::new(ArchiveHandler),
        );

        let first = stale_chore(&db, &product.id, "first");
        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &first.id))
            .unwrap();
        let BoothbyActOutcome::Executed { action_id } = outcome else {
            panic!("expected Executed, got {outcome:?}");
        };

        // A human undoes it, still inside the same pass.
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE boothby_actions SET undo_state = 'undone', undone_by = 'human' WHERE id = ?1",
            [&action_id],
        )
        .unwrap();
        drop(conn);

        let second = stale_chore(&db, &product.id, "second");
        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &second.id))
            .unwrap();
        assert!(
            matches!(outcome, BoothbyActOutcome::Capped { .. }),
            "the undone action still counts against the pass budget, got {outcome:?}",
        );
    }

    // ── guards ───────────────────────────────────────────────────────────

    /// The rail that matters most: never archive a task out from under a
    /// running worker.
    #[test]
    fn a_task_with_a_live_execution_is_refused() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "busy chore");
        crate::test_support::create_ready_chore_execution(&db, &chore.id);
        db.force_execution_status_for_test(&chore.id, ExecutionStatus::Running)
            .unwrap();
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("live execution"));
        handler.assert_never_ran();
    }

    /// The design's rail is "a non-terminal execution", which is wider than
    /// "a worker is running". A `queued` execution has no worker yet and no
    /// lease, so it slips past both a `running`/`waiting_human` liveness test
    /// and the lease guard — and archiving its task strands the dispatch.
    #[test]
    fn a_task_with_a_merely_queued_execution_is_refused() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "queued chore");
        crate::test_support::create_ready_chore_execution(&db, &chore.id);
        db.force_execution_status_for_test(&chore.id, ExecutionStatus::Queued)
            .unwrap();
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("live execution"));
        handler.assert_never_ran();
    }

    /// Every non-terminal status, not just the two the coordinator's
    /// double-spawn oracle calls "live". Pinned as a set so a new
    /// `ExecutionStatus` variant has to be considered here.
    #[test]
    fn every_non_terminal_execution_status_blocks() {
        for status in [
            ExecutionStatus::Queued,
            ExecutionStatus::Ready,
            ExecutionStatus::WaitingDependency,
            ExecutionStatus::Running,
            ExecutionStatus::WaitingHuman,
            ExecutionStatus::WaitingReview,
            ExecutionStatus::WaitingMerge,
        ] {
            let label = status.as_str();
            assert!(!status.is_terminal(), "{label} is not a non-terminal status");
            let (_dir, db) = open_db();
            let product = create_test_product_named(&db, "Boss");
            let chore = stale_chore(&db, &product.id, "chore");
            crate::test_support::create_ready_chore_execution(&db, &chore.id);
            db.force_execution_status_for_test(&chore.id, status).unwrap();

            let verdict = guards::live_work_guard(&db, &chore.id).unwrap();
            assert!(!verdict.is_clear(), "{label} must read as work in flight");
        }
    }

    /// A *terminal* execution is not live work — the common case, and if this
    /// refused, Boothby could never close anything that had ever run.
    #[test]
    fn a_task_whose_execution_is_terminal_is_not_treated_as_live() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "finished chore");
        crate::test_support::create_ready_chore_execution(&db, &chore.id);
        db.force_execution_status_for_test(&chore.id, ExecutionStatus::Completed)
            .unwrap();
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(ArchiveHandler));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
    }

    /// The gap the lease guard covers: `waiting_review` is not live by
    /// `is_live`, so the live-work guard passes it through, but the workspace
    /// is still checked out. Without this rail Boothby would mutate a row
    /// whose workspace something else still holds.
    #[test]
    fn the_lease_guard_names_the_holding_execution() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "leased chore");
        crate::test_support::create_ready_chore_execution(&db, &chore.id);
        hold_lease_while_waiting_review(&db, &chore.id, "lease-abc");

        // Exercised directly rather than through `act`: now that the
        // live-work guard covers every non-terminal status, it fires first on
        // any row this one would also catch, and the lease guard's own
        // reachable case is the terminal-but-still-leased row below. This
        // keeps the guard itself covered, and its refusal legible.
        let verdict = guards::lease_guard(&db, &chore.id).unwrap();
        let reason = verdict.reason().expect("a recorded lease must refuse");
        assert!(reason.contains("lease-abc"), "reason should name the lease: {reason}");
        assert!(reason.contains("cube lease"), "{reason}");
    }

    /// A lease recorded on a *terminal* execution is provenance — which
    /// workspace produced this work — not a live checkout. Treating it as one
    /// would lock Boothby out of every row that ever ran.
    #[test]
    fn a_lease_still_recorded_on_a_terminal_execution_is_refused_as_stuck() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "crashed chore");
        crate::test_support::create_ready_chore_execution(&db, &chore.id);
        hold_lease_while_waiting_review(&db, &chore.id, "lease-abc");
        db.force_execution_status_for_test(&chore.id, ExecutionStatus::Failed)
            .unwrap();
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("cube lease"));
        handler.assert_never_ran();
    }

    /// The common case, and the one that would be catastrophic to get wrong:
    /// a properly-released lease is NULL, so a finished task stays closable.
    /// If this refused, Boothby could never close anything that had ever run.
    #[test]
    fn a_released_lease_on_a_terminal_execution_does_not_block() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "finished chore");
        crate::test_support::create_ready_chore_execution(&db, &chore.id);
        hold_lease_while_waiting_review(&db, &chore.id, "lease-abc");
        // Release NULLs the column, exactly as `release_execution_workspace`
        // does on the normal teardown path.
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET cube_lease_id = NULL WHERE work_item_id = ?1",
            [&chore.id],
        )
        .unwrap();
        drop(conn);
        db.force_execution_status_for_test(&chore.id, ExecutionStatus::Completed)
            .unwrap();
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(ArchiveHandler));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
    }

    /// The rail that protects a human's judgement rather than a worker's.
    /// Note what this test does *not* do: it never ages the row, because a
    /// freshly-created chore is by definition one a human touched seconds
    /// ago. Every other test here has to call `stale_chore` precisely because
    /// this guard is on by default.
    #[test]
    fn a_task_a_human_touched_recently_is_refused() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = create_test_chore_manual(&db, &product.id, "just created");
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("cooldown"));
        handler.assert_never_ran();
    }

    /// Projects carry `updated_at` + `last_status_actor` too, so the cooldown
    /// covers them: archiving a project a human was editing an hour ago is as
    /// unwelcome as closing their task. The live-work and lease guards do not
    /// apply — a project has no execution of its own.
    #[test]
    fn a_project_a_human_touched_recently_is_refused() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let project = db
            .create_project(crate::work::CreateProjectInput {
                product_id: product.id.clone(),
                name: "P".to_owned(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: true,
            })
            .unwrap();
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "archive_empty_project", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("archive_empty_project", &project.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("cooldown"));
        handler.assert_never_ran();
    }

    /// The same project, aged past the cooldown, goes through — proving the
    /// test above pins the cooldown rather than a project-shaped target being
    /// rejected outright.
    #[test]
    fn an_aged_project_passes_the_guards() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let project = db
            .create_project(crate::work::CreateProjectInput {
                product_id: product.id.clone(),
                name: "P".to_owned(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: true,
            })
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE projects SET updated_at = ?2 WHERE id = ?1",
            rusqlite::params![project.id, (guards::now_epoch_secs() - 90 * 24 * 3600).to_string()],
        )
        .unwrap();
        drop(conn);
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "archive_empty_project", Box::new(ArchiveProjectHandler));

        let outcome = executor
            .act(&db, "bp_1", &input("archive_empty_project", &project.id))
            .unwrap();
        let BoothbyActOutcome::Executed { action_id } = outcome else {
            panic!("expected Executed, got {outcome:?}");
        };
        let conn = db.connect().unwrap();
        let (kind, post): (String, String) = conn
            .query_row(
                "SELECT target_kind, post_image FROM boothby_actions WHERE id = ?1",
                [&action_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "project");
        assert!(post.contains(r#""status":"archived""#), "post_image was {post}");
    }

    /// A vanished target is an ordinary mid-pass race (a human deleted it),
    /// not a machinery fault.
    #[test]
    fn a_target_that_does_not_exist_is_refused_not_errored() {
        let (_dir, db) = open_db();
        open_pass(&db, "bp_1");
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor.act(&db, "bp_1", &input("close_stale_task", "T999")).unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
    }

    /// The operational verbs exist to clean up after work that is already
    /// broken, so a live-work guard would refuse exactly the cases they are
    /// for. Their protection is two-pass confirmation instead.
    #[test]
    fn an_execution_targeting_verb_skips_the_work_item_guards() {
        let (_dir, db) = open_db();
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "reap_dead_execution", Box::new(SpyHandler::default()));

        // exec_ghost is not a work item at all; the guards must not try to
        // read it as one and refuse.
        let outcome = executor
            .act(&db, "bp_1", &input("reap_dead_execution", "exec_ghost"))
            .unwrap();
        assert!(
            matches!(outcome, BoothbyActOutcome::Deferred { .. }),
            "expected the two-pass gate, not a guard refusal: {outcome:?}",
        );
    }

    // ── two-pass confirmation ────────────────────────────────────────────

    #[test]
    fn an_irreversible_verb_defers_on_the_first_pass_and_fires_on_the_second() {
        let (_dir, db) = open_db();
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "reap_dead_execution", Box::new(SpyHandler::default()));

        let outcome = executor
            .act(&db, "bp_1", &input("reap_dead_execution", "exec_1"))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Deferred { .. }), "{outcome:?}");
        let conn = db.connect().unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM boothby_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "a deferred action must journal nothing");
        drop(conn);

        roll_pass(&db, "bp_2");
        let outcome = executor
            .act(&db, "bp_2", &input("reap_dead_execution", "exec_1"))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
    }

    /// A reversible verb has an undo, so making it wait a pass would buy
    /// nothing and halve Boothby's throughput.
    #[test]
    fn a_reversible_verb_needs_no_confirmation() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(ArchiveHandler));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
    }

    /// The ordering property behind putting the gate last. If a guard
    /// refusal still banked a nomination, the same request next pass would
    /// find itself "confirmed" on the strength of a pass where it never
    /// cleared — an irreversible action justified by a pass that refused it.
    #[test]
    fn a_guard_refusal_does_not_bank_a_two_pass_nomination() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "leased chore");
        crate::test_support::create_ready_chore_execution(&db, &chore.id);
        hold_lease_while_waiting_review(&db, &chore.id, "lease-abc");
        open_pass(&db, "bp_1");
        // A task-targeting I-class verb would be the sharp case, but the
        // catalogue has none; assert the gate's own state directly instead.
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(SpyHandler::default()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }));
        assert_eq!(
            executor.gate.nominate("bp_1", "close_stale_task", &chore.id),
            Confirmation::Deferred,
            "the refused request must not have nominated anything",
        );
    }

    // ── fingerprint refusal ──────────────────────────────────────────────

    /// Human veto is permanent: re-making a decision a human undid is exactly
    /// the close→reopen→close flap the design set out to prevent.
    #[test]
    fn a_previously_undone_action_is_refused_forever() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(ArchiveHandler));

        // Boothby closed it; a human undid that.
        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        let BoothbyActOutcome::Executed { action_id } = outcome else {
            panic!("expected Executed, got {outcome:?}");
        };
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE boothby_actions SET undo_state = 'undone', undone_by = 'human' WHERE id = ?1",
            [&action_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE boothby_passes SET outcome = 'completed', finished_at = '2' WHERE id = 'bp_1'",
            [],
        )
        .unwrap();
        drop(conn);
        open_pass(&db, "bp_2");

        let outcome = executor
            .act(&db, "bp_2", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("already reversed"));
    }

    /// A `conflicted` undo means a human tried to reverse it and the row had
    /// moved. The intent to veto is just as clear as a clean `undone`.
    #[test]
    fn a_conflicted_action_is_also_treated_as_a_veto() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO boothby_actions
                (id, pass_id, seq, verb, target_kind, target_id, rationale, reversibility, undo_state, created_at)
             VALUES ('ba_old', 'bp_1', 1, 'close_stale_task', 'task', ?1, 'earlier close', 'reversible',
                     'conflicted', '1')",
            [&chore.id],
        )
        .unwrap();
        drop(conn);
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(SpyHandler::default()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
    }

    /// The veto is scoped to the decision, not the row: a *different* verb on
    /// the same target is a different judgement and is not pre-vetoed.
    #[test]
    fn a_veto_on_one_verb_does_not_block_another_verb_on_the_same_target() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a chore");
        open_pass(&db, "bp_1");
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO boothby_actions
                (id, pass_id, seq, verb, target_kind, target_id, rationale, reversibility, undo_state, created_at)
             VALUES ('ba_old', 'bp_1', 1, 'close_stale_task', 'task', ?1, 'earlier close', 'reversible',
                     'undone', '1')",
            [&chore.id],
        )
        .unwrap();
        drop(conn);
        let executor = executor_with(auto_policy(), "rerun_effort_heuristic", Box::new(ArchiveHandler));

        let outcome = executor
            .act(&db, "bp_1", &input("rerun_effort_heuristic", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Executed { .. }), "{outcome:?}");
    }

    /// The operator-clearable half of the veto rail.
    #[test]
    fn a_suppressed_fingerprint_is_refused() {
        let (_dir, db) = open_db();
        let product = create_test_product_named(&db, "Boss");
        let chore = stale_chore(&db, &product.id, "a stale chore");
        open_pass(&db, "bp_1");
        let fingerprint = action_fingerprint("close_stale_task", "task", &chore.id);
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO boothby_findings (id, fingerprint, kind, subject, first_seen, last_seen, status)
             VALUES ('bf_1', ?1, 'taxonomy', '{}', '1', '1', 'suppressed')",
            [&fingerprint],
        )
        .unwrap();
        drop(conn);
        let handler = SpyHandler::default();
        let executor = executor_with(auto_policy(), "close_stale_task", Box::new(handler.clone()));

        let outcome = executor
            .act(&db, "bp_1", &input("close_stale_task", &chore.id))
            .unwrap();
        assert!(matches!(outcome, BoothbyActOutcome::Refused { .. }), "{outcome:?}");
        assert!(reason_of(&outcome).contains("suppressed"));
        handler.assert_never_ran();
    }

    /// The fingerprint is what an operator reads in `boss boothby
    /// suppressions` and what the ledger joins on, so its shape is a
    /// contract, not an implementation detail.
    #[test]
    fn the_action_fingerprint_is_stable_and_readable() {
        assert_eq!(
            action_fingerprint("close_stale_task", "task", "T12"),
            "action:close_stale_task:task:T12",
        );
    }
}
