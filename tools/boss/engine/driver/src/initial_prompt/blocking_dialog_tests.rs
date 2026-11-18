use super::*;

#[test]
fn blocking_dialog_refuses_brief_and_submit_without_answering() {
    for after_paste in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
        let stub = temp.path().join("driver.py");
        fs::write(
            &stub,
            format!(
                r#"
import os, select, sys, tty
from pathlib import Path
tty.setraw(0)
if {after_paste}:
    os.write(1, b'\x1b[?2004hClaude Code')
    data = b''
    while not data.endswith(b'\x1b[201~'):
        data += os.read(0, 4096)
os.write(1, b'\r\nDo you want to proceed?\r\n1. Yes\r\n2. No\r\nEsc to cancel')
while True:
    data = os.read(0, 4096)
    if data:
        Path(sys.argv[1]).write_bytes(data)
"#,
                after_paste = if after_paste { "True" } else { "False" }
            ),
        )
        .unwrap();
        let received = temp.path().join("received");
        let output = boss_command_runner::output_blocking_timeout(
            std::process::Command::new("python3")
                .arg(temp.path().join("initial-prompt.py"))
                .arg("python3")
                .arg(stub)
                .arg(&received)
                .stdin(std::process::Stdio::null()),
            Duration::from_secs(15),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(BRIEF_NOT_DELIVERED_EXIT), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("BlockingDialogOnScreen"), "{stderr}");
        assert!(stderr.contains("Do you want to proceed?"), "{stderr}");
        assert!(!received.exists(), "dialog received input: {after_paste}");
        assert!(!delivered_marker_present(temp.path()));
        assert_eq!(
            blocking_dialog_marker(temp.path()).as_deref(),
            Some("Do you want to proceed?")
        );
    }
}

#[test]
fn every_claude_marker_is_exported_to_the_launcher() {
    use crate::AgentDriver;
    let markers = crate::ClaudeDriver.pane_monitor_spec().unwrap().blocking_dialog_markers;
    assert!(!markers.is_empty());
    let config: serde_json::Value = serde_json::from_str(&launcher_config(&crate::ClaudeDriver).unwrap()).unwrap();
    assert_eq!(config["blocking_dialog_markers"], serde_json::json!(markers));
}

#[test]
fn launcher_distinguishes_active_dialogs_from_payload_and_transcript() {
    let temp = tempfile::tempdir().unwrap();
    provision(temp.path(), temp.path(), "brief", &crate::ClaudeDriver).unwrap();
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(
            r#"
import runpy, sys
module = runpy.run_path(sys.argv[1], run_name='dialog_check')
check, error = module['check_blocking_dialog'], module['BlockingDialogOnScreen']
class View:
    def __init__(self, text): self.screen = text
    def text(self): return self.screen
import json
for case in json.loads(sys.argv[2]):
    actual = None
    try:
        check(View(case['screen'].encode()), [m.encode() for m in case['markers']], case['payload'].encode())
    except error as refusal:
        actual = refusal.marker
    assert actual == case['expected_marker'], (case['name'], actual, case['expected_marker'])
"#,
        )
        .arg(temp.path().join("initial-prompt.py"))
        .arg(include_str!("../../../../tmux/blocking_dialog_cases.json"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn launcher_refuses_an_antigravity_approval_dialog() {
    use crate::AgentDriver;
    let temp = tempfile::tempdir().unwrap();
    provision(
        temp.path(),
        temp.path(),
        "private sentinel",
        &crate::AgyDriver::default(),
    )
    .unwrap();
    // The launcher also gates on the CLI's startup log, which never appears
    // here: the dialog must be refused while the brief is still held back.
    fs::write(
        temp.path().join("settings-log-check.json"),
        serde_json::json!({"log_path": temp.path().join("agy.log"), "expected": "x"}).to_string(),
    )
    .unwrap();
    let stub = temp.path().join("driver.py");
    // The selected choice is marked `>`, and the footer shares a line with the model name.
    fs::write(
        &stub,
        r#"
import os, sys, tty
from pathlib import Path
tty.setraw(0)
os.write(1, b'\r\nRequesting permission for:\r\n   printf X > f.txt\r\nRun this command?\r\n> 1. Yes, run command\r\n  2. Yes, and always allow in this conversation\r\n  3. Yes, and always allow (Persist to settings.json)\r\n  4. No, cancel\r\nesc to cancel      Gemini 3.8 Flash \xc2\xb7 low')
while True:
    data = os.read(0, 4096)
    if data:
        Path(sys.argv[1]).write_bytes(data)
"#,
    )
    .unwrap();
    let received = temp.path().join("received");
    let output = boss_command_runner::output_blocking_timeout(
        std::process::Command::new("python3")
            .arg(temp.path().join("initial-prompt.py"))
            .arg("python3")
            .arg(stub)
            .arg(&received)
            .stdin(std::process::Stdio::null()),
        Duration::from_secs(15),
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(BRIEF_NOT_DELIVERED_EXIT), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("BlockingDialogOnScreen"),
        "{output:?}"
    );
    assert!(!received.exists(), "the dialog received input");
    assert_eq!(
        blocking_dialog_marker(temp.path()).as_deref(),
        Some("Run this command?")
    );
    assert!(
        crate::AgyDriver::default()
            .pane_monitor_spec()
            .unwrap()
            .blocking_dialog_markers
            .iter()
            .any(|marker| marker == "Run this command?")
    );
}

#[test]
fn launcher_fails_the_spawn_when_the_cli_reports_a_dropped_brief() {
    use crate::AgentDriver;
    // The vendor error discards the brief without any hook, so the launcher is
    // the only place that can see it.
    let markers = crate::AgyDriver::default().post_submit_failure_markers();
    assert_eq!(markers, ["Eligibility check failed"]);
    let temp = tempfile::tempdir().unwrap();
    provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
    let config_path = temp.path().join("initial-prompt.json");
    let mut config: serde_json::Value = serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config["post_submit_failure_markers"] = serde_json::json!(markers);
    fs::write(&config_path, config.to_string()).unwrap();
    let stub = temp.path().join("driver.py");
    fs::write(
        &stub,
        r#"
import os, tty
tty.setraw(0)
os.write(1, b'\x1b[?2004hClaude Code')
data = b''
while not data.endswith(b'\x1b[201~'):
    data += os.read(0, 4096)
os.write(1, b'\r\n[Pasted text #1 +1 lines]')
while not data.endswith(b'\r'):
    data += os.read(0, 4096)
os.write(1, b'\r\nEligibility check failed: failed to fetch user info: 500 Internal Server Error\r\n')
import time
time.sleep(30)
"#,
    )
    .unwrap();
    let output = boss_command_runner::output_blocking_timeout(
        std::process::Command::new("python3")
            .arg(temp.path().join("initial-prompt.py"))
            .arg("python3")
            .arg(stub)
            .stdin(std::process::Stdio::null()),
        Duration::from_secs(20),
    )
    .unwrap();
    // Delivered (the brief was submitted), then failed: not the undelivered code.
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(delivered_marker_present(temp.path()));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("submitted brief was dropped"), "{stderr}");
    let log = fs::read_to_string(temp.path().join("initial-prompt.delivery.jsonl")).unwrap();
    assert!(log.contains("post_submit_failure"), "{log}");
}
