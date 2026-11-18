//! Per-repo worker guidance: `BOSS.md` at the root of a product's repo,
//! injected into the prompts of Boss-dispatched coding workers.
//!
//! `BOSS.md` holds worker rules that are specific to running under Boss (for
//! example "attach screenshots of visual changes with `boss attach`") and so
//! do not belong in the repo's `CLAUDE.md` / `AGENTS.md`, which workers started
//! outside Boss read too. It is read from GitHub at the default branch's HEAD
//! at dispatch time, through the same fetch, cap and last-known-good cache as
//! the coordinator's `BOSS_COORDINATOR.md`, and rendered as a visible,
//! sha-stamped block next to the product dispatch preamble.
//!
//! This module owns the three decisions that are about the *prompt* rather
//! than about GitHub: which executions get the block
//! ([`receives_repo_boss_guidance`]), what each read outcome looks like in the
//! prompt ([`render_block`]), and the seam through which the engine supplies
//! the read ([`RepoGuidanceSource`]). A read can never fail a dispatch: every
//! outcome is either a block, a deliberate absence, or a visible note.
//!
//! See `tools/boss/docs/repo-boss-guidance.md`.

use async_trait::async_trait;
use boss_protocol::{CoordinatorGuidanceState, CoordinatorGuidanceView, ExecutionKind, Product};

/// Opening/closing marker names of the injected block. The opening marker
/// carries `repo=`, `sha=` and, for a cached copy, `cached` and `read_at=`, so
/// anyone reading a transcript can tell which version a worker was given.
const MARKER: &str = "repo-boss-guidance";

/// Longest fetch-error text echoed into a one-line prompt note.
const MAX_NOTE_REASON_CHARS: usize = 240;

/// Whether an execution of `kind` is given the repo's `BOSS.md`.
///
/// Coding executions only: implementation of chores and project tasks, plus
/// revisions and the engine-minted fixes that edit a PR branch (merge-conflict
/// resolution and CI remediation). Not design, investigation, or any of the
/// read-only and administrative kinds.
///
/// The match is exhaustive on purpose: a new [`ExecutionKind`] does not compile
/// until someone decides whether it is a coding execution.
pub fn receives_repo_boss_guidance(kind: &ExecutionKind) -> bool {
    match kind {
        ExecutionKind::ChoreImplementation
        | ExecutionKind::TaskImplementation
        | ExecutionKind::RevisionImplementation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::CiRemediation => true,
        ExecutionKind::AnswerAgent
        | ExecutionKind::AutomationTriage
        | ExecutionKind::InvestigationImplementation
        | ExecutionKind::PrReview
        | ExecutionKind::PrReviewGuide
        | ExecutionKind::ProductDesign
        | ExecutionKind::ProjectDesign
        | ExecutionKind::Progression => false,
    }
}

/// The engine's read of a product's `BOSS.md`, supplied to the prompt composer.
///
/// An implementation is expected to be bounded in time and to fold every
/// failure into the returned view's state (including the last-known-good
/// fallback to [`CoordinatorGuidanceState::Cached`]); it never errors, because
/// a GitHub problem must not stop a dispatch.
#[async_trait]
pub trait RepoGuidanceSource: std::fmt::Debug + Send + Sync {
    async fn read(&self, product: &Product) -> CoordinatorGuidanceView;
}

/// Resolve the block to inject for one execution, or `None` when this
/// execution gets nothing (not a coding kind, no source, no product, or no
/// file). Logs the outcome, so every dispatch leaves a trail of which `BOSS.md`
/// version it carried or why it carried none.
pub async fn resolve_block(
    source: Option<&dyn RepoGuidanceSource>,
    kind: &ExecutionKind,
    product: Option<&Product>,
    execution_id: &str,
) -> Option<String> {
    if !receives_repo_boss_guidance(kind) {
        return None;
    }
    let (source, product) = (source?, product?);
    let view = source.read(product).await;
    log_outcome(&view, execution_id);
    render_block(&view)
}

