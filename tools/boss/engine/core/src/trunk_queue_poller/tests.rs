//! Unit tests for the Trunk queue poller.
//!
//! Every test drives [`TrunkQueueProbe::run_pass`] directly with an
//! injected `now`, so cadence tiers, the backoff ladder, and the
//! 15-minute unreachable threshold are all exercised without sleeping and
//! without a mock HTTP server ([`StubTrunkApi`] stands in for the
//! transport).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use boss_trunk_client::TrunkReadiness;

use super::*;
use crate::test_support::{RecordingPublisher, create_test_chore_manual, create_test_product_named};
use crate::work::{TrunkMergeIntentInsertInput, WorkItemPatch};

const REPO: &str = "brianduff/flunge";

// ── Test doubles ──────────────────────────────────────────────────────────

/// Canned failures, since [`TrunkError`] is not `Clone` and a stub reply
/// may be replayed across several passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StubError {
    Auth,
    Unavailable,
    NotFound,
}

impl StubError {
    fn into_trunk(self) -> TrunkError {
        match self {
            StubError::Auth => TrunkError::Auth("token rejected".to_owned()),
            StubError::Unavailable => TrunkError::QueueUnavailable("trunk returned 503".to_owned()),
            StubError::NotFound => TrunkError::NotFound("pr not queued".to_owned()),
        }
    }
}

#[derive(Default, bon::Builder)]
struct StubTrunkApi {
    /// Replies for successive `getQueue` calls. The last entry is sticky,
    /// so a test that only cares about one shape can enqueue it once and
    /// run as many passes as it likes.
    queue_replies: Mutex<VecDeque<Result<TrunkQueue, StubError>>>,
    entry_replies: Mutex<HashMap<u64, Result<TrunkPullRequest, StubError>>>,
    /// Replies for successive `listPullRequests` calls, same sticky-last
    /// convention as `queue_replies`. Empty by default (no reconciliation
    /// hits), tracked separately from `queue_calls`/`entry_calls` below.
    list_pull_requests_replies: Mutex<VecDeque<Result<ListPullRequestsResponse, StubError>>>,
    queue_calls: Mutex<Vec<(String, String)>>,
    entry_calls: Mutex<Vec<u64>>,
    /// `(since, cursor)` for every `listPullRequests` call, in order —
    /// lets a test assert directly on whether the reconciliation
    /// backstop advanced its cursor past a failed window, and whether it
    /// followed `next_cursor` into a second page.
    list_pull_requests_calls: Mutex<Vec<(Option<String>, Option<String>)>>,
    /// Replies for successive `submitPullRequest` calls, sticky-last like
    /// `queue_replies`. `Ok(())` by default (no reply queued).
    submit_replies: Mutex<VecDeque<Result<(), StubError>>>,
    /// `(owner/name, pr_number, target_branch)` for every `submitPullRequest`
    /// call, in order.
    submit_calls: Mutex<Vec<(String, u64, String)>>,
    /// Replies for successive `cancelPullRequest` calls, sticky-last like
    /// `queue_replies`. `Ok(())` by default.
    cancel_replies: Mutex<VecDeque<Result<(), StubError>>>,
    /// `(owner/name, pr_number, target_branch)` for every `cancelPullRequest`
    /// call, in order.
    cancel_calls: Mutex<Vec<(String, u64, String)>>,
}

impl StubTrunkApi {
    fn with_queue(replies: Vec<Result<TrunkQueue, StubError>>) -> Self {
        Self {
            queue_replies: Mutex::new(replies.into()),
            ..Self::default()
        }
    }

    fn set_entry(&self, pr_number: u64, reply: Result<TrunkPullRequest, StubError>) {
        self.entry_replies.lock().unwrap().insert(pr_number, reply);
    }

    fn set_list_pull_requests(&self, replies: Vec<Result<ListPullRequestsResponse, StubError>>) {
        *self.list_pull_requests_replies.lock().unwrap() = replies.into();
    }

    fn queue_call_count(&self) -> usize {
        self.queue_calls.lock().unwrap().len()
    }

    fn entry_call_count(&self) -> usize {
        self.entry_calls.lock().unwrap().len()
    }

    fn list_pull_requests_calls(&self) -> Vec<(Option<String>, Option<String>)> {
        self.list_pull_requests_calls.lock().unwrap().clone()
    }

    fn set_submit_replies(&self, replies: Vec<Result<(), StubError>>) {
        *self.submit_replies.lock().unwrap() = replies.into();
    }

    fn submit_call_count(&self) -> usize {
        self.submit_calls.lock().unwrap().len()
    }

    fn set_cancel_replies(&self, replies: Vec<Result<(), StubError>>) {
        *self.cancel_replies.lock().unwrap() = replies.into();
    }

    fn cancel_call_count(&self) -> usize {
        self.cancel_calls.lock().unwrap().len()
    }
}

#[async_trait]
impl TrunkQueueApi for StubTrunkApi {
    async fn get_queue(&self, request: &GetQueueRequest) -> Result<TrunkQueue, TrunkError> {
        self.queue_calls.lock().unwrap().push((
            format!("{}/{}", request.repo.owner, request.repo.name),
            request.target_branch.clone(),
        ));
        let mut replies = self.queue_replies.lock().unwrap();
        let reply = if replies.len() > 1 {
            replies.pop_front()
        } else {
            replies.front().cloned()
        };
        match reply {
            Some(Ok(queue)) => Ok(queue),
            Some(Err(err)) => Err(err.into_trunk()),
            None => Ok(queue_of(TrunkQueueState::Running, Vec::new())),
        }
    }

    async fn get_submitted_pull_request(&self, request: &TrunkPrLookup) -> Result<TrunkPullRequest, TrunkError> {
        self.entry_calls.lock().unwrap().push(request.pr.number);
        match self.entry_replies.lock().unwrap().get(&request.pr.number) {
            Some(Ok(pr)) => Ok(pr.clone()),
            Some(Err(err)) => Err(err.into_trunk()),
            None => Err(TrunkError::NotFound("no stub reply".to_owned())),
        }
    }

    async fn list_pull_requests(
        &self,
        request: &ListPullRequestsRequest,
    ) -> Result<ListPullRequestsResponse, TrunkError> {
        self.list_pull_requests_calls
            .lock()
            .unwrap()
            .push((request.since.clone(), request.cursor.clone()));
        let mut replies = self.list_pull_requests_replies.lock().unwrap();
        let reply = if replies.len() > 1 {
            replies.pop_front()
        } else {
            replies.front().cloned()
        };
        match reply {
            Some(Ok(response)) => Ok(response),
            Some(Err(err)) => Err(err.into_trunk()),
            None => Ok(ListPullRequestsResponse {
                pull_requests: Vec::new(),
                next_cursor: None,
            }),
        }
    }

    async fn submit_pull_request(&self, request: &SubmitPullRequestRequest) -> Result<(), TrunkError> {
        self.submit_calls.lock().unwrap().push((
            format!("{}/{}", request.repo.owner, request.repo.name),
            request.pr.number,
            request.target_branch.clone(),
        ));
        let mut replies = self.submit_replies.lock().unwrap();
        let reply = if replies.len() > 1 {
            replies.pop_front()
        } else {
            replies.front().cloned()
        };
        match reply {
            Some(Ok(())) => Ok(()),
            Some(Err(err)) => Err(err.into_trunk()),
            None => Ok(()),
        }
    }

    async fn cancel_pull_request(&self, request: &TrunkPrLookup) -> Result<(), TrunkError> {
        self.cancel_calls.lock().unwrap().push((
            format!("{}/{}", request.repo.owner, request.repo.name),
            request.pr.number,
            request.target_branch.clone(),
        ));
        let mut replies = self.cancel_replies.lock().unwrap();
        let reply = if replies.len() > 1 {
            replies.pop_front()
        } else {
            replies.front().cloned()
        };
        match reply {
            Some(Ok(())) => Ok(()),
            Some(Err(err)) => Err(err.into_trunk()),
            None => Ok(()),
        }
    }
}

