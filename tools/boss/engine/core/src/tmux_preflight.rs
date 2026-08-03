//! Startup validation for Boss's required local tmux runtime.

use std::path::PathBuf;

use boss_tmux::{MINIMUM_VERSION, Tmux, TmuxVersion};

/// Result of resolving and version-checking the one tmux executable the
/// engine may use for its lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxPreflight {
    Ready { program: PathBuf, version: TmuxVersion },
    Unavailable { reason: String },
}

impl TmuxPreflight {
    pub async fn probe() -> Self {
        let tmux = match Tmux::resolve() {
            Ok(tmux) => tmux,
            Err(error) => {
                return Self::Unavailable {
                    reason: missing_tmux_reason(&error.to_string()),
                };
            }
        };
        let program = tmux.program().to_path_buf();
        match tmux.version().await {
            Ok(version) if version >= MINIMUM_VERSION => Self::Ready { program, version },
            Ok(version) => Self::Unavailable {
                reason: format!(
                    "Boss requires tmux {}.{} or newer, but {} is tmux {}.{}. Install it with `brew install tmux`.",
                    MINIMUM_VERSION.major,
                    MINIMUM_VERSION.minor,
                    program.display(),
                    version.major,
                    version.minor,
                ),
            },
            Err(error) => Self::Unavailable {
                reason: format!(
                    "Boss could not run tmux at {}: {error}. Install or repair it with `brew install tmux`.",
                    program.display(),
                ),
            },
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Ready { .. } => None,
            Self::Unavailable { reason } => Some(reason),
        }
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
    fn unavailable_reason_names_the_version_and_install_command() {
        let reason = missing_tmux_reason("not found");
        assert!(reason.contains("3.2"));
        assert!(reason.contains("brew install tmux"));
    }
}
