use super::*;

/// One `RemovedFromMergeQueueEvent` entry from the PR's timeline.
#[derive(Debug, Clone)]
pub struct MergeQueueDequeueEvent {
    pub reason: String,
    /// `beforeCommit.oid` — the synthetic merge SHA that failed CI.
    /// `None` when GitHub omitted it (edge case for non-CI reasons).
    pub before_commit_oid: Option<String>,
}

/// Timeline-events selection set for [`build_batch_query`]'s `fields`
/// parameter, used by [`fetch_merge_queue_dequeue_events_batch`]. Requests
/// the last 20 `RemovedFromMergeQueueEvent` timeline entries — enough to
/// cover any realistically plausible burst of re-enqueue/dequeue cycles on
/// a single PR within one poll pass.
pub(crate) const DEQUEUE_EVENTS_FIELDS: &str = concat!(
    "timelineItems(itemTypes: [REMOVED_FROM_MERGE_QUEUE_EVENT], last: 20) { nodes { ",
    "... on RemovedFromMergeQueueEvent { reason beforeCommit { oid } } } }",
);

/// Batch-fetch merge-queue dequeue events for every PR in `pr_urls` with
/// one GraphQL round trip per pass — one `repository(...)` alias per
/// distinct repo, one `pullRequest(...)` alias per PR inside it, the same
/// grouping [`build_batch_query`] uses for the lifecycle probe — instead of
/// one `gh api graphql` call per PR. This is what collapses
/// [`check_merge_queue_rebounce`]'s per-candidate polling from O(open rows)
/// requests per pass down to O(1): previously every `in_review` /
/// `blocked_ci` candidate got its own unbatched timeline query on every
/// full sweep, which was the dominant per-pass request volume behind the
/// hourly quota exhaustion this batching fixes.
///
/// Best-effort like the lifecycle probe's degradation path, but stricter:
/// on ANY failure (spawn error, non-zero exit, unparseable body, top-level
/// GraphQL `errors`) this returns an empty map for the whole batch rather
/// than falling back to per-PR fetches. Unlike PR-lifecycle probing, a
/// missed pass of dequeue detection is harmless — the next pass re-queries
/// the same 20-item timeline window — so spending N extra requests on a
/// per-PR fallback is never worth it, and would specifically defeat the
/// point when the failure IS the rate limit (a fallback would turn one
/// rejected batch into N more rejected requests).
pub(crate) async fn fetch_merge_queue_dequeue_events_batch(
    pr_urls: &[String],
) -> HashMap<String, Vec<MergeQueueDequeueEvent>> {
    let mut out = HashMap::new();
    let mut parsed: HashMap<String, (String, String, u64)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for url in pr_urls {
        if parsed.contains_key(url) {
            continue;
        }
        if let Some((owner, repo, number)) = parse_pr_url_parts(url) {
            parsed.insert(url.clone(), (owner.to_owned(), repo.to_owned(), number));
            order.push(url.clone());
        }
    }
    if order.is_empty() {
        return out;
    }

    let (query, alias_map) = build_batch_query(&order, &parsed, DEQUEUE_EVENTS_FIELDS);
    let output = gh_output(&["api", "graphql", "-f", &format!("query={query}")]).await;
    let body: serde_json::Value = match output {
        Ok(o) if o.status.success() => match serde_json::from_slice(&o.stdout) {
            Ok(v) => v,
            Err(err) => {
                tracing::debug!(
                    ?err,
                    "merge poller: failed to parse batched dequeue-events graphql response"
                );
                return out;
            }
        },
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if is_rate_limit_error(&stderr) {
                tracing::warn!(
                    stderr = %stderr.trim(),
                    pr_count = order.len(),
                    "merge poller: batched dequeue-events graphql call rejected — GitHub API rate limit exceeded; skipping this pass",
                );
                RATE_LIMIT_REMAINING.store(0, Ordering::Relaxed);
            } else {
                tracing::debug!(
                    stderr = %stderr.trim(),
                    "merge poller: batched dequeue-events graphql call failed",
                );
            }
            return out;
        }
        Err(err) => {
            tracing::debug!(
                ?err,
                "merge poller: failed to spawn batched dequeue-events `gh api graphql`"
            );
            return out;
        }
    };

    record_rate_limit(&body);

    if body
        .get("errors")
        .and_then(|e| e.as_array())
        .is_some_and(|errors| !errors.is_empty())
    {
        tracing::debug!(
            errors = %body["errors"],
            "merge poller: batched dequeue-events graphql response carried errors",
        );
        return out;
    }

    for (url, pr_node) in walk_batch_response(&body, &alias_map) {
        let nodes = pr_node.and_then(|node| node["timelineItems"]["nodes"].as_array());
        out.insert(url, nodes.map(|n| parse_dequeue_event_nodes(n)).unwrap_or_default());
    }
    out
}

