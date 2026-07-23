//! Trunk merge-queue state ingestion — the [`TrunkQueueProbe`] sibling of
//! [`crate::merge_poller::CommandMergeProbe`].
//!
//! Deliberately *not* a free-running loop of its own: [`TrunkQueueProbe`]
//! is a plain state machine driven from the merge poller's own wait loop
//! (`merge_poller::spawn_loop`), so it inherits that sweep's publisher
//! plumbing, error accounting, and lifetime. See
//! `tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-
//! merges-merging-ui.md` §"Queue state ingestion: polling".
//!
//! # What one pass does
//!
//! 1. Read every `active` `trunk_merge_intents` row (a cheap local-DB
//!    read; no Trunk traffic) and group it by `(repo, target_branch)`.
//! 2. For each group whose cadence tier has elapsed, issue **one**
//!    `getQueue` call — it returns queue state plus every enqueued PR, so
//!    position and per-PR state for every tracked entry arrive together.
//! 3. Each tracked PR still in `enqueuedPullRequests` gets its Merging-UI
//!    columns rewritten; each one that has *left* costs one
//!    `getSubmittedPullRequest` to resolve the terminal state it left for.
//!
//! # What it owns, and what it deliberately doesn't
//!
//! It owns `tasks.merge_queue_state` / `merge_queue_detail` for
//! `trunk_queue` products (the GitHub probe stands off them via
//! `PrPollStateInput::preserve_merge_queue_state`) and
//! `trunk_merge_intents.last_trunk_state`.
//!
//! It does **not** decide that a PR merged: Trunk's `merged` state only
//! retires the intent, because the GitHub-side probe already detects the
//! merged PR and runs the whole `mark_merged()` cascade (design §"The
//! merge verb"). And it does **not** remediate an eviction — observing
//! `failed`/`pending_failure` records the state and leaves the intent
//! `active` for the `ci_watch` eviction path (design task 6) to pick up.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use boss_protocol::{CreateAttentionItemInput, FrontendEvent};
use boss_trunk_client::{
    GetQueueRequest, ListPullRequestsRequest, ListPullRequestsResponse, TrunkError, TrunkPrLookup, TrunkPrRef,
    TrunkPrState, TrunkPullRequest, TrunkQueue, TrunkQueueState, TrunkRepoRef,
};

use crate::coordinator::ExecutionPublisher;
use crate::metrics::Registry;
use crate::trunk_merge::trunk_repo_ref;
use crate::work::{ActiveTrunkMergeIntent, WorkDb};

// ── Metrics ───────────────────────────────────────────────────────────────

crate::register_counter!(
    QUEUE_PROBES,
    "trunk_queue_poller.queue_probes",
    "getQueue calls issued (one per (repo, target_branch) with >=1 active intent, per cadence tick)."
);
crate::register_counter!(
    QUEUE_PROBE_FAILURES,
    "trunk_queue_poller.queue_probe_failures",
    "getQueue calls that failed, putting the queue into exponential backoff."
);
crate::register_counter!(
    ENTRY_LOOKUPS,
    "trunk_queue_poller.entry_lookups",
    "getSubmittedPullRequest calls issued to resolve an entry that left the queue."
);
crate::register_counter!(
    STATE_WRITES,
    "trunk_queue_poller.state_writes",
    "Merging-UI column writes that actually changed a task's stored queue state."
);
crate::register_counter!(
    INTENTS_RETIRED,
    "trunk_queue_poller.intents_retired",
    "Active merge intents retired to a terminal status by the queue poller."
);
crate::register_counter!(
    ATTENTIONS_FILED,
    "trunk_queue_poller.attentions_filed",
    "Attention items filed for a paused/draining queue, an unreachable/rejecting API, or a cancelled entry."
);

/// Register every counter this module declares. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn init(registry: &Registry) {
    registry.register_counter(&QUEUE_PROBES);
    registry.register_counter(&QUEUE_PROBE_FAILURES);
    registry.register_counter(&ENTRY_LOOKUPS);
    registry.register_counter(&STATE_WRITES);
    registry.register_counter(&INTENTS_RETIRED);
    registry.register_counter(&ATTENTIONS_FILED);
}

/// Fold one pass's [`TrunkSweepOutcome`] into the registry.
pub fn record_pass_metrics(metrics: &Registry, outcome: &TrunkSweepOutcome) {
    QUEUE_PROBES.inc_by(metrics, outcome.queues_probed as u64);
    QUEUE_PROBE_FAILURES.inc_by(metrics, outcome.probe_failures as u64);
    ENTRY_LOOKUPS.inc_by(metrics, outcome.entry_lookups as u64);
    STATE_WRITES.inc_by(metrics, outcome.state_writes as u64);
    INTENTS_RETIRED.inc_by(metrics, outcome.intents_retired as u64);
    ATTENTIONS_FILED.inc_by(metrics, outcome.attentions_filed as u64);
}

// ── Tunables ──────────────────────────────────────────────────────────────

/// `tasks.merge_queue_state` value for any live Trunk entry. Shared with
/// the GitHub-native queue path on purpose: `isInMergingSection` in the
/// macOS app keys on the column being non-NULL, and `"queued"` is what it
/// already understands — the mechanism is disambiguated inside
/// `merge_queue_detail` by `source: "trunk"`.
pub const MERGE_QUEUE_STATE_QUEUED: &str = "queued";

/// How often the poller re-reads the (local, cheap) active-intent list
/// when no queue is currently tracked. "Idle off" in the design means *no
/// Trunk API traffic* while nothing is enqueued — not no wakeups: a merge
/// click has to be noticed somehow, and `handle_trunk_queue_merge`
/// deliberately does not kick the PR reconciler (that kick wakes the
/// GitHub probe, which has nothing to do for a `trunk_queue` product).
const IDLE_RESCAN_INTERVAL: Duration = Duration::from_secs(15);

/// First backoff step after a failed `getQueue`, doubling per consecutive
/// failure up to [`BACKOFF_CAP`]. Unjittered: `boss_trunk_client` already
/// jitters its own in-call retries, and a single-operator engine has no
/// thundering-herd to spread.
const BACKOFF_BASE: Duration = Duration::from_secs(30);
/// Ceiling on the per-queue backoff, so a long Trunk outage costs 12
/// requests/hour per queue rather than compounding to hours of silence.
const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// Cadence of the `listPullRequests since=<last sweep>` reconciliation
/// backstop, independent of the 15 s/30 s point-probe cadence above.
/// Catches transitions the point probes (`getQueue` plus a per-missing-
/// entry `getSubmittedPullRequest`) missed — e.g. an entry that both
/// joined and left the queue between two `getQueue` calls. Ten minutes
/// matches the design's stated backstop cadence: frequent enough that a
/// missed transition doesn't strand a card indefinitely, infrequent
/// enough to add negligible Trunk API traffic on top of the point probes.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Hard cap on pages walked by one reconciliation call to
/// `listPullRequests`. At the default `take` of 50 this covers 1000 PRs
/// per queue per attempt — far more than one repo's branch should ever
/// have concluded inside one [`RECONCILE_INTERVAL`] window — while
/// bounding worst-case Trunk traffic and wall-clock for a single sweep.
const RECONCILE_MAX_PAGES: usize = 20;

