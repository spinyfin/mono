//! Read-only "mini-coordinator" answer agent (P3a of
//! `tools/boss/docs/designs/comment-triggered-document-revisions.md`).
//!
//! This module owns the answer agent's **worker-facing** surface: the
//! read-only agent-rules file it is spawned with, and the one command name it
//! is allowed to run that changes state (posting its reply). The
//! *capability-restricted dispatch* itself — the `dontAsk` permission mode, the
//! `permissions.allow` allowlist, and the deny belt — lives in
//! [`crate::worker_setup`], keyed on [`crate::worker_setup::WorkerKind::AnswerAgent`],
//! so the whole permission surface is assembled in one place. The two are
//! linked by [`THREAD_REPLY_COMMAND`] (the sole allowlisted mutation) so the
//! allowlist and the prose can never drift apart.
//!
//! ## Enforcement model (resolves the design's open question)
//!
//! The design (§ Risks) left open whether the read-only sandbox should be a
//! *hard-coded reduced tool table* (recommended) or a *runtime capability-token
//! check on every RPC*. This implements the former, using Claude Code's native
//! deny-by-default `dontAsk` permission mode:
//!
//! - The agent is launched with `--permission-mode dontAsk` (forced at the
//!   dispatch layer — never `--dangerously-skip-permissions`, which would
//!   bypass settings, and never `--permission-mode auto`, which auto-approves).
//! - `dontAsk` auto-denies every tool call except those matching
//!   `permissions.allow` and built-in read-only Bash commands — a true
//!   allowlist, not a blocklist.
//! - A comprehensive `permissions.deny` belt (deny always wins over allow)
//!   covers the known-catastrophic mutating surfaces as defense-in-depth.
//!
//! No new per-RPC token machinery is introduced. This mirrors how the revision
//! PreToolUse guard blocks at the tool-availability layer, applied to a much
//! smaller allowed set.

/// The single state-mutating command the answer agent is permitted to run:
/// post its comprehensive reply as an engine-authored thread entry on the
/// comment. Every other write/push/mutate surface is denied.
///
/// This is the one entry in the answer agent's `permissions.allow` allowlist
/// beyond read-only tools (see
/// [`crate::worker_setup::answer_agent_allow_rules`]). The command itself — a
/// `boss` subcommand that appends an `entry_kind='answer'` thread entry and
/// flips the comment `answering → answered` — is implemented in P3b (spawn +
/// reply). Naming it here as a single constant keeps the P3a allowlist and the
/// P3b reply path referencing one source of truth.
///
/// SECURITY (for the P3b implementer): the allowlist entry is
/// `Bash({THREAD_REPLY_COMMAND}:*)`, which permits ANY arguments/flags to this
/// command (Claude Code blocks chaining/`$(...)`, but not in-command flags).
/// The sandbox therefore trusts this command to be strictly single-purpose —
/// post one reply to the run's own target comment thread. Do NOT give it
/// side-effecting flags (arbitrary `--comment-id`, `--status`, a `--body-file`
/// that reads any path, etc.); anything it accepts is inside the sandbox.
pub const THREAD_REPLY_COMMAND: &str = "boss comment reply";

/// Render the read-only agent-rules file (`CLAUDE.md`) for an answer-agent
/// worker. Modeled on [`boss_engine_pr_review::render_reviewer_claude_md`] but for
/// the answer agent's mandate: read anything the coordinator can see, read code
/// in a leased checkout, and post exactly one thread reply — never edit, push,
/// open a PR, or mutate task/comment/cube state.
///
/// Returned in place of the Standard worker `CLAUDE.md` (which is
/// PR-deliverable-oriented and would be actively wrong here) by
/// [`crate::worker_setup::render_claude_md`] when
/// `worker_kind == WorkerKind::AnswerAgent`.
pub fn render_answer_agent_claude_md(lease_id: &str, workspace_path: &str) -> String {
    format!(
        "# Boss answer-agent rules\n\
         \n\
         You are running inside a Boss-managed **answer-agent** session. The\n\
         engine spawned you to answer one reviewer question left as a comment on\n\
         a design/investigation document, in that comment's thread. You are a\n\
         read-only mini-coordinator: you can read everything the Boss\n\
         coordinator can see and read code in a leased checkout, but you change\n\
         nothing except by posting your reply.\n\
         \n\
         ## Read-only mandate (HARD CONSTRAINT)\n\
         \n\
         **Your only state-changing action is posting your reply to the comment\n\
         thread.** Tool calls for anything else are denied at the dispatch layer\n\
         (deny-by-default `dontAsk` permission mode + an explicit deny list) —\n\
         this is enforced, not advisory.\n\
         \n\
         Forbidden (tool calls for these are denied):\n\
         \n\
         - Editing or writing ANY file (`Edit`, `Write`, `NotebookEdit`).\n\
         - Committing or pushing (`jj git push`, `git push`).\n\
         - Opening, updating, merging, closing, editing, or commenting on a PR\n\
           (`gh pr …`, `cube pr create`, `cube pr update`).\n\
         - Creating or mutating tasks, chores, revisions, or projects, or\n\
           changing comment status by any means other than posting your reply\n\
           (the reply command flips this comment `answering → answered` as its\n\
           own bundled side effect — that is the one sanctioned exception).\n\
         - Leasing, releasing, or otherwise mutating cube state (`cube …`). The\n\
           engine already handed you a read-only checkout — use it as-is.\n\
         \n\
         Anything you would \"fix\", describe in your reply instead. Your reply\n\
         may include concrete proposed edits (a sketch, an embedded diff), but\n\
         you have no mechanism to apply them — it is prose, not a patch.\n\
         \n\
         ## Posting your reply\n\
         \n\
         When you have answered the question, post exactly one reply with\n\
         `{reply_cmd}` (the sole write you are permitted). Do not post more than\n\
         one reply.\n\
         \n\
         ## What you can read\n\
         \n\
         - The commented-on document, the comment, and its full thread.\n\
         - Product/project/task/execution/PR state via the coordinator's\n\
           read-only query layer.\n\
         - Code in your leased workspace — use `Read`, `Grep`, `Glob`, and\n\
           read-only shell (`cat`, `grep`, `jj log`, `jj show`, `jj diff`).\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace_path}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         The workspace is a read-only checkout. Lease held for the lifetime of\n\
         this run by the engine. Do not lease, release, or mutate cube state.\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files inside or outside your workspace. Other\n\
           workspaces belong to other workers.\n\
         - Do not modify cube's database, lease state, or workspace registry.\n\
         - `~/Library/Application Support/Boss/` is coordinator/engine-only.\n\
           Never read, write, or touch it.\n",
        reply_cmd = THREAD_REPLY_COMMAND,
        workspace_path = workspace_path,
        lease = lease_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_md_states_read_only_mandate_and_reply_command() {
        let md = render_answer_agent_claude_md("lease-1", "/ws/path");
        assert!(md.contains("Read-only mandate"));
        assert!(md.contains(THREAD_REPLY_COMMAND));
        assert!(md.contains("/ws/path"));
        assert!(md.contains("lease-1"));
        // It must NOT tell the agent a PR is the deliverable — that is the
        // Standard-worker contract and is actively wrong here.
        assert!(!md.contains("PR is the deliverable"));
        assert!(!md.contains("cube pr create --branch"));
    }
}
