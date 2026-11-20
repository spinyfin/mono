use super::*;

/// Only the git subprocess sees an empty search path. Other tests retain
/// their environment, unlike a process-wide PATH mutation.
struct MissingGit<'a>(&'a DeclaredJj);

impl CommandRunner for MissingGit<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        let mut invocation = invocation.clone();
        if invocation.program == "git" {
            invocation.env.push(("PATH".into(), String::new()));
        }
        self.0.run(&invocation)
    }
}

#[test]
fn pull_ref_fetch_names_missing_git_program_and_store() {
    let f = Fixture::new();
    let head = base_repo_serving_only_a_pull_ref(&f);
    let info = serde_json::json!({"head": {"ref": "deleted", "sha": head, "repo": null}});
    let error =
        crate::app::pr_head::resolve_pr_head(&MissingGit(&f.runner), None, &f.ws, "example/repo", 1, &info, None)
            .err()
            .expect("git must be unavailable")
            .to_string();
    assert!(error.contains("running git fetch"), "{error}");
    assert!(error.contains(&f.ws.display().to_string()), "{error}");
    assert!(error.contains("--git-dir"), "{error}");
    assert!(error.contains("No such file or directory"), "{error}");
    assert!(!f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch-"));
}
