//! Shared test fixture for crates that need an [`AgentDriver`] stand-in
//! without a second real driver implementation. Unconditionally compiled
//! (not `#[cfg(test)]`) so downstream crates can depend on it from their own
//! `[dev-dependencies]`; this crate's own unit tests use the same fixture.

use std::path::Path;

use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{NormalizeError, WorkerEvent};

use super::{
    AgentDriver, CapabilitySet, DriverDescriptor, DriverRuntimeState, ModelMenu, PermissionArtifacts, PermissionInput,
    PostHocInterceptionFn, ProgressFidelity, ProgressIngress, ProgressObservationConfig, SpawnPlan, SpawnRequest,
    ToolUseInterceptionConfig, ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass, WorkerProcessLifetime,
};

/// RAII override of [`crate::codex::CODEX_HOMES_ROOT_ENV`], obtained
/// from [`codex_homes_override`].
///
/// Holds [`crate::codex::CODEX_HOMES_ENV_TEST_LOCK`] for its whole
/// lifetime and restores the prior value of the variable on drop, so
/// two tests in the same binary cannot interleave their overrides.
/// That coupling is the point: the lock and the `set_var` are acquired
/// together by construction, so no call site can set the variable
/// while forgetting the lock.
pub struct CodexHomesOverride {
    /// Dropped last-but-one; releasing it after the restore below is
    /// what makes the whole set/restore pair atomic w.r.t. other tests.
    _lock: std::sync::MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
}

/// Point per-run `CODEX_HOME` resolution at `root` for as long as the
/// returned guard lives.
///
/// Every test in every crate that needs a disposable homes root goes
/// through here. Because the guard owns a `MutexGuard`, holding it
/// across an `.await` would trip `clippy::await_holding_lock`; tests
/// that need async work under the override drive their own
/// current-thread runtime with `block_on` instead, which keeps the
/// guard inside a single blocking call on a runtime the test owns.
pub fn codex_homes_override(root: &Path) -> CodexHomesOverride {
    let lock = crate::codex::CODEX_HOMES_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prior = std::env::var_os(crate::codex::CODEX_HOMES_ROOT_ENV);
    // SAFETY: `lock` is held for the lifetime of the returned guard and
    // is the process-wide gate on this key, so no other thread is
    // reading or writing it concurrently. The prior value is restored
    // in `Drop` before the lock is released.
    unsafe { std::env::set_var(crate::codex::CODEX_HOMES_ROOT_ENV, root) };
    CodexHomesOverride { _lock: lock, prior }
}

impl Drop for CodexHomesOverride {
    fn drop(&mut self) {
        // SAFETY: same as in `codex_homes_override` — the lock this
        // guard owns is still held, and is released only after this
        // body returns.
        match self.prior.as_ref() {
            Some(value) => unsafe { std::env::set_var(crate::codex::CODEX_HOMES_ROOT_ENV, value) },
            None => unsafe { std::env::remove_var(crate::codex::CODEX_HOMES_ROOT_ENV) },
        }
    }
}

/// RAII override of [`crate::grok::GROK_HOMES_ROOT_ENV`], obtained from
/// [`grok_homes_override`]. Mirrors [`CodexHomesOverride`] /
/// [`codex_homes_override`] above — see those docs for the
/// lock-and-set-together rationale.
pub struct GrokHomesOverride {
    _lock: std::sync::MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
}

/// Point per-run `GROK_HOME` resolution at `root` for as long as the
/// returned guard lives. Mirrors [`codex_homes_override`].
pub fn grok_homes_override(root: &Path) -> GrokHomesOverride {
    let lock = crate::grok::GROK_HOMES_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prior = std::env::var_os(crate::grok::GROK_HOMES_ROOT_ENV);
    // SAFETY: `lock` is held for the lifetime of the returned guard and
    // is the process-wide gate on this key, so no other thread is
    // reading or writing it concurrently. The prior value is restored
    // in `Drop` before the lock is released.
    unsafe { std::env::set_var(crate::grok::GROK_HOMES_ROOT_ENV, root) };
    GrokHomesOverride { _lock: lock, prior }
}

