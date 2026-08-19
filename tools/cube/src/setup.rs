use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::CubeError;
use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::metadata::WorkspaceRecord;
use crate::store::{Store, WorkspaceSetupState};

pub const SETUP_FILE_RELATIVE: &str = ".cube/setup.yaml";
const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SetupConfig {
    pub version: u32,
    #[serde(default)]
    pub steps: Vec<SetupStep>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SetupStep {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub run_when: RunPolicy,
    #[serde(default)]
    pub fingerprint: Vec<FingerprintInput>,
    /// When true, a non-zero exit is recorded as [`StepStatus::Failed`] and
    /// printed as a warning, but does not abort the lease or stop later steps.
    /// Setup state is not persisted, so the step retries on the next lease.
    #[serde(default)]
    pub allow_failure: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunPolicy {
    /// Run only when no successful run has been recorded for this step.
    OnCreate,
    /// Run when the fingerprint of tracked inputs differs from the
    /// last recorded fingerprint, or on first run. This is the default.
    #[default]
    OnFingerprintChange,
    /// Always run, regardless of recorded state.
    Always,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum FingerprintInput {
    File { file: String },
    Value { value: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupReport {
    pub steps: Vec<StepOutcome>,
}

impl SetupReport {
    pub fn empty() -> Self {
        Self { steps: Vec::new() }
    }

    pub fn ran_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Ran))
            .count()
    }

    pub fn skipped_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Skipped { .. }))
            .count()
    }

    pub fn failed_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Failed { .. }))
            .count()
    }

    /// Hard (non-tolerated) failures only. A step that declared
    /// `allow_failure: true` stays in the report as [`StepStatus::Failed`]
    /// but is not a lease-aborting failure.
    pub fn first_failure(&self) -> Option<&StepOutcome> {
        self.steps.iter().find(|step| step.status.is_hard_failure())
    }

    pub fn warning_count(&self) -> usize {
        self.tolerated_failures().count()
    }

    pub fn tolerated_failures(&self) -> impl Iterator<Item = &StepOutcome> {
        self.steps.iter().filter(|step| step.status.is_tolerated_failure())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub id: String,
    #[serde(flatten)]
    pub status: StepStatus,
    pub fingerprint: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StepStatus {
    Ran,
    Skipped {
        reason: SkipReason,
    },
    Failed {
        error: String,
        /// Copied from the step's `allow_failure` declaration. Stays `Failed`
        /// even when tolerated — never rewritten to success or skipped.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_failure: bool,
        /// Underlying command stderr when the runner captured a non-empty
        /// one. Printed verbatim on the tolerated-failure warning path.
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr: Option<String>,
    },
}

impl StepStatus {
    fn failed(error: CubeError, allow_failure: bool) -> Self {
        let stderr = match &error {
            CubeError::CommandFailed { stderr, .. } if !stderr.is_empty() => Some(stderr.clone()),
            _ => None,
        };
        Self::Failed {
            error: error.to_string(),
            allow_failure,
            stderr,
        }
    }

    fn is_hard_failure(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                allow_failure: false,
                ..
            }
        )
    }

    fn is_tolerated_failure(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                allow_failure: true,
                ..
            }
        )
    }
}

