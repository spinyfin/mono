import SwiftUI

extension WorkTask {
    /// Available on root PR cards, including merged and closed work.
    var generateReviewGuideMenuTitle: String? {
        guard kind != "revision", let prURL, !prURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
        return reviewGuideReadableVersionId == nil ? "Generate Review Guide" : "Regenerate Review Guide"
    }
}

// ===========================================================================
// PR review-guide wire types and card/popover presentation.
//
// The card badge and popover row need the scalar `WorkTask` fields
// (`reviewGuideLifecycle` / `reviewGuideReadableVersionId` /
// `reviewGuideError`, mirroring the `Task.review_guide_*` wire fields) —
// they arrive on every normal task read/push, exactly like
// `ciRequiredState`, so no separate summary fetch is needed to paint the
// affordance. Only the async markdown viewer needs the version's full
// Markdown, fetched on demand via `GetReviewGuideContent`.
//
// See tools/boss/docs/designs/automatic-pr-review-guides.md, "Review card
// and viewer".
// ===========================================================================

/// One immutable, validated guide version's full content — the
/// `GetReviewGuideContent` RPC's reply payload. Mirrors
/// `boss_protocol::ReviewGuideVersion` field-for-field.
struct ReviewGuideVersionContent: Codable, Equatable {
    let id: String
    let seriesId: String
    let comparisonId: String
    let attemptId: String
    let markdown: String
    let contentHash: String
    let promptVersion: String
    let generatedAt: String

    enum CodingKeys: String, CodingKey {
        case id
        case seriesId = "series_id"
        case comparisonId = "comparison_id"
        case attemptId = "attempt_id"
        case markdown
        case contentHash = "content_hash"
        case promptVersion = "prompt_version"
        case generatedAt = "generated_at"
    }
}

/// One durable generation attempt's diagnostic state — the
/// `RetryReviewGuide` RPC's reply payload. Mirrors
/// `boss_protocol::ReviewGuideAttempt` field-for-field.
struct ReviewGuideAttempt: Codable, Equatable {
    let id: String
    let seriesId: String
    let comparisonId: String
    let requestEpoch: Int
    /// One of `"queued"` / `"running"` / `"succeeded"` / `"failed"` /
    /// `"cancelled"` / `"superseded"`.
    let status: String
    let error: String?

    enum CodingKeys: String, CodingKey {
        case id
        case seriesId = "series_id"
        case comparisonId = "comparison_id"
        case requestEpoch = "request_epoch"
        case status
        case error
    }
}

/// Compact per-card presentation for the Review-card guide affordance —
/// derived purely from `WorkTask.reviewGuideLifecycle` /
/// `reviewGuideReadableVersionId`, matching the design's "Review card and
/// viewer" brief-state table exactly (five states, all mechanically
/// derivable from those two fields: `queued`/`ready`/`failed` crossed with
/// whether a readable version already exists).
struct ReviewGuideCardPresentation: Equatable {
    enum Kind: Equatable {
        /// Queued or generating, no previous content — no document button,
        /// only an indeterminate spinner.
        case generating
        /// Queued or generating, but an earlier version remains open.
        case refreshing
        /// A readable version exists and generation is not in flight.
        case ready
        /// Generation failed and there is no previous content.
        case failed
        /// Generation failed but an earlier version remains open.
        case refreshFailed
    }

    let kind: Kind
    /// The version to open, when a document button should be shown at all
    /// (`ready`, `refreshing`, `refreshFailed`).
    let readableVersionId: String?
    /// Mirrors `WorkTask.reviewGuideStaleSource` — only meaningful for
    /// `.refreshFailed`, where it distinguishes genuine source staleness
    /// (the PR's head moved) from a same-comparison prompt/prose retry
    /// failure. Defaulted `false` for callers that only need the other four
    /// states, which never consult it.
    var staleSource: Bool = false
    /// Engine-provided failure text (`WorkTask.reviewGuideError`). Rendered
    /// as-is; the app does not infer a cause. `nil` unless `kind` is a
    /// failed state and the engine stored a reason.
    var error: String? = nil

    var showsDocumentButton: Bool { readableVersionId != nil }
    var showsProgress: Bool { kind == .generating || kind == .refreshing }
    var showsRetry: Bool { kind == .failed || kind == .refreshFailed }

    /// First line of `error`, truncated for the summary chip. Full text
    /// stays on `error` for hover/expand.
    var errorSummary: String? {
        Self.summaryLine(of: error)
    }

    var accessibilityLabel: String {
        switch kind {
        case .generating: return "Generating review guide"
        case .refreshing: return "Open older review guide; updating"
        case .ready: return "Open review guide"
        case .failed:
            if let errorSummary { return "Review guide failed to generate: \(errorSummary)" }
            return "Review guide failed to generate"
        case .refreshFailed:
            if let errorSummary { return "Review guide refresh failed: \(errorSummary)" }
            return "Review guide refresh failed"
        }
    }

