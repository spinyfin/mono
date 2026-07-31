//! Unit tests for the driver-abstraction vocabulary defined in `lib.rs`.

use super::*;

/// The trait default must be the *safe* answer. A new driver that says
/// nothing about mid-turn stdin gets `Rejects`, so the engine never writes
/// into a pane whose foreground process might leave the bytes in the tty
/// for the interactive shell to execute. Buffering has to be claimed
/// deliberately, with evidence about the process.
#[test]
fn mid_turn_pane_input_defaults_to_rejects() {
    let stub = test_support::StubDriver::new(test_support::stub_descriptor(), CapabilitySet::new([]));
    assert_eq!(stub.mid_turn_pane_input(), MidTurnPaneInput::Rejects);
    assert!(!MidTurnPaneInput::Rejects.buffers());
    assert!(MidTurnPaneInput::Buffers.buffers());
}

/// ControlVerbs defaults are the safe answers: probe/interrupt absent
/// (fire-and-forget), stop/reap always available at the process level.
#[test]
fn control_verbs_default_to_safe_answers() {
    let stub = test_support::StubDriver::new(test_support::stub_descriptor(), CapabilitySet::new([]));
    assert_eq!(stub.probe(), ProbeDelivery::Unsupported);
    assert_eq!(stub.interrupt(), InterruptDelivery::Unsupported);
    assert_eq!(stub.stop(), StopDelivery::ProcessOnly);
    assert_eq!(stub.reap(), ReapDelivery::ProcessGroup);
}

#[test]
fn hook_wiring_destination_defaults_to_worker_settings_file() {
    assert_eq!(
        HookWiringDestination::default(),
        HookWiringDestination::WorkerSettingsFile,
    );
    let wiring = ProgressObservationWiring::default();
    assert_eq!(wiring.destination, HookWiringDestination::WorkerSettingsFile);
    assert!(wiring.hooks.is_empty());
}

/// The trait default must reproduce the behaviour every process-liveness
/// reaper had before this property existed: a vanished process is a dead
/// worker. A driver only escapes that by declaring one-turn-per-process
/// deliberately — silence must never buy an exemption from being reaped.
#[test]
fn worker_process_lifetime_defaults_to_persistent() {
    let stub = test_support::StubDriver::new(test_support::stub_descriptor(), CapabilitySet::new([]));
    assert_eq!(stub.worker_process_lifetime(), WorkerProcessLifetime::Persistent);
}

/// All three document-producing / design-family kinds (`Design`,
/// `Investigation`, `DesignPostmortem` — Codex-eligibility Phase 2) escalate
/// the same pair to required-strict, not just `Design`.
#[test]
fn required_strict_capabilities_refuse_absent_driver() {
    for kind in [TaskKind::Design, TaskKind::Investigation, TaskKind::DesignPostmortem] {
        let reqs = KindRequirements::for_kind(kind.clone());
        let no_caps = CapabilitySet::new([]);

        assert_eq!(
            reqs.resolve_absence_disposition(Capability::StructuredOutput, &no_caps),
            Some(AbsenceDisposition::Refuse),
            "{kind:?} should refuse absent StructuredOutput",
        );
        assert_eq!(
            reqs.resolve_absence_disposition(Capability::ToolUseInterception, &no_caps),
            Some(AbsenceDisposition::Refuse),
            "{kind:?} should refuse absent ToolUseInterception",
        );
    }
}

#[test]
fn non_strict_capability_uses_default_disposition() {
    let reqs = KindRequirements::for_kind(TaskKind::Design);
    let no_caps = CapabilitySet::new([]);

    // ModelAndEffortMenu is not required-strict for Design; default is Degrade.
    assert_eq!(
        reqs.resolve_absence_disposition(Capability::ModelAndEffortMenu, &no_caps),
        Some(AbsenceDisposition::Degrade),
    );
}

#[test]
fn provided_capability_resolves_to_none() {
    let reqs = KindRequirements::for_kind(TaskKind::Design);
    let all_caps = CapabilitySet::new([Capability::StructuredOutput, Capability::ToolUseInterception]);

    assert_eq!(
        reqs.resolve_absence_disposition(Capability::StructuredOutput, &all_caps),
        None,
    );
}

