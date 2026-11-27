use boss_acp_client::schema::RequestId;
use boss_protocol::{CoordinatorChatMessageRole, CoordinatorChatTurnOrigin};

use crate::queue::{PromptContent, PromptImageRef, PromptOrigin, QueuedPrompt, TurnQueue};

fn prompt(intent_id: u64, origin: PromptOrigin, text: &str) -> QueuedPrompt {
    QueuedPrompt {
        intent_id,
        origin,
        content: PromptContent {
            text: text.to_owned(),
            images: Vec::new(),
        },
        client_message_id: (origin == PromptOrigin::Operator).then(|| format!("cm-{intent_id}")),
        queued_ts_ms: 1_795_000_000_000 + intent_id,
    }
}

#[test]
fn origins_map_to_expected_message_roles_and_turn_origins() {
    assert_eq!(
        PromptOrigin::Operator.message_role(),
        CoordinatorChatMessageRole::Operator
    );
    assert_eq!(PromptOrigin::Engine.message_role(), CoordinatorChatMessageRole::Engine);
    assert_eq!(PromptOrigin::Operator.turn_origin(), CoordinatorChatTurnOrigin::Human);
    assert_eq!(PromptOrigin::Engine.turn_origin(), CoordinatorChatTurnOrigin::Engine);
}

#[test]
fn operator_prompts_dispatch_single_flight_in_fifo_order() {
    let mut queue = TurnQueue::new();
    assert!(!queue.can_send());
    assert!(queue.peek_next_dispatchable().is_none());

    queue.enqueue(prompt(1, PromptOrigin::Operator, "first"));
    queue.enqueue(prompt(2, PromptOrigin::Operator, "second"));
    queue.enqueue(prompt(3, PromptOrigin::Operator, "third"));
    assert_eq!(queue.queued_len(), 3);
    assert!(queue.can_send());

    // Begin sending first; queue stays strictly single-flight even though the
    // adapter advertises `promptQueueing: true`.
    let first = queue
        .begin_send(RequestId::Number(10), 100)
        .expect("first prompt dispatches");
    assert_eq!(first.prompt.intent_id, 1);
    assert!(queue.is_busy());
    assert!(!queue.can_send());
    assert!(queue.begin_send(RequestId::Number(11), 101).is_none());
    assert!(queue.peek_next_dispatchable().is_none());

    // Wrong request id does not complete the in-flight prompt.
    assert!(queue.on_wire_prompt_finished(&RequestId::Number(99)).is_none());
    assert!(queue.is_busy());

    let finished = queue
        .on_wire_prompt_finished(&RequestId::Number(10))
        .expect("matching response clears in-flight");
    assert_eq!(finished.prompt.intent_id, 1);
    assert!(queue.can_send());

    let second = queue
        .begin_send(RequestId::Number(11), 200)
        .expect("second dispatches next");
    assert_eq!(second.prompt.intent_id, 2);
    queue.on_wire_prompt_finished(&RequestId::Number(11));

    let third = queue
        .begin_send(RequestId::Number(12), 300)
        .expect("third dispatches last");
    assert_eq!(third.prompt.intent_id, 3);
    queue.on_wire_prompt_finished(&RequestId::Number(12));

    assert_eq!(queue.queued_len(), 0);
    assert!(!queue.is_busy());
}

