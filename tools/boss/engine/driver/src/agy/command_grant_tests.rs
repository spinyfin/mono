//! An allowlist kind's command grants: where the adapter gets them, and what
//! one grant covers.
//!
//! What a grant covers is checked by driving the adapter's matcher in a single
//! process. A call through the installed hook spawns the whole guard chain, so
//! the tests that go that way keep to the few cases that need the chain.

use super::*;

use super::super::super::permissions;

/// What `BOSS_BIN` holds while the matcher runs.
const BOSS_BIN: &str = "/opt/boss-tools/boss";

/// Loads the adapter as it is installed and prints every case whose coverage
/// is not what the case says. The second list is evaluated with `BOSS_BIN`
/// unset.
const MATRIX_DRIVER: &str = r#"
import importlib.util
import json
import os
import sys

spec = importlib.util.spec_from_file_location("boss_agy_hook_adapter", sys.argv[1])
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
matrix = json.load(sys.stdin)


def wrong(cases):
    return [case for case in cases
            if adapter.command_grant_covers(case["command"], matrix["grants"]) != case["covered"]]


mismatched = wrong(matrix["cases"])
del os.environ["BOSS_BIN"]
mismatched += wrong(matrix["without_boss_bin"])
print(json.dumps(mismatched))
"#;

fn cases(commands: &[&str], covered: bool) -> Vec<Value> {
    commands
        .iter()
        .map(|command| json!({"command": command, "covered": covered}))
        .collect()
}

