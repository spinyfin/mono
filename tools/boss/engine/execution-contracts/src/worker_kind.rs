//! Worker permission postures and exhaustive execution-kind mapping.

use boss_protocol::ExecutionKind;

/// The worker posture shared by prompt rendering and engine-owned permission installers.
///
/// New kinds should document their read/write access contract in a comment
/// so reviewers can verify the deny rules match the stated posture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WorkerKind {
    /// Normal implementation worker (task, chore, revision, etc.). Has
    /// write access to its leased workspace; can push branches and open
    /// PRs (subject to kind-specific guards such as the revision PR guard).
    #[default]
    Standard,
    /// Read-only reviewer worker (design §9). Reads the PR diff and workspace
    /// files; MUST NOT mutate files, push commits, or interact with GitHub
    /// write endpoints. The engine reviewer deny rules are the
    /// primary enforcement layer for this mandate.
    Reviewer,
    /// Automation triage worker. Investigates the repo and
    /// emits a single decision marker (`automation: task <id>` /
    /// `automation: skip — …`), optionally running one
    /// `boss task create --automation`. It MUST NOT do the work itself: no
    /// file edits, commits, pushes, or PRs — and crucially there is **no PR
    /// deliverable**, so it must not receive the [`WorkerKind::Standard`]
    /// "a PR is the deliverable / print the PR URL as your last line"
    /// CLAUDE.md, which otherwise overrides the marker contract and leaves
    /// the run ending without a decision marker. The engine triage deny rules
    /// enforce the no-work posture; `boss task create`
    /// is intentionally left allowed.
    Triage,
    /// Read-only "mini-coordinator" answer agent (see
    /// `comment-triggered-document-revisions.md`). Answers one reviewer
    /// question in a doc-comment thread. Unlike every other kind it is
    /// enforced with an **allowlist, not a blocklist**: it is spawned in
    /// deny-by-default `dontAsk` permission mode with an explicit
    /// allowlist (read-only tools and the single thread-reply command), plus
    /// comprehensive engine answer-agent deny rules. It reads anything the
    /// coordinator can see and reads code in a
    /// read-only checkout, but MUST NOT edit, push, open/modify a PR, mutate
    /// task/comment/cube state, or take any action other than posting its one
    /// reply. See `answer_agent` for the worker-facing surface.
    AnswerAgent,
    /// PR review-guide generator on the fixed Codex Astra profile. Reads its pinned, read-only workspace and
    /// the embedded source packet and submits Markdown with
    /// `"$BOSS_BIN" propose review-guide --body`. A shared PreToolUse allowlist
    /// allows only source reads and submission; Claude additionally uses dontAsk.
    ReviewGuide,
}

impl WorkerKind {
    /// The permission mode that MUST be forced at spawn (`--permission-mode
    /// <mode>`) for this kind, overriding the model-derived default, if any.
    ///
    /// `Some("dontAsk")` for the capability-restricted answer agent so its
    /// deny-by-default allowlist is authoritative and cannot be silently
    /// downgraded to `auto` (allow-by-default) or `--dangerously-skip-permissions`
    /// (bypasses settings entirely). `None` lets the model-derived default
    /// apply.
    ///
    /// Deriving BOTH the settings posture (via [`worker_kind_for_execution`])
    /// and the forced CLI mode from the same exhaustive `WorkerKind` closes the
    /// divergence footgun where a restricted kind gets a `dontAsk` settings
    /// file but a downgradable CLI flag: the match here is exhaustive, so a new
    /// restricted kind must decide its mode rather than inheriting the
    /// downgradable default.
    pub fn forced_permission_mode(&self) -> Option<&'static str> {
        match self {
            WorkerKind::AnswerAgent | WorkerKind::ReviewGuide => Some("dontAsk"),
            WorkerKind::Standard | WorkerKind::Reviewer | WorkerKind::Triage => None,
        }
    }
}

impl WorkerKind {
    /// Short display label for alerts and bossctl output.
    pub fn label(&self) -> &'static str {
        match self {
            WorkerKind::Standard => "standard",
            WorkerKind::Reviewer => "reviewer",
            WorkerKind::Triage => "triage",
            WorkerKind::AnswerAgent => "answer-agent",
            WorkerKind::ReviewGuide => "review-guide",
        }
    }
}

/// Map a dispatched [`ExecutionKind`] to the [`WorkerKind`] whose permission
/// posture it must run under. The single source of truth shared by the local
/// spawn path (`PaneSpawnRunner`) and the remote spawn path
/// (`host_adapter`) so the two can never diverge — a new execution
/// kind that needs a restricted surface is wired here once.
///
/// Security-critical, so the match is **exhaustive** (no `_` arm): adding a new
/// [`ExecutionKind`] forces a compile error here, making every new kind decide
/// its permission posture explicitly rather than silently inheriting
/// [`WorkerKind::Standard`] (full write/push access).
pub fn worker_kind_for_execution(kind: &ExecutionKind) -> WorkerKind {
    match kind {
        ExecutionKind::PrReview => WorkerKind::Reviewer,
        ExecutionKind::AutomationTriage => WorkerKind::Triage,
        ExecutionKind::AnswerAgent => WorkerKind::AnswerAgent,
        ExecutionKind::PrReviewGuide => WorkerKind::ReviewGuide,
        // Interim posture: the engine cannot spawn a progression agent yet (the
        // dispatcher holds every `progression` row `ready`; see
        // `ExecutionCoordinator::progression_spawn_enabled`). Should one ever
        // reach a spawn path anyway, it gets the most restricted surface that
        // exists today — the answer agent's deny-by-default allowlist, with no
        // write access to a repo — and never `Standard`'s full write and push
        // access. The progression worker kind replaces this arm with its own.
        ExecutionKind::Progression => WorkerKind::AnswerAgent,
        ExecutionKind::ChoreImplementation
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::TaskImplementation => WorkerKind::Standard,
    }
}