fn log_outcome(view: &CoordinatorGuidanceView, execution_id: &str) {
    let tag = view.state.tag();
    let git_ref = view.state.git_ref().unwrap_or("");
    let owner_repo = view.owner_repo.as_deref().unwrap_or("");
    match &view.state {
        CoordinatorGuidanceState::Loaded { bytes, .. } => tracing::info!(
            execution_id,
            product_id = %view.product_id,
            owner_repo,
            git_ref,
            bytes,
            "repo BOSS.md injected into the worker prompt"
        ),
        CoordinatorGuidanceState::Cached {
            fetch_error,
            read_at,
            markdown: Some(_),
            ..
        } => tracing::warn!(
            execution_id,
            product_id = %view.product_id,
            owner_repo,
            git_ref,
            read_at,
            fetch_error,
            "repo BOSS.md could not be fetched; injecting the cached last-known-good copy"
        ),
        CoordinatorGuidanceState::Cached {
            fetch_error,
            read_at,
            markdown: None,
            ..
        } => tracing::warn!(
            execution_id,
            product_id = %view.product_id,
            owner_repo,
            git_ref,
            read_at,
            fetch_error,
            "repo BOSS.md could not be fetched; the cached read found no file, so the prompt carries a note"
        ),
        CoordinatorGuidanceState::OverCap { bytes, cap_bytes, .. } => tracing::warn!(
            execution_id,
            product_id = %view.product_id,
            owner_repo,
            git_ref,
            bytes,
            cap_bytes,
            "repo BOSS.md is over the size cap; nothing injected, the prompt carries a note"
        ),
        CoordinatorGuidanceState::Failed { reason } => tracing::warn!(
            execution_id,
            product_id = %view.product_id,
            owner_repo,
            reason,
            "repo BOSS.md could not be fetched and no cached copy exists; the prompt carries a note"
        ),
        CoordinatorGuidanceState::Missing { .. }
        | CoordinatorGuidanceState::NoRepoConfigured
        | CoordinatorGuidanceState::NotGitHub { .. } => tracing::debug!(
            execution_id,
            product_id = %view.product_id,
            state = tag,
            git_ref,
            "no repo BOSS.md to inject"
        ),
    }
}

/// Render one read outcome as the prompt block, or `None` where the right
/// behaviour is to say nothing.
///
/// | state                              | prompt                                  |
/// | ---------------------------------- | --------------------------------------- |
/// | loaded                             | the file, marked with repo and sha      |
/// | cached, with a file                | the file, marked cached with sha + time |
/// | over cap                           | a one-line note, no file text           |
/// | failed (no cached copy)            | a one-line "could not be loaded" note   |
/// | cached, without a file             | a one-line "could not be loaded" note   |
/// | missing                            | nothing                                 |
/// | no repo / not a GitHub remote      | nothing                                 |
pub fn render_block(view: &CoordinatorGuidanceView) -> Option<String> {
    let repo = view.owner_repo.as_deref();
    let path = view.path.as_str();
    match &view.state {
        CoordinatorGuidanceState::Loaded { git_ref, markdown, .. } => Some(wrap(
            &open_marker(repo, Some(git_ref), None),
            &format!("{}\n{}", preface(path), markdown.trim_end()),
        )),
        CoordinatorGuidanceState::Cached {
            git_ref,
            read_at,
            fetch_error,
            markdown: Some(markdown),
        } => Some(wrap(
            &open_marker(repo, Some(git_ref), Some(read_at)),
            &format!(
                "NOTE: CACHED copy, not freshly loaded. GitHub could not be read at dispatch ({}); this is the last \
                 `{path}` read successfully, at {read_at} (sha {git_ref}), and it may be out of date.\n{}\n{}",
                one_line(fetch_error),
                preface(path),
                markdown.trim_end()
            ),
        )),
        CoordinatorGuidanceState::Cached {
            git_ref,
            read_at,
            fetch_error,
            markdown: None,
        } => Some(wrap(
            &open_marker(repo, Some(git_ref), Some(read_at)),
            &format!(
                "NOTE: this repo's Boss guidance (`{path}`) could not be loaded ({}) and no earlier copy is \
                 available; the last successful read at {read_at} (sha {git_ref}) found no file, so any rules it \
                 holds are NOT in this prompt. Mention this in your final summary.",
                one_line(fetch_error)
            ),
        )),
        CoordinatorGuidanceState::OverCap {
            git_ref,
            bytes,
            cap_bytes,
        } => Some(wrap(
            &open_marker(repo, Some(git_ref), None),
            &format!(
                "NOTE: `{path}` is {bytes} bytes, over the {cap_bytes}-byte cap, so this repo's Boss guidance was NOT \
                 included in this prompt. Mention this in your final summary so the file can be trimmed."
            ),
        )),
        CoordinatorGuidanceState::Failed { reason } => Some(wrap(
            &open_marker(repo, None, None),
            &format!(
                "NOTE: this repo's Boss guidance (`{path}`) could not be loaded ({}) and no earlier copy is \
                 available, so any rules it holds are NOT in this prompt. Mention this in your final summary.",
                one_line(reason)
            ),
        )),
        CoordinatorGuidanceState::Missing { .. }
        | CoordinatorGuidanceState::NoRepoConfigured
        | CoordinatorGuidanceState::NotGitHub { .. } => None,
    }
}

