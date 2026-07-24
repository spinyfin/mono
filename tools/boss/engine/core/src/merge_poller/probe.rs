use super::*;

/// One slice of GitHub-reported PR lifecycle state, captured by a
/// single `gh pr view` round-trip. Carries everything the poller's
/// sweep dispatch needs to route to merge/conflict/CI/clear paths.
///
/// The "four-state" naming in the design doc refers to the leaf
/// values of [`PrLifecycleState`] — `Open(...)` (with its own
/// mergeability + ci sub-state), `Merged`, `ClosedUnmerged`.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrLifecycleProbe {
    pub url: String,
    pub state: PrLifecycleState,
    /// Sha of the PR's base ref at probe time. Captured for the
    /// conflict-resolution flow (`conflict_resolutions.base_sha_at_trigger`,
    /// design Q3); currently informational for the merge poller.
    /// `None` when GitHub didn't report one (rare; usually means the
    /// PR has been force-detached from its base).
    pub base_ref_oid: Option<String>,
    /// Sha of the PR's head ref at probe time. The CI-watch path's
    /// idempotency key (`(work_item_id, head_sha_at_trigger,
    /// attempt_kind)`) needs this; `None` when GitHub didn't report
    /// it (rare).
    pub head_ref_oid: Option<String>,
    /// Name of the PR's head branch (e.g. `"my-feature"`). Required by
    /// the conflict-resolution attempt row (`head_branch` column); `None`
    /// when GitHub didn't report it.
    pub head_ref_name: Option<String>,
    /// Name of the PR's base branch (e.g. `"main"`). Required by the
    /// conflict-resolution attempt row (`base_branch` column); `None`
    /// when GitHub didn't report it.
    pub base_ref_name: Option<String>,
    /// Labels currently applied to the PR. Carried so the
    /// conflict-watch / auto-rebase / ci-watch paths can honour the
    /// per-PR opt-out label (`boss/no-auto-rebase`, design Q7 /
    /// Phase 6 #18) without a second `gh` round trip.
    pub labels: Vec<String>,
    /// Review-gating state derived from GitHub's `reviewDecision` and
    /// `reviews` fields. Used by the merge poller to update the
    /// `review_required_state` / `review_required_detail` columns on
    /// the task row for display in the macOS kanban Review-lane card.
    pub review: PrReviewState,
    /// Whether the PR is currently in GitHub's merge queue at probe time.
    /// Derived from `mergeQueueEntry` — non-null means in queue, null means
    /// not queued. Used to render the merging indicator on Review-lane cards
    /// (replaces the CI icon while the PR is merging).
    #[builder(default)]
    pub in_merge_queue: bool,
    /// GitHub's raw `mergeQueueEntry.state` (e.g. `"AWAITING_CHECKS"`,
    /// `"MERGEABLE"`, `"LOCKED"`, `"QUEUED"`, `"UNMERGEABLE"`). `None` when
    /// `in_merge_queue` is `false` or GitHub omitted the field.
    pub merge_queue_entry_state: Option<String>,
    /// GitHub's raw `mergeQueueEntry.position` (1-indexed queue position).
    /// `None` when `in_merge_queue` is `false` or GitHub omitted the field.
    pub merge_queue_position: Option<i64>,
    /// GitHub's raw `mergeQueueEntry.enqueuedAt` (RFC 3339 timestamp of when
    /// the PR entered the queue). `None` when `in_merge_queue` is `false` or
    /// GitHub omitted the field.
    pub merge_queue_enqueued_at: Option<String>,
    /// Raw `mergeable` string from GitHub (e.g. `"MERGEABLE"`, `"CONFLICTING"`,
    /// `"UNKNOWN"`). Carried through so callers can log the exact GitHub signal
    /// that drove each transition decision without a second round trip.
    #[builder(default)]
    pub raw_mergeable: String,
    /// Raw `mergeStateStatus` string from GitHub (e.g. `"CLEAN"`, `"DIRTY"`,
    /// `"BLOCKED"`, `"BEHIND"`, `"UNKNOWN"`). Paired with `raw_mergeable`
    /// for diagnosability on transition log lines.
    #[builder(default)]
    pub raw_merge_state_status: String,
    /// Whether GitHub's auto-merge is currently armed for this PR
    /// (`autoMergeRequest` non-null) — the PR will merge automatically once
    /// required checks and reviews pass, independent of whether it is also
    /// in a merge queue. Used to render the "Merging" kanban section for a
    /// task that requested Merge When Ready but hasn't reached the queue
    /// (or the repo has no queue at all).
    #[builder(default)]
    pub auto_merge_enabled: bool,
    /// GitHub's raw `autoMergeRequest.enabledAt` (RFC 3339). `None` when
    /// `auto_merge_enabled` is `false` or GitHub omitted the field. Used
    /// only as a deterministic secondary ordering key for the Merging
    /// section (earlier-armed PRs sort above later ones).
    pub auto_merge_enabled_at: Option<String>,
}

/// Lifecycle states the poller reacts to. The split between
/// `Open(Clean)` and `Open(Conflict)` is the load-bearing addition
/// for the merge-conflict design — they share `state='OPEN'` on the
/// GitHub side and are disambiguated by `mergeable` /
/// `mergeStateStatus`. The `Open` variant carries the joint
/// (mergeability, CI) status (design §Q1's `OpenPrStatus`). `Merged`
/// is what the original poller detected. `ClosedUnmerged` is
/// captured for completeness (per the closed-unmerged design); the
/// current sweep treats it as a no-op (a PR force-deleted out of
/// review is the user's problem, not the poller's), preserving prior
/// behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycleState {
    Open(OpenPrStatus),
    Merged,
    ClosedUnmerged,
}

