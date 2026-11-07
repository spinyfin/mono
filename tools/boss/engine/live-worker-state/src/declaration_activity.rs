use super::*;

/// Worker-originated evidence for a completion question, captured atomically.
pub struct DeclarationActivity {
    pub last_activity_at: Option<String>,
    pub at_turn_boundary: bool,
    pub tool_in_flight: bool,
}

impl LiveWorkerStateRegistry {
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_declaration_activity_at_for_test(&self, slot_id: u8, timestamp: String) {
        self.inner
            .lock()
            .unwrap()
            .get_mut(&slot_id)
            .unwrap()
            .meta
            .declaration_activity_at = Some(timestamp);
    }

    /// Engine display updates cannot refresh this clock. Unknown turn/tool
    /// state cannot authorize a question; only a real Stop can do that.
    pub fn declaration_activity_for_run(&self, run_id: &str) -> Option<DeclarationActivity> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        let mut result: Option<DeclarationActivity> = None;
        for entry in guard
            .values()
            .filter(|entry| entry.state.run_id == run_id && !entry.state.activity.is_terminal())
        {
            let activity = DeclarationActivity {
                last_activity_at: entry.meta.declaration_activity_at.clone(),
                tool_in_flight: matches!(
                    entry.meta.semantic_tool_condition,
                    SemanticToolCondition::InFlight | SemanticToolCondition::Yielded
                ),
                at_turn_boundary: entry.meta.semantic_turn_condition == SemanticTurnCondition::Ended
                    && entry.meta.semantic_tool_condition == SemanticToolCondition::Idle,
            };
            match &mut result {
                Some(result) => {
                    result.at_turn_boundary &= activity.at_turn_boundary;
                    result.tool_in_flight |= activity.tool_in_flight;
                    result.last_activity_at = result.last_activity_at.take().max(activity.last_activity_at);
                }
                None => result = Some(activity),
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_hook_refreshes_silence_without_rewinding_the_stop_boundary() {
        let states = LiveWorkerStateRegistry::new();
        states.register_spawn(1, "run", "driver", 42, None);
        states.apply_sequenced_event(
            1,
            Some(20),
            &WorkerEvent::Stop {
                session_id: "s".into(),
                stop_reason: boss_protocol::StopReason::Completed,
                stop_hook_active: false,
            },
        );
        let old = "2000-01-01T00:00:00Z";
        states.set_declaration_activity_at_for_test(1, old.into());
        let result = states.apply_sequenced_event(
            1,
            Some(19),
            &WorkerEvent::PostToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );
        assert!(matches!(result, ApplyEventOutcome::Stale { .. }));
        let activity = states.declaration_activity_for_run("run").unwrap();
        assert_ne!(activity.last_activity_at.as_deref(), Some(old));
        assert!(activity.at_turn_boundary);
        assert!(!activity.tool_in_flight);
    }

    #[test]
    fn driver_activity_refreshes_silence_but_stop_and_display_updates_do_not() {
        let states = LiveWorkerStateRegistry::new();
        states.register_spawn(1, "run", "driver", 42, None);
        let old = "2000-01-01T00:00:00Z";
        for event in [
            WorkerEvent::ModelProgress { session_id: "s".into() },
            WorkerEvent::ToolPoll {
                session_id: "s".into(),
                awaiting_model: true,
            },
            WorkerEvent::Notification {
                session_id: "s".into(),
                message: "permission".into(),
            },
        ] {
            states.set_declaration_activity_at_for_test(1, old.into());
            states.apply_event(1, &event);
            let activity = states.declaration_activity_for_run("run").unwrap();
            assert_ne!(activity.last_activity_at.as_deref(), Some(old));
            assert!(!activity.at_turn_boundary);
        }
        states.set_declaration_activity_at_for_test(1, old.into());
        states.set_live_status(1, Some("engine display update".into()));
        states.apply_event(
            1,
            &WorkerEvent::Stop {
                session_id: "s".into(),
                stop_reason: boss_protocol::StopReason::Completed,
                stop_hook_active: false,
            },
        );
        let activity = states.declaration_activity_for_run("run").unwrap();
        assert_eq!(activity.last_activity_at.as_deref(), Some(old));
        assert!(activity.at_turn_boundary);
        states.apply_event(
            1,
            &WorkerEvent::UserPromptSubmit {
                session_id: "s".into(),
                prompt: "continue".into(),
            },
        );
        assert!(!states.declaration_activity_for_run("run").unwrap().at_turn_boundary);
    }
}
