//! Exercise the actual PTY admission path without launching an authenticated CLI.

use super::*;
use crate::initial_prompt::{self, LAUNCHER};

#[test]
fn relay_checks_settings_before_brief_and_rechecks_after_initialization() {
    let root = tempfile::tempdir().unwrap();
    let settings = render_settings(&SettingsPlan {
        worker_kind: WorkerKind::Standard,
        workspace: Path::new("/workspace"),
        boss_data_dir: None,
        hooks_dir: Path::new("/private/hooks"),
        control_dir: Path::new("/private/control"),
    })
    .unwrap();
    let expected = expected_initialized_fragment(&settings);
    fs::write(root.path().join("initial-prompt.py"), LAUNCHER).unwrap();
    initial_prompt::write_private(&root.path().join("initial-prompt.txt"), b"private sentinel").unwrap();
    fs::write(
        root.path().join("initial-prompt.json"),
        initial_prompt::launcher_config(&crate::agy::AgyDriver::default()).unwrap(),
    )
    .unwrap();
    fs::write(
        root.path().join("settings-log-check.json"),
        serde_json::to_string(&serde_json::json!({
            "log_path": root.path().join("agy.log"), "expected": expected,
        }))
        .unwrap(),
    )
    .unwrap();
    let code = r#"
import os, pathlib, select, sys, time, tty
root = pathlib.Path(__file__).parent
mode = sys.argv[1]
tty.setraw(0)
os.write(1, b'\x1b[?2004h? for shortcuts')
# A ready composer alone must not admit the brief.
assert not select.select([0], [], [], 0.35)[0], 'brief before initialization'
line = 'CLI settings initialized: ' + sys.argv[2]
if mode == 'mismatch':
    line = 'CLI settings initialized: permissions=wrong'
if mode == 'invalid':
    line += '\nignoring invalid deny rule'
(root / 'agy.log').write_text(line)
if mode == 'late-invalid':
    # Starting UI prevents admission while a later grant-store warning arrives.
    os.write(1, b'\x1b[2JSigning in...')
    time.sleep(0.3)
    with (root / 'agy.log').open('a') as log:
        log.write('\nignoring invalid deny rule')
    os.write(1, b'\x1b[2J? for shortcuts')
if mode not in ('valid', 'after-delivery-invalid'):
    assert not select.select([0], [], [], 1)[0], 'brief despite invalid settings'
    raise SystemExit(7)
data = b''
while not data.endswith(b'\x1b[201~'):
    data += os.read(0, 4096)
assert data == b'\x1b[200~private sentinel\x1b[201~', data
os.write(1, b'\r\nprivate sentinel')
assert os.read(0, 1) == b'\r'
if mode == 'after-delivery-invalid':
    with (root / 'agy.log').open('a') as log:
        log.write('\nignoring invalid deny rule')
    time.sleep(2)
    raise AssertionError('relay did not terminate the invalid run')
"#;
    fs::write(root.path().join("stub.py"), code).unwrap();
    for mode in ["valid", "mismatch", "invalid", "late-invalid", "after-delivery-invalid"] {
        for name in [
            "agy.log",
            initial_prompt::DELIVERED_MARKER,
            initial_prompt::UNDELIVERED_MARKER,
        ] {
            let _ = fs::remove_file(root.path().join(name));
        }
        let output = boss_command_runner::output_blocking_timeout(
            std::process::Command::new("python3")
                .arg(root.path().join("initial-prompt.py"))
                .arg("python3")
                .arg(root.path().join("stub.py"))
                .arg(mode)
                .arg(&expected)
                .stdin(std::process::Stdio::null()),
            std::time::Duration::from_secs(15),
        )
        .unwrap();
        if mode == "valid" {
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(root.path().join(initial_prompt::DELIVERED_MARKER).exists());
        } else if mode == "after-delivery-invalid" {
            assert_eq!(output.status.code(), Some(1));
            assert!(root.path().join(initial_prompt::DELIVERED_MARKER).exists());
            assert!(String::from_utf8_lossy(&output.stderr).contains("dropped rendered permission rules"));
        } else {
            assert_eq!(output.status.code(), Some(initial_prompt::BRIEF_NOT_DELIVERED_EXIT));
            assert!(!root.path().join(initial_prompt::DELIVERED_MARKER).exists());
            assert!(root.path().join(initial_prompt::UNDELIVERED_MARKER).exists());
        }
    }
}
