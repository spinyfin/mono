use std::path::PathBuf;
use std::time::Duration;

use super::rebase_tests::failing_cmd;
use super::support::{ExpectedCommand, FakeRunner};

use crate::app::gh_pr;

const CWD: &str = "/gh-pr-ws";
const OWNER_REPO: &str = "spinyfin/mono";

fn cwd() -> PathBuf {
    PathBuf::from(CWD)
}

/// `fetch_pr_json` must go through the REST `pulls` endpoint (`gh api
/// repos/{owner}/{repo}/pulls/{n}`), never the GraphQL-backed `gh pr view` —
/// that's the whole point of this module. `FakeRunner` is a strict FIFO of
/// exact-match invocations, so if the production code regressed to `gh pr
/// view`, this test would panic on "unexpected command invocation" rather
/// than silently pass.
#[test]
fn fetch_pr_json_uses_rest_not_graphql() {
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        cwd(),
        "gh",
        &["api", "repos/spinyfin/mono/pulls/42"],
        r#"{"head":{"ref":"boss/exec_x"},"state":"open","merged":false}"#,
    )]);
    let value = gh_pr::fetch_pr_json(&runner, &cwd(), OWNER_REPO, 42).expect("REST fetch succeeds");
    runner.assert_exhausted();
    assert_eq!(gh_pr::head_ref(&value), Some("boss/exec_x"));
    assert_eq!(gh_pr::state(&value), "OPEN");
}

#[test]
fn state_normalizes_merged_over_closed() {
    let merged: serde_json::Value = serde_json::from_str(r#"{"state":"closed","merged":true}"#).unwrap();
    assert_eq!(gh_pr::state(&merged), "MERGED");

    let closed: serde_json::Value = serde_json::from_str(r#"{"state":"closed","merged":false}"#).unwrap();
    assert_eq!(gh_pr::state(&closed), "CLOSED");

    let open: serde_json::Value = serde_json::from_str(r#"{"state":"open","merged":false}"#).unwrap();
    assert_eq!(gh_pr::state(&open), "OPEN");
}

/// `find_open_pr_for_branch` replaces `gh pr list` with the REST `pulls`
/// list endpoint filtered by `head`/`state` query params.
#[test]
fn find_open_pr_for_branch_uses_rest_list_endpoint() {
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        cwd(),
        "gh",
        &["api", "repos/spinyfin/mono/pulls?head=spinyfin:boss/exec_y&state=open"],
        r#"[{"number":123}]"#,
    )]);
    let number = gh_pr::find_open_pr_for_branch(&runner, &cwd(), OWNER_REPO, "boss/exec_y")
        .expect("REST list succeeds")
        .expect("one open PR found");
    runner.assert_exhausted();
    assert_eq!(number, 123);
}

#[test]
fn find_open_pr_for_branch_returns_none_when_list_is_empty() {
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        cwd(),
        "gh",
        &["api", "repos/spinyfin/mono/pulls?head=spinyfin:boss/exec_z&state=open"],
        "[]",
    )]);
    let number =
        gh_pr::find_open_pr_for_branch(&runner, &cwd(), OWNER_REPO, "boss/exec_z").expect("REST list succeeds");
    runner.assert_exhausted();
    assert_eq!(number, None);
}

/// When the REST `pulls` lookup itself fails with a rate-limit-shaped error
/// (the same failure mode this module exists to move off of GraphQL, in
/// case the REST `core` quota is ever the one that's exhausted instead),
/// the error is enriched with when the quota resets rather than surfacing a
/// bare failure — so a caller (or the human reading it) can tell "retry
/// after T" apart from "PR genuinely unresolvable".
#[test]
fn fetch_pr_json_enriches_rate_limit_errors_with_reset_time() {
    let runner = FakeRunner::new(vec![
        failing_cmd(
            cwd(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            "gh: API rate limit exceeded for user ID 401512. (HTTP 403)",
        ),
        ExpectedCommand::ok(
            cwd(),
            "gh",
            &["api", "rate_limit", "--jq", ".resources.core.reset"],
            "9999999999",
        ),
    ]);
    let err = gh_pr::fetch_pr_json(&runner, &cwd(), OWNER_REPO, 42).expect_err("rate-limited fetch fails");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(msg.contains("rate limit"), "names the original failure: {msg}");
    assert!(msg.contains("resets in"), "surfaces the reset hint: {msg}");
    assert!(msg.contains("9999999999"), "includes the reset timestamp: {msg}");
}

/// If the best-effort reset lookup itself fails, the original error is
/// returned unenriched rather than a second failure masking the first.
#[test]
fn fetch_pr_json_falls_back_to_original_error_when_reset_lookup_fails() {
    let runner = FakeRunner::new(vec![
        failing_cmd(
            cwd(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            "gh: API rate limit exceeded for user ID 401512. (HTTP 403)",
        ),
        failing_cmd(
            cwd(),
            "gh",
            &["api", "rate_limit", "--jq", ".resources.core.reset"],
            "network down",
        ),
    ]);
    let err = gh_pr::fetch_pr_json(&runner, &cwd(), OWNER_REPO, 42).expect_err("still fails");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(msg.contains("rate limit"), "original failure preserved: {msg}");
    assert!(
        !msg.contains("resets in"),
        "no hint when reset lookup itself failed: {msg}"
    );
}

/// A plain, non-rate-limit `gh` failure (PR not found, auth error, etc.) is
/// passed through untouched — the enrichment path never fires for it.
#[test]
fn fetch_pr_json_leaves_non_rate_limit_errors_untouched() {
    let runner = FakeRunner::new(vec![failing_cmd(
        cwd(),
        "gh",
        &["api", "repos/spinyfin/mono/pulls/42"],
        "gh: Could not resolve to a PullRequest with the number 42.",
    )]);
    let err = gh_pr::fetch_pr_json(&runner, &cwd(), OWNER_REPO, 42).expect_err("not found fails");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(msg.contains("Could not resolve"), "{msg}");
    assert!(!msg.contains("resets in"), "{msg}");
}

/// `fetch_pr_states_batch` resolves many PR numbers with one `gh api
/// graphql` round trip (aliased `pullRequest(...)` blocks), never one REST
/// call per number — that's the whole point of this helper.
#[test]
fn fetch_pr_states_batch_issues_one_graphql_call_for_many_prs() {
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        cwd(),
        "gh",
        &[
            "api",
            "graphql",
            "-f",
            "query={ repository(owner: \"spinyfin\", name: \"mono\") { pr0: pullRequest(number: 1579) { state } pr1: pullRequest(number: 2332) { state } } }",
        ],
        r#"{"data":{"repository":{"pr0":{"state":"CLOSED"},"pr1":{"state":"MERGED"}}}}"#,
    )]);
    let states = gh_pr::fetch_pr_states_batch(&runner, &cwd(), OWNER_REPO, &[1579, 2332], Duration::from_secs(30))
        .expect("batched fetch succeeds");
    runner.assert_exhausted();
    assert_eq!(states.get(&1579).map(String::as_str), Some("CLOSED"));
    assert_eq!(states.get(&2332).map(String::as_str), Some("MERGED"));
}

