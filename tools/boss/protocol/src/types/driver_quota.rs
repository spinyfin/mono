//! Per-driver provider quota usage: what each coding-agent CLI's own
//! provider says is left of the maintainer's subscription window.
//!
//! # What this is, and what it deliberately is not
//!
//! Every figure here originates from the *provider*, obtained through the
//! driver's own CLI or the endpoint that CLI reads. Boss's internal token
//! accounting (`cost_report`, the per-execution spend rows) is a different
//! number measuring a different thing, and the two must never be presented
//! as interchangeable — Boss sees only the work Boss dispatched, the
//! provider sees the whole subscription. Nothing in this module is derived
//! from Boss-side accounting.
//!
//! # Why failure is modelled, not swallowed
//!
//! A quota display that renders a broken fetch as blank, as zero, or as a
//! dash reads exactly like "plenty of headroom" at the moment the maintainer
//! most needs the truth. So there is no `Option<f64>` here: an entry is
//! either a [`DriverQuotaOutcome::Reading`] carrying a real provider figure,
//! or a [`DriverQuotaOutcome::Unavailable`] carrying a typed
//! [`DriverQuotaFailureKind`] and a short human reason. A healthy 0% and a
//! failed probe are different variants, so no renderer can confuse them
//! even by accident.
//!
//! Staleness is modelled the same way: every entry carries its own
//! `observed_at_epoch_s`, so "3% used" is never shown without the "as of"
//! that makes it meaningful.

use serde::{Deserialize, Serialize};

/// The window a [`DriverQuotaReading`] describes. Each provider expresses
/// its own limit period differently; this normalises the common case
/// (a rolling week — what all three drivers currently report) while keeping
/// anything else expressible rather than silently relabelled as weekly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverQuotaWindow {
    /// A rolling seven-day window.
    Weekly,
    /// Any other window the provider reported, in whole minutes. Kept
    /// distinct so a provider that changes its window length surfaces as
    /// "over 43200 minutes" rather than being mislabelled "this week".
    Other { minutes: u32 },
}

impl DriverQuotaWindow {
    /// Classify a provider-reported window length. Exactly one week becomes
    /// [`Self::Weekly`]; anything else stays [`Self::Other`] verbatim.
    pub fn from_minutes(minutes: u32) -> Self {
        if minutes == MINUTES_PER_WEEK {
            Self::Weekly
        } else {
            Self::Other { minutes }
        }
    }
}

/// Minutes in a seven-day window — the value Codex reports as
/// `windowDurationMins` for its weekly limit.
pub const MINUTES_PER_WEEK: u32 = 7 * 24 * 60;

/// One driver's successfully-read provider figure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverQuotaReading {
    /// Percentage of the window consumed, as the provider reported it.
    /// Not rounded, not clamped, not derived from anything else.
    pub used_percent: f64,
    /// The window `used_percent` is a fraction of.
    pub window: DriverQuotaWindow,
    /// Epoch seconds at which the window resets, when the provider gives a
    /// machine-readable instant. `None` is normal — see `resets_at_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_epoch_s: Option<i64>,
    /// The provider's own human-readable reset phrasing, kept verbatim when
    /// that is all it offers (Claude's `/usage` prints "Aug 25 at 7pm
    /// (America/Chicago)" and no timestamp). Rendered as-is rather than
    /// parsed into an instant Boss would then have to guess the year for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_text: Option<String>,
    /// Short provenance string naming the mechanism that produced this
    /// figure (e.g. `claude -p /usage`). Shown in the UI so the maintainer
    /// can tell at a glance which of the three different mechanisms
    /// answered, and never has to wonder whether a number is Boss's own
    /// accounting wearing a provider label.
    pub source: String,
}

/// Why a driver's quota could not be read. Distinct variants because the
/// remedies differ: a missing CLI is an install, an expired token is a
/// login, an unparseable output is a Boss-side parser fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverQuotaFailureKind {
    /// The driver's CLI is not installed or not on `PATH`.
    NotInstalled,
    /// The CLI ran (or the endpoint answered) but the operator is not
    /// logged in, or the stored credential has expired.
    NotAuthenticated,
    /// The probe ran and failed: non-zero exit, transport error, provider
    /// error response.
    ProbeFailed,
    /// The probe produced output Boss could not turn into a figure. A
    /// distinct kind because it means the provider changed its format and
    /// the parser needs updating — not that the maintainer did anything.
    Unparseable,
    /// The probe exceeded its per-driver deadline.
    Timeout,
}

impl DriverQuotaFailureKind {
    /// Stable lowercase slug, for logs and the app's wire decoding.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::NotAuthenticated => "not_authenticated",
            Self::ProbeFailed => "probe_failed",
            Self::Unparseable => "unparseable",
            Self::Timeout => "timeout",
        }
    }
}

/// The result of one probe attempt: a real figure, or a typed failure.
/// There is deliberately no third "unknown" state — see the module doc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DriverQuotaOutcome {
    Reading(DriverQuotaReading),
    Unavailable {
        kind: DriverQuotaFailureKind,
        /// One short sentence the UI shows verbatim. Must never contain a
        /// credential, token, or session identifier — probe implementations
        /// are responsible for keeping provider error bodies out of here.
        reason: String,
    },
}