#[test]
fn absence_override_takes_precedence_over_default() {
    let caps =
        CapabilitySet::new([]).with_absence_override(Capability::ToolUseInterception, AbsenceDisposition::Refuse);

    // Default for ToolUseInterception is Degrade; override makes it Refuse.
    assert_eq!(
        caps.absence_disposition(Capability::ToolUseInterception),
        AbsenceDisposition::Refuse,
    );
}

/// The document-producing / design-family kinds (`Design`, `Investigation`,
/// `DesignPostmortem`) are covered separately by
/// `required_strict_capabilities_refuse_absent_driver` — they DO escalate.
#[test]
fn task_kind_has_no_strict_requirements_by_default() {
    for kind in [
        TaskKind::Chore,
        TaskKind::ProjectTask,
        TaskKind::Revision,
        TaskKind::Task,
    ] {
        let reqs = KindRequirements::for_kind(kind.clone());
        assert!(
            !reqs.is_required_strict(Capability::StructuredOutput),
            "{kind:?} should not require-strict StructuredOutput",
        );
        assert!(
            !reqs.is_required_strict(Capability::ToolUseInterception),
            "{kind:?} should not require-strict ToolUseInterception",
        );
    }
}

#[test]
fn spawn_and_prompt_composition_refuse_when_absent() {
    assert_eq!(
        Capability::Spawn.default_absence_disposition(),
        AbsenceDisposition::Refuse,
    );
    assert_eq!(
        Capability::PromptComposition.default_absence_disposition(),
        AbsenceDisposition::Refuse,
    );
    assert_eq!(
        Capability::WorkspaceProvisioning.default_absence_disposition(),
        AbsenceDisposition::Refuse,
    );
    assert_eq!(
        Capability::PermissionPolicy.default_absence_disposition(),
        AbsenceDisposition::Refuse,
    );
}

#[test]
fn progress_and_turn_boundary_synthesize_when_absent() {
    assert_eq!(
        Capability::ProgressObservation.default_absence_disposition(),
        AbsenceDisposition::Synthesize,
    );
    assert_eq!(
        Capability::TurnBoundary.default_absence_disposition(),
        AbsenceDisposition::Synthesize,
    );
}

#[test]
fn awaiting_input_signal_degrades_when_absent_not_synthesizes() {
    // Must never be Synthesize: Boss must not guess WaitingForInput
    // from a lower-fidelity signal when the driver can't back it.
    assert_eq!(
        Capability::AwaitingInputSignal.default_absence_disposition(),
        AbsenceDisposition::Degrade,
    );
}

#[test]
fn command_outcome_observation_degrades_when_absent_not_synthesizes() {
    // Must never be Synthesize: Boss must not guess a command's
    // exit status from activity alone when the driver never observed it
    // (Codex's rollout `exit_code`/`status` fields are unreliable —
    // sometimes absent, projection-dropped, or truncated-unparseable).
    assert_eq!(
        Capability::CommandOutcomeObservation.default_absence_disposition(),
        AbsenceDisposition::Degrade,
    );
}

#[test]
fn post_hoc_interception_action_variants_are_distinct() {
    assert_eq!(PostHocInterceptionAction::Accept, PostHocInterceptionAction::Accept);
    let edit = PostHocInterceptionAction::RequestEdit {
        reason: "bad content".to_owned(),
    };
    assert_ne!(PostHocInterceptionAction::Accept, edit);
}

#[test]
fn driver_default_registers_no_post_hoc_interception_fn() {
    // A driver that never overrides `post_hoc_interception` (every driver
    // today, including Claude) must resolve to `None` — the trait
    // default. Degrade-path dispatch relies on this to mean "no
    // registered fn" rather than a stale/leftover Some.
    let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
    assert!(driver.post_hoc_interception().is_none());
}

#[test]
fn stub_driver_registers_and_invokes_post_hoc_interception_fn() {
    fn always_request_edit(
        _tool_name: &str,
        _tool_input: &serde_json::Value,
        _tool_output: &serde_json::Value,
    ) -> PostHocInterceptionAction {
        PostHocInterceptionAction::RequestEdit {
            reason: "fixture".to_owned(),
        }
    }

    let driver =
        StubDriver::new(stub_descriptor(), CapabilitySet::new([])).with_post_hoc_interception(always_request_edit);
    let f = driver.post_hoc_interception().expect("fn was registered");
    assert_eq!(
        f("Bash", &serde_json::Value::Null, &serde_json::Value::Null),
        PostHocInterceptionAction::RequestEdit {
            reason: "fixture".to_owned(),
        },
    );
}

