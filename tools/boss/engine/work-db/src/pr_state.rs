use super::*;

/// The PR number in `pr_url`, as the `i64` the durable rows store.
///
/// Thin `i64` adapter over [`boss_github::pr_url::pr_number_from_url`] —
/// the persistence layer parses PR URLs straight from the shared GitHub
/// crate rather than borrowing the merge poller's copy, so nothing here
/// depends on a sweep that itself reads these rows.
///
/// Named `stored_pr_number` (not `pr_number_from_url`) because the
/// lower-crate helper returns `Option<u64>` while durable rows store
/// `i64`; the distinct name keeps the storage adaptor from shadowing
/// the canonical parser at call sites that import both.
pub fn stored_pr_number(pr_url: &str) -> Option<i64> {
    boss_github::pr_url::pr_number_from_url(pr_url).map(|n| n as i64)
}

/// Trait for checking the live state of a GitHub PR URL.
///
/// Injected into `create_revision` so the gate can distinguish "open"
/// from "closed without merging" without hardcoding a `gh` call, which
/// would make unit tests depend on GitHub access. Production wires in
/// [`GhPrStateChecker`]; tests pass `FakePrStateChecker`.
pub trait PrStateChecker: Send + Sync {
    /// Return the live lifecycle state of the given PR URL.
    fn check(&self, pr_url: &str) -> Result<PrOpenState>;

    /// Lifecycle state plus the current head SHA and branch name, from one
    /// parameterised GraphQL query.
    ///
    /// Default implementation has no SHA or branch name (existing
    /// [`Self::check`]-only impls keep working). [`GhPrStateChecker`] fetches
    /// `headRefOid` and `headRefName` in the same call as `state,mergedAt`
    /// so supervisor recovery can refuse to collate stale leaf reports
    /// against a moved head without a second GitHub round-trip, and
    /// positioning recheck can require the same `head.ref` cube's
    /// `goto --pr` requires.
    fn inspect(&self, pr_url: &str) -> Result<PrInspect> {
        Ok(PrInspect {
            open_state: self.check(pr_url)?,
            head_sha: None,
            head_ref_name: None,
        })
    }

    /// Check base policy independently of lifecycle reads.
    fn base_verdict(&self, _pr_url: &str) -> Result<PrBaseVerdict> {
        Ok(PrBaseVerdict::NotChecked)
    }

    /// Hermetic compare response used only by the review-service test fixtures.
    #[cfg(any(test, feature = "test-support"))]
    fn merged_parent_deletions(
        &self,
        _repo_slug: &str,
        _head_before: &str,
        _base_sha: &str,
        _head_after: &str,
    ) -> Vec<String> {
        Vec::new()
    }
}

/// Default-branch policy observation, separate from lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrBaseVerdict {
    NotChecked,
    DefaultBranch,
    Refused(String),
}

/// Snapshot returned by [`PrStateChecker::inspect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInspect {
    pub open_state: PrOpenState,
    pub head_sha: Option<String>,
    /// GitHub `headRefName`. Cube's `goto --pr` refuses when this is missing,
    /// so callers that decide whether positioning would succeed must require
    /// a non-empty name rather than treating a SHA alone as sufficient.
    pub head_ref_name: Option<String>,
}

/// Lifecycle state returned by [`PrStateChecker::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrOpenState {
    Open,
    Merged,
    ClosedUnmerged,
}

/// Coarse GitHub PR lifecycle classification derivable purely from the
/// `(state, mergedAt)` pair returned by the GraphQL PR query.
///
/// This is the single source of truth for the merged / closed-unmerged /
/// open tri-state. Both the merge poller
/// (`boss_engine::merge_poller::classify_state`) and the revision gate
/// ([`map_gh_state`]) map from this shared classification into their own
/// richer enums (`boss_engine::merge_poller::PrLifecycleState` /
/// [`PrOpenState`]), so the (state, mergedAt) decision lives in exactly
/// one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrMergeClass {
    /// `state == MERGED`, or a non-empty `mergedAt` timestamp is present.
    Merged,
    /// `state == CLOSED` and the PR was not merged.
    ClosedUnmerged,
    /// Still open, or an unknown / empty state treated as still-open.
    Open,
}

