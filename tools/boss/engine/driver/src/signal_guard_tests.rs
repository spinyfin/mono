//! Commands are only JSON input to decision hooks; none are executed.
use crate::{AgentDriver, ClaudeDriver, ToolUseInterceptionConfig};
use std::io::Write;
use std::process::{Command, Stdio};

pub(crate) fn denied() -> Vec<String> {
    let mut commands = [
        "echo \"$(bash <<'EOF'\npkill foo\nEOF\n)\"",
        "echo \"$(cat <<EOF\n$(pkill foo)\nEOF\n)\"",
        "# don't\npkill -f foo\n# it's",
        "echo $(# don't\npkill foo\n# it's\n)",
        "if false; then :; else pkill -f foo; fi",
        "function f { pkill foo; }",
        "coproc pkill foo",
        "nice -n 10 pkill -f foo",
        "nice pkill foo",
        "ionice -c 3 killall foo",
        "doas -u root pkill foo",
        "$(command -v pkill) -f foo",
        "`which killall` foo",
        "K=pkill; $K -f foo",
        "K=/usr/bin/killall; ${K} foo",
        "echo 'pkill -f foo' | sh",
        "printf '%s' 'killall foo' | bash",
        "pid=$!; pid=0; kill $pid",
        "pid=0; pid=$!; kill $pid",
        "echo pid=$!; kill $pid",
        "K=pkill; echo K=ls; $K foo",
        "cd .../mono-agent-533; pkill -f 'cube.*pr update'; jj st ...",
        "pkill foo",
        "pkill -9 -f foo",
        "killall foo",
        "killall -9 foo",
        "/usr/bin/pkill -f foo",
        "/usr/bin/killall foo",
        "'pkill' foo",
        "p\\kill foo",
        "pgrep -f foo | xargs kill",
        "pgrep foo|xargs -I{} /bin/kill -TERM {}",
        "kill $(pgrep -f foo)",
        "kill `pgrep -f foo`",
        "kill \"$(pgrep foo)\"",
        "ps aux | grep foo | awk '{print $2}' | xargs kill",
        "pids=$(pgrep foo); kill $pids",
        "kill -- -12345",
        "kill -TERM -12345",
        "kill -9 -12345",
        "kill -1",
        "kill -9 -1",
        "kill 0",
        "kill 4294967295",
        "kill 4294967296",
        "kill -s TERM 0",
        "kill --signal=TERM -1",
        "echo $(pkill foo)",
        "echo \"$(killall foo)\"",
        "echo `pkill foo`",
        "true;pkill foo",
        "true&&killall foo",
        "false||pkill foo",
        "(pkill foo)",
        "((pkill foo))",
        "{ killall foo; }",
        "if true; then pkill foo; fi",
        "bash -c 'echo ready\npkill foo'",
        "eval 'pkill foo'",
        "env -S 'pkill foo'",
        "env -S'pkill foo'",
        "env --split-string='killall foo'",
        "echo '<<EOF'\npkill foo\nEOF",
        "cat <<EOF\n$(pkill foo)\nEOF",
        "cat <<EOF\n'$(pkill foo)'\nEOF",
        "bash <<'EOF'\npkill foo\nEOF",
        "p\\\nkill foo",
        "sudo --role worker pkill foo",
        ">/tmp/log pkill foo",
        "2>/tmp/log sudo killall foo",
        "2>&1 command pkill foo",
        "kill >/tmp/log -9 -1",
        "cat <(pkill foo)",
    ]
    .map(str::to_owned)
    .to_vec();
    for command in ["pkill -f foo", "killall foo", "kill -9 -1", "pgrep foo | xargs kill"] {
        for wrapper in [
            "sudo",
            "sudo -u root --",
            "env",
            "env -u FOO",
            "env FOO=bar",
            "command",
            "command --",
            "exec",
            "exec -a worker",
            "nohup",
            "timeout -s TERM 5",
            "builtin",
        ] {
            commands.push(format!("{wrapper} {command}"));
        }
        for shell in ["bash -c", "sh -c", "zsh -c", "bash -lc", "sudo env FOO=bar bash -c"] {
            commands.push(format!("{shell} '{command}'"));
        }
    }
    commands
}

pub(crate) const ALLOWED: &[&str] = &[
    "jj commit -m \"$(cat <<'EOF'\nDon't block workers\n\n1) first\nEOF\n)\"",
    "echo $((1<<3))",
    "echo $((1<<3))\nls",
    "echo `cat <<'EOF'\nDon't parse ) this\nEOF\n`",
    "# what's <<EOF?\nls",
    "# what's running?\nls",
    "echo foo#bar",
    "echo '# not a comment'",
    "echo $(# what's running?\nls\n)",
    "sleep 100 & pid=$!; kill $pid",
    "sleep 100 & pid=${!}; kill -0 \"${pid}\"",
    "kill $!",
    "kill ${!}",
    "kill 12345",
    "kill -TERM 12345 12346",
    "kill -9 12345",
    "kill -s TERM -- 12345",
    "kill --signal=TERM 12345",
    "kill -- 12345",
    "kill 12345 >/tmp/log",
    "2>/tmp/log kill -TERM 12345",
    "pgrep -f foo",
    "pgrep -f foo | head -10",
    "sudo -u root kill 12345",
    "env FOO=bar command kill -TERM 12345",
    "bash -c 'kill 12345'",
    "(kill 12345)",
    "kill -l",
    "command -v pkill",
    "echo 'pkill foo; killall bar; kill $(pgrep foo)'",
    "rg 'pkill|killall' /tmp/source",
    "cat > /tmp/doc <<'DOC'\npkill foo\nkill $(pgrep foo)\nDOC",
];

#[test]
fn claude_signal_guard_decisions() {
    let config = ToolUseInterceptionConfig {
        data_dir: None,
        path_guard_script: None,
        checkleft_guard_script: None,
        is_revision: false,
        is_standard_worker: false,
        is_reviewer: false,
        is_review_guide: false,
        run_id: None,
        workspace_path: None,
    };
    let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
    let guard = wiring.pre_tool_use_hooks[0]["hooks"][0]["command"].as_str().unwrap();
    for (command, expected) in denied()
        .iter()
        .map(|c| (c.as_str(), "block"))
        .chain(ALLOWED.iter().map(|c| (*c, "approve")))
    {
        let payload = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}});
        let mut child = Command::new("sh")
            .args(["-c", guard])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{command}: {:?}", output);
        let decision: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(decision["decision"], expected, "{command}: {decision}");
        if expected == "block" {
            let reason = decision["reason"].as_str().unwrap();
            assert!(reason.contains("other workers") && reason.contains("explicit positive numeric PIDs"));
        }
    }
}

#[test]
fn signal_fragment_is_safe_inside_the_inline_shell_command() {
    let fragment = include_str!("signal_guard.py");
    assert!(!fragment.contains(['"', '$', '`', '\\']));
}