#[test]
fn tool_use_interception_wiring_default_is_empty() {
    let wiring = ToolUseInterceptionWiring::default();
    assert!(wiring.pre_tool_use_hooks.is_empty());
}

#[test]
fn tool_use_interception_config_fields_are_accessible() {
    let config = ToolUseInterceptionConfig {
        data_dir: Some(PathBuf::from("/Library/Boss")),
        path_guard_script: Some(PathBuf::from("/tmp/boss-path-guard.py")),
        checkleft_guard_script: Some(PathBuf::from("/tmp/boss-checkleft-push-guard.py")),
        is_revision: true,
        is_standard_worker: true,
        run_id: Some("run-1".into()),
        workspace_path: Some(PathBuf::from("/ws")),
    };
    assert!(config.is_revision);
    assert!(config.is_standard_worker);
    assert_eq!(config.data_dir.unwrap(), PathBuf::from("/Library/Boss"));
    assert_eq!(config.run_id.as_deref(), Some("run-1"));
}

#[test]
fn rich_fidelity_reuses_the_passed_in_default_threshold_unchanged() {
    // Claude declares Rich; the sweep must reuse whatever threshold it is
    // configured with (30 min in production) so its behaviour is
    // unchanged by this mapping existing.
    assert_eq!(ProgressFidelity::Rich.stale_threshold_secs(1_800), Some(1_800));
    assert_eq!(ProgressFidelity::Rich.stale_threshold_secs(42), Some(42));
}

#[test]
fn coarse_and_minimal_fidelity_are_exempt_from_cadence_staleness() {
    assert_eq!(ProgressFidelity::Coarse.stale_threshold_secs(1_800), None);
    assert_eq!(ProgressFidelity::Minimal.stale_threshold_secs(1_800), None);
}

#[test]
fn all_capabilities_covers_every_variant() {
    let all: Vec<_> = Capability::all().collect();
    // Every variant must appear exactly once.
    assert_eq!(all.len(), 14, "Capability::all() must cover all 14 variants");
    // Spot-check a few to ensure the enum and all() stay in sync.
    assert!(all.contains(&Capability::Spawn));
    assert!(all.contains(&Capability::StructuredOutput));
    assert!(all.contains(&Capability::PromptComposition));
}

#[test]
fn capability_resolver_returns_ok_plan_when_no_refused_caps() {
    // A driver that provides every capability must always yield Ok for any kind.
    let all_caps = CapabilitySet::new(Capability::all());
    let driver = StubDriver::new(stub_descriptor(), all_caps);
    let resolver = CapabilityResolver::new(&driver);
    let plan = resolver.check_dispatch(&TaskKind::Design).unwrap();
    assert!(plan.is_full_fidelity(), "full-capability driver must be full-fidelity");
    assert_eq!(plan.driver_name, "stub");
}

#[test]
fn capability_resolver_refuses_design_task_without_structured_output() {
    let caps = CapabilitySet::new([Capability::Spawn, Capability::PromptComposition]);
    let driver = StubDriver::new(stub_descriptor(), caps);
    let resolver = CapabilityResolver::new(&driver);
    let err = resolver.check_dispatch(&TaskKind::Design).unwrap_err();
    assert!(
        err.refused.contains(&Capability::StructuredOutput),
        "Design kind must refuse StructuredOutput when absent: {:?}",
        err.refused,
    );
    assert!(
        err.refused.contains(&Capability::ToolUseInterception),
        "Design kind must refuse ToolUseInterception when absent: {:?}",
        err.refused,
    );
}

#[test]
fn capability_resolver_refuses_any_kind_without_spawn() {
    // Spawn has Refuse as its global default; any kind without Spawn fails.
    let caps = CapabilitySet::new(Capability::all().filter(|c| *c != Capability::Spawn));
    let driver = StubDriver::new(stub_descriptor(), caps);
    let resolver = CapabilityResolver::new(&driver);
    let err = resolver.check_dispatch(&TaskKind::Chore).unwrap_err();
    assert!(
        err.refused.contains(&Capability::Spawn),
        "Spawn must be refused when absent: {:?}",
        err.refused,
    );
}

