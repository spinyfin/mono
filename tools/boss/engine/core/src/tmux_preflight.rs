//! Startup validation for Boss's required local tmux runtime.

use std::path::PathBuf;
use std::time::Duration;

use boss_tmux::{MINIMUM_VERSION, Tmux, TmuxVersion};

const TMUX_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Result of resolving and version-checking the one tmux executable the
/// engine may use for its lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxPreflight {
    Ready { program: PathBuf, version: TmuxVersion },
    ReadyUnknown { program: PathBuf },
    Unavailable { reason: String },
}

impl TmuxPreflight {
    pub async fn probe() -> Self {
        let socket =
            boss_log_files::default_tmux_socket_path().unwrap_or_else(|| PathBuf::from("/state/boss/tmux.sock"));
        Self::probe_with_socket(&socket).await
    }

    pub async fn probe_with_socket(socket_path: &std::path::Path) -> Self {
        let tmux = match Tmux::resolve(socket_path) {
            Ok(tmux) => tmux,
            Err(error) => {
                return Self::Unavailable {
                    reason: missing_tmux_reason(&error.to_string()),
                };
            }
        };
        let program = tmux.program().to_path_buf();
        match tokio::time::timeout(TMUX_VERSION_PROBE_TIMEOUT, tmux.version_output()).await {
            Ok(Ok(output)) => classify_output(program, &output),
            Ok(Err(error)) => classify(program, Err(error)),
            Err(_) => Self::Unavailable {
                reason: timeout_reason(&program),
            },
        }
    }

    /// Probe the original version label using the engine health warning policy.
    pub async fn crash_warning(&self) -> Option<boss_protocol::EngineHealthIssue> {
        let program = match self {
            Self::Ready { program, .. } | Self::ReadyUnknown { program } => program,
            Self::Unavailable { .. } => return None,
        };
        match Tmux::from_path_with_socket(program, "/unused-version-probe.sock") {
            Ok(tmux) => crate::tmux_health::installed_warning(&tmux).await,
            Err(error) => crate::tmux_health::version_warning("Installed tmux", &error.to_string()),
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Ready { .. } | Self::ReadyUnknown { .. } => None,
            Self::Unavailable { reason } => Some(reason),
        }
    }
}

fn classify_output(program: PathBuf, output: &str) -> TmuxPreflight {
    match TmuxVersion::parse(output) {
        Ok(version) => classify(program, Ok(version)),
        // Development builds remain usable; health reports explain the unknown risk.
        Err(_) => TmuxPreflight::ReadyUnknown { program },
    }
}

fn timeout_reason(program: &std::path::Path) -> String {
    format!(
        "tmux at {} did not respond to `tmux -V` within {}s",
        program.display(),
        TMUX_VERSION_PROBE_TIMEOUT.as_secs(),
    )
}

fn classify(program: PathBuf, version: anyhow::Result<TmuxVersion>) -> TmuxPreflight {
    match version {
        Ok(version) if version.supports_session_environment() => TmuxPreflight::Ready { program, version },
        Ok(version) => TmuxPreflight::Unavailable {
            reason: format!(
                "Boss requires tmux {}.{} or newer, but {} is tmux {}.{}. Install it with `brew install tmux`.",
                MINIMUM_VERSION.major,
                MINIMUM_VERSION.minor,
                program.display(),
                version.major,
                version.minor,
            ),
        },
        Err(error) => TmuxPreflight::Unavailable {
            reason: format!(
                "Boss could not run tmux at {}: {error}. Install or repair it with `brew install tmux`.",
                program.display(),
            ),
        },
    }
}

fn missing_tmux_reason(error: &str) -> String {
    format!(
        "Boss requires tmux {}.{} or newer for durable worker sessions. Install it with `brew install tmux`. ({error})",
        MINIMUM_VERSION.major, MINIMUM_VERSION.minor
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_affected_and_unknown_versions_do_not_block_startup() {
        for output in [
            "tmux 3.3",
            "tmux 3.6a",
            "tmux 3.7c",
            "tmux 3.8",
            "tmux next-3.9",
            "tmux master",
            "unknown",
        ] {
            assert!(
                classify_output(PathBuf::from("/usr/local/bin/tmux"), output)
                    .unavailable_reason()
                    .is_none(),
                "{output}"
            );
        }
    }

    #[test]
    fn unavailable_reason_names_the_version_and_install_command() {
        let reason = missing_tmux_reason("not found");
        assert!(reason.contains("3.2"));
        assert!(reason.contains("brew install tmux"));
    }

    #[test]
    fn below_floor_version_is_unavailable_with_both_versions() {
        let preflight = classify(
            PathBuf::from("/usr/local/bin/tmux"),
            Ok(TmuxVersion { major: 3, minor: 1 }),
        );
        let reason = preflight.unavailable_reason().expect("below floor is unavailable");
        assert!(reason.contains("3.1"));
        assert!(reason.contains("3.2"));
    }

    #[test]
    fn floor_version_is_ready() {
        assert!(matches!(
            classify(PathBuf::from("/usr/local/bin/tmux"), Ok(MINIMUM_VERSION)),
            TmuxPreflight::Ready { .. }
        ));
    }

    #[test]
    fn execution_failure_is_unavailable() {
        assert!(matches!(
            classify(
                PathBuf::from("/usr/local/bin/tmux"),
                Err(anyhow::anyhow!("permission denied")),
            ),
            TmuxPreflight::Unavailable { .. }
        ));
    }

    #[test]
    fn timeout_reason_names_program_and_duration() {
        let reason = timeout_reason(std::path::Path::new("/usr/local/bin/tmux"));
        assert!(reason.contains("/usr/local/bin/tmux"));
        assert!(reason.contains("within 3s"));
    }
}
