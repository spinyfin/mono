//! Test doubles over the execution contracts, shared by every crate that
//! exercises completion, dispatch or sweeps against a fake cube and publisher.
//! Compiled only with the `test-support` feature.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use boss_protocol::FrontendEvent;
use tokio::sync::Mutex;

use std::sync::Arc;

use crate::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    ExecutionCubes, ExecutionPublisher, ResolvedExecutionCube,
};

#[doc(hidden)]
pub use boss_protocol::{WorkExecution, WorkItem};

/// A [`CubeClient`] test double that never touches cube. Every method
/// panics with `unimplemented!()` except `list_workspaces`/`list_repos`,
/// which return an empty vector.
///
/// The sweep test modules (`dead_pid_sweep`, `orphan_sweep`,
/// `pool_claim_sweep`, `stale_worker_sweep`) drive coordinators whose
/// cube interactions are never exercised, so this single stub replaces
/// the byte-identical copy each used to hand-roll.
pub struct NoopCube;

#[async_trait]
impl CubeClient for NoopCube {
    async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
        unimplemented!()
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        unimplemented!()
    }
    async fn create_change(&self, _: &Path, _: &str) -> Result<CubeChangeHandle> {
        unimplemented!()
    }
    async fn goto_workspace(&self, _: &Path, _: u64) -> Result<()> {
        unimplemented!()
    }
    async fn release_workspace(&self, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
        unimplemented!()
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        unimplemented!()
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        unimplemented!()
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(vec![])
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
}

