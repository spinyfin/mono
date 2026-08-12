//! clap defs for `boss comment …` and `boss task|chore comment`.

use crate::*;

#[derive(Debug, Subcommand)]
pub(crate) enum CommentCommand {
    /// List comments on an artifact. `--task` is shorthand for a work-item
    /// comment thread; pass `--artifact` + `--artifact-kind` for a
    /// `pr_doc:<owner>/<repo>:<branch>:<path>` composite key. Excludes
    /// `resolved`/`dismissed` unless `--include-resolved` — `orphaned`
    /// comments are always included. Engine-RPC half of
    /// `bossctl comments list`.
    List(CommentListArgs),
    /// Show one comment: anchor, status, intent classification, thread
    /// entries, and full answer-agent-run history. Engine-RPC half of
    /// `bossctl comments show`.
    Show(CommentShowArgs),
    /// List every `answer_agent_runs` row for a comment, oldest first.
    /// Engine-RPC half of `bossctl comments runs`.
    Runs(CommentRunsArgs),
    /// Create a top-level comment on a work item or other artifact.
    ///
    /// Thin write surface over the existing `CommentsCreate` engine RPC
    /// (same machinery the markdown viewer uses). For the common
    /// work-item case prefer `boss task comment <id> --body …` /
    /// `boss chore comment <id> --body …`, which resolve friendly short
    /// ids and default the text-quote anchor to the item's name.
    Create(CommentCreateArgs),
    /// Post the answer agent's reply to the comment thread this run was
    /// spawned for. The target thread is derived from the caller's own
    /// `BOSS_RUN_ID` — there is no `--comment-id` (or similar) flag, by
    /// design: this is the one write action a read-only answer-agent
    /// session is permitted, and it must not be able to target any other
    /// comment. Post exactly one reply; a second call fails (the tracking
    /// run row is no longer `running`).
    Reply(CommentReplyArgs),
}

/// Args for `boss task comment` / `boss chore comment` — post a top-level
/// comment on a leaf work item.
#[derive(Debug, Clone, Args)]
pub(crate) struct KindCommentArgs {
    /// Task/chore id. Accepts primary id (`task_…`), friendly short id
    /// (`#42` / bare number), or cross-product form (`boss/42`).
    #[arg(value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) id: String,
    /// Resolve a friendly short id against this product (slug or id).
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Comment body. Required and may not be empty / whitespace-only.
    #[arg(long)]
    pub(crate) body: String,
    /// Author stamp stored on the row. Defaults to `user:$USER` (or
    /// `user:cli` when `$USER` is unset). Matches the app's `user:…`
    /// author convention.
    #[arg(long)]
    pub(crate) author: Option<String>,
    /// W3C TextQuoteSelector `exact` span. Defaults to the work item's
    /// name when omitted (CLI free-form notes are not text-selections;
    /// the name is a stable, non-empty anchor the engine accepts).
    #[arg(long)]
    pub(crate) exact: Option<String>,
    /// Up to ~64 chars of plain text preceding `exact`.
    #[arg(long, default_value = "")]
    pub(crate) prefix: String,
    /// Up to ~64 chars of plain text following `exact`.
    #[arg(long, default_value = "")]
    pub(crate) suffix: String,
}

/// Args for `boss comment create` — kind-agnostic create over
/// `CommentsCreate`. Prefer `boss task comment` / `boss chore comment`
/// for work items.
#[derive(Debug, Clone, Args)]
pub(crate) struct CommentCreateArgs {
    /// Work item (task/chore) id whose comment thread to post on —
    /// shorthand for `--artifact-kind work_item --artifact <id>`.
    /// Accepts primary / friendly short ids (with `--product` for the
    /// latter).
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) task: Option<String>,
    /// Raw artifact id. Pairs with `--artifact-kind`.
    #[arg(long)]
    pub(crate) artifact: Option<String>,
    /// Artifact kind for `--artifact` (`work_item` or `pr_doc`).
    #[arg(long, default_value = "work_item")]
    pub(crate) artifact_kind: String,
    /// Resolve a friendly short id in `--task` against this product.
    #[arg(long)]
    pub(crate) product: Option<String>,
    /// Comment body. Required and may not be empty / whitespace-only.
    #[arg(long)]
    pub(crate) body: String,
    /// Author stamp. Defaults to `user:$USER` (or `user:cli`).
    #[arg(long)]
    pub(crate) author: Option<String>,
    /// TextQuoteSelector `exact`. Required for non-`work_item` artifacts;
    /// defaults to the work item's name when `--task` is used.
    #[arg(long)]
    pub(crate) exact: Option<String>,
    /// Up to ~64 chars of plain text preceding `exact`.
    #[arg(long, default_value = "")]
    pub(crate) prefix: String,
    /// Up to ~64 chars of plain text following `exact`.
    #[arg(long, default_value = "")]
    pub(crate) suffix: String,
    /// Opaque doc-version stamp (equality token only). Defaults to `cli`
    /// for `work_item` free-form notes. Required for non-`work_item`
    /// artifacts (e.g. `pr_doc`) — pass the document digest/version the
    /// anchor refers to so the engine can detect stale anchors.
    #[arg(long)]
    pub(crate) doc_version: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct CommentListArgs {
    /// Work item (task/chore) id whose comments to list — shorthand
    /// for `--artifact-kind work_item --artifact <id>`.
    #[arg(long, value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) task: Option<String>,
    /// Raw artifact id (e.g. a `pr_doc:<owner>/<repo>:<branch>:<path>`
    /// composite key — an SSH or HTTPS remote URL also works for
    /// `<owner>/<repo>`). Pairs with `--artifact-kind`.
    #[arg(long)]
    pub(crate) artifact: Option<String>,
    /// Artifact kind for `--artifact` (`work_item` or `pr_doc`).
    #[arg(long, default_value = "pr_doc")]
    pub(crate) artifact_kind: String,
    /// Include `resolved`/`dismissed` comments (excluded by default).
    #[arg(long)]
    pub(crate) include_resolved: bool,
}

#[derive(Debug, Args)]
pub(crate) struct CommentShowArgs {
    /// Comment id (`cmt_…`).
    pub(crate) comment_id: String,
}

#[derive(Debug, Args)]
pub(crate) struct CommentRunsArgs {
    /// Comment id (`cmt_…`) whose answer-agent runs to list.
    pub(crate) comment_id: String,
}

#[derive(Debug, Args)]
pub(crate) struct CommentReplyArgs {
    /// The comprehensive answer to post. Pass the full text inline —
    /// there is deliberately no `--body-file` (a file-reading flag on this
    /// command would let a read-only session exfiltrate arbitrary file
    /// contents into the thread).
    #[arg(long)]
    pub(crate) body: String,
}
