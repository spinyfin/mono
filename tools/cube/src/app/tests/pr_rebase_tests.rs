use super::rebase_tests::failing_cmd;
use super::support::{ExpectedCommand, FakeRunner};
use crate::app::pr_rebase::{PR_TARGETS, rebase_preserving_pr_bookmarks};
use std::path::PathBuf;

#[test]
fn local_rebase_fetches_github_base_and_preserves_current_and_sibling_published_heads() {
    for base in ["main", "parent-pr"] {
        let cwd = PathBuf::from("/ws");
        let ok = |args: &[&str], output: &str| ExpectedCommand::ok(cwd.clone(), "jj", args, output);
        let destination = format!("\"{base}\"@\"github\"");
        let runner = FakeRunner::new(vec![
            ok(
                &["git", "remote", "list"],
                "origin /stale/mirror\ngithub git@github.com:org/repo.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr",
                    "view",
                    "77",
                    "-R",
                    "org/repo",
                    "--json",
                    "baseRefName",
                    "--jq",
                    ".baseRefName",
                ],
                base,
            ),
            ok(&["git", "fetch", "--remote", "github"], ""),
            ok(&["git", "import"], ""),
            ok(
                &[
                    "bookmark",
                    "list",
                    "-r",
                    &format!("({destination}..@)::"),
                    "-T",
                    PR_TARGETS,
                ],
                "pr/77\taaaa\npr/78\tbbbb\n",
            ),
            ok(&["bookmark", "delete", "pr/77", "pr/78"], ""),
            ok(&["git", "export"], ""),
            ok(
                &["rebase", "-b", "@", "-d", &destination, "--ignore-immutable"],
                "rebased",
            ),
            ok(&["bookmark", "set", "pr/77", "-r", "aaaa", "--allow-backwards"], ""),
            ok(&["bookmark", "set", "pr/78", "-r", "bbbb", "--allow-backwards"], ""),
            ok(&["git", "export"], ""),
            ok(
                &[
                    "log",
                    "-r",
                    &format!("{destination}..@"),
                    "--no-graph",
                    "-T",
                    "commit_id ++ \" conflicts=\" ++ conflict ++ \"\\n\"",
                ],
                "cccc conflicts=false\n",
            ),
        ]);
        let result = crate::app::workspace_local_rebase::rebase_at(&runner, None, &cwd, 77).unwrap();
        runner.assert_exhausted();
        assert_eq!(result.payload["destination"], destination);
        assert_eq!(result.payload["base_branch"], base);
        assert!(result.message.contains("cccc conflicts=false"));
    }
}

#[test]
fn export_failures_restore_bookmarks_and_report_the_original_error() {
    for initial_export_fails in [false, true] {
        let cwd = PathBuf::from("/ws");
        let ok = |args: &[&str], output: &str| ExpectedCommand::ok(cwd.clone(), "jj", args, output);
        let mut commands = vec![
            ok(&["git", "import"], ""),
            ok(
                &["bookmark", "list", "-r", "(main..feature)::", "-T", PR_TARGETS],
                "pr/1\taaaa\n",
            ),
            ok(&["bookmark", "delete", "pr/1"], ""),
        ];
        if initial_export_fails {
            commands.push(failing_cmd(
                cwd.clone(),
                "jj",
                &["git", "export"],
                "initial export failure",
            ));
        } else {
            commands.push(ok(&["git", "export"], ""));
            commands.push(ok(
                &["rebase", "-b", "feature", "-d", "main", "--ignore-immutable"],
                "rebased",
            ));
        }
        commands.push(ok(&["bookmark", "set", "pr/1", "-r", "aaaa", "--allow-backwards"], ""));
        commands.push(failing_cmd(
            cwd.clone(),
            "jj",
            &["git", "export"],
            "final export failure",
        ));
        let runner = FakeRunner::new(commands);
        let error = rebase_preserving_pr_bookmarks(&runner, None, &cwd, "feature", "main")
            .unwrap_err()
            .to_string();
        runner.assert_exhausted();
        assert!(
            error.contains(if initial_export_fails {
                "initial export failure"
            } else {
                "final export failure"
            }),
            "{error}"
        );
    }
}

#[test]
fn restoration_attempts_every_bookmark_and_export_preserving_primary_error() {
    for rebase_fails in [false, true] {
        let cwd = PathBuf::from("/ws");
        let ok = |args: &[&str], output: &str| ExpectedCommand::ok(cwd.clone(), "jj", args, output);
        let rebase = ["rebase", "-b", "feature", "-d", "main", "--ignore-immutable"];
        let runner = FakeRunner::new(vec![
            ok(&["git", "import"], ""),
            ok(
                &["bookmark", "list", "-r", "(main..feature)::", "-T", PR_TARGETS],
                "pr/1\taaaa\npr/2\tbbbb\npr/3\tcccc\n",
            ),
            ok(&["bookmark", "delete", "pr/1", "pr/2", "pr/3"], ""),
            ok(&["git", "export"], ""),
            if rebase_fails {
                failing_cmd(cwd.clone(), "jj", &rebase, "primary rebase failure")
            } else {
                ok(&rebase, "rebased")
            },
            failing_cmd(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/1", "-r", "aaaa", "--allow-backwards"],
                "first restore failure",
            ),
            failing_cmd(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/2", "-r", "bbbb", "--allow-backwards"],
                "second restore failure",
            ),
            ok(&["bookmark", "set", "pr/3", "-r", "cccc", "--allow-backwards"], ""),
            failing_cmd(cwd.clone(), "jj", &["git", "export"], "final export failure"),
        ]);
        let error = rebase_preserving_pr_bookmarks(&runner, None, &cwd, "feature", "main")
            .unwrap_err()
            .to_string();
        runner.assert_exhausted();
        if rebase_fails {
            assert!(error.contains("primary rebase failure"), "{error}");
        } else {
            assert!(error.contains("first restore failure"), "{error}");
            assert!(error.contains("pr/1, pr/2"), "{error}");
        }
    }
}