/// Shared node-level parser for a `timelineItems.nodes` array (whichever
/// shape the caller located it in — [`fetch_merge_queue_dequeue_events_batch`]'s
/// per-PR aliased node). Returns events with `reason == "failed_checks"`
/// (case-insensitive; GitHub's API returns the lowercase form even though
/// the GraphQL schema documents the enum as uppercase `FAILED_CHECKS`).
/// Events for other reasons (`MANUAL_REMOVAL`, `MERGE_CONFLICT`, etc.) are
/// filtered out.
///
/// Pure and side-effect-free so the filtering/casing rules can be
/// unit-tested without a live `gh` call.
pub(crate) fn parse_dequeue_event_nodes(nodes: &[serde_json::Value]) -> Vec<MergeQueueDequeueEvent> {
    let mut events = Vec::new();
    for node in nodes {
        let reason = match node["reason"].as_str() {
            Some(r) => r.to_owned(),
            None => continue,
        };
        // Only surface FAILED_CHECKS — all other reasons are informational
        // or terminal-success and must not feed the ci_failure path.
        // GitHub returns the lowercase form "failed_checks" even though
        // the schema declares the enum as FAILED_CHECKS; compare
        // case-insensitively to accept both.
        if !reason.eq_ignore_ascii_case("failed_checks") {
            continue;
        }
        let before_commit_oid = node["beforeCommit"]["oid"].as_str().map(|s| s.to_owned());
        events.push(MergeQueueDequeueEvent {
            reason,
            before_commit_oid,
        });
    }
    events
}

/// Shared by [`run_one_pass`] and [`reconcile_one`]: dedup `in_review` and
/// `blocked_ci` by `work_item_id`, drop `trunk_queue` products (see
/// [`is_trunk_queue_product`]), batch-fetch merge-queue dequeue events for
/// the remaining candidates, and run [`check_merge_queue_rebounce`] on each.
pub(crate) async fn run_merge_queue_rebounce_pass(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    in_review: &[PendingMergeCheck],
    blocked_ci: &[PendingMergeCheck],
    outcome: &mut SweepOutcome,
) {
    let mut rebounce_seen = std::collections::HashSet::new();
    let rebounce_candidates: Vec<&PendingMergeCheck> = in_review
        .iter()
        .chain(blocked_ci.iter())
        .filter(|candidate| rebounce_seen.insert(candidate.work_item_id.clone()))
        .filter(|candidate| !is_trunk_queue_product(work_db, &candidate.product_id))
        .collect();
    let rebounce_urls: Vec<String> = rebounce_candidates.iter().map(|c| c.pr_url.clone()).collect();
    let dequeue_events = fetch_merge_queue_dequeue_events_batch(&rebounce_urls).await;
    for candidate in &rebounce_candidates {
        check_merge_queue_rebounce(work_db, publisher, candidate, &dequeue_events, outcome).await;
    }
}

/// Poll the PR's merge-queue timeline for `FAILED_CHECKS` dequeue
/// events and fire [`ci_watch::on_merge_queue_rebounce_detected`] for
/// any event whose `beforeCommit.oid` is not yet recorded in
/// `ci_remediations`. Best-effort: a failed GraphQL call is logged at
/// debug and skipped; the next sweep will retry.
pub(crate) async fn check_merge_queue_rebounce(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    dequeue_events: &HashMap<String, Vec<MergeQueueDequeueEvent>>,
    outcome: &mut SweepOutcome,
) {
    let Some(events) = dequeue_events.get(&candidate.pr_url) else {
        return;
    };
    if events.is_empty() {
        return;
    }
    for event in events {
        let Some(before_commit_sha) = event.before_commit_oid.as_deref() else {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: FAILED_CHECKS dequeue event has no beforeCommit.oid; skipping",
            );
            continue;
        };
        // Fetch the failing CI checks for the synthetic merge commit so the
        // worker revision directive can show the exact build URL, job id,
        // and a log excerpt — without the worker having to rediscover them.
        // Best-effort: an empty result falls back to generic instructions.
        let failures = match repo_from_pr_url(&candidate.pr_url) {
            Some(owner_repo) => fetch_failing_checks_for_commit(owner_repo, before_commit_sha).await,
            None => Vec::new(),
        };
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            before_commit_sha,
            failing_checks = failures.len(),
            "merge poller: fetched failing checks for merge-queue commit",
        );
        if ci_watch::on_merge_queue_rebounce_detected(
            work_db,
            publisher,
            candidate,
            None, // head_ref_name not available without a probe round-trip
            before_commit_sha,
            &[], // labels not available here; opt-out check uses product flag only
            &failures,
        )
        .await
        {
            // `ci_watch::on_merge_queue_rebounce_detected` is the single
            // authority for the merge-queue-failure -> blocked transition and
            // already logs it (at most once per before_commit_sha). Don't
            // re-log the same flip here — two log lines for one transition,
            // firing at the same instant, read like two redundant watchers.
            // Just aggregate the sweep metric.
            outcome.merge_queue_rebounced += 1;
        }
    }
}