/// How long a queue must be continuously unreachable, while entries are
/// being tracked, before the operator gets told. Short enough to catch a
/// real outage during a merge, long enough that a Trunk blip riding out
/// two or three backoff steps stays silent.
const UNREACHABLE_ATTENTION_AFTER: Duration = Duration::from_secs(15 * 60);

/// `work_attention_items.kind` for a queue Boss cannot reach.
pub const TRUNK_QUEUE_UNREACHABLE_ATTENTION_KIND: &str = "trunk_queue_unreachable";
/// `work_attention_items.kind` for a token Trunk rejected at poll time.
pub const TRUNK_TOKEN_REJECTED_ATTENTION_KIND: &str = "trunk_token_rejected";
/// `work_attention_items.kind` for a queue that is not `RUNNING`.
pub const TRUNK_QUEUE_NOT_RUNNING_ATTENTION_KIND: &str = "trunk_queue_not_running";
/// `work_attention_items.kind` for an entry that left the queue via
/// cancellation rather than a merge or a test failure.
pub const TRUNK_QUEUE_ENTRY_CANCELLED_ATTENTION_KIND: &str = "trunk_queue_entry_cancelled";

// ── Transport seam ────────────────────────────────────────────────────────

/// The two Trunk read verbs this poller issues, behind a trait so tests
/// can drive the whole state machine without a mock HTTP server.
///
/// Deliberately narrower than [`boss_trunk_client::TrunkClient`]: the
/// poller never writes to the queue (no submit/cancel/restart), and
/// spelling that out in the type keeps it that way.
#[async_trait]
pub trait TrunkQueueApi: Send + Sync {
    async fn get_queue(&self, request: &GetQueueRequest) -> Result<TrunkQueue, TrunkError>;
    async fn get_submitted_pull_request(&self, request: &TrunkPrLookup) -> Result<TrunkPullRequest, TrunkError>;
    async fn list_pull_requests(
        &self,
        request: &ListPullRequestsRequest,
    ) -> Result<ListPullRequestsResponse, TrunkError>;
}

#[async_trait]
impl TrunkQueueApi for boss_trunk_client::TrunkClient {
    async fn get_queue(&self, request: &GetQueueRequest) -> Result<TrunkQueue, TrunkError> {
        boss_trunk_client::TrunkClient::get_queue(self, request).await
    }

    async fn get_submitted_pull_request(&self, request: &TrunkPrLookup) -> Result<TrunkPullRequest, TrunkError> {
        boss_trunk_client::TrunkClient::get_submitted_pull_request(self, request).await
    }

    async fn list_pull_requests(
        &self,
        request: &ListPullRequestsRequest,
    ) -> Result<ListPullRequestsResponse, TrunkError> {
        boss_trunk_client::TrunkClient::list_pull_requests(self, request).await
    }
}

// ── Cadence ───────────────────────────────────────────────────────────────

/// Per-queue polling cadence, the Trunk analogue of
/// [`crate::merge_poller::PollTier`]. Chosen so the Merging lane is never
/// more than ~30 s stale while anything is enqueued, at 2-4 requests/min
/// against an API with no documented rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrunkPollTier {
    /// At least one tracked entry is testing, or has passed tests and is
    /// about to merge — the states that move fastest and that an operator
    /// is most likely watching. 15 s, matching the GitHub probe's Hot tier.
    Testing,
    /// Everything tracked is merely waiting its turn. Position churn is
    /// slower than test progress, so 30 s.
    Pending,
}

impl TrunkPollTier {
    pub fn interval(self) -> Duration {
        match self {
            TrunkPollTier::Testing => Duration::from_secs(15),
            TrunkPollTier::Pending => Duration::from_secs(30),
        }
    }
}

/// Whether an observed Trunk PR state puts its queue on the fast tier.
fn is_fast_tier_state(state: &TrunkPrState) -> bool {
    matches!(state, TrunkPrState::Testing | TrunkPrState::TestsPassed)
}

/// Whether an observed Trunk PR state is one the entry never leaves — the
/// states that resolve an intent rather than describing a live entry.
fn is_terminal_trunk_state(state: &TrunkPrState) -> bool {
    matches!(
        state,
        TrunkPrState::Merged | TrunkPrState::Cancelled | TrunkPrState::Failed | TrunkPrState::PendingFailure
    )
}

/// Whether a previous pass already resolved this intent's exit from the
/// queue into a terminal Trunk state. True only for an intent that stayed
/// `active` afterwards — i.e. an eviction, since `merged`/`cancelled`
/// retire the intent and so never reach a later pass.
fn already_resolved_terminal(member: &ActiveTrunkMergeIntent) -> bool {
    member
        .intent
        .last_trunk_state
        .as_deref()
        .is_some_and(|state| is_terminal_trunk_state(&TrunkPrState::from(state.to_owned())))
}

/// Task statuses past which a merge intent is moot — the card has left the
/// review lifecycle entirely, so there is nothing left to render or
/// resubmit. Their intents are retired instead of polled forever (a PR
/// merged outside the queue would otherwise 404 from
/// `getSubmittedPullRequest` on every sweep, indefinitely).
fn is_terminal_task_status(status: &str) -> bool {
    matches!(status, "done" | "archived")
}

/// Backoff before the next `getQueue` after `consecutive_failures`
/// consecutive failures (1-based): 30 s, 60 s, 120 s, 240 s, then capped
/// at 5 min.
fn backoff_delay(consecutive_failures: u32) -> Duration {
    let steps = consecutive_failures.saturating_sub(1).min(8);
    BACKOFF_BASE.saturating_mul(1u32 << steps).min(BACKOFF_CAP)
}

// ── Pass wiring ───────────────────────────────────────────────────────────

/// Everything one [`TrunkQueueProbe::run_pass`] borrows. Bundled so the
/// call site in `merge_poller::spawn_loop` (already threading a long
/// argument list) stays readable.
pub struct TrunkSweepContext<'a> {
    pub work_db: &'a WorkDb,
    pub publisher: &'a dyn ExecutionPublisher,
    pub api: &'a dyn TrunkQueueApi,
}

