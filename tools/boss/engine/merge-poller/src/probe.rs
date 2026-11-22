use super::*;

pub use boss_observation_contracts::{
    MergeProbe, NoopMergeProbe, OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleProbe, PrLifecycleState,
    PrReviewState, TrunkQueueCheckFailure,
};

/// `MergeProbe` that shells out to `gh pr view <url> --json …`.
#[derive(Default)]
pub struct CommandMergeProbe {
    #[cfg(test)]
    pub(crate) test_runner: Option<TestProbeRunner>,
    /// ETag cache for the commit combined-status REST sub-signal
    /// (`repos/{owner}/{repo}/commits/{sha}/status`, fetched by
    /// [`Self::fetch_commit_combined_state_for_empty_rollup`]), keyed by
    /// the API path. A cached entry lets the next identical request carry
    /// `If-None-Match` and get back a `304` — which costs no primary REST
    /// quota — instead of re-fetching and re-parsing an unchanged
    /// resource (doc: `github-event-detection-webhooks-vs-polling`, §9.2).
    commit_status_cache: Mutex<HashMap<String, CachedCommitStatus>>,
}

tokio::task_local! { static RATE_LIMIT_REJECTED: std::cell::Cell<bool>; }

type BoxedBatchProbe<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = HashMap<String, std::result::Result<PrLifecycleProbe, String>>> + Send + 'a>,
>;

#[cfg(test)]
pub(crate) type TestProbeRunner = Arc<dyn Fn(Vec<String>) -> TestProbeOutput + Send + Sync>;
#[cfg(test)]
type TestProbeOutput =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<std::process::Output>> + Send>>;

impl std::fmt::Debug for CommandMergeProbe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CommandMergeProbe").finish_non_exhaustive()
    }
}

