//! Expected packets for the per-shape snapshot tests: ids and timestamps are
//! normalized, and the static verb catalogue is pinned separately.

const S16: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Review churned
- kind: chore
- status: in_review
- product: ProgressionBrief
- project: (none)
- stuck: S16_review_churn
- stuck since: <TS>
- blocked_reason: (none)
- dispatch_failed_reason: (none)
- pr_url: https://github.com/spinyfin/mono/pull/16

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → in_review by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

- url: https://github.com/spinyfin/mono/pull/16
- mergeable: (unobserved)
- merge_state_status: (unobserved)
- head_sha: (unobserved)
- observed_at: (unobserved)
- ci_required_state: (unobserved)
- review_required_state: (unobserved)

### Conflict resolutions

(none)

### CI remediations

(none)

### Failing-check history (other open PRs in this product)

Stored cross-PR observations follow. Engine probes below supply recent main-branch outcomes before spawn; missing observations remain unknown.

(no failing required checks recorded on this item)

## Open attentions

- attn_ID on=task_ID kind=churn_guard_parked title=Parked by churn guard — 4 recent failures
  The `pr_review_recovery` sweep stopped auto-redispatching this work item: it produced 4 terminal pr_review executions within the trailing 1h window (threshold 3), which usually means something structural is broken (a bad host, a repo/config issue) rather than a one-off blip.  Failing executions: (could not resolve the failing execution ids)  This clears automatically once the window drains below the threshold and the next sweep pass redispatches successfully. To bypass the guard and retry immediately, run `bossctl work start task_ID`.

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S14: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Recovery exhausted
- kind: chore
- status: active
- product: ProgressionBrief
- project: (none)
- stuck: S14_worker_recovery
- stuck since: <TS>
- blocked_reason: (none)
- dispatch_failed_reason: (none)
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → active by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

- attn_ID on=task_ID kind=worker_recovery_exhausted title=Recovery exhausted
  three API-error resumes failed

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S3: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Parked
- kind: chore
- status: todo
- product: ProgressionBrief
- project: (none)
- stuck: S3_deliberate_park
- stuck since: <TS>
- blocked_reason: (none)
- blocked_detail: Should the API return 404 or 410 for retired ids? I need the owner to pick.

Should the API return 404 or 410 for retired ids? I need the owner to pick.
- dispatch_failed_reason: deliberate_park
- dispatch_failed_error: The `orphan_sweep` sweep is holding this work item in Backlog because its worker's most recent execution ended in a deliberate park — either it declared itself blocked (`boss propose done --outcome blocked`) or the auto-nudge breaker gave up waiting for a response — and the attention item raised at the time is still open. This is not a failure: the worker did its job and is waiting on a decision. Review the open attention item, then run `bossctl work start task_ID` (or drag the card to Doing) to resume it.
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status active → todo by engine at <TS> (deliberate_park)
- dispatch_failed_reason ∅ → deliberate_park by engine at <TS> (deliberate_park)
- status blocked → active by engine at <TS> (unattributed_write)
- status active → blocked by engine at <TS> (declared_blocked)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 1

- exec_ID item=task_ID kind=chore_implementation status=failed via=(unset) run_done=blocked error=Should the API return 404 or 410 for retired ids? I need the owner to pick.

Should the API return 404 or 410 for retired ids? I need the owner to pick.

## Declared blocker (2.9)

Producer: declared_blocked