/// Stands in for the two shell-outs [`CliEvictionEvidence`] performs (`bk`
/// and `gh`). Every test uses one, so no poller test ever spawns a
/// subprocess or touches the network — with a real authenticated `gh` on a
/// developer's PATH the production impl would otherwise issue live API calls
/// against whatever repo/PR number the fixture happens to name.
#[derive(Default)]
struct StubEvictionEvidence {
    /// Returned verbatim from `failing_construction_build`. Empty by default
    /// — the common case, since Trunk usually leaves no build behind.
    failing_jobs: Vec<RequiredCheckFailure>,
    /// Returned from `newest_trunk_bot_comment`. `None` by default.
    bot_comment: Option<String>,
    /// Calls to `newest_trunk_bot_comment`, so a test can assert the
    /// classifier short-circuits on the structured signal instead of
    /// reaching for the comment.
    bot_comment_calls: Mutex<usize>,
}

impl StubEvictionEvidence {
    fn with_failing_job() -> Self {
        Self {
            failing_jobs: vec![RequiredCheckFailure {
                name: "Trunk merge queue: flunge-ci".to_owned(),
                conclusion: "failure".to_owned(),
                target_url: "https://buildkite.com/flunge/flunge-ci/builds/2364#job-uuid".to_owned(),
                provider: crate::merge_poller::CiProvider::Buildkite,
                provider_job_id: Some("job-uuid".to_owned()),
            }],
            ..Self::default()
        }
    }

    fn with_bot_comment(body: &str) -> Self {
        Self {
            bot_comment: Some(body.to_owned()),
            ..Self::default()
        }
    }

    fn bot_comment_call_count(&self) -> usize {
        *self.bot_comment_calls.lock().unwrap()
    }
}

#[async_trait]
impl TrunkEvictionEvidence for StubEvictionEvidence {
    async fn failing_construction_build(&self, _pr_number: u64) -> Vec<RequiredCheckFailure> {
        self.failing_jobs.clone()
    }

    async fn newest_trunk_bot_comment(&self, _repo: &str, _pr_number: i64) -> Option<String> {
        *self.bot_comment_calls.lock().unwrap() += 1;
        self.bot_comment.clone()
    }
}

/// A probe with no eviction evidence available — the default for tests that
/// are not about eviction classification at all.
fn probe() -> TrunkQueueProbe {
    TrunkQueueProbe::with_evidence(Arc::new(StubEvictionEvidence::default()))
}

/// A probe wired to `evidence`, kept alive by the returned `Arc` so a test
/// can assert on the stub's call counters afterwards.
fn probe_with(evidence: Arc<StubEvictionEvidence>) -> TrunkQueueProbe {
    TrunkQueueProbe::with_evidence(evidence)
}

// ── Fixtures ──────────────────────────────────────────────────────────────

fn entry_of(pr_number: u64, state: TrunkPrState) -> TrunkPullRequest {
    TrunkPullRequest::builder()
        .id(format!("entry_{pr_number}"))
        .state(state)
        .pr_number(pr_number)
        .build()
}

/// Like [`entry_of`], but with `state_changed_at` set — the field
/// `handle_trunk_queue_eviction` needs to key the `ci_watch` remediation
/// (see [`an_evicted_entry_with_state_changed_at_triggers_ci_watch`]).
/// Kept separate from `entry_of` rather than adding an optional parameter
/// there, since most existing fixtures don't care about this field.
fn entry_of_with_state_changed_at(pr_number: u64, state: TrunkPrState, state_changed_at: &str) -> TrunkPullRequest {
    TrunkPullRequest::builder()
        .id(format!("entry_{pr_number}"))
        .state(state)
        .pr_number(pr_number)
        .state_changed_at(state_changed_at)
        .build()
}

/// An evicted entry carrying Trunk's `readiness` block — the structured
/// channel `classify_trunk_eviction_from_payload` reads.
fn evicted_entry_with_readiness(pr_number: u64, readiness: TrunkReadiness) -> TrunkPullRequest {
    TrunkPullRequest::builder()
        .id(format!("entry_{pr_number}"))
        .state(TrunkPrState::Failed)
        .pr_number(pr_number)
        .state_changed_at("2026-07-25T03:54:24.000Z")
        .readiness(readiness)
        .build()
}

/// The `readiness` shape observed on a healthy entry against
/// `github.com/brianduff/flunge`.
fn healthy_readiness() -> TrunkReadiness {
    TrunkReadiness {
        has_impacted_targets: false,
        requires_impacted_targets: false,
        does_base_branch_match: true,
        git_hub_mergeability: "mergeable".to_owned(),
    }
}

fn queue_of(state: TrunkQueueState, entries: Vec<TrunkPullRequest>) -> TrunkQueue {
    TrunkQueue::builder()
        .state(state)
        .branch("main")
        .enqueued_pull_requests(entries)
        .build()
}

fn pr_url(pr_number: i64) -> String {
    format!("https://github.com/{REPO}/pull/{pr_number}")
}

/// An `in_review` task with an active Trunk merge intent for `pr_number`.
/// Returns `(product_id, task_id)`.
fn seed_intent(db: &WorkDb, name: &str, pr_number: i64) -> (String, String) {
    seed_intent_on(db, name, pr_number, REPO, "main")
}

fn seed_intent_on(db: &WorkDb, name: &str, pr_number: i64, repo: &str, target_branch: &str) -> (String, String) {
    let product = create_test_product_named(db, &format!("Product-{name}"));
    let task = create_test_chore_manual(db, product.id.clone(), name);
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url(pr_number)),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.insert_trunk_merge_intent(
        TrunkMergeIntentInsertInput::builder()
            .work_item_id(task.id.clone())
            .pr_url(pr_url(pr_number))
            .pr_number(pr_number)
            .repo(repo)
            .target_branch(target_branch)
            .build(),
    )
    .unwrap()
    .unwrap();
    (product.id, task.id)
}

fn stored_queue_state(db: &WorkDb, task_id: &str) -> (Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
            rusqlite::params![task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn stored_detail(db: &WorkDb, task_id: &str) -> serde_json::Value {
    let (_, detail) = stored_queue_state(db, task_id);
    serde_json::from_str(&detail.expect("a detail blob was written")).unwrap()
}

fn attention_kinds(db: &WorkDb, task_id: &str) -> Vec<String> {
    db.list_attention_items_for_work_item(task_id)
        .unwrap()
        .into_iter()
        .map(|item| item.kind)
        .collect()
}

// ── Cadence / scheduling ──────────────────────────────────────────────────

#[test]
fn backoff_ladder_doubles_from_thirty_seconds_and_caps_at_five_minutes() {
    assert_eq!(backoff_delay(1), Duration::from_secs(30));
    assert_eq!(backoff_delay(2), Duration::from_secs(60));
    assert_eq!(backoff_delay(3), Duration::from_secs(120));
    assert_eq!(backoff_delay(4), Duration::from_secs(240));
    assert_eq!(backoff_delay(5), Duration::from_secs(300));
    assert_eq!(backoff_delay(99), Duration::from_secs(300));
}

#[test]
fn tier_intervals_match_the_design() {
    assert_eq!(TrunkPollTier::Testing.interval(), Duration::from_secs(15));
    assert_eq!(TrunkPollTier::Pending.interval(), Duration::from_secs(30));
}

#[test]
fn an_idle_probe_still_wakes_to_discover_new_intents() {
    let probe = probe();
    let now = Instant::now();
    assert_eq!(probe.next_wake_at(now), now + IDLE_RESCAN_INTERVAL);
}

#[tokio::test]
async fn no_active_intents_means_no_trunk_traffic() {
    let (_tmp, db) = crate::test_support::open_db();
    let api = StubTrunkApi::default();
    let publisher = RecordingPublisher::default();
    let mut probe = probe();

    let outcome = probe
        .run_pass(
            &TrunkSweepContext {
                work_db: &db,
                publisher: &publisher,
                api: &api,
            },
            Instant::now(),
        )
        .await;

    assert_eq!(outcome, TrunkSweepOutcome::default());
    assert_eq!(api.queue_call_count(), 0, "idle means no getQueue calls at all");
}

#[tokio::test]
async fn intents_sharing_a_repo_and_branch_cost_one_get_queue_call() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent(&db, "first", 978);
    seed_intent(&db, "second", 979);
    // A third intent on a different target branch is a separate queue.
    seed_intent_on(&db, "third", 980, REPO, "release");

    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![
            entry_of(978, TrunkPrState::Pending),
            entry_of(979, TrunkPrState::Testing),
        ],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();

    let outcome = probe
        .run_pass(
            &TrunkSweepContext {
                work_db: &db,
                publisher: &publisher,
                api: &api,
            },
            Instant::now(),
        )
        .await;

    assert_eq!(outcome.queues_probed, 2, "one probe per (repo, target_branch)");
    assert_eq!(api.queue_call_count(), 2);
    let calls = api.queue_calls.lock().unwrap().clone();
    assert!(calls.contains(&(REPO.to_owned(), "main".to_owned())));
    assert!(calls.contains(&(REPO.to_owned(), "release".to_owned())));
}

#[tokio::test]
async fn a_queue_is_not_re_probed_before_its_tier_interval_elapses() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent(&db, "pending", 978);
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![entry_of(978, TrunkPrState::Pending)],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    assert_eq!(api.queue_call_count(), 1);

    // Pending tier is 30s: a pass at +29s must not spend a request.
    probe.run_pass(&ctx, t0 + Duration::from_secs(29)).await;
    assert_eq!(api.queue_call_count(), 1);

    probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;
    assert_eq!(api.queue_call_count(), 2);
}

