//! Advisory tmux crash detection. Never participates in dispatch admission.

use boss_protocol::EngineHealthIssue;
use boss_tmux::{Tmux, TmuxVersion};
use std::sync::LazyLock;
use tokio::sync::broadcast;

static SERVER_OBSERVATIONS: LazyLock<broadcast::Sender<Tmux>> = LazyLock::new(|| broadcast::channel(64).0);

pub(crate) fn server_observed(tmux: &Tmux) {
    let _ = SERVER_OBSERVATIONS.send(tmux.clone());
}

pub(crate) fn subscribe() -> broadcast::Receiver<Tmux> {
    SERVER_OBSERVATIONS.subscribe()
}

pub(crate) fn version_warning(source: &str, output: &str) -> Option<EngineHealthIssue> {
    let version = TmuxVersion::parse(output).ok();
    // Introduced by tmux 2bb0b9d6 (first released in 3.3); fixed by
    // 7a816f90 (first released in 3.8). Development names without a
    // numeric version cannot establish whether that fix is present.
    if version.is_some_and(|v| v < (TmuxVersion { major: 3, minor: 3 }) || v >= (TmuxVersion { major: 3, minor: 8 })) {
        return None;
    }
    let detail = if version.is_some() {
        format!("{source} ({output}) has a known focus-event crash that can kill every session at once.")
    } else {
        format!(
            "The version of {source} ({output}) could not be verified; it may have a known focus-event crash that can kill every session at once."
        )
    };
    Some(EngineHealthIssue {
        kind: "tmux_focus_out_crash".to_owned(),
        severity: "warning".to_owned(),
        title: if version.is_some() {
            format!("{source} ({output}) can crash every session — upgrade to 3.8 or newer")
        } else {
            "tmux crash-fix status is unknown — upgrade to 3.8 or newer".to_owned()
        },
        body: format!(
            "{detail} Upgrade with `brew upgrade tmux` to tmux 3.8 or newer. An already-running server keeps its old version: after active work finishes, restart Boss and its tmux server. Boss continues running and detaches viewers before session teardown."
        ),
    })
}

pub(crate) async fn installed_warning(tmux: &Tmux) -> Option<EngineHealthIssue> {
    let output = match tokio::time::timeout(std::time::Duration::from_secs(3), tmux.version_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => format!("version probe failed: {error}"),
        Err(_) => "version probe timed out".to_owned(),
    };
    version_warning("Installed tmux", &output)
}

pub(crate) async fn server_warning(tmux: &Tmux) -> Option<EngineHealthIssue> {
    let output = match tokio::time::timeout(std::time::Duration::from_secs(3), tmux.server_version_output()).await {
        Ok(Ok(Some(output))) => output,
        Ok(Ok(None)) => return None,
        Ok(Err(error)) => format!("version probe failed: {error}"),
        Err(_) => "version probe timed out".to_owned(),
    };
    version_warning("Running tmux server", &output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affected_and_unknown_versions_warn_without_a_refusal() {
        for version in ["3.3", "3.6a", "3.7c", "master", "", "3.8garbage", "3.8.1", "999999.8"] {
            let issue = version_warning("Installed tmux", &format!("tmux {version}")).unwrap();
            assert_eq!(issue.severity, "warning");
            assert!(issue.body.contains("every session at once"));
            assert!(issue.body.contains("brew upgrade tmux"));
        }
        for version in ["3.2", "3.2a", "3.8", "3.8a", "next-3.9", "4.0"] {
            assert!(
                version_warning("Installed tmux", &format!("tmux {version}")).is_none(),
                "{version}"
            );
        }
    }
}