/// An empty `pr_numbers` list resolves to an empty map without spawning a
/// `gh` subprocess at all.
#[test]
fn fetch_pr_states_batch_short_circuits_on_empty_input() {
    let runner = FakeRunner::new(vec![]);
    let states = gh_pr::fetch_pr_states_batch(&runner, &cwd(), OWNER_REPO, &[], Duration::from_secs(30))
        .expect("empty batch succeeds");
    runner.assert_exhausted();
    assert!(states.is_empty());
}

/// A PR number whose GraphQL node comes back null (force-deleted or
/// transferred) is simply absent from the map, not an error — mirroring how
/// `fetch_pr_json`'s REST 404 is handled by its own caller.
#[test]
fn fetch_pr_states_batch_omits_prs_with_null_nodes() {
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        cwd(),
        "gh",
        &[
            "api",
            "graphql",
            "-f",
            "query={ repository(owner: \"spinyfin\", name: \"mono\") { pr0: pullRequest(number: 5) { state } } }",
        ],
        r#"{"data":{"repository":{"pr0":null}}}"#,
    )]);
    let states = gh_pr::fetch_pr_states_batch(&runner, &cwd(), OWNER_REPO, &[5], Duration::from_secs(30))
        .expect("batched fetch succeeds even with a null node");
    runner.assert_exhausted();
    assert!(states.is_empty());
}

/// A top-level GraphQL `errors` array (e.g. the repo alias didn't resolve)
/// fails the whole batch rather than silently returning partial/empty data.
#[test]
fn fetch_pr_states_batch_fails_on_graphql_errors_array() {
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        cwd(),
        "gh",
        &[
            "api",
            "graphql",
            "-f",
            "query={ repository(owner: \"spinyfin\", name: \"mono\") { pr0: pullRequest(number: 5) { state } } }",
        ],
        r#"{"data":null,"errors":[{"message":"Could not resolve to a Repository"}]}"#,
    )]);
    let err = gh_pr::fetch_pr_states_batch(&runner, &cwd(), OWNER_REPO, &[5], Duration::from_secs(30))
        .expect_err("a graphql errors array fails the batch");
    runner.assert_exhausted();
    assert!(err.to_string().contains("Could not resolve to a Repository"), "{err}");
}

/// The batched call is bounded by the `timeout` the caller passes in —
/// exactly the property that was missing before this helper existed, when a
/// per-PR REST call could hang `cube workspace release` indefinitely. A
/// subprocess that outlives the bound is killed and reported as a timeout,
/// not left to run forever.
#[test]
fn fetch_pr_states_batch_is_bounded_by_the_given_timeout() {
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd(),
            "gh",
            &[
                "api",
                "graphql",
                "-f",
                "query={ repository(owner: \"spinyfin\", name: \"mono\") { pr0: pullRequest(number: 5) { state } } }",
            ],
            "",
        )
        .taking(Duration::from_secs(5)),
    ]);
    let timeout = Duration::from_millis(50);
    let err = gh_pr::fetch_pr_states_batch(&runner, &cwd(), OWNER_REPO, &[5], timeout)
        .expect_err("a call that outlives its timeout must fail, not hang");
    runner.assert_exhausted();
    assert_eq!(runner.observed_timeouts(), vec![timeout]);
    assert!(err.to_string().contains("did not complete within"), "{err}");
}
