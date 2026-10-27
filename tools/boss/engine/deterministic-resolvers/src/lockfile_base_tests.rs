use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::command::{CommandOutput, CommandRunner};
use crate::{BazelModuleLockResolver, ConflictedFile, RegistryResolution, ResolverRegistry};

struct SeedThenRegenerate {
    calls: Mutex<usize>,
    fail_regeneration: bool,
}

#[async_trait]
impl CommandRunner for SeedThenRegenerate {
    async fn run(&self, program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        let cwd = cwd.unwrap();
        let args: Vec<_> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        if *calls == 1 {
            assert_eq!(program.file_name().unwrap(), "jj");
            assert_eq!(args, ["file", "show", "-r", "main@origin", "--", "MODULE.bazel.lock"]);
            return Ok(CommandOutput {
                success: true,
                code: Some(0),
                stdout: "base lockfile\n".into(),
                stderr: String::new(),
            });
        }
        assert_eq!(program, Path::new("bazel"));
        assert_eq!(args, ["mod", "deps", "--lockfile_mode=update"]);
        assert_eq!(
            std::fs::read_to_string(cwd.join("MODULE.bazel.lock"))?,
            "base lockfile\n"
        );
        assert_eq!(std::fs::read_to_string(cwd.join("MODULE.bazel"))?, "merged inputs\n");
        if !self.fail_regeneration {
            std::fs::write(cwd.join("MODULE.bazel.lock"), "regenerated from merged inputs\n")?;
        }
        Ok(CommandOutput {
            success: !self.fail_regeneration,
            code: Some(i32::from(self.fail_regeneration)),
            stdout: String::new(),
            stderr: "tool failure".into(),
        })
    }
}

#[tokio::test]
async fn lockfile_only_conflict_uses_base_then_regenerates_and_never_accepts_tool_failure() {
    for fail_regeneration in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("MODULE.bazel"), "merged inputs\n").unwrap();
        std::fs::write(workspace.path().join("MODULE.bazel.lock"), "<<<<<<< conflict\n").unwrap();
        let runner = Arc::new(SeedThenRegenerate {
            calls: Mutex::new(0),
            fail_regeneration,
        });
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(
            BazelModuleLockResolver::with_runner(runner.clone()).with_base_revision("main@origin".into()),
        ));
        let result = registry
            .resolve_all(
                workspace.path(),
                &[ConflictedFile {
                    path: "MODULE.bazel.lock".into(),
                    marker_count: Some(1),
                    shape: "content".into(),
                }],
            )
            .await;
        assert_eq!(*runner.calls.lock().unwrap(), 2);
        if fail_regeneration {
            assert!(
                matches!(result, RegistryResolution::Declined { failed, .. } if failed.len() == 1 && failed[0].reason.contains("tool failure"))
            );
        } else {
            assert!(matches!(result, RegistryResolution::AllResolved(files) if files.len() == 1));
            assert_eq!(
                std::fs::read_to_string(workspace.path().join("MODULE.bazel.lock")).unwrap(),
                "regenerated from merged inputs\n"
            );
        }
    }
}

#[tokio::test]
async fn failed_base_read_preserves_conflict_and_does_not_run_regeneration() {
    use crate::DeterministicResolver;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("MODULE.bazel"), "merged inputs\n").unwrap();
    std::fs::write(workspace.path().join("MODULE.bazel.lock"), "<<<<<<< conflict\n").unwrap();
    let runner = Arc::new(crate::command::FakeCommandRunner::failure("base unavailable"));
    let resolver = BazelModuleLockResolver::with_runner(runner.clone()).with_base_revision("main@origin".into());
    let outcome = resolver
        .resolve(
            workspace.path(),
            &ConflictedFile {
                path: "MODULE.bazel.lock".into(),
                marker_count: Some(1),
                shape: "content".into(),
            },
        )
        .await;
    assert!(matches!(outcome, crate::ResolveOutcome::Failed { reason } if reason.contains("base unavailable")));
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("MODULE.bazel.lock")).unwrap(),
        "<<<<<<< conflict\n"
    );
}