/// Classify a GitHub PR's `(state, mergedAt)` fields into the shared
/// [`PrMergeClass`] tri-state.
///
/// Rules:
///   - `state == MERGED` (case-insensitive) **or** a non-empty,
///     non-`"null"` `mergedAt` → [`PrMergeClass::Merged`].
///   - `state == CLOSED` (case-insensitive), not merged →
///     [`PrMergeClass::ClosedUnmerged`].
///   - anything else (including a missing / empty / unrecognized state)
///     → [`PrMergeClass::Open`].
///
/// Matching is case-insensitive so `gh`'s canonical uppercase values and
/// any lowercase drift map alike. `merged_at` is treated as absent when
/// it is empty or the literal string `"null"` — a JSON `null` surfaces
/// either as an absent field (empty here) or, when stringified, as the
/// text `null`.
pub fn classify_pr_merge_state(state: &str, merged_at: &str) -> PrMergeClass {
    let merged_at_present = !merged_at.is_empty() && !merged_at.eq_ignore_ascii_case("null");
    if state.eq_ignore_ascii_case("MERGED") || merged_at_present {
        PrMergeClass::Merged
    } else if state.eq_ignore_ascii_case("CLOSED") {
        PrMergeClass::ClosedUnmerged
    } else {
        PrMergeClass::Open
    }
}

/// Production implementation: reads PR and repository fields with GraphQL.
///
/// Deletion observations are a separate, required input to verdict storage.
pub struct GhPrStateChecker;

impl PrStateChecker for GhPrStateChecker {
    fn check(&self, pr_url: &str) -> Result<PrOpenState> {
        Ok(self.inspect(pr_url)?.open_state)
    }

    fn inspect(&self, pr_url: &str) -> Result<PrInspect> {
        let repository = query_pr(pr_url)?;
        Ok(map_gh_inspect(&repository["pullRequest"]))
    }

    fn base_verdict(&self, pr_url: &str) -> Result<PrBaseVerdict> {
        map_base_verdict(&query_pr(pr_url)?)
    }
}

/// Hard timeout for one GraphQL PR query (`gh api graphql`), shared by the
/// lifecycle `check`/`inspect` readers and the base-policy lookup made from
/// async sweeps.
pub const PR_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Run [`PrStateChecker::base_verdict`] off the async runtime with a deadline.
///
/// The production checker also bounds and kills its `gh` child; running it
/// off-thread keeps the async runtime responsive during that blocking wait.
/// A timeout or a panicked blocking task is an `Err`: callers treat it like
/// any other retryable lookup failure and leave their state untouched.
pub async fn bounded_base_verdict(
    checker: std::sync::Arc<dyn PrStateChecker>,
    pr_url: &str,
    budget: std::time::Duration,
) -> Result<PrBaseVerdict> {
    let url = pr_url.to_owned();
    let check = tokio::task::spawn_blocking(move || checker.base_verdict(&url));
    match tokio::time::timeout(budget, check).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => bail!("PR base check task failed for {pr_url}: {join_err}"),
        Err(_) => bail!("PR base check for {pr_url} exceeded {budget:?}"),
    }
}

/// `gh api graphql` arguments for the shared PR query.
fn pr_query_args(owner: &str, repo: &str, number: u64) -> Vec<String> {
    let query = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){defaultBranchRef{name} pullRequest(number:$number){state mergedAt headRefOid headRefName baseRefName}}}";
    vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={query}"),
        "-f".into(),
        format!("owner={owner}"),
        "-f".into(),
        format!("repo={repo}"),
        "-F".into(),
        format!("number={number}"),
    ]
}

/// Pure validation of the GraphQL response: the `repository` object, or an
/// error when GraphQL reported `errors` or the PR does not exist.
fn repository_from_response(value: &serde_json::Value, pr_url: &str) -> Result<serde_json::Value> {
    if value.get("errors").is_some() || !value["data"]["repository"]["pullRequest"].is_object() {
        bail!("PR query returned errors or no PR for {pr_url}: {value}");
    }
    Ok(value["data"]["repository"].clone())
}

