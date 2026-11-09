use super::*;
use boss_protocol::{Product, Project, ProjectStatus, Task, TaskKind, TaskStatus};

fn failed_run_and_execution() -> (WorkRun, WorkExecution) {
    let run = WorkRun::builder()
        .id("run_1")
        .agent_id("agent_1")
        .execution_id("exec_1")
        .created_at("1747000000")
        .status("failed")
        .error_text("pre-start failed: cube workspace goto failed")
        .build();
    let execution = WorkExecution::builder()
        .id("exec_1")
        .work_item_id("task_1")
        .kind(boss_protocol::ExecutionKind::ChoreImplementation)
        .status(boss_protocol::ExecutionStatus::Failed)
        .repo_remote_url("git@example.com:foo.git")
        .created_at("1747000000")
        .last_error("could not restage preserved work: base unresolvable")
        .build();
    (run, execution)
}

#[test]
fn status_text_shows_the_stored_run_and_execution_errors() {
    let (run, execution) = failed_run_and_execution();
    let lines = stored_error_lines(Some(&run), Some(&execution)).join("\n");
    assert!(
        lines.contains("pre-start failed: cube workspace goto failed"),
        "{lines}"
    );
    assert!(
        lines.contains("could not restage preserved work: base unresolvable"),
        "{lines}"
    );
    // Neither error stored: nothing is printed.
    let (mut quiet_run, mut quiet_execution) = failed_run_and_execution();
    quiet_run.error_text = None;
    quiet_execution.last_error = None;
    assert!(stored_error_lines(Some(&quiet_run), Some(&quiet_execution)).is_empty());
}

#[test]
fn status_json_carries_the_stored_run_and_execution_errors() {
    let (run, execution) = failed_run_and_execution();
    let errors = stored_errors_json(Some(&run), Some(&execution));
    assert_eq!(errors["run_error_text"], "pre-start failed: cube workspace goto failed");
    assert_eq!(
        errors["execution_last_error"],
        "could not restage preserved work: base unresolvable"
    );
    let absent = stored_errors_json(None, None);
    assert!(absent["run_error_text"].is_null() && absent["execution_last_error"].is_null());
}

#[test]
fn failed_hosted_pane_shows_stored_errors() {
    let (run, execution) = failed_run_and_execution();
    let panes = [hosted_pane(
        2,
        "exec_1",
        "Data",
        HostedPaneState::DurableOccupantNoRegistry {
            status: "failed".to_owned(),
        },
    )];
    for reference in ["data", "2", "exec_1"] {
        let pane = resolve_hosted_pane_ref(reference, &panes)
            .expect("no ambiguity")
            .expect("reference should resolve");
        let json = hosted_pane_status_json(pane, Some(&run), Some(&execution));
        assert_eq!(
            json["errors"]["run_error_text"],
            "pre-start failed: cube workspace goto failed"
        );
        assert_eq!(
            json["errors"]["execution_last_error"],
            "could not restage preserved work: base unresolvable"
        );
    }
    // An error stored only on the run is shown without an execution error.
    let mut run_only = execution.clone();
    run_only.last_error = None;
    let json = hosted_pane_status_json(&panes[0], Some(&run), Some(&run_only));
    assert_eq!(
        json["errors"]["run_error_text"],
        "pre-start failed: cube workspace goto failed"
    );
    assert!(json["errors"]["execution_last_error"].is_null());
    let lines = stored_error_lines(Some(&run), Some(&run_only)).join("\n");
    assert!(
        lines.contains("pre-start failed") && !lines.contains("last error"),
        "{lines}"
    );
}

#[test]
fn tmux_adoption_state_label_marks_not_tmux_hosted_as_remote_detached() {
    assert_eq!(
        tmux_adoption_state_label(TmuxAdoptionState::NotTmuxHosted),
        "remote_detached"
    );
    assert_eq!(tmux_adoption_state_label(TmuxAdoptionState::Adopted), "adopted");
}

#[test]
fn agents_send_output_surfaces_the_probe_receipt() {
    let json = worker_input_sent_json("run-1", 4, Some("probe-9"));
    assert_eq!(json["probe_id"], "probe-9");
    assert_eq!(json["slot_id"], 4);
    assert!(worker_input_sent_json("run-1", 4, None)["probe_id"].is_null());

    let lines = worker_input_sent_lines("run-1", 4, Some("probe-9"));
    assert_eq!(lines[0], "sent input to slot 4 (run run-1)");
    assert_eq!(lines[1], "check delivery with: bossctl probe-status probe-9");
    assert_eq!(worker_input_sent_lines("run-1", 4, None).len(), 1);
}