/// One driver's entry in a [`DriverQuotaSnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverQuotaEntry {
    /// Driver slug — one of [`crate::DRIVER_SLUG_CLAUDE`],
    /// [`crate::DRIVER_SLUG_CODEX`], [`crate::DRIVER_SLUG_GROK`].
    pub driver: String,
    /// Epoch seconds when this outcome was produced. Present on failures
    /// too: "we tried at 14:03 and it failed" is information.
    pub observed_at_epoch_s: i64,
    pub outcome: DriverQuotaOutcome,
}

/// Everything the Preferences pane renders: one entry per implemented
/// driver, plus when the last probe cycle ran.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DriverQuotaSnapshot {
    /// One entry per driver, in [`DRIVER_QUOTA_ORDER`]. Empty only before
    /// the first probe cycle has ever run, which is exactly the state
    /// `generated_at_epoch_s: None` marks.
    #[serde(default)]
    pub entries: Vec<DriverQuotaEntry>,
    /// Epoch seconds when the most recent probe cycle finished. `None`
    /// means no cycle has run yet in this engine process — the UI shows
    /// "not checked yet", never a zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at_epoch_s: Option<i64>,
    /// `true` when an explicit refresh was declined because one ran too
    /// recently, so the UI can say "showing the cached figures" instead of
    /// leaving the maintainer to wonder why the timestamp did not move.
    #[serde(default)]
    pub refresh_throttled: bool,
}

/// Display and probe order for the three implemented drivers. Alphabetical
/// so the pane reads the same way every time regardless of which probe
/// finished first.
pub const DRIVER_QUOTA_ORDER: [&str; 3] = [
    super::DRIVER_SLUG_CLAUDE,
    super::DRIVER_SLUG_CODEX,
    super::DRIVER_SLUG_GROK,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_window_recognised_from_minutes() {
        assert_eq!(DriverQuotaWindow::from_minutes(10080), DriverQuotaWindow::Weekly);
    }

    #[test]
    fn non_weekly_window_kept_verbatim_rather_than_relabelled() {
        assert_eq!(
            DriverQuotaWindow::from_minutes(300),
            DriverQuotaWindow::Other { minutes: 300 }
        );
    }

    #[test]
    fn healthy_zero_and_failure_are_different_wire_shapes() {
        let healthy = serde_json::to_value(DriverQuotaOutcome::Reading(DriverQuotaReading {
            used_percent: 0.0,
            window: DriverQuotaWindow::Weekly,
            resets_at_epoch_s: None,
            resets_at_text: None,
            source: "probe".to_owned(),
        }))
        .expect("serialize");
        let failed = serde_json::to_value(DriverQuotaOutcome::Unavailable {
            kind: DriverQuotaFailureKind::NotAuthenticated,
            reason: "not logged in".to_owned(),
        })
        .expect("serialize");

        assert_eq!(healthy["state"], "reading");
        assert_eq!(healthy["used_percent"], 0.0);
        assert_eq!(failed["state"], "unavailable");
        assert_eq!(failed["kind"], "not_authenticated");
        // The failure carries no percentage at all, so no renderer can read
        // one off it and show a plausible-looking zero.
        assert!(failed.get("used_percent").is_none());
    }

    #[test]
    fn empty_snapshot_reports_never_probed_rather_than_epoch_zero() {
        let snapshot = DriverQuotaSnapshot::default();
        assert!(snapshot.entries.is_empty());
        assert_eq!(snapshot.generated_at_epoch_s, None);
        let wire = serde_json::to_value(&snapshot).expect("serialize");
        assert!(wire.get("generated_at_epoch_s").is_none());
    }

    #[test]
    fn snapshot_round_trips_over_the_wire() {
        let snapshot = DriverQuotaSnapshot {
            entries: vec![
                DriverQuotaEntry {
                    driver: "claude".to_owned(),
                    observed_at_epoch_s: 1_787_000_000,
                    outcome: DriverQuotaOutcome::Reading(DriverQuotaReading {
                        used_percent: 3.0,
                        window: DriverQuotaWindow::Weekly,
                        resets_at_epoch_s: None,
                        resets_at_text: Some("Aug 25 at 7pm (America/Chicago)".to_owned()),
                        source: "claude -p /usage".to_owned(),
                    }),
                },
                DriverQuotaEntry {
                    driver: "codex".to_owned(),
                    observed_at_epoch_s: 1_787_000_001,
                    outcome: DriverQuotaOutcome::Unavailable {
                        kind: DriverQuotaFailureKind::Timeout,
                        reason: "app-server did not answer within 20s".to_owned(),
                    },
                },
            ],
            generated_at_epoch_s: Some(1_787_000_002),
            refresh_throttled: true,
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let back: DriverQuotaSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, snapshot);
    }

    #[test]
    fn order_covers_every_implemented_driver_exactly_once() {
        let mut sorted = DRIVER_QUOTA_ORDER;
        sorted.sort_unstable();
        assert_eq!(sorted, ["claude", "codex", "grok"]);
    }
}