/// Implement [`CubeClient`](crate::CubeClient) for a test
/// double, spelling out only the methods a given test actually drives.
/// Every trait method left unlisted is filled in with `unimplemented!()`,
/// which is exactly what a double wants for the paths it never exercises.
///
/// Overrides are written as ordinary `async fn`s with their real bodies
/// and **must appear in the trait's declaration order** (the order the
/// arms below enumerate: `ensure_repo`, `lease_workspace`, `create_change`,
/// `goto_workspace`, `goto_workspace_revision`, `rebase_workspace`,
/// `rebase_workspace_no_push`, `push_resolution`, `release_workspace`,
/// `workspace_status`, `heartbeat_lease`, `force_release_lease`,
/// `list_workspaces`, `list_repos`).
/// `goto_workspace_revision`, `rebase_workspace`, `rebase_workspace_no_push`,
/// and `push_resolution` all have trait defaults (erroring), so leaving any
/// unlisted keeps that default rather than an
/// `unimplemented!()`. The macro emits the `#[async_trait]` impl for you.
///
/// ```ignore
/// stub_cube_client! { RecordingCube {
///     async fn release_workspace(&self, lease_id: &str) -> Result<()> {
///         self.releases.lock().await.push(lease_id.to_owned());
///         Ok(())
///     }
/// } }
/// ```
#[macro_export]
macro_rules! stub_cube_client {
    ($ty:ty { $($body:tt)* }) => {
        $crate::stub_cube_client!(@munch $ty [] @ensure_repo $($body)*);
    };

    // ── ensure_repo ─────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo async fn ensure_repo $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn ensure_repo $a -> $r $b] @lease_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn ensure_repo(&self, _origin: &str) -> ::anyhow::Result<$crate::CubeRepoHandle> { ::core::unimplemented!() }
        ] @lease_workspace $($rest)*);
    };

    // ── lease_workspace ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace async fn lease_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn lease_workspace $a -> $r $b] @create_change $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn lease_workspace(&self, _repo_id: &str, _task: &str, _prefer: ::core::option::Option<&str>, _allow_dirty: bool, _exclude: &[&str]) -> ::anyhow::Result<$crate::CubeWorkspaceLease> { ::core::unimplemented!() }
        ] @create_change $($rest)*);
    };

    // ── create_change ───────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @create_change async fn create_change $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn create_change $a -> $r $b] @goto_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @create_change $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn create_change(&self, _workspace_path: &::std::path::Path, _title: &str) -> ::anyhow::Result<$crate::CubeChangeHandle> { ::core::unimplemented!() }
        ] @goto_workspace $($rest)*);
    };

    // ── goto_workspace ──────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace async fn goto_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn goto_workspace $a -> $r $b] @goto_workspace_revision $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn goto_workspace(&self, _workspace_path: &::std::path::Path, _pr: u64) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @goto_workspace_revision $($rest)*);
    };

    // ── goto_workspace_revision ──────────────────────────────────────────────
    // Has a trait default (erroring) — same "unlisted keeps the default"
    // convention as rebase_workspace, since most doubles never exercise the
    // post-merge-review positioning path.
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace_revision async fn goto_workspace_revision $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn goto_workspace_revision $a -> $r $b] @rebase_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace_revision $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @rebase_workspace $($rest)*);
    };

    // ── rebase_workspace ────────────────────────────────────────────────────
    // Has a trait default (erroring), so when a double doesn't override it the
    // macro emits nothing and the default applies — matching how the many
    // pre-ladder stubs behave (they never drive rung 1).
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace async fn rebase_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn rebase_workspace $a -> $r $b] @rebase_workspace_no_push $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @rebase_workspace_no_push $($rest)*);
    };

    // ── rebase_workspace_no_push ────────────────────────────────────────────
    // Also has a trait default (erroring) — same "unlisted keeps the default"
    // behaviour as `rebase_workspace`, used by the speculative-conflict
    // prediction sweep.
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace_no_push async fn rebase_workspace_no_push $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn rebase_workspace_no_push $a -> $r $b] @push_resolution $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @rebase_workspace_no_push $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @push_resolution $($rest)*);
    };

    // ── push_resolution ─────────────────────────────────────────────────────
    // Has a trait default (erroring), same convention as rebase_workspace.
    (@munch $ty:ty [$($acc:tt)*] @push_resolution async fn push_resolution $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn push_resolution $a -> $r $b] @verify_deletion_tripwire $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @push_resolution $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @verify_deletion_tripwire $($rest)*);
    };

    // ── verify_deletion_tripwire ────────────────────────────────────────────
    // Has a trait default (fails open, empty findings) — same "unlisted keeps
    // the default" convention as rebase_workspace/push_resolution: a double
    // that doesn't script this never makes a network call.
    (@munch $ty:ty [$($acc:tt)*] @verify_deletion_tripwire async fn verify_deletion_tripwire $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn verify_deletion_tripwire $a -> $r $b] @release_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @verify_deletion_tripwire $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @release_workspace $($rest)*);
    };

    // ── release_workspace ───────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @release_workspace async fn release_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn release_workspace $a -> $r $b] @release_workspace_with_reset $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @release_workspace $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn release_workspace(&self, _lease_id: &str) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @release_workspace_with_reset $($rest)*);
    };

    // ── release_workspace_with_reset ────────────────────────────────────────
    // Has a trait default (delegates to release_workspace).
    (@munch $ty:ty [$($acc:tt)*] @release_workspace_with_reset async fn release_workspace_with_reset $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn release_workspace_with_reset $a -> $r $b] @workspace_status $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @release_workspace_with_reset $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*] @workspace_status $($rest)*);
    };

    // ── workspace_status ────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @workspace_status async fn workspace_status $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn workspace_status $a -> $r $b] @heartbeat_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @workspace_status $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn workspace_status(&self, _workspace_path: &::std::path::Path) -> ::anyhow::Result<$crate::CubeWorkspaceStatus> { ::core::unimplemented!() }
        ] @heartbeat_lease $($rest)*);
    };

    // ── heartbeat_lease ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease async fn heartbeat_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn heartbeat_lease $a -> $r $b] @force_release_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: ::core::option::Option<u64>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @force_release_lease $($rest)*);
    };

    // ── force_release_lease ─────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease async fn force_release_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn force_release_lease $a -> $r $b] @list_workspaces $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn force_release_lease(&self, _lease_id: &str, _reason: ::core::option::Option<&str>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @list_workspaces $($rest)*);
    };

    // ── list_workspaces ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces async fn list_workspaces $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn list_workspaces $a -> $r $b] @list_repos $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn list_workspaces(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::CubeWorkspaceStatus>> { ::core::unimplemented!() }
        ] @list_repos $($rest)*);
    };

    // ── list_repos ──────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_repos async fn list_repos $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn list_repos $a -> $r $b] @done $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_repos $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)*
            async fn list_repos(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::CubeRepoSummary>> { ::core::unimplemented!() }
        ] @done $($rest)*);
    };

    (@munch $ty:ty [$($acc:tt)*] @done async fn recovery_pr_base $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_cube_client!(@munch $ty [$($acc)* async fn recovery_pr_base $a -> $r $b] @done $($rest)*);
    };

    // ── emit ────────────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @done) => {
        #[::async_trait::async_trait]
        impl $crate::CubeClient for $ty {
            $($acc)*
            async fn create_execution_bookmark(&self, workspace: &::std::path::Path, execution_id: &str, _predecessor: Option<&::boss_engine_recovery::execution_bookmark::ExecutionBookmark>, _inherited_base: Option<&str>) -> ::anyhow::Result<::boss_engine_recovery::execution_bookmark::ExecutionBookmark> {
                Ok(::boss_engine_recovery::execution_bookmark::ExecutionBookmark {
                    execution_id: execution_id.to_owned(),
                    repo_path: workspace.to_path_buf(),
                    host_id: "local".to_owned(),
                    base_commit: None,
                })
            }
        }
    };
    (@munch $ty:ty [$($acc:tt)*] @done async fn create_execution_bookmark $a:tt -> $r:ty $b:block) => {
        #[::async_trait::async_trait]
        impl $crate::CubeClient for $ty {
            $($acc)*
            async fn create_execution_bookmark $a -> $r $b
        }
    };
}