#[test]
fn dispatch_plan_degraded_and_synthesized_populated_for_partial_driver() {
    // ModelAndEffortMenu is Degrade by default; ProgressObservation is Synthesize.
    let caps = CapabilitySet::new(
        Capability::all().filter(|c| *c != Capability::ModelAndEffortMenu && *c != Capability::ProgressObservation),
    );
    let driver = StubDriver::new(stub_descriptor(), caps);
    let resolver = CapabilityResolver::new(&driver);
    let plan = resolver.check_dispatch(&TaskKind::Chore).unwrap();
    assert!(!plan.is_full_fidelity());
    assert!(
        plan.degraded.contains(&Capability::ModelAndEffortMenu),
        "ModelAndEffortMenu must appear in degraded: {:?}",
        plan.degraded,
    );
    assert!(
        plan.synthesized.contains(&Capability::ProgressObservation),
        "ProgressObservation must appear in synthesized: {:?}",
        plan.synthesized,
    );
}

#[test]
fn capability_gate_error_message_names_driver_and_refused_caps() {
    let err = CapabilityGateError {
        driver_name: "copilot",
        driver_label: "GitHub Copilot CLI",
        task_kind: TaskKind::Design,
        refused: vec![Capability::StructuredOutput, Capability::ToolUseInterception],
    };
    let msg = err.to_string();
    assert!(msg.contains("GitHub Copilot CLI"), "error must name the driver label");
    assert!(msg.contains("design"), "error must name the task kind");
    assert!(msg.contains("StructuredOutput"), "error must name refused caps");
    assert!(msg.contains("ToolUseInterception"), "error must name refused caps");
}

// ── Test-only stub driver ──────────────────────────────────────────────
//
// Shared with other crates' tests via `crate::test_support::StubDriver`;
// see that module's doc comment.

use crate::test_support::{StubDriver, stub_descriptor};

#[test]
fn a_driver_declaring_no_turn_boundary_reports_none() {
    // The absence case the (deliberately unbuilt) engine-side synthesizer
    // would cover: no boundary from the driver, for any event.
    let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
    assert!(
        driver
            .turn_boundary(&WorkerEvent::Stop {
                session_id: "sess-1".to_owned(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            })
            .is_none(),
        "a driver without Capability::TurnBoundary must not claim a boundary",
    );
}

// ── pr_url_capture_feed / default_pr_url_capture_feed ──────────────────

#[test]
fn default_feed_reads_claude_bash_stdout_stderr_shape() {
    let input = serde_json::json!({
        "command": "cube pr create --branch boss/exec_x --title t"
    });
    let response = serde_json::json!({
        "stdout": "https://github.com/spinyfin/mono/pull/458",
        "stderr": "",
    });
    let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
    assert_eq!(feed.output_text, "https://github.com/spinyfin/mono/pull/458");
    assert_eq!(feed.command, "cube pr create --branch boss/exec_x --title t");
    assert_eq!(
        boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
        Some("https://github.com/spinyfin/mono/pull/458"),
    );
}

#[test]
fn default_feed_prefers_stdout_url_over_stderr_url() {
    let input = serde_json::json!({ "command": "gh pr create --title t" });
    let response = serde_json::json!({
        "stdout": "https://github.com/spinyfin/mono/pull/458",
        "stderr": "https://github.com/spinyfin/mono/pull/100",
    });
    let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
    assert_eq!(
        boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
        Some("https://github.com/spinyfin/mono/pull/458"),
    );
}

#[test]
fn default_feed_falls_back_to_stderr_when_stdout_empty() {
    let input = serde_json::json!({ "command": "gh pr create --title t" });
    let response = serde_json::json!({
        "stdout": "",
        "stderr": "Created: https://github.com/spinyfin/mono/pull/458\n",
    });
    let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
    assert_eq!(
        boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
        Some("https://github.com/spinyfin/mono/pull/458"),
    );
}

#[test]
fn default_feed_reads_codex_aggregated_output_string_shape() {
    // Mirrors what a stdout-JSONL normaliser emits after mapping
    // `item.command` / `item.aggregated_output` onto PostToolUse as
    // bare strings (see stdout-progress Codex-shaped test driver).
    let input = serde_json::json!("/bin/zsh -lc 'cube pr create --branch boss/x --title t'");
    let response = serde_json::json!("https://github.com/spinyfin/mono/pull/99\n");
    let feed = default_pr_url_capture_feed("Bash", &input, &response).expect("feed");
    assert_eq!(feed.command, "/bin/zsh -lc 'cube pr create --branch boss/x --title t'");
    assert_eq!(
        boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).as_deref(),
        Some("https://github.com/spinyfin/mono/pull/99"),
    );
}