/// Outcome of one pass, for logging, metrics, and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, bon::Builder)]
pub struct TrunkSweepOutcome {
    /// `getQueue` calls issued this pass.
    pub queues_probed: usize,
    /// `getSubmittedPullRequest` calls issued this pass.
    pub entry_lookups: usize,
    /// Merging-UI column writes that actually moved a task's stored state.
    pub state_writes: usize,
    /// Intents retired to a terminal status this pass.
    pub intents_retired: usize,
    /// Trunk calls that failed (queue probes and entry lookups alike).
    pub probe_failures: usize,
    /// Attention items filed this pass.
    pub attentions_filed: usize,
}

impl TrunkSweepOutcome {
    /// Whether this pass did anything worth an info-level log line.
    pub fn is_noteworthy(&self) -> bool {
        self.state_writes > 0 || self.intents_retired > 0 || self.probe_failures > 0 || self.attentions_filed > 0
    }
}

/// One queue's identity: exactly the pair `getQueue` is addressed by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct QueueKey {
    repo: String,
    target_branch: String,
}

/// Consecutive-failure bookkeeping for one queue. Split out of
/// [`QueueRuntime`] so each struct stays a single concern (and under the
/// repo's giant-struct threshold).
#[derive(Debug, Default)]
struct QueueFailureState {
    consecutive_failures: u32,
    /// When the current unbroken run of failures started. `None` while the
    /// queue is healthy.
    unreachable_since: Option<Instant>,
    unreachable_attention_filed: bool,
    auth_attention_filed: bool,
}

impl QueueFailureState {
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Per-queue runtime state. Purely in-memory and best-effort, exactly like
/// [`crate::merge_poller::PrPollSchedule`]: it is rebuilt from the DB's own
/// active-intent list on every pass, so a restart costs at most one
/// re-filed attention item and one early probe — never a lost transition.
#[derive(Debug)]
struct QueueRuntime {
    next_due_at: Instant,
    failure: QueueFailureState,
    /// The non-`RUNNING` queue state an attention item has already been
    /// filed for. `None` while the queue is `RUNNING` — that is what makes
    /// the item one-per-episode rather than one-per-sweep.
    queue_state_attention: Option<TrunkQueueState>,
    /// When the `listPullRequests` reconciliation backstop is next due for
    /// this queue. Independent of `next_due_at`'s point-probe cadence.
    next_reconcile_due_at: Instant,
    /// The `since` cursor for the next reconciliation call — the wall-clock
    /// time (RFC 3339) the previous one ran, or `None` before the first.
    last_reconciled_since: Option<String>,
}

impl QueueRuntime {
    /// A newly-discovered queue is due immediately: the merge click that
    /// created its first intent should surface a queue position within
    /// seconds, not after a full tier interval. The reconciliation
    /// backstop is not: it exists to catch what the point probes missed,
    /// so a queue with no prior observations has nothing to reconcile yet.
    fn due_at(now: Instant) -> Self {
        Self {
            next_due_at: now,
            failure: QueueFailureState::default(),
            queue_state_attention: None,
            next_reconcile_due_at: now + RECONCILE_INTERVAL,
            last_reconciled_since: None,
        }
    }
}

/// The Trunk-side merge-queue observer. Holds only cadence/backoff/
/// attention-dedup state; everything durable lives in the DB.
#[derive(Debug, Default)]
pub struct TrunkQueueProbe {
    queues: HashMap<QueueKey, QueueRuntime>,
}

impl TrunkQueueProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// When [`Self::run_pass`] next has something to do. Never later than
    /// [`IDLE_RESCAN_INTERVAL`], so a fresh merge intent is discovered
    /// promptly even when nothing is currently tracked; a backed-off queue
    /// simply gets skipped by the pass until its own due time arrives.
    pub fn next_wake_at(&self, now: Instant) -> Instant {
        let idle = now + IDLE_RESCAN_INTERVAL;
        self.queues
            .values()
            .map(|runtime| runtime.next_due_at)
            .min()
            .map_or(idle, |due| due.min(idle))
    }

    /// Run one pass. `now` is injected rather than read from the clock so
    /// tests can exercise the cadence tiers, the backoff ladder, and the
    /// 15-minute unreachable threshold without sleeping.
    pub async fn run_pass(&mut self, ctx: &TrunkSweepContext<'_>, now: Instant) -> TrunkSweepOutcome {
        let mut outcome = TrunkSweepOutcome::default();
        let intents = match ctx.work_db.list_active_trunk_merge_intents() {
            Ok(intents) => intents,
            Err(err) => {
                tracing::warn!(?err, "trunk queue poller: failed to list active merge intents");
                return outcome;
            }
        };

        // `BTreeMap` (not `HashMap`): probe order across queues is then
        // deterministic, which keeps the tests — and any trace read
        // afterwards — reproducible. `BTreeSet` likewise for the products
        // touched this pass, so the renumbering pass below runs in a
        // reproducible order too.
        let mut groups: BTreeMap<QueueKey, Vec<ActiveTrunkMergeIntent>> = BTreeMap::new();
        let mut product_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for intent in intents {
            product_ids.insert(intent.product_id.clone());
            if is_terminal_task_status(&intent.task_status) {
                retire_moot_intent(ctx, &intent, &mut outcome);
                continue;
            }
            let key = QueueKey {
                repo: intent.intent.repo.clone(),
                target_branch: intent.intent.target_branch.clone(),
            };
            groups.entry(key).or_default().push(intent);
        }

        // Idle off: a queue with no active intents stops being polled at
        // all, and forgetting its runtime state means the next intent for
        // it probes immediately instead of inheriting a stale backoff.
        self.queues.retain(|key, _| groups.contains_key(key));

        for (key, members) in groups {
            let due = self
                .queues
                .entry(key.clone())
                .or_insert_with(|| QueueRuntime::due_at(now))
                .next_due_at;
            if due > now {
                continue;
            }
            self.probe_queue(ctx, &key, &members, now, &mut outcome).await;
        }

        // Mirrors `merge_poller::renumber_merge_queue`'s "recompute the
        // whole product's ranking after every probe-driven write" shape
        // for the Trunk path — see `renumber_trunk_merge_queue`'s doc
        // comment for why it can't just call that function directly.
        for product_id in product_ids {
            renumber_trunk_merge_queue(ctx, &product_id, &mut outcome).await;
        }
        outcome
    }