/// Implement [`HostAdapter`](crate::HostAdapter) for a test
/// double, spelling out only the methods a given test drives. Every
/// workspace-lifecycle method and `spawn_worker` left unlisted is filled
/// in with `unimplemented!()`; `command_repr` defaults to `None`; and the
/// four trait-defaulted methods (`read_transcript_tail_bytes`,
/// `reattach_events_forward`, `probe_remote_worker_alive`,
/// `read_worker_log_tail`) fall through to their trait defaults unless
/// overridden.
///
/// `host_id` has no universal default and **must be listed first**;
/// remaining overrides follow in the trait's declaration order.
#[macro_export]
macro_rules! stub_host_adapter {
    ($ty:ty { $($body:tt)* }) => {
        $crate::stub_host_adapter!(@munch $ty [] @host_id $($body)*);
    };

    // ── host_id (required) ──────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @host_id fn host_id $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* fn host_id $a -> $r $b] @ensure_repo $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @host_id $($rest:tt)*) => {
        ::core::compile_error!("stub_host_adapter! requires `fn host_id(&self) -> &str` as the first method");
    };

    // ── ensure_repo ─────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo async fn ensure_repo $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn ensure_repo $a -> $r $b] @lease_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @ensure_repo $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn ensure_repo(&self, _origin: &str) -> ::anyhow::Result<$crate::CubeRepoHandle> { ::core::unimplemented!() }
        ] @lease_workspace $($rest)*);
    };

    // ── lease_workspace ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace async fn lease_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn lease_workspace $a -> $r $b] @release_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @lease_workspace $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn lease_workspace(&self, _repo_id: &str, _task: &str, _prefer: ::core::option::Option<&str>, _allow_dirty: bool, _exclude: &[&str]) -> ::anyhow::Result<$crate::CubeWorkspaceLease> { ::core::unimplemented!() }
        ] @release_workspace $($rest)*);
    };

    // ── release_workspace ───────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @release_workspace async fn release_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn release_workspace $a -> $r $b] @release_workspace_with_reset $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @release_workspace $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn release_workspace(&self, _lease_id: &str) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @release_workspace_with_reset $($rest)*);
    };

    // ── release_workspace_with_reset (trait default unless overridden) ──
    (@munch $ty:ty [$($acc:tt)*] @release_workspace_with_reset async fn release_workspace_with_reset $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn release_workspace_with_reset $a -> $r $b] @probe_initial_input_alive $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @release_workspace_with_reset $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @probe_initial_input_alive $($rest)*);
    };

    // ── probe_initial_input_alive (trait default unless overridden) ────────
    (@munch $ty:ty [$($acc:tt)*] @probe_initial_input_alive async fn probe_initial_input_alive $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn probe_initial_input_alive $a -> $r $b] @cleanup_initial_input $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @probe_initial_input_alive $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @cleanup_initial_input $($rest)*);
    };

    // ── cleanup_initial_input (trait default unless overridden) ────────────
    (@munch $ty:ty [$($acc:tt)*] @cleanup_initial_input async fn cleanup_initial_input $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn cleanup_initial_input $a -> $r $b] @heartbeat_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @cleanup_initial_input $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @heartbeat_lease $($rest)*);
    };

    // ── heartbeat_lease ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease async fn heartbeat_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn heartbeat_lease $a -> $r $b] @force_release_lease $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @heartbeat_lease $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: ::core::option::Option<u64>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @force_release_lease $($rest)*);
    };

    // ── force_release_lease ─────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease async fn force_release_lease $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn force_release_lease $a -> $r $b] @create_change $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @force_release_lease $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn force_release_lease(&self, _lease_id: &str, _reason: ::core::option::Option<&str>) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @create_change $($rest)*);
    };

    // ── create_change ───────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @create_change async fn create_change $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn create_change $a -> $r $b] @goto_workspace $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @create_change $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn create_change(&self, _workspace_path: &::std::path::Path, _title: &str) -> ::anyhow::Result<$crate::CubeChangeHandle> { ::core::unimplemented!() }
        ] @goto_workspace $($rest)*);
    };

    // ── goto_workspace ──────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace async fn goto_workspace $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn goto_workspace $a -> $r $b] @workspace_status $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @goto_workspace $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn goto_workspace(&self, _workspace_path: &::std::path::Path, _pr: u64) -> ::anyhow::Result<()> { ::core::unimplemented!() }
        ] @workspace_status $($rest)*);
    };

    // ── workspace_status ────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @workspace_status async fn workspace_status $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn workspace_status $a -> $r $b] @list_workspaces $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @workspace_status $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn workspace_status(&self, _workspace_path: &::std::path::Path) -> ::anyhow::Result<$crate::CubeWorkspaceStatus> { ::core::unimplemented!() }
        ] @list_workspaces $($rest)*);
    };

    // ── list_workspaces ─────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces async fn list_workspaces $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn list_workspaces $a -> $r $b] @list_repos $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_workspaces $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn list_workspaces(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::CubeWorkspaceStatus>> { ::core::unimplemented!() }
        ] @list_repos $($rest)*);
    };

    // ── list_repos ──────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @list_repos async fn list_repos $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn list_repos $a -> $r $b] @command_repr $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @list_repos $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn list_repos(&self) -> ::anyhow::Result<::std::vec::Vec<$crate::CubeRepoSummary>> { ::core::unimplemented!() }
        ] @command_repr $($rest)*);
    };

    // ── command_repr (defaults to None) ─────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @command_repr fn command_repr $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* fn command_repr $a -> $r $b] @spawn_worker $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @command_repr $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            fn command_repr(&self, _args: &[&str]) -> ::core::option::Option<(::std::string::String, ::std::string::String)> { ::core::option::Option::None }
        ] @spawn_worker $($rest)*);
    };

    // ── spawn_worker ────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @spawn_worker async fn spawn_worker $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn spawn_worker $a -> $r $b] @read_transcript_tail_bytes $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @spawn_worker $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*
            async fn spawn_worker(&self, _worker_id: &str, _execution: &$crate::test_doubles::WorkExecution, _work_item: &$crate::test_doubles::WorkItem, _workspace_path: &::std::path::Path, _cube_change_id: ::core::option::Option<&str>) -> ::anyhow::Result<$crate::RunOutcome> { ::core::unimplemented!() }
        ] @read_transcript_tail_bytes $($rest)*);
    };

    // ── read_transcript_tail_bytes (trait default unless overridden) ─────────
    (@munch $ty:ty [$($acc:tt)*] @read_transcript_tail_bytes async fn read_transcript_tail_bytes $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn read_transcript_tail_bytes $a -> $r $b] @reattach_events_forward $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @read_transcript_tail_bytes $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @reattach_events_forward $($rest)*);
    };

    // ── reattach_events_forward (trait default unless overridden) ────────────
    (@munch $ty:ty [$($acc:tt)*] @reattach_events_forward async fn reattach_events_forward $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn reattach_events_forward $a -> $r $b] @probe_remote_worker_alive $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @reattach_events_forward $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @probe_remote_worker_alive $($rest)*);
    };

    // ── probe_remote_worker_alive (trait default unless overridden) ──────────
    (@munch $ty:ty [$($acc:tt)*] @probe_remote_worker_alive async fn probe_remote_worker_alive $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn probe_remote_worker_alive $a -> $r $b] @read_worker_log_tail $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @probe_remote_worker_alive $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @read_worker_log_tail $($rest)*);
    };

    // ── read_worker_log_tail (trait default unless overridden) ───────────────
    (@munch $ty:ty [$($acc:tt)*] @read_worker_log_tail async fn read_worker_log_tail $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn read_worker_log_tail $a -> $r $b] @collect_structured_output $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @read_worker_log_tail $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @collect_structured_output $($rest)*);
    };

    // ── collect_structured_output (trait default unless overridden) ──────────
    (@munch $ty:ty [$($acc:tt)*] @collect_structured_output async fn collect_structured_output $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn collect_structured_output $a -> $r $b] @probe_workspace_occupancy $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @collect_structured_output $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @probe_workspace_occupancy $($rest)*);
    };

    // ── probe_workspace_occupancy (trait default unless overridden) ──────────
    (@munch $ty:ty [$($acc:tt)*] @probe_workspace_occupancy async fn probe_workspace_occupancy $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn probe_workspace_occupancy $a -> $r $b] @done $($rest)*);
    };
    (@munch $ty:ty [$($acc:tt)*] @probe_workspace_occupancy $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)*] @done $($rest)*);
    };

    (@munch $ty:ty [$($acc:tt)*] @done async fn restore_preserved_work $a:tt -> $r:ty $b:block $($rest:tt)*) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn restore_preserved_work $a -> $r $b] @done $($rest)*);
    };

    (@munch $ty:ty [$($acc:tt)*] @done async fn confirm_workspace_recovery $a:tt -> $r:ty $b:block) => {
        $crate::stub_host_adapter!(@munch $ty [$($acc)* async fn confirm_workspace_recovery $a -> $r $b] @done);
    };

    // ── emit ────────────────────────────────────────────────────────────────
    (@munch $ty:ty [$($acc:tt)*] @done) => {
        #[::async_trait::async_trait]
        impl $crate::HostAdapter for $ty {
            $($acc)*
        }
    };
}