#[tokio::test]
async fn a_testing_entry_pulls_the_queue_onto_the_fifteen_second_tier() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent(&db, "testing", 978);
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![entry_of(978, TrunkPrState::Testing)],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    probe.run_pass(&ctx, t0 + Duration::from_secs(14)).await;
    assert_eq!(api.queue_call_count(), 1, "still inside the 15s tier");

    probe.run_pass(&ctx, t0 + Duration::from_secs(16)).await;
    assert_eq!(api.queue_call_count(), 2);
}

// ── State writes ──────────────────────────────────────────────────────────

#[tokio::test]
async fn writes_queued_state_with_a_one_based_position_and_section_order() {
    let (_tmp, db) = crate::test_support::open_db();
    let (product_id, task_id) = seed_intent(&db, "queued", 979);
    // 979 is second in the queue -> position 2, not the array index 1.
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![
            entry_of(978, TrunkPrState::Testing),
            entry_of(979, TrunkPrState::Pending),
        ],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();

    let outcome = probe
        .run_pass(
            &TrunkSweepContext {
                work_db: &db,
                publisher: &publisher,
                api: &api,
            },
            Instant::now(),
        )
        .await;

    // One write from `write_live_entry` (position 2, the real queue index)
    // plus one from `renumber_trunk_merge_queue` — 978 isn't a Boss-tracked
    // member, so the product's only tracked entry re-ranks to section_order
    // 1 even though its raw `position` stays 2.
    assert_eq!(outcome.state_writes, 2);
    let (state, _) = stored_queue_state(&db, &task_id);
    assert_eq!(state.as_deref(), Some("queued"));

    let detail = stored_detail(&db, &task_id);
    assert_eq!(detail["source"], "trunk");
    assert_eq!(detail["state"], "pending");
    assert_eq!(detail["position"], 2);
    assert_eq!(detail["section_order"], 1);
    assert_eq!(detail["queue_state"], "running");
    // `enqueued_at` is RFC 3339, per the field's documented contract on
    // the app side, derived from when Boss submitted the PR.
    let enqueued_at = detail["enqueued_at"].as_str().expect("an enqueued_at timestamp");
    assert!(enqueued_at.contains('T') && enqueued_at.ends_with('Z'), "{enqueued_at}");

    // The card only reaches the Merging lane via a pushed event — one for
    // the live-entry write, one for the renumbering pass right after it.
    let events = publisher.events.lock().await.clone();
    assert_eq!(
        events,
        vec![
            (
                product_id.clone(),
                task_id.clone(),
                "trunk_queue_state_updated".to_owned()
            ),
            (product_id, task_id, "trunk_queue_renumbered".to_owned()),
        ]
    );

    // And the intent remembers what the queue said.
    let intent = db.list_active_trunk_merge_intents().unwrap().remove(0);
    assert_eq!(intent.intent.last_trunk_state.as_deref(), Some("pending"));
}

#[tokio::test]
async fn an_unchanged_queue_state_is_not_republished_every_cycle() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent(&db, "steady", 978);
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![entry_of(978, TrunkPrState::Pending)],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    let second = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;

    assert_eq!(second.queues_probed, 1, "the queue was re-probed");
    assert_eq!(second.state_writes, 0, "but nothing moved, so nothing was written");
    assert_eq!(publisher.events.lock().await.len(), 1);
}

// ── Terminal resolution ───────────────────────────────────────────────────

#[tokio::test]
async fn a_cancelled_entry_retires_the_intent_and_snaps_the_card_back_to_review() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "cancelled", 978);
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(978, TrunkPrState::Pending)],
        )),
        // Second pass: the entry has left the queue.
        Ok(queue_of(TrunkQueueState::Running, Vec::new())),
    ]);
    api.set_entry(978, Ok(entry_of(978, TrunkPrState::Cancelled)));
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    assert_eq!(stored_queue_state(&db, &task_id).0.as_deref(), Some("queued"));

    let outcome = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;

    assert_eq!(api.entry_call_count(), 1, "one lookup for the entry that vanished");
    assert_eq!(outcome.intents_retired, 1);
    assert_eq!(outcome.attentions_filed, 1);
    assert_eq!(
        stored_queue_state(&db, &task_id),
        (None, None),
        "the card must leave the Merging lane"
    );
    assert!(db.get_active_trunk_merge_intent(&task_id).unwrap().is_none());
    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![TRUNK_QUEUE_ENTRY_CANCELLED_ATTENTION_KIND.to_owned()]
    );

    // A third pass has no active intent left to chase.
    let third = probe.run_pass(&ctx, t0 + Duration::from_secs(120)).await;
    assert_eq!(third.queues_probed, 0);
    assert_eq!(third.intents_retired, 0);
}

#[tokio::test]
async fn a_merged_entry_retires_the_intent_but_leaves_the_columns_to_the_github_probe() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "merged", 978);
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(978, TrunkPrState::TestsPassed)],
        )),
        Ok(queue_of(TrunkQueueState::Running, Vec::new())),
    ]);
    api.set_entry(978, Ok(entry_of(978, TrunkPrState::Merged)));
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    let outcome = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;

    assert_eq!(outcome.intents_retired, 1);
    assert_eq!(outcome.attentions_filed, 0, "a merge is not an attention-worthy event");
    assert_eq!(
        stored_queue_state(&db, &task_id).0.as_deref(),
        Some("queued"),
        "the merged card stays in Merging until the GitHub probe runs mark_merged()",
    );
    assert!(db.get_active_trunk_merge_intent(&task_id).unwrap().is_none());
}