    /// One `getQueue` round trip for `key`, plus the per-member
    /// reconciliation it enables.
    async fn probe_queue(
        &mut self,
        ctx: &TrunkSweepContext<'_>,
        key: &QueueKey,
        members: &[ActiveTrunkMergeIntent],
        now: Instant,
        outcome: &mut TrunkSweepOutcome,
    ) {
        let Some(repo_ref) = trunk_repo_ref(&key.repo) else {
            tracing::error!(
                repo = %key.repo,
                target_branch = %key.target_branch,
                intents = members.len(),
                "trunk queue poller: intent carries a repo slug that is not `owner/name`; parking this queue",
            );
            self.set_next_due(key, now + BACKOFF_CAP);
            return;
        };

        // Run independently of whether `getQueue` below succeeds: a queue
        // whose `getQueue` is persistently failing (or backed off to the
        // 5-minute cap) is exactly the condition the backstop exists to
        // cover, so it can't be gated on that call's own success.
        self.reconcile_missed_transitions(ctx, key, &repo_ref, members, now, outcome)
            .await;

        outcome.queues_probed += 1;
        let request = GetQueueRequest::new(repo_ref.clone(), key.target_branch.clone());
        let queue = match ctx.api.get_queue(&request).await {
            Ok(queue) => queue,
            Err(err) => {
                self.record_queue_failure(ctx, key, members, &err, now, outcome).await;
                return;
            }
        };

        if let Some(runtime) = self.queues.get_mut(key) {
            runtime.failure.clear();
        }
        self.reconcile_queue_state_attention(ctx, key, members, &queue, outcome)
            .await;

        // Position is the 1-based index into the queue as a whole (not
        // just the entries Boss tracks), matching the GitHub path's
        // `mergeQueueEntry.position` convention that `MergeQueueDetail`
        // renders as `#n`. First occurrence wins if Trunk ever reports a
        // PR twice.
        let mut by_pr_number: HashMap<u64, (i64, &TrunkPullRequest)> = HashMap::new();
        for (index, entry) in queue.enqueued_pull_requests.iter().enumerate() {
            by_pr_number
                .entry(entry.pr_number)
                .or_insert(((index + 1) as i64, entry));
        }

        let mut tier = TrunkPollTier::Pending;
        for member in members {
            let observed = match by_pr_number.get(&(member.intent.pr_number as u64)) {
                Some((position, entry)) if !is_terminal_trunk_state(&entry.state) => {
                    write_live_entry(ctx, member, entry, *position, &queue.state, outcome).await;
                    Some(entry.state.clone())
                }
                // Terminal state reported inline. Trunk's live queue
                // shouldn't carry one, but resolving it here rather than
                // waiting for it to disappear costs nothing and closes the
                // window where a failed/cancelled entry keeps rendering as
                // a healthy queue member.
                Some((_, entry)) => {
                    apply_resolved_state(ctx, member, &entry.state, outcome).await;
                    Some(entry.state.clone())
                }
                // Already resolved on an earlier pass and deliberately
                // left `active` (an eviction awaiting remediation). Its
                // terminal state cannot change while it is out of the
                // queue, and a resubmit puts it back in
                // `enqueuedPullRequests` — where the arm above picks it up
                // — so re-asking every cycle would buy nothing and never
                // stop.
                None if already_resolved_terminal(member) => None,
                None => resolve_missing_entry(ctx, member, &repo_ref, &key.target_branch, outcome).await,
            };
            if observed.as_ref().is_some_and(is_fast_tier_state) {
                tier = TrunkPollTier::Testing;
            }
        }

        self.set_next_due(key, now + tier.interval());
        tracing::debug!(
            repo = %key.repo,
            target_branch = %key.target_branch,
            queue_state = %String::from(queue.state.clone()),
            enqueued = queue.enqueued_pull_requests.len(),
            tracked = members.len(),
            ?tier,
            "trunk queue poller: queue probed",
        );
    }

    fn set_next_due(&mut self, key: &QueueKey, at: Instant) {
        if let Some(runtime) = self.queues.get_mut(key) {
            runtime.next_due_at = at;
        }
    }

