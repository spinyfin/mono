//! Coverage for the blocked-recovery prompt fragments (generic composer's
//! RESUME EXISTING PR gap, and `compose_revision_directive`'s own
//! goto-vs-stay-put branching). Split out of `compose_prompt_tests.rs` so
//! that file stays under the `file/size` cap.

use super::{ExecutionPromptParams, base_execution, chore_with_pr, compose_execution_prompt, revision_execution};

#[test]
fn blocked_recovery_still_resumes_existing_pr_instead_of_dangling_the_reference() {
    use boss_engine_recovery::recovery_apply::{RecoveryReport, RecoverySource};
    let ws = tempfile::tempdir().unwrap();
    let mut execution = base_execution();
    execution.allow_dirty = true;
    execution.prefer_is_soft = true;
    execution.preferred_workspace_id = Some("prior-workspace".into());
    let work_item = chore_with_pr("https://github.com/org/repo/pull/42");

    // BlockedInPlace: the inherited checkout IS the PR branch already —
    // guidance must say "stay put", never offer `workspace goto` (that
    // would discard the preserved commits the recovery block just told the
    // worker to keep).
    RecoveryReport {
        for_execution_id: execution.id.clone(),
        from_execution_id: "prior".into(),
        source: RecoverySource::BlockedInPlace,
        applied: None,
        patch_error: None,
    }
    .write(ws.path())
    .unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&execution)
            .work_item(&work_item)
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(prompt.contains("BLOCKED WORKSPACE RECOVERY"));
    assert!(
        prompt.contains("## RESUME EXISTING PR"),
        "the acceptance-criterion block references this heading; it must actually be rendered:\n{prompt}",
    );
    assert!(prompt.contains("already on the inherited checkout"));
    assert!(prompt.contains("Do NOT run `jj new main`"));
    assert!(
        !prompt.contains("lands you on the PR branch"),
        "the full reposition-fallback code block must not be offered as something to run:\n{prompt}",
    );

    // BlockedFresh: no inherited checkout — the engine still needs to
    // position the workspace, so the generic goto wording is correct here.
    RecoveryReport {
        for_execution_id: execution.id.clone(),
        from_execution_id: "prior".into(),
        source: RecoverySource::BlockedFresh,
        applied: None,
        patch_error: None,
    }
    .write(ws.path())
    .unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&execution)
            .work_item(&work_item)
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(prompt.contains("## RESUME EXISTING PR"));
    assert!(prompt.contains("workspace goto --pr 42"));
}

#[test]
fn revision_directive_keeps_inherited_checkout_for_blocked_in_place_recovery() {
    // `compose_revision_directive` used to ignore the RecoveryReport
    // entirely: it always claimed the engine ran `cube workspace goto` and
    // offered a `workspace goto --pr` fallback, even when this exact
    // execution's checkout was already verified and re-leased in place
    // (`RecoverySource::BlockedInPlace`) — a real `workspace goto` there
    // would fetch, force-move the bookmark, and `jj new` onto the remote PR
    // head, discarding the preserved commits the recovery handoff block
    // (above, in the generic composer) just told the worker to keep.
    use boss_engine_recovery::recovery_apply::{RecoveryReport, RecoverySource};
    let ws = tempfile::tempdir().unwrap();
    let execution = revision_execution("https://github.com/org/repo/pull/77");
    RecoveryReport {
        for_execution_id: execution.id.clone(),
        from_execution_id: "prior_rev".into(),
        source: RecoverySource::BlockedInPlace,
        applied: None,
        patch_error: None,
    }
    .write(ws.path())
    .unwrap();
    let work_item = super::revision_task_with_created_via(None, "operator");
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&execution)
            .work_item(&work_item)
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(
        prompt.contains("already verified and re-leased this revision's own prior"),
        "revision directive must explain the in-place recovery instead of claiming a goto:\n{prompt}",
    );
    assert!(
        prompt.contains("Stay at `@`"),
        "revision directive must tell the worker to stay put, not reposition:\n{prompt}",
    );
    assert!(
        !prompt.contains("The engine pre-positioned this workspace via"),
        "the generic 'engine pre-positioned via goto' claim is false for BlockedInPlace and must not appear:\n{prompt}",
    );
    assert!(
        !prompt.contains("**Fallback**"),
        "the goto fallback would discard the preserved commits and must not be offered:\n{prompt}",
    );

    // BlockedFresh: the engine still positions the workspace normally, so
    // the generic goto wording must be unchanged.
    RecoveryReport {
        for_execution_id: execution.id.clone(),
        from_execution_id: "prior_rev".into(),
        source: RecoverySource::BlockedFresh,
        applied: None,
        patch_error: None,
    }
    .write(ws.path())
    .unwrap();
    let prompt = compose_execution_prompt(
        ExecutionPromptParams::builder()
            .execution(&execution)
            .work_item(&work_item)
            .workspace_path(ws.path())
            .pr_template_set(&crate::pr_template::PrTemplateSet::default())
            .build(),
    );
    assert!(prompt.contains("The engine pre-positioned this workspace via"));
    assert!(prompt.contains("workspace goto --pr 77"));
}