#[tokio::test]
async fn an_evicted_entry_keeps_its_intent_active_for_the_remediation_path() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "evicted", 978);
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(978, TrunkPrState::Testing)],
        )),
        Ok(queue_of(TrunkQueueState::Running, Vec::new())),
    ]);
    api.set_entry(978, Ok(entry_of(978, TrunkPrState::Failed)));
    let publisher = RecordingPublisher::default();
    // A failing construction build present: the test-failure arm, whose
    // behaviour must be unchanged by eviction-cause classification.
    let mut probe = probe_with(Arc::new(StubEvictionEvidence::with_failing_job()));
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    let outcome = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;

    assert_eq!(outcome.intents_retired, 0, "eviction is not a retirement");
    assert_eq!(outcome.attentions_filed, 0);
    let intent = db
        .get_active_trunk_merge_intent(&task_id)
        .unwrap()
        .expect("still active");
    assert_eq!(intent.last_trunk_state.as_deref(), Some("failed"));

    // The eviction is already resolved, so later cycles keep watching the
    // queue (a resubmit has to be noticed) without re-asking Trunk about
    // an entry whose terminal state cannot change while it is out of it.
    probe.run_pass(&ctx, t0 + Duration::from_secs(62)).await;
    probe.run_pass(&ctx, t0 + Duration::from_secs(93)).await;
    assert_eq!(api.entry_call_count(), 1);
    assert!(api.queue_call_count() >= 4);
}

/// An evicted entry that carries `stateChangedAt` must be handed to
/// `ci_watch::on_trunk_queue_eviction_detected`, which flips the owning
/// chore to `blocked: ci_failure` and records a `trunk_queue_eviction`
/// `ci_remediations` row.
///
/// The evidence channels are stubbed ([`StubEvictionEvidence`]) rather than
/// shelled out, so this exercises the whole poller → classify → `ci_watch`
/// wiring for the test-failure arm; the `bk`/`gh` invocations themselves stay
/// untested, mirroring the `fetch_and_store_log_excerpt` precedent.
#[tokio::test]
async fn an_evicted_entry_with_state_changed_at_triggers_ci_watch() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "evicted-ci-watch", 1007);
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(1007, TrunkPrState::Testing)],
        )),
        Ok(queue_of(TrunkQueueState::Running, Vec::new())),
    ]);
    api.set_entry(
        1007,
        Ok(entry_of_with_state_changed_at(
            1007,
            TrunkPrState::Failed,
            "2026-07-23T01:32:50.000Z",
        )),
    );
    let publisher = RecordingPublisher::default();
    let mut probe = probe_with(Arc::new(StubEvictionEvidence::with_failing_job()));
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    let outcome = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;

    assert_eq!(outcome.evictions_detected, 1);

    let task = match db.get_work_item(&task_id).unwrap() {
        crate::work::WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        task.status,
        crate::work::TaskStatus::InReview,
        "revision spawn unblocks the parent"
    );

    let attempt = db
        .active_ci_remediation_for_work_item(&task_id)
        .unwrap()
        .expect("active ci_remediations row");
    assert_eq!(attempt.failure_kind.as_deref(), Some("trunk_queue_eviction"));
    assert_eq!(attempt.head_sha_at_trigger, "trunk:entry_1007@2026-07-23T01:32:50.000Z");

    // A repeat sweep for the same (already-resolved) episode must not
    // fire a second time.
    probe.run_pass(&ctx, t0 + Duration::from_secs(62)).await;
    let conn = db.connect().unwrap();
    let attempt_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ci_remediations WHERE work_item_id = ?1",
            rusqlite::params![&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        attempt_count, 1,
        "an already-resolved eviction must not re-trigger ci_watch"
    );
}

// ── Eviction cause classification (T792/T793) ─────────────────────────────

/// Rows in `ci_remediations` for `task_id`, and the CI attempt budget it has
/// consumed. Both must stay at zero for every merge-side eviction: the whole
/// point is that a failure Trunk raised before testing never enters the CI
/// ledger.
fn ci_ledger_state(db: &WorkDb, task_id: &str) -> (i64, i64) {
    // Scoped so the connection guard is released before `get_ci_attempts_used`
    // asks `WorkDb` for one of its own — `connect()` is behind a mutex, so
    // holding one across that call self-deadlocks.
    let attempts: i64 = {
        let conn = db.connect().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM ci_remediations WHERE work_item_id = ?1",
            rusqlite::params![task_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    (attempts, db.get_ci_attempts_used(task_id).unwrap())
}

fn revision_count(db: &WorkDb, task_id: &str) -> i64 {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![task_id],
            |r| r.get(0),
        )
        .unwrap()
}

fn task_status_of(db: &WorkDb, task_id: &str) -> (crate::work::TaskStatus, Option<String>) {
    match db.get_work_item(task_id).unwrap() {
        crate::work::WorkItem::Chore(t) => (t.status, t.blocked_reason),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Drive one enqueue → eviction cycle and return the last pass's outcome.
async fn run_eviction(
    db: &WorkDb,
    pr_number: u64,
    evicted: TrunkPullRequest,
    evidence: Arc<StubEvictionEvidence>,
) -> TrunkSweepOutcome {
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(pr_number, TrunkPrState::Testing)],
        )),
        Ok(queue_of(TrunkQueueState::Running, Vec::new())),
    ]);
    api.set_entry(pr_number, Ok(evicted));
    let publisher = RecordingPublisher::default();
    let mut probe = probe_with(evidence);
    let ctx = TrunkSweepContext {
        work_db: db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();
    probe.run_pass(&ctx, t0).await;
    probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await
}

/// The T792/T793 bug, from the structured channel: Trunk evicted the PR
/// before testing because it no longer merges into the target branch. That
/// must not read as a CI failure — no `ci_remediations` row, no "Fix failing
/// CI" revision, no CI budget spent — and the slot must be handed to the
/// conflict resolver, which is what can actually fix it.
#[tokio::test]
async fn a_merge_conflict_eviction_is_handed_to_the_conflict_lane_not_to_ci() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "evicted-conflict", 1136);
    let evidence = Arc::new(StubEvictionEvidence::default());
    let entry = evicted_entry_with_readiness(
        1136,
        TrunkReadiness {
            git_hub_mergeability: "conflicting".to_owned(),
            ..healthy_readiness()
        },
    );

    let outcome = run_eviction(&db, 1136, entry, Arc::clone(&evidence)).await;

    assert_eq!(outcome.merge_side_evictions, 1);
    assert_eq!(
        outcome.evictions_detected, 0,
        "a merge failure must never reach the CI ledger",
    );
    assert_eq!(
        evidence.bot_comment_call_count(),
        0,
        "the structured readiness signal is conclusive; no `gh` round trip is owed",
    );

    assert_eq!(
        ci_ledger_state(&db, &task_id),
        (0, 0),
        "no ci_remediations row and no CI attempt consumed",
    );
    assert_eq!(revision_count(&db, &task_id), 0, "no 'Fix failing CI' revision");
    let (status, blocked_reason) = task_status_of(&db, &task_id);
    assert_eq!(status, crate::work::TaskStatus::InReview);
    assert_eq!(blocked_reason, None, "the row must not be flipped to ci_failure");

    // Ownership transferred, so `conflict_watch::on_resolved` can advance the
    // intent to `awaiting_resubmit` once the conflict is fixed. Left at
    // `failed` the PR would be remediated and then never re-queued.
    let intent = db
        .get_active_trunk_merge_intent(&task_id)
        .unwrap()
        .expect("intent stays active for the resubmit");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
    );

    let (queue_state, _) = stored_queue_state(&db, &task_id);
    assert_eq!(queue_state, None, "the card leaves the Merging lane");
    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![TRUNK_QUEUE_MERGE_FAILURE_ATTENTION_KIND.to_owned()],
        "the eviction is attributed, not silent",
    );
}

/// Same outcome via the corroborating channel: Trunk omitted `readiness`, so
/// the only positive reason signal is its bot comment — the verbatim string
/// it posted on flunge#1136 and #1137. Consulted only because the Buildkite
/// search also came back empty.
#[tokio::test]
async fn a_merge_conflict_eviction_is_recognised_from_the_trunk_bot_comment() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "evicted-bot-comment", 1137);
    let evidence = Arc::new(StubEvictionEvidence::with_bot_comment(
        "❌ This pull request could not start testing because there was a merge conflict.",
    ));
    let entry = entry_of_with_state_changed_at(1137, TrunkPrState::Failed, "2026-07-25T03:54:36.000Z");

    let outcome = run_eviction(&db, 1137, entry, Arc::clone(&evidence)).await;

    assert_eq!(outcome.merge_side_evictions, 1);
    assert_eq!(outcome.evictions_detected, 0);
    assert_eq!(evidence.bot_comment_call_count(), 1);
    assert_eq!(ci_ledger_state(&db, &task_id), (0, 0));
    assert_eq!(revision_count(&db, &task_id), 0);
    assert_eq!(
        db.get_active_trunk_merge_intent(&task_id)
            .unwrap()
            .expect("still active")
            .last_trunk_state
            .as_deref(),
        Some(TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
    );
}