/// Build a live-worker fixture with a caller-chosen slot id, run id, and
/// crew name. Setting `name` explicitly (rather than deriving it from
/// `slot_id` the way production does) lets the resolver tests target each
/// match tier — and the ambiguous-slot / ambiguous-name paths —
/// independently of the roster's slot→name mapping.
fn worker(slot_id: u8, run_id: &str, name: &str) -> LiveWorkerState {
    let mut state = LiveWorkerState::new_spawning(slot_id, run_id, "opus", 0, None);
    state.name = name.to_owned();
    state
}

#[test]
fn format_live_state_short_shows_pool_and_kind_dashes_when_unset() {
    let state = worker(3, "run_a", "Riker");
    let line = format_live_state_short(&state, TmuxListEvidence::Missing);
    assert!(
        line.contains("pool=-") && line.contains("kind=-"),
        "expected pool=- and kind=- placeholders when unset: {line}"
    );
    assert!(
        line.starts_with("slot 3  name=Riker  run=run_a  model=opus  activity=spawning  pool=-  kind=-"),
        "unexpected column order: {line}"
    );
}

#[test]
fn format_live_state_short_renders_declared_wait() {
    let mut state = worker(3, "run_a", "Riker");
    let plain = format_live_state_short(&state, TmuxListEvidence::Missing);
    assert!(!plain.contains("wait_reason"), "{plain}");
    state.wait_reason = Some("bazel test".to_owned());
    state.wait_expires_at = Some("2026-10-09T12:00:00Z".to_owned());
    let line = format_live_state_short(&state, TmuxListEvidence::Missing);
    assert!(
        line.contains("wait_reason=\"bazel test\"  wait_expires_at=2026-10-09T12:00:00Z"),
        "{line}"
    );
}

#[test]
fn format_live_state_short_renders_type_project_host_and_start() {
    let mut state = worker(2, "run_c", "Worf");
    state.agent_type = Some("coding".to_owned());
    state.project_id = Some("proj_1".to_owned());
    state.project_name = Some("Dynamic Agents pane layout".to_owned());
    state.host_id = Some("local".to_owned());
    state.started_at = Some("2026-10-09T01:02:03Z".to_owned());
    let line = format_live_state_short(&state, TmuxListEvidence::Missing);
    assert!(
        line.contains("kind=-  type=coding  project=proj_1  host=local  started_at=2026-10-09T01:02:03Z  tool="),
        "expected metadata columns after kind: {line}"
    );
    // Unfiled and older-engine rows print dashes rather than dropping columns.
    let bare = format_live_state_short(&worker(3, "run_d", "Riker"), TmuxListEvidence::Missing);
    assert!(
        bare.contains("type=-  project=-  host=-  started_at=-"),
        "expected placeholder metadata columns: {bare}"
    );
}

#[test]
fn project_status_label_distinguishes_unfiled_from_unreported() {
    let mut state = worker(1, "run_e", "Data");
    assert_eq!(project_status_label(&state), None, "older engine: no claim");
    state.host_id = Some("local".to_owned());
    assert_eq!(project_status_label(&state).as_deref(), Some("Unfiled"));
    state.project_id = Some("proj_9".to_owned());
    assert_eq!(project_status_label(&state).as_deref(), Some("proj_9"));
    state.project_name = Some("Nine".to_owned());
    assert_eq!(project_status_label(&state).as_deref(), Some("Nine (proj_9)"));
}

#[test]
fn format_live_state_short_renders_stamped_pool_and_kind() {
    let mut state = worker(5, "run_b", "Data");
    state.pool = Some("automation".to_owned());
    state.kind = Some("automation_triage".to_owned());
    state.held = true;
    let line = format_live_state_short(&state, TmuxListEvidence::FetchFailed);
    assert!(
        line.contains("pool=automation") && line.contains("kind=automation_triage"),
        "expected stamped pool+kind: {line}"
    );
    assert!(
        line.contains("held=true  tmux_session=-  adoption_state=probe_unavailable"),
        "held and fetch-failed tmux evidence should trail the row: {line}"
    );
}

