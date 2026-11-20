use super::*;

/// Simulate an executable disappearing after resolution, before the spawn.
struct MissingGit<'a>(&'a DeclaredJj);

impl CommandRunner for MissingGit<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        let mut invocation = invocation.clone();
        if Path::new(&invocation.program)
            .file_name()
            .is_some_and(|name| name == "git")
        {
            invocation.program.push_str("-missing");
        }
        self.0.run(&invocation)
    }
}

/// Reproduce the old spawn failure without racing the test process PATH.
/// Git inherits the empty search path the old Checkleft guard installed unless
/// its invocation explicitly supplies the captured PATH.
struct EmptyGitPath<'a> {
    router: &'a PrJsonOnly<'a>,
    bare_git: bool,
}

impl CommandRunner for EmptyGitPath<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        let mut invocation = invocation.clone();
        if Path::new(&invocation.program)
            .file_name()
            .is_some_and(|name| name == "git")
        {
            assert!(Path::new(&invocation.program).is_absolute());
            assert!(Path::new(&invocation.program).is_file());
            assert!(invocation.cwd.is_dir());
            assert!(Path::new(&invocation.args[1]).is_dir());
            if self.bare_git {
                invocation.program = "git".into();
                invocation.env.retain(|(name, _)| name != "PATH");
            }
            invocation.env.insert(0, ("PATH".into(), String::new()));
        }
        self.router.run(&invocation)
    }
}

#[test]
fn pull_ref_fetch_pins_git_and_helper_path_before_spawn() {
    let f = Fixture::new();
    let head = base_repo_serving_only_a_pull_ref(&f);
    f.jj(
        &f.ws,
        &["git", "remote", "add", "upstream", "https://github.com/example/repo"],
    );
    let router = PrJsonOnly {
        jj: &f.runner,
        base: f._dir.path().join("base"),
        lookups: Default::default(),
        pr_json: String::new(),
    };
    assert_eq!(f.commit(&format!("present({head})")), "");
    let info = serde_json::json!({"head": {"ref": "deleted", "sha": head, "repo": null}});
    let old = EmptyGitPath {
        router: &router,
        bare_git: true,
    };
    let error = crate::app::pr_head::resolve_pr_head(&old, None, &f.ws, "example/repo", 1, &info, None)
        .err()
        .expect("the old bare git invocation must fail")
        .to_string();
    assert!(error.contains("running git fetch"), "{error}");
    assert!(error.contains("No such file or directory"), "{error}");
    assert_eq!(f.commit(&format!("present({head})")), "");
    assert!(!f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch-"));
    let runner = EmptyGitPath {
        router: &router,
        bare_git: false,
    };
    let resolved = crate::app::pr_head::resolve_pr_head(&runner, None, &f.ws, "example/repo", 1, &info, None).unwrap();
    assert_eq!(resolved.sha, head);
    assert_eq!(f.commit(&head), head, "head was imported into the shared store");
    assert!(!f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch-"));
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