/// Both readers use one parameterised query; policy never changes lifecycle.
///
/// Uses `scope_blocking_if_unattributed` rather than an unconditional scope:
/// a call issued from inside a more specific sweep (the merge poller's
/// sweep label, for one) must keep that attribution, and only an
/// unattributed call falls back to `PR_STATE_CHECK`.
fn query_pr(pr_url: &str) -> Result<serde_json::Value> {
    let (owner, repo, number) =
        boss_github::pr_url::parse_pr_url_parts(pr_url).with_context(|| format!("invalid PR URL: {pr_url}"))?;
    let args = pr_query_args(owner, repo, number);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = boss_gh_telemetry::scope_blocking_if_unattributed(boss_gh_telemetry::callers::PR_STATE_CHECK, || {
        boss_github::gh_runner::gh_output_blocking_timeout(&arg_refs, PR_QUERY_TIMEOUT)
    })
    .with_context(|| format!("failed to query PR {pr_url}"))?;
    if !output.status.success() {
        bail!(
            "PR query failed for {pr_url}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    repository_from_response(&value, pr_url)
}

fn map_base_verdict(repository: &serde_json::Value) -> Result<PrBaseVerdict> {
    let base = optional_trimmed_json_str(&repository["pullRequest"], "baseRefName")
        .context("PR query returned no base branch")?;
    let default = optional_trimmed_json_str(&repository["defaultBranchRef"], "name")
        .context("PR query returned no default branch")?;
    Ok(match boss_github::require_default_branch(Some(&base), Some(&default)) {
        Ok(()) => PrBaseVerdict::DefaultBranch,
        Err(reason) => PrBaseVerdict::Refused(reason),
    })
}

/// Pure mapping from the parsed GraphQL PR JSON (`state`, `mergedAt`) to a
/// [`PrOpenState`]. Extracted from [`GhPrStateChecker::check`] so the mapping
/// can be unit-tested without shelling out to `gh`.
///
/// Delegates the merged / closed / open decision to the shared
/// [`classify_pr_merge_state`] helper, so matching is case-insensitive
/// (`gh`'s canonical uppercase values and any lowercase drift map alike)
/// and a non-empty `mergedAt` counts as merged even when `state` is not
/// `MERGED`. A missing or unrecognized state falls through to
/// [`PrOpenState::Open`].
fn map_gh_state(v: &serde_json::Value) -> PrOpenState {
    let state = v["state"].as_str().unwrap_or("");
    let merged_at = v["mergedAt"].as_str().unwrap_or("");
    match classify_pr_merge_state(state, merged_at) {
        PrMergeClass::Merged => PrOpenState::Merged,
        PrMergeClass::ClosedUnmerged => PrOpenState::ClosedUnmerged,
        PrMergeClass::Open => PrOpenState::Open,
    }
}

/// Pure mapping from the GraphQL PR JSON (`state`, `mergedAt`, `headRefOid`, `headRefName`).
/// Empty / missing / non-string `headRefOid` or `headRefName` is `None` so a
/// caller that cannot confirm the live SHA or branch name does not treat
/// "unknown" as positionable.
fn map_gh_inspect(v: &serde_json::Value) -> PrInspect {
    let head_sha = optional_trimmed_json_str(v, "headRefOid");
    let head_ref_name = optional_trimmed_json_str(v, "headRefName");
    PrInspect {
        open_state: map_gh_state(v),
        head_sha,
        head_ref_name,
    }
}

fn optional_trimmed_json_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v[key]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A `PrStateChecker` that returns a fixed, already-observed state without
/// issuing a `gh` call. Used by the merge-conflict producer (Phase 3): the
/// poller has *just* probed the PR live and is acting on an
/// `OpenPrMergeability::Conflict` result, which by construction means the PR
/// is open. Feeding that observation straight into the `create_revision`
/// gate reuses the gate's parent-revisable invariant (R4) while avoiding a
/// redundant — and, in tests, non-hermetic — GraphQL round-trip.
pub struct StaticPrStateChecker(pub PrOpenState);

impl PrStateChecker for StaticPrStateChecker {
    fn check(&self, _pr_url: &str) -> Result<PrOpenState> {
        Ok(self.0.clone())
    }
}

/// Test double: returns a preset state for known PR URLs.
#[derive(Clone)]
#[cfg(any(test, feature = "test-support"))]
pub struct FakePrStateChecker {
    pub base: PrBaseVerdict,
    pub states: std::collections::HashMap<String, PrOpenState>,
    pub default: PrOpenState,
    pub head_sha: Option<String>,
    pub head_ref_name: Option<String>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakePrStateChecker {
    pub fn always(state: PrOpenState) -> Self {
        Self {
            base: PrBaseVerdict::NotChecked,
            states: Default::default(),
            default: state,
            head_sha: None,
            head_ref_name: None,
        }
    }
    pub fn with_base(mut self, verdict: PrBaseVerdict) -> Self {
        self.base = verdict;
        self
    }
    pub fn with(mut self, url: &str, state: PrOpenState) -> Self {
        self.states.insert(url.to_owned(), state);
        self
    }
    pub fn with_head_sha(mut self, sha: impl Into<String>) -> Self {
        self.head_sha = Some(sha.into());
        self
    }
    pub fn with_head_ref_name(mut self, name: impl Into<String>) -> Self {
        self.head_ref_name = Some(name.into());
        self
    }
}

#[cfg(any(test, feature = "test-support"))]
impl PrStateChecker for FakePrStateChecker {
    fn base_verdict(&self, _pr_url: &str) -> Result<PrBaseVerdict> {
        Ok(self.base.clone())
    }

    fn check(&self, pr_url: &str) -> Result<PrOpenState> {
        Ok(self.states.get(pr_url).cloned().unwrap_or(self.default.clone()))
    }

    fn inspect(&self, pr_url: &str) -> Result<PrInspect> {
        Ok(PrInspect {
            open_state: self.check(pr_url)?,
            head_sha: self.head_sha.clone(),
            head_ref_name: self.head_ref_name.clone(),
        })
    }
}

/// Errors produced by the create-time revision gate.
#[derive(Debug, thiserror::Error)]
pub enum RevisionGateError {
    #[error(
        "T{short_id} has no PR yet; a revision targets an existing open PR. \
         Wait for T{short_id} to reach review, or file a normal follow-up chore."
    )]
    NoPr { short_id: i64 },

    #[error(
        "T{short_id}'s PR (#{pr_number}) is already merged; revisions only apply to \
         open, unmerged PRs. File a new chore against main instead."
    )]
    Merged { short_id: i64, pr_number: i64 },

    #[error(
        "T{short_id}'s PR (#{pr_number}) is closed without merging; \
         there is no open PR to revise."
    )]
    ClosedUnmerged { short_id: i64, pr_number: i64 },
}