    /// The low-frequency `listPullRequests since=<last sweep>` backstop:
    /// catches a transition the point probes (`getQueue` plus a per-
    /// missing-entry `getSubmittedPullRequest`) missed entirely — e.g. an
    /// entry that both joined and left the real queue between two
    /// `getQueue` calls, so it never showed up as "present" or "newly
    /// missing" to either point probe. Runs at most once per
    /// [`RECONCILE_INTERVAL`] per queue, piggybacking on whatever cadence
    /// already calls [`Self::probe_queue`] rather than adding its own
    /// wakeup.
    async fn reconcile_missed_transitions(
        &mut self,
        ctx: &TrunkSweepContext<'_>,
        key: &QueueKey,
        repo_ref: &TrunkRepoRef,
        members: &[ActiveTrunkMergeIntent],
        now: Instant,
        outcome: &mut TrunkSweepOutcome,
    ) {
        let Some(runtime) = self.queues.get(key) else {
            return;
        };
        if runtime.next_reconcile_due_at > now {
            return;
        }
        let since = runtime.last_reconciled_since.clone();
        // Stamped before the (possibly multi-page) round trip below, not
        // after: capturing it on return would leave a permanent gap for
        // anything Trunk concludes between this snapshot and the last
        // page landing.
        let observed_at =
            boss_engine_utils::iso8601::format_epoch_iso8601(boss_engine_utils::epoch_time::now_epoch_secs());

        // Walk every page `listPullRequests` reports: Trunk defaults
        // `take` to 50, and the very first call (no `since` yet, or a
        // `since` inherited from before an engine restart) can be an
        // unbounded list of the branch's PRs, so a tracked member's
        // terminal transition is very likely beyond page one. Capped so a
        // pathological queue can't wedge a sweep in an unbounded fetch
        // loop.
        let mut cursor: Option<String> = None;
        let mut succeeded = true;
        for _ in 0..RECONCILE_MAX_PAGES {
            let request = ListPullRequestsRequest::builder()
                .repo(repo_ref.clone())
                .target_branch(key.target_branch.clone())
                .maybe_since(since.clone())
                .maybe_cursor(cursor.take())
                .build();
            match ctx.api.list_pull_requests(&request).await {
                Ok(response) => {
                    for pr in &response.pull_requests {
                        if !is_terminal_trunk_state(&pr.state) {
                            continue;
                        }
                        if let Some(member) = members
                            .iter()
                            .find(|member| member.intent.pr_number as u64 == pr.pr_number)
                        {
                            apply_resolved_state(ctx, member, &pr.state, outcome).await;
                        }
                    }
                    match response.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
                Err(err) => {
                    succeeded = false;
                    outcome.probe_failures += 1;
                    tracing::warn!(
                        repo = %key.repo,
                        target_branch = %key.target_branch,
                        error = %err,
                        "trunk queue poller: listPullRequests reconciliation failed",
                    );
                    break;
                }
            }
        }

        if let Some(runtime) = self.queues.get_mut(key) {
            runtime.next_reconcile_due_at = now + RECONCILE_INTERVAL;
            // Only advance the cursor past this window when every page of
            // it was actually examined — on failure, leaving the cursor
            // at its prior value (or `None`) means the next attempt
            // re-covers the window instead of silently skipping whatever
            // transition concluded inside it, which is the exact case
            // this backstop exists to catch.
            if succeeded {
                runtime.last_reconciled_since = Some(observed_at);
            }
        }
    }

    /// Account for a failed `getQueue`: back the queue off, and — once the
    /// failure run crosses [`UNREACHABLE_ATTENTION_AFTER`] — tell the
    /// operator once. A rejected token is called out immediately and
    /// separately: waiting 15 minutes to report "nothing can merge" would
    /// be the wrong trade once enforcement is on.
    async fn record_queue_failure(
        &mut self,
        ctx: &TrunkSweepContext<'_>,
        key: &QueueKey,
        members: &[ActiveTrunkMergeIntent],
        err: &TrunkError,
        now: Instant,
        outcome: &mut TrunkSweepOutcome,
    ) {
        outcome.probe_failures += 1;
        let is_auth = matches!(err, TrunkError::Auth(_));
        let (delay, file_unreachable, file_auth, failures) = {
            let Some(runtime) = self.queues.get_mut(key) else {
                return;
            };
            let failure = &mut runtime.failure;
            failure.consecutive_failures = failure.consecutive_failures.saturating_add(1);
            let delay = backoff_delay(failure.consecutive_failures);
            runtime.next_due_at = now + delay;

            let file_auth = is_auth && !failure.auth_attention_filed;
            failure.auth_attention_filed |= is_auth;

            let since = *failure.unreachable_since.get_or_insert(now);
            let file_unreachable = !failure.unreachable_attention_filed
                && now.saturating_duration_since(since) >= UNREACHABLE_ATTENTION_AFTER;
            failure.unreachable_attention_filed |= file_unreachable;
            (delay, file_unreachable, file_auth, failure.consecutive_failures)
        };

        tracing::warn!(
            repo = %key.repo,
            target_branch = %key.target_branch,
            consecutive_failures = failures,
            backoff_secs = delay.as_secs(),
            tracked = members.len(),
            error = %err,
            "trunk queue poller: getQueue failed; backing off",
        );

        if file_auth {
            let waiting = members.len();
            file_queue_attention(
                ctx,
                members,
                QueueAttention {
                    kind: TRUNK_TOKEN_REJECTED_ATTENTION_KIND,
                    title: format!("Trunk API token rejected — merges for {} are stalled", key.repo),
                    body: format!(
                        "Trunk rejected Boss's API token while polling the merge queue for `{}` \
                         (target branch `{}`): {err}\n\n\
                         {waiting} merge(s) are waiting on this queue and none of them can progress \
                         until the token is replaced — run `boss engine trunk set-token`, then \
                         `boss engine trunk status` to confirm. Boss never falls back to \
                         `gh pr merge` for a `trunk_queue` product, so nothing is merging around \
                         the queue in the meantime.",
                        key.repo, key.target_branch
                    ),
                },
                outcome,
            )
            .await;
        }
        if file_unreachable {
            let waiting = members.len();
            let minutes = UNREACHABLE_ATTENTION_AFTER.as_secs() / 60;
            file_queue_attention(
                ctx,
                members,
                QueueAttention {
                    kind: TRUNK_QUEUE_UNREACHABLE_ATTENTION_KIND,
                    title: format!("Trunk merge queue for {} is unreachable", key.repo),
                    body: format!(
                        "Boss has been unable to read the Trunk merge queue for `{}` (target branch \
                         `{}`) for over {minutes} minutes. Latest error: {err}\n\n\
                         {waiting} merge(s) are being tracked on this queue; their Merging-lane state \
                         is frozen at the last successful observation until Trunk responds again. \
                         The poller keeps retrying on a capped {cap}-minute backoff — no action is \
                         needed if this was a Trunk outage that has since recovered.",
                        key.repo,
                        key.target_branch,
                        cap = BACKOFF_CAP.as_secs() / 60,
                    ),
                },
                outcome,
            )
            .await;
        }
    }

    /// File (at most) one attention item per non-`RUNNING` episode, and
    /// re-arm when the queue recovers. A paused queue is a queue-level
    /// fact: reporting it per tracked card would be N copies of one
    /// problem.
    async fn reconcile_queue_state_attention(
        &mut self,
        ctx: &TrunkSweepContext<'_>,
        key: &QueueKey,
        members: &[ActiveTrunkMergeIntent],
        queue: &TrunkQueue,
        outcome: &mut TrunkSweepOutcome,
    ) {
        let should_file = {
            let Some(runtime) = self.queues.get_mut(key) else {
                return;
            };
            match &queue.state {
                TrunkQueueState::Running => {
                    runtime.queue_state_attention = None;
                    false
                }
                other => {
                    if runtime.queue_state_attention.as_ref() == Some(other) {
                        false
                    } else {
                        runtime.queue_state_attention = Some(other.clone());
                        true
                    }
                }
            }
        };
        if !should_file {
            return;
        }

        let state = String::from(queue.state.clone());
        let waiting = members.len();
        tracing::warn!(
            repo = %key.repo,
            target_branch = %key.target_branch,
            queue_state = %state,
            waiting,
            "trunk queue poller: queue is not RUNNING while merges are enqueued",
        );
        file_queue_attention(
            ctx,
            members,
            QueueAttention {
                kind: TRUNK_QUEUE_NOT_RUNNING_ATTENTION_KIND,
                title: format!("Trunk queue for {} is {state} — {waiting} merge(s) waiting", key.repo),
                body: format!(
                    "The Trunk merge queue for `{}` (target branch `{}`) reports state **{state}**, \
                     not `RUNNING`, while {waiting} Boss-tracked merge(s) are enqueued.\n\n\
                     Queue administration lives in the Trunk web app, not in Boss — resume or drain \
                     the queue there. Tracked entries stay in the Merging lane and resume progressing \
                     on their own once the queue is `RUNNING` again; no merge has been lost.",
                    key.repo, key.target_branch,
                ),
            },
            outcome,
        )
        .await;
    }
}

// ── Per-member reconciliation ─────────────────────────────────────────────

/// Write the Merging-UI columns for an entry still live in the queue.
async fn write_live_entry(
    ctx: &TrunkSweepContext<'_>,
    member: &ActiveTrunkMergeIntent,
    entry: &TrunkPullRequest,
    position: i64,
    queue_state: &TrunkQueueState,
    outcome: &mut TrunkSweepOutcome,
) {
    let state = String::from(entry.state.clone());
    // Seed `section_order` from whatever the last renumbering pass settled
    // on rather than this probe's raw `position` — see
    // `get_task_merge_queue_detail`'s doc comment for why re-deriving it
    // from `position` here would fight the renumbering pass on every
    // cycle once the two disagree.
    let previous_section_order = ctx
        .work_db
        .get_task_merge_queue_detail(&member.intent.work_item_id)
        .ok()
        .flatten()
        .as_deref()
        .and_then(|json| parse_stored_trunk_queue_detail(Some(json)))
        .and_then(|detail| detail.section_order);
    let detail = live_entry_detail_json(member, &state, position, queue_state, previous_section_order);
    match ctx.work_db.set_task_merge_queue_state(
        &member.intent.work_item_id,
        Some(MERGE_QUEUE_STATE_QUEUED),
        detail.as_deref(),
    ) {
        Ok(true) => {
            outcome.state_writes += 1;
            // The macOS app is push-only, so the write only reaches the
            // Merging lane via this event.
            ctx.publisher
                .publish_work_item_changed(
                    &member.product_id,
                    &member.intent.work_item_id,
                    "trunk_queue_state_updated",
                )
                .await;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %member.intent.work_item_id,
                ?err,
                "trunk queue poller: failed to write merge-queue columns",
            );
        }
    }
    record_observed_state(ctx, member, &state);
}