impl StepOutcome {
    /// Human-facing warning body for a tolerated failure: the command's
    /// captured stderr when present, otherwise the full error string.
    pub fn warning_detail(&self) -> Option<&str> {
        match &self.status {
            StepStatus::Failed {
                allow_failure: true,
                stderr,
                error,
            } => Some(stderr.as_deref().filter(|s| !s.is_empty()).unwrap_or(error.as_str())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    AlreadyRan,
    FingerprintUnchanged,
}

pub fn setup_config_path(workspace_path: &Path) -> PathBuf {
    workspace_path.join(SETUP_FILE_RELATIVE)
}

pub fn read_setup_config(workspace_path: &Path) -> Result<Option<SetupConfig>, CubeError> {
    let path = setup_config_path(workspace_path);
    let raw = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CubeError::Io(source)),
    };
    let config: SetupConfig = serde_yaml::from_str(&raw)
        .map_err(|source| CubeError::InvalidArgument(format!("failed to parse `{}`: {source}", path.display())))?;
    if config.version != SUPPORTED_VERSION {
        return Err(CubeError::InvalidArgument(format!(
            "unsupported setup config version `{}` in `{}`",
            config.version,
            path.display()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for step in &config.steps {
        if step.id.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "setup step in `{}` is missing an id",
                path.display()
            )));
        }
        if !seen.insert(step.id.clone()) {
            return Err(CubeError::InvalidArgument(format!(
                "duplicate setup step id `{}` in `{}`",
                step.id,
                path.display()
            )));
        }
        if step.command.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "setup step `{}` has an empty command in `{}`",
                step.id,
                path.display()
            )));
        }
    }
    Ok(Some(config))
}