/// A PR pointed at a branch this queue does not merge into. No rebase and no
/// code change fixes that, so nothing automated is attempted: the intent is
/// retired (freeing the slot for a re-merge after the operator retargets)
/// and the reason is filed.
#[tokio::test]
async fn a_base_branch_mismatch_eviction_retires_the_intent_with_no_remediation() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "evicted-wrong-base", 1200);
    let entry = TrunkPullRequest::builder()
        .id("entry_1200")
        .state(TrunkPrState::Failed)
        .pr_number(1200u64)
        .state_changed_at("2026-07-25T03:54:24.000Z")
        .pr_base_branch("release-2026")
        .build();

    let outcome = run_eviction(&db, 1200, entry, Arc::new(StubEvictionEvidence::default())).await;

    assert_eq!(outcome.merge_side_evictions, 1);
    assert_eq!(outcome.evictions_detected, 0);
    assert_eq!(outcome.intents_retired, 1);
    assert_eq!(ci_ledger_state(&db, &task_id), (0, 0));
    assert_eq!(revision_count(&db, &task_id), 0);
    assert!(
        db.get_active_trunk_merge_intent(&task_id).unwrap().is_none(),
        "no remediation exists, so the intent is not left waiting for one",
    );
    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![TRUNK_QUEUE_MERGE_FAILURE_ATTENTION_KIND.to_owned()],
    );
}

/// Neither a failing build nor any positive merge-side signal. The
/// classification falls through to the unchanged test-failure arm, and
/// `on_queue_side_failure_detected`'s empty-`failures` guard then refuses the
/// flip — an attempt with no failing check to name is one a worker cannot
/// act on, and dispatching one anyway is what destroyed flunge#1137.
#[tokio::test]
async fn an_eviction_with_no_evidence_at_all_does_not_flip_the_row() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "evicted-no-evidence", 1300);
    let entry = evicted_entry_with_readiness(1300, healthy_readiness());

    let outcome = run_eviction(&db, 1300, entry, Arc::new(StubEvictionEvidence::default())).await;

    assert_eq!(outcome.evictions_detected, 0);
    assert_eq!(outcome.merge_side_evictions, 0);
    assert_eq!(ci_ledger_state(&db, &task_id), (0, 0));
    assert_eq!(revision_count(&db, &task_id), 0);
    let (status, blocked_reason) = task_status_of(&db, &task_id);
    assert_eq!(status, crate::work::TaskStatus::InReview);
    assert_eq!(blocked_reason, None);
}

/// The sentinel a merge-side eviction writes must survive the
/// `listPullRequests` reconciliation backstop re-observing the same terminal
/// entry. `record_observed_state` would otherwise stamp `failed` back over
/// it, and `trunk_merge::needs_remediation` treats `failed` as "an eviction
/// episode owns this slot" — which would leave `conflict_watch::on_resolved`
/// unable to ever mark the intent `awaiting_resubmit`.
#[tokio::test]
async fn a_conflict_handover_sentinel_survives_the_reconciliation_backstop() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "evicted-sentinel", 1400);
    let evicted = evicted_entry_with_readiness(
        1400,
        TrunkReadiness {
            git_hub_mergeability: "conflicting".to_owned(),
            ..healthy_readiness()
        },
    );
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(1400, TrunkPrState::Testing)],
        )),
        Ok(queue_of(TrunkQueueState::Running, Vec::new())),
    ]);
    api.set_entry(1400, Ok(evicted.clone()));
    // The backstop re-reports the same terminal entry on its next window.
    api.set_list_pull_requests(vec![Ok(ListPullRequestsResponse {
        pull_requests: vec![evicted],
        next_cursor: None,
    })]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;
    // Past RECONCILE_INTERVAL, so the backstop runs.
    probe
        .run_pass(&ctx, t0 + RECONCILE_INTERVAL + Duration::from_secs(62))
        .await;

    assert_eq!(
        db.get_active_trunk_merge_intent(&task_id)
            .unwrap()
            .expect("still active")
            .last_trunk_state
            .as_deref(),
        Some(TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
        "the backstop must not clobber the handover sentinel",
    );
    assert_eq!(ci_ledger_state(&db, &task_id), (0, 0));
}

// ── Pure classification helpers ───────────────────────────────────────────

#[test]
fn build_evidence_outranks_a_conflicting_readiness_snapshot() {
    // `readiness` is a live snapshot with no episode attached: the target
    // branch moving after a genuine test failure must not retroactively
    // reclassify it and lose the failing build.
    let entry = evicted_entry_with_readiness(
        1,
        TrunkReadiness {
            git_hub_mergeability: "conflicting".to_owned(),
            ..healthy_readiness()
        },
    );
    assert_eq!(
        classify_trunk_eviction_from_payload(&entry, "main", true),
        Some(TrunkEvictionCause::TestFailure),
    );
}

#[test]
fn a_healthy_readiness_block_is_not_evidence_of_a_merge_failure() {
    let entry = evicted_entry_with_readiness(1, healthy_readiness());
    assert_eq!(classify_trunk_eviction_from_payload(&entry, "main", false), None);
}

#[test]
fn an_unrecognised_mergeability_value_is_inconclusive_not_conflicting() {
    for raw in ["unknown", "checking", "", "something_trunk_adds_later"] {
        let entry = evicted_entry_with_readiness(
            1,
            TrunkReadiness {
                git_hub_mergeability: raw.to_owned(),
                ..healthy_readiness()
            },
        );
        assert_eq!(
            classify_trunk_eviction_from_payload(&entry, "main", false),
            None,
            "{raw:?} must fall through to the unchanged test-failure arm",
        );
    }
}

#[test]
fn conflicting_mergeability_spellings_are_matched_case_insensitively() {
    for raw in ["conflicting", "CONFLICTING", "Dirty", "not_mergeable", "unmergeable"] {
        assert!(is_conflicting_mergeability(raw), "{raw:?}");
    }
    for raw in ["mergeable", "clean", "unknown", "behind", "blocked"] {
        assert!(!is_conflicting_mergeability(raw), "{raw:?}");
    }
}

#[test]
fn a_base_branch_mismatch_is_read_from_either_channel() {
    // From the entry's own `pr_base_branch`, with no `readiness` at all.
    let entry = TrunkPullRequest::builder()
        .id("e")
        .state(TrunkPrState::Failed)
        .pr_number(1u64)
        .pr_base_branch("release")
        .build();
    assert_eq!(
        classify_trunk_eviction_from_payload(&entry, "main", false),
        Some(TrunkEvictionCause::BaseBranchMismatch {
            pr_base: "release".to_owned()
        }),
    );

    // From `readiness`, when Trunk did not report the base branch.
    let entry = evicted_entry_with_readiness(
        1,
        TrunkReadiness {
            does_base_branch_match: false,
            ..healthy_readiness()
        },
    );
    assert_eq!(
        classify_trunk_eviction_from_payload(&entry, "main", false),
        Some(TrunkEvictionCause::BaseBranchMismatch {
            pr_base: "<unreported>".to_owned()
        }),
    );
}

#[test]
fn a_matching_base_branch_is_compared_case_insensitively() {
    let entry = TrunkPullRequest::builder()
        .id("e")
        .state(TrunkPrState::Failed)
        .pr_number(1u64)
        .pr_base_branch("Main")
        .readiness(healthy_readiness())
        .build();
    assert_eq!(classify_trunk_eviction_from_payload(&entry, "main", false), None);
}

#[test]
fn the_trunk_bot_eviction_phrase_is_recognised() {
    assert_eq!(
        bot_comment_merge_failure_marker(
            "❌ This pull request could not start testing because there was a merge conflict."
        ),
        Some("could not start testing"),
    );
    assert_eq!(
        bot_comment_merge_failure_marker("Blocked: this PR has a Merge Conflict with main."),
        Some("merge conflict"),
    );
    assert_eq!(
        bot_comment_merge_failure_marker("⏳ This pull request is queued for testing (position 3)."),
        None,
    );
}

#[test]
fn only_the_newest_trunk_bot_comment_is_read() {
    let body = serde_json::json!([
        {"user": {"login": "someone"}, "body": "merge conflict incoming, heads up"},
        {"user": {"login": "trunk-io[bot]"}, "body": "⏳ queued for testing"},
        {"user": {"login": "trunk-io[bot]"}, "body": "❌ could not start testing because there was a merge conflict"},
        {"user": {"login": "reviewer"}, "body": "lgtm"},
    ])
    .to_string();
    assert_eq!(
        newest_trunk_bot_comment(body.as_bytes()).as_deref(),
        Some("❌ could not start testing because there was a merge conflict"),
        "a human saying 'merge conflict' must not be mistaken for Trunk's verdict",
    );
}

#[test]
fn an_absent_or_unparseable_comment_payload_yields_nothing() {
    assert_eq!(newest_trunk_bot_comment(b"not json"), None);
    assert_eq!(newest_trunk_bot_comment(b"[]"), None);
    assert_eq!(
        newest_trunk_bot_comment(br#"[{"user":{"login":"human"},"body":"hi"}]"#),
        None,
    );
}

#[tokio::test]
async fn a_resubmitted_entry_returns_to_the_merging_lane() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "resubmitted", 978);
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(978, TrunkPrState::Testing)],
        )),
        // Evicted...
        Ok(queue_of(TrunkQueueState::Running, Vec::new())),
        // ...then the remediation lands and the intent is resubmitted.
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(978, TrunkPrState::Pending)],
        )),
    ]);
    api.set_entry(978, Ok(entry_of(978, TrunkPrState::Failed)));
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;
    probe.run_pass(&ctx, t0 + Duration::from_secs(62)).await;

    let intent = db
        .get_active_trunk_merge_intent(&task_id)
        .unwrap()
        .expect("still active");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some("pending"),
        "seeing the entry back in the queue clears the terminal state",
    );
    assert_eq!(stored_detail(&db, &task_id)["position"], 1);
}