#[test]
fn engine_prompts_go_ahead_of_queued_operator_prompts_without_preempting_in_flight() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "in-flight operator"));
    queue.begin_send(RequestId::Number(1), 100).unwrap();

    // Queue operator messages, then engine prompts, then another operator message.
    queue.enqueue(prompt(2, PromptOrigin::Operator, "op-a"));
    queue.enqueue(prompt(3, PromptOrigin::Operator, "op-b"));
    queue.enqueue(prompt(4, PromptOrigin::Engine, "eng-1 (reset)"));
    queue.enqueue(prompt(5, PromptOrigin::Operator, "op-c"));
    queue.enqueue(prompt(6, PromptOrigin::Engine, "eng-2 (auth)"));

    // In-flight operator turn was NOT pre-empted.
    assert_eq!(queue.in_flight().unwrap().prompt.intent_id, 1);
    assert!(!queue.can_send());

    // Queue order puts all engine prompts FIFO ahead of all operator prompts FIFO.
    let ids: Vec<u64> = queue.queued().iter().map(|entry| entry.intent_id).collect();
    assert_eq!(ids, vec![4, 6, 2, 3, 5]);

    queue.on_wire_prompt_finished(&RequestId::Number(1));
    let mut dispatched_ids = Vec::new();
    for req_num in 2..=6 {
        let sent = queue
            .begin_send(RequestId::Number(req_num), req_num as u64 * 100)
            .expect("queued prompt dispatches");
        dispatched_ids.push(sent.prompt.intent_id);
        queue.on_wire_prompt_finished(&RequestId::Number(req_num));
    }
    assert_eq!(dispatched_ids, vec![4, 6, 2, 3, 5]);
}

#[test]
fn agent_started_turns_block_prompt_dispatch_until_closed() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "waiting for agent turn"));
    assert!(queue.can_send());

    // Background agent follow-up opens an autonomous turn (`taskType: "turn"`).
    queue.on_agent_turn_started("autonomous-turn-1");
    assert!(queue.has_active_turn());
    assert!(queue.is_busy());
    assert!(!queue.can_send());
    assert!(queue.begin_send(RequestId::Number(1), 100).is_none());

    // Duplicate start id is idempotent; nested/second turn keeps queue blocked
    // until all open turns finish.
    queue.on_agent_turn_started("autonomous-turn-1");
    queue.on_agent_turn_started("autonomous-turn-2");
    assert_eq!(queue.open_agent_turns().len(), 2);

    assert!(queue.on_agent_turn_finished("autonomous-turn-1"));
    assert!(!queue.can_send(), "second agent turn is still open");
    assert!(!queue.on_agent_turn_finished("unknown-turn"));

    assert!(queue.on_agent_turn_finished("autonomous-turn-2"));
    assert!(!queue.has_active_turn());
    assert!(queue.can_send());
    let sent = queue.begin_send(RequestId::Number(1), 200).unwrap();
    assert_eq!(sent.prompt.intent_id, 1);
}

#[test]
fn withdraw_removes_only_queued_prompts_and_never_touches_in_flight() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "in-flight"));
    queue.begin_send(RequestId::Number(1), 100).unwrap();

    queue.enqueue(prompt(2, PromptOrigin::Engine, "reset-timeout"));
    queue.enqueue(prompt(3, PromptOrigin::Operator, "draft-a"));
    queue.enqueue(prompt(4, PromptOrigin::Operator, "draft-b"));

    // Cannot withdraw the in-flight prompt.
    assert!(queue.withdraw(1).is_none());
    assert!(queue.withdraw_by_client_message_id("cm-1").is_none());
    assert_eq!(queue.in_flight().unwrap().prompt.intent_id, 1);

    // Withdraw timed-out engine prompt by intent_id.
    let withdrawn_engine = queue.withdraw(2).expect("queued engine prompt withdraws");
    assert_eq!(withdrawn_engine.origin, PromptOrigin::Engine);
    assert!(queue.withdraw(2).is_none());

    // Withdraw operator prompt by client_message_id.
    let withdrawn_op = queue
        .withdraw_by_client_message_id("cm-3")
        .expect("queued operator prompt withdraws by client id");
    assert_eq!(withdrawn_op.intent_id, 3);

    let remaining: Vec<u64> = queue.queued().iter().map(|entry| entry.intent_id).collect();
    assert_eq!(remaining, vec![4]);
}