/// Build the `merge_queue_detail` blob for a live Trunk entry.
///
/// A superset of the GitHub path's `{position, state, enqueued_at,
/// section_order}` — `MergeQueueDetail.parse` in the macOS app reads keys
/// by name and ignores the rest, so `source`/`queue_state` are additive
/// and a pre-task-8 app renders position + clock rather than breaking.
///
/// `section_order` is the queue position: without it `mergingSection()`
/// sorts every Trunk card at `.max` and the lane is unordered.
/// `previous_section_order` — the value the last renumbering pass wrote,
/// if any — is carried forward as-is so this write doesn't reintroduce a
/// stale provisional value on every cycle; a brand-new entry with no
/// stored value yet falls back to `position`, corrected to the canonical
/// per-product ranking by [`renumber_trunk_merge_queue`] right after.
fn live_entry_detail_json(
    member: &ActiveTrunkMergeIntent,
    state: &str,
    position: i64,
    queue_state: &TrunkQueueState,
    previous_section_order: Option<i64>,
) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "source": "trunk",
        "state": state,
        "position": position,
        "enqueued_at": enqueued_at(member),
        "queue_state": String::from(queue_state.clone()),
        "section_order": previous_section_order.unwrap_or(position),
        // Additive, ignored by the macOS app's `MergeQueueDetail.parse`
        // (same forward-compat contract as `source`/`queue_state` above).
        // Carried so `renumber_trunk_merge_queue` can group members by the
        // Trunk queue `position` was actually observed in — see
        // `trunk_queue_sort_key`'s doc comment.
        "repo": member.intent.repo,
        "target_branch": member.intent.target_branch,
    }))
    .ok()
}

/// Fields pulled back out of a stored Trunk `merge_queue_detail` JSON blob
/// — the read-side counterpart to [`live_entry_detail_json`]'s write. Used
/// only by [`renumber_trunk_merge_queue`] to re-derive `section_order`
/// across every currently-queued Trunk member of a product while
/// preserving the rest of the blob's fields byte-for-byte in meaning
/// (`source`/`queue_state` included) — the reason this can't just call
/// `merge_poller::renumber_merge_queue` directly, whose rewritten blob
/// only knows the GitHub-native `{position, state, enqueued_at,
/// section_order}` shape and would silently drop `source`/`queue_state`.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct StoredTrunkQueueDetail {
    state: Option<String>,
    position: Option<i64>,
    enqueued_at: Option<String>,
    queue_state: Option<String>,
    section_order: Option<i64>,
    repo: Option<String>,
    target_branch: Option<String>,
}

/// Parse a stored `merge_queue_detail` blob as a Trunk entry, or `None` if
/// it isn't one (unparsable, or `source != "trunk"` — a GitHub-native row,
/// which `list_queued_merge_queue_members` returns indiscriminately since
/// `merge_queue_state = "queued"` is shared between the two mechanisms).
fn parse_stored_trunk_queue_detail(json: Option<&str>) -> Option<StoredTrunkQueueDetail> {
    let parsed: serde_json::Value = json.and_then(|s| serde_json::from_str(s).ok())?;
    if parsed.get("source").and_then(|v| v.as_str()) != Some("trunk") {
        return None;
    }
    Some(StoredTrunkQueueDetail {
        state: parsed.get("state").and_then(|v| v.as_str()).map(str::to_owned),
        position: parsed.get("position").and_then(|v| v.as_i64()),
        enqueued_at: parsed.get("enqueued_at").and_then(|v| v.as_str()).map(str::to_owned),
        queue_state: parsed.get("queue_state").and_then(|v| v.as_str()).map(str::to_owned),
        section_order: parsed.get("section_order").and_then(|v| v.as_i64()),
        repo: parsed.get("repo").and_then(|v| v.as_str()).map(str::to_owned),
        target_branch: parsed.get("target_branch").and_then(|v| v.as_str()).map(str::to_owned),
    })
}

/// Sort key for [`renumber_trunk_merge_queue`]: group by the `(repo,
/// target_branch)` queue the member's `position` was last observed in,
/// then order ascending by that `position` within the group. A product
/// can have intents against more than one Trunk queue at once (two repos,
/// or two target branches on one repo) — each queue's `position` is only
/// a 1-based index into *that* queue, so two members from different
/// queues can both legitimately report `position: 1`. Grouping by queue
/// first keeps those from interleaving; within one queue there is no
/// cross-probe staleness to correct for (`merge_poller::merge_queue_sort_key`
/// has to handle that, this doesn't) because every tracked member of one
/// Trunk queue is rewritten together from the same `getQueue` response
/// (`probe_queue`'s per-member loop), so `position` is always as fresh as
/// this pass itself. A missing queue identity or position (a member not
/// yet observed this pass) sorts after every present value; `task_id`
/// breaks ties deterministically.
fn trunk_queue_sort_key(detail: &StoredTrunkQueueDetail, task_id: &str) -> (u8, String, String, u8, i64, String) {
    let (queue_rank, repo, target_branch) = match (&detail.repo, &detail.target_branch) {
        (Some(repo), Some(target_branch)) => (0u8, repo.clone(), target_branch.clone()),
        _ => (1u8, String::new(), String::new()),
    };
    match detail.position {
        Some(position) => (queue_rank, repo, target_branch, 0, position, task_id.to_owned()),
        None => (queue_rank, repo, target_branch, 1, i64::MAX, task_id.to_owned()),
    }
}