/// RAII override of [`crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV`],
/// obtained from [`transcript_store_override`].
///
/// The guard serializes every test that changes the process-wide transcript
/// store root, including tests that temporarily clear it to exercise the
/// production default. Always acquire this guard after any homes override;
/// it is the innermost environment lock.
pub struct TranscriptStoreOverride {
    _lock: std::sync::MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
}

static TRANSCRIPT_STORE_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point durable worker transcript storage at `root` for as long as the
/// returned guard lives. Always call this after acquiring any homes override,
/// so the transcript-store lock remains innermost.
pub fn transcript_store_override(root: &Path) -> TranscriptStoreOverride {
    TranscriptStoreOverride::set(Some(root))
}

impl TranscriptStoreOverride {
    /// Temporarily clear the transcript-store override while retaining the
    /// shared environment lock. This is used to test the production default.
    pub fn unset() -> Self {
        Self::set(None)
    }

    fn set(root: Option<&Path>) -> Self {
        let lock = TRANSCRIPT_STORE_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prior = std::env::var_os(crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV);
        // SAFETY: the process-wide lock above is held until this guard restores the variable.
        match root {
            Some(root) => unsafe { std::env::set_var(crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV, root) },
            None => unsafe { std::env::remove_var(crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV) },
        }
        Self { _lock: lock, prior }
    }
}

impl Drop for TranscriptStoreOverride {
    fn drop(&mut self) {
        // SAFETY: this guard still holds the process-wide lock.
        match self.prior.as_ref() {
            Some(value) => unsafe { std::env::set_var(crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV, value) },
            None => unsafe { std::env::remove_var(crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV) },
        }
    }
}

impl Drop for GrokHomesOverride {
    fn drop(&mut self) {
        // SAFETY: same as in `grok_homes_override` — the lock this guard
        // owns is still held, and is released only after this body
        // returns.
        match self.prior.as_ref() {
            Some(value) => unsafe { std::env::set_var(crate::grok::GROK_HOMES_ROOT_ENV, value) },
            None => unsafe { std::env::remove_var(crate::grok::GROK_HOMES_ROOT_ENV) },
        }
    }
}

/// RAII override of `HOME`, obtained from [`home_override`].
///
/// Holds [`crate::grok::GROK_HOMES_ENV_TEST_LOCK`] for its whole lifetime
/// and restores the prior value of the variable on drop, so tests that
/// redirect any driver home cannot interleave their process-global changes.
pub struct HomeOverride {
    _lock: std::sync::MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
}

/// Point `HOME` at `home` for as long as the returned guard lives.
pub fn home_override(home: &Path) -> HomeOverride {
    let lock = crate::grok::GROK_HOMES_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prior = std::env::var_os("HOME");
    // SAFETY: `lock` is held for the lifetime of the returned guard and
    // is the process-wide gate on this key, so no other thread is
    // reading or writing it concurrently. The prior value is restored
    // in `Drop` before the lock is released.
    unsafe { std::env::set_var("HOME", home) };
    HomeOverride { _lock: lock, prior }
}

