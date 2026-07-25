//! Types for `boss pr status` and `boss pr body` — bounded, read-only
//! worker verbs over PR state Boss already has stored (the merge poller's
//! last probe, and the execution-start PR snapshot), so a worker does not
//! need to shell out to `gh pr view` / `gh pr edit --body` just to *read*
//! what Boss already knows.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Read-only model access and the exposure boundary" (same attribution
//! model and exposure boundary as `boss context` / `boss propose`).

use serde::{Deserialize, Serialize};

/// The sanitized reply to `boss pr status`: the caller's own PR's
/// mergeability as Boss last observed it, sourced from the merge poller's
/// stored state (or, with `--refresh`, a single fresh on-demand probe) —
/// never live GitHub truth by default.
///
/// Every field beyond `pr_url` is `None` when the caller's task has no PR
/// yet, or has one the merge poller has not yet probed (e.g. a PR bound in
/// the last few seconds, before the next sweep).
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct PrStatusView {
    pub pr_url: Option<String>,
    /// Boss's normalized mergeability: `"mergeable"`, `"conflicting"`, or
    /// `"unknown"` — derived from GitHub's `mergeable` field. `None` before
    /// the first successful probe.
    pub mergeable: Option<String>,
    /// GitHub's raw `mergeStateStatus` (`"CLEAN"`, `"DIRTY"`, `"BLOCKED"`,
    /// `"BEHIND"`, `"UNSTABLE"`, `"UNKNOWN"`) — a distinct signal from
    /// `mergeable`: e.g. `mergeable = "mergeable"` with
    /// `merge_state_status = "BLOCKED"` means the head merges cleanly but a
    /// required check/review is still outstanding. `None` before the first
    /// successful probe.
    pub merge_state_status: Option<String>,
    /// The PR's head SHA as of the last observation. `None` before the
    /// first successful probe.
    pub head_sha: Option<String>,
    /// Unix epoch seconds (decimal string) of the observation this
    /// response reflects. Compare against your own push time to tell
    /// whether this snapshot predates a push you just made — Boss does not
    /// re-probe on every call, so a `boss pr status` right after `cube pr
    /// update` will usually show a stale `observed_at` unless `--refresh`
    /// was passed. `None` when the PR has never been probed.
    pub observed_at: Option<String>,
    /// `true` when this response was produced by a live, on-demand
    /// `--refresh` probe rather than the merge poller's last stored sweep.
    /// `false` for a plain `boss pr status` call, and also `false` for a
    /// `--refresh` call that was denied its own probe by the bounded
    /// refresh budget (see `refresh_throttled`) — in that case the stored
    /// snapshot is returned instead.
    #[serde(default)]
    #[builder(default)]
    pub refreshed: bool,
    /// `true` when `--refresh` was requested but the bounded, engine-wide
    /// refresh budget was exhausted, so the stored snapshot was returned
    /// instead of a fresh probe. Always `false` when `refresh` was not
    /// requested. Lets a worker distinguish "Boss chose not to re-probe
    /// yet" from "there is nothing newer to see."
    #[serde(default)]
    #[builder(default)]
    pub refresh_throttled: bool,
}

/// The sanitized reply to `boss pr body`: the caller's own PR's body as
/// Boss last snapshotted it at the start of this execution's run — never a
/// live GitHub read. Read-only; `cube pr update` (with `--body-file`)
/// remains the only write path for the description.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct PrBodyView {
    pub pr_url: Option<String>,
    /// The PR title Boss snapshotted when this execution's run started,
    /// re-read verbatim (not edited since). Same `None` cases as `body`:
    /// no baseline snapshot (new-PR flow) or a failed fetch at run start.
    pub title: Option<String>,
    /// The PR body Boss snapshotted when this execution's run started,
    /// re-read verbatim (not edited since). `None` when: the execution
    /// began a brand-new PR flow (nothing to snapshot yet — you are about
    /// to write the very first body via `cube pr create --body-file`), or
    /// the snapshot fetch failed at run start. A `None` body does NOT mean
    /// the PR has an empty description — an intentionally empty
    /// description is stored as `Some("")`, not `None`. See `boss pr body
    /// --help` for what to do in the `None` case.
    pub body: Option<String>,
}