/// Recompute a canonical `section_order` for every Trunk-sourced member of
/// `product_id` currently in the Merging lane (`merge_queue_state =
/// "queued"`), and re-broadcast for every member whose value actually
/// changed — the Trunk-shaped mirror of
/// [`crate::merge_poller::renumber_merge_queue`] the design calls for
/// (`trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`
/// §"Queue state ingestion: polling").
///
/// `section_order` is not optional chrome: the macOS app's
/// `mergingSection()` sorts the lane by `MergeQueueDetail.parse(...)?
/// .sectionOrder ?? .max`, so a detail JSON without it ties every Trunk
/// card at `.max` and the lane goes unordered. Called once per product at
/// the end of every [`TrunkQueueProbe::run_pass`], after every queue that
/// product's members belong to has been probed, so it always sees this
/// pass's freshest positions.
async fn renumber_trunk_merge_queue(ctx: &TrunkSweepContext<'_>, product_id: &str, outcome: &mut TrunkSweepOutcome) {
    let renumber_ctx = crate::merge_queue_renumber::RenumberContext {
        work_db: ctx.work_db,
        publisher: ctx.publisher,
        product_id,
        log_context: "trunk queue poller",
        event: "trunk_queue_renumbered",
    };
    let writes = crate::merge_queue_renumber::renumber_section_order(
        &renumber_ctx,
        |member| {
            let detail = parse_stored_trunk_queue_detail(member.merge_queue_detail.as_deref())?;
            Some((member.task_id, detail))
        },
        trunk_queue_sort_key,
        // "Only touch what changed" — same invariant as
        // `merge_poller::renumber_merge_queue`.
        |detail, section_order| detail.section_order == Some(section_order),
        |detail, section_order| {
            serde_json::to_string(&serde_json::json!({
                "source": "trunk",
                "state": detail.state,
                "position": detail.position,
                "enqueued_at": detail.enqueued_at,
                "queue_state": detail.queue_state,
                "section_order": section_order,
                "repo": detail.repo,
                "target_branch": detail.target_branch,
            }))
            .ok()
        },
    )
    .await;
    outcome.state_writes += writes;
}

/// RFC 3339 rendering of when Boss submitted this PR to the queue.
///
/// Trunk's PR object carries no enqueue timestamp (only `stateChangedAt`,
/// which churns on every transition), so the intent's own `created_at` is
/// the stable "joined the queue at" fact — and for a Boss-submitted entry
/// it is the accurate one. Rendered as RFC 3339 because that is what the
/// app documents the field as, and what the GitHub path writes.
fn enqueued_at(member: &ActiveTrunkMergeIntent) -> Option<String> {
    member
        .intent
        .created_at
        .parse::<i64>()
        .ok()
        .map(boss_engine_utils::iso8601::format_epoch_iso8601)
}

/// Resolve an intent whose PR is no longer in `enqueuedPullRequests` — one
/// `getSubmittedPullRequest` for the terminal state it left for. Returns
/// the observed state, if any, so the caller can still tier on it.
async fn resolve_missing_entry(
    ctx: &TrunkSweepContext<'_>,
    member: &ActiveTrunkMergeIntent,
    repo: &TrunkRepoRef,
    target_branch: &str,
    outcome: &mut TrunkSweepOutcome,
) -> Option<TrunkPrState> {
    outcome.entry_lookups += 1;
    let lookup = TrunkPrLookup::new(
        repo.clone(),
        TrunkPrRef::new(member.intent.pr_number as u64),
        target_branch.to_owned(),
    );
    match ctx.api.get_submitted_pull_request(&lookup).await {
        Ok(pr) => {
            apply_resolved_state(ctx, member, &pr.state, outcome).await;
            Some(pr.state)
        }
        Err(TrunkError::NotFound(detail)) => {
            // Trunk has no record of this PR on this branch. Not a
            // terminal state — retiring the intent on it would discard a
            // human's merge approval over an entry that may simply not be
            // visible yet. Left `active`; the task-terminal-status sweep
            // above is what stops this repeating forever.
            tracing::debug!(
                work_item_id = %member.intent.work_item_id,
                pr_number = member.intent.pr_number,
                detail,
                "trunk queue poller: PR absent from the queue and unknown to getSubmittedPullRequest",
            );
            None
        }
        Err(err) => {
            outcome.probe_failures += 1;
            tracing::warn!(
                work_item_id = %member.intent.work_item_id,
                pr_number = member.intent.pr_number,
                error = %err,
                "trunk queue poller: getSubmittedPullRequest failed; retrying next cycle",
            );
            None
        }
    }
}

/// Apply one observed Trunk state to an intent, routing the terminal ones.
async fn apply_resolved_state(
    ctx: &TrunkSweepContext<'_>,
    member: &ActiveTrunkMergeIntent,
    state: &TrunkPrState,
    outcome: &mut TrunkSweepOutcome,
) {
    let state_str = String::from(state.clone());
    record_observed_state(ctx, member, &state_str);
    match state {
        // Terminal detection stays GitHub-side: the existing probe sees
        // the merged PR and runs the whole `mark_merged()` cascade, which
        // also clears the Merging-lane columns. All that is owed here is
        // retiring the intent so its dedup slot is freed.
        TrunkPrState::Merged => retire_intent(ctx, member, "merged", false, outcome).await,
        // A human ran `/trunk cancel`, the queue was drained, or Boss
        // cancelled the entry. Cancellation is a decision, not a failure:
        // no revision is spawned, the card just returns to Review.
        TrunkPrState::Cancelled => retire_intent(ctx, member, "cancelled", true, outcome).await,
        // Eviction. Recorded and left `active` on purpose — the
        // `ci_watch` eviction path (design task 6) owns the remediation,
        // and the intent is what authorizes the resubmit after it.
        TrunkPrState::Failed | TrunkPrState::PendingFailure => {
            tracing::info!(
                work_item_id = %member.intent.work_item_id,
                pr_url = %member.intent.pr_url,
                state = %state_str,
                "trunk queue poller: entry left the queue on a test failure; intent kept active for remediation",
            );
        }
        // A live state observed while the entry was missing from the queue
        // snapshot (a race between the two calls). Nothing to resolve —
        // the next cycle's `getQueue` reports it with a position again.
        _ => {}
    }
}