impl CommandMergeProbe {
    async fn output(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        #[cfg(test)]
        if let Some(runner) = &self.test_runner {
            return runner(args.iter().map(|s| (*s).to_owned()).collect()).await;
        }
        gh_output(args).await
    }

    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MergeProbe for CommandMergeProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
        let output = self.output(&[
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
            "state,mergedAt,closedAt,mergeable,mergeStateStatus,baseRefOid,headRefOid,headRefName,baseRefName,labels,statusCheckRollup,reviewDecision,reviews,autoMergeRequest,mergeCommit,updatedAt",
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
        let (owner, repo, _) = parse_pr_url_parts(pr_url).context("invalid GitHub PR URL")?;
        let default_branch = boss_github::trees::fetch_default_branch_cached(owner, repo).await?;
        let mut document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        document["defaultBranch"] = default_branch.into();
        let stdout = document.to_string();
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
/// `pr_merge_queue_entry` — because `mergeQueueEntry` isn't a `--json` field in
/// all `gh` versions; here we just ask GraphQL for it inline, since we're
/// already hand-building the query).
///
/// GraphQL bills by node count (~cost = nodes / 100), so the connection
/// `first`/`last` caps here are the dominant per-PR lever on quota spend for
/// the *batched* sweep — a batch of `N` PRs costs `ceil(51N/100)` points at
/// the caps below, against `ceil(81N/100)` before them. (It is not a lever
/// for a single-PR query, where the 1-point floor dominates whatever the
/// node count; the fix for that path is to stop issuing single-PR queries —
/// see [`reconcile_batch`].)
///
/// The caps were re-cut against the 60 most recent PRs in the repo this
/// poller actually watches: **max 0 labels, max 12 reviews, exactly 3 rollup
/// contexts** across all of them. They are deliberately tight rather than
/// blanket `first: 100`:
///
/// - `labels(first: 10)` — boss's own label vocabulary (`blocked:*`,
///   priority, opt-out flags) is a handful of names, no sampled PR carried
///   any, and only the label *set* is consumed ([`parse_probe_json`]).
/// - `reviews(last: 10)` — the merge-gating decision is read from the
///   authoritative `reviewDecision` field, not this array; the array only
///   supplies the tooltip's reviewer login list ([`classify_review`]), which
///   takes the latest state *per author*. Truncating from the oldest end is
///   therefore the harmless direction: the only casualty is an author whose
///   sole review is older than the 10 most recent, on a PR busy enough to
///   have more than 10 (1 of 60 sampled).
/// - `contexts(first: 30)` — the CI rollup on the head commit, and
///   deliberately **not** trimmed further even though the sampled PRs all
///   carry 3. This connection is the only one whose truncation is not
///   cosmetic: a dropped failing context reads as a *green* rollup, which
///   would move a task to Review on a red PR. The asymmetry (a wasted node
///   budget versus a false green) is worth ~20 nodes.
///
/// Together these cut a typical probe from ~300 requested nodes to ~51 —
/// roughly a 6x GraphQL-cost reduction with no behavioural loss for any
/// realistically-shaped PR.
pub(crate) const PR_PROBE_FIELDS: &str = concat!(
    "repository { defaultBranchRef { name } } ",
    "state mergedAt closedAt mergeable mergeStateStatus baseRefOid headRefOid headRefName baseRefName ",
    "labels(first: 10) { nodes { name } } ",
    "reviewDecision reviews(last: 10) { nodes { author { login } state } } ",
    "mergeQueueEntry { state position enqueuedAt } ",
    "autoMergeRequest { enabledAt } ",
    "mergeCommit { oid } updatedAt ",
    "commits(last: 1) { nodes { commit { oid statusCheckRollup { contexts(first: 30) { nodes { ",
    // `completedAt` is requested for one reader only: it is the episode
    // discriminator `trunk_queue_adopt` keys adoption on (a head sha is not
    // one — a PR can be queued twice on an unchanged head) and the only
    // datum that says how old an eviction is. It is a scalar on a node
    // already being fetched, so it costs no extra GraphQL nodes.
    "__typename ... on CheckRun { name status conclusion detailsUrl startedAt completedAt } ",
    "... on StatusContext { context state targetUrl createdAt } } } } } } }",
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
/// handling), is resolved independently of its siblings. Query-wide errors,
/// malformed JSON and non-zero exits without a usable body retry in halves,
/// reaching individual probes only at singleton leaves. The shared deadline
/// bounds every await; completed results survive while unfinished URLs return
/// retryable errors for a later pass. Spawn failures and rate-limit rejections
/// return retryable errors without amplifying requests.
impl CommandMergeProbe {
    pub(crate) async fn probe_batch_via_graphql(
        &self,
        pr_urls: &[String],
    ) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        let order = crate::probe_budget::CONTEXT
            .try_with(|c| c.order.lock().unwrap().order(pr_urls))
            .unwrap_or_else(|_| pr_urls.to_vec());
        let results = crate::probe_budget::FALLBACK
            .scope(Mutex::new(Default::default()), async {
                let _report = crate::probe_budget::FallbackReport;
                RATE_LIMIT_REJECTED
                    .scope(
                        std::cell::Cell::new(false),
                        self.probe_batch_with_deadline(&order, crate::probe_budget::deadline()),
                    )
                    .await
            })
            .await;
        let _ = crate::probe_budget::CONTEXT.try_with(|c| c.order.lock().unwrap().observe(&results));
        results
    }

    /// Re-runs a batch that failed with no usable response (or with an error
    /// naming no PR) as two half-size batches, recursing until a half
    /// succeeds or is a single PR (which is then probed on its own). A heavy
    /// aliased query that times out as a whole usually succeeds at half the
    /// size, so this costs a handful of batched calls instead of `N` serial
    /// per-PR calls. Every call shares `deadline`: once it passes, PRs not yet
    /// reached come back as retryable errors and the next pass tries again.
    fn probe_batch_bisect<'a>(&'a self, order: &'a [String], deadline: tokio::time::Instant) -> BoxedBatchProbe<'a> {
        Box::pin(async move {
            let _fallback = crate::probe_budget::start_fallback(order);
            if order.len() <= 1 {
                return self.probe_each_individually(order, deadline).await;
            }
            let (left, right) = order.split_at(order.len() / 2);
            let mut out = self.probe_batch_with_deadline(left, deadline).await;
            if RATE_LIMIT_REJECTED.try_with(|flag| flag.get()).unwrap_or(false) {
                out.extend(right.iter().map(|url| {
                    (
                        url.clone(),
                        Err("GitHub API rate limit exceeded; retrying next pass".to_owned()),
                    )
                }));
            } else {
                out.extend(self.probe_batch_with_deadline(right, deadline).await);
            }
            out
        })
    }

    async fn probe_batch_with_deadline(
        &self,
        pr_urls: &[String],
        deadline: tokio::time::Instant,
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
            "merge poller: batched probe — one GraphQL query for this batch",
        );

        if tokio::time::Instant::now() >= deadline {
            let msg = "batched probe fallback budget exhausted; retrying next pass".to_owned();
            for url in &order {
                out.insert(url.clone(), Err(msg.clone()));
            }
            return out;
        }
        crate::probe_budget::attend(&order);
        let output = match crate::probe_budget::bounded(
            deadline,
            self.output(&["api", "graphql", "-f", &format!("query={query}")]),
        )
        .await
        {
            Ok(output) => output,
            Err(err) => {
                for url in &order {
                    out.insert(url.clone(), Err(err.clone()));
                }
                return out;
            }
        };
        let body: serde_json::Value = match output {
            Ok(o) if o.status.success() => match serde_json::from_slice(&o.stdout) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(%err, "merge poller: invalid batched probe response; retrying in halves");
                    out.extend(self.probe_batch_bisect(&order, deadline).await);
                    return out;
                }
            },
            // A non-zero exit here isn't necessarily a transport failure: `gh
            // api graphql` also exits non-zero whenever the response carries a
            // top-level `errors` array, which GitHub returns (alongside HTTP
            // 200 and a null node) whenever a `repository(...)` or
            // `pullRequest(...)` alias can't be resolved (repo deleted/renamed/
            // access revoked, PR deleted or transferred). Failing every URL in
            // the pass for that case would let one tracked PR that has gone
            // away stall reconciliation for every other PR indefinitely, since
            // `run_one_pass` rebuilds the same probe set every pass.
            //
            // `gh` still prints the response body in that case, and the body
            // still carries every node GitHub *could* resolve. It is parsed
            // below like a successful response, and only the PRs an error
            // names are re-probed individually — see [`batch_error_scope`].
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
                    //
                    // Routed through `record_rate_limit_remaining` rather
                    // than storing into `RATE_LIMIT_REMAINING` directly: a
                    // bare store pins the throttle multiplier at 8.0 while
                    // emitting no threshold-crossing line, so the trace's
                    // `throttle_factor` readings stop being a faithful record
                    // of the multiplier actually in force.
                    record_rate_limit_remaining(0, "rate_limit_rejection");
                    let _ = RATE_LIMIT_REJECTED.try_with(|flag| flag.set(true));
                    let msg = "gh api graphql rejected: GitHub API rate limit exceeded".to_owned();
                    for url in &order {
                        out.insert(url.clone(), Err(msg.clone()));
                    }
                    return out;
                }
                match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    Ok(v) if v.get("data").is_some_and(|data| data.is_object()) => v,
                    _ => {
                        // No usable body identifies affected PRs. Retry in
                        // halves, reaching individual probes only at leaves,
                        // with every await bounded by the shared deadline.
                        tracing::warn!(
                            stderr = %stderr.trim(),
                            pr_count = order.len(),
                            "merge poller: batched probe graphql call failed with no usable response — \
                             retrying in halves",
                        );
                        out.extend(self.probe_batch_bisect(&order, deadline).await);
                        return out;
                    }
                }
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