/// Joint mergeability + CI status for an open PR. The two signals
/// share a probe round-trip and a single sweep dispatch (design §Q1's
/// "Composing the CI signal into the same probe"). The merge-poller
/// match expression routes on the pair: a conflict pre-empts CI
/// detection (the conflict-resolver owns the slot first); both clean
/// drives the retire path; CI-only failures fan out to `ci_watch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPrStatus {
    pub mergeability: OpenPrMergeability,
    pub ci: OpenPrCiStatus,
}

impl OpenPrStatus {
    /// Mergeable, CI clean — the steady-state "in_review and happy"
    /// shape. Used both by the production parser and by tests that
    /// only care about one of the two signals.
    pub fn clean() -> Self {
        Self {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Clean,
        }
    }

    /// Convenience for tests that only care about the conflict signal
    /// (the corresponding `ci` slot is `Clean`).
    pub fn conflict_only() -> Self {
        Self {
            mergeability: OpenPrMergeability::Conflict,
            ci: OpenPrCiStatus::Clean,
        }
    }

    /// Convenience for tests that only care about the CI-failing
    /// signal (the corresponding `mergeability` slot is `Clean`).
    pub fn ci_failing(failures: Vec<RequiredCheckFailure>) -> Self {
        Self {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Failing { failures },
        }
    }

    /// GitHub returned `mergeable=UNKNOWN` — mergeability indeterminate,
    /// CI clean. Used by tests that exercise the UNKNOWN skip path.
    pub fn unknown_mergeability() -> Self {
        Self {
            mergeability: OpenPrMergeability::Unknown,
            ci: OpenPrCiStatus::Clean,
        }
    }
}

/// Whether an open PR's head ref currently merges cleanly into its
/// base. Derived from GitHub's `mergeable` + `mergeStateStatus`
/// pair.
///
/// `UNKNOWN` (GitHub is mid-recompute) maps to the `Unknown` variant —
/// neither conflict-detection nor conflict-retire fires while mergeability
/// is indeterminate; the next sweep picks up the definitive `MERGEABLE`
/// or `CONFLICTING` result. Using `Unknown` instead of mapping to `Clean`
/// prevents phantom `blocked→in_review` transitions when GitHub returns
/// `UNKNOWN` transiently right after a base-branch move (root cause of
/// the conflict_watch blocked↔in_review flap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPrMergeability {
    Clean,
    Conflict,
    /// GitHub's `mergeable` field is `UNKNOWN` — the mergeability check
    /// is still being computed asynchronously. Skip all conflict-watch
    /// transitions this sweep; re-poll next sweep for a definitive answer.
    Unknown,
}

/// CI status of an open PR's required checks at probe time. Derived
/// from `statusCheckRollup` after collapsing by name (latest leaf per
/// check name; design §Q1) and applying the closed failure-conclusion
/// set against required checks only.
///
/// `Clean` means every required check is either `COMPLETED+SUCCESS`,
/// `NEUTRAL`, or `SKIPPED`. `Failing` carries the set of failing required
/// checks for the worker prompt and is reported only once the rollup is
/// *terminal* — every required check has reached a terminal conclusion and
/// at least one failed. `InFlight` is the wait state — at least one
/// required check has not reached a terminal conclusion yet; we do not
/// trigger a CI-fix attempt on it. `InFlight` dominates `Failing`: a
/// failing leaf alongside still-running checks reads as `InFlight`, so a
/// transient/early failure mid-run never spawns a moot remediation or
/// lights the "ci failing" badge (the `auto-retire` path is symmetric — it
/// waits for terminal success across the board, design §Q5 / "Auto-retire"
/// requires *all* checks at SUCCESS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrCiStatus {
    Clean,
    Failing { failures: Vec<RequiredCheckFailure> },
    InFlight,
}

/// Probe the lifecycle state of a single PR. Implemented for
/// production by shelling out to `gh`; test doubles can stub it
/// directly.
#[async_trait]
pub trait MergeProbe: Send + Sync {
    /// Returns the latest lifecycle state for `pr_url`. Errors are
    /// reserved for tool / network failures; "PR doesn't exist" is
    /// reported as `Ok` with `state=ClosedUnmerged` so the poller's
    /// in-review-stays-in-review behaviour is preserved (a deleted
    /// PR's row stays where it was).
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe>;

    /// Probe every PR in `pr_urls` in as few round trips as possible,
    /// keyed by URL (duplicates in the input collapse to one entry).
    /// Errors are carried as `String` rather than `anyhow::Error` so the
    /// result map stays cheaply clonable across the sweep's fan-out call
    /// sites.
    ///
    /// The default implementation probes each PR individually via
    /// [`Self::probe`] — used by [`NoopMergeProbe`] and every test double,
    /// none of which have a batched transport to gain from. [`CommandMergeProbe`]
    /// overrides this to issue one aliased GraphQL query for the whole batch
    /// instead of one `gh pr view` per PR (the PR-reconciler batching
    /// follow-up from the GitHub event-detection investigation, §9.1).
    async fn probe_batch(&self, pr_urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        let mut out = HashMap::new();
        for url in pr_urls {
            if out.contains_key(url) {
                continue;
            }
            let result = self.probe(url).await.map_err(|err| err.to_string());
            out.insert(url.clone(), result);
        }
        out
    }
}

