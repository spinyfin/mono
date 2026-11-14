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
question = b'Do you want to proceed?'
dialog = question + b'\n1. Yes\n2. No\nEsc to cancel'
bordered = (
    '╭───────────────────────────────────────────────────╮\n'
    '│ Do you want to proceed?                           │\n'
    '│ ❯ 1. Yes                                          │\n'
    '│   2. No                                           │\n'
    '│ Esc to cancel                                     │\n'
    '╰───────────────────────────────────────────────────╯\n'
    '  bypass permissions on'
).encode()
bordered_trust = (
    '╭───────────────────────────────────────────────────╮\n'
    '│ Quick safety check: Is this a project you created │\n'
    '│ ❯ No, exit                                        │\n'
    '│   Yes, I trust this folder                        │\n'
    '│ Enter to confirm · Esc to cancel                  │\n'
    '╰───────────────────────────────────────────────────╯\n'
).encode()
for screen, marker in [
    (dialog, question),
    (bordered, question),
    (bordered_trust, b'Yes, I trust this folder'),
    ('❯ No, exit\nYes, I trust this folder\nEnter to confirm · Esc to cancel'.encode(), b'Yes, I trust this folder'),
    ('❯ No, exit\nYes, I accept\nEnter to confirm · Esc to cancel'.encode(), b'Yes, I accept'),
]:
    try: check(View(screen), [marker])
    except error: pass
    else: raise AssertionError('active dialog accepted')
check(View(question), [question])
check(View(dialog + b'\nClaude Code\n> status?'), [question])
check(View("❯ ".encode() + dialog), [question], dialog)
"#,
        )
        .arg(temp.path().join("initial-prompt.py"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