#[test]
fn format_live_state_short_renders_tmux_observability_fields() {
    let state = worker(5, "exec_tmux", "Data");
    let status = TmuxWorkerStatus::builder()
        .execution_id("exec_tmux")
        .session_name("boss-worker-5")
        .adoption_state(TmuxAdoptionState::Adopted)
        .pane_dead(false)
        .last_output_at("2026-08-19T12:00:00Z")
        .attach_command("/opt/homebrew/bin/tmux -L boss attach-session -t boss-worker-5")
        .build();
    let line = format_live_state_short(&state, TmuxListEvidence::Present(&status));
    assert!(line.contains("tmux_session=boss-worker-5"), "missing session: {line}");
    assert!(
        line.contains("adoption_state=adopted"),
        "missing adoption state: {line}"
    );
    assert!(line.contains("pane_dead=false"), "missing pane state: {line}");
    assert!(
        line.contains("window_activity_at=2026-08-19T12:00:00Z"),
        "missing output time: {line}"
    );
}

#[test]
fn format_live_state_short_distinguishes_missing_row_from_fetch_failure() {
    let state = worker(1, "run_c", "Geordi");
    let missing = format_live_state_short(&state, TmuxListEvidence::Missing);
    let failed = format_live_state_short(&state, TmuxListEvidence::FetchFailed);
    assert!(
        missing.contains("adoption_state=no_status"),
        "engine answered with no row: {missing}"
    );
    assert!(
        failed.contains("adoption_state=probe_unavailable"),
        "engine could not answer: {failed}"
    );
}

fn adopted_status(session_name: Option<&str>, attach_command: Option<&str>) -> TmuxWorkerStatus {
    TmuxWorkerStatus::builder()
        .execution_id("exec_attach")
        .maybe_session_name(session_name)
        .adoption_state(TmuxAdoptionState::Adopted)
        .pane_dead(false)
        .maybe_attach_command(attach_command)
        .build()
}

#[test]
fn attach_command_from_status_refuses_non_adopted() {
    let status = TmuxWorkerStatus::builder()
        .execution_id("exec_attach")
        .session_name("boss-worker-1")
        .adoption_state(TmuxAdoptionState::TokenMismatch)
        .pane_dead(false)
        .attach_command("tmux -L boss attach-session -t boss-worker-1")
        .build();
    let err = attach_command_from_status("exec_attach", &status).unwrap_err();
    assert!(
        err.to_string().contains("adoption_state=token_mismatch"),
        "unexpected refusal: {err}"
    );
}

#[test]
fn attach_command_from_status_refuses_adopted_without_session_name() {
    let status = adopted_status(None, Some("/usr/bin/tmux -L boss attach-session -t missing"));
    let err = attach_command_from_status("exec_attach", &status).unwrap_err();
    assert!(
        err.to_string().contains("has no tmux session name"),
        "unexpected refusal: {err}"
    );
}

#[test]
fn attach_command_from_status_prints_engine_command_without_exec() {
    let command = "/opt/homebrew/bin/tmux -L boss attach-session -t boss-worker-5";
    let status = adopted_status(Some("boss-worker-5"), Some(command));
    let attach = attach_command_from_status("exec_attach", &status).expect("adopted status is attachable");
    assert_eq!(attach.session_name, "boss-worker-5");
    assert_eq!(attach.command, command);
    assert!(
        !attach.command.starts_with("exec "),
        "operator paste must not exec the shell: {}",
        attach.command
    );
}

fn task(id: &str, kind: TaskKind) -> Task {
    Task::builder()
        .id(id)
        .product_id("prod_1")
        .kind(kind)
        .name("n")
        .description("")
        .status(TaskStatus::Todo)
        .created_at("")
        .updated_at("")
        .build()
}

fn product(id: &str) -> Product {
    Product::builder()
        .id(id)
        .name("n")
        .slug("n")
        .description("")
        .status("active")
        .created_at("")
        .updated_at("")
        .build()
}

fn project(id: &str) -> Project {
    Project::builder()
        .id(id)
        .product_id("prod_1")
        .name("n")
        .slug("n")
        .description("")
        .goal("")
        .status(ProjectStatus::Planned)
        .created_at("")
        .updated_at("")
        .build()
}

// ---- resolve_document_path ---------------------------------------------

#[test]
fn resolve_document_path_leaves_absolute_path_untouched() {
    let cwd = Path::new("/some/other/dir");
    assert_eq!(resolve_document_path(cwd, "/abs/path/notes.md"), "/abs/path/notes.md");
}

#[test]
fn resolve_document_path_joins_relative_path_against_cwd() {
    let cwd = Path::new("/home/user/project");
    assert_eq!(
        resolve_document_path(cwd, "docs/notes.md"),
        "/home/user/project/docs/notes.md"
    );
}

// ---- resolve_agent_ref -------------------------------------------------