        // A response can carry a top-level `errors` array alongside partial
        // `data` (one PR or repo alias doesn't resolve, the rest do). Only the
        // PRs an error names are untrusted; the others are read from this
        // response as usual. Re-probing every PR individually would cost two
        // or three serial `gh` calls per tracked PR, so only the named PRs
        // are re-probed.
        let untrusted: std::collections::HashSet<String> = match batch_error_scope(&body, &alias_map) {
            BatchErrorScope::Clean => Default::default(),
            BatchErrorScope::Urls(urls) => {
                tracing::info!(
                    errors = %body["errors"],
                    affected = urls.len(),
                    pr_count = order.len(),
                    "merge poller: batched probe graphql response carried errors for some PRs — \
                     probing only those individually",
                );
                urls
            }
            BatchErrorScope::Whole => {
                tracing::warn!(
                    errors = %body["errors"],
                    pr_count = order.len(),
                    "merge poller: batched probe graphql response carried an error that names no PR — \
                     retrying in halves",
                );
                out.extend(self.probe_batch_bisect(&order, deadline).await);
                return out;
            }
        };

        let nodes: HashMap<_, _> = walk_batch_response(&body, &alias_map).into_iter().collect();
        let mut pending = HashMap::new();
        for url in &order {
            let pr_node = nodes.get(url).copied().flatten();
            if untrusted.contains(url) {
                pending.insert(url.clone(), None);
                continue;
            }
            let Some(pr_node) = pr_node else {
                // No PullRequest for this number any more (force-deleted,
                // transferred) — same "can't decide it's merged just because
                // we can't see it" fallback as `probe`'s 404 path.
                crate::probe_budget::completed(url);
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
            let flat = flatten_batched_pr_node(pr_node);
            if combined_status_path(&flat, url).is_some() {
                pending.insert(url.clone(), Some(flat.to_string()));
                continue;
            }
            let result = parse_probe_json(url, &flat.to_string(), None).map_err(|err| err.to_string());
            if result.is_ok() {
                crate::probe_budget::completed(url);
            }
            out.insert(url.clone(), result);
        }
        // Preserve every observation requiring no more I/O before spending
        // the allowance on status lookups or untrusted nodes.
        for url in &order {
            let Some(flat) = pending.remove(url) else { continue };
            let Some(flat) = flat else {
                out.extend(self.probe_each_individually(std::slice::from_ref(url), deadline).await);
                continue;
            };
            let combined_state = match self
                .fetch_commit_combined_state_with_deadline(&flat, url, deadline)
                .await
            {
                Ok(state) => state,
                Err(err) => {
                    out.insert(url.clone(), Err(err));
                    continue;
                }
            };
            let result = parse_probe_json(url, &flat, combined_state.as_deref()).map_err(|err| err.to_string());
            if result.is_ok() {
                crate::probe_budget::completed(url);
            }
            out.insert(url.clone(), result);
        }
        out
    }
}