/// `MergeProbe` that always returns an error — used as the default in
/// contexts that do not need real GitHub probing (e.g. unit tests that
/// never reach the CI-fetch path).
#[derive(Debug, Default)]
pub struct NoopMergeProbe;

#[async_trait]
impl MergeProbe for NoopMergeProbe {
    async fn probe(&self, _pr_url: &str) -> Result<PrLifecycleProbe> {
        anyhow::bail!("NoopMergeProbe: no real probe configured")
    }
}

/// `MergeProbe` that shells out to `gh pr view <url> --json …`.
#[derive(Debug, Default)]
pub struct CommandMergeProbe {
    /// ETag cache for the commit combined-status REST sub-signal
    /// (`repos/{owner}/{repo}/commits/{sha}/status`, fetched by
    /// [`Self::fetch_commit_combined_state_for_empty_rollup`]), keyed by
    /// the API path. A cached entry lets the next identical request carry
    /// `If-None-Match` and get back a `304` — which costs no primary REST
    /// quota — instead of re-fetching and re-parsing an unchanged
    /// resource (doc: `github-event-detection-webhooks-vs-polling`, §9.2).
    commit_status_cache: Mutex<HashMap<String, CachedCommitStatus>>,
}

impl CommandMergeProbe {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MergeProbe for CommandMergeProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
        let output = gh_output(&[
            "pr",
            "view",
            pr_url,
            "--json",
            // `statusCheckRollup` is a nested array we parse in
            // Rust (design §Q1's "Composing the CI signal into
            // the same probe"); the previous TSV-via-jq shape
            // can't carry it without escaping headaches, so we
            // take the raw JSON document from gh instead.
            // `reviewDecision` and `reviews` are added to capture
            // the review-required state for UI indicators.
            // NOTE: `mergeQueueEntry` is intentionally omitted here —
            // `gh pr view --json` does not expose it in all `gh` versions.
            // Merge-queue state is queried separately via `gh api graphql`
            // in `fetch_merge_queue_status` below. `autoMergeRequest` (used
            // to detect a "Merge When Ready" PR that hasn't reached the
            // queue) IS a valid `--json` field, so it's requested directly.
            "state,mergedAt,closedAt,mergeable,mergeStateStatus,baseRefOid,headRefOid,headRefName,baseRefName,labels,statusCheckRollup,reviewDecision,reviews,autoMergeRequest",
        ])
        .await
        .with_context(|| format!("failed to spawn `gh pr view {pr_url}`"))?;
        if !output.status.success() {
            let stderr_lower = String::from_utf8_lossy(&output.stderr).to_lowercase();
            // "could not resolve to a Resource" / 404 means the PR
            // doesn't exist any more (force-deleted, transferred). We
            // can't decide it's merged just because we can't see it,
            // so treat as closed-unmerged (a no-op for the sweep) and
            // leave the chore where it was.
            if stderr_lower.contains("could not resolve")
                || stderr_lower.contains("404")
                || stderr_lower.contains("not found")
            {
                return Ok(PrLifecycleProbe::builder()
                    .url(pr_url.to_owned())
                    .state(PrLifecycleState::ClosedUnmerged)
                    .labels(Vec::new())
                    .review(PrReviewState::Unknown)
                    .build());
            }
            return Err(anyhow!(
                "`gh pr view {pr_url}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // When `statusCheckRollup` is empty the GraphQL field omits
        // required-but-unstarted status contexts ("EXPECTED" in GitHub's
        // web UI). The legacy commit-status REST endpoint returns
        // `state:"pending"` in that case, which lets us show a non-green
        // indicator instead of a false-positive green.
        let combined_state = self.fetch_commit_combined_state_for_empty_rollup(&stdout, pr_url).await;
        let mut probe = parse_probe_json(pr_url, &stdout, combined_state.as_deref())?;
        // Query merge-queue status separately via GraphQL since `gh pr view --json`
        // does not expose `mergeQueueEntry` in all installed `gh` versions.
        // `mergeQueueEntry` is not exposed as a `--json` field in all
        // installed `gh` versions, so this is a separate GraphQL probe.
        let queue_entry = pr_merge_queue_entry(pr_url).await;
        probe.in_merge_queue = queue_entry.is_some();
        probe.merge_queue_entry_state = queue_entry.as_ref().map(|e| e.state.clone());
        probe.merge_queue_position = queue_entry.as_ref().and_then(|e| e.position);
        probe.merge_queue_enqueued_at = queue_entry.and_then(|e| e.enqueued_at);
        Ok(probe)
    }

    async fn probe_batch(&self, pr_urls: &[String]) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        self.probe_batch_via_graphql(pr_urls).await
    }
}

/// Shared GraphQL selection set for one PR's lifecycle fields, reused for
/// every aliased `pullRequest(...)` block in [`probe_batch_via_graphql`]'s
/// query. Mirrors the field set `CommandMergeProbe::probe`'s `gh pr view
/// --json` call requests, plus `mergeQueueEntry` folded in directly (that
/// probe fetches it via a *second* `gh api graphql` round trip —
/// `pr_in_merge_queue` — because `mergeQueueEntry` isn't a `--json` field in
/// all `gh` versions; here we just ask GraphQL for it inline, since we're
/// already hand-building the query).
///
/// GraphQL bills by node count (~cost = nodes / 100), so the connection
/// `first`/`last` caps here are the dominant per-PR lever on quota spend.
/// They are deliberately tight rather than blanket `first: 100`:
///
/// - `labels(first: 30)` — boss's own label vocabulary (`blocked:*`,
///   priority, opt-out flags) is a handful of names; 30 is comfortable
///   headroom and only the label *set* is consumed ([`parse_probe_json`]).
/// - `reviews(last: 20)` — the merge-gating decision is read from the
///   authoritative `reviewDecision` field, not this array; the array only
///   supplies the tooltip's reviewer login list ([`classify_review`]), for
///   which the 20 most-recent reviews cover every realistic PR.
/// - `contexts(first: 30)` — the CI rollup on the head commit. Required
///   checks are few (this repo gates on three Buildkite contexts); 30 is
///   several times the real fan-out while still cutting the worst-case
///   node count by ~3x versus `first: 100`.
///
/// Together these cut a typical probe from ~300 requested nodes to ~80 —
/// roughly a 3x GraphQL-cost reduction with no behavioural loss for any
/// realistically-shaped PR.
pub(crate) const PR_PROBE_FIELDS: &str = concat!(
    "state mergedAt closedAt mergeable mergeStateStatus baseRefOid headRefOid headRefName baseRefName ",
    "labels(first: 30) { nodes { name } } ",
    "reviewDecision reviews(last: 20) { nodes { author { login } state } } ",
    "mergeQueueEntry { state position enqueuedAt } ",
    "autoMergeRequest { enabledAt } ",
    "commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 30) { nodes { ",
    "__typename ... on CheckRun { name status conclusion detailsUrl } ",
    "... on StatusContext { context state targetUrl } } } } } } }",
);

/// Batch-probe every PR in `pr_urls` with a single `gh api graphql` round
/// trip covering the whole sweep, instead of one `gh pr view` (+ one
/// `mergeQueueEntry` lookup) per PR. Each distinct repository referenced
/// gets one aliased `repository(...)` block; each PR within that repo gets
/// one aliased `pullRequest(...)` block inside it — so the round-trip count
/// is 1 regardless of how many PRs (or repos) are in play this pass.
///
/// Reuses [`parse_probe_json`] by reshaping each aliased PR node back into
/// the same flat JSON document shape `gh pr view --json` produces, so the
/// single-PR classification logic (CI rollup collapsing, review-state
/// derivation, per-org review-signal reclassification, …) isn't duplicated.
///
/// Degrades per-PR, not per-pass: a URL that isn't a canonical GitHub PR
/// URL, or whose GraphQL node comes back null (PR force-deleted/
/// transferred — mapped to `ClosedUnmerged`, matching `probe`'s 404
/// handling), is resolved independently of its siblings. Only a failure of
/// the batched round trip itself (spawn failure, non-zero `gh` exit,
/// unparseable response) fails every requested PR for this pass — the
/// sweep already treats individual probe failures as "retry next pass", so
/// this is a graceful (if pass-wide) degradation, not a crash.
impl CommandMergeProbe {
    pub(crate) async fn probe_batch_via_graphql(
        &self,
        pr_urls: &[String],
    ) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        let mut out = HashMap::new();
        // Dedup while parsing so a URL repeated in the input (e.g. a PR that's
        // both `in_review` and, defensively, in `stranded_blocked`) costs one
        // query slot, not two.
        let mut parsed: HashMap<String, (String, String, u64)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for url in pr_urls {
            if out.contains_key(url) || parsed.contains_key(url) {
                continue;
            }
            match parse_pr_url_parts(url) {
                Some((owner, repo, number)) => {
                    parsed.insert(url.clone(), (owner.to_owned(), repo.to_owned(), number));
                    order.push(url.clone());
                }
                None => {
                    out.insert(url.clone(), Err(format!("`{url}` is not a canonical GitHub PR URL")));
                }
            }
        }
        if order.is_empty() {
            return out;
        }