#[test]
fn resolves_by_exact_run_id() {
    let states = [worker(1, "exec_abc", "Riker"), worker(2, "exec_def", "Data")];
    let resolved = resolve_agent_ref("exec_def", &states).expect("run id should resolve");
    assert_eq!(resolved.run_id, "exec_def");
    assert_eq!(resolved.slot_id, 2);
}

#[test]
fn resolves_by_numeric_slot_id() {
    let states = [worker(1, "exec_abc", "Riker"), worker(7, "exec_def", "Yar")];
    let resolved = resolve_agent_ref("7", &states).expect("slot id should resolve");
    assert_eq!(resolved.slot_id, 7);
    assert_eq!(resolved.run_id, "exec_def");
}

#[test]
fn run_slot_and_persona_references_resolve_to_the_same_focus_target() {
    // `agents focus` sends only the resolved run id to the engine, so the
    // three reference forms must be interchangeable.
    let states = [worker(1, "exec_abc", "Riker"), worker(7, "exec_def", "Yar")];
    for reference in ["exec_def", "7", "yar"] {
        assert_eq!(
            resolve_agent_ref(reference, &states)
                .expect("reference should resolve")
                .run_id,
            "exec_def",
            "{reference}"
        );
    }
}

#[test]
fn resolves_by_name_case_insensitive() {
    let states = [worker(1, "exec_abc", "Riker"), worker(2, "exec_def", "Data")];
    let resolved = resolve_agent_ref("dATa", &states).expect("crew name should resolve");
    assert_eq!(resolved.slot_id, 2);
    assert_eq!(resolved.run_id, "exec_def");
}

#[test]
fn resolves_durable_personas_independently_of_slots_including_overflow() {
    let states = [worker(27, "local", "Riker"), worker(241, "remote", "Ensign 1 (Remote)")];
    assert_eq!(resolve_agent_ref("riker", &states).unwrap().run_id, "local");
    assert_eq!(
        resolve_agent_ref("ENSIGN 1 (REMOTE)", &states).unwrap().run_id,
        "remote"
    );
    assert_eq!(resolve_agent_ref("241", &states).unwrap().run_id, "remote");
    let panes = [hosted_pane(9, "historical", "Ensign 2", HostedPaneState::Husk)];
    assert_eq!(
        resolve_hosted_pane_ref("ensign 2", &panes).unwrap().unwrap().run_id,
        "historical"
    );
}

#[test]
fn missing_overflow_and_remote_names_never_become_work_item_selectors() {
    for name in ["Ensign 1", "ensign 56", "Ensign 3 (Remote)", "riker (REMOTE)"] {
        assert!(looks_like_name_or_slot(name), "{name}");
        assert!(!work_item_fallback_eligible(name), "{name}");
    }
    assert!(!looks_like_name_or_slot("Ensign engineering"));
}

/// Slot 4's crew name is "La Forge" — the space is part of the name,
/// and the case-insensitive *exact* match honours it.
#[test]
fn resolves_multiword_name_with_space() {
    let states = [worker(4, "exec_d", "La Forge")];
    let resolved = resolve_agent_ref("la forge", &states).expect("multi-word name should resolve");
    assert_eq!(resolved.slot_id, 4);
}

/// A numeric reference that matches one worker's run id and *also*
/// another worker's slot resolves to the run-id match — a defensive
/// case, since real run ids are never bare numbers, but it pins the
/// tier order (run id before slot).
#[test]
fn run_id_match_takes_precedence_over_slot() {
    let states = [worker(2, "1", "Data"), worker(1, "exec_a", "Riker")];
    let resolved = resolve_agent_ref("1", &states).expect("run id tier should win");
    assert_eq!(resolved.run_id, "1");
    assert_eq!(resolved.slot_id, 2, "run-id match must win over the slot match");
}

/// A reference that matches one worker's run id and *also* another
/// worker's name resolves to the run-id match — the run-id tier is
/// consulted first and short-circuits.
#[test]
fn run_id_match_takes_precedence_over_name() {
    let states = [worker(1, "shared", "Riker"), worker(2, "exec_def", "shared")];
    let resolved = resolve_agent_ref("shared", &states).expect("run id tier should win");
    assert_eq!(resolved.slot_id, 1, "run-id match must win over the name match");
    assert_eq!(resolved.run_id, "shared");
}