/// Test-only [`ExecutionPublisher`] that records every call it receives.
///
/// All three [`ExecutionPublisher`] call kinds are captured behind public
/// fields, so a single recorder serves every module that needs to assert on
/// publisher activity:
///
/// - `publish_calls` — `(execution_id, work_item_id, status, reason)` 4-tuples
///   from [`publish`](ExecutionPublisher::publish).
/// - `events` — `(product_id, work_item_id, reason)` triples from
///   [`publish_work_item_changed`](ExecutionPublisher::publish_work_item_changed).
/// - `typed_events` — `(product_id, event)` pairs from
///   [`publish_frontend_event_on_product`](ExecutionPublisher::publish_frontend_event_on_product).
///
/// The `conflict_watch`, `ci_watch`, `completion`, `coordinator`,
/// `merge_poller`, and `populator` test modules all previously hand-rolled a
/// near-duplicate recorder over some subset of these fields; this canonical
/// copy is a superset of every one, so it replaces all of them. Module-specific
/// query helpers live in the `impl` block below.
#[derive(Default)]
pub struct RecordingPublisher {
    pub publish_calls: Mutex<Vec<(String, String, String, String)>>,
    pub events: Mutex<Vec<(String, String, String)>>,
    /// The subset of `events` published through
    /// `publish_work_item_runtime_changed` (runtime-class invalidations).
    pub runtime_events: Mutex<Vec<(String, String, String)>>,
    pub typed_events: Mutex<Vec<(String, FrontendEvent)>>,
}

