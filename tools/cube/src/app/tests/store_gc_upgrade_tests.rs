//! Upgrade from an already collected legacy store, including its primary.
use super::*;

#[test]
fn legacy_collection_primary_is_repaired_before_staged_collection() {
    if !staged_jj() {
        return;
    }
    for manual_refresh in [true, false] {
        let mut f = Fixture::with_jj("CUBE_OLDEST_JJ_BIN", "ws");
        for description in ["one", "two", "three"] {
            f.jj(&f.ws, &["describe", "-m", description]);
        }
        let database = f._dir.path().join("state.db");
        let legacy = runner(&f);
        let report = staged_collect(&f, &database, &legacy).unwrap();
        assert_eq!(report.collection, Collection::Immediate);
        f.runner.binary = std::fs::canonicalize(std::env::var("BOSS_JJ_BIN").unwrap()).unwrap();
        let current = runner(&f);
        let status = RealCommandRunner::invocation(&f.root, "jj", &["status"]);
        let error = current.run(&status).unwrap_err();
        assert!(
            crate::app::jj::jj_update_stale_recovery_kind(&error).is_some(),
            "{error}"
        );
        Store::open_at(&database)
            .unwrap()
            .set_pool_metadata_i(&crate::app::store_gc_integrity::oldest_client_key("scratch"), 46)
            .unwrap();
        if manual_refresh {
            // The upgrade runbook requires this before restarting the engine.
            f.jj(&f.root, &["workspace", "update-stale"]);
            f.jj(
                &f.root,
                &["workspace", "add", f._dir.path().join("fresh").to_str().unwrap()],
            );
        } else {
            // No runbook refresh and no staged collection yet. Provisioning
            // must recover the stale primary itself.
            let provisioned = crate::app::provision::auto_create_workspace(
                &current,
                Some(&database),
                &staged_record(&f),
                &[],
                &mut None,
            )
            .expect("provision recovers a stale primary left by 0.26 collection");
            assert!(
                provisioned.workspace_path.join(".jj").exists(),
                "provisioned workspace at {} must have .jj",
                provisioned.workspace_path.display()
            );
        }
        let report = staged_collect(&f, &database, &current).unwrap();
        assert_eq!(report.collection, Collection::Immediate);
        current.run(&status).unwrap();
        f.jj(
            &f.root,
            &["workspace", "add", f._dir.path().join("after-gc").to_str().unwrap()],
        );
    }
}

#[test]
fn scratch_rewrites_do_not_leave_repository_configuration() {
    if !staged_jj() {
        return;
    }
    let (f, database) = staged_fixture();
    struct Isolated<'a> {
        inner: Hooked<'a>,
        config_home: PathBuf,
    }
    impl CommandRunner for Isolated<'_> {
        fn run(&self, invocation: &CommandInvocation) -> Result<String> {
            let mut invocation = invocation.clone();
            for name in ["HOME", "XDG_CONFIG_HOME"] {
                if !invocation.env.iter().any(|(key, _)| key == name) {
                    invocation
                        .env
                        .push((name.into(), self.config_home.display().to_string()));
                }
            }
            self.inner.run(&invocation)
        }
    }
    let isolated = Isolated {
        inner: Hooked::new(&f),
        config_home: f._dir.path().join("config-home"),
    };
    isolated
        .run(&RealCommandRunner::invocation(
            &f.root,
            "jj",
            &["config", "set", "--repo", "user.name", "Cube Test"],
        ))
        .unwrap();
    let configs = isolated.config_home.join("jj/repos");
    let entries = || {
        let mut names: Vec<_> = std::fs::read_dir(&configs)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = entries();
    assert_eq!(before.len(), 1);
    staged_collect(&f, &database, &isolated).unwrap();
    assert_eq!(entries(), before);
}