/// Derive the `merge_queue_state` DB string from a probe's merge-queue and
/// auto-merge flags. `Some("queued")` when the PR is in GitHub's merge
/// queue; `Some("auto_merge_enabled")` when auto-merge is armed but the PR
/// hasn't reached a queue (either the "Merge When Ready" request is still
/// pending required checks, or the repo has no merge queue at all); `None`
/// (NULL in DB) when neither. A queue entry always implies auto-merge is
/// also armed on GitHub's side, so `in_merge_queue` takes precedence.
///
/// Both values place the task in the macOS kanban's "Merging" section
/// (above "Today" in Done) — see [`merge_queue_detail_json`] for the
/// section's ordering key.
pub(crate) fn merge_queue_state_str(in_merge_queue: bool, auto_merge_enabled: bool) -> Option<&'static str> {
    if in_merge_queue {
        Some("queued")
    } else if auto_merge_enabled {
        Some("auto_merge_enabled")
    } else {
        None
    }
}

/// `section_order` sentinel for a queued PR whose GitHub queue position
/// wasn't reported (rare — schema allows it). Sorts below every PR with a
/// real numbered position but above the merge-when-ready bucket.
pub(crate) const QUEUED_NO_POSITION_SECTION_ORDER: i64 = 500_000;

/// `section_order` fallback for a merge-when-ready PR whose
/// `autoMergeRequest.enabledAt` GitHub didn't report. Sorts after every
/// merge-when-ready PR with a known enable time, at the very bottom of the
/// Merging section.
pub(crate) const MERGE_WHEN_READY_UNKNOWN_ENABLED_AT_SECTION_ORDER: i64 = i64::MAX;

/// Build the `merge_queue_detail` JSON blob (`{"position", "state",
/// "enqueued_at", "section_order"}`) from a probe's merge-queue and
/// auto-merge sub-state. Returns `None` when the probe is in neither state
/// (nothing to show). Otherwise always includes `section_order` — the
/// engine-computed sort key the macOS kanban's Merging section renders in,
/// so the client never has to reconstruct "queue position first, then
/// merge-when-ready" ordering itself:
///   - queued: the GitHub queue position (falls back to
///     [`QUEUED_NO_POSITION_SECTION_ORDER`] if GitHub omitted it).
///   - auto-merge armed but not queued: the `enabledAt` epoch (falls back
///     to [`MERGE_WHEN_READY_UNKNOWN_ENABLED_AT_SECTION_ORDER`]) — always
///     larger than any realistic queue position/sentinel, so this bucket
///     sorts below every queued PR, and multiple armed PRs order by which
///     requested Merge When Ready first.
pub(crate) fn merge_queue_detail_json(probe: &PrLifecycleProbe) -> Option<String> {
    let section_order = if probe.in_merge_queue {
        probe.merge_queue_position.unwrap_or(QUEUED_NO_POSITION_SECTION_ORDER)
    } else if probe.auto_merge_enabled {
        probe
            .auto_merge_enabled_at
            .as_deref()
            .and_then(parse_iso8601_lenient)
            .unwrap_or(MERGE_WHEN_READY_UNKNOWN_ENABLED_AT_SECTION_ORDER)
    } else {
        return None;
    };
    serde_json::to_string(&serde_json::json!({
        "position": probe.merge_queue_position,
        "state": probe.merge_queue_entry_state,
        "enqueued_at": probe.merge_queue_enqueued_at,
        "section_order": section_order,
    }))
    .ok()
}

/// Whether `product_id` is on the `trunk_queue` merge mechanism — the Trunk
/// probe owns `merge_queue_state`/`merge_queue_detail` and the eviction
/// signal for these products, so the GitHub-side probe must not write those
/// columns or run GitHub-queue-specific coordination (see
/// `preserve_merge_queue_state` in [`update_pr_poll_state`] and the
/// `check_merge_queue_rebounce` gate in [`run_one_pass`]/[`reconcile_one`]).
/// Defaults to `false` (treat as `direct`) on a missing product or a DB
/// error, logging the latter — a transient DB failure must not silently
/// disable GitHub-side merge handling for a `direct` product.
pub(crate) fn is_trunk_queue_product(work_db: &WorkDb, product_id: &str) -> bool {
    match work_db.get_product(product_id) {
        Ok(Some(product)) => matches!(
            crate::merge_mechanism::MergeMechanism::parse(product.merge_mechanism.as_deref()),
            Ok(crate::merge_mechanism::MergeMechanism::TrunkQueue { .. })
        ),
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                product_id,
                ?err,
                "merge poller: failed to load product to check merge mechanism; treating as direct",
            );
            false
        }
    }
}