#[test]
fn stop_cancels_active_turn_returns_operator_prompts_and_preserves_engine_prompts() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "active"));
    queue.begin_send(RequestId::Number(5), 100).unwrap();

    let mut op_with_image = prompt(2, PromptOrigin::Operator, "queued op 1");
    op_with_image.content.images.push(PromptImageRef {
        path: "images/shot-1.png".to_owned(),
        mime_type: "image/png".to_owned(),
        filename: Some("shot.png".to_owned()),
        byte_len: Some(1740),
    });
    queue.enqueue(op_with_image.clone());
    queue.enqueue(prompt(3, PromptOrigin::Engine, "queued engine 1"));
    queue.enqueue(prompt(4, PromptOrigin::Operator, "queued op 2"));
    queue.enqueue(prompt(5, PromptOrigin::Engine, "queued engine 2"));

    // `plan_stop` predicts the exact same outcome without mutating.
    let planned = queue.plan_stop();
    assert!(planned.send_cancel);
    assert!(planned.active_turn);
    assert_eq!(
        planned
            .returned_operator_prompts
            .iter()
            .map(|p| p.intent_id)
            .collect::<Vec<_>>(),
        vec![2, 4]
    );
    assert_eq!(queue.queued_len(), 4);

    let stop = queue.request_stop();
    assert_eq!(stop, planned);
    assert_eq!(stop.returned_operator_prompts[0], op_with_image);
    assert!(queue.cancel_requested());

    // Active turn is still in flight until its wire response arrives, so engine
    // prompts wait rather than racing the cancelled turn.
    assert!(queue.is_busy());
    assert!(!queue.can_send());
    assert_eq!(
        queue.queued().iter().map(|p| p.intent_id).collect::<Vec<_>>(),
        vec![3, 5]
    );

    // Pressing Stop a second time before the cancelled turn ends does not
    // request a duplicate `session/cancel`, and keeps engine prompts intact.
    let second_stop = queue.request_stop();
    assert!(!second_stop.send_cancel);
    assert!(second_stop.active_turn);
    assert!(second_stop.returned_operator_prompts.is_empty());

    // Once the cancelled turn finishes, `cancel_requested` clears and queued
    // engine prompts proceed in FIFO order.
    queue.on_wire_prompt_finished(&RequestId::Number(5));
    assert!(!queue.cancel_requested());
    assert!(queue.can_send());

    let next = queue.begin_send(RequestId::Number(6), 200).unwrap();
    assert_eq!(next.prompt.intent_id, 3);
    queue.on_wire_prompt_finished(&RequestId::Number(6));

    let last = queue.begin_send(RequestId::Number(7), 300).unwrap();
    assert_eq!(last.prompt.intent_id, 5);
    queue.on_wire_prompt_finished(&RequestId::Number(7));
    assert_eq!(queue.queued_len(), 0);
}

#[test]
fn stop_during_agent_started_turn_and_while_idle() {
    let mut queue = TurnQueue::new();

    // Stopping while idle sends no cancel.
    let idle_stop = queue.request_stop();
    assert!(!idle_stop.send_cancel);
    assert!(!idle_stop.active_turn);
    assert!(idle_stop.returned_operator_prompts.is_empty());

    // Stopping during an agent-started turn sends cancel, returns queued
    // operator prompts, and keeps engine prompts until the turn finishes.
    queue.on_agent_turn_started("autonomous-turn-9");
    queue.enqueue(prompt(1, PromptOrigin::Operator, "user msg"));
    queue.enqueue(prompt(2, PromptOrigin::Engine, "engine msg"));

    let stop = queue.request_stop();
    assert!(stop.send_cancel);
    assert!(stop.active_turn);
    assert_eq!(stop.returned_operator_prompts.len(), 1);
    assert_eq!(stop.returned_operator_prompts[0].intent_id, 1);
    assert!(queue.cancel_requested());

    queue.on_agent_turn_finished("autonomous-turn-9");
    assert!(!queue.cancel_requested());
    assert!(queue.can_send());
    assert_eq!(queue.peek_next_dispatchable().unwrap().intent_id, 2);
}

