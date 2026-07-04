import Foundation

/// Mirrors the engine's `INTENT_DIRECTIVE` / `INTENT_QUESTION` /
/// `INTENT_LARGER_CHANGE` constants (`boss-protocol/src/types.rs`). A
/// `directive`/`largerChange` classification routes to the revision/
/// update-task path; `question` routes to the read-only answer agent.
/// See `tools/boss/docs/designs/comment-triggered-document-revisions.md`.
enum CommentIntent: String, CaseIterable, Equatable {
    case directive
    case question
    case largerChange = "larger_change"

    var displayName: String {
        switch self {
        case .directive: return "Directive"
        case .question: return "Question"
        case .largerChange: return "Larger Change"
        }
    }

    var symbolName: String {
        switch self {
        case .directive: return "arrow.right.circle"
        case .question: return "questionmark.circle"
        case .largerChange: return "shippingbox.circle"
        }
    }
}

/// Mirrors the engine's full `work_comments.status` domain
/// (`boss-protocol/src/types.rs` `COMMENT_STATUS_*`, types.rs:892-923):
/// `active`/`resolved`/`inRevision` drive the bucket-1&3 `[Revise]`-track chip;
/// `answering`/`answered`/`awaitingFollowup` drive the bucket-2 thread's
/// thinking indicator and follow-up composer; `orphaned` (anchor lost) and
/// `dismissed` are terminal states the sidebar surfaces distinctly. The full
/// set is mirrored (P529 Phase-2 scope item 6) so a comment loaded from the
/// engine in any state round-trips without falling through to a default.
enum CommentStatus: String, Equatable {
    case active
    case resolved
    case inRevision = "in_revision"
    /// The answer agent is running for this comment (design § "Bucket 2" —
    /// "Thinking indicator").
    case answering
    /// The answer agent posted its reply; awaiting an operator follow-up.
    case answered
    /// An operator follow-up was posted and awaits reclassification —
    /// loops back to `.answering` (another question) or bridges to
    /// `.active` (directive/larger_change).
    case awaitingFollowup = "awaiting_followup"
    /// The renderer could no longer resolve this comment's anchor against the
    /// current doc; the engine recorded the flip. Shown in the sidebar with an
    /// "anchor lost" badge and no doc highlight.
    case orphaned
    /// Reserved for a future hard-dismiss (soft-dismiss uses `.resolved`).
    case dismissed
}

/// How a comment's anchor last resolved against the doc's plain-text
/// projection — mirrors the engine's `last_resolved_with` / `CommentResolution.kind`
/// (`RESOLVED_WITH_*`). `fuzzy` drives the ⚠ sidebar glyph.
enum ResolvedWith: String, Equatable {
    case exact
    case fuzzy
    case orphan
}

/// A [W3C Web Annotation `TextQuoteSelector`][wadm], mirroring the engine's
/// `CommentAnchor` (`boss-protocol/src/types.rs:863-877`). The three fields are
/// taken from the rendered *plain-text projection* of the markdown (not the raw
/// source), because the user selects on rendered text. `prefix`/`suffix`
/// (~64 chars each) disambiguate `exact` when it recurs and let the engine's
/// fuzzy resolver re-anchor through edits that touch the surrounding text —
/// replacing the Phase-1 `occurrenceIndex` scheme.
///
/// [wadm]: https://www.w3.org/TR/annotation-model/#text-quote-selector
struct CommentAnchor: Codable, Equatable, Sendable {
    /// The verbatim selected text.
    let exact: String
    /// Up to ~64 chars of plain text immediately preceding `exact`.
    let prefix: String
    /// Up to ~64 chars of plain text immediately following `exact`.
    let suffix: String

    init(exact: String, prefix: String = "", suffix: String = "") {
        self.exact = exact
        self.prefix = prefix
        self.suffix = suffix
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        exact = try c.decode(String.self, forKey: .exact)
        // `prefix`/`suffix` carry `#[serde(default)]` on the engine side.
        prefix = try c.decodeIfPresent(String.self, forKey: .prefix) ?? ""
        suffix = try c.decodeIfPresent(String.self, forKey: .suffix) ?? ""
    }
}

/// A comment attached to a markdown viewer. Since P529 Phase 2 comments are
/// engine-backed: `id` is the engine's `work_comments.id` (`cmt_…`) for a
/// persisted comment, or a `local:` sentinel for an optimistic in-memory
/// comment on an artifact-less viewer. Anchoring is W3C `{exact, prefix,
/// suffix}`; the occurrence-index scheme is gone.
struct Comment: Identifiable, Equatable {
    /// Engine `work_comments.id`, or a `local:<uuid>` sentinel for the
    /// artifact-less in-memory fallback path.
    let id: String
    /// The W3C anchor the comment is attached to.
    let anchor: CommentAnchor
    /// The comment body the user typed.
    let body: String
    let author: String
    let createdAt: Date

    /// `nil` while classification is pending. Set by the engine's classifier or
    /// a manual override.
    var intent: CommentIntent? = nil
    /// `true` once the operator has manually reclassified this comment via the
    /// badge's override control, mirroring the engine's `intent_overridden_by`.
    var intentOverriddenByUser: Bool = false

