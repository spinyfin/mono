use super::{environment, home};
use crate::scoped_home::SCOPED_HOME_ENV_TEST_LOCK;
use std::ffi::OsString;
use std::path::Path;

/// Write the real permission-renderer output for `kind` into the run's
/// control directory, exactly as `write_permission_config` does before the
/// authorization adapter is installed.
pub(crate) fn write_final_settings(run_id: &str, workspace: &Path, kind: crate::WorkerKind, events_socket: &Path) {
    let input = crate::PermissionInput::builder()
        .worker_kind(kind)
        .workspace_path(workspace.to_path_buf())
        .events_socket_path(events_socket.to_path_buf())
        .boss_event_path(events_socket.with_file_name("boss-event"))
        .run_id(run_id)
        .lease_id("lease-1")
        .execution_kind("task_implementation")
        .is_remote(false)
        .build();
    super::permissions::write_session_settings(&input).unwrap();
}

pub(crate) struct EnvGuard {
    _lock: Option<std::sync::MutexGuard<'static, ()>>,
    prior: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    pub(crate) fn set(pairs: &[(&'static str, &str)]) -> Self {
        let lock = SCOPED_HOME_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        Self::set_with_lock(pairs, Some(lock))
    }

    pub(crate) fn set_with_lock(
        pairs: &[(&'static str, &str)],
        lock: Option<std::sync::MutexGuard<'static, ()>>,
    ) -> Self {
        let mut prior = Vec::new();
        for (key, value) in pairs {
            prior.push((*key, std::env::var_os(key)));
            // SAFETY: serialised by SCOPED_HOME_ENV_TEST_LOCK.
            unsafe { std::env::set_var(key, value) };
        }
        Self { _lock: lock, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.prior {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}

pub(crate) fn env_for_provision(homes: &Path) -> EnvGuard {
    let lock = SCOPED_HOME_ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    env_for_provision_with_lock(homes, Some(lock))
}

pub(crate) fn env_for_provision_with_home_override(homes: &Path) -> EnvGuard {
    env_for_provision_with_lock(homes, None)
}

fn env_for_provision_with_lock(homes: &Path, lock: Option<std::sync::MutexGuard<'static, ()>>) -> EnvGuard {
    EnvGuard::set_with_lock(
        &[
            (home::AGY_HOMES_ROOT_ENV, homes.to_str().unwrap()),
            (home::AGY_SKIP_POSTURE_ASSERT_ENV, "1"),
            (environment::AGY_SKIP_BINARY_ASSERT_ENV, "1"),
        ],
        lock,
    )
}
