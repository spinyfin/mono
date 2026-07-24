//! Test suite for `worker_setup`, split into behavior-area modules to keep
//! each file well under the repo's file-size cap.
//!
//! Shared fixtures (`sample_input`, `claude_md_for`, `HomeGuard`,
//! `lock_shared_settings_dir`, …) live in [`helpers`]; each sibling module
//! pulls them in with `use super::helpers::*;` and reaches the items under
//! test with `use super::super::*;`.

mod helpers;

mod answer_agent;
mod checkleft_guard;
mod claude_md;
mod deny_rules;
mod heal;
mod launch_guard;
mod leaked_hooks;
mod path_guard;
mod pr_redirect_guard;
mod pre_tool_use_composition;
mod remote;
mod revision_guard;
mod settings_hooks;
mod write_workspace;