/// Persist the observed state on the intent, logging real transitions.
fn record_observed_state(ctx: &TrunkSweepContext<'_>, member: &ActiveTrunkMergeIntent, state: &str) {
    match ctx.work_db.record_trunk_merge_intent_state(&member.intent.id, state) {
        Ok(true) => tracing::info!(
            work_item_id = %member.intent.work_item_id,
            pr_url = %member.intent.pr_url,
            from = member.intent.last_trunk_state.as_deref().unwrap_or("-"),
            to = state,
            "trunk queue poller: trunk entry state changed",
        ),
        Ok(false) => {}
        Err(err) => tracing::warn!(
            intent_id = %member.intent.id,
            ?err,
            "trunk queue poller: failed to record trunk entry state",
        ),
    }
}

/// Retire an intent, optionally snapping its card back to Review.
///
/// `snap_back` clears the Merging-lane columns and files the "removed from
/// the queue" attention item; it is off for a merge, where the GitHub-side
/// merged observation owns the card's next move.
async fn retire_intent(
    ctx: &TrunkSweepContext<'_>,
    member: &ActiveTrunkMergeIntent,
    status: &str,
    snap_back: bool,
    outcome: &mut TrunkSweepOutcome,
) {
    match ctx.work_db.retire_trunk_merge_intent(&member.intent.id, status) {
        // `false` means another pass already retired it — the guard is what
        // keeps the snap-back and its attention item single-shot.
        Ok(false) => return,
        Ok(true) => {}
        Err(err) => {
            tracing::warn!(
                intent_id = %member.intent.id,
                ?err,
                "trunk queue poller: failed to retire merge intent",
            );
            return;
        }
    }
    outcome.intents_retired += 1;
    tracing::info!(
        work_item_id = %member.intent.work_item_id,
        pr_url = %member.intent.pr_url,
        status,
        "trunk queue poller: merge intent retired",
    );
    if !snap_back {
        return;
    }

    match ctx
        .work_db
        .set_task_merge_queue_state(&member.intent.work_item_id, None, None)
    {
        Ok(true) => {
            outcome.state_writes += 1;
            ctx.publisher
                .publish_work_item_changed(
                    &member.product_id,
                    &member.intent.work_item_id,
                    "trunk_queue_entry_cancelled",
                )
                .await;
        }
        Ok(false) => {}
        Err(err) => tracing::warn!(
            work_item_id = %member.intent.work_item_id,
            ?err,
            "trunk queue poller: failed to clear merge-queue columns after cancellation",
        ),
    }

    file_attention(
        ctx,
        &member.product_id,
        &member.intent.work_item_id,
        QueueAttention {
            kind: TRUNK_QUEUE_ENTRY_CANCELLED_ATTENTION_KIND,
            title: "PR was removed from the Trunk merge queue".to_owned(),
            body: format!(
                "{} left the Trunk merge queue as `cancelled` — a human ran `/trunk cancel`, the queue \
                 was drained, or Boss cancelled the entry.\n\n\
                 Cancellation is treated as a decision, not a failure: **no fix revision was spawned** \
                 and no CI attempt budget was consumed. The card is back in Review; click Merge again \
                 to resubmit it to the queue.",
                member.intent.pr_url
            ),
        },
        outcome,
    )
    .await;
}

/// Retire an intent whose task already reached a terminal status.
///
/// Deliberately synchronous and silent: the card has left the review
/// lifecycle, so there is nothing to publish or draw attention to — this
/// only stops the poller from chasing a moot entry forever and frees the
/// work item's dedup slot.
fn retire_moot_intent(ctx: &TrunkSweepContext<'_>, member: &ActiveTrunkMergeIntent, outcome: &mut TrunkSweepOutcome) {
    // `done` is overwhelmingly "the PR merged" (the merge poller's own
    // terminal path); `archived` is an operator retiring the card.
    let status = if member.task_status == "done" {
        "merged"
    } else {
        "cancelled"
    };
    match ctx.work_db.retire_trunk_merge_intent(&member.intent.id, status) {
        Ok(true) => {
            outcome.intents_retired += 1;
            tracing::info!(
                work_item_id = %member.intent.work_item_id,
                task_status = %member.task_status,
                status,
                "trunk queue poller: retired a merge intent whose task is already terminal",
            );
        }
        Ok(false) => {}
        Err(err) => tracing::warn!(
            intent_id = %member.intent.id,
            ?err,
            "trunk queue poller: failed to retire a moot merge intent",
        ),
    }
}

// ── Attention items ───────────────────────────────────────────────────────

/// The varying part of an attention item this module files.
struct QueueAttention {
    kind: &'static str,
    title: String,
    body: String,
}

/// File a queue-level attention item against the queue's anchor member —
/// the oldest active intent on it (`list_active_trunk_merge_intents`
/// orders by `created_at`, so the anchor is stable across sweeps).
/// Attention items must attach to a work item or an execution, and a queue
/// is neither.
async fn file_queue_attention(
    ctx: &TrunkSweepContext<'_>,
    members: &[ActiveTrunkMergeIntent],
    attention: QueueAttention,
    outcome: &mut TrunkSweepOutcome,
) {
    let Some(anchor) = members.first() else {
        return;
    };
    file_attention(ctx, &anchor.product_id, &anchor.intent.work_item_id, attention, outcome).await;
}

async fn file_attention(
    ctx: &TrunkSweepContext<'_>,
    product_id: &str,
    work_item_id: &str,
    attention: QueueAttention,
    outcome: &mut TrunkSweepOutcome,
) {
    match ctx.work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: None,
        work_item_id: Some(work_item_id.to_owned()),
        kind: attention.kind.to_owned(),
        status: None,
        title: attention.title,
        body_markdown: attention.body,
        resolved_at: None,
    }) {
        Ok(item) => {
            outcome.attentions_filed += 1;
            ctx.publisher
                .publish_frontend_event_on_product(product_id, FrontendEvent::AttentionItemCreated { item })
                .await;
        }
        Err(err) => tracing::warn!(
            work_item_id,
            kind = attention.kind,
            ?err,
            "trunk queue poller: failed to file attention item",
        ),
    }
}

#[cfg(test)]
mod tests;