/// Which PRs of a batched probe response its `errors` array leaves
/// untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchErrorScope {
    /// No errors: every node is read from the response.
    Clean,
    /// Every error names a repo or PR alias of this batch. Only these URLs
    /// need probing individually; the rest of the response stands.
    Urls(std::collections::HashSet<String>),
    /// An error names nothing in this batch (no `path`, or one that matches
    /// no alias — a query-level failure such as a server-side timeout), so
    /// no node can be trusted. Bisect, reaching single-PR probes only at
    /// leaves, within the shared deadline.
    Whole,
}

/// Scope a batched response's GraphQL `errors` to the PRs they name.
///
/// GitHub gives each error a `path` into the response: `["repo0"]` for a
/// repository that did not resolve, `["repo0", "pr3"]` for a pull request
/// that did not, and a longer path for a field inside one PR. Anything at or
/// below a PR alias taints that PR only — including a nested field error,
/// where the node is present but partly null and a null CI rollup would
/// otherwise read as green. A repo-level error taints every PR in that
/// repo. An error with no usable path taints the whole response.
///
/// Pure, so the scoping is unit-tested against synthetic responses.
pub(crate) fn batch_error_scope(body: &serde_json::Value, alias_map: &BatchAliasMap) -> BatchErrorScope {
    let Some(errors) = body
        .get("errors")
        .and_then(|e| e.as_array())
        .filter(|errors| !errors.is_empty())
    else {
        return BatchErrorScope::Clean;
    };
    let mut urls = std::collections::HashSet::new();
    for error in errors {
        let path: Vec<&str> = error
            .get("path")
            .and_then(|p| p.as_array())
            .map(|segments| segments.iter().filter_map(|segment| segment.as_str()).collect())
            .unwrap_or_default();
        let Some((_, pr_aliases)) = path
            .first()
            .and_then(|repo| alias_map.iter().find(|(repo_alias, _)| repo_alias == repo))
        else {
            return BatchErrorScope::Whole;
        };
        match path
            .get(1)
            .and_then(|pr| pr_aliases.iter().find(|(pr_alias, _)| pr_alias == pr))
        {
            Some((_, url)) => {
                urls.insert(url.clone());
            }
            // The repo itself, or something under it this batch did not ask
            // for by PR alias: every PR in the repo is untrusted.
            None => urls.extend(pr_aliases.iter().map(|(_, url)| url.clone())),
        }
    }
    BatchErrorScope::Urls(urls)
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
/// Also requests [`boss_gh_telemetry::RATE_LIMIT_SELECTION`] at the query's
/// top level so every batched call doubles as a quota reading, at zero extra
/// cost (querying `rateLimit` itself doesn't consume quota).
///
/// The selection is the full block (`cost limit remaining used resetAt`),
/// not just `remaining` as it used to be. `remaining` alone tells you the
/// shared budget moved but not who moved it or by how much; `cost` is the
/// per-call number that makes the spend attributable to this sweep rather
/// than inferable from the gap between two readings.
///
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

    let mut query = format!("{{ {}", boss_gh_telemetry::RATE_LIMIT_SELECTION);
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

impl CommandMergeProbe {
    /// Probe scoped errors and singleton bisection leaves in their incoming
    /// attendance order, within the same deadline as their parent batches.
    async fn probe_each_individually(
        &self,
        urls: &[String],
        deadline: tokio::time::Instant,
    ) -> HashMap<String, std::result::Result<PrLifecycleProbe, String>> {
        let _fallback = crate::probe_budget::start_fallback(urls);
        let mut out = HashMap::new();
        for url in urls {
            if tokio::time::Instant::now() < deadline {
                crate::probe_budget::attend(std::slice::from_ref(url));
            }
            let result = crate::probe_budget::bounded(deadline, self.probe(url))
                .await
                .and_then(|result| result.map_err(|err| err.to_string()));
            if result.is_ok() {
                crate::probe_budget::completed(url);
            }
            out.insert(url.clone(), result);
        }
        out
    }
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
        "defaultBranch": node["repository"]["defaultBranchRef"]["name"],
        "labels": labels,
        "statusCheckRollup": rollup,
        "ciHeadOid": node["commits"]["nodes"][0]["commit"]["oid"],
        "reviewDecision": node["reviewDecision"],
        "reviews": reviews,
        "mergeQueueEntry": node["mergeQueueEntry"],
        "autoMergeRequest": node["autoMergeRequest"],
        "mergeCommit": node["mergeCommit"],
        "updatedAt": node["updatedAt"],
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
        self.fetch_commit_combined_state_with_deadline(json_body, pr_url, crate::probe_budget::deadline())
            .await
            .ok()
            .flatten()
    }

    async fn fetch_commit_combined_state_with_deadline(
        &self,
        json_body: &str,
        pr_url: &str,
        deadline: tokio::time::Instant,
    ) -> std::result::Result<Option<String>, String> {
        let root: serde_json::Value = serde_json::from_str(json_body.trim()).map_err(|err| err.to_string())?;
        let Some(api_path) = combined_status_path(&root, pr_url) else {
            return Ok(None);
        };
        if tokio::time::Instant::now() >= deadline {
            return Err("probe deadline exhausted; retrying next pass".to_owned());
        }

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
        let mut measurement = crate::probe_budget::Measurement::start("probe_combined_status", 1);
        measurement.attempt();
        let _ = crate::probe_budget::CONTEXT.try_with(|c| c.order.lock().unwrap().attempt(pr_url));
        let output = crate::probe_budget::bounded(deadline, self.output(&args)).await?;
        let Ok(output) = output else { return Ok(None) };
        measurement.complete();

        let outcome = classify_conditional_output(&output);
        Ok(resolve_and_cache_combined_state(
            &self.commit_status_cache,
            &api_path,
            outcome,
        ))
    }
}