// ── Auto-resubmit / conflict-during-queue coordination ─────────────────────

#[tokio::test]
async fn an_intent_awaiting_resubmit_is_resubmitted_and_returns_to_the_merging_lane() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "awaiting-resubmit", 978);
    let intent = db.get_active_trunk_merge_intent(&task_id).unwrap().unwrap();
    // Mirrors what `ci_watch::on_ci_resolved` / `conflict_watch::on_resolved`
    // do once the fix lands — no need to drive the whole ci_watch/
    // conflict_watch pipeline to exercise the poller's own reaction.
    db.record_trunk_merge_intent_state(&intent.id, crate::trunk_merge::TRUNK_INTENT_AWAITING_RESUBMIT)
        .unwrap();

    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };

    let outcome = probe.run_pass(&ctx, Instant::now()).await;

    assert_eq!(outcome.resubmits, 1);
    assert_eq!(api.submit_call_count(), 1);
    let calls = api.submit_calls.lock().unwrap().clone();
    assert_eq!(calls[0], (REPO.to_owned(), 978, "main".to_owned()));

    let intent = db
        .get_active_trunk_merge_intent(&task_id)
        .unwrap()
        .expect("still active");
    assert_eq!(intent.submit_count, 2, "resubmit bumps submit_count");
    assert!(
        intent.last_trunk_state.is_none(),
        "the sentinel clears so live tracking resumes on the next getQueue",
    );
    let (state, _) = stored_queue_state(&db, &task_id);
    assert_eq!(
        state.as_deref(),
        Some("queued"),
        "the card moves back into the Merging lane optimistically",
    );
}

#[tokio::test]
async fn a_failed_resubmit_leaves_the_sentinel_for_the_next_cycle() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "resubmit-fails", 978);
    let intent = db.get_active_trunk_merge_intent(&task_id).unwrap().unwrap();
    db.record_trunk_merge_intent_state(&intent.id, crate::trunk_merge::TRUNK_INTENT_AWAITING_RESUBMIT)
        .unwrap();

    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    api.set_submit_replies(vec![Err(StubError::Unavailable)]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };

    let outcome = probe.run_pass(&ctx, Instant::now()).await;

    assert_eq!(outcome.resubmits, 0);
    assert_eq!(outcome.probe_failures, 1);
    let intent = db
        .get_active_trunk_merge_intent(&task_id)
        .unwrap()
        .expect("still active");
    assert_eq!(intent.submit_count, 1, "a failed resubmit must not bump submit_count");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_AWAITING_RESUBMIT),
        "the sentinel stays put so the next due cycle retries",
    );
}

/// Covers the case the feature actually exists for: the conflict was
/// detected while the entry was STILL LIVE in Trunk's `getQueue` snapshot,
/// so `by_pr_number` carries a non-terminal entry for it on this pass. The
/// superseded check must win over the live-entry match arm, or
/// `record_observed_state` clobbers the sentinel before `cancelPullRequest`
/// is ever issued.
#[tokio::test]
async fn a_live_intent_superseded_by_conflict_is_cancelled_before_its_sentinel_can_be_clobbered() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "superseded-live", 978);
    let intent = db.get_active_trunk_merge_intent(&task_id).unwrap().unwrap();
    db.record_trunk_merge_intent_state(&intent.id, crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
        .unwrap();

    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![entry_of(978, TrunkPrState::Pending)],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };

    let outcome = probe.run_pass(&ctx, Instant::now()).await;

    assert_eq!(outcome.queue_cancellations, 1);
    assert_eq!(api.cancel_call_count(), 1);
    let calls = api.cancel_calls.lock().unwrap().clone();
    assert_eq!(calls[0], (REPO.to_owned(), 978, "main".to_owned()));

    // The sentinel must survive: a live-entry write here would mean
    // `cancelPullRequest` is unreachable in production.
    let intent = db
        .get_active_trunk_merge_intent(&task_id)
        .unwrap()
        .expect("still active");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
    );
    assert!(db.active_ci_remediation_for_work_item(&task_id).unwrap().is_none());
}

/// The entry-absent case: the conflict resolver already pulled it, or Trunk
/// failed it independently, before this pass ran.
#[tokio::test]
async fn an_intent_superseded_by_conflict_is_cancelled_and_the_sentinel_persists() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "superseded", 978);
    let intent = db.get_active_trunk_merge_intent(&task_id).unwrap().unwrap();
    db.record_trunk_merge_intent_state(&intent.id, crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
        .unwrap();

    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };

    let outcome = probe.run_pass(&ctx, Instant::now()).await;

    assert_eq!(outcome.queue_cancellations, 1);
    assert_eq!(api.cancel_call_count(), 1);
    let calls = api.cancel_calls.lock().unwrap().clone();
    assert_eq!(calls[0], (REPO.to_owned(), 978, "main".to_owned()));

    // No eviction remediation, and the conflict resolver still owns the
    // slot: the sentinel is untouched, and no `ci_remediations` row exists.
    let intent = db
        .get_active_trunk_merge_intent(&task_id)
        .unwrap()
        .expect("still active");
    assert_eq!(
        intent.last_trunk_state.as_deref(),
        Some(crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT),
    );
    assert!(db.active_ci_remediation_for_work_item(&task_id).unwrap().is_none());
}

