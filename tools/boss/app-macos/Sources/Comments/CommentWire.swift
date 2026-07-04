import CryptoKit
import Foundation
import Textual

/// Swift Codable mirrors of the engine's comment wire types
/// (`boss-protocol` `tools/boss/protocol/src/types.rs` + `wire.rs`) plus the
/// small helpers the renderer needs to speak the `comments_*` protocol. This
/// is the P529 Phase-2 half PR #915 deferred: the engine ships the RPCs; this
/// is the macOS side that finally calls them.
///
/// The engine serialises every field under its verbatim `snake_case` Rust name
/// with no container `rename_all`, so these `CodingKeys` map 1:1. Every engine
/// `Option<T>` field is `skip_serializing_if = "Option::is_none"`, i.e. the key
/// is *absent* (never `null`) when unset — so all optionals decode with
/// `decodeIfPresent`. `plain_text_projection_version` defaults to `0` and
/// `status` defaults to `"active"` on the wire.

// MARK: - Constants (mirror the engine string constants)

/// `work_comments.status` values (`COMMENT_STATUS_*`, types.rs:892-923).
enum WireCommentStatus {
    static let active = "active"
    static let resolved = "resolved"
    static let orphaned = "orphaned"
    static let dismissed = "dismissed"
    static let inRevision = "in_revision"
    static let answering = "answering"
    static let answered = "answered"
    static let awaitingFollowup = "awaiting_followup"
}

/// `CommentResolution.kind` / `work_comments.last_resolved_with` values
/// (`RESOLVED_WITH_*`, types.rs:927-929). `fuzzy` drives the ⚠ sidebar glyph.
enum WireResolvedWith {
    static let exact = "exact"
    static let fuzzy = "fuzzy"
    static let orphan = "orphan"
}

/// Artifact-kind discriminator (`work_comments.artifact_kind`).
enum WireArtifactKind {
    static let workItem = "work_item"
    static let prDoc = "pr_doc"
}

// MARK: - CommentResolution (types.rs:948-961)

/// The outcome of resolving one comment's anchor against a doc's current
/// plain-text projection. `start`/`length` are Unicode-scalar character offsets
/// of the `exact` span; both `nil` for an orphan. `score` is the fuzzy match
/// score (only set for `fuzzy`).
struct CommentResolution: Codable, Equatable, Sendable {
    /// `exact` | `fuzzy` | `orphan`.
    let kind: String
    let length: Int?
    let score: Double?
    let start: Int?

    var isFuzzy: Bool { kind == WireResolvedWith.fuzzy }
    var isOrphan: Bool { kind == WireResolvedWith.orphan }
}

// MARK: - WorkComment (types.rs:3398-3479)

/// The `work_comments` row. Anchor is embedded inline as a nested object.
struct WorkComment: Codable, Equatable, Sendable {
    let id: String
    let artifactId: String
    let anchor: CommentAnchor
    let artifactKind: String
    let author: String
    let body: String
    let createdAt: String
    let docVersion: String
    let plainTextProjectionVersion: Int
    let status: String
    let updatedAt: String
    let dismissedAt: String?
    let lastResolvedWith: String?
    let statusActor: String?
    let intent: String?
    let intentConfidence: Double?
    let intentClassifiedAt: String?
    let intentOverriddenBy: String?
    let reviseTaskId: String?

    enum CodingKeys: String, CodingKey {
        case id
        case artifactId = "artifact_id"
        case anchor
        case artifactKind = "artifact_kind"
        case author
        case body
        case createdAt = "created_at"
        case docVersion = "doc_version"
        case plainTextProjectionVersion = "plain_text_projection_version"
        case status
        case updatedAt = "updated_at"
        case dismissedAt = "dismissed_at"
        case lastResolvedWith = "last_resolved_with"
        case statusActor = "status_actor"
        case intent
        case intentConfidence = "intent_confidence"
        case intentClassifiedAt = "intent_classified_at"
        case intentOverriddenBy = "intent_overridden_by"
        case reviseTaskId = "revise_task_id"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        artifactId = try c.decode(String.self, forKey: .artifactId)
        anchor = try c.decode(CommentAnchor.self, forKey: .anchor)
        artifactKind = try c.decode(String.self, forKey: .artifactKind)
        author = try c.decode(String.self, forKey: .author)
        body = try c.decode(String.self, forKey: .body)
        createdAt = try c.decode(String.self, forKey: .createdAt)
        docVersion = try c.decode(String.self, forKey: .docVersion)
        // Defaults mirror the engine's `#[serde(default …)]` on these fields.
        plainTextProjectionVersion =
            try c.decodeIfPresent(Int.self, forKey: .plainTextProjectionVersion) ?? 0
        status = try c.decodeIfPresent(String.self, forKey: .status) ?? WireCommentStatus.active
        updatedAt = try c.decode(String.self, forKey: .updatedAt)
        dismissedAt = try c.decodeIfPresent(String.self, forKey: .dismissedAt)
        lastResolvedWith = try c.decodeIfPresent(String.self, forKey: .lastResolvedWith)
        statusActor = try c.decodeIfPresent(String.self, forKey: .statusActor)
        intent = try c.decodeIfPresent(String.self, forKey: .intent)
        intentConfidence = try c.decodeIfPresent(Double.self, forKey: .intentConfidence)
        intentClassifiedAt = try c.decodeIfPresent(String.self, forKey: .intentClassifiedAt)
        intentOverriddenBy = try c.decodeIfPresent(String.self, forKey: .intentOverriddenBy)
        reviseTaskId = try c.decodeIfPresent(String.self, forKey: .reviseTaskId)
    }