/// The cases the installed matcher gets wrong; empty when it agrees with all
/// of them.
fn mismatches(grants: &[String], cases: Vec<Value>, without_boss_bin: Vec<Value>) -> Value {
    let tmp = TempDir::new().unwrap();
    let adapter = tmp.path().join("adapter.py");
    fs::write(
        &adapter,
        crate::guard_python::with_conversation_scratch(&crate::guard_python::with_command_tokenizer(ADAPTER_SCRIPT)),
    )
    .unwrap();
    let mut child = Command::new("python3")
        .args(["-c", MATRIX_DRIVER])
        .arg(&adapter)
        .env("BOSS_BIN", BOSS_BIN)
        .env("CUBE_BIN", "/opt/boss-tools/cube")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 is required for adapter acceptance tests");
    let matrix = json!({"grants": grants, "cases": cases, "without_boss_bin": without_boss_bin});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(matrix.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn a_progression_grant_covers_one_simple_command_that_begins_with_it() {
    let grants = permissions::command_grants(WorkerKind::Progression).unwrap();
    let covered = [
        // What progression agents typed in the runs that failed.
        r#""$BOSS_BIN" task show task_1"#,
        r#""$BOSS_BIN" pr status task_1"#,
        r#""$BOSS_BIN" pr status --json"#,
        r#""$BOSS_BIN" task list"#,
        r#""$BOSS_BIN" task list --product boss"#,
        r#""$BOSS_BIN" propose"#,
        r#""$BOSS_BIN" propose --help"#,
        r#""$BOSS_BIN" propose done --outcome no-changes-needed --summary "Still conflicting (10 files), and the revision is waiting.""#,
        r#""$BOSS_BIN" propose done --outcome no-changes-needed --summary 'Still conflicting and the revision is waiting.'"#,
        "gh pr view 3338 --repo spinyfin/mono --json state,mergeable,title,url",
        "gh pr checks 3338 --repo spinyfin/mono",
        // Other spellings of the program `BOSS_BIN` holds.
        r#""${BOSS_BIN}" propose unblock"#,
        "$BOSS_BIN propose unblock",
        "/opt/boss-tools/boss propose unblock",
        // Redirections that are not writes, and a brief piped in as a heredoc.
        r#""$BOSS_BIN" propose unblock 2>&1"#,
        r#""$BOSS_BIN" propose unblock > /dev/null"#,
        "\"$BOSS_BIN\" propose revise --brief-file - <<'EOF'\nfirst line; rm -rf /\nEOF",
        "gh pr view 1 --json title --jq '.title'",
    ];
    let refused = [
        // No grant names the verb or the program.
        "printenv BOSS_BIN",
        "gh pr list --repo spinyfin/mono --state all",
        "gh pr merge 1",
        "gh api repos/spinyfin/mono/pulls/1",
        "ghx pr view 1",
        r#""$BOSS_BIN" comment reply thread"#,
        r#""$BOSS_BIN" engine stop"#,
        r#""$BOSS_BIN" task"#,
        r#""$BOSS_BIN""#,
        "",
        // Not the program `BOSS_BIN` holds: literal text, or another variable.
        r#"'$BOSS_BIN' propose unblock"#,
        r#"\$BOSS_BIN propose unblock"#,
        r#""$BOSS_BIN_OTHER" propose unblock"#,
        r#""$CUBE_BIN" propose unblock"#,
        // A granted verb inside anything more than one simple command.
        r#""$BOSS_BIN" propose unblock; gh pr merge 1"#,
        r#""$BOSS_BIN" propose unblock && gh pr merge 1"#,
        r#""$BOSS_BIN" propose unblock | tee out"#,
        r#""$BOSS_BIN" propose unblock &"#,
        r#"( "$BOSS_BIN" propose unblock )"#,
        r#"{ "$BOSS_BIN" propose unblock; }"#,
        "\"$BOSS_BIN\" propose unblock\ngh pr merge 1",
        "! gh pr view 1",
        // Behind an assignment or a launcher.
        r#"FOO=1 "$BOSS_BIN" propose unblock"#,
        r#"env "$BOSS_BIN" propose unblock"#,
        "timeout 5 gh pr view 1",
        r#"eval "$BOSS_BIN" propose unblock"#,
        r#"sh -c '"$BOSS_BIN" propose unblock'"#,
        // Holding a substitution.
        r#""$BOSS_BIN" propose done --summary "$(cat /etc/passwd)""#,
        r#""$BOSS_BIN" propose done --summary `id`"#,
        "gh pr view $(echo 1)",
        "gh pr view 1 <(echo x)",
    ];
    // A grant this process cannot resolve covers nothing; a plain one is
    // unaffected.
    let mut without_boss_bin = cases(&[r#""$BOSS_BIN" propose unblock"#, "$BOSS_BIN propose unblock"], false);
    without_boss_bin.extend(cases(&["gh pr view 1"], true));

    let mut matrix = cases(&covered, true);
    matrix.extend(cases(&refused, false));
    assert_eq!(mismatches(&grants, matrix, without_boss_bin), json!([]));
}

#[test]
fn a_grant_of_several_words_needs_every_one_of_them() {
    // The review-guide table grants narrow shapes of broad programs.
    let grants = permissions::command_grants(WorkerKind::ReviewGuide).unwrap();
    let mut matrix = cases(
        &[
            "sed -n '1,5p' f",
            "wc -l f",
            "git show HEAD:f",
            "git --no-pager show HEAD:f",
            "git diff --no-ext-diff --no-textconv HEAD~1",
            "cat f",
            r#""${BOSS_BIN}" propose review-guide --body 'text'"#,
        ],
        true,
    );
    matrix.extend(cases(
        &[
            "sed -i s/a/b/ f",
            "sed f",
            "wc f",
            "git push",
            "git diff HEAD~1",
            "git --no-pager push",
            r#""$BOSS_BIN" propose done --outcome delivered --summary x"#,
        ],
        false,
    ));
    assert_eq!(mismatches(&grants, matrix, Vec::new()), json!([]));
}

#[test]
fn the_policy_carries_each_kinds_command_grants() {
    let fx = Fixture::new("run-policy-grants");
    for kind in [
        WorkerKind::Standard,
        WorkerKind::Reviewer,
        WorkerKind::Triage,
        WorkerKind::AnswerAgent,
        WorkerKind::ReviewGuide,
        WorkerKind::Progression,
    ] {
        let policy =
            expected_authorization_policy(&fx.process_home, kind, &fx.obs(), &fx.interception(|_| {})).unwrap();
        let expected = permissions::command_grants(kind).unwrap();
        assert_eq!(policy["command_grants"], json!(expected), "{kind:?}");
        // Only a kind answered `deny_unless_prior_grant` has grants to decide.
        assert_eq!(
            expected.is_empty(),
            policy["approval"] == "auto_approve",
            "{kind:?}: {policy:#}"
        );
    }
    // The installed document is that policy.
    fx.install(WorkerKind::Progression);
    let installed: Value = serde_json::from_slice(&fs::read(policy_path(&fx.process_home)).unwrap()).unwrap();
    assert_eq!(
        installed["command_grants"],
        json!(permissions::command_grants(WorkerKind::Progression).unwrap())
    );
}

#[test]
fn an_allowlist_kinds_command_runs_only_while_the_policy_grants_it() {
    let fx = Fixture::new("run-grant-source");
    fx.install_with(WorkerKind::Progression, |c| {
        c.is_standard_worker = false;
    });
    let command = run_command_payload("\"$BOSS_BIN\" propose done --outcome delivered --summary ok");
    let (_, body, stderr) = authorize(&fx.process_home, &command);
    assert_eq!(decision(&body), "auto_approve", "{body} {stderr}");
    // The same call with the grant withdrawn, and the binding kept
    // consistent, goes back to the kind's deny-by-default answer.
    failure_tests::change_policy(&fx, |policy| policy["command_grants"] = json!(["gh pr view"]));
    let (_, body, stderr) = authorize(&fx.process_home, &command);
    assert_eq!(decision(&body), "deny_unless_prior_grant", "{body} {stderr}");
}
