//! Commands are only JSON input to decision hooks; none are executed.
use crate::{AgentDriver, ClaudeDriver, ToolUseInterceptionConfig};
use std::io::Write;
use std::process::{Command, Stdio};

pub(crate) fn denied() -> Vec<String> {
    let mut commands = [
        "echo -n 'pkill foo' > k.sh; sh ./k.sh",
        "echo 'pkill -f foo' > /tmp/k.sh; bash -c 'sh /tmp/k.sh'",
        "echo 'pkill -f foo' > /tmp/k.sh; eval 'sh /tmp/k.sh'",
        "cat > /tmp/k.sh <<'EOF'\npkill -f foo\nEOF\nout=$(bash /tmp/k.sh)",
        "printf '%s' pk 'ill -f foo' > /tmp/k.sh; sh /tmp/k.sh",
        "printf pk > /tmp/k.sh; printf 'ill -f foo' >> /tmp/k.sh; sh /tmp/k.sh",
        "echo -n pk > /tmp/k.sh; echo -n 'ill -f foo' >> /tmp/k.sh; sh /tmp/k.sh",
        "cat <<EOF>/tmp/k.sh\npkill -f foo\nEOF\nsh /tmp/k.sh",
        "cat<<'EOF' > /tmp/k.sh\npkill -f foo\nEOF\nsh /tmp/k.sh",
        "cat <<EOF; echo $((1 << 3))\nuse care\nEOF\npkill -f foo",
        "cat <<EOF >/dev/null; x=$((1 << 3))\nuse care\nEOF\n3\npkill -f foo",
        "echo -e 'pkill foo' > k.sh; sh ./k.sh",
        "awk 'BEGIN {print \"pkill foo\"}' > /tmp/k.sh; sh /tmp/k.sh",
        "cat > /tmp/k.sh <<'EOF'\npkill foo\nEOF\ncat >> /tmp/k.sh <<'EOF'\necho fine\nEOF\nsh /tmp/k.sh",
        "echo 'pkill foo' > /tmp/k.sh; echo harmless | tee -a /tmp/k.sh; sh /tmp/k.sh",
        "pgrep -f foo | xargs -I {} kill 12345 {}",
        "if true; then command pkill -f foo; fi",
        "! command pkill -f foo",
        "coproc command pkill -f foo",
        "bash -c 'K=\"pkill -f\"; $K foo'",
        "K=\"kill -9\"; $K -1",
        "bash <<'EOF'; echo done\npkill -f foo\nEOF",
        "cat > /tmp/k.sh <<'EOF'; echo done\npkill -f foo\nEOF\nbash /tmp/k.sh",
        "echo 'pkill -f foo' | (cat) | sh",
        "echo 'pkill -f foo' | { cat; } | sh",
        "cat > k.sh <<'EOF'\npkill -f foo\nEOF\nbash ./k.sh",
        "cat > /tmp/k.sh <<'EOF'\npkill -f foo\nEOF\nsh < /tmp/k.sh",
        "cat > /tmp/k.sh <<'EOF'\npkill -f foo\nEOF\nchmod +x /tmp/k.sh; /tmp/k.sh",
        "tee /tmp/k.sh <<'EOF'\npkill -f foo\nEOF\nbash /tmp/k.sh",
        "echo 'pkill -f foo' | tee /tmp/k.sh; sh /tmp/k.sh",
        "printf '%s' 'pkill -f foo' > /tmp/k.sh; sh /tmp/k.sh",
        "echo 'pkill -f foo' > /tmp/k.sh; echo harmless >> /tmp/k.sh; sh /tmp/k.sh",
        "=sudo pkill -f foo",
        "=nohup killall foo",
        "pgrep foo | =xargs kill",
        "if true; then env K=pkill command $K foo; fi",
        "bash <<'ONE'; bash <<'TWO'\necho safe\nONE\npkill foo\nTWO",
        "K=ls; read K <<< 'pkill foo'; bash -c '$K'",
        "K=pkill; eval '$K foo'",
        "K=pkill; find . -exec sh -c '$K foo' \\;",
        "K=pkill; flock /tmp/l -c '$K foo'",
        "K=kill; A=p; $A$K foo",
        "pk$UNKNOWN foo",
        "${UNKNOWN}ill -9 -1",
        "sleep 1 & pid=$!; echo $(a[pid=-1]=1); kill -9 $pid",
        "true # comment \\\npkill -f foo",
        "K=pkill; bash -c \"$K foo\"",
        "K=pkill; echo $($K foo)",
        "export K=pkill; sh -c '$K -f foo'",
        "env K=pkill sh -c '$K foo'",
        "K=pkill; bash <<EOF\n$K foo\nEOF",
        "S=bash; $S <<'EOF'\npkill -f foo\nEOF",
        "S=bash; $S <<EOF\nkillall foo\nEOF",
        "K=kill; p$K foo",
        "K=ill; pk$K foo",
        "A=ki; ${A}ll -9 -1",
        "A=pk; ${A}ill foo",
        "sleep 1 & pid=$!; a[pid=-1]=1; kill -9 $pid",
        "sleep 1 & pid=$!; echo $[pid=-1]; kill -9 $pid",
        "sleep 1 & pid=$!; [[ pid=-1 -eq -1 ]]; kill -9 $pid",
        "sleep 1 & pid=$!; declare -n r=pid; r=-1; kill -9 $pid",
        "read c <<EOF\npkill -f foo\nEOF\n$c",
        "read c <<EOF\npkill -f foo\nEOF\nbash -c '$c'",
        "read c <<< 'pkill foo'; $c",
        "bash -c -- 'pkill -f foo'",
        "sh -c -e 'killall foo'",
        "bash -c -o pipefail 'kill $(pgrep x)'",
        "zsh -c +o errexit -- 'pkill foo'",
        "su -c -- 'pkill foo'",
        "flock /tmp/l -c -- 'pkill foo'",
        "script -c -- 'killall foo'",
        "C='pkill -f foo'; bash -c \"$C\"",
        "C='pkill -f foo'; eval \"$C\"",
        "C='killall foo'; bash -c 'eval \"$C\"'",
        "$SHELL -c 'true; pkill -f foo'",
        "$BASH -c 'true; killall foo'",
        "$SHELL -c -- 'true; pkill foo'",
        "echo 'pkill -f foo' | (sh)",
        "echo 'killall foo' | { sh; }",
        "echo 'pkill -f foo' | bash /dev/stdin",
        "echo 'pkill -f foo' | bash /dev/fd/0",
        "echo 'pkill -f foo' | bash -",
        "bash <(echo 'pkill -f foo')",
        "source <(echo 'pkill foo')",
        ". <(echo 'pkill foo')",
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
        "echo $(date)#; pkill -f foo",
        "echo foo\\ #bar; pkill -f foo",
        "echo \"$(date)\"#; pkill -f foo",
        "sleep 100 & pid=$!; if true; then pid=-1; fi; kill -9 \"$pid\"",
        "sleep 1 & pid=$!; if false; then :; else pid=-1; fi; kill $pid",
        "sleep 1 & pid=$!; while false; do pid=-1; done; kill $pid",
        "pid=$!; for pid in $(pgrep -f foo); do kill $pid; done",
        "pid=$!; read pid < <(pgrep foo); kill $pid",
        "pid=$!; printf -v pid %s -1; kill $pid",
        "pid=$!; export pid=-1; kill $pid",
        "pid=$!; declare pid=-1; kill $pid",
        "pid=$!; local pid=-1; kill $pid",
        "pid=$!; pid+=1; kill $pid",
        "pid=$!; echo $((pid=-1)); kill $pid",
        "pid=$!; (( pid = -1 )); kill $pid",
        "pid=$!; echo ${pid:=-1}; kill $pid",
        "export K=pkill; $K -f foo",
        "declare K=pkill; $K foo",
        "S=bash; $S -c 'pkill -f foo'",
        "W=command; $W killall foo",
        "K=$(echo pkill); $K -f foo",
        "echo $((pkill -f foo); true)",
        "echo $((pkill foo) )",
        "x=$((pkill foo) )",
        "echo pkill foo | bash -o pipefail",
        "bash <<< 'pkill foo'",
        "cat <<'EOF' | sh\npkill foo\nEOF",
        "watch -n1 pkill foo",
        "watch 'pkill foo'",
        "flock /tmp/l pkill foo",
        "flock -w 5 /tmp/l killall foo",
        "flock /tmp/l -c 'pkill foo'",
        "chrt -f 10 pkill foo",
        "taskset 0x1 pkill foo",
        "taskset -c 0 killall foo",
        "find . -maxdepth 0 -exec pkill foo \\;",
        "find . -exec sh -c 'pkill foo' \\;",
        "su -c 'pkill foo'",
        "su root -c 'killall foo'",
        "script -q /dev/null -c 'pkill foo'",
        "coproc NAME { pkill foo; }",
        "$'pkill' foo",
        "pk$(echo ill) foo",
        "K=$(command -v pkill); bash -c 'echo $(date); $K -f foo'",
        "ps aux | grep 'cube.*pr update' | awk '{print \"kill \" $2}' | sh",
        "(echo 'pkill -f foo'; echo done) | sh",
        "{ echo 'pkill -f foo'; echo done; } | sh",
        "trap 'kill 0' EXIT",
        "trap 'pkill -f foo' EXIT",
        "trap 'kill -9 -1' EXIT",
        "cat > /tmp/k.sh <<'EOF'\npkill -f foo\nEOF\nbash /tmp/k.sh",
        "=pkill -f foo",
        "noglob pkill -f foo",
        "nocorrect killall foo",
        "- pkill foo",
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
    "cat <<EOF>out.txt\nuse pkill carefully\nEOF",
    "cat<<'EOF' > notes.md\nit's fine to mention pkill here\nEOF",
    "echo 'echo hi' > /tmp/k.sh; bash -c 'sh /tmp/k.sh'",
    "printf '%s\\n' pk 'ill -f foo' > /tmp/k.sh; sh /tmp/k.sh",
    "echo pk > /tmp/k.sh; echo 'ill -f foo' >> /tmp/k.sh; sh /tmp/k.sh",
    "echo 'echo safe' > k.sh; sh ./k.sh",
    "printf '%s' 'echo safe' > k.sh; sh < ./k.sh",
    "cat <<'EOF'; echo done\npkill foo\nEOF",
    "find . -exec ls {} +",
    "K=\"pkill -f\"; \"$K\" foo",
    "read c <<< 'pkill foo'; bash -c 'echo $c'",
    "S=cat; $S <<EOF\npkill -f foo\nEOF",
    "true # comment \\\necho hi",
    "\"$BOSS_BIN\" propose done --summary 'pkill/killall are now denied'",
    "\"$CUBE_BIN\" pr update --title 'pkill guard'",
    "S=cat; $S <<'EOF'\npkill -f foo\nEOF",
    "echo 'p\\\nkill foo'",
    "cat <<'EOF'\np\\\nkill foo\nEOF",
    "\"$CUBE_BIN\" pr update --title 'pkill is now blocked for workers'",
    "\"$BOSS_BIN\" propose done --summary 'killall is now blocked for workers'",
    "C='echo hi'; bash -c -- \"$C\"",
    "C='echo hi'; eval \"$C\"",
    "$SHELL -c 'echo hi'",
    "bash -c -- 'kill 12345'",
    "sh -c -e 'echo hi'",
    "echo hi | (sh)",
    "echo hi | bash /dev/stdin",
    "bash <(echo 'echo hi')",
    "source <(echo 'echo hi')",
    ". <(echo 'echo hi')",
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
    "echo $(date)#tag",
    "ls # pkill foo",
    "ls # $(pkill foo)",
    "echo foo\\ #bar",
    "python3 -c \"\nx = 1 << 2\nprint(x)\n\"",
    "jj describe -m \"Title\n\nHandle cat <<'EOF' bodies\"",
    "cat <<'END-DOC'\nhi\nEND-DOC",
    "cat <<END-OF-DOC\nhi\nEND-OF-DOC",
    "cat <<EOF\nuse pkill carefully\nEOF",
    "bash -c 'pgrep -f \"pkill|killall\"'",
    "bash -c 'rg \"pkill|killall\" src'",
    "rg -n pkill src | head; bash scripts/check.sh",
    "echo hi | sh",
    "S=bash; $S -c 'echo hi'",
    "sleep 100 & pid=$!; echo \"started $pid\"; kill $pid",
    "sleep 1 & pid=$!; sleep 1 & pid=$!; kill $pid",
    "watch -n1 ls",
    "flock /tmp/l ls",
    "taskset -c 0 ls",
    "find . -maxdepth 1 -exec ls {} \\;",
    "su -c 'ls'",
    "\"$TOOL\" pr create --title 'Block pkill and killall'",
    "eval \"$(brew shellenv)\"",
    "bash -c \"$(cat run.sh)\"",
    "ROOT=$(jj root); bash -c \"$ROOT/scripts/x.sh\"",
    "trap -l",
    "trap -p EXIT",
    "cat > /tmp/k.sh <<'EOF'\necho hi\nEOF\nbash /tmp/k.sh",
];

fn claude_launch_guard_script() -> tempfile::NamedTempFile {
    let config = ToolUseInterceptionConfig {
        data_dir: None,
        path_guard_script: None,
        checkleft_guard_script: None,
        git_store_guard_script: None,
        is_revision: false,
        is_standard_worker: false,
        is_reviewer: false,
        is_review_guide: false,
        run_id: None,
        workspace_path: None,
    };
    let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
    let guard = wiring.pre_tool_use_hooks[0]["hooks"][0]["command"].as_str().unwrap();
    let body = guard
        .strip_prefix("python3 -c \"")
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("launch guard is python3 -c");
    let mut script = tempfile::NamedTempFile::new().unwrap();
    script.write_all(body.as_bytes()).unwrap();
    script.flush().unwrap();
    script
}

/// One interpreter for the whole matrix. Spawning python3 per command blew
/// the small-test 60s shard budget once the deny list grew past ~180 cases.
const BATCH_RUNNER: &str = r#"
import json, sys, io, contextlib
script = open(sys.argv[1]).read()
results = []
for cmd in json.load(sys.stdin):
    buf = io.StringIO()
    fake_in = io.StringIO(json.dumps({"tool_name": "Bash", "tool_input": {"command": cmd}}))
    with contextlib.redirect_stdout(buf):
        old = sys.stdin
        sys.stdin = fake_in
        try:
            exec(compile(script, "guard.py", "exec"), {"__name__": "__main__"})
        except SystemExit:
            pass
        finally:
            sys.stdin = old
    results.append(json.loads(buf.getvalue()))
print(json.dumps(results))
"#;

fn claude_guard_decisions(script: &std::path::Path, commands: &[String]) -> Vec<serde_json::Value> {
    let mut child = Command::new("python3")
        // Hook decisions only: these named tools are never executed.
        .env("BOSS_BIN", "/worker/bin/boss")
        .env("CUBE_BIN", "/worker/bin/cube")
        .arg("-c")
        .arg(BATCH_RUNNER)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(commands).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "batch runner failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn claude_signal_guard_denies_forbidden_commands() {
    let script = claude_launch_guard_script();
    let commands = denied();
    let decisions = claude_guard_decisions(script.path(), &commands);
    assert_eq!(decisions.len(), commands.len());
    for (command, decision) in commands.iter().zip(decisions) {
        assert_eq!(decision["decision"], "block", "{command}: {decision}");
        let reason = decision["reason"].as_str().unwrap();
        assert!(
            reason.contains("other workers") && reason.contains("explicit positive numeric PIDs"),
            "{command}: {reason}"
        );
    }
}

#[test]
fn claude_signal_guard_allows_safe_commands() {
    let script = claude_launch_guard_script();
    let commands: Vec<String> = ALLOWED.iter().map(|s| (*s).to_string()).collect();
    let decisions = claude_guard_decisions(script.path(), &commands);
    assert_eq!(decisions.len(), commands.len());
    for (command, decision) in commands.iter().zip(decisions) {
        assert_eq!(decision["decision"], "approve", "{command}: {decision}");
    }
}

#[test]
fn signal_fragment_is_safe_inside_the_inline_shell_command() {
    let fragment = include_str!("signal_guard.py");
    assert!(!fragment.contains(['"', '$', '`', '\\']));
}

#[test]
fn launch_guard_strips_the_same_wrappers_as_the_signal_guard() {
    let script = claude_launch_guard_script();
    let commands: Vec<String> = ["flock /tmp/l bossctl status", "sudo -D /tmp bossctl status"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let decisions = claude_guard_decisions(script.path(), &commands);
    for (command, decision) in commands.iter().zip(decisions) {
        assert_eq!(decision["decision"], "block", "{command}: {decision}");
    }
}