        let (query, alias_map) = build_batch_query(&order, &parsed, PR_PROBE_FIELDS);

        tracing::debug!(
            pr_count = order.len(),
            repo_count = alias_map.len(),
            "merge poller: batched probe — one GraphQL query for the whole pass",
        );

        let output = gh_output(&["api", "graphql", "-f", &format!("query={query}")]).await;
        let body: serde_json::Value = match output {
            Ok(o) if o.status.success() => match serde_json::from_slice(&o.stdout) {
                Ok(v) => v,
                Err(err) => {
                    let msg = format!("failed to parse batched probe graphql response: {err}");
                    for url in &order {
                        out.insert(url.clone(), Err(msg.clone()));
                    }
                    return out;
                }
            },
            // A non-zero exit here isn't necessarily a transport failure: `gh
            // api graphql` also exits non-zero whenever the response carries a
            // top-level `errors` array, which GitHub returns (alongside HTTP
            // 200 and `data.<repo_alias> = null`) whenever a `repository(...)`
            // alias can't be resolved (repo deleted/renamed/access revoked).
            // Failing every URL in the pass for that case would let one
            // tracked PR whose repo has gone away stall reconciliation for
            // every other PR indefinitely, since `run_one_pass` rebuilds the
            // same probe set every pass. Fall back to per-PR probing instead,
            // so the graceful 404 -> `ClosedUnmerged` handling in `probe`
            // isolates the unresolvable repo from its siblings, exactly like
            // the pre-batching code did.
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if is_rate_limit_error(&stderr) {
                    // Do NOT fall back to per-PR probing here: that would
                    // turn one failed batch request into `order.len()`
                    // additional requests, every one of which is also
                    // going to be rejected while the quota is exhausted —
                    // exactly the "widen the retry loop" amplification
                    // that burns the remaining budget faster and starves
                    // every other GitHub consumer sharing this token.
                    // Fail the whole batch gracefully instead; the next
                    // pass (stretched by `rate_limit_throttle_factor` once
                    // a successful call reports the low reading) retries.
                    tracing::warn!(
                        stderr = %stderr.trim(),
                        pr_count = order.len(),
                        "merge poller: batched probe graphql call rejected — GitHub API rate limit exceeded; not retrying per-PR this pass",
                    );
                    // A rejected call carries no `rateLimit` body to read a
                    // real number from — force the budget to empty so
                    // `rate_limit_throttle_factor` maxes out immediately
                    // rather than waiting for a future successful call to
                    // report a low `remaining` reading.
                    RATE_LIMIT_REMAINING.store(0, Ordering::Relaxed);
                    let msg = "gh api graphql rejected: GitHub API rate limit exceeded".to_owned();
                    for url in &order {
                        out.insert(url.clone(), Err(msg.clone()));
                    }
                    return out;
                }
                tracing::debug!(
                    stderr = %stderr.trim(),
                    "merge poller: batched probe graphql call failed — falling back to per-PR probing for this pass",
                );
                out.extend(probe_each_individually(&order).await);
                return out;
            }
            Err(err) => {
                let msg = format!("failed to spawn batched probe `gh api graphql`: {err}");
                for url in &order {
                    out.insert(url.clone(), Err(msg.clone()));
                }
                return out;
            }
        };