impl RecordingPublisher {
    /// Reasons published as runtime-class invalidations, in order.
    pub async fn runtime_reasons(&self) -> Vec<String> {
        self.runtime_events
            .lock()
            .await
            .iter()
            .map(|(_, _, reason)| reason.clone())
            .collect()
    }

    /// `publish_work_item_changed` reasons with poll-state housekeeping
    /// (`pr_poll_state_updated`) filtered out, so lifecycle-focused assertions
    /// don't have to account for the background sweep's bookkeeping writes.
    pub async fn lifecycle_reasons(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .filter(|(_, _, reason)| reason != "pr_poll_state_updated")
            .map(|(_, _, reason)| reason.clone())
            .collect()
    }

    /// Count of `CiFailureCleared` frontend events broadcast for `pr_url`.
    pub async fn ci_failure_cleared_count(&self, pr_url: &str) -> usize {
        self.typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e,
                    FrontendEvent::CiFailureCleared { pr_url: p, .. } if p == pr_url
                )
            })
            .count()
    }

    /// Count of `AttentionCreated` frontend events published.
    pub async fn attentions_created(&self) -> usize {
        self.typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, e)| matches!(e, FrontendEvent::AttentionCreated { .. }))
            .count()
    }

    /// Count of legacy `AttentionItemCreated` frontend events published.
    /// `FrontendEvent::AttentionItemCreated` is still a live wire variant
    /// published from several production sites outside the populator (e.g.
    /// `completion.rs`, `conflict_ladder.rs`) — this counter stays
    /// alongside `attentions_created` rather than replacing it.
    pub async fn attention_items_created(&self) -> usize {
        self.typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, e)| matches!(e, FrontendEvent::AttentionItemCreated { .. }))
            .count()
    }

    /// `Some(n)` — a `WorkItemsCreated` event was published carrying `n`
    /// items. `None` if no such event was published.
    pub async fn work_items_created_len(&self) -> Option<usize> {
        self.typed_events.lock().await.iter().find_map(|(_, e)| match e {
            FrontendEvent::WorkItemsCreated { items } => Some(items.len()),
            _ => None,
        })
    }
}