#[test]
fn recover_unsent_dispatches_preserves_original_order_within_each_origin() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "op-1"));
    queue.enqueue(prompt(2, PromptOrigin::Operator, "op-2"));
    queue.enqueue(prompt(3, PromptOrigin::Engine, "eng-1"));
    queue.enqueue(prompt(4, PromptOrigin::Engine, "eng-2"));

    // Simulate intents 3 (engine) and 1 (operator) marked `Sent` in intent log
    // before a crash, without ever reaching the wire journal.
    queue.mark_intent_sent(3, RequestId::Number(10), 100);
    queue.mark_intent_sent(1, RequestId::Number(11), 101);
    assert_eq!(queue.dispatched_awaiting_wire().len(), 2);

    let recovered = queue.recover_unsent_dispatches();
    assert_eq!(recovered.iter().map(|p| p.intent_id).collect::<Vec<_>>(), vec![1, 3]);
    assert!(queue.dispatched_awaiting_wire().is_empty());

    // Engine prompts remain ahead of operator prompts, and each origin keeps
    // its original intent_id FIFO ordering (`3` before `4`, `1` before `2`).
    let order: Vec<u64> = queue.queued().iter().map(|p| p.intent_id).collect();
    assert_eq!(order, vec![3, 4, 1, 2]);
}

#[test]
fn request_id_matching_distinguishes_numeric_from_string_ids() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "numeric"));
    queue.mark_intent_sent(1, RequestId::Number(7), 100);

    // String `"7"` must not match numeric `7`.
    assert!(queue.on_wire_prompt_sent(&RequestId::Str("7".into()), 10).is_none());
    assert!(queue.on_wire_prompt_sent(&RequestId::Number(7), 10).is_some());
    assert!(queue.on_wire_prompt_finished(&RequestId::Str("7".into())).is_none());
    assert!(queue.on_wire_prompt_finished(&RequestId::Number(7)).is_some());
}

#[test]
fn stray_or_late_wire_cancel_does_not_suppress_next_turn_stop() {
    let mut queue = TurnQueue::new();

    // A stray `session/cancel` observed while idle must not arm `cancel_requested`.
    queue.note_wire_cancel_sent();
    assert!(!queue.cancel_requested());

    // Starting a prompt turn afterwards still allows Stop to send `session/cancel`.
    queue.enqueue(prompt(1, PromptOrigin::Operator, "next turn"));
    queue.mark_intent_sent(1, RequestId::Number(10), 100);
    // Even if a late cancel arrives while awaiting wire echo (before the turn is active),
    // it must not suppress Stop once the prompt reaches the wire.
    queue.note_wire_cancel_sent();
    assert!(!queue.cancel_requested());

    queue.on_wire_prompt_sent(&RequestId::Number(10), 1);
    assert!(queue.plan_stop().send_cancel);
    assert!(queue.request_stop().send_cancel);
    assert!(queue.cancel_requested());
    queue.on_wire_prompt_finished(&RequestId::Number(10));
    assert!(!queue.cancel_requested());

    // A late cancel arriving right after the turn closed must not suppress Stop
    // on a subsequent agent-started turn either.
    queue.note_wire_cancel_sent();
    assert!(!queue.cancel_requested());
    queue.on_agent_turn_started("autonomous-turn-1");
    assert!(queue.request_stop().send_cancel);
}

#[test]
fn withdrawn_sent_prompt_is_not_requeued_by_reconcile_when_echo_never_arrives() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "stale"));
    queue.mark_intent_sent(1, RequestId::Number(7), 100).unwrap();
    assert!(queue.apply_withdrawn_intent(1).is_some());
    assert!(queue.is_busy(), "adapter may still be running the dispatched prompt");

    let recovered = queue.recover_unsent_dispatches();
    assert!(recovered.is_empty());
    assert!(queue.queued().is_empty());
    assert!(!queue.is_busy());
}

#[test]
fn withdrawn_sent_prompt_still_binds_its_echo() {
    let mut queue = TurnQueue::new();
    queue.enqueue(prompt(1, PromptOrigin::Operator, "stale"));
    queue.mark_intent_sent(1, RequestId::Number(7), 100).unwrap();
    queue.apply_withdrawn_intent(1);
    let bound = queue.on_wire_prompt_sent(&RequestId::Number(7), 3).unwrap();
    assert_eq!(bound.prompt.intent_id, 1);
    assert!(queue.recover_unsent_dispatches().is_empty());
    assert!(queue.queued().is_empty());
}
