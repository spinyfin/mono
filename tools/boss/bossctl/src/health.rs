//! `bossctl health` — the engine's health report, including whether the
//! running engine is older than the newest published release.
//!
//! The engine does not poll for releases. "Newest published" is whatever
//! the Boss app's updater last reported to it, so it reads `unknown`
//! until an app has connected and checked.

use anyhow::{Context, Result, bail};
use boss_protocol::{EngineHealthReport, EngineReleaseStatus, FrontendEvent, FrontendRequest};

pub async fn run(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = crate::connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetEngineHealth)
        .await
        .context("sending GetEngineHealth")?;
    let report = match response {
        FrontendEvent::EngineHealthResult { report } => report,
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected health: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("EngineHealthReport serializes")
        );
    } else {
        print!("{}", render(&report));
    }
    Ok(())
}

/// One line saying how the running engine compares with the newest
/// published release, always naming both versions.
fn release_status_line(report: &EngineHealthReport) -> String {
    let newest = report.newest_published_release.as_deref();
    let dev = if report.engine_is_dev_build {
        " (dev build, never auto-installed over)"
    } else {
        ""
    };
    match (report.engine_release_status, newest) {
        (EngineReleaseStatus::Behind, Some(newest)) => {
            format!("BEHIND — running {} < published {newest}{dev}", report.engine_version)
        }
        (EngineReleaseStatus::Current, Some(newest)) => {
            format!("current — running {} >= published {newest}{dev}", report.engine_version)
        }
        (_, None) => "unknown — no published release reported by the app updater yet".to_owned(),
        (_, Some(newest)) => format!(
            "unknown — cannot compare running {} with published {newest}",
            report.engine_version
        ),
    }
}

fn render(report: &EngineHealthReport) -> String {
    let mut out = String::new();
    out.push_str("engine health\n");
    out.push_str(&format!("  engine_version:             {}\n", report.engine_version));
    out.push_str(&format!("  engine_git_sha:             {}\n", report.engine_git_sha));
    out.push_str(&format!(
        "  newest_published_release:   {}\n",
        report.newest_published_release.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  release_status:             {}\n",
        release_status_line(report)
    ));
    out.push_str(&format!(
        "  anthropic_api_key_present:  {}\n",
        if report.anthropic_api_key_present { "yes" } else { "NO" }
    ));
    out.push_str(&format!("  dispatch_paused:            {}\n", report.dispatch_paused));
    out.push_str(&format!("  automation_paused:          {}\n", report.automation_paused));
    if report.issues.is_empty() {
        out.push_str("  issues:                     none\n");
    } else {
        out.push_str("  issues:\n");
        for issue in &report.issues {
            out.push_str(&format!("    [{}] {}: {}\n", issue.severity, issue.kind, issue.title));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(version: &str, newest: Option<&str>) -> EngineHealthReport {
        let freshness = boss_protocol::engine_release_freshness(version, newest);
        EngineHealthReport::builder()
            .engine_version(version)
            .engine_git_sha("abc123")
            .maybe_newest_published_release(newest)
            .engine_release_status(freshness.status)
            .engine_is_dev_build(freshness.is_dev_build)
            .anthropic_api_key_present(true)
            .dispatch_paused(false)
            .automation_paused(false)
            .issues(vec![])
            .build()
    }

    #[test]
    fn behind_names_both_versions() {
        let text = render(&report("1.0.685", Some("1.0.686")));
        assert!(text.contains("BEHIND — running 1.0.685 < published 1.0.686"), "{text}");
    }

    #[test]
    fn equal_is_current() {
        let text = render(&report("1.0.686", Some("1.0.686")));
        assert!(
            text.contains("current — running 1.0.686 >= published 1.0.686"),
            "{text}"
        );
    }

    #[test]
    fn dev_build_behind_is_shown_not_hidden() {
        let text = render(&report("1.0.685-dev-abc1234", Some("1.0.686")));
        assert!(
            text.contains("BEHIND — running 1.0.685-dev-abc1234 < published 1.0.686"),
            "{text}"
        );
        assert!(text.contains("dev build"), "{text}");
    }

    #[test]
    fn unknown_cases_say_unknown() {
        let text = render(&report("1.0.686", None));
        assert!(
            text.contains("release_status:             unknown — no published release"),
            "{text}"
        );
        let text = render(&report("unknown", Some("1.0.686")));
        assert!(
            text.contains("unknown — cannot compare running unknown with published 1.0.686"),
            "{text}"
        );
    }
}