/// Put `block` in front of the prompt body.
pub fn prepend_to_prompt(block: &str, prompt_text: &str) -> String {
    format!("{block}\n\n{prompt_text}")
}

fn open_marker(repo: Option<&str>, sha: Option<&str>, cached_read_at: Option<&str>) -> String {
    let mut marker = format!("[{MARKER}");
    if let Some(repo) = repo {
        marker.push_str(&format!(" repo={repo}"));
    }
    if let Some(sha) = sha {
        marker.push_str(&format!(" sha={sha}"));
    }
    if let Some(read_at) = cached_read_at {
        marker.push_str(&format!(" cached read_at={read_at}"));
    }
    marker.push(']');
    marker
}

fn wrap(open: &str, body: &str) -> String {
    format!("{open}\n{body}\n[/{MARKER}]")
}

fn preface(path: &str) -> String {
    format!(
        "Boss-specific rules for this repo (`{path}` at the default branch's HEAD). They apply to this run alongside \
         the repo's CLAUDE.md / AGENTS.md."
    )
}

/// Collapse whitespace so an error message stays on one line, and bound its
/// length so a verbose GitHub response cannot swamp the prompt.
fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_NOTE_REASON_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(MAX_NOTE_REASON_CHARS).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use boss_protocol::REPO_BOSS_GUIDANCE_PATH;

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn view(state: CoordinatorGuidanceState) -> CoordinatorGuidanceView {
        CoordinatorGuidanceView::builder()
            .product_id("prod_a")
            .fetched_at("2026-11-10T00:00:00Z")
            .path(REPO_BOSS_GUIDANCE_PATH)
            .product_name("Alpha")
            .state(state)
            .owner_repo("acme/alpha")
            .build()
    }

    fn product() -> Product {
        Product::builder()
            .id("prod_a")
            .created_at("2026-01-01T00:00:00Z")
            .description("")
            .name("Alpha")
            .slug("alpha")
            .status("active")
            .updated_at("2026-01-01T00:00:00Z")
            .build()
    }

    #[test]
    fn only_coding_executions_receive_the_guidance() {
        let coding = [
            ExecutionKind::ChoreImplementation,
            ExecutionKind::TaskImplementation,
            ExecutionKind::RevisionImplementation,
            ExecutionKind::ConflictResolution,
            ExecutionKind::CiRemediation,
        ];
        let not_coding = [
            ExecutionKind::AnswerAgent,
            ExecutionKind::AutomationTriage,
            ExecutionKind::InvestigationImplementation,
            ExecutionKind::PrReview,
            ExecutionKind::PrReviewGuide,
            ExecutionKind::ProductDesign,
            ExecutionKind::ProjectDesign,
            ExecutionKind::Progression,
        ];
        for kind in &coding {
            assert!(receives_repo_boss_guidance(kind), "{kind} should receive BOSS.md");
        }
        for kind in &not_coding {
            assert!(!receives_repo_boss_guidance(kind), "{kind} must not receive BOSS.md");
        }
    }

    #[test]
    fn loaded_block_carries_repo_sha_and_the_file() {
        let block = render_block(&view(CoordinatorGuidanceState::Loaded {
            git_ref: SHA.to_owned(),
            bytes: 22,
            markdown: "- attach screenshots\n".to_owned(),
        }))
        .unwrap();
        assert!(
            block.starts_with(&format!("[repo-boss-guidance repo=acme/alpha sha={SHA}]\n")),
            "{block}"
        );
        assert!(
            block.ends_with("- attach screenshots\n[/repo-boss-guidance]"),
            "{block}"
        );
        assert!(!block.contains("cached"), "a fresh load must not look cached: {block}");
    }

    #[test]
    fn cached_block_is_labelled_with_sha_read_time_and_the_error() {
        let block = render_block(&view(CoordinatorGuidanceState::Cached {
            git_ref: SHA.to_owned(),
            read_at: "2026-11-09T08:00:00Z".to_owned(),
            fetch_error: "could not reach GitHub\n(connection reset)".to_owned(),
            markdown: Some("- rule\n".to_owned()),
        }))
        .unwrap();
        assert!(
            block.starts_with(&format!(
                "[repo-boss-guidance repo=acme/alpha sha={SHA} cached read_at=2026-11-09T08:00:00Z]\n"
            )),
            "{block}"
        );
        assert!(block.contains("CACHED copy, not freshly loaded"), "{block}");
        assert!(block.contains("could not reach GitHub (connection reset)"), "{block}");
        assert!(block.ends_with("- rule\n[/repo-boss-guidance]"), "{block}");
    }

    #[test]
    fn a_cached_absence_leaves_a_failure_note_but_a_fresh_missing_file_is_silent() {
        let block = render_block(&view(CoordinatorGuidanceState::Cached {
            git_ref: SHA.to_owned(),
            read_at: "2026-11-09T08:00:00Z".to_owned(),
            fetch_error: "offline".to_owned(),
            markdown: None,
        }))
        .unwrap();
        assert!(block.contains("could not be loaded (offline)"), "{block}");
        assert!(
            block.contains("last successful read at 2026-11-09T08:00:00Z"),
            "{block}"
        );
        assert!(block.contains(&format!("sha {SHA}")), "{block}");
        assert!(block.contains("found no file"), "{block}");
        assert_eq!(block.lines().count(), 3, "{block}");
        assert_eq!(
            render_block(&view(CoordinatorGuidanceState::Missing {
                git_ref: SHA.to_owned()
            })),
            None
        );
    }

    #[test]
    fn products_with_nowhere_to_read_from_inject_nothing() {
        assert_eq!(render_block(&view(CoordinatorGuidanceState::NoRepoConfigured)), None);
        assert_eq!(
            render_block(&view(CoordinatorGuidanceState::NotGitHub {
                repo_remote_url: "https://gitlab.com/a/b".to_owned()
            })),
            None
        );
    }

    #[test]
    fn over_cap_withholds_the_text_and_leaves_a_one_line_note() {
        let block = render_block(&view(CoordinatorGuidanceState::OverCap {
            git_ref: SHA.to_owned(),
            bytes: 40_000,
            cap_bytes: 32_768,
        }))
        .unwrap();
        assert!(block.starts_with(&format!("[repo-boss-guidance repo=acme/alpha sha={SHA}]\n")));
        assert!(block.contains("40000 bytes, over the 32768-byte cap"), "{block}");
        assert!(block.contains("NOT included"), "{block}");
        assert_eq!(block.lines().count(), 3, "marker, one note line, marker: {block}");
    }

    #[test]
    fn a_failure_without_a_cache_says_so_in_one_line_and_bounds_the_reason() {
        let reason = format!("could not reach GitHub.\nGitHub said: {}", "x".repeat(1000));
        let block = render_block(&view(CoordinatorGuidanceState::Failed { reason })).unwrap();
        assert!(block.starts_with("[repo-boss-guidance repo=acme/alpha]\n"), "{block}");
        assert!(block.contains("could not be loaded"), "{block}");
        assert_eq!(block.lines().count(), 3, "marker, one note line, marker: {block}");
        assert!(block.contains('…'), "a long reason is truncated: {block}");
    }

    #[test]
    fn prepending_puts_the_block_before_the_prompt() {
        assert_eq!(prepend_to_prompt("[b]", "task"), "[b]\n\ntask");
    }

    #[derive(Debug, Default)]
    struct CountingSource {
        reads: AtomicUsize,
    }

    #[async_trait]
    impl RepoGuidanceSource for CountingSource {
        async fn read(&self, _product: &Product) -> CoordinatorGuidanceView {
            self.reads.fetch_add(1, Ordering::SeqCst);
            view(CoordinatorGuidanceState::Loaded {
                git_ref: SHA.to_owned(),
                bytes: 6,
                markdown: "rules\n".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn non_coding_executions_do_not_even_read_the_file() {
        let source = CountingSource::default();
        let product = product();
        let block = resolve_block(Some(&source), &ExecutionKind::PrReview, Some(&product), "exec_1").await;
        assert_eq!(block, None);
        assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_coding_execution_reads_once_and_gets_the_block() {
        let source = CountingSource::default();
        let product = product();
        let block = resolve_block(
            Some(&source),
            &ExecutionKind::ChoreImplementation,
            Some(&product),
            "exec_1",
        )
        .await
        .unwrap();
        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
        assert!(block.contains(&format!("sha={SHA}")), "{block}");
    }

    #[tokio::test]
    async fn no_source_or_no_product_means_no_block() {
        let product = product();
        assert_eq!(
            resolve_block(None, &ExecutionKind::ChoreImplementation, Some(&product), "exec_1").await,
            None
        );
        let source = CountingSource::default();
        assert_eq!(
            resolve_block(Some(&source), &ExecutionKind::ChoreImplementation, None, "exec_1").await,
            None
        );
        assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    }
}