#[test]
fn default_feed_rejects_non_bash_tools() {
    let input = serde_json::json!({ "command": "gh pr create" });
    let response = serde_json::json!({
        "stdout": "https://github.com/spinyfin/mono/pull/1",
    });
    assert!(default_pr_url_capture_feed("Read", &input, &response).is_none());
}

#[test]
fn default_feed_rejects_unrecognised_response_shape() {
    let input = serde_json::json!({ "command": "gh pr create" });
    assert!(default_pr_url_capture_feed("Bash", &input, &serde_json::json!(null)).is_none());
    assert!(default_pr_url_capture_feed("Bash", &input, &serde_json::json!(42)).is_none());
    assert!(default_pr_url_capture_feed("Bash", &input, &serde_json::json!({ "other": true })).is_none());
}

#[test]
fn trait_default_pr_url_capture_feed_matches_free_function() {
    // StubDriver uses the trait default; it must match the free function
    // so an un-overridden driver still feeds PR-URL capture.
    let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
    let input = serde_json::json!("cube pr create --branch b");
    let response = serde_json::json!("see https://github.com/o/r/pull/7\n");
    assert_eq!(
        driver.pr_url_capture_feed("Bash", &input, &response),
        default_pr_url_capture_feed("Bash", &input, &response),
    );
}

// ── structured_output_wiring / default_structured_output_wiring ────────

fn so_request<'a>(
    kind: StructuredOutputKind,
    result_path: &'a Path,
    schema: Option<&'a serde_json::Value>,
) -> StructuredOutputRequest<'a> {
    StructuredOutputRequest {
        kind,
        result_path,
        schema,
    }
}

#[test]
fn default_wiring_exports_pr_url_env_and_echoes_result_path() {
    let path = PathBuf::from("/tmp/boss-worker-output/exec_1.pr-url.json");
    let arts = default_structured_output_wiring(&so_request(StructuredOutputKind::PrUrl, &path, None));
    assert_eq!(
        arts.env,
        vec![(
            boss_engine_structured_output::PR_URL_OUTPUT_ENV.to_owned(),
            path.display().to_string(),
        )],
    );
    assert!(arts.extra_args.is_empty(), "env-file contract has no CLI flags");
    assert_eq!(arts.result_path, path);
}

#[test]
fn default_wiring_exports_structured_output_env_for_non_pr_kinds() {
    let path = PathBuf::from("/tmp/boss-worker-output/exec_1.review-result.json");
    for kind in [
        StructuredOutputKind::ReviewResult,
        StructuredOutputKind::TriageDecision,
        StructuredOutputKind::Followups,
        StructuredOutputKind::PostmortemFollowups,
    ] {
        let arts = default_structured_output_wiring(&so_request(kind, &path, None));
        assert_eq!(
            arts.env,
            vec![(
                boss_engine_structured_output::STRUCTURED_OUTPUT_ENV.to_owned(),
                path.display().to_string(),
            )],
            "{kind:?} must export BOSS_STRUCTURED_OUTPUT",
        );
        assert!(arts.extra_args.is_empty());
        assert_eq!(arts.result_path, path);
    }
}

#[test]
fn default_wiring_ignores_schema() {
    // The common-denominator contract has no native schema enforcement;
    // schema is carried for richer drivers only.
    let path = PathBuf::from("/tmp/out.json");
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "pr_url": { "type": "string" } },
        "required": ["pr_url"],
    });
    let arts = default_structured_output_wiring(&so_request(StructuredOutputKind::PrUrl, &path, Some(&schema)));
    assert!(
        arts.extra_args.is_empty(),
        "default must not materialise schema into CLI flags: {:?}",
        arts.extra_args,
    );
    assert_eq!(arts.env.len(), 1);
    assert_eq!(arts.result_path, path);
}

