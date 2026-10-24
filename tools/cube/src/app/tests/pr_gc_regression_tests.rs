use super::*;

#[test]
fn concurrent_fork_fetches_use_distinct_remotes() {
    struct ConcurrentFetch<'a> {
        jj: &'a DeclaredJj,
        added: std::sync::Mutex<Vec<String>>,
        barrier: std::sync::Barrier,
    }
    impl CommandRunner for ConcurrentFetch<'_> {
        fn run(&self, invocation: &CommandInvocation) -> Result<String> {
            let result = self.jj.run(invocation);
            if invocation
                .args
                .starts_with(&["git".into(), "remote".into(), "add".into()])
            {
                self.added.lock().unwrap().push(invocation.args[3].clone());
                self.barrier.wait();
            }
            result
        }
    }
    let f = Fixture::new();
    let (forge, sha) = fork_with_new_head(&f);
    let info = serde_json::from_str(&LeaseRunner::fork_json(&sha, "feat", &forge)).unwrap();
    let runner = ConcurrentFetch {
        jj: &f.runner,
        added: Default::default(),
        barrier: std::sync::Barrier::new(2),
    };
    std::thread::scope(|scope| {
        let resolve = || {
            crate::app::pr_bookmarks::resolve_pr_head(&runner, None, &f.ws, "example/repo", 1, &info, None)
                .unwrap()
                .sha
        };
        let first = scope.spawn(resolve);
        let second = scope.spawn(resolve);
        assert_eq!(first.join().unwrap(), sha);
        assert_eq!(second.join().unwrap(), sha);
    });
    let names = runner.added.lock().unwrap();
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
    assert!(!f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch-"));
}

#[test]
fn published_head_is_collected_after_remote_branch_deletion() {
    let f = Fixture::new();
    let (_, head) = pr_rewritten_outside_this_store(&f, false);
    point_pr_bookmark(&f.runner, None, &f.ws, 1, &head).unwrap();
    crate::app::pr_bookmarks::record_head(&f.ws, 1, &head).unwrap();
    let forge = f._dir.path().join("forge");
    f.jj(&forge, &["bookmark", "delete", "feat"]);
    f.jj(&f.ws, &["git", "fetch", "--remote", "forge"]);
    assert_eq!(f.commit("present(feat@forge)"), "");
    let deleted = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();
    assert!(deleted.contains(&"pr/1".to_string()));
    assert!(f.pr_commits(1).is_empty());
    assert_eq!(f.git_ref(1), None);
}

#[test]
fn forgotten_obsolete_git_ref_is_collected_for_a_merged_pr() {
    let f = Fixture::new();
    let (old, head) = f.rewritten_feat_with_exported_pr_ref();
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    assert_eq!(f.git_ref(1), Some(old));
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"closed","merged":true,"head":{{"ref":"feat","sha":"{head}","repo":null}}}}"#,),
    );
    let deleted = crate::app::gc::gc_workspace_bookmarks(&runner, None, &f.ws, false, false).unwrap();
    assert!(deleted.contains(&"pr/1".to_string()));
    assert!(f.pr_commits(1).is_empty());
    assert_eq!(f.git_ref(1), None);
}

#[test]
fn recorded_published_head_protects_its_predecessor_without_network() {
    let f = Fixture::new();
    let (old, head) = f.rewritten_feat_with_exported_pr_ref();
    crate::app::pr_bookmarks::record_head(&f.ws, 1, &head).unwrap();
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    assert_eq!(f.git_ref(1), Some(old));
    let deleted = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();
    assert!(deleted.contains(&"pr/1".to_string()));
    assert!(f.pr_commits(1).is_empty());
    assert_eq!(f.git_ref(1), None);
}

#[test]
fn forgotten_obsolete_git_ref_is_repointed_for_an_open_pr() {
    let f = Fixture::new();
    let (_, head) = f.rewritten_feat_with_exported_pr_ref();
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}"}}}}"#,),
    );
    let deleted = crate::app::gc::gc_workspace_bookmarks(&runner, None, &f.ws, false, false).unwrap();
    assert!(!deleted.contains(&"pr/1".to_string()));
    assert_eq!(f.pr_commits(1), vec![head.clone()]);
    assert_eq!(f.git_ref(1), Some(head));
}

#[test]
fn unpushed_advance_cannot_license_replacing_unique_work() {
    let f = Fixture::new();
    let head = f.commit("feat");
    f.jj(&f.ws, &["new", "feat", "-m", "unpublished work"]);
    std::fs::write(f.ws.join("unpublished.txt"), "keep me\n").unwrap();
    crate::app::pr::advance_pr_bookmarks(&f.runner, &f.ws, "feat", "pr/1").unwrap();
    let unpublished = f.commit("pr/1");
    assert!(crate::app::pr_bookmarks::recorded_heads(&f.ws, 1).is_empty());
    // Old cube versions recorded local tips here before attempting a push.
    let legacy = crate::lock::shared_repo_dir(&f.ws).unwrap().join("cube-pr-heads");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("1"), format!("{unpublished}\n")).unwrap();
    assert!(crate::app::pr_bookmarks::recorded_heads(&f.ws, 1).is_empty());
    let before = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    let error = point_pr_bookmark(&f.runner, None, &f.ws, 1, &head)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&head) && error.contains(&unpublished), "{error}");
    assert_eq!(f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]), before);
    f.protect_feat_remotely();
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}","repo":{{"full_name":"example/repo"}}}}}}"#,),
    );
    assert!(
        crate::app::workspace_ops::workspace_goto(None, &runner, Some(f.ws.display().to_string()), None, Some(1), None)
            .is_err()
    );
    assert_eq!(
        f.commit("feat"),
        unpublished,
        "goto must refuse before moving the branch"
    );
}

