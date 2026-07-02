<!-- Ground-truth fixture for planner e2e tests (design task 11).
     Verbatim excerpt of the "Implementation phases" section of
     tools/boss/docs/designs/unify-pr-remediation-on-revisions.md (project P707).
     Do not edit by hand except to re-sync with the source doc. -->

## Implementation phases

1. **Provenance + reverse link (additive, no behaviour change).** Extend `canonicalize_created_via` for `merge-conflict:*` / `ci-fix:*`; add `revision_task_id` columns + migrations; render engine-triggered revision chrome in the kanban projection. _Acceptance:_ a revision created with `created_via='merge-conflict:<id>'` round-trips and renders with the right badge; bespoke flows untouched.
2. **Injected directive fragments.** Refactor the bodies of the two `compose_*_prompt` into directive fragments that `compose_revision_directive` appends based on `created_via` + linked attempt. _Acceptance:_ a `revision_implementation` worker created with a conflict provenance receives the same diagnosis/steps text it gets today.
3. **Conflict producer cutover.** `on_conflict_detected`: on a new attempt row, create an engine-triggered revision (parent = the chore) and stamp `revision_task_id`, instead of creating a `conflict_resolution` execution. Keep flipping `blocked: merge_conflict`. Wire retire-on-clean to clear the parent block + mark ledger succeeded (revision rides its own lifecycle). Old `conflict_resolution` dispatch dormant. _Acceptance:_ end-to-end on a test PR — conflict → revision spawns into the warm workspace → pushes the rebased branch → no new PR → poller sees clean → parent back to `in_review`, ledger `succeeded`, revision `in_review`; parent merge flips revision `done`. Churn guard still caps at 3/3600s.
4. **CI producer cutover.** Same for `on_ci_failure_detected` (`fix` kind only; retrigger unchanged). Budget enforced before create; exhaustion → `ci_failure_exhausted`, no revision. Rebase-only refund preserved. _Acceptance:_ CI fail → revision → green; budget exhaustion still blocks; retrigger still works without a revision.
5. **Remove the dormant bespoke paths** once 3+4 prove out for a release: delete the two execution kinds, their dispatch arms, and the standalone composers.
6. _(Stretch, separate effort)_ fold auto-rebase in as a fourth producer.