#[async_trait]
impl ExecutionPublisher for RecordingPublisher {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str) {
        self.publish_calls.lock().await.push((
            execution_id.to_owned(),
            work_item_id.to_owned(),
            status.to_owned(),
            reason.to_owned(),
        ));
    }
    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.events
            .lock()
            .await
            .push((product_id.to_owned(), work_item_id.to_owned(), reason.to_owned()));
    }
    async fn publish_work_item_runtime_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.runtime_events
            .lock()
            .await
            .push((product_id.to_owned(), work_item_id.to_owned(), reason.to_owned()));
        self.publish_work_item_changed(product_id, work_item_id, reason).await;
    }
    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
        let projection = match &event {
            FrontendEvent::ConflictResolutionStarted {
                attempt, attempt_id, ..
            }
            | FrontendEvent::CiRemediationStarted {
                attempt, attempt_id, ..
            } => Some((attempt, attempt_id, "started")),
            FrontendEvent::ConflictResolutionSucceeded {
                attempt, attempt_id, ..
            }
            | FrontendEvent::CiRemediationSucceeded {
                attempt, attempt_id, ..
            } => Some((attempt, attempt_id, "succeeded")),
            FrontendEvent::ConflictResolutionFailed {
                attempt, attempt_id, ..
            }
            | FrontendEvent::CiRemediationFailed {
                attempt, attempt_id, ..
            } => Some((attempt, attempt_id, "failed")),
            FrontendEvent::ConflictResolutionAbandoned {
                attempt, attempt_id, ..
            }
            | FrontendEvent::CiRemediationAbandoned {
                attempt, attempt_id, ..
            } => Some((attempt, attempt_id, "abandoned")),
            _ => None,
        };
        if let Some((row, id, status)) = projection {
            assert_eq!(&row.id, id);
            assert_eq!(row.product_id, product_id);
            assert!(row.work_item_id.is_some());
            assert!(!row.created_at.is_empty());
            if status == "started" {
                assert!(matches!(row.status.as_str(), "pending" | "running"));
            } else {
                assert_eq!(row.status, status);
                assert!(row.finished_at.is_some());
            }
        }
        self.typed_events.lock().await.push((product_id.to_owned(), event));
    }
}

/// Test-only routing for shared cube clients, including heartbeat fixtures.
/// The implementation lives with the trait because downstream crates cannot
/// implement this external trait for the external Arc type (the orphan rule).
/// Route every execution to the one cube — the single-cube shape tests model,
/// which is exactly right for a local-only engine. Multi-host routing needs
/// its own fixture, because a stub that answers for every execution cannot
/// tell a correctly routed call from a misrouted one.
#[async_trait]
impl<T: CubeClient + 'static> ExecutionCubes for Arc<T> {
    async fn cube_for_execution(&self, _execution_id: &str) -> Result<ResolvedExecutionCube> {
        Ok(ResolvedExecutionCube {
            cube: Arc::clone(self) as Arc<dyn CubeClient>,
            host_id: "local".to_owned(),
            host_adapter: None,
        })
    }
}