impl RevisionGateError {
    pub fn no_pr(task: &Task) -> Self {
        Self::NoPr {
            short_id: task.short_id.unwrap_or(0),
        }
    }
    pub fn merged(task: &Task, pr_url: &str) -> Self {
        Self::Merged {
            short_id: task.short_id.unwrap_or(0),
            pr_number: stored_pr_number(pr_url).unwrap_or(0),
        }
    }
    pub fn closed(task: &Task, pr_url: &str) -> Self {
        Self::ClosedUnmerged {
            short_id: task.short_id.unwrap_or(0),
            pr_number: stored_pr_number(pr_url).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_policy_does_not_change_lifecycle_reads() {
        for state in ["OPEN", "MERGED", "CLOSED"] {
            let repository = json!({
                "defaultBranchRef": {"name": "main"},
                "pullRequest": {"state": state, "baseRefName": "feature", "headRefOid": "abc"}
            });
            assert!(matches!(
                map_base_verdict(&repository).unwrap(),
                PrBaseVerdict::Refused(_)
            ));
            let inspect = map_gh_inspect(&repository["pullRequest"]);
            assert_eq!(inspect.open_state, map_gh_state(&json!({"state": state})));
            assert_eq!(inspect.head_sha.as_deref(), Some("abc"));
        }
        assert_eq!(
            map_base_verdict(&json!({
                "defaultBranchRef": {"name": "trunk"},
                "pullRequest": {"baseRefName": "trunk"}
            }))
            .unwrap(),
            PrBaseVerdict::DefaultBranch
        );
        assert!(map_base_verdict(&json!({"pullRequest": {"baseRefName": "main"}})).is_err());
        assert!(map_base_verdict(&json!({"defaultBranchRef": {"name": "main"}})).is_err());
        assert_eq!(
            StaticPrStateChecker(PrOpenState::Open).base_verdict("unused").unwrap(),
            PrBaseVerdict::NotChecked
        );
    }

    #[test]
    fn query_args_carry_owner_repo_and_number() {
        let args = pr_query_args("acme", "widgets", 42);
        assert_eq!(&args[..3], ["api", "graphql", "-f"]);
        assert!(args[3].starts_with("query=query($owner:String!"));
        assert!(args.contains(&"owner=acme".to_owned()));
        assert!(args.contains(&"repo=widgets".to_owned()));
        let n = args.iter().position(|a| a == "number=42").unwrap();
        assert_eq!(args[n - 1], "-F");
    }

    #[test]
    fn response_validation_rejects_errors_and_missing_pr() {
        let url = "https://github.com/a/b/pull/1";
        let ok = json!({"data": {"repository": {"pullRequest": {"state": "OPEN"}}}});
        assert_eq!(
            repository_from_response(&ok, url).unwrap()["pullRequest"]["state"],
            "OPEN"
        );
        let no_pr = json!({"data": {"repository": {"pullRequest": null}}});
        assert!(repository_from_response(&no_pr, url).is_err());
        let errors =
            json!({"errors": [{"message": "boom"}], "data": {"repository": {"pullRequest": {"state": "OPEN"}}}});
        assert!(repository_from_response(&errors, url).is_err());
        assert!(repository_from_response(&json!({"data": null}), url).is_err());
    }

    struct Hanging(std::sync::Mutex<std::sync::mpsc::Receiver<()>>);
    impl PrStateChecker for Hanging {
        fn check(&self, _: &str) -> Result<PrOpenState> {
            Ok(PrOpenState::Open)
        }
        fn base_verdict(&self, _: &str) -> Result<PrBaseVerdict> {
            let _ = self.0.lock().unwrap().recv();
            Ok(PrBaseVerdict::DefaultBranch)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_base_verdict_times_out_on_a_stalled_checker() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let checker: std::sync::Arc<dyn PrStateChecker> = std::sync::Arc::new(Hanging(std::sync::Mutex::new(wait)));
        let (result, ()) = tokio::join!(bounded_base_verdict(checker, "u", PR_QUERY_TIMEOUT), async {
            tokio::task::yield_now().await;
            tokio::time::advance(PR_QUERY_TIMEOUT + std::time::Duration::from_secs(1)).await;
        });
        drop(release);
        assert!(result.unwrap_err().to_string().contains("exceeded"));
    }

    #[test]
    fn merged_state_maps_to_merged() {
        assert_eq!(
            map_gh_state(&json!({"state": "MERGED", "mergedAt": "2026-01-01T00:00:00Z"})),
            PrOpenState::Merged
        );
    }

    #[test]
    fn closed_state_maps_to_closed_unmerged() {
        assert_eq!(
            map_gh_state(&json!({"state": "CLOSED", "mergedAt": null})),
            PrOpenState::ClosedUnmerged
        );
    }

    #[test]
    fn open_state_maps_to_open() {
        assert_eq!(
            map_gh_state(&json!({"state": "OPEN", "mergedAt": null})),
            PrOpenState::Open
        );
    }

    #[test]
    fn inspect_extracts_head_ref_oid() {
        assert_eq!(
            map_gh_inspect(&json!({"state": "OPEN", "mergedAt": null, "headRefOid": "abc123"})),
            PrInspect {
                open_state: PrOpenState::Open,
                head_sha: Some("abc123".to_owned()),
                head_ref_name: None,
            }
        );
    }

    #[test]
    fn inspect_missing_or_empty_head_ref_oid_is_none() {
        assert_eq!(map_gh_inspect(&json!({"state": "OPEN"})).head_sha, None);
        assert_eq!(
            map_gh_inspect(&json!({"state": "OPEN", "headRefOid": ""})).head_sha,
            None
        );
        assert_eq!(
            map_gh_inspect(&json!({"state": "OPEN", "headRefOid": "   "})).head_sha,
            None
        );
    }

    #[test]
    fn inspect_extracts_head_ref_name() {
        assert_eq!(
            map_gh_inspect(&json!({
                "state": "OPEN",
                "mergedAt": null,
                "headRefOid": "abc123",
                "headRefName": "feature"
            })),
            PrInspect {
                open_state: PrOpenState::Open,
                head_sha: Some("abc123".to_owned()),
                head_ref_name: Some("feature".to_owned()),
            }
        );
    }

    #[test]
    fn inspect_open_pr_with_sha_and_no_head_name_is_not_named() {
        let sha = "a".repeat(40);
        let inspect = map_gh_inspect(&json!({
            "state": "OPEN",
            "mergedAt": null,
            "headRefOid": sha
        }));
        assert_eq!(inspect.open_state, PrOpenState::Open);
        assert_eq!(inspect.head_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(inspect.head_ref_name, None);
        assert_eq!(
            map_gh_inspect(&json!({"state": "OPEN", "headRefName": ""})).head_ref_name,
            None
        );
        assert_eq!(
            map_gh_inspect(&json!({"state": "OPEN", "headRefName": "   "})).head_ref_name,
            None
        );
    }

    #[test]
    fn lowercase_states_still_map_correctly() {
        // check() upper-cases before matching, so lowercase inputs map like their canonical forms.
        assert_eq!(map_gh_state(&json!({"state": "merged"})), PrOpenState::Merged);
        assert_eq!(map_gh_state(&json!({"state": "closed"})), PrOpenState::ClosedUnmerged);
        assert_eq!(map_gh_state(&json!({"state": "open"})), PrOpenState::Open);
    }

    #[test]
    fn unrecognized_state_maps_to_open() {
        assert_eq!(map_gh_state(&json!({"state": "SOMETHING_ELSE"})), PrOpenState::Open);
    }

    #[test]
    fn missing_state_maps_to_open() {
        assert_eq!(map_gh_state(&json!({"mergedAt": null})), PrOpenState::Open);
    }

    #[test]
    fn non_string_state_maps_to_open() {
        // A non-string `state` value can't be read as a str, so it falls through to Open.
        assert_eq!(map_gh_state(&json!({"state": 42})), PrOpenState::Open);
    }

    #[test]
    fn non_empty_merged_at_maps_to_merged_even_if_state_not_merged() {
        // Previously map_gh_state ignored `mergedAt`; now a non-empty
        // timestamp classifies as Merged regardless of `state`, matching
        // the merge poller's `classify_state`.
        assert_eq!(
            map_gh_state(&json!({"state": "OPEN", "mergedAt": "2026-01-01T00:00:00Z"})),
            PrOpenState::Merged
        );
    }

    #[test]
    fn classify_pr_merge_state_covers_the_tri_state() {
        // state == MERGED
        assert_eq!(classify_pr_merge_state("MERGED", ""), PrMergeClass::Merged);
        // non-empty mergedAt wins even when state isn't MERGED
        assert_eq!(
            classify_pr_merge_state("OPEN", "2026-01-01T00:00:00Z"),
            PrMergeClass::Merged
        );
        // closed and not merged
        assert_eq!(classify_pr_merge_state("CLOSED", ""), PrMergeClass::ClosedUnmerged);
        assert_eq!(classify_pr_merge_state("CLOSED", "null"), PrMergeClass::ClosedUnmerged);
        // open / unknown / empty
        assert_eq!(classify_pr_merge_state("OPEN", ""), PrMergeClass::Open);
        assert_eq!(classify_pr_merge_state("", ""), PrMergeClass::Open);
        assert_eq!(classify_pr_merge_state("SOMETHING_ELSE", ""), PrMergeClass::Open);
    }

    #[test]
    fn checker_attribution_yields_to_an_active_merge_poller_sweep_scope() {
        // GhPrStateChecker::check attributes its `gh` call via
        // `scope_blocking_if_unattributed(PR_STATE_CHECK, ...)` rather than
        // an unconditional `scope_blocking`, so a call issued from inside
        // the sweep's own `merge_poller.sweep` scope stays billed to the
        // sweep instead of being overridden to `pr_state_check`.
        let observed = boss_gh_telemetry::scope_blocking(boss_gh_telemetry::callers::MERGE_POLLER_SWEEP, || {
            boss_gh_telemetry::scope_blocking_if_unattributed(
                boss_gh_telemetry::callers::PR_STATE_CHECK,
                boss_gh_telemetry::current_caller,
            )
        });
        assert_eq!(observed, boss_gh_telemetry::callers::MERGE_POLLER_SWEEP);

        // Called with no ambient scope, it still self-attributes.
        let observed = boss_gh_telemetry::scope_blocking_if_unattributed(
            boss_gh_telemetry::callers::PR_STATE_CHECK,
            boss_gh_telemetry::current_caller,
        );
        assert_eq!(observed, boss_gh_telemetry::callers::PR_STATE_CHECK);
    }

    #[test]
    fn classify_pr_merge_state_is_case_insensitive() {
        assert_eq!(classify_pr_merge_state("merged", ""), PrMergeClass::Merged);
        assert_eq!(classify_pr_merge_state("closed", "null"), PrMergeClass::ClosedUnmerged);
        assert_eq!(classify_pr_merge_state("open", ""), PrMergeClass::Open);
        // "null" in any case is treated as absent mergedAt.
        assert_eq!(classify_pr_merge_state("OPEN", "NULL"), PrMergeClass::Open);
    }
}
