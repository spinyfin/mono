use super::*;

fn started() -> AgyProgressSession {
    let mut session = stateless();
    session.normalize_progress_events(&session_start(PARENT)).unwrap();
    session
}

fn pending_call_ids(session: &AgyProgressSession) -> Vec<String> {
    session
        .resume_state()
        .and_then(|state| {
            Some(
                state
                    .get("pending_tools")?
                    .get("calls")?
                    .as_object()?
                    .keys()
                    .map(|key| key.rsplit('/').next().unwrap_or(key).to_owned())
                    .collect(),
            )
        })
        .unwrap_or_default()
}

fn assert_no_notification_and_pending(session: &mut AgyProgressSession, raw: &serde_json::Value) {
    let events = session.normalize_progress_events(raw).unwrap();
    if raw["hookEventName"] == "PreToolUse" {
        assert!(matches!(events.as_slice(), [WorkerEvent::PreToolUse { .. }]));
    } else {
        assert!(
            events.is_empty(),
            "a non-idle boundary must not emit a refused-tool notification"
        );
    }
    assert_eq!(pending_call_ids(session), vec!["call_1".to_string()]);
}

#[test]
fn missing_id_cannot_be_cleared_by_an_unrelated_post() {
    let mut session = started();
    let mut pre = pre_tool_use(PARENT, "uncorrelatable");
    pre["toolCall"].as_object_mut().unwrap().remove("id");
    session.normalize_progress_events(&pre).unwrap();
    let mut post = post_tool_use(PARENT);
    post["toolCall"]["id"] = json!("missing-tool-call-id");
    session.normalize_progress_events(&post).unwrap();
    assert!(
        matches!(session.normalize_progress_events(&stop(PARENT, true)).unwrap().as_slice(),
        [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }] if message.contains("missing toolCall.id"))
    );
}