impl Drop for HomeOverride {
    fn drop(&mut self) {
        match self.prior.as_ref() {
            // SAFETY: this override still owns the shared environment lock.
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            // SAFETY: this override still owns the shared environment lock.
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

/// RAII override of both [`crate::codex::CODEX_HOMES_ROOT_ENV`] and
/// [`crate::driver::codex::CODEX_AUTH_SOURCE_ENV`], obtained from
/// [`codex_auth_source_override`].
///
/// `CODEX_AUTH_SOURCE_ENV` rides on the same
/// [`crate::codex::CODEX_HOMES_ENV_TEST_LOCK`] a bare homes override
/// takes (see that type's doc comment): both env vars gate the same
/// per-run Codex-home resolution, so one lock, taken once, covers both.
/// This type exists so a test that needs to point *both* at a temp tree
/// does it through one call instead of hand-rolling the set/restore pair
/// at each call site.
pub struct CodexAuthSourceOverride {
    auth_prior: Option<std::ffi::OsString>,
    _homes: CodexHomesOverride,
}

/// Point per-run `CODEX_HOME` resolution at `homes_root` and the operator
/// auth snapshot at `auth_source`, both for as long as the returned guard
/// lives. See [`codex_homes_override`] for the locking/async caveats,
/// which apply identically here.
pub fn codex_auth_source_override(homes_root: &Path, auth_source: &Path) -> CodexAuthSourceOverride {
    let homes = codex_homes_override(homes_root);
    let auth_prior = std::env::var_os(crate::codex::CODEX_AUTH_SOURCE_ENV);
    // SAFETY: `homes` holds the process-wide lock covering this key too
    // (see the type doc above) for the lifetime of the returned guard.
    unsafe { std::env::set_var(crate::codex::CODEX_AUTH_SOURCE_ENV, auth_source) };
    CodexAuthSourceOverride {
        auth_prior,
        _homes: homes,
    }
}

impl Drop for CodexAuthSourceOverride {
    fn drop(&mut self) {
        // Runs before the `_homes` field's own `Drop` (struct fields drop
        // in declaration order, but this impl's body runs first), so the
        // auth var is restored while the lock is still held.
        // SAFETY: same as `codex_homes_override` — lock still held.
        match self.auth_prior.take() {
            Some(value) => unsafe { std::env::set_var(crate::codex::CODEX_AUTH_SOURCE_ENV, value) },
            None => unsafe { std::env::remove_var(crate::codex::CODEX_AUTH_SOURCE_ENV) },
        }
    }
}

/// A minimal [`DriverDescriptor`] to pair with [`StubDriver`]. Its menu
/// resolves everything to one `"stub-model"` slug, which is enough for
/// tests that only exercise capability declaration or the seams a stub
/// stands in for.
pub fn stub_descriptor() -> DriverDescriptor {
    DriverDescriptor {
        name: "stub",
        label: "Stub Driver",
        binary: "stub",
        config_dir: ".stub",
        agent_rules_filename: "AGENTS.md",
        initial_prompt_filename: "initial-prompt.txt",
        model_menu: ModelMenu {
            engine_default: "stub-model",
            effort_value_for_level: |_| None,
            default_model_for_level: |_| "stub-model",
            model_for_reasoning: |_| "stub-model",
            prompt_addendum_for_level: |_| None,
            model_requires_auto_permissions: |_| false,
            model_belongs_to_driver: |_| true,
        },
    }
}

/// Configurable [`AgentDriver`] stub. Every method beyond
/// `descriptor`/`capabilities` is unimplemented (or a harmless no-op for
/// the methods that can't panic on the hot paths, like
/// `normalize_transcript_entry`) — callers that need this fixture only
/// ever exercise capability declaration and menu resolution against it.
///
/// `post_hoc_interception_fn` defaults to `None` (same as the trait's
/// default); set it with [`StubDriver::with_post_hoc_interception`] to
/// exercise a downstream crate's `AbsenceDisposition::Degrade` dispatch
/// for [`super::Capability::ToolUseInterception`] without a second real
/// driver implementation.
pub struct StubDriver {
    pub descriptor: DriverDescriptor,
    pub caps: CapabilitySet,
    pub post_hoc_interception_fn: Option<PostHocInterceptionFn>,
    pub worker_process_lifetime: WorkerProcessLifetime,
}

impl StubDriver {
    pub fn new(descriptor: DriverDescriptor, caps: CapabilitySet) -> Self {
        Self {
            descriptor,
            caps,
            post_hoc_interception_fn: None,
            worker_process_lifetime: WorkerProcessLifetime::Persistent,
        }
    }

    /// Chainable: register the fixture's [`PostHocInterceptionFn`].
    pub fn with_post_hoc_interception(mut self, f: PostHocInterceptionFn) -> Self {
        self.post_hoc_interception_fn = Some(f);
        self
    }

    /// Chainable: override the fixture's declared [`WorkerProcessLifetime`]
    /// (defaults to `Persistent`, same as the trait default) — e.g. to
    /// exercise [`crate::registry::DriverRegistry`]'s admission guard against
    /// a driver that declares `OneTurnPerProcess`.
    pub fn with_worker_process_lifetime(mut self, lifetime: WorkerProcessLifetime) -> Self {
        self.worker_process_lifetime = lifetime;
        self
    }
}

#[async_trait]
impl AgentDriver for StubDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &self.descriptor
    }
    fn capabilities(&self) -> CapabilitySet {
        self.caps.clone()
    }
    fn post_hoc_interception(&self) -> Option<PostHocInterceptionFn> {
        self.post_hoc_interception_fn
    }
    fn spawn_invocation(&self, request: SpawnRequest<'_>) -> SpawnPlan {
        // Distinctive, non-Claude command line so call-site cutover tests
        // can prove a registered stub (not ClaudeDriver) produced the
        // spawn plan. Uses the descriptor's binary so each stub is
        // recognisable by slug without a custom impl.
        SpawnPlan {
            command: format!("{} --model {}\n", self.descriptor.binary, request.model),
            env: vec![],
        }
    }
    async fn provision_workspace(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<Option<DriverRuntimeState>> {
        unimplemented!()
    }
    async fn teardown_workspace(
        &self,
        _: Option<&Path>,
        _: &str,
        _: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn write_permission_config(&self, _: &PermissionInput, _: &Path) -> anyhow::Result<PermissionArtifacts> {
        unimplemented!()
    }
    fn progress_fidelity(&self) -> ProgressFidelity {
        // Minimal tier: call-site cutover tests that seed live state
        // from a stub don't need a Claude-shaped cadence.
        ProgressFidelity::Minimal
    }
    fn progress_observation_wiring(&self, _: &ProgressObservationConfig) -> ProgressIngress {
        // No hooks: a StdoutJsonl-shaped ingress. `write_workspace_files`
        // / settings rendering tolerate an empty hooks map.
        ProgressIngress::StdoutJsonl
    }
    fn normalize_progress_event(&self, _: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        unimplemented!()
    }
    fn turn_boundary(&self, _: &WorkerEvent) -> Option<TurnEnd> {
        // Declares no turn boundary. Like `transcript_path_for_session`
        // this sits on an ingress hot path, so it answers instead of
        // panicking — and "this driver has no boundary to report" is the
        // answer a stub can honestly give.
        None
    }
    fn tool_use_interception_wiring(&self, _: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        ToolUseInterceptionWiring {
            pre_tool_use_hooks: Vec::new(),
        }
    }
    fn agent_rules_preamble(&self) -> &'static str {
        // Distinctive marker so write_workspace_files tests can prove
        // the rendered agent-rules file came from this stub, not Claude.
        "# stub-driver preamble\n"
    }
    fn transcript_path_for_session(&self, _: &serde_json::Value) -> Option<String> {
        None
    }
    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        raw
    }
    fn extract_error_from_transcript(&self, _: &[serde_json::Value]) -> Option<String> {
        None
    }
    fn classify_error(&self, _: &str) -> WorkerErrorClass {
        unimplemented!()
    }
    fn structured_output_fallback(&self, _: StructuredOutputKind, _: &str) -> Vec<FallbackCandidate> {
        Vec::new()
    }
    fn worker_process_lifetime(&self) -> WorkerProcessLifetime {
        self.worker_process_lifetime
    }
}
