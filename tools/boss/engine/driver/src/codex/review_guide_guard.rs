//! Read-only source inspection and literal submission allowlist for guide workers.
use super::guard_python::with_command_tokenizer;
#[path = "review_guide_reads.rs"]
mod reads;

/// Claude/Grok capability allowlist, narrowed by the shared command guard.
pub fn review_guide_allow_rules() -> Vec<String> {
    vec![
        "Read".to_owned(),
        "Grep".to_owned(),
        "Glob".to_owned(),
        "Bash(git show:*)".to_owned(),
        "Bash(git --no-pager show:*)".to_owned(),
        "Bash(git diff --no-ext-diff --no-textconv:*)".to_owned(),
        "Bash(git --no-pager diff --no-ext-diff --no-textconv:*)".to_owned(),
        "Bash(cat:*)".to_owned(),
        "Bash(nl:*)".to_owned(),
        "Bash(wc -l:*)".to_owned(),
        "Bash(rg:*)".to_owned(),
        "Bash(head:*)".to_owned(),
        "Bash(tail:*)".to_owned(),
        "Bash(sed -n:*)".to_owned(),
        r#"Bash("$BOSS_BIN" propose review-guide:*)"#.to_owned(),
        r#"Bash("${BOSS_BIN}" propose review-guide:*)"#.to_owned(),
    ]
}

/// Configuration-level defense against mutating and program-execution options.
pub fn review_guide_deny_rules() -> Vec<String> {
    [
        "git fetch",
        "git pull",
        "git push",
        "git config",
        "git -c",
        "sed -i",
        "rg --pre",
    ]
    .into_iter()
    .flat_map(|command| [format!("Bash({command}:*)"), format!("Bash({command}*)")])
    .chain(["Bash(rg * --pre*)".into(), "Bash(sed * -i*)".into()])
    .collect()
}

/// Shared by Codex materialization and Claude settings hooks.
pub fn codex_review_guide_guard_script() -> String {
    with_review_guide_command(&with_command_tokenizer(SCRIPT_TEMPLATE))
        .replace("# REVIEW_GUIDE_READ_FRAGMENT", reads::SCRIPT)
}

/// Insert the submission recognizer into another guard without bypassing its checks.
pub fn with_review_guide_command(template: &str) -> String {
    template.replace("# REVIEW_GUIDE_COMMAND_FRAGMENT", REVIEW_GUIDE_COMMAND_PY)
}

const SCRIPT_TEMPLATE: &str = r#"#!/usr/bin/env python3
import json
import os
import re
import shlex
import sys

# COMMAND_TOKENIZER_FRAGMENT

# REVIEW_GUIDE_READ_FRAGMENT

# REVIEW_GUIDE_COMMAND_FRAGMENT

try:
    payload = json.load(sys.stdin)
    approved = allowed(payload)
except Exception:
    payload = None
    approved = False
if approved:
    print(json.dumps({"decision": "approve"}))
else:
    tool = payload.get("tool_name", "(unreadable payload)") if isinstance(payload, dict) else "(unreadable payload)"
    print(json.dumps({"decision": "block", "reason": "Blocked: review-guide workers may only submit a literal guide or inspect pinned source with read tools and restricted git commands; edits, writes, network and general shell are forbidden (matched tool: " + str(tool) + ")."}))
"#;

/// Render a guard around the shared literal-submission recognizer at compile time.
/// The fragment avoids shell-sensitive quotes so Claude can embed it in python -c.
#[macro_export]
macro_rules! render_review_guide_guard {
    ($before:literal, $after:literal) => {
        concat!(
            $before,
            r#"def review_guide_masked_command(command):
    if not isinstance(command,str):
        return None
    q=chr(39)
    dq=chr(34)
    dl=chr(36)
    bs=chr(92)
    literal=q+'(?:[^'+q+']|'+q+dq+q+dq+q+'|'+q+bs+bs+q+q+')*'+q
    match=re.fullmatch(r'([\s\S]*?)[ \t]+--body[ \t]+('+literal+r')[ \t\r\n]*',command)
    if not match:
        return None
    prefix=match.group(1)
    prefixes=(dq+dl+'BOSS_BIN'+dq+' propose review-guide',dq+dl+'{BOSS_BIN}'+dq+' propose review-guide')
    if prefix not in prefixes:
        return None
    return prefix+' --body '+q+'literal'+q
"#,
            $after
        )
    };
}