#[test]
fn refused_tool_notification_precedes_only_the_parent_idle_stop() {
    let mut session = started();
    session
        .normalize_progress_events(&pre_tool_use(PARENT, "denied"))
        .unwrap();
    session.normalize_progress_events(&post_tool_use(CHILD)).unwrap();
    assert!(
        session
            .normalize_progress_events(&stop(CHILD, true))
            .unwrap()
            .is_empty()
    );
    assert!(
        session
            .normalize_progress_events(&stop(PARENT, false))
            .unwrap()
            .is_empty()
    );
    assert!(
        session
            .normalize_progress_events(&post_invocation(PARENT))
            .unwrap()
            .is_empty()
    );
    let events = session.normalize_progress_events(&stop(PARENT, true)).unwrap();
    assert!(
        matches!(events.as_slice(), [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
        if message.starts_with(crate::codex::UNOBSERVED_COMMAND_MARKER)
            && message.contains(crate::agy::REFUSED_TOOL_PREFIX)
            && message.contains("call_1")
            && !message.contains("lastUserInput"))
    );
    assert_eq!(session.normalize_progress_events(&stop(PARENT, true)).unwrap().len(), 1);
}

#[test]
fn rewritten_and_background_calls_match_by_id_even_when_the_command_fails() {
    let mut session = started();
    session
        .normalize_progress_events(&pre_tool_use(PARENT, "before rewrite"))
        .unwrap();
    assert!(
        session
            .normalize_progress_events(&stop(PARENT, false))
            .unwrap()
            .is_empty()
    );
    let mut post = post_tool_use(PARENT);
    post["error"] = json!("validation error");
    session.normalize_progress_events(&post).unwrap();
    assert!(matches!(
        session
            .normalize_progress_events(&stop(PARENT, true))
            .unwrap()
            .as_slice(),
        [WorkerEvent::Stop { .. }]
    ));
}

#[test]
fn denied_child_call_is_flagged_and_child_post_cannot_clear_parent_call() {
    let mut session = started();
    session.normalize_progress_events(&session_start(CHILD)).unwrap();
    session
        .normalize_progress_events(&pre_tool_use(CHILD, "child denied"))
        .unwrap();
    let events = session.normalize_progress_events(&stop(PARENT, true)).unwrap();
    assert!(
        matches!(events.as_slice(), [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
        if message.contains(&format!("conversation={CHILD}")))
    );

    let mut session = started();
    session.normalize_progress_events(&session_start(CHILD)).unwrap();
    session
        .normalize_progress_events(&pre_tool_use(PARENT, "parent denied"))
        .unwrap();
    session.normalize_progress_events(&post_tool_use(CHILD)).unwrap();
    let events = session.normalize_progress_events(&stop(PARENT, true)).unwrap();
    assert!(
        matches!(events.as_slice(), [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
        if message.contains(&format!("conversation={PARENT}")))
    );
}

#[test]
fn post_invocation_does_not_emit_a_refused_tool_or_clear_the_pending_call() {
    let mut session = started();
    assert_no_notification_and_pending(&mut session, &pre_tool_use(PARENT, "denied"));
    assert_eq!(pending_call_ids(&session), vec!["call_1".to_string()]);
    assert_no_notification_and_pending(&mut session, &post_invocation(PARENT));
    let events = session.normalize_progress_events(&stop(PARENT, true)).unwrap();
    assert!(matches!(
        events.as_slice(),
        [WorkerEvent::Notification { .. }, WorkerEvent::Stop { .. }]
    ));
}

#[test]
fn non_idle_stop_does_not_emit_a_refused_tool_or_clear_the_pending_call() {
    let mut session = started();
    assert_no_notification_and_pending(&mut session, &pre_tool_use(PARENT, "denied"));
    assert_no_notification_and_pending(&mut session, &stop(PARENT, false));
    assert!(matches!(
        session
            .normalize_progress_events(&stop(PARENT, true))
            .unwrap()
            .as_slice(),
        [WorkerEvent::Notification { .. }, WorkerEvent::Stop { .. }]
    ));
}

#[test]
fn headless_denial_without_stop_retains_the_pending_call() {
    // Headless auto-deny emits PreToolUse and no later Stop (permission study).
    let mut session = started();
    assert_no_notification_and_pending(&mut session, &pre_tool_use(PARENT, "denied"));
    assert_eq!(pending_call_ids(&session), vec!["call_1".to_string()]);
    assert!(
        session
            .normalize_progress_events(&post_invocation(PARENT))
            .unwrap()
            .is_empty()
    );
    assert_eq!(pending_call_ids(&session), vec!["call_1".to_string()]);
}

#[test]
fn approval_dialog_without_stop_retains_the_pending_call() {
    // Interactive approval waits with no further hook and no Stop (permission study).
    let mut session = started();
    assert_no_notification_and_pending(&mut session, &pre_tool_use(PARENT, "denied"));
    assert_eq!(pending_call_ids(&session), vec!["call_1".to_string()]);
}

#[test]
fn completed_but_failed_matching_post_with_empty_error_is_not_a_refusal() {
    let mut session = started();
    session
        .normalize_progress_events(&pre_tool_use(PARENT, "before rewrite"))
        .unwrap();
    let mut post = post_tool_use(PARENT);
    post["error"] = json!("");
    session.normalize_progress_events(&post).unwrap();
    assert!(pending_call_ids(&session).is_empty());
    assert!(matches!(
        session
            .normalize_progress_events(&stop(PARENT, true))
            .unwrap()
            .as_slice(),
        [WorkerEvent::Stop { .. }]
    ));
}

#[test]
fn validation_error_matching_post_is_not_a_refusal() {
    let mut session = started();
    session
        .normalize_progress_events(&pre_tool_use(PARENT, "before rewrite"))
        .unwrap();
    let mut post = post_tool_use(PARENT);
    post["error"] = json!("validation error");
    session.normalize_progress_events(&post).unwrap();
    assert!(pending_call_ids(&session).is_empty());
    assert!(matches!(
        session
            .normalize_progress_events(&stop(PARENT, true))
            .unwrap()
            .as_slice(),
        [WorkerEvent::Stop { .. }]
    ));
}

#[test]
fn singular_normalizer_returns_the_stop_when_pending_calls_emit_a_notification() {
    let mut session = started();
    session
        .normalize_progress_events(&pre_tool_use(PARENT, "denied"))
        .unwrap();
    assert!(matches!(
        session.normalize_progress_event(&stop(PARENT, true)).unwrap(),
        WorkerEvent::Stop { .. }
    ));
}

#[test]
fn pending_calls_survive_connection_snapshot_and_only_matching_ids_complete() {
    let mut session = started();
    session
        .normalize_progress_events(&pre_tool_use(PARENT, "denied"))
        .unwrap();
    let mut restored = stateless();
    restored.restore_resume_state(&session.resume_state().unwrap()).unwrap();
    let mut unrelated = post_tool_use(PARENT);
    unrelated["toolCall"]["id"] = json!("other-call");
    restored.normalize_progress_events(&unrelated).unwrap();
    let events = restored.normalize_progress_events(&stop(PARENT, true)).unwrap();
    assert_eq!(events.len(), 2);
    assert!(crate::agy::AgyDriver::default().turn_boundary(&events[0]).is_none());
    assert!(crate::agy::AgyDriver::default().turn_boundary(&events[1]).is_some());
}