/// A numeric reference that matches one worker's slot and *also*
/// another worker's (numeric) name resolves to the slot match — the
/// slot tier is consulted before the name tier.
#[test]
fn slot_match_takes_precedence_over_name() {
    let states = [worker(5, "exec_abc", "Data"), worker(2, "exec_def", "5")];
    let resolved = resolve_agent_ref("5", &states).expect("slot tier should win");
    assert_eq!(resolved.slot_id, 5, "slot match must win over the name match");
    assert_eq!(resolved.run_id, "exec_abc");
}

#[test]
fn ambiguous_name_reports_all_candidates() {
    let states = [worker(1, "exec_abc", "Data"), worker(2, "exec_def", "Data")];
    let err = resolve_agent_ref("data", &states).expect_err("two workers share a name");
    let msg = err.to_string();
    assert!(msg.contains("matches multiple live workers"), "message was: {msg}");
    assert!(msg.contains("slot 1 (Data) run exec_abc"), "message was: {msg}");
    assert!(msg.contains("slot 2 (Data) run exec_def"), "message was: {msg}");
}

#[test]
fn ambiguous_slot_reports_all_candidates() {
    let states = [worker(3, "exec_abc", "Worf"), worker(3, "exec_def", "Riker")];
    let err = resolve_agent_ref("3", &states).expect_err("two workers share a slot");
    let msg = err.to_string();
    assert!(msg.contains("matches multiple live workers"), "message was: {msg}");
    assert!(msg.contains("slot 3 (Worf) run exec_abc"), "message was: {msg}");
    assert!(msg.contains("slot 3 (Riker) run exec_def"), "message was: {msg}");
}

#[test]
fn no_match_errors_with_live_candidates() {
    let states = [worker(2, "exec_def", "Data"), worker(1, "exec_abc", "Riker")];
    let err = resolve_agent_ref("nonesuch", &states).expect_err("no worker matches");
    let msg = err.to_string();
    assert!(msg.contains("no live worker matches `nonesuch`"), "message was: {msg}");
    // The candidate summary is appended and sorted by slot id.
    assert!(
        msg.contains("Live: slot 1 (Riker), slot 2 (Data)"),
        "message was: {msg}"
    );
}

#[test]
fn no_match_with_no_live_workers() {
    let err = resolve_agent_ref("anything", &[]).expect_err("no workers at all");
    let msg = err.to_string();
    assert!(msg.contains("no live worker matches `anything`"), "message was: {msg}");
    assert!(msg.contains("no live workers"), "message was: {msg}");
}

// ---- agents stop fallback ---------------------------------------------

/// `agents stop` must remain usable for a worker the engine has lost track
/// of — the shape whose `LiveWorkerState` was cleared while its process
/// kept running. An `exec_…` selector that misses the live list is the
/// signal that lets `agents_stop` forward it to the engine anyway rather
/// than reporting `no live worker matches` and leaving the operator to
/// `kill` by pid.
#[test]
fn execution_ids_are_recognised_for_the_untracked_stop_fallback() {
    assert!(looks_like_execution_id("exec_18c6a6add38b5fe0_97"));
    assert!(looks_like_execution_id("exec_"));
}

/// The fallback must stay narrow. A crew name or slot number that misses
/// is far more likely a typo than a stranded worker, and forwarding it to
/// the engine as a run id would turn a clear error into a silent no-op.
#[test]
fn names_and_slots_do_not_get_the_untracked_stop_fallback() {
    assert!(!looks_like_execution_id("Worf"));
    assert!(!looks_like_execution_id("3"));
    assert!(!looks_like_execution_id("markdown-striping"));
    assert!(!looks_like_execution_id("task_abc"));
}

// ---- pick_unique -------------------------------------------------------

#[test]
fn pick_unique_returns_sole_match() {
    let states = [worker(4, "exec_abc", "La Forge")];
    let resolved = pick_unique("La Forge", vec![&states[0]], &states).expect("exactly one match");
    assert_eq!(resolved.slot_id, 4);
    assert_eq!(resolved.run_id, "exec_abc");
}

#[test]
fn pick_unique_bails_on_multiple_matches() {
    let states = [worker(1, "exec_abc", "Riker"), worker(2, "exec_def", "Data")];
    let err = pick_unique("x", vec![&states[0], &states[1]], &states).expect_err("two matches");
    let msg = err.to_string();
    assert!(msg.contains("`x` matches multiple live workers"), "message was: {msg}");
    assert!(msg.contains("slot 1 (Riker) run exec_abc"), "message was: {msg}");
    assert!(msg.contains("slot 2 (Data) run exec_def"), "message was: {msg}");
}

// ---- live_candidates_summary ------------------------------------------