#[tokio::test]
async fn a_cancel_that_finds_no_live_entry_is_not_a_probe_failure() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "cancel-not-found", 978);
    let intent = db.get_active_trunk_merge_intent(&task_id).unwrap().unwrap();
    db.record_trunk_merge_intent_state(&intent.id, crate::trunk_merge::TRUNK_INTENT_SUPERSEDED_BY_CONFLICT)
        .unwrap();

    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    api.set_cancel_replies(vec![Err(StubError::NotFound)]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };

    let outcome = probe.run_pass(&ctx, Instant::now()).await;

    assert_eq!(outcome.queue_cancellations, 0);
    assert_eq!(outcome.probe_failures, 0, "a NotFound cancel reply is not a failure");
}

#[tokio::test]
async fn an_entry_unknown_to_trunk_leaves_the_intent_untouched() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "unknown", 978);
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    api.set_entry(978, Err(StubError::NotFound));
    let publisher = RecordingPublisher::default();
    let mut probe = probe();

    let outcome = probe
        .run_pass(
            &TrunkSweepContext {
                work_db: &db,
                publisher: &publisher,
                api: &api,
            },
            Instant::now(),
        )
        .await;

    assert_eq!(outcome.intents_retired, 0);
    assert_eq!(outcome.probe_failures, 0, "a 404 is an answer, not a transport failure");
    assert!(db.get_active_trunk_merge_intent(&task_id).unwrap().is_some());
}

#[tokio::test]
async fn an_intent_whose_task_already_finished_is_retired_without_any_trunk_call() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "done", 978);
    db.update_work_item(
        &task_id,
        WorkItemPatch {
            status: Some("done".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let api = StubTrunkApi::default();
    let publisher = RecordingPublisher::default();
    let mut probe = probe();

    let outcome = probe
        .run_pass(
            &TrunkSweepContext {
                work_db: &db,
                publisher: &publisher,
                api: &api,
            },
            Instant::now(),
        )
        .await;

    assert_eq!(outcome.intents_retired, 1);
    assert_eq!(outcome.queues_probed, 0);
    assert_eq!(api.queue_call_count(), 0);
    assert!(db.get_active_trunk_merge_intent(&task_id).unwrap().is_none());
}

// ── Failure handling ──────────────────────────────────────────────────────

#[tokio::test]
async fn repeated_failures_back_the_queue_off_before_the_next_probe() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent(&db, "down", 978);
    let api = StubTrunkApi::with_queue(vec![Err(StubError::Unavailable)]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    let first = probe.run_pass(&ctx, t0).await;
    assert_eq!(first.probe_failures, 1);

    // First backoff step is 30s, so a pass at +29s must be skipped.
    probe.run_pass(&ctx, t0 + Duration::from_secs(29)).await;
    assert_eq!(api.queue_call_count(), 1);

    probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;
    assert_eq!(api.queue_call_count(), 2);

    // Second failure doubles the step to 60s.
    probe.run_pass(&ctx, t0 + Duration::from_secs(31 + 59)).await;
    assert_eq!(api.queue_call_count(), 2);
    probe.run_pass(&ctx, t0 + Duration::from_secs(31 + 61)).await;
    assert_eq!(api.queue_call_count(), 3);
}

#[tokio::test]
async fn a_queue_unreachable_for_fifteen_minutes_files_exactly_one_attention_item() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "unreachable", 978);
    let api = StubTrunkApi::with_queue(vec![Err(StubError::Unavailable)]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    // A short outage stays silent.
    probe.run_pass(&ctx, t0).await;
    let early = probe.run_pass(&ctx, t0 + Duration::from_secs(10 * 60)).await;
    assert_eq!(early.attentions_filed, 0);
    assert!(attention_kinds(&db, &task_id).is_empty());

    let late = probe.run_pass(&ctx, t0 + Duration::from_secs(16 * 60)).await;
    assert_eq!(late.attentions_filed, 1);
    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![TRUNK_QUEUE_UNREACHABLE_ATTENTION_KIND.to_owned()]
    );

    // Still down an hour later: one problem, one item.
    let later = probe.run_pass(&ctx, t0 + Duration::from_secs(60 * 60)).await;
    assert_eq!(later.attentions_filed, 0);
    assert_eq!(attention_kinds(&db, &task_id).len(), 1);
}

#[tokio::test]
async fn a_rejected_token_is_reported_immediately_and_once() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "authfail", 978);
    let api = StubTrunkApi::with_queue(vec![Err(StubError::Auth)]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    let first = probe.run_pass(&ctx, t0).await;
    assert_eq!(
        first.attentions_filed, 1,
        "a dead token stalls every merge — say so now"
    );
    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![TRUNK_TOKEN_REJECTED_ATTENTION_KIND.to_owned()]
    );

    let second = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;
    assert_eq!(second.attentions_filed, 0);
    assert_eq!(attention_kinds(&db, &task_id).len(), 1);
}

#[tokio::test]
async fn recovery_re_arms_the_unreachable_attention_for_the_next_outage() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "flaky", 978);
    let api = StubTrunkApi::with_queue(vec![
        Err(StubError::Unavailable),
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(978, TrunkPrState::Pending)],
        )),
        Err(StubError::Unavailable),
    ]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await; // fail
    probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await; // recover
    // The failure clock restarted, so 16 min after t0 — but only ~15.5 min
    // into the *new* outage's first failure — nothing is filed yet.
    probe.run_pass(&ctx, t0 + Duration::from_secs(16 * 60)).await; // fail again
    assert!(attention_kinds(&db, &task_id).is_empty());

    probe.run_pass(&ctx, t0 + Duration::from_secs(32 * 60)).await;
    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![TRUNK_QUEUE_UNREACHABLE_ATTENTION_KIND.to_owned()]
    );
}

#[tokio::test]
async fn a_paused_queue_files_one_attention_item_per_episode() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "paused", 978);
    let api = StubTrunkApi::with_queue(vec![
        Ok(queue_of(
            TrunkQueueState::Paused,
            vec![entry_of(978, TrunkPrState::Pending)],
        )),
        Ok(queue_of(
            TrunkQueueState::Paused,
            vec![entry_of(978, TrunkPrState::Pending)],
        )),
        Ok(queue_of(
            TrunkQueueState::Running,
            vec![entry_of(978, TrunkPrState::Pending)],
        )),
        Ok(queue_of(
            TrunkQueueState::Draining,
            vec![entry_of(978, TrunkPrState::Pending)],
        )),
    ]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    let first = probe.run_pass(&ctx, t0).await;
    assert_eq!(first.attentions_filed, 1);
    // A paused queue still reports positions — the card keeps rendering,
    // now carrying the queue-level state the app's banner reads.
    assert_eq!(stored_detail(&db, &task_id)["queue_state"], "paused");

    // Still paused on the next cycle — the same problem, not a new one.
    let second = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;
    assert_eq!(second.attentions_filed, 0);

    // RUNNING again clears the episode...
    probe.run_pass(&ctx, t0 + Duration::from_secs(62)).await;
    // ...so a later DRAINING is a fresh one worth reporting.
    let fourth = probe.run_pass(&ctx, t0 + Duration::from_secs(93)).await;
    assert_eq!(fourth.attentions_filed, 1);

    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![
            TRUNK_QUEUE_NOT_RUNNING_ATTENTION_KIND.to_owned(),
            TRUNK_QUEUE_NOT_RUNNING_ATTENTION_KIND.to_owned(),
        ]
    );
    assert_eq!(stored_detail(&db, &task_id)["queue_state"], "draining");
}

#[tokio::test]
async fn an_unparseable_repo_slug_parks_the_queue_instead_of_calling_trunk() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent_on(&db, "badrepo", 978, "not-a-slug", "main");
    let api = StubTrunkApi::default();
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    probe.run_pass(&ctx, t0 + Duration::from_secs(60)).await;

    assert_eq!(api.queue_call_count(), 0, "never issue a request Trunk would reject");
}

// ── section_order renumbering ───────────────────────────────────────────────