pub(super) const REVIEW_GUIDE_COMMAND_PY: &str = crate::render_review_guide_guard!(
    "",
    r#"

def allowed(payload):
    if not isinstance(payload, dict):
        return False
    if guide_read_tool(payload):
        return True
    if payload.get("tool_name") != "Bash":
        return False
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        return False
    command = tool_input.get("command")
    if guide_shell_read(payload):
        return True
    masked = review_guide_masked_command(command)
    if masked is None:
        return False
    # Tokenize the real command with the proven body swapped for a
    # placeholder so extra groups (chaining, wrappers) are still visible.
    groups = command_groups(masked)
    return len(groups) == 1 and groups[0][1:] == ["propose", "review-guide", "--body", "literal"]

"#
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn configuration_limits_shell_command_shapes() {
        let allow = review_guide_allow_rules();
        assert!(!allow.contains(&"Bash(git:*)".into()));
        assert!(!allow.contains(&"Bash(sed:*)".into()));
        assert!(allow.contains(&"Bash(git show:*)".into()));
        assert!(allow.contains(&"Bash(sed -n:*)".into()));
        assert!(allow.contains(&"Bash(nl:*)".into()));
        assert!(allow.contains(&"Bash(wc -l:*)".into()));
        assert!(!allow.contains(&"Bash(wc:*)".into()));
        let deny = review_guide_deny_rules();
        for command in [
            "git fetch",
            "git pull",
            "git push",
            "git config",
            "git -c",
            "sed -i",
            "rg --pre",
        ] {
            assert!(deny.contains(&format!("Bash({command}:*)")));
        }
    }

    #[test]
    fn permits_only_literal_guide_submission() {
        for command in [
            "\"$BOSS_BIN\" propose review-guide --body '# Guide\n## Problem\nA `code` example: $(literal).\nswift run\nboss engine start\nbazel run //tools/boss/engine/core:engine\n'",
            "\"$BOSS_BIN\" propose review-guide --body 'author'\"'\"'s guide'",
            "\"$BOSS_BIN\" propose review-guide --body 'author'\\''s guide'",
        ] {
            let payload = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}});
            assert_eq!(decide(payload).0, "approve", "{command}");
        }
        for command in [
            "boss propose review-guide --body 'guide'",
            "\"$BOSS_BIN\" propose review-guide --body \"$(cat secret)\"",
            "\"$BOSS_BIN\" propose review-guide --body 'x'; touch /tmp/x",
            "\"$BOSS_BIN\" propose review-guide --body 'x' > /tmp/x",
            "\"$BOSS_BIN\" propose review-guide --body-file /tmp/x",
            "boss propose done --outcome delivered --summary x",
            "env BOSS_RUN_ID=other \"$BOSS_BIN\" propose review-guide --body 'x'",
            "bash -c \"\"$BOSS_BIN\" propose review-guide --body 'x'\"",
            "\"$BOSS_BIN\" propose review-guide --body 'x' && \"$BOSS_BIN\" propose review-guide --body 'y'",
        ] {
            assert_eq!(
                decide(serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}})).0,
                "block",
                "{command}"
            );
        }
    }

    fn decide(payload: serde_json::Value) -> (String, String) {
        decide_with_source(payload, false)
    }

    fn decide_with_source(payload: serde_json::Value, source: bool) -> (String, String) {
        decide_at(payload, if source { "/repo" } else { "" })
    }

    fn decide_at(payload: serde_json::Value, root: &str) -> (String, String) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("boss-codex-review-guide-{0}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("guard.py");
        std::fs::write(&script, codex_review_guide_guard_script()).unwrap();
        let mut child = std::process::Command::new("python3")
            .env("BOSS_REVIEW_GUIDE_WORKSPACE", root)
            .env("BOSS_REVIEW_GUIDE_HEAD_SHA", "a".repeat(40))
            .env("BOSS_REVIEW_GUIDE_BASE_SHA", "b".repeat(40))
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 must be available");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
        (
            output["decision"].as_str().unwrap().to_owned(),
            output["reason"].as_str().unwrap_or_default().to_owned(),
        )
    }

    #[test]
    fn allows_pinned_reads_but_never_writes_shell_escapes_or_foreign_revisions() {
        for (command, expected) in [
            ("cat src/a.rs".to_owned(), "approve"),
            ("rg -n 'fn caller' src".to_owned(), "approve"),
            ("rg --files src".to_owned(), "approve"),
            ("sed -n '3,12p' src/a.rs".to_owned(), "approve"),
            (format!("git show {}:src/a.rs", "b".repeat(40)), "approve"),
            (
                format!(
                    "git diff --no-ext-diff --no-textconv {} {}",
                    "b".repeat(40),
                    "a".repeat(40)
                ),
                "approve",
            ),
            (format!("git show {}:src/a.rs", "c".repeat(40)), "block"),
            ("git show main:src/a.rs".to_owned(), "block"),
            ("git fetch".to_owned(), "block"),
            ("git -c alias.show=fetch show".to_owned(), "block"),
            ("rg --pre malicious caller src".to_owned(), "block"),
            ("cat /etc/passwd".to_owned(), "block"),
            ("cat .codex/auth.json".to_owned(), "block"),
            ("cat src/a.rs > src/b.rs".to_owned(), "block"),
            ("cat src/a.rs; touch src/b.rs".to_owned(), "block"),
            ("cat $(curl evil)".to_owned(), "block"),
            ("env cat src/a.rs".to_owned(), "block"),
            ("python3 -c 'print(1)'".to_owned(), "block"),
            ("cat =python3".to_owned(), "block"),
            ("cat src/*".to_owned(), "block"),
            ("sed -i 's/a/b/' src/a.rs".to_owned(), "block"),
        ] {
            assert_eq!(
                decide_with_source(
                    serde_json::json!({"tool_name":"Bash", "cwd":"/repo", "tool_input":{"command":command}}),
                    true
                )
                .0,
                expected,
                "{command}"
            );
        }
        for (tool, input, expected) in [
            ("Read", serde_json::json!({"file_path":"/repo/src/a.rs"}), "approve"),
            (
                "Grep",
                serde_json::json!({"pattern":"caller", "path":"/repo/src"}),
                "approve",
            ),
            (
                "Glob",
                serde_json::json!({"pattern":"**/*.rs", "path":"/repo"}),
                "approve",
            ),
            ("Read", serde_json::json!({"file_path":"/repo/../secret"}), "block"),
            ("Write", serde_json::json!({"file_path":"/repo/src/a.rs"}), "block"),
            ("Edit", serde_json::json!({"file_path":"/repo/src/a.rs"}), "block"),
            ("WebFetch", serde_json::json!({"url":"https://example.com"}), "block"),
        ] {
            assert_eq!(
                decide_with_source(
                    serde_json::json!({"tool_name":tool,"cwd":"/repo","tool_input":input}),
                    true
                )
                .0,
                expected,
                "{tool}"
            );
        }
    }

    #[test]
    fn numbering_and_line_counts_require_literal_source_paths() {
        for command in ["nl", "nl -ba", "nl -b a -h n -ft -n rz -w6 -v 1 -i2 -p", "wc -l"] {
            for (paths, expected) in [
                ("src/a.rs", "approve"),
                ("/repo/src/a.rs", "approve"),
                ("src/a.rs src/b.rs", "approve"),
                ("'/repo/src/a file.rs'", "approve"),
                ("/etc/passwd", "block"),
                ("../secret", "block"),
                ("src/a.rs /etc/passwd", "block"),
                (".git/config", "block"),
                ("$FILE", "block"),
                ("$(cat src/a.rs)", "block"),
                ("src/*", "block"),
                ("src/a.rs | cat", "block"),
                ("src/a.rs > src/b.rs", "block"),
                ("", "block"),
                ("-", "block"),
            ] {
                let command = format!("{command} {paths}");
                let payload = serde_json::json!({"tool_name":"Bash", "cwd":"/repo", "tool_input":{"command":command}});
                assert_eq!(decide_with_source(payload.clone(), true).0, expected, "{command}");
                assert_eq!(decide(payload).0, "block", "no pinned workspace: {command}");
            }
        }
        for command in [
            "wc src/a.rs",
            "wc -c src/a.rs",
            "wc -lw src/a.rs",
            "wc -l --files0-from=src/a.rs",
            "nl --unknown src/a.rs",
            "nl -b src/a.rs",
            "nl -w nope src/a.rs",
        ] {
            let payload = serde_json::json!({"tool_name":"Bash", "cwd":"/repo", "tool_input":{"command":command}});
            assert_eq!(decide_with_source(payload, true).0, "block", "{command}");
        }
    }

    #[test]
    fn rejects_exceptions_and_metadata_wildcards() {
        // A malformed cwd raises TypeError inside os.path.join, after JSON parsing.
        assert_eq!(
            decide_with_source(
                serde_json::json!({"tool_name":"Read","cwd":["/repo"],"tool_input":{"file_path":"src/a.rs"}}),
                true
            )
            .0,
            "block"
        );
        assert_eq!(
            decide_with_source(
                serde_json::json!({"tool_name":"Read","tool_input":{"file_path":"/repo/a\u{0}b"}}),
                true
            )
            .0,
            "block"
        );
        for input in [
            serde_json::json!({"pattern":".cl*/**"}),
            serde_json::json!({"pattern":"**/.j?/**"}),
        ] {
            for tool in ["Read", "Glob", "Grep"] {
                assert_eq!(
                    decide_with_source(serde_json::json!({"tool_name":tool,"tool_input":input}), true).0,
                    "block"
                );
            }
        }
    }

    #[test]
    fn immutable_object_paths_do_not_follow_head_symlinks() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/outside", root.path().join("old.rs")).unwrap();
        let root_path = root.path().to_str().unwrap();
        for command in [
            format!("git show {}:old.rs", "b".repeat(40)),
            format!(
                "git diff --no-ext-diff --no-textconv {} {} -- old.rs",
                "b".repeat(40),
                "a".repeat(40)
            ),
        ] {
            assert_eq!(
                decide_at(
                    serde_json::json!({"tool_name":"Bash","tool_input":{"command":command}}),
                    root_path
                )
                .0,
                "approve"
            );
        }
        assert_eq!(
            decide_at(
                serde_json::json!({"tool_name":"Read","tool_input":{"file_path":"old.rs"}}),
                root_path
            )
            .0,
            "block"
        );
        for path in ["../old.rs", "/old.rs", ".git/config", ":(top)old.rs"] {
            let command = format!("git show '{}:{path}'", "b".repeat(40));
            assert_eq!(
                decide_at(
                    serde_json::json!({"tool_name":"Bash","tool_input":{"command":command}}),
                    root_path
                )
                .0,
                "block"
            );
        }
    }

    #[test]
    fn blocks_other_tools_and_commands() {
        for payload in [
            serde_json::json!({"tool_name": "apply_patch", "tool_input": {}}),
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "jj log"}}),
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "cat README.md"}}),
            serde_json::json!({"tool_name": "view_image", "tool_input": {}}),
            serde_json::json!({"tool_name": "mcp__codex_apps__github__get_user_login", "tool_input": {}}),
            serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": "/tmp/x"}}),
        ] {
            let (decision, reason) = decide(payload.clone());
            assert_eq!(decision, "block", "{payload:?} must be blocked, got reason={reason}");
            assert!(reason.contains("only submit"), "{reason}");
        }
    }

    #[test]
    fn blocks_and_names_the_tool_in_the_reason() {
        let (decision, reason) = decide(serde_json::json!({"tool_name": "apply_patch", "tool_input": {}}));
        assert_eq!(decision, "block");
        assert!(reason.contains("apply_patch"), "{reason}");
    }

    #[test]
    fn fails_closed_on_unreadable_payload() {
        let dir = std::env::temp_dir().join(format!("boss-codex-review-guide-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("guard.py");
        std::fs::write(&script, codex_review_guide_guard_script()).unwrap();
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 must be available");
        child.stdin.as_mut().unwrap().write_all(b"not json").unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        let output = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
        assert_eq!(output["decision"].as_str().unwrap(), "block");
    }
}