#[test]
fn summary_reports_no_live_workers_when_empty() {
    assert_eq!(live_candidates_summary(&[]), "no live workers");
}

#[test]
fn summary_lists_workers_sorted_by_slot_id() {
    // Deliberately out of slot order on input to prove the sort.
    let states = [worker(2, "exec_def", "Data"), worker(1, "exec_abc", "Riker")];
    assert_eq!(live_candidates_summary(&states), "Live: slot 1 (Riker), slot 2 (Data)");
}

// ---- resolve_hosted_pane_ref / no_worker_matches_error -----------------

fn hosted_pane(slot_id: u8, run_id: &str, crew_name: &str, state: HostedPaneState) -> HostedPaneStatus {
    HostedPaneStatus {
        slot_id,
        run_id: run_id.to_owned(),
        crew_name: crew_name.to_owned(),
        summary: None,
        task_title: None,
        state,
    }
}

/// The exact incident shape: a crew name the operator can see in the
/// app, backing a run the live registry has dropped entirely, must
/// still resolve — by name, by slot, and by run id.
#[test]
fn resolves_terminal_entry_live_process_pane_by_name_slot_and_run_id() {
    let panes = [hosted_pane(
        1,
        "exec_riker",
        "Riker",
        HostedPaneState::LiveProcessNoRegistry {
            evidence: "durably-recorded shell pid 33112 is alive".to_owned(),
        },
    )];
    let by_name = resolve_hosted_pane_ref("riker", &panes)
        .expect("no ambiguity")
        .expect("name should resolve");
    assert_eq!(by_name.run_id, "exec_riker");
    let by_slot = resolve_hosted_pane_ref("1", &panes)
        .expect("no ambiguity")
        .expect("slot should resolve");
    assert_eq!(by_slot.crew_name, "Riker");
    let by_run = resolve_hosted_pane_ref("exec_riker", &panes)
        .expect("no ambiguity")
        .expect("run id should resolve");
    assert_eq!(by_run.slot_id, 1);
}

#[test]
fn durable_occupant_renderings_name_agents_stop_and_omit_retire_pane() {
    let pane = hosted_pane(
        1,
        "exec_riker",
        "Riker",
        HostedPaneState::DurableOccupantNoRegistry {
            status: "running".to_owned(),
        },
    );
    for out in [
        durable_occupant_list_line(&pane, "running"),
        durable_occupant_detail_lines(&pane, "running"),
    ] {
        assert!(out.contains("agents stop exec_riker"), "output was: {out}");
        assert!(!out.contains("retire-pane"), "output was: {out}");
    }
}

#[test]
fn resolve_hosted_pane_ref_misses_return_none_not_error() {
    let panes = [hosted_pane(1, "exec_riker", "Riker", HostedPaneState::Husk)];
    assert!(
        resolve_hosted_pane_ref("nonesuch", &panes)
            .expect("a miss is not an error")
            .is_none()
    );
}

#[test]
fn resolve_hosted_pane_ref_bails_on_ambiguous_name() {
    let panes = [
        hosted_pane(1, "exec_a", "Data", HostedPaneState::Husk),
        hosted_pane(2, "exec_b", "Data", HostedPaneState::Husk),
    ];
    let err = resolve_hosted_pane_ref("data", &panes).expect_err("two panes share a name");
    let msg = err.to_string();
    assert!(msg.contains("matches multiple tracked panes"), "message was: {msg}");
}

#[test]
fn resolve_hosted_pane_ref_prefers_live_pane_over_husk_with_reused_name() {
    let panes = [
        hosted_pane(1, "exec_old", "Data", HostedPaneState::Husk),
        hosted_pane(2, "exec_new", "Data", HostedPaneState::Live),
    ];
    let pane = resolve_hosted_pane_ref("data", &panes)
        .expect("a live pane disambiguates")
        .expect("name matches");
    assert_eq!(pane.run_id, "exec_new");
}