#[tokio::test]
async fn section_order_is_a_contiguous_rank_across_only_the_boss_tracked_members() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = create_test_product_named(&db, "Product-renumber");
    // Three Boss-tracked entries, deliberately not contiguous in the real
    // queue: a non-Boss PR (500) sits between 978 and 979.
    let task_a = create_test_chore_manual(&db, product.id.clone(), "a");
    let task_b = create_test_chore_manual(&db, product.id.clone(), "b");
    for (task, pr_number) in [(&task_a, 978), (&task_b, 979)] {
        db.update_work_item(
            &task.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url(pr_number)),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.insert_trunk_merge_intent(
            TrunkMergeIntentInsertInput::builder()
                .work_item_id(task.id.clone())
                .pr_url(pr_url(pr_number))
                .pr_number(pr_number)
                .repo(REPO)
                .target_branch("main")
                .build(),
        )
        .unwrap();
    }

    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![
            entry_of(978, TrunkPrState::TestsPassed),
            entry_of(500, TrunkPrState::Testing),
            entry_of(979, TrunkPrState::Pending),
        ],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();

    probe
        .run_pass(
            &TrunkSweepContext {
                work_db: &db,
                publisher: &publisher,
                api: &api,
            },
            Instant::now(),
        )
        .await;

    let detail_a = stored_detail(&db, &task_a.id);
    let detail_b = stored_detail(&db, &task_b.id);
    // Raw `position` keeps the real (gappy) queue index...
    assert_eq!(detail_a["position"], 1);
    assert_eq!(detail_b["position"], 3);
    // ...but `section_order` is the contiguous rank among tracked members
    // only, so the Merging lane orders them back-to-back.
    assert_eq!(detail_a["section_order"], 1);
    assert_eq!(detail_b["section_order"], 2);
}

#[tokio::test]
async fn a_stable_queue_does_not_re_renumber_every_pass() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "renumber-stable", 979);
    // 978 is a phantom, non-Boss-tracked entry ahead of the tracked one —
    // the gap scenario that makes `position` (2) diverge from the
    // contiguous `section_order` (1).
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![
            entry_of(978, TrunkPrState::Testing),
            entry_of(979, TrunkPrState::Pending),
        ],
    ))]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    let first = probe.run_pass(&ctx, t0).await;
    assert_eq!(stored_detail(&db, &task_id)["section_order"], 1);
    assert!(first.state_writes >= 1);

    // Same shape next cycle: nothing actually moved, so nothing should be
    // rewritten or republished by either the live-entry write or the
    // renumbering pass — the provisional-then-corrected churn is a
    // one-time cost on first entry, not a steady-state one.
    let second = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;
    assert_eq!(second.state_writes, 0);
}

// ── listPullRequests reconciliation backstop ────────────────────────────────

#[tokio::test]
async fn the_reconciliation_backstop_catches_a_transition_the_point_probes_missed() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "missed", 978);
    // The entry is gone from every `getQueue` response the point probe
    // sees, and `getSubmittedPullRequest` never resolves it either (as if
    // Trunk hadn't indexed it yet) — nothing but the backstop can close
    // this out.
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    api.set_entry(978, Err(StubError::NotFound));
    api.set_list_pull_requests(vec![Ok(ListPullRequestsResponse {
        pull_requests: vec![entry_of(978, TrunkPrState::Cancelled)],
        next_cursor: None,
    })]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    assert!(
        db.get_active_trunk_merge_intent(&task_id).unwrap().is_some(),
        "still active — nothing has resolved it yet"
    );

    // The backstop is due only after RECONCILE_INTERVAL (10 min).
    let outcome = probe.run_pass(&ctx, t0 + Duration::from_secs(10 * 60 + 1)).await;

    assert_eq!(outcome.intents_retired, 1, "the backstop resolved the cancellation");
    assert!(db.get_active_trunk_merge_intent(&task_id).unwrap().is_none());
    assert_eq!(
        attention_kinds(&db, &task_id),
        vec![TRUNK_QUEUE_ENTRY_CANCELLED_ATTENTION_KIND.to_owned()]
    );
}

#[tokio::test]
async fn the_reconciliation_backstop_does_not_fire_before_its_own_cadence() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent(&db, "not-due-yet", 978);
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(
        TrunkQueueState::Running,
        vec![entry_of(978, TrunkPrState::Pending)],
    ))]);
    api.set_list_pull_requests(vec![Ok(ListPullRequestsResponse {
        pull_requests: vec![entry_of(978, TrunkPrState::Cancelled)],
        next_cursor: None,
    })]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    // Well within the 30 s Pending-tier cadence, but nowhere near the 10
    // minute reconciliation cadence.
    let second = probe.run_pass(&ctx, t0 + Duration::from_secs(31)).await;

    assert_eq!(
        second.intents_retired, 0,
        "a stale listPullRequests reply must not fire early"
    );
}

#[tokio::test]
async fn a_failed_reconciliation_call_does_not_advance_the_since_cursor() {
    let (_tmp, db) = crate::test_support::open_db();
    seed_intent(&db, "reconcile-failure", 978);
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    // First reconciliation attempt fails outright; the second (a full
    // RECONCILE_INTERVAL later) would succeed and resolve the entry, but
    // this test only cares about what `since` the second call sent.
    api.set_list_pull_requests(vec![
        Err(StubError::Unavailable),
        Ok(ListPullRequestsResponse {
            pull_requests: vec![entry_of(978, TrunkPrState::Cancelled)],
            next_cursor: None,
        }),
    ]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    let first = probe.run_pass(&ctx, t0 + Duration::from_secs(10 * 60 + 1)).await;
    assert_eq!(first.probe_failures, 1, "the failed listPullRequests call was counted");

    // Due again a further RECONCILE_INTERVAL later.
    probe.run_pass(&ctx, t0 + Duration::from_secs(2 * (10 * 60 + 1))).await;

    let calls = api.list_pull_requests_calls();
    assert_eq!(calls.len(), 2, "one call per due reconciliation");
    assert_eq!(
        calls[1].0, None,
        "the cursor must not advance past the window the failed call never actually examined"
    );
}

#[tokio::test]
async fn the_reconciliation_backstop_follows_next_cursor_into_a_second_page() {
    let (_tmp, db) = crate::test_support::open_db();
    let (_, task_id) = seed_intent(&db, "paginated", 978);
    let api = StubTrunkApi::with_queue(vec![Ok(queue_of(TrunkQueueState::Running, Vec::new()))]);
    api.set_entry(978, Err(StubError::NotFound));
    // The tracked member's terminal transition only appears on the
    // second page; a backstop that ignores `next_cursor` never sees it.
    api.set_list_pull_requests(vec![
        Ok(ListPullRequestsResponse {
            pull_requests: vec![entry_of(1, TrunkPrState::Merged)],
            next_cursor: Some("page-2".to_owned()),
        }),
        Ok(ListPullRequestsResponse {
            pull_requests: vec![entry_of(978, TrunkPrState::Cancelled)],
            next_cursor: None,
        }),
    ]);
    let publisher = RecordingPublisher::default();
    let mut probe = probe();
    let ctx = TrunkSweepContext {
        work_db: &db,
        publisher: &publisher,
        api: &api,
    };
    let t0 = Instant::now();

    probe.run_pass(&ctx, t0).await;
    let outcome = probe.run_pass(&ctx, t0 + Duration::from_secs(10 * 60 + 1)).await;

    assert_eq!(outcome.intents_retired, 1, "the second page's transition was observed");
    assert!(db.get_active_trunk_merge_intent(&task_id).unwrap().is_none());

    let calls = api.list_pull_requests_calls();
    assert_eq!(calls.len(), 2, "the backstop followed next_cursor onto a second page");
    assert_eq!(calls[0].1, None, "the first page is requested with no cursor");
    assert_eq!(
        calls[1].1,
        Some("page-2".to_owned()),
        "the second page is requested with the prior response's next_cursor"
    );
}