pub fn compute_fingerprint(workspace_path: &Path, step: &SetupStep) -> Result<String, CubeError> {
    let mut hasher = Sha256::new();
    hasher.update(step.command.as_bytes());
    hasher.update([0u8]);
    for input in &step.fingerprint {
        match input {
            FingerprintInput::File { file } => {
                hasher.update(b"file:");
                hasher.update(file.as_bytes());
                hasher.update([0u8]);
                let candidate = workspace_path.join(file);
                match fs::read(&candidate) {
                    Ok(bytes) => hasher.update(&bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        hasher.update(b"<missing>");
                    }
                    Err(source) => return Err(CubeError::Io(source)),
                }
                hasher.update([0u8]);
            }
            FingerprintInput::Value { value } => {
                hasher.update(b"value:");
                hasher.update(value.as_bytes());
                hasher.update([0u8]);
            }
        }
    }
    let digest = hasher.finalize();
    Ok(hex_digest(&digest))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn run_setup_engine(
    store: &Store,
    runner: &dyn CommandRunner,
    workspace: &WorkspaceRecord,
    config: &SetupConfig,
    now_epoch_s: i64,
) -> Result<SetupReport, CubeError> {
    let source_path = store.get_repo(&workspace.repo)?.and_then(|r| r.source);
    let mut step_env = vec![(
        "CUBE_WORKSPACE".to_string(),
        workspace.workspace_path.display().to_string(),
    )];
    if let Some(ref source) = source_path {
        step_env.push(("CUBE_BASE_REPO".to_string(), source.display().to_string()));
    }

    let mut report = SetupReport::empty();
    for step in &config.steps {
        let started = Instant::now();
        let fingerprint = compute_fingerprint(&workspace.workspace_path, step)?;
        let stored = store.get_workspace_setup_state(&workspace.repo, &workspace.workspace_id, &step.id)?;
        let action = decide_action(step.run_when, stored.as_ref(), &fingerprint);
        match action {
            StepAction::Skip(reason) => {
                report.steps.push(StepOutcome {
                    id: step.id.clone(),
                    status: StepStatus::Skipped { reason },
                    fingerprint,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
            StepAction::Run => match invoke_step(runner, &workspace.workspace_path, step, &step_env) {
                Ok(()) => {
                    store.upsert_workspace_setup_state(&WorkspaceSetupState {
                        repo: workspace.repo.clone(),
                        workspace_id: workspace.workspace_id.clone(),
                        step_id: step.id.clone(),
                        fingerprint: fingerprint.clone(),
                        last_run_epoch_s: now_epoch_s,
                    })?;
                    report.steps.push(StepOutcome {
                        id: step.id.clone(),
                        status: StepStatus::Ran,
                        fingerprint,
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                }
                Err(error) => {
                    let duration_ms = started.elapsed().as_millis() as u64;
                    let allow_failure = step.allow_failure;
                    report.steps.push(StepOutcome {
                        id: step.id.clone(),
                        status: StepStatus::failed(error, allow_failure),
                        fingerprint,
                        duration_ms,
                    });
                    if allow_failure {
                        // Tolerated: leave remaining steps to run, and do
                        // not persist workspace_setup state so this step
                        // retries on the next lease.
                        continue;
                    }
                    // Stop on first hard failure: subsequent steps may
                    // depend on this one having run.
                    return Ok(report);
                }
            },
        }
    }
    Ok(report)
}

#[derive(Debug)]
enum StepAction {
    Run,
    Skip(SkipReason),
}

fn decide_action(policy: RunPolicy, stored: Option<&WorkspaceSetupState>, fingerprint: &str) -> StepAction {
    match policy {
        RunPolicy::Always => StepAction::Run,
        RunPolicy::OnCreate => match stored {
            Some(_) => StepAction::Skip(SkipReason::AlreadyRan),
            None => StepAction::Run,
        },
        RunPolicy::OnFingerprintChange => match stored {
            Some(state) if state.fingerprint == fingerprint => StepAction::Skip(SkipReason::FingerprintUnchanged),
            _ => StepAction::Run,
        },
    }
}

fn invoke_step(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
    step: &SetupStep,
    extra_env: &[(String, String)],
) -> Result<(), CubeError> {
    // Run setup commands through a shell so that env var references like
    // $CUBE_BASE_REPO are expanded by the shell before the program executes.
    runner.run(&CommandInvocation {
        cwd: workspace_path.to_path_buf(),
        program: "sh".to_string(),
        args: vec!["-c".to_string(), step.command.clone()],
        env: extra_env.to_vec(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{
        FingerprintInput, RunPolicy, SetupConfig, SetupStep, compute_fingerprint, decide_action, read_setup_config,
        setup_config_path,
    };
    use crate::store::WorkspaceSetupState;

    use super::{SkipReason, StepAction};

    #[test]
    fn read_setup_config_returns_none_when_file_missing() {
        let temp = TempDir::new().unwrap();
        assert!(read_setup_config(temp.path()).unwrap().is_none());
    }

    #[test]
    fn read_setup_config_parses_full_example() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(setup_config_path(temp.path()).parent().unwrap()).unwrap();
        fs::write(
            setup_config_path(temp.path()),
            r#"version: 1
steps:
  - id: secrets
    command: ./tools/dev/decode-secrets.sh
    run_when: on-create
  - id: deps
    command: pnpm install --frozen-lockfile
    fingerprint:
      - file: pnpm-lock.yaml
      - value: v3
"#,
        )
        .unwrap();

        let config = read_setup_config(temp.path()).unwrap().unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].id, "secrets");
        assert_eq!(config.steps[0].run_when, RunPolicy::OnCreate);
        assert!(!config.steps[0].allow_failure, "allow_failure defaults to false");
        assert_eq!(config.steps[1].id, "deps");
        assert_eq!(config.steps[1].run_when, RunPolicy::OnFingerprintChange);
        assert!(!config.steps[1].allow_failure);
        assert_eq!(
            config.steps[1].fingerprint,
            vec![
                FingerprintInput::File {
                    file: "pnpm-lock.yaml".to_string(),
                },
                FingerprintInput::Value {
                    value: "v3".to_string(),
                },
            ]
        );
    }

    #[test]
    fn read_setup_config_rejects_duplicate_ids() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(setup_config_path(temp.path()).parent().unwrap()).unwrap();
        fs::write(
            setup_config_path(temp.path()),
            r#"version: 1
steps:
  - id: deps
    command: a
  - id: deps
    command: b
"#,
        )
        .unwrap();
        let err = read_setup_config(temp.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate setup step id"));
    }

    #[test]
    fn fingerprint_changes_when_file_contents_change() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("lock"), b"a").unwrap();
        let step = SetupStep {
            id: "deps".to_string(),
            command: "echo".to_string(),
            run_when: RunPolicy::OnFingerprintChange,
            fingerprint: vec![FingerprintInput::File {
                file: "lock".to_string(),
            }],
            allow_failure: false,
        };
        let first = compute_fingerprint(temp.path(), &step).unwrap();

        fs::write(temp.path().join("lock"), b"b").unwrap();
        let second = compute_fingerprint(temp.path(), &step).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn fingerprint_changes_when_command_changes() {
        let temp = TempDir::new().unwrap();
        let mut step = SetupStep {
            id: "deps".to_string(),
            command: "a".to_string(),
            run_when: RunPolicy::OnFingerprintChange,
            fingerprint: vec![],
            allow_failure: false,
        };
        let first = compute_fingerprint(temp.path(), &step).unwrap();
        step.command = "b".to_string();
        let second = compute_fingerprint(temp.path(), &step).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn missing_fingerprint_file_is_distinct_from_empty_file() {
        let temp = TempDir::new().unwrap();
        let step = SetupStep {
            id: "deps".to_string(),
            command: "echo".to_string(),
            run_when: RunPolicy::OnFingerprintChange,
            fingerprint: vec![FingerprintInput::File {
                file: "lock".to_string(),
            }],
            allow_failure: false,
        };
        let missing = compute_fingerprint(temp.path(), &step).unwrap();

        fs::write(temp.path().join("lock"), b"").unwrap();
        let empty = compute_fingerprint(temp.path(), &step).unwrap();
        assert_ne!(missing, empty);
    }

    fn stored(fingerprint: &str) -> WorkspaceSetupState {
        WorkspaceSetupState {
            repo: "mono".to_string(),
            workspace_id: "mono-agent-001".to_string(),
            step_id: "deps".to_string(),
            fingerprint: fingerprint.to_string(),
            last_run_epoch_s: 0,
        }
    }

    #[test]
    fn decide_action_on_create_runs_only_on_first_run() {
        assert!(matches!(
            decide_action(RunPolicy::OnCreate, None, "abc"),
            StepAction::Run
        ));
        let ran = stored("abc");
        assert!(matches!(
            decide_action(RunPolicy::OnCreate, Some(&ran), "abc"),
            StepAction::Skip(SkipReason::AlreadyRan)
        ));
        // Even with a different fingerprint, on-create stays skipped after first run.
        assert!(matches!(
            decide_action(RunPolicy::OnCreate, Some(&ran), "different"),
            StepAction::Skip(SkipReason::AlreadyRan)
        ));
    }

    #[test]
    fn decide_action_on_fingerprint_change_runs_when_changed_or_unset() {
        assert!(matches!(
            decide_action(RunPolicy::OnFingerprintChange, None, "abc"),
            StepAction::Run
        ));
        let ran = stored("abc");
        assert!(matches!(
            decide_action(RunPolicy::OnFingerprintChange, Some(&ran), "abc"),
            StepAction::Skip(SkipReason::FingerprintUnchanged)
        ));
        assert!(matches!(
            decide_action(RunPolicy::OnFingerprintChange, Some(&ran), "different"),
            StepAction::Run
        ));
    }

    #[test]
    fn decide_action_always_runs() {
        let ran = stored("abc");
        assert!(matches!(decide_action(RunPolicy::Always, None, "abc"), StepAction::Run));
        assert!(matches!(
            decide_action(RunPolicy::Always, Some(&ran), "abc"),
            StepAction::Run
        ));
    }

    #[test]
    fn config_steps_default_to_on_fingerprint_change() {
        let raw = r#"version: 1
steps:
  - id: deps
    command: pnpm install
"#;
        let parsed: SetupConfig = serde_yaml::from_str(raw).unwrap();
        assert_eq!(parsed.steps[0].run_when, RunPolicy::OnFingerprintChange);
        assert!(
            !parsed.steps[0].allow_failure,
            "allow_failure defaults to false when omitted"
        );
    }

    #[test]
    fn allow_failure_parses_true() {
        let raw = r#"version: 1
steps:
  - id: copy-config
    command: cp missing dest
    allow_failure: true
"#;
        let parsed: SetupConfig = serde_yaml::from_str(raw).unwrap();
        assert!(parsed.steps[0].allow_failure);
    }

    // ── env-var injection tests ──────────────────────────────────────────────

    use super::run_setup_engine;
    use crate::app::CubeError;
    use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
    use crate::metadata::{RepoRecord, WorkspaceRecord, WorkspaceState};
    use crate::store::Store;
    use std::cell::RefCell;

    struct CapturingRunner {
        invocations: RefCell<Vec<CommandInvocation>>,
    }

    impl CapturingRunner {
        fn new() -> Self {
            Self {
                invocations: RefCell::new(vec![]),
            }
        }
        fn into_invocations(self) -> Vec<CommandInvocation> {
            self.invocations.into_inner()
        }
    }

    impl CommandRunner for CapturingRunner {
        fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
            self.invocations.borrow_mut().push(invocation.clone());
            Ok(String::new())
        }
    }

    fn make_store_with_repo(tmp: &TempDir, source: Option<&std::path::Path>) -> Store {
        let mut store = Store::open_at(tmp.path().join("state.db")).unwrap();
        let ws_root = tmp.path().join("workspaces");
        std::fs::create_dir_all(&ws_root).unwrap();
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:org/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: ws_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: source.map(|p| p.to_path_buf()),
                clone_command: None,
            })
            .unwrap();
        let ws_path = ws_root.join("mono-agent-001");
        std::fs::create_dir_all(&ws_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[crate::metadata::WorkspaceCandidate {
                    workspace_id: "mono-agent-001".to_string(),
                    workspace_path: ws_path,
                }],
            )
            .unwrap();
        store
    }

    fn workspace_record(tmp: &TempDir) -> WorkspaceRecord {
        WorkspaceRecord {
            repo: "mono".to_string(),
            workspace_id: "mono-agent-001".to_string(),
            workspace_path: tmp.path().join("workspaces/mono-agent-001"),
            state: WorkspaceState::Free,
            lease_id: None,
            holder: None,
            task: None,
            leased_at_epoch_s: None,
            lease_expires_at_epoch_s: None,
            head_commit: None,
            last_release_reason: None,
            health_status: None,
            unhealthy_since_epoch_s: None,
            last_holder: None,
            last_task: None,
            last_activity_at_epoch_s: None,
        }
    }

    fn one_step_config() -> SetupConfig {
        serde_yaml::from_str(
            r#"version: 1
steps:
  - id: copy-config
    command: echo hello
    run_when: always
"#,
        )
        .unwrap()
    }

    #[test]
    fn setup_engine_injects_cube_workspace_always() {
        let tmp = TempDir::new().unwrap();
        let store = make_store_with_repo(&tmp, None);
        let ws = workspace_record(&tmp);
        let runner = CapturingRunner::new();

        run_setup_engine(&store, &runner, &ws, &one_step_config(), 0).unwrap();

        let invocations = runner.into_invocations();
        assert_eq!(invocations.len(), 1);
        let env: std::collections::HashMap<_, _> = invocations[0].env.iter().cloned().collect();
        assert_eq!(
            env.get("CUBE_WORKSPACE").map(String::as_str),
            Some(ws.workspace_path.display().to_string()).as_deref()
        );
        assert!(
            !env.contains_key("CUBE_BASE_REPO"),
            "CUBE_BASE_REPO should be absent when source_path is None"
        );
    }

    #[test]
    fn setup_engine_injects_cube_base_repo_when_source_path_present() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let store = make_store_with_repo(&tmp, Some(&source_dir));
        let ws = workspace_record(&tmp);
        let runner = CapturingRunner::new();

        run_setup_engine(&store, &runner, &ws, &one_step_config(), 0).unwrap();

        let invocations = runner.into_invocations();
        assert_eq!(invocations.len(), 1);
        let env: std::collections::HashMap<_, _> = invocations[0].env.iter().cloned().collect();
        assert_eq!(
            env.get("CUBE_BASE_REPO").map(String::as_str),
            Some(source_dir.display().to_string()).as_deref()
        );
        assert!(env.contains_key("CUBE_WORKSPACE"));
    }

    #[test]
    fn setup_engine_expands_cube_base_repo_in_command() {
        let tmp = TempDir::new().unwrap();

        // Create a source directory with a file that should be copied.
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("config.toml"), b"secret").unwrap();

        let store = make_store_with_repo(&tmp, Some(&source_dir));
        let ws = workspace_record(&tmp);

        // Command references $CUBE_BASE_REPO — must be expanded by the shell.
        let config: SetupConfig = serde_yaml::from_str(
            r#"version: 1
steps:
  - id: copy-config
    command: 'cp "$CUBE_BASE_REPO/config.toml" config.toml'
    run_when: always
"#,
        )
        .unwrap();

        let runner = RealCommandRunner;
        run_setup_engine(&store, &runner, &ws, &config, 0).unwrap();

        // Assert the file was actually copied, proving $CUBE_BASE_REPO expanded.
        let dest = ws.workspace_path.join("config.toml");
        assert!(
            dest.exists(),
            "config.toml should have been copied from $CUBE_BASE_REPO"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"secret");
    }

    struct ScriptedRunner {
        results: RefCell<std::collections::VecDeque<Result<String, CubeError>>>,
        invocations: RefCell<Vec<CommandInvocation>>,
    }

    impl ScriptedRunner {
        fn new(results: Vec<Result<String, CubeError>>) -> Self {
            Self {
                results: RefCell::new(results.into()),
                invocations: RefCell::new(vec![]),
            }
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
            self.invocations.borrow_mut().push(invocation.clone());
            self.results
                .borrow_mut()
                .pop_front()
                .expect("unexpected extra setup invocation")
        }
    }

    fn command_failed(stderr: &str) -> CubeError {
        CubeError::CommandFailed {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "cmd".to_string()],
            status: Some(1),
            stderr: stderr.to_string(),
        }
    }

    fn two_step_config(first_allow_failure: bool) -> SetupConfig {
        let allow = if first_allow_failure { "true" } else { "false" };
        serde_yaml::from_str(&format!(
            r#"version: 1
steps:
  - id: copy-config
    command: cp missing dest
    run_when: on-create
    allow_failure: {allow}
  - id: after
    command: echo ok
    run_when: always
"#
        ))
        .unwrap()
    }

    #[test]
    fn first_failure_skips_tolerated_failures() {
        use super::{SetupReport, StepOutcome, StepStatus};
        let report = SetupReport {
            steps: vec![
                StepOutcome {
                    id: "copy-config".to_string(),
                    status: StepStatus::Failed {
                        error: "boom".to_string(),
                        allow_failure: true,
                        stderr: Some("missing".to_string()),
                    },
                    fingerprint: "abc".to_string(),
                    duration_ms: 1,
                },
                StepOutcome {
                    id: "after".to_string(),
                    status: StepStatus::Ran,
                    fingerprint: "def".to_string(),
                    duration_ms: 1,
                },
            ],
        };
        assert!(report.first_failure().is_none());
        assert_eq!(report.warning_count(), 1);
        assert_eq!(report.steps[0].warning_detail(), Some("missing"));
    }

    #[test]
    fn first_failure_still_reports_hard_failures() {
        use super::{SetupReport, StepOutcome, StepStatus};
        let report = SetupReport {
            steps: vec![StepOutcome {
                id: "deps".to_string(),
                status: StepStatus::Failed {
                    error: "boom".to_string(),
                    allow_failure: false,
                    stderr: Some("pnpm exploded".to_string()),
                },
                fingerprint: "abc".to_string(),
                duration_ms: 1,
            }],
        };
        let failure = report.first_failure().expect("hard failure");
        assert_eq!(failure.id, "deps");
        assert_eq!(report.warning_count(), 0);
    }

    #[test]
    fn tolerated_failure_continues_and_does_not_persist() {
        let tmp = TempDir::new().unwrap();
        let store = make_store_with_repo(&tmp, None);
        let ws = workspace_record(&tmp);
        let runner = ScriptedRunner::new(vec![
            Err(command_failed("cp: missing: No such file or directory")),
            Ok(String::new()),
        ]);

        let report = run_setup_engine(&store, &runner, &ws, &two_step_config(true), 0).unwrap();

        assert!(report.first_failure().is_none());
        assert_eq!(report.steps.len(), 2);
        assert!(
            report.steps[0].status.is_tolerated_failure(),
            "tolerated failure stays Failed, not rewritten"
        );
        assert!(matches!(report.steps[1].status, super::StepStatus::Ran));
        assert_eq!(runner.invocations.borrow().len(), 2, "later step still ran");
        assert!(
            store
                .get_workspace_setup_state("mono", "mono-agent-001", "copy-config")
                .unwrap()
                .is_none(),
            "tolerated failure must not persist setup state"
        );
        assert!(
            store
                .get_workspace_setup_state("mono", "mono-agent-001", "after")
                .unwrap()
                .is_some(),
            "successful later step still persists"
        );
    }

    #[test]
    fn hard_failure_still_stops_later_steps() {
        let tmp = TempDir::new().unwrap();
        let store = make_store_with_repo(&tmp, None);
        let ws = workspace_record(&tmp);
        let runner = ScriptedRunner::new(vec![Err(command_failed("boom"))]);

        let report = run_setup_engine(&store, &runner, &ws, &two_step_config(false), 0).unwrap();

        let failure = report.first_failure().expect("hard failure");
        assert_eq!(failure.id, "copy-config");
        assert_eq!(report.steps.len(), 1, "later steps must not run");
        assert_eq!(runner.invocations.borrow().len(), 1);
        assert!(
            store
                .get_workspace_setup_state("mono", "mono-agent-001", "copy-config")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn tolerated_failure_retries_on_next_run() {
        let tmp = TempDir::new().unwrap();
        let store = make_store_with_repo(&tmp, None);
        let ws = workspace_record(&tmp);
        let config = two_step_config(true);

        let first = ScriptedRunner::new(vec![
            Err(command_failed("cp: missing: No such file or directory")),
            Ok(String::new()),
        ]);
        run_setup_engine(&store, &first, &ws, &config, 0).unwrap();
        assert_eq!(first.invocations.borrow().len(), 2);

        // No state for the tolerated step, so on-create runs again. The
        // later `always` step also runs.
        let second = ScriptedRunner::new(vec![Ok(String::new()), Ok(String::new())]);
        let report = run_setup_engine(&store, &second, &ws, &config, 1).unwrap();
        assert_eq!(second.invocations.borrow().len(), 2);
        assert!(matches!(report.steps[0].status, super::StepStatus::Ran));
        assert!(
            store
                .get_workspace_setup_state("mono", "mono-agent-001", "copy-config")
                .unwrap()
                .is_some(),
            "a later success must persist so on-create can skip afterwards"
        );
    }

    #[test]
    fn tolerated_missing_file_copy_surfaces_real_stderr() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let store = make_store_with_repo(&tmp, Some(&source_dir));
        let ws = workspace_record(&tmp);
        let config: SetupConfig = serde_yaml::from_str(
            r#"version: 1
steps:
  - id: copy-config
    command: 'cp "$CUBE_BASE_REPO/config.toml" config.toml'
    run_when: on-create
    allow_failure: true
  - id: after
    command: 'touch ran-after'
    run_when: always
"#,
        )
        .unwrap();

        let report = run_setup_engine(&store, &RealCommandRunner, &ws, &config, 0).unwrap();
        assert!(report.first_failure().is_none());
        assert_eq!(report.steps.len(), 2);
        assert!(report.steps[0].status.is_tolerated_failure());
        assert!(matches!(report.steps[1].status, super::StepStatus::Ran));
        let detail = report.steps[0].warning_detail().expect("warning detail");
        assert!(
            detail.contains("No such file") || detail.contains("config.toml"),
            "real cp stderr should name the missing path, got: {detail}"
        );
        assert!(ws.workspace_path.join("ran-after").exists());
        assert!(
            store
                .get_workspace_setup_state("mono", "mono-agent-001", "copy-config")
                .unwrap()
                .is_none()
        );
    }
}