/// Requirement: a total miss must list what was searched, including
/// the non-live candidates — not only the live crew.
#[test]
fn no_worker_matches_error_lists_non_live_candidates_too() {
    let states = [worker(2, "exec_data", "Data")];
    let hosted = [
        hosted_pane(2, "exec_data", "Data", HostedPaneState::Live),
        hosted_pane(
            1,
            "exec_riker",
            "Riker",
            HostedPaneState::LiveProcessNoRegistry {
                evidence: "durably-recorded shell pid 33112 is alive".to_owned(),
            },
        ),
        hosted_pane(3, "exec_worf", "Worf", HostedPaneState::Husk),
    ];
    let err = no_worker_matches_error("nonesuch", &states, &hosted);
    let msg = err.to_string();
    assert!(msg.contains("Live: slot 2 (Data)"), "message was: {msg}");
    assert!(msg.contains("Also tracked (not live)"), "message was: {msg}");
    assert!(
        msg.contains("slot 1 (Riker) run exec_riker [terminal entry, live process]"),
        "message was: {msg}"
    );
    assert!(msg.contains("slot 3 (Worf) run exec_worf [husk]"), "message was: {msg}");
    // The Live-classified pane is not double-listed in the "not live" tail.
    assert!(!msg.contains("slot 2 (Data) run exec_data ["), "message was: {msg}");
}

#[test]
fn no_worker_matches_error_omits_tail_when_nothing_non_live() {
    let err = no_worker_matches_error("nonesuch", &[], &[]);
    assert_eq!(err.to_string(), "no worker matches `nonesuch`. no live workers");
}

// ---- looks_like_name_or_slot ------------------------------------------

#[test]
fn numeric_slot_looks_like_name_or_slot() {
    assert!(looks_like_name_or_slot("5"));
    assert!(looks_like_name_or_slot("0"));
    assert!(looks_like_name_or_slot("300"));
}

#[test]
fn roster_name_looks_like_name_or_slot_case_insensitive() {
    assert!(looks_like_name_or_slot("Riker"));
    assert!(looks_like_name_or_slot("riker"));
    assert!(looks_like_name_or_slot("LA FORGE"));
}

#[test]
fn non_slot_references_do_not_look_like_name_or_slot() {
    assert!(!looks_like_name_or_slot("exec_18ad9f"));
    assert!(!looks_like_name_or_slot("Picard"));
}

// ---- work_item_fallback_eligible --------------------------------------

#[test]
fn work_item_fallback_excludes_slot_shaped_references_and_crew_names() {
    assert!(!work_item_fallback_eligible("1"));
    assert!(!work_item_fallback_eligible("300"));
    assert!(!work_item_fallback_eligible("riker"));
}

#[test]
fn work_item_fallback_allows_work_item_and_execution_references() {
    assert!(work_item_fallback_eligible("exec_18ad9f"));
    assert!(work_item_fallback_eligible("T1"));
    assert!(work_item_fallback_eligible("task_123"));
}

// ---- WorkItem::primary_id ---------------------------------------------

#[test]
fn primary_id_for_each_work_item_variant() {
    assert_eq!(WorkItem::Product(product("prod_9")).primary_id(), "prod_9");
    assert_eq!(WorkItem::Project(project("proj_9")).primary_id(), "proj_9");
    assert_eq!(WorkItem::Task(task("task_9", TaskKind::Task)).primary_id(), "task_9");
    assert_eq!(
        WorkItem::Chore(task("chore_9", TaskKind::Chore)).primary_id(),
        "chore_9"
    );
}

// ---- parse_transcript_tail_events --------------------------------------

use boss_engine::transcript_markdown::TranscriptEventKind;