/// Fields pulled back out of a stored `merge_queue_detail` JSON blob — the
/// read-side counterpart to [`merge_queue_detail_json`]'s write. Used only
/// by [`renumber_merge_queue`] to re-sort and rebuild the blob for every
/// currently queued member of a product.
pub(crate) struct StoredMergeQueueDetail {
    state: Option<String>,
    enqueued_at: Option<String>,
    position: Option<i64>,
}

pub(crate) fn parse_stored_merge_queue_detail(json: Option<&str>) -> StoredMergeQueueDetail {
    let parsed: Option<serde_json::Value> = json.and_then(|s| serde_json::from_str(s).ok());
    StoredMergeQueueDetail {
        state: parsed
            .as_ref()
            .and_then(|v| v.get("state"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        enqueued_at: parsed
            .as_ref()
            .and_then(|v| v.get("enqueued_at"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        position: parsed.as_ref().and_then(|v| v.get("position")).and_then(|v| v.as_i64()),
    }
}

/// Sort key for [`renumber_merge_queue`]: ascending `enqueued_at` (GitHub's
/// immutable FIFO join time for a queue entry) first since it can't be
/// clobbered by our own renumbering the way a raw `position` value can;
/// falls back to the last-known raw `position`, then `task_id`, for members
/// missing an `enqueued_at` so the ranking stays fully deterministic. A
/// missing value on either axis always sorts after every present value on
/// that axis, rather than comparing as smaller/earlier.
pub(crate) fn merge_queue_sort_key(detail: &StoredMergeQueueDetail, task_id: &str) -> (u8, String, u8, i64, String) {
    let (enqueued_rank, enqueued_val) = match &detail.enqueued_at {
        Some(ts) => (0u8, ts.clone()),
        None => (1u8, String::new()),
    };
    let (position_rank, position_val) = match detail.position {
        Some(p) => (0u8, p),
        None => (1u8, i64::MAX),
    };
    (
        enqueued_rank,
        enqueued_val,
        position_rank,
        position_val,
        task_id.to_owned(),
    )
}

/// Recompute a canonical, contiguous, duplicate-free position for every
/// task in `product_id` currently in GitHub's merge queue
/// (`merge_queue_state = 'queued'`), and re-broadcast for every member
/// whose displayed position actually changed (mono#1997).
///
/// [`update_pr_poll_state`] only ever learns and writes a probed PR's OWN
/// raw `mergeQueueEntry.position` — siblings aren't touched by that write.
/// So when one member enters, exits, fails, or reorders, every other
/// member can keep showing a stale number, including one that now
/// collides with a sibling's (two cards both showing `#2`) or a missing
/// number for a member GitHub hadn't yet reported a position for. This
/// pass is the fix: called after every merge-queue-relevant probe write —
/// not just for the row that changed — it re-derives a fresh `1..N`
/// ranking from every currently-tracked queued member and rewrites +
/// re-broadcasts every row whose rank changed, so the WHOLE queue stays
/// self-consistent rather than only the mutated item.
///
/// Membership in the renumbered set is governed *only* by
/// `merge_queue_state == "queued"` — a queued-but-failing member (e.g. one
/// GitHub reports as `UNMERGEABLE` while it's being dequeued) keeps a
/// position like any other member for as long as that column still reads
/// `"queued"`, and is excluded the moment it flips away. That is the one
/// rule callers need: a failed member's number never races between
/// "kept" and "excluded" mid-transition.
pub(crate) async fn renumber_merge_queue(work_db: &WorkDb, publisher: &dyn ExecutionPublisher, product_id: &str) {
    let ctx = crate::merge_queue_renumber::RenumberContext {
        work_db,
        publisher,
        product_id,
        log_context: "merge poller",
        event: "merge_queue_renumbered",
    };
    crate::merge_queue_renumber::renumber_section_order(
        &ctx,
        |member| {
            Some((
                member.task_id,
                parse_stored_merge_queue_detail(member.merge_queue_detail.as_deref()),
            ))
        },
        merge_queue_sort_key,
        // Diff on the parsed position, not the raw JSON string: a member
        // already at its canonical rank must be skipped even though the
        // rewritten blob's key order/section_order differs byte-for-byte
        // from whatever an earlier write (or a legacy/pre-migration row)
        // left behind — this is the "only touch what changed" guarantee,
        // not merely a micro-optimisation.
        |detail, position| detail.position == Some(position),
        |detail, position| {
            serde_json::to_string(&serde_json::json!({
                "position": position,
                "state": detail.state,
                "enqueued_at": detail.enqueued_at,
                "section_order": position,
            }))
            .ok()
        },
    )
    .await;
}