        record_rate_limit(&body);

        // A 200 response can still carry a top-level `errors` array alongside
        // partial `data` (e.g. one repo alias resolves, another doesn't). Treat
        // that the same as the non-zero-exit case above: fall back to per-PR
        // probing rather than failing every URL in the pass over one bad repo
        // alias.
        if body
            .get("errors")
            .and_then(|e| e.as_array())
            .is_some_and(|errors| !errors.is_empty())
        {
            tracing::debug!(
                errors = %body["errors"],
                "merge poller: batched probe graphql response carried errors — falling back to per-PR probing for this pass",
            );
            out.extend(probe_each_individually(&order).await);
            return out;
        }

        for (url, pr_node) in walk_batch_response(&body, &alias_map) {
            let Some(pr_node) = pr_node else {
                // No PullRequest for this number any more (force-deleted,
                // transferred) — same "can't decide it's merged just because
                // we can't see it" fallback as `probe`'s 404 path.
                out.insert(
                    url.clone(),
                    Ok(PrLifecycleProbe::builder()
                        .url(url.clone())
                        .state(PrLifecycleState::ClosedUnmerged)
                        .labels(Vec::new())
                        .review(PrReviewState::Unknown)
                        .build()),
                );
                continue;
            };
            let flat = flatten_batched_pr_node(pr_node).to_string();
            let combined_state = self.fetch_commit_combined_state_for_empty_rollup(&flat, &url).await;
            let result = parse_probe_json(&url, &flat, combined_state.as_deref()).map_err(|err| err.to_string());
            out.insert(url.clone(), result);
        }
        out
    }
}

/// repo_alias -> [(pr_alias, url)] — the aliases [`build_batch_query`] used
/// to request each PR's node, and that [`walk_batch_response`] uses to find
/// that same node back out of the response.
pub(crate) type BatchAliasMap = Vec<(String, Vec<(String, String)>)>;

/// Builds the aliased GraphQL query for a batch of PRs, grouping by (owner,
/// repo) so multi-PR-per-repo sweeps (the common case) get one
/// `repository(...)` block instead of one per PR, with `fields` as each
/// `pullRequest(...)` block's selection set (callers pass [`PR_PROBE_FIELDS`]
/// for lifecycle probing or [`DEQUEUE_EVENTS_FIELDS`] for merge-queue
/// dequeue-event polling — same aliasing/grouping logic, different payload).
/// Also requests `rateLimit { remaining }` at the query's top level so every
/// batched call doubles as a quota reading for [`record_rate_limit`], at
/// zero extra cost (querying `rateLimit` itself doesn't consume quota).
/// Returns the query alongside the alias map needed to walk the response
/// back out.
///
/// Pure and side-effect-free so the alias-construction logic — grouping
/// multiple PRs per repo and multiple repos per batch — can be
/// unit-tested without a live `gh` call.
pub(crate) fn build_batch_query(
    order: &[String],
    parsed: &HashMap<String, (String, String, u64)>,
    fields: &str,
) -> (String, BatchAliasMap) {
    let mut by_repo: std::collections::BTreeMap<(String, String), Vec<&String>> = std::collections::BTreeMap::new();
    for url in order {
        let (owner, repo, _) = &parsed[url];
        by_repo.entry((owner.clone(), repo.clone())).or_default().push(url);
    }

    let mut query = String::from("{ rateLimit { remaining }");
    let mut alias_map: BatchAliasMap = Vec::new();
    for (repo_idx, ((owner, repo), urls)) in by_repo.iter().enumerate() {
        let repo_alias = format!("repo{repo_idx}");
        query.push_str(&format!(
            " {repo_alias}: repository(owner: \"{owner}\", name: \"{repo}\") {{"
        ));
        let mut pr_aliases = Vec::with_capacity(urls.len());
        for (pr_idx, url) in urls.iter().enumerate() {
            let number = parsed[url.as_str()].2;
            let pr_alias = format!("pr{pr_idx}");
            query.push_str(&format!(" {pr_alias}: pullRequest(number: {number}) {{ {fields} }}"));
            pr_aliases.push((pr_alias, (*url).clone()));
        }
        query.push_str(" }");
        alias_map.push((repo_alias, pr_aliases));
    }
    query.push_str(" }");
    (query, alias_map)
}