fn assistant_texts(events: &[boss_engine::transcript_markdown::TranscriptEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            TranscriptEventKind::AssistantText(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn parse_transcript_tail_events_reads_claude_dialect_without_a_driver() {
    let jsonl = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
    let events = parse_transcript_tail_events(jsonl, None, "/tmp/t.jsonl").expect("claude dialect must parse");
    assert_eq!(assistant_texts(&events), vec!["hi".to_owned()]);
}

#[test]
fn parse_transcript_tail_events_falls_back_to_the_grok_driver_for_acp_records() {
    // Grok's raw ACP `session/update` envelope carries no top-level
    // `type` field, so the direct schema check must fail before the
    // driver fallback kicks in — this is the exact shape
    // `bossctl agents transcript` received for a Grok run and rendered
    // nothing for.
    let jsonl = concat!(
        r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
        r#""sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"[blocked] reason=\"needs a decision\""}}}}"#,
    );
    assert!(
        boss_engine::transcript_markdown::parse_transcript_checked(jsonl).is_err(),
        "raw ACP content must not match the Claude/Codex schema directly"
    );
    let events =
        parse_transcript_tail_events(jsonl, Some("grok"), "/tmp/t.jsonl").expect("grok driver must normalize");
    assert!(
        assistant_texts(&events)
            .iter()
            .any(|text| text.contains("[blocked] reason=\"needs a decision\"")),
        "grok ACP prose must survive the driver-aware fallback parse; got {events:?}",
    );
}

#[test]
fn parse_transcript_tail_events_errors_when_driver_resolves_but_cannot_normalize_content() {
    // A resolvable driver slug that still can't make sense of the tail
    // content (e.g. a Grok tail window landing entirely on a
    // `session_update` variant `parse_acp_envelope` maps to `Unknown`,
    // or any other non-empty content the driver silently drops) must
    // not swallow the original schema error into an empty transcript —
    // the doc comment on `parse_transcript_tail_events` promises the
    // opposite, and this is the exact "renders nothing" symptom this
    // module set out to fix.
    let jsonl = concat!(
        r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
        r#""sessionUpdate":"totally_unknown_variant","foo":"bar"}}}"#,
    );
    assert!(
        boss_engine::transcript_markdown::parse_transcript_checked(jsonl).is_err(),
        "raw ACP content must not match the Claude/Codex schema directly"
    );
    let err = parse_transcript_tail_events(jsonl, Some("grok"), "/tmp/t.jsonl")
        .expect_err("a resolvable driver that normalizes to nothing must still surface the schema error");
    assert!(err.to_string().contains("rendering transcript"), "got: {err}");
}

#[test]
fn parse_transcript_tail_events_errors_when_no_driver_can_normalize_unknown_content() {
    let jsonl = r#"{"totally":"unrecognised"}"#;
    let err = parse_transcript_tail_events(jsonl, None, "/tmp/t.jsonl")
        .expect_err("unrecognised content with no driver to fall back on must still error");
    assert!(err.to_string().contains("rendering transcript"));
}

#[test]
fn parse_transcript_tail_events_grok_joins_tool_call_and_surfaces_hook_events() {
    // A `tool_call` / `tool_call_update` pair only joins into one
    // `ToolUse` (no spurious filler for the update line) because
    // `parse_transcript_with_driver` builds a stateful
    // `GrokTranscriptSession` for the whole tail — an isolated
    // per-line normalize (a regression back to
    // `normalize_transcript_entry`) would instead see the update in
    // isolation, fail the `toolCallId` correlation, and emit an
    // extra `{"type":"system"}` filler event with no subtype. A
    // `hook_execution` record must also surface as a system event,
    // per the brief's requirement that hook events render alongside
    // assistant text and tool calls/results.
    let jsonl = concat!(
        r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
        r#""sessionUpdate":"tool_call","toolCallId":"call-1","title":"run_terminal_command","rawInput":{"command":"echo hi"}}}}"#,
        "\n",
        r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
        r#""sessionUpdate":"tool_call_update","toolCallId":"call-1","rawOutput":{"exit_code":0,"output":"hi"}}}}"#,
        "\n",
        r#"{"method":"session/update","params":{"sessionId":"s1","update":{"#,
        r#""sessionUpdate":"hook_execution","event_name":"Stop"}}}"#,
    );
    let events = parse_transcript_tail_events(jsonl, Some("grok"), "/tmp/t.jsonl").expect("grok driver must normalize");

    let tool_uses: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            TranscriptEventKind::ToolUse { name, input } => Some((name.as_str(), input)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_uses.len(),
        1,
        "expected exactly one joined tool call; got {events:?}"
    );
    assert_eq!(tool_uses[0].0, "Bash");
    assert_eq!(tool_uses[0].1, &serde_json::json!({"command": "echo hi"}));

    let system_events: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            TranscriptEventKind::System { subtype, .. } => Some(subtype.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        system_events,
        vec![Some("hook_execution".to_owned())],
        "the tool_call_update must join silently (no unmatched filler) and the hook \
         execution must be the only system event; got {events:?}",
    );

    let render_opts = boss_engine::transcript_markdown::RenderOpts {
        hide_tools: true,
        ..Default::default()
    };
    let segments = boss_engine::transcript_markdown::events_to_segments(&events, &render_opts);
    assert!(
        segments
            .iter()
            .all(|segment| segment.role != boss_engine::transcript_markdown::SegmentRole::Tool),
        "--no-tools must drop the Grok tool segment exactly as it does for Claude; got {segments:?}",
    );
}

#[test]
fn parse_transcript_tail_events_errors_when_driver_slug_is_unknown() {
    let jsonl = r#"{"totally":"unrecognised"}"#;
    let err = parse_transcript_tail_events(jsonl, Some("not-a-real-driver"), "/tmp/t.jsonl")
        .expect_err("an unresolvable driver slug must not swallow the schema error");
    assert!(err.to_string().contains("rendering transcript"));
}
