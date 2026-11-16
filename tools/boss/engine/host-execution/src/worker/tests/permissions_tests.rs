use super::*;
use boss_engine_driver::agy::{
    AgyRuntimeState,
    permissions::{SettingsPlan, render_settings},
};

fn agy_state(root: &std::path::Path) -> (AgyRuntimeState, String) {
    let state = AgyRuntimeState {
        process_home: root.join("process-home"),
        control_dir: root.join("control"),
        session_settings_path: root.join("settings.json"),
        log_file: root.join("agy.log"),
        workspace_path: root.join("workspace"),
        agy_binary: "/nonexistent/agy".into(),
    };
    let settings = render_settings(&SettingsPlan {
        worker_kind: boss_engine_driver::WorkerKind::Standard,
        workspace: &state.workspace_path,
        boss_data_dir: None,
        hooks_dir: &state.process_home.join(".gemini/config"),
        control_dir: &state.control_dir,
        rules_root: &root.join("rules-root"),
    })
    .unwrap();
    let body = settings.to_json().unwrap();
    let value: Value = serde_json::from_str(&body).unwrap();
    let deny = value["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| rule.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    std::fs::write(&state.session_settings_path, &body).unwrap();
    let matching = format!(
        "CLI settings initialized: permissions=&{{Allow:[] Deny:[{deny}] Ask:[]}}, toolPermission=request-review"
    );
    (state, matching)
}

#[tokio::test]
async fn agy_rechecks_settings_before_input_and_kills_on_invalid_rules() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let supervisor = supervisor(root.path(), fake.clone()).await;
    seed(&supervisor.store, Phase::Alive);
    let (state, matching) = agy_state(root.path());
    update(&supervisor.store, |record| {
        record.driver_id = "agy".into();
        record.runtime_state = Some(state.to_driver_runtime_state());
    });
    assert!(!permissions::check(&stored(&supervisor.store)).unwrap());
    std::fs::write(&state.log_file, matching).unwrap();
    let result = supervisor
        .execute(&deliver_command("valid", DeliveryKind::Answer, "hello"), false, 1)
        .await;
    assert_eq!(result.code, ResultCode::Ok as i32, "{}", result.reason);
    fake.sent.lock().unwrap().clear();
    std::fs::write(&state.log_file, "ignoring invalid deny rule").unwrap();
    let record = stored(&supervisor.store);
    assert!(supervisor.wait_for_brief(&record).await.is_err());
    let result = supervisor
        .execute(&deliver_command("invalid", DeliveryKind::Answer, "secret"), false, 2)
        .await;
    assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
    assert!(result.reason.contains("dropped rendered permission rules"));
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(!fake.sessions.lock().unwrap().contains_key(&session_name("token")));
    assert_eq!(stored(&supervisor.store).phase, Phase::Terminal);
}

#[tokio::test]
async fn wait_for_brief_kills_when_delivered_without_settings_initialization() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let supervisor = supervisor(root.path(), fake.clone()).await;
    seed(&supervisor.store, Phase::Alive);
    let (state, _matching) = agy_state(root.path());
    let home = run_home_delivered("settings-no-init");
    update(&supervisor.store, |record| {
        record.driver_id = "agy".into();
        record.runtime_state = Some(state.to_driver_runtime_state());
        record.home = Some(home);
    });
    let pane = supervisor.wait_for_brief(&stored(&supervisor.store)).await.unwrap();
    assert_eq!(pane_life(&pane), PaneLife::BriefNotDelivered);
    assert!(!fake.sessions.lock().unwrap().contains_key(&session_name("token")));
}

#[tokio::test]
async fn wait_for_brief_fails_closed_on_a_mismatched_settings_line() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeTmux::default());
    live(&fake);
    let supervisor = supervisor(root.path(), fake.clone()).await;
    seed(&supervisor.store, Phase::Alive);
    let (state, _matching) = agy_state(root.path());
    std::fs::write(
        &state.log_file,
        "CLI settings initialized: permissions=&{Allow:[] Deny:[] Ask:[]}, toolPermission=always-proceed",
    )
    .unwrap();
    update(&supervisor.store, |record| {
        record.driver_id = "agy".into();
        record.runtime_state = Some(state.to_driver_runtime_state());
        record.home = Some(run_home_delivered("settings-mismatch"));
    });
    let record = stored(&supervisor.store);
    assert!(supervisor.wait_for_brief(&record).await.is_err());
    let result = supervisor
        .execute(&deliver_command("mismatch", DeliveryKind::Answer, "secret"), false, 1)
        .await;
    assert_eq!(result.code, ResultCode::Rejected as i32, "{}", result.reason);
    assert!(
        result.reason.contains("differ from the rendered posture"),
        "{}",
        result.reason
    );
    assert!(fake.sent.lock().unwrap().is_empty());
}