#[test]
fn deleted_fork_does_not_move_the_local_main_bookmark() {
    let f = Fixture::new();
    f.protect_feat_remotely();
    let main = f.commit("main");
    let head = f.commit("feat");
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"main","sha":"{head}","repo":null}}}}"#,),
    );
    crate::app::workspace_ops::workspace_goto(None, &runner, Some(f.ws.display().to_string()), None, Some(1), None)
        .unwrap();
    assert_eq!(f.commit("main"), main);
    assert_eq!(f.commit("@-"), head);
}

/// A final PR head that only the base repository's `refs/pull/<n>/head`
/// still serves: the source branch was deleted on merge and the fork is gone.
fn base_repo_serving_only_a_pull_ref(f: &Fixture) -> String {
    let base = f._dir.path().join("base");
    f.jj(
        f._dir.path(),
        &[
            "git",
            "clone",
            "--colocate",
            f.root.to_str().unwrap(),
            base.to_str().unwrap(),
        ],
    );
    f.jj(&base, &["bookmark", "track", "feat@origin"]);
    f.jj(&base, &["new", "feat", "-m", "squashed final head"]);
    std::fs::write(base.join("final.txt"), "final\n").unwrap();
    f.jj(&base, &["bookmark", "create", "tip", "-r", "@"]);
    let head = f.jj(&base, &["log", "--no-graph", "-r", "tip", "-T", "commit_id"]);
    f.jj(&base, &["new", "main@origin"]);
    // GitHub keeps this ref after the branch is deleted; the fixture creates
    // only loose refs, so write it directly rather than invoking host git.
    let pull = base.join(".git/refs/pull/1");
    std::fs::create_dir_all(&pull).unwrap();
    std::fs::write(pull.join("head"), format!("{head}\n")).unwrap();
    f.jj(&base, &["bookmark", "delete", "tip"]);
    f.jj(&f.ws, &["git", "remote", "add", "origin", base.to_str().unwrap()]);
    head
}

/// Answers GitHub's PR lookup and leaves every other command to real jj/git.
struct PrJsonOnly<'a> {
    jj: &'a DeclaredJj,
    pr_json: String,
}

impl CommandRunner for PrJsonOnly<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.program == "gh" {
            return Ok(self.pr_json.clone());
        }
        self.jj.run(invocation)
    }
}

#[test]
fn merged_pr_ref_is_collected_when_only_the_pull_ref_serves_the_head() {
    let f = Fixture::new();
    let ancestor = f.commit("feat");
    point_pr_bookmark(&f.runner, None, &f.ws, 1, &ancestor).unwrap();
    let head = base_repo_serving_only_a_pull_ref(&f);
    assert_eq!(f.commit(&format!("present({head})")), "", "head must start absent");
    assert_eq!(f.git_ref(1), Some(ancestor));
    // A temporary remote orphaned by a killed run, and one a live fetch may own.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let url = f.root.to_str().unwrap();
    f.jj(&f.ws, &["git", "remote", "add", "cube-pr-fetch-9-100-killed", url]);
    f.jj(
        &f.ws,
        &["git", "remote", "add", &format!("cube-pr-fetch-9-{now}-live"), url],
    );
    // The PR's base repository, as GitHub identifies it. Never contacted:
    // `origin` (the local `base`) is what the pull-ref fetch uses.
    f.jj(
        &f.ws,
        &["git", "remote", "add", "upstream", "https://github.com/example/repo"],
    );
    let runner = PrJsonOnly {
        jj: &f.runner,
        pr_json: format!(
            r#"{{"state":"closed","merged":true,"head":{{"ref":"deleted-branch","sha":"{head}","repo":null}}}}"#
        ),
    };
    let deleted = crate::app::gc::gc_workspace_bookmarks(&runner, None, &f.ws, false, false).unwrap();
    assert!(deleted.contains(&"pr/1".to_string()), "{deleted:?}");
    let remotes = f.jj(&f.ws, &["git", "remote", "list"]);
    assert!(!remotes.contains("killed"), "{remotes}");
    assert!(remotes.contains("-live"), "{remotes}");
    assert!(f.pr_commits(1).is_empty());
    assert_eq!(f.git_ref(1), None);
}

#[test]
fn unresolvable_head_is_not_looked_up_again_on_the_next_sweep() {
    let f = Fixture::new();
    let ancestor = f.commit("feat");
    point_pr_bookmark(&f.runner, None, &f.ws, 1, &ancestor).unwrap();
    f.jj(&f.ws, &["git", "remote", "add", "origin", f.root.to_str().unwrap()]);
    f.jj(
        &f.ws,
        &["git", "remote", "add", "upstream", "https://github.com/example/repo"],
    );
    let runner = PrJsonOnly {
        jj: &f.runner,
        pr_json: r#"{"state":"closed","merged":true,"head":{"ref":"gone","sha":"0123456789abcdef0123456789abcdef01234567","repo":null}}"#
            .to_string(),
    };
    let deleted = crate::app::gc::gc_workspace_bookmarks(&runner, None, &f.ws, false, false).unwrap();
    assert!(!deleted.contains(&"pr/1".to_string()));
    assert!(crate::app::pr_bookmarks::recently_unresolved(&f.ws, 1));
    assert_eq!(f.pr_commits(1), vec![ancestor]);
}
