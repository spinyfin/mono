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
        rules_root: Path::new("/rules-root"),
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
if mode == 'split-write':
    # A poll landing mid-line must not judge the incomplete initialization line.
    half = len(line) // 2
    (root / 'agy.log').write_text(line[:half])
    time.sleep(0.7)
    with (root / 'agy.log').open('a') as log:
        log.write(line[half:] + '\n')
else:
    (root / 'agy.log').write_text(line + '\n')
if mode == 'late-invalid':
    # Starting UI prevents admission while a later grant-store warning arrives.
    os.write(1, b'\x1b[2JSigning in...')
    time.sleep(0.3)
    with (root / 'agy.log').open('a') as log:
        log.write('\nignoring invalid deny rule')
    os.write(1, b'\x1b[2J? for shortcuts')
if mode == 'grant-store-delay':
    time.sleep(1)
    with (root / 'agy.log').open('a') as log:
        log.write('\nignoring invalid deny rule')
if mode not in ('valid', 'split-write', 'after-delivery-invalid', 'after-delivery-missing'):
    assert not select.select([0], [], [], 1.5)[0], 'brief despite invalid settings'
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
if mode == 'after-delivery-missing':
    (root / 'agy.log').unlink()
    time.sleep(2)
    raise AssertionError('relay did not terminate after the log disappeared')
"#;
    fs::write(root.path().join("stub.py"), code).unwrap();
    for mode in [
        "valid",
        "split-write",
        "mismatch",
        "invalid",
        "late-invalid",
        "grant-store-delay",
        "after-delivery-invalid",
        "after-delivery-missing",
    ] {
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
        if mode == "valid" || mode == "split-write" {
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(root.path().join(initial_prompt::DELIVERED_MARKER).exists());
        } else if mode == "after-delivery-invalid" {
            assert_eq!(output.status.code(), Some(1));
            assert!(root.path().join(initial_prompt::DELIVERED_MARKER).exists());
            assert!(String::from_utf8_lossy(&output.stderr).contains("dropped rendered permission rules"));
        } else if mode == "after-delivery-missing" {
            assert_eq!(output.status.code(), Some(1));
            assert!(root.path().join(initial_prompt::DELIVERED_MARKER).exists());
            assert!(String::from_utf8_lossy(&output.stderr).contains("disappeared"));
        } else {
            assert_eq!(output.status.code(), Some(initial_prompt::BRIEF_NOT_DELIVERED_EXIT));
            assert!(!root.path().join(initial_prompt::DELIVERED_MARKER).exists());
            assert!(root.path().join(initial_prompt::UNDELIVERED_MARKER).exists());
        }
    }
}