/// Walks the batched GraphQL response body back out by the same aliases
/// [`build_batch_query`] used to request it, returning for each URL either
/// its raw PR node (for the caller to flatten + parse) or `None` when the
/// node came back null (PR force-deleted/transferred).
///
/// Pure and side-effect-free so the response-walk — including the
/// null-node branch — can be unit-tested against a synthetic response
/// without a live `gh` call.
pub(crate) fn walk_batch_response<'a>(
    body: &'a serde_json::Value,
    alias_map: &BatchAliasMap,
) -> Vec<(String, Option<&'a serde_json::Value>)> {
    let mut out = Vec::new();
    for (repo_alias, pr_aliases) in alias_map {
        let repo_node = &body["data"][repo_alias.as_str()];
        for (pr_alias, url) in pr_aliases {
            let pr_node = &repo_node[pr_alias.as_str()];
            out.push((url.clone(), if pr_node.is_null() { None } else { Some(pr_node) }));
        }
    }
    out
}

/// Falls back to probing each URL individually via [`CommandMergeProbe::probe`]
/// when the batched round trip itself failed in a way that isn't isolated
/// to a single PR (non-zero `gh` exit, or a response carrying a top-level
/// GraphQL `errors` array). This is what preserves per-PR isolation for
/// repository-level errors (a tracked PR's repo has been deleted, renamed,
/// or had access revoked) — `gh api graphql` exits non-zero for those the
/// same way it would for a genuine transport failure, but unlike a
/// transport failure, a single unresolvable repo must not stall
/// reconciliation for every other PR in the pass.
pub(crate) async fn probe_each_individually(
    urls: &[String],
) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
    let probe = CommandMergeProbe::new();
    let mut out = HashMap::new();
    for url in urls {
        let result = probe.probe(url).await.map_err(|err| err.to_string());
        out.insert(url.clone(), result);
    }
    out
}

/// Reshape one aliased `pullRequest(...)` node from [`probe_batch_via_graphql`]'s
/// batched response into the flat JSON document shape `gh pr view --json`
/// produces (`labels`/`reviews`/`statusCheckRollup` as plain arrays rather
/// than `{ nodes: [...] }` connections), so [`parse_probe_json`] can parse
/// it unmodified.
pub(crate) fn flatten_batched_pr_node(node: &serde_json::Value) -> serde_json::Value {
    let labels = node["labels"]["nodes"].as_array().cloned().unwrap_or_default();
    let reviews = node["reviews"]["nodes"].as_array().cloned().unwrap_or_default();
    let rollup = node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    serde_json::json!({
        "state": node["state"],
        "mergedAt": node["mergedAt"],
        "closedAt": node["closedAt"],
        "mergeable": node["mergeable"],
        "mergeStateStatus": node["mergeStateStatus"],
        "baseRefOid": node["baseRefOid"],
        "headRefOid": node["headRefOid"],
        "headRefName": node["headRefName"],
        "baseRefName": node["baseRefName"],
        "labels": labels,
        "statusCheckRollup": rollup,
        "reviewDecision": node["reviewDecision"],
        "reviews": reviews,
        "mergeQueueEntry": node["mergeQueueEntry"],
        "autoMergeRequest": node["autoMergeRequest"],
    })
}

/// One cached `ETag` for a commit combined-status REST resource, plus the
/// combined-state string that resource resolved to the last time it was
/// fetched. Replayed verbatim on a `304 Not Modified` response, since a
/// `304` body carries no content.
#[derive(Debug, Clone)]
pub(crate) struct CachedCommitStatus {
    pub(crate) etag: String,
    pub(crate) state: Option<String>,
}

/// Outcome of a conditional `gh api -i <path>` REST GET, classified from
/// the raw process output. `gh` exits non-zero on a `304` (it treats any
/// non-2xx as an error), so the outcome must be read from the `-i`
/// status line in stdout rather than from the process exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConditionalGetOutcome {
    /// A 2xx response with a body and the `ETag` header, if GitHub sent one.
    Modified { body: String, etag: Option<String> },
    /// A `304` — the resource is unchanged since the cached `ETag` was captured.
    NotModified,
    /// Anything else: a spawn error, a non-2xx/304 status, or a response
    /// `gh -i` didn't shape the way this parser expects.
    Failed,
}