#[test]
fn claude_wiring_matches_env_file_contract_with_no_behavioural_change() {
    // Claude expresses the existing BOSS_* env-file contract through the
    // trait method. Schema is ignored; no CLI flags; result path echoes.
    let path = PathBuf::from("/tmp/boss-worker-output/exec_x.followups.json");
    let schema = serde_json::json!({ "type": "array" });
    let request = so_request(StructuredOutputKind::Followups, &path, Some(&schema));

    let via_claude = ClaudeDriver
        .structured_output_wiring(&request)
        .expect("claude wiring is infallible");
    let via_default = default_structured_output_wiring(&request);

    assert_eq!(via_claude, via_default, "Claude must be the env-file contract");
    assert_eq!(
        via_claude.env,
        vec![(
            boss_engine_structured_output::STRUCTURED_OUTPUT_ENV.to_owned(),
            path.display().to_string(),
        )],
    );
    assert!(via_claude.extra_args.is_empty());
    assert_eq!(via_claude.result_path, path);
}

#[test]
fn trait_default_wiring_matches_free_function() {
    // StubDriver uses the trait default; un-overridden drivers still get
    // the env-file contract without implementing the richer method.
    let driver = StubDriver::new(stub_descriptor(), CapabilitySet::new([]));
    let path = PathBuf::from("/tmp/out.triage.json");
    let request = so_request(StructuredOutputKind::TriageDecision, &path, None);
    assert_eq!(
        driver.structured_output_wiring(&request).unwrap(),
        default_structured_output_wiring(&request),
    );
}

#[test]
fn schema_capable_driver_passes_schema_and_result_path_to_cli() {
    // Shape a richer driver (Codex `--output-schema` /
    // `--output-last-message`) would produce: start from the env-file
    // fallback, materialise the opaque schema next to the result path,
    // and append the native CLI flags. Engine applies
    // `StructuredOutputArtifacts` generically — no further trait change
    // needed when a real Codex driver lands.
    struct SchemaCapableDriver;

    impl SchemaCapableDriver {
        fn wiring(request: &StructuredOutputRequest<'_>) -> anyhow::Result<StructuredOutputArtifacts> {
            let mut arts = default_structured_output_wiring(request);
            if let Some(schema) = request.schema {
                // `foo.json` → `foo.schema.json` so the schema sits next
                // to the result without colliding with it.
                let schema_path = request.result_path.with_extension("schema.json");
                if let Some(parent) = schema_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&schema_path, serde_json::to_vec_pretty(schema)?)?;
                arts.extra_args.push("--output-schema".to_owned());
                arts.extra_args.push(schema_path.display().to_string());
                arts.extra_args.push("--output-last-message".to_owned());
                arts.extra_args.push(request.result_path.display().to_string());
            }
            Ok(arts)
        }
    }

    let dir = std::env::temp_dir().join(format!(
        "boss-so-schema-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let result_path = dir.join("exec_1.review-result.json");
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "findings": { "type": "array" }
        },
        "required": ["findings"],
    });
    let request = so_request(StructuredOutputKind::ReviewResult, &result_path, Some(&schema));

    let arts = SchemaCapableDriver::wiring(&request).expect("schema wiring");

    // File-contract fallback still present.
    assert_eq!(
        arts.env,
        vec![(
            boss_engine_structured_output::STRUCTURED_OUTPUT_ENV.to_owned(),
            result_path.display().to_string(),
        )],
    );
    assert_eq!(arts.result_path, result_path, "engine still reads the designated path");

    // Native flags carry schema path + result path.
    let schema_path = result_path.with_extension("schema.json");
    assert_eq!(
        arts.extra_args,
        vec![
            "--output-schema".to_owned(),
            schema_path.display().to_string(),
            "--output-last-message".to_owned(),
            result_path.display().to_string(),
        ],
    );
    // Schema was materialised as whatever the caller supplied.
    let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&schema_path).unwrap()).unwrap();
    assert_eq!(written, schema);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn env_name_helper_splits_pr_url_from_designated_payload() {
    assert_eq!(
        structured_output_env_name(StructuredOutputKind::PrUrl),
        boss_engine_structured_output::PR_URL_OUTPUT_ENV,
    );
    assert_eq!(
        structured_output_env_name(StructuredOutputKind::Followups),
        boss_engine_structured_output::STRUCTURED_OUTPUT_ENV,
    );
}
