use super::super::home::{AGY_HOMES_ROOT_ENV, control_dir_for_run};
use super::super::test_environment::EnvGuard;
use super::*;
use crate::{AgentDriver, AgyDriver};

const PARENT: &str = "9476903e-615e-4a03-b5b5-ea82733cc97b";
const CHILD: &str = "30dedab2-dc3a-4990-8b2c-53e481e66031";

fn line(id: &str) -> String {
    format!("I1111 04:17:39.400 conversation_manager.go:123] Cancelling in-progress response for conversation {id}")
}

#[test]
fn matches_only_cancellation_log_for_exact_parent() {
    let driver = AgyDriver::default();
    let snapshot = InterruptRecoverySnapshot {
        events_path: "/unused/agy.log".into(),
        offset: 0,
        session_id: PARENT.into(),
        settle_window: SETTLE_WINDOW,
    };
    assert!(driver.is_interrupt_recovery_turn_end(&serde_json::json!(line(PARENT))));
    assert!(driver.is_interrupt_recovery_line(&line(PARENT), &snapshot));
    for file in ["server.go", "internal/response.go"] {
        let alternate = line(PARENT).replace("conversation_manager.go", file);
        assert!(driver.is_interrupt_recovery_line(&alternate, &snapshot));
    }
    for noise in [
        line(PARENT).replace(".go:123]", ".rs:123]"),
        line(PARENT).replace(":123]", ":abc]"),
        line(PARENT).replace(":123]", ":]"),
        line(PARENT).replace(":123]", ":123"),
        line(PARENT).replace("] Cancelling", "] quoted Cancelling"),
        line(CHILD),
        line(&format!("{PARENT}-suffix")),
        format!("server.go:10] Cancelling conversation {PARENT} (killBackgroundTasks=false)"),
        "⎿  Interrupted · What should Antigravity CLI do instead?".into(),
        format!("Cancelling in-progress response for conversation {PARENT}"),
        r#"{"type":"PLANNER_RESPONSE","status":"DONE"}"#.into(),
        line(""),
    ] {
        assert!(!driver.is_interrupt_recovery_line(&noise, &snapshot), "{noise}");
    }
}

#[test]
fn snapshot_requires_claim_and_private_run_then_skips_existing_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[(AGY_HOMES_ROOT_ENV, tmp.path().to_str().unwrap())]);
    let driver = AgyDriver::default();
    assert!(
        driver
            .prepare_interrupt_recovery("unprovisioned", Some(PARENT))
            .is_none()
    );
    let control = control_dir_for_run("run-one").unwrap();
    std::fs::create_dir_all(&control).unwrap();
    assert!(driver.prepare_interrupt_recovery("run-one", None).is_none());
    assert!(driver.prepare_interrupt_recovery("run-one", Some("")).is_none());
    let initial = driver.prepare_interrupt_recovery("run-one", Some(PARENT)).unwrap();
    assert_eq!(initial.offset, 0);
    assert_eq!(initial.events_path, control.join("agy.log"));
    assert_eq!(initial.session_id, PARENT);
    let old = format!("{}\n", line(PARENT));
    std::fs::write(&initial.events_path, &old).unwrap();
    let snapshot = driver.prepare_interrupt_recovery("run-one", Some(PARENT)).unwrap();
    assert_eq!(snapshot.offset, old.len() as u64);
    assert_eq!(snapshot.settle_window, SETTLE_WINDOW);
    std::fs::create_dir_all(control_dir_for_run("run-two").unwrap()).unwrap();
    let other = driver.prepare_interrupt_recovery("run-two", Some(CHILD)).unwrap();
    assert_ne!(snapshot.events_path, other.events_path);
    assert_eq!(other.offset, 0);
    assert_eq!(other.session_id, CHILD);
}