    /// Mirrors `work_comments.status`. Defaults to `active`.
    var status: CommentStatus = .active
    /// Mirrors `work_comments.revise_task_id`.
    var reviseTaskId: String? = nil
    /// Mirrors `work_comments.status_actor`.
    var statusActor: String? = nil
    /// How this comment's anchor last resolved on load (drives the ⚠/anchor-lost
    /// sidebar glyphs). `nil` until a `comments_resolve` round-trip lands.
    var lastResolvedWith: ResolvedWith? = nil
    /// Engine-authored nudge/answer/follow-up entries, oldest first.
    var threadEntries: [CommentThreadEntry] = []

    // Persistence provenance carried so the layer can issue mutations without a
    // separate lookup. Empty on the artifact-less in-memory path.
    var artifactKind: String = ""
    var artifactId: String = ""
    var docVersion: String = ""

    /// The selected text this comment is anchored to. Alias for `anchor.exact`,
    /// kept so the sidebar snippet and tests read naturally.
    var quotedText: String { anchor.exact }

    /// `true` when the engine could not re-anchor this comment on the current
    /// doc — either an explicit `orphaned` status or an `orphan` resolution.
    var isOrphaned: Bool {
        status == .orphaned || lastResolvedWith == .orphan
    }

    /// `true` when the anchor re-attached only via fuzzy match — the ⚠ glyph.
    var isFuzzyAnchored: Bool { lastResolvedWith == .fuzzy }

    /// Whether this comment should paint a highlight in the doc: it must have
    /// text to anchor to and be in a live, resolvable state (orphaned / resolved
    /// / dismissed comments carry no doc highlight).
    var isHighlightable: Bool {
        !anchor.exact.isEmpty && !isOrphaned && status != .resolved && status != .dismissed
    }

    /// The `[Revise]`-track chip state, derived from `status` /
    /// `reviseTaskId` / thread history — mirrors the engine's comment state
    /// machine (design § "Comment/thread state machine"). `nil` when the
    /// comment isn't on the directive/larger_change track at all.
    var revisionChipState: RevisionChipState? {
        switch status {
        case .inRevision:
            guard let taskId = reviseTaskId else { return nil }
            return .inRevision(taskId: taskId)
        case .resolved:
            guard let taskId = reviseTaskId else { return nil }
            return .resolved(taskId: taskId)
        case .active:
            let wasInRevision = threadEntries.contains { $0.entryKind == .nudge && $0.reviseTaskId != nil }
            if wasInRevision, statusActor == "engine" { return .reopened }
            let hasNudge = threadEntries.contains { $0.entryKind == .nudge }
            return hasNudge ? .nudged : nil
        case .answering, .answered, .awaitingFollowup, .orphaned, .dismissed:
            return nil
        }
    }
}

// MARK: - Wire → UI mapping

extension Comment {
    /// Build a UI `Comment` from an engine `WorkComment`, its thread entries,
    /// and (optionally) the anchor resolution from `comments_resolve`.
    static func from(
        _ wc: WorkComment,
        threadEntries: [WireCommentThreadEntry] = [],
        resolution: CommentResolution? = nil
    ) -> Comment {
        var c = Comment(
            id: wc.id,
            anchor: wc.anchor,
            body: wc.body,
            author: wc.author,
            createdAt: parseWireTimestamp(wc.createdAt)
        )
        c.intent = wc.intent.flatMap(CommentIntent.init(rawValue:))
        c.intentOverriddenByUser = wc.intentOverriddenBy != nil
        c.status = CommentStatus(rawValue: wc.status) ?? .active
        c.reviseTaskId = wc.reviseTaskId
        c.statusActor = wc.statusActor
        // Prefer the fresh resolution kind when present, else the persisted
        // `last_resolved_with` the engine echoes on the row.
        c.lastResolvedWith =
            resolution.map { ResolvedWith(rawValue: $0.kind) ?? .exact }
            ?? wc.lastResolvedWith.flatMap(ResolvedWith.init(rawValue:))
        c.threadEntries = threadEntries.map(CommentThreadEntry.from)
        c.artifactKind = wc.artifactKind
        c.artifactId = wc.artifactId
        c.docVersion = wc.docVersion
        return c
    }

    /// Lenient ISO-8601 parse of an engine timestamp string; falls back to the
    /// current instant so a malformed value never drops the comment. Shared with
    /// [`CommentThreadEntry.from`]. Delegates to `WorkerStaleness.parse` so both
    /// call sites share the same cached formatters.
    static func parseWireTimestamp(_ s: String) -> Date {
        WorkerStaleness.parse(s) ?? Date()
    }
}

/// The four `[Revise]`-track chip states 2f renders, per the design's
/// comment/thread state machine.
enum RevisionChipState: Equatable {
    /// Classified `directive`/`larger_change`; nudge posted, `[Revise]` not
    /// yet clicked.
    case nudged
    case inRevision(taskId: String)
    case resolved(taskId: String)
    /// The claiming task was abandoned; the comment is back on the banner.
    case reopened
}

/// Client mirror of the engine's `CommentsBannerState`
/// (`boss-protocol/src/types.rs`) — a read-only summary driving the
/// `[Revise]` banner.
struct CommentsBannerState: Equatable {
    let revisable: Bool
    let unresolvedCount: Int
    let inRevisionCount: Int
}