#[test]
fn relay_forwards_terminal_query_replies_before_settings_initialize() {
    let root = tempfile::tempdir().unwrap();
    let settings = render_settings(&SettingsPlan {
        worker_kind: WorkerKind::Standard,
        workspace: Path::new("/workspace"),
        boss_data_dir: None,
        hooks_dir: Path::new("/private/hooks"),
        control_dir: Path::new("/private/control"),
        rules_root: Path::new("/rules-root"),
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
    let harness = root.path().join("outer-pty.py");
    fs::write(
        &harness,
        r#"
import importlib.util, os, pty, select, signal, subprocess, sys, time
from pathlib import Path
root = Path(sys.argv[1])
expected = sys.argv[2]
spec = importlib.util.spec_from_file_location("launcher", root / "initial-prompt.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
assert launcher.take_terminal_replies(b"\x1b[24;80R", b"") == (b"\x1b[24;80R", b"", False)
assert launcher.take_terminal_replies(b"hello", b"")[2]
assert launcher.take_terminal_replies(b"\x1b[1;", b"") == (b"", b"\x1b[1;", False)
(root / "stub.py").write_text('''
import os, pathlib, select, sys, time, tty
root = pathlib.Path(__file__).parent
tty.setraw(0)
os.write(1, b"\\x1b[6n")
response = b""
while not response.endswith(b"R"):
    response += os.read(0, 4096)
assert response == b"\\x1b[24;10R", response
os.write(1, b"QUERY_OK")
assert not select.select([0], [], [], 0.3)[0], "brief before initialization"
(root / "agy.log").write_text("CLI settings initialized: " + sys.argv[1] + "\\n")
os.write(1, b"\\x1b[?2004h? for shortcuts")
data = b""
while not data.endswith(b"\\x1b[201~"):
    data += os.read(0, 4096)
assert data == b"\\x1b[200~private sentinel\\x1b[201~", data
os.write(1, b"\\r\\nprivate sentinel")
assert os.read(0, 1) == b"\\r"
os.write(1, b"FIRST_DELIVERED")
''')
master, slave = pty.openpty()
proc = subprocess.Popen(
    [sys.executable, str(root / "initial-prompt.py"), sys.executable, str(root / "stub.py"), expected],
    stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
    preexec_fn=lambda: signal.signal(signal.SIGHUP, signal.SIG_IGN),
)
os.close(slave)
captured = b""
def wait_for(marker):
    global captured
    deadline = time.monotonic() + 12
    while marker not in captured:
        if time.monotonic() >= deadline:
            raise AssertionError((marker, captured, proc.poll()))
        readable, _, _ = select.select([master], [], [], 0.1)
        if readable:
            data = os.read(master, 65536)
            if not data:
                raise AssertionError(("early EOF", captured, proc.poll()))
            captured += data
try:
    wait_for(b"\x1b[6n")
    os.write(master, b"\x1b[24;10R")
    wait_for(b"QUERY_OK")
    assert not (root / "initial-prompt.delivered").exists()
    wait_for(b"FIRST_DELIVERED")
    assert proc.wait(timeout=10) == 0
    assert (root / "initial-prompt.delivered").exists()
finally:
    if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=10)
    os.close(master)
"#,
    )
    .unwrap();
    let output = boss_command_runner::output_blocking_timeout(
        std::process::Command::new("python3")
            .arg(&harness)
            .arg(root.path())
            .arg(&expected),
        std::time::Duration::from_secs(30),
    )
    .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn relay_refuses_keyboard_input_before_settings_initialize() {
    let root = tempfile::tempdir().unwrap();
    let settings = render_settings(&SettingsPlan {
        worker_kind: WorkerKind::Standard,
        workspace: Path::new("/workspace"),
        boss_data_dir: None,
        hooks_dir: Path::new("/private/hooks"),
        control_dir: Path::new("/private/control"),
        rules_root: Path::new("/rules-root"),
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
    let harness = root.path().join("refuse-stdin.py");
    fs::write(
        &harness,
        r#"
import os, pty, select, signal, subprocess, sys, time
from pathlib import Path
root = Path(sys.argv[1])
(root / "stub.py").write_text('''
import os, time, tty
tty.setraw(0)
os.write(1, b"\\x1b[?2004h? for shortcuts")
time.sleep(30)
''')
master, slave = pty.openpty()
proc = subprocess.Popen(
    [sys.executable, str(root / "initial-prompt.py"), sys.executable, str(root / "stub.py")],
    stdin=slave, stdout=slave, stderr=subprocess.PIPE, start_new_session=True,
    preexec_fn=lambda: signal.signal(signal.SIGHUP, signal.SIG_IGN),
)
os.close(slave)
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    readable, _, _ = select.select([master], [], [], 0.1)
    if readable:
        data = os.read(master, 65536)
        if b"? for shortcuts" in data:
            break
else:
    raise AssertionError("stub never became ready")
os.write(master, b"x")
stdout, stderr = proc.communicate(timeout=10)
os.close(master)
assert proc.returncode == 64, (proc.returncode, stderr)
assert b"input refused" in stderr
"#,
    )
    .unwrap();
    let output = boss_command_runner::output_blocking_timeout(
        std::process::Command::new("python3").arg(&harness).arg(root.path()),
        std::time::Duration::from_secs(20),
    )
    .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn relay_holds_the_brief_until_the_cli_log_shows_the_hooks_loaded() {
    let root = tempfile::tempdir().unwrap();
    let settings = render_settings(&SettingsPlan {
        worker_kind: WorkerKind::Standard,
        workspace: Path::new("/workspace"),
        boss_data_dir: None,
        hooks_dir: Path::new("/private/hooks"),
        control_dir: Path::new("/private/control"),
        rules_root: Path::new("/rules-root"),
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
    // The exact check file `write_session_settings` writes: settings plus the hook requirement.
    fs::write(
        root.path().join("settings-log-check.json"),
        serde_json::to_string(&serde_json::json!({
            "log_path": root.path().join("agy.log"),
            "expected": expected,
            "require": [HOOKS_LOADED_LINE],
            "forbid": [HOOKS_UNPARSEABLE_LINE],
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
init = 'I1117 cli_setting_manager.go:98] CLI settings initialized: ' + sys.argv[2]
hooks = {
    'loaded': 'I1117 hooks_manager.go:54] loaded 1 named hooks from 1 hooks.json file(s)',
    'absent': 'I1117 hooks_manager.go:54] loaded 0 named hooks from 0 hooks.json file(s)',
    'unparseable': 'E1117 hooks_manager.go:34] failed to parse hooks.json at /h: bad\nI1117 hooks_manager.go:54] loaded 0 named hooks from 1 hooks.json file(s)',
    'missing': '',
}[mode]
(root / 'agy.log').write_text(init + '\n' + hooks + '\n')
if mode != 'loaded':
    assert not select.select([0], [], [], 3.5)[0], 'brief although the hooks did not load'
    raise SystemExit(7)
data = b''
while not data.endswith(b'\x1b[201~'):
    data += os.read(0, 4096)
os.write(1, b'\r\nprivate sentinel')
assert os.read(0, 1) == b'\r'
"#;
    fs::write(root.path().join("stub.py"), code).unwrap();
    for mode in ["loaded", "missing", "absent", "unparseable"] {
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
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        if mode == "loaded" {
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(root.path().join(initial_prompt::DELIVERED_MARKER).exists());
        } else {
            assert_eq!(
                output.status.code(),
                Some(initial_prompt::BRIEF_NOT_DELIVERED_EXIT),
                "{mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!root.path().join(initial_prompt::DELIVERED_MARKER).exists(), "{mode}");
            assert!(root.path().join(initial_prompt::UNDELIVERED_MARKER).exists(), "{mode}");
        }
    }
}