    /// Memberwise-style init for tests (the decoder is the production path).
    init(
        id: String,
        artifactId: String,
        anchor: CommentAnchor,
        artifactKind: String,
        author: String,
        body: String,
        createdAt: String,
        docVersion: String = "",
        plainTextProjectionVersion: Int = 0,
        status: String = WireCommentStatus.active,
        updatedAt: String = "",
        dismissedAt: String? = nil,
        lastResolvedWith: String? = nil,
        statusActor: String? = nil,
        intent: String? = nil,
        intentConfidence: Double? = nil,
        intentClassifiedAt: String? = nil,
        intentOverriddenBy: String? = nil,
        reviseTaskId: String? = nil
    ) {
        self.id = id
        self.artifactId = artifactId
        self.anchor = anchor
        self.artifactKind = artifactKind
        self.author = author
        self.body = body
        self.createdAt = createdAt
        self.docVersion = docVersion
        self.plainTextProjectionVersion = plainTextProjectionVersion
        self.status = status
        self.updatedAt = updatedAt
        self.dismissedAt = dismissedAt
        self.lastResolvedWith = lastResolvedWith
        self.statusActor = statusActor
        self.intent = intent
        self.intentConfidence = intentConfidence
        self.intentClassifiedAt = intentClassifiedAt
        self.intentOverriddenBy = intentOverriddenBy
        self.reviseTaskId = reviseTaskId
    }
}

// MARK: - Wire thread entry (types.rs:2204-2222)

/// The `comment_thread_entries` row as it arrives on the wire. Mapped into the
/// UI-facing [`CommentThreadEntry`] (which keys on a `String` for `Identifiable`).
struct WireCommentThreadEntry: Codable, Equatable, Sendable {
    let id: String
    let commentId: String
    let entryKind: String
    let author: String
    let body: String
    let reviseTaskId: String?
    let answerAgentRunId: String?
    let createdAt: String

    enum CodingKeys: String, CodingKey {
        case id
        case commentId = "comment_id"
        case entryKind = "entry_kind"
        case author
        case body
        case reviseTaskId = "revise_task_id"
        case answerAgentRunId = "answer_agent_run_id"
        case createdAt = "created_at"
    }
}

// MARK: - CommentWithThread (types.rs:2535-2544) — the comments_list element

struct CommentWithThread: Codable, Equatable, Sendable {
    let comment: WorkComment
    let threadEntries: [WireCommentThreadEntry]
    let answerAgentRunning: Bool

    enum CodingKeys: String, CodingKey {
        case comment
        case threadEntries = "thread_entries"
        case answerAgentRunning = "answer_agent_running"
    }
}

// MARK: - ResolvedComment (types.rs:2526-2530) — the comments_resolve element

struct ResolvedComment: Codable, Equatable, Sendable {
    let comment: WorkComment
    let resolution: CommentResolution
}

// MARK: - Plain-text projection + doc version

/// The renderer's plain-text projection of a markdown source, and the opaque
/// digest the engine stores as `doc_version`. Both the anchor-capture path and
/// the resolve-on-load path must compute the projection *identically* to what
/// [`HighlightingMarkdownParser`] paints against, so they share this helper —
/// it flattens the exact same Textual parse (`String(attributedString.characters)`)
/// the parser uses at `CommentHighlightingParser.swift:54-55`.
enum CommentProjection {
    /// Bump this whenever the projection algorithm changes so the engine can
    /// re-anchor everything once on a renderer upgrade (design § "Risks":
    /// "projection algorithm version"). Sent as `plain_text_projection_version`.
    static let version = 1

    /// Flatten `source` to the rendered plain text the user selects on. Falls
    /// back to the raw source if the Textual parse throws (degenerate but never
    /// crashes the viewer).
    @MainActor
    static func plainText(for source: String, baseURL: URL? = nil) -> String {
        if let attributed = try? AttributedStringMarkdownParser
            .markdown(baseURL: baseURL)
            .attributedString(for: source) {
            return String(attributed.characters)
        }
        return source
    }

    /// SHA-256 of the plain-text projection, hex-encoded and `sha256:`-prefixed.
    /// The engine treats this as opaque and only compares for equality.
    static func docVersion(forPlainText plainText: String) -> String {
        let digest = SHA256.hash(data: Data(plainText.utf8))
        let hex = digest.map { String(format: "%02x", $0) }.joined()
        return "sha256:\(hex)"
    }
}