/// Terminal observations and populated rollups are complete without REST.
fn combined_status_path(root: &serde_json::Value, pr_url: &str) -> Option<String> {
    if root.get("state").and_then(|v| v.as_str()) != Some("OPEN")
        || !root.get("statusCheckRollup")?.as_array()?.is_empty()
    {
        return None;
    }
    let head = root.get("headRefOid")?.as_str().filter(|s| !s.is_empty())?;
    let repo = repo_from_pr_url(pr_url)?;
    Some(format!("repos/{repo}/commits/{head}/status"))
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
    // A force-push can race a batched probe. Never attribute a previous
    // commit's rollup to the newly observed PR head.
    if let Some(ci_head) = root.get("ciHeadOid").and_then(|v| v.as_str())
        && root.get("state").and_then(|v| v.as_str()) == Some("OPEN")
        && root.get("headRefOid").and_then(|v| v.as_str()) != Some(ci_head)
    {
        return Err(anyhow!("CI rollup commit does not match current PR head for {url}"));
    }
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
    // Read from the same post-partition leaf set `classify_ci` sees, so the
    // two readers of the Trunk leaf can never disagree about which leaves
    // were in scope. (The partition above only ever moves per-org review
    // signal checks, never Trunk's.)
    let trunk_queue_check_failure = trunk_queue_check_failure(&ci_leaves);
    let state = classify_state(raw_state, merged_at, mergeable, ci);
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
    // `mergeCommit` is non-null once the PR has merged (null while open, and
    // on the rare merged PR where GitHub omitted the field entirely — the
    // post-merge review trigger falls back to `head_ref_oid` for that case).
    let merge_commit_oid = root
        .get("mergeCommit")
        .filter(|v| !v.is_null())
        .and_then(|v| v.get("oid"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let updated_at = root
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok(PrLifecycleProbe {
        url: url.to_owned(),
        state,
        base_ref_oid,
        head_ref_oid,
        head_ref_name,
        base_branch_error: boss_github::require_default_branch(
            base_ref_name.as_deref(),
            root["defaultBranch"].as_str(),
        )
        .err(),
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
        trunk_queue_check_failure,
        merge_commit_oid,
        updated_at,
    })
}
