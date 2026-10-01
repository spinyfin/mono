import SwiftUI

/// Drafts survive viewer replacement and app restarts through a write-through cache. Their
/// immutable version key prevents a refresh from moving a quote to new prose.
/// `composerId` distinguishes a parked draft from a later composer on the
/// same version so neither can overwrite the other.
struct GuideCommentDraft: Codable, Equatable {
    let seriesId: String
    let quote: String
    let occurrenceIndex: Int
    let body: String
    let composerId: UUID

    func withQuote(_ quote: String) -> GuideCommentDraft {
        GuideCommentDraft(
            seriesId: seriesId, quote: quote, occurrenceIndex: occurrenceIndex,
            body: body, composerId: composerId)
    }
}

@MainActor
final class GuideCommentDrafts: ObservableObject {
    private let directory: URL?

    init(directory: URL? = IdeaDraftCache.defaultDirectory.deletingLastPathComponent().appendingPathComponent("guide-comment-drafts")) {
        self.directory = directory
        guard let directory else { return }
        var loaded: [String: [GuideCommentDraft]] = [:]
        var inFlight: [UUID: GuideCommentDraft] = [:]
        for entry in GuideCommentDraftCache.readAll(in: directory) {
            loaded[entry.versionId, default: []].append(entry.draft)
            inFlight[entry.draft.composerId] = entry.submitted
        }
        byVersion = loaded
        submitted = inFlight
    }

    private func persist() {
        guard let directory else { return }
        GuideCommentDraftCache.write(byVersion: byVersion, submitted: submitted, in: directory)
    }
    /// Parked and in-flight drafts, insertion-ordered per version.
    @Published private(set) var byVersion: [String: [GuideCommentDraft]] = [:] { didSet { persist() } }
    /// Set by "Resume draft on original guide" before that version is open.
    /// Cleared only after the comment layer presents the draft popover.
    var pendingResumeVersionId: String?
    var pendingResumeComposerId: UUID?

    /// In-flight creates, keyed by composer. `quote` is the persisted
    /// `anchor.exact` so acknowledgement can match the engine echo.
    var submitted: [UUID: GuideCommentDraft] = [:] { didSet { persist() } }

    func drafts(for version: String) -> [GuideCommentDraft] {
        byVersion[version] ?? []
    }

    func upsert(_ draft: GuideCommentDraft, version: String) {
        var list = byVersion[version] ?? []
        if let index = list.firstIndex(where: { $0.composerId == draft.composerId }) {
            list[index] = draft
        } else {
            list.append(draft)
        }
        byVersion[version] = list
    }

    func remove(version: String, composerId: UUID) {
        if var list = byVersion[version] {
            list.removeAll { $0.composerId == composerId }
            if list.isEmpty {
                byVersion.removeValue(forKey: version)
            } else {
                byVersion[version] = list
            }
        }
        submitted.removeValue(forKey: composerId)
    }

    func removeAll(for version: String) {
        for draft in byVersion[version] ?? [] {
            submitted.removeValue(forKey: draft.composerId)
        }
        byVersion.removeValue(forKey: version)
    }

    func acknowledge(_ comment: WorkComment) {
        guard let version = comment.guideContext?.versionId,
              let (composerId, _) = submitted.first(where: { _, draft in
                  byVersion[version]?.contains(where: { $0.composerId == draft.composerId }) == true
                      && draft.seriesId == comment.artifactId
                      && draft.body.trimmingCharacters(in: .whitespacesAndNewlines) == comment.body
                      && draft.quote == comment.anchor.exact
              }) else { return }
        remove(version: version, composerId: composerId)
    }

    func hasPendingResume(for versionId: String?) -> Bool {
        guard let versionId, pendingResumeVersionId == versionId else { return false }
        return true
    }
}

extension CommentLayer {
    var guideDrafts: [GuideCommentDraft] {
        guideVersionId.map { draftStore.drafts(for: $0) } ?? []
    }

    var guideDraft: GuideCommentDraft? {
        let drafts = guideDrafts
        if let requested = drafts.first(where: { $0.composerId == draftStore.pendingResumeComposerId }) { return requested }
        if let mine = drafts.first(where: { $0.composerId == composerId }) { return mine }
        return drafts.first { draftStore.submitted[$0.composerId] == nil } ?? drafts.first
    }

    func saveGuideDraft(body: String) {
        guard let guideVersionId else { return }
        guard !body.isEmpty else {
            if ownsGuideDraft {
                draftStore.remove(version: guideVersionId, composerId: composerId)
                objectWillChange.send()
            }
            return
        }
        ownsGuideDraft = true
        draftStore.upsert(
            GuideCommentDraft(
                seriesId: artifactId, quote: pendingQuotedText, occurrenceIndex: pendingOccurrenceIndex,
                body: body, composerId: composerId),
            version: guideVersionId)
        objectWillChange.send()
    }

    func discardGuideDraft() {
        guard let guideVersionId else { return }
        draftStore.removeAll(for: guideVersionId)
        objectWillChange.send()
    }

    func discardCurrentComposerDraft() {
        guard let guideVersionId else { return }
        draftStore.remove(version: guideVersionId, composerId: composerId)
        objectWillChange.send()
    }

    func displayAnchor(for resolution: CommentResolution) -> CommentAnchor? {
        let scalars = Array(currentProjection().unicodeScalars)
        guard let start = resolution.start, let length = resolution.length,
              start >= 0, length > 0, start <= scalars.count,
              length <= scalars.count - start else { return nil }
        func text(_ range: Range<Int>) -> String {
            String(String.UnicodeScalarView(scalars[range]))
        }
        return CommentAnchor(
            exact: text(start..<(start + length)),
            prefix: text(max(0, start - 64)..<start),
            suffix: text((start + length)..<min(scalars.count, start + length + 64)))
    }

    var currentVersionComments: [Comment] {
        comments.filter { $0.guideContext?.versionId == guideVersionId }
    }

    var otherVersionComments: [Comment] {
        comments.filter { $0.guideContext?.versionId != guideVersionId }
    }
}

private struct OpenOriginalGuideKey: EnvironmentKey {
    static var defaultValue: ((String) -> Void)? { nil }
}

extension EnvironmentValues {
    var openOriginalGuide: ((String) -> Void)? {
        get { self[OpenOriginalGuideKey.self] }
        set { self[OpenOriginalGuideKey.self] = newValue }
    }
}