    var tooltip: String {
        let fullError = error?.trimmingCharacters(in: .whitespacesAndNewlines)
        switch kind {
        case .generating: return "Generating review guide\u{2026}"
        case .refreshing: return "Open older review guide; updating\u{2026}"
        case .ready: return "Open review guide"
        case .failed:
            if let fullError, !fullError.isEmpty {
                return "Review guide failed to generate. \(fullError)"
            }
            return "Review guide failed to generate. Retry?"
        case .refreshFailed:
            let prefix = staleSource
                ? "Guide covers an older revision \u{2014} refresh failed."
                : "Explanation refresh failed."
            if let fullError, !fullError.isEmpty {
                return "\(prefix) \(fullError)"
            }
            return "\(prefix) Retry?"
        }
    }

    /// `nil` when `lifecycle` is `nil`/`"idle"`/unrecognized — no series has
    /// been captured for this PR yet (including every non-root row, since a
    /// series is always keyed by the chain-root task id), so the card
    /// renders no affordance at all.
    static func from(
        lifecycle: String?,
        readableVersionId: String?,
        staleSource: Bool = false,
        error: String? = nil
    ) -> ReviewGuideCardPresentation? {
        guard let lifecycle else { return nil }
        let hasContent = readableVersionId != nil
        switch lifecycle {
        case "queued", "generating":
            return ReviewGuideCardPresentation(
                kind: hasContent ? .refreshing : .generating,
                readableVersionId: readableVersionId
            )
        case "ready":
            return ReviewGuideCardPresentation(kind: .ready, readableVersionId: readableVersionId)
        case "failed":
            return ReviewGuideCardPresentation(
                kind: hasContent ? .refreshFailed : .failed,
                readableVersionId: readableVersionId,
                staleSource: staleSource,
                error: error
            )
        default:
            return nil
        }
    }

    static func summaryLine(of error: String?, limit: Int = 120) -> String? {
        guard let error else { return nil }
        let first = error.split(whereSeparator: \.isNewline).first.map(String.init) ?? error
        let trimmed = first.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        if trimmed.count <= limit { return trimmed }
        return String(trimmed.prefix(limit - 1)) + "\u{2026}"
    }
}

/// Currentness of the version currently on screen in the long-lived
/// review-guide viewer. Distinct from `ReviewGuideCardPresentation`:
/// the card describes the series' *current* readable version, while
/// the viewer may still be pinned on an older one. Displayed
/// source-staleness is `displayedComparisonId != selectedComparisonId`,
/// not `WorkTask.reviewGuideStaleSource` (that flag is about the
/// current readable version). Offering a different readable version
/// does not depend on the latest attempt's lifecycle being `"ready"`.
struct ReviewGuideViewerCurrentness: Equatable {
    enum Status: Equatable {
        case none
        case refreshing
        case refreshFailed(displayedStaleSource: Bool, error: String?)
        case failed(error: String?)
    }

    var showsOpenUpdatedGuide: Bool
    var status: Status

    static func from(
        lifecycle: String?,
        readableVersionId: String?,
        selectedComparisonId: String?,
        displayedVersionId: String?,
        displayedComparisonId: String?,
        error: String? = nil
    ) -> ReviewGuideViewerCurrentness {
        let showsOpenUpdatedGuide = readableVersionId != nil
            && readableVersionId != displayedVersionId
        let displayedStaleSource = displayedComparisonId != nil
            && selectedComparisonId != nil
            && displayedComparisonId != selectedComparisonId
        let status: Status
        switch lifecycle {
        case "queued", "generating":
            status = readableVersionId != nil ? .refreshing : .none
        case "failed":
            status = readableVersionId != nil
                ? .refreshFailed(displayedStaleSource: displayedStaleSource, error: error)
                : .failed(error: error)
        default:
            status = .none
        }
        return ReviewGuideViewerCurrentness(
            showsOpenUpdatedGuide: showsOpenUpdatedGuide,
            status: status
        )
    }
}

/// Engine-owned live supplement; the app does not interpret tracking status.
struct ReviewGuideFindings: Codable, Equatable {
    let statusText: String
    let addendumMarkdown: String

    enum CodingKeys: String, CodingKey {
        case statusText = "status_text"
        case addendumMarkdown = "addendum_markdown"
    }

    static func parse(_ payload: Any?) -> Self? {
        guard let payload, let data = try? JSONSerialization.data(withJSONObject: payload) else { return nil }
        return try? JSONDecoder().decode(Self.self, from: data)
    }
}

struct OpenMergeRevision: Codable, Equatable {
    let id: String
    let label: String
    let status: String

    var wirePayload: [String: String] { ["id": id, "label": label, "status": status] }
}
