# Boss macOS app — worker guidance

Rules specific to `tools/boss/app-macos`. The repo-root `AGENTS.md` still applies.

## No modals for non-actionable status or errors

**Never use a modal dialog for a non-actionable status or error message in the Boss macOS app** (one the user can do nothing about: engine busy/locked/retry errors, a reveal that failed, a doc pointer that is broken, a background save that hiccuped). Modals steal keyboard focus mid-typing, block every other control, and re-pop for each repeat of a message that arrives unprompted. Use inline, banner, or notification surfaces that don't steal focus, or just log it.

**Modal confirmations and decisions about an action the user just took are fine** — "Merge while revisions are open?", "Dispatch is paused — Start anyway?", the quit confirmation, destructive-action `confirmationDialog`s, and sheets that collect input the user asked to give. Leave those as they are.

Surfaces to use, in order of preference:

- **Inline, next to what failed** — a caption/`Label` beside the control (`IdeasView`'s send-failure note, `WorkDispatchFailureBanner` on a card, `mergeErrorNoticesByTaskID` in the review-guide header). Prefer this whenever the failure belongs to one control or one card.
- **Chrome banner** — set `ChatViewModel.workErrorMessage` for a failure with no narrower home; `ContentView.chromeBanners` renders it as the dismissable `WorkNoticeBanner` in the titlebar accessory slot next to the connection/health banners. A newer message replaces the text in place. Every value is also written to the unified log (`dev.spinyfin.bossmacapp` / `work-notice`).
- **Notifications** — the existing `ReviewNotificationCenter` surface, for events worth a persistent record.
- **Log only** — for a condition the user can neither act on nor benefit from seeing (transport retries already go through `appendSystemMessage`).

Enforcement: `ReportOnlyAlertScanTests` (via `Tests/BossTests/scan_for_report_only_alerts.sh`, run as a Bazel build action over `Sources/`) fails on any SwiftUI `.alert(isPresented:)` or `NSAlert` with fewer than two buttons, and on any `.alert` bound to `workErrorMessage`. A one-button alert is a report, not a decision. The scan does not live in `checkleft`: its line-scoped `text/forbidden-pattern` check cannot see an alert's action block, and an alert's title does not reliably say whether it is a decision, so a regex lint would flag confirmations or miss reports. Do not work around the scan by adding a second throwaway button to a report; move the message to one of the surfaces above.