Engine sweep note (not the worker's words): The `orphan_sweep` sweep is holding this work item in Backlog because its worker's most recent execution ended in a deliberate park — either it declared itself blocked (`boss propose done --outcome blocked`) or the auto-nudge breaker gave up waiting for a response — and the attention item raised at the time is still open. This is not a failure: the worker did its job and is waiting on a decision. Review the open attention item, then run `bossctl work start task_ID` (or drag the card to Doing) to resume it.

Verbatim:

```
Should the API return 404 or 410 for retired ids? I need the owner to pick.
```

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S1: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Lease failed
- kind: chore
- status: todo
- product: ProgressionBrief
- project: (none)
- stuck: S1_dispatch_failure
- stuck since: <TS>
- blocked_reason: (none)
- dispatch_failed_reason: cube_workspace_lease_failed
- dispatch_failed_error: cube refused the lease
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status active → todo by engine at <TS> (cube_workspace_lease_failed)
- dispatch_failed_reason ∅ → cube_workspace_lease_failed by engine at <TS> (cube_workspace_lease_failed)
- status todo → active by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S22: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Orphan branch
- kind: chore
- status: active
- product: ProgressionBrief
- project: (none)
- stuck: S22_abandoned_branch
- stuck since: <TS>
- blocked_reason: (none)
- dispatch_failed_reason: (none)
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → active by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

- attn_ID on=task_ID kind=abandoned_branch_no_pr title=Branch has no PR
  pushed boss/orphan-branch more than 24h ago

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S11: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Unbound PR
- kind: chore
- status: blocked
- product: ProgressionBrief
- project: (none)
- stuck: S11_pr_binding
- stuck since: <TS>
- blocked_reason: pr_binding_unresolved
- dispatch_failed_reason: (none)
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → blocked by human at <TS> (unattributed_write)
- blocked_reason ∅ → pr_binding_unresolved by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S2: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Churned
- kind: chore
- status: todo
- product: ProgressionBrief
- project: (none)
- stuck: S2_churn_guard
- stuck since: <TS>
- blocked_reason: (none)
- dispatch_failed_reason: churn_guard
- dispatch_failed_error: three unproductive runs
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status active → todo by engine at <TS> (churn_guard)
- dispatch_failed_reason ∅ → churn_guard by engine at <TS> (churn_guard)
- status todo → active by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S13: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Dead revision
- kind: revision
- status: blocked
- product: ProgressionBrief
- project: (none)
- stuck: S13_revision_chain
- stuck since: <TS>
- blocked_reason: worker_failed
- dispatch_failed_reason: (none)
- failure_basis: unattributed_write
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → blocked by human at <TS> (unattributed_write)
- blocked_reason ∅ → worker_failed by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- task_ID (chore, blocked, blocks)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

Parent PR (owned by chain root task_ID); this item is a revision.

- url: https://github.com/spinyfin/mono/pull/90
- mergeable: MERGEABLE
- merge_state_status: (unobserved)
- head_sha: feed90
- observed_at: <TS>
- ci_required_state: success
- review_required_state: (unobserved)

### Conflict resolutions

(none)

### CI remediations

- cir_ID status=pending kind=fix failure=(none) checks=[{"name":"parent-check","conclusion":"FAILURE"}]
  log excerpt:
```
parent remediation evidence
```

### Failing-check history (other open PRs in this product)

Stored cross-PR observations follow. Engine probes below supply recent main-branch outcomes before spawn; missing observations remain unknown.

(no failing required checks recorded on this item)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S5: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Conflicted
- kind: chore
- status: blocked
- product: ProgressionBrief
- project: (none)
- stuck: S5_merge_conflict
- stuck since: <TS>
- blocked_reason: merge_conflict
- dispatch_failed_reason: (none)
- pr_url: https://github.com/spinyfin/mono/pull/41

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → blocked by human at <TS> (unattributed_write)
- blocked_reason ∅ → merge_conflict by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

- url: https://github.com/spinyfin/mono/pull/41
- mergeable: MERGEABLE
- merge_state_status: (unobserved)
- head_sha: ccc333
- observed_at: <TS>
- ci_required_state: fail
- review_required_state: (unobserved)

### Conflict resolutions

- crz_ID status=pending failure=(none) class=registry
  diagnosis:
  STALE: stored for head bbb222; the PR head is now ccc333
  - src/lib.rs (content)

### CI remediations

(none)

### Failing-check history (other open PRs in this product)

Stored cross-PR observations follow. Engine probes below supply recent main-branch outcomes before spawn; missing observations remain unknown.

(no failing required checks recorded on this item)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S7_DECLARED: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Declared
- kind: chore
- status: blocked
- product: ProgressionBrief
- project: (none)
- stuck: S7_worker_failed
- stuck since: <TS>
- blocked_reason: worker_failed
- blocked_detail: need a decision on the retry budget

need a decision on the retry budget
- dispatch_failed_reason: (none)
- failure_basis: declared_blocked
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status active → blocked by engine at <TS> (declared_blocked)
- blocked_reason ∅ → worker_failed by engine at <TS> (declared_blocked)
- status todo → active by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 1

- exec_ID item=task_ID kind=chore_implementation status=failed via=(unset) run_done=blocked error=need a decision on the retry budget

need a decision on the retry budget

## Declared blocker (2.9)

Producer: declared_blocked

Engine sweep note (not the worker's words): need a decision on the retry budget

need a decision on the retry budget

Verbatim:

```
need a decision on the retry budget
```

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S7_NUDGE: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Nudged
- kind: chore
- status: blocked
- product: ProgressionBrief
- project: (none)
- stuck: S7_worker_failed
- stuck since: <TS>
- blocked_reason: worker_failed
- blocked_detail: nudge loop bounded after silence
- dispatch_failed_reason: (none)
- failure_basis: nudge_breaker
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status active → blocked by engine at <TS> (nudge_breaker)
- blocked_reason ∅ → worker_failed by engine at <TS> (nudge_breaker)
- status todo → active by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 1

- exec_ID item=task_ID kind=chore_implementation status=failed via=(unset) run_done=(none) error=nudge loop bounded after silence

## Declared blocker (2.9)

Producer: nudge_breaker

Engine sweep note (not the worker's words): nudge loop bounded after silence

Verbatim:

```
nudge loop bounded after silence
```

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

- attn_ID on=task_ID kind=nudge_breaker_tripped title=Nudge breaker
  nudge loop bounded after silence

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S6: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Exhausted CI
- kind: chore
- status: blocked
- product: ProgressionBrief
- project: (none)
- stuck: S6_ci_failure
- stuck since: <TS>
- blocked_reason: ci_failure_exhausted
- dispatch_failed_reason: (none)
- pr_url: https://github.com/spinyfin/mono/pull/50

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → blocked by human at <TS> (unattributed_write)
- blocked_reason ∅ → ci_failure_exhausted by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

- url: https://github.com/spinyfin/mono/pull/50
- mergeable: MERGEABLE
- merge_state_status: (unobserved)
- head_sha: deadbeef
- observed_at: <TS>
- ci_required_state: fail
- review_required_state: (unobserved)

### Conflict resolutions

(none)

### CI remediations

- cir_ID status=pending kind=fix failure=(none) checks=[{"name":"bazel-test","conclusion":"FAILURE"},{"name":"checkleft","conclusion":"TIMED_OUT"}]
  log excerpt:
```
FAIL: tools/boss/engine/core/src/lib.rs unused import
```

### Failing-check history (other open PRs in this product)

Stored cross-PR observations follow. Engine probes below supply recent main-branch outcomes before spawn; missing observations remain unknown.

- `bazel-test` failed on this PR and on 1 other open/recent PRs (Sibling PR) → likely unrelated or flaky.
- `checkleft` failed on this PR and on 1 other open/recent PRs (Sibling PR) → likely unrelated or flaky.

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

const S9: &str = r#"# Progression diagnosis

You are a restricted unstick agent. You hold no code checkout. Diagnose this stuck work item from the packet below and act with `boss propose` verbs. Stored attempts are supplemented by engine probes for current conflict, CI-log and main-branch evidence before spawn. You may run `gh pr view` and `gh pr checks` if you need a live check.

## Item

- id: task_ID
- name: Bookmark failed
- kind: chore
- status: blocked
- product: ProgressionBrief
- project: (none)
- stuck: S9_execution_recovery
- stuck since: <TS>
- blocked_reason: execution_recovery_failed
- dispatch_failed_reason: (none)
- pr_url: (none)

## Ledger (last 3 status changes and the row that entered the stuck state)

- status todo → blocked by human at <TS> (unattributed_write)
- blocked_reason ∅ → execution_recovery_failed by human at <TS> (unattributed_write)
- status ∅ → todo by engine at <TS> (insert_chore_with_project_in_tx_as_actor)

## Dependencies

Prerequisites:
- (none)
Dependents:
- (none)

## Executions (item and revision chain)

Churn-window terminal count (1h, excluding progression): 0

(none)

## Declared blocker (2.9)

(none)

## PR

(no PR bound)

### Conflict resolutions

(none)

### CI remediations

(none)

## Open attentions

(none)

## Operator questions

(none)

## Progression history

Current fingerprint: `<FINGERPRINT>`

Prior progression runs: (none)
Prior fingerprint actions: none recorded.

"#;

pub fn lookup(name: &str) -> &'static str {
    match name {
        "s1" => S1,
        "s11" => S11,
        "s13" => S13,
        "s14" => S14,
        "s16" => S16,
        "s2" => S2,
        "s22" => S22,
        "s3" => S3,
        "s5" => S5,
        "s6" => S6,
        "s7_declared" => S7_DECLARED,
        "s7_nudge" => S7_NUDGE,
        "s9" => S9,
        other => panic!("no golden packet named {other}"),
    }
}