/// Split `gh api -i`'s stdout (HTTP status line, headers, a blank line,
/// then the body) into `(status_code, etag_header, body)`. Pure so it is
/// unit-testable without shelling out. Returns `None` when the first line
/// isn't a parseable status line (e.g. empty/garbled output).
///
/// Uses `str::lines()` rather than splitting on a literal `"\n\n"` because
/// `gh`'s own status line is `\n`-terminated while the header block it
/// copies verbatim from the HTTP response is `\r\n`-terminated — `lines()`
/// normalizes both.
pub(crate) fn parse_include_response(stdout: &str) -> Option<(u16, Option<String>, String)> {
    let mut lines = stdout.lines();
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let mut etag = None;
    for line in lines.by_ref() {
        if line.is_empty() {
            break; // blank line separates headers from the body
        }
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("etag")
        {
            etag = Some(value.trim().to_owned());
        }
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    Some((status, etag, body.trim().to_owned()))
}

/// Classify a `gh api -i <path> [-H "If-None-Match: …"]` invocation's raw
/// output into a [`ConditionalGetOutcome`]. Ignores `output.status` (see
/// [`ConditionalGetOutcome`]) in favour of the parsed status line.
pub(crate) fn classify_conditional_output(output: &std::process::Output) -> ConditionalGetOutcome {
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_include_response(&stdout) {
        Some((304, _, _)) => ConditionalGetOutcome::NotModified,
        Some((status, etag, body)) if (200..300).contains(&status) => ConditionalGetOutcome::Modified { body, etag },
        _ => ConditionalGetOutcome::Failed,
    }
}

/// Apply a [`ConditionalGetOutcome`] for `api_path` against `cache`,
/// returning the resolved combined-state string. `NotModified` replays the
/// previously cached state without touching the cache; `Modified` parses
/// the fresh body and refreshes the cache entry when GitHub sent an
/// `ETag` (no `ETag` means nothing usable to conditionally request next
/// time, so the stale entry, if any, is left in place rather than wiped).
/// Pure w.r.t. the cache map so it is unit-testable without a live `gh`
/// call.
pub(crate) fn resolve_and_cache_combined_state(
    cache: &Mutex<HashMap<String, CachedCommitStatus>>,
    api_path: &str,
    outcome: ConditionalGetOutcome,
) -> Option<String> {
    match outcome {
        ConditionalGetOutcome::NotModified => cache
            .lock()
            .unwrap()
            .get(api_path)
            .and_then(|entry| entry.state.clone()),
        ConditionalGetOutcome::Modified { body, etag } => {
            let state = parse_combined_status_response(&body);
            if let Some(etag) = etag {
                cache.lock().unwrap().insert(
                    api_path.to_owned(),
                    CachedCommitStatus {
                        etag,
                        state: state.clone(),
                    },
                );
            }
            state
        }
        ConditionalGetOutcome::Failed => None,
    }
}

impl CommandMergeProbe {
    /// When `statusCheckRollup` is empty/null in `json_body`, fetches the
    /// legacy commit-status combined state (`pending` / `success` /
    /// `failure` / `error`) from GitHub's REST endpoint and returns it as
    /// a lowercase string. Returns `None` on any error, when the rollup
    /// is non-empty (the caller should rely on rollup data in that
    /// case), or when the commit has zero recorded statuses — GitHub
    /// reports `state:"pending"` even when `total_count == 0`, which
    /// would otherwise show up as a stuck yellow "waiting for CI" icon on
    /// PRs in repos with no checks configured.
    ///
    /// This is the one sub-signal in the probe fetched over REST rather
    /// than GraphQL, so it's the one that can use a conditional request:
    /// a cached `ETag` for this resource is sent as `If-None-Match`, and
    /// an unchanged commit's status comes back as a free `304` instead of
    /// a full re-fetch (doc: `github-event-detection-webhooks-vs-polling`,
    /// §9.2).
    async fn fetch_commit_combined_state_for_empty_rollup(&self, json_body: &str, pr_url: &str) -> Option<String> {
        let root: serde_json::Value = serde_json::from_str(json_body.trim()).ok()?;
        let rollup = root.get("statusCheckRollup").and_then(|v| v.as_array())?;
        if !rollup.is_empty() {
            return None; // non-empty rollup; use rollup data
        }
        let head_sha = root
            .get("headRefOid")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?;
        let repo = repo_from_pr_url(pr_url)?;
        let api_path = format!("repos/{repo}/commits/{head_sha}/status");

        let cached_etag = self
            .commit_status_cache
            .lock()
            .unwrap()
            .get(&api_path)
            .map(|entry| entry.etag.clone());
        let if_none_match = cached_etag.map(|etag| format!("If-None-Match: {etag}"));
        let mut args: Vec<&str> = vec![
            "api",
            &api_path,
            "-i",
            "--jq",
            "{state: .state, total_count: .total_count}",
        ];
        if let Some(header) = if_none_match.as_deref() {
            args.push("-H");
            args.push(header);
        }
        let output = gh_output(&args).await.ok()?;

        let outcome = classify_conditional_output(&output);
        resolve_and_cache_combined_state(&self.commit_status_cache, &api_path, outcome)
    }
}

/// Pure parser for GitHub's `repos/{owner}/{repo}/commits/{sha}/status`
/// response shape (`{state, total_count}`). A commit with zero recorded
/// statuses reports `state:"pending"` even though there is nothing to
/// wait on — keying on `total_count` collapses that case to `None` so
/// the caller treats the PR as `Clean` instead of stuck in-flight.
pub(crate) fn parse_combined_status_response(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let total_count = v.get("total_count").and_then(|t| t.as_u64()).unwrap_or(0);
    if total_count == 0 {
        return None;
    }
    let state = v.get("state").and_then(|s| s.as_str())?.trim().to_ascii_lowercase();
    if state.is_empty() { None } else { Some(state) }
}

/// Parse the raw JSON document `gh pr view --json …` returns into a
/// [`PrLifecycleProbe`]. Pure function so the parsing rules can be
/// unit-tested without shelling out. A document that fails to parse
/// is *not* treated as conflicting / failing — we fall back to an
/// `Open(clean)` shape so a malformed gh response can't fire a
/// false-positive blocked flip. Real failures (auth, network) come
/// through as `Err` from the shelling-out layer, not via this path.
///
/// `combined_state` is the optional result from the legacy commit-status
/// REST API (`pending` / `success` / `failure` / `error`). It is only
/// consulted when `statusCheckRollup` is empty — see
/// [`fetch_commit_combined_state_for_empty_rollup`].
pub(crate) fn parse_probe_json(url: &str, body: &str, combined_state: Option<&str>) -> Result<PrLifecycleProbe> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("`gh pr view {url}` returned an empty document"));
    }
    let root: serde_json::Value =
        serde_json::from_str(trimmed).with_context(|| format!("failed to parse `gh pr view {url}` JSON"))?;
    let raw_state = root.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let merged_at = root.get("mergedAt").and_then(|v| v.as_str()).unwrap_or("");
    let mergeable = root.get("mergeable").and_then(|v| v.as_str()).unwrap_or("");
    let merge_state_status = root.get("mergeStateStatus").and_then(|v| v.as_str()).unwrap_or("");
    let base_ref_oid = root
        .get("baseRefOid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let head_ref_oid = root
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let head_ref_name = root
        .get("headRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let base_ref_name = root
        .get("baseRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let labels = root
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("name").and_then(|n| n.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let rollup = root
        .get("statusCheckRollup")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // Per-org reclassification: a status check that GitHub reports as a
    // required CI check but that semantically gates merge on a human
    // approval signal (e.g. LinkedIn's `Owner Approval` / LI-ACL) is
    // partitioned out of the rollup before CI classification and fed
    // into the review-signal axis instead. Outside the configured orgs
    // the partition is a no-op and the rollup is classified normally.
    let owner = owner_from_pr_url(url).unwrap_or("");
    let review_signal_names = review_signal_checks_for_owner(owner);
    let (review_signal_leaves, ci_leaves): (Vec<serde_json::Value>, Vec<serde_json::Value>) = rollup
        .into_iter()
        .partition(|leaf| leaf_matches_check_name(leaf, review_signal_names));
    let ci = classify_ci(&ci_leaves, combined_state);
    let state = classify_state(raw_state, merged_at, mergeable, merge_state_status, ci);
    let review_signal = classify_review_signal(&review_signal_leaves);
    let review_decision = root.get("reviewDecision").and_then(|v| v.as_str()).unwrap_or("");
    let reviews = root
        .get("reviews")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let review = classify_review(review_decision, &reviews, review_signal);
    // `mergeQueueEntry` is non-null when the PR is in GitHub's merge queue.
    // Null, missing, or explicit JSON null → not in queue. When present, pull
    // the sub-state fields (state / position / enqueuedAt) too — the batched
    // GraphQL probe's `PR_PROBE_FIELDS` selection set requests them inline;
    // the single-PR `gh pr view` probe path stamps them separately (see
    // `CommandMergeProbe::probe`) since `mergeQueueEntry` isn't a `--json`
    // field there.
    let merge_queue_entry_node = root.get("mergeQueueEntry").filter(|v| !v.is_null());
    let in_merge_queue = merge_queue_entry_node.is_some();
    let merge_queue_entry_state = merge_queue_entry_node
        .and_then(|v| v.get("state"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let merge_queue_position = merge_queue_entry_node
        .and_then(|v| v.get("position"))
        .and_then(|v| v.as_i64());
    let merge_queue_enqueued_at = merge_queue_entry_node
        .and_then(|v| v.get("enqueuedAt"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    // `autoMergeRequest` is non-null while GitHub auto-merge is armed for
    // this PR (the "Merge When Ready" state before it reaches a merge
    // queue, or on repos with no merge queue at all). Null, missing, or
    // explicit JSON null → auto-merge not requested / already resolved.
    let auto_merge_request_node = root.get("autoMergeRequest").filter(|v| !v.is_null());
    let auto_merge_enabled = auto_merge_request_node.is_some();
    let auto_merge_enabled_at = auto_merge_request_node
        .and_then(|v| v.get("enabledAt"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok(PrLifecycleProbe {
        url: url.to_owned(),
        state,
        base_ref_oid,
        head_ref_oid,
        head_ref_name,
        base_ref_name,
        labels,
        review,
        in_merge_queue,
        merge_queue_entry_state,
        merge_queue_position,
        merge_queue_enqueued_at,
        raw_mergeable: mergeable.to_owned(),
        raw_merge_state_status: merge_state_status.to_owned(),
        auto_merge_enabled,
        auto_merge_enabled_at,
    })
}
