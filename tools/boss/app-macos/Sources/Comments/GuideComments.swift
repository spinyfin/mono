import SwiftUI

/// Drafts survive replacement of the shared viewer's view hierarchy. Their
/// immutable version key prevents a refresh from moving a quote to new prose.
struct GuideCommentDraft: Equatable {
    let seriesId: String
    let quote: String
    let occurrenceIndex: Int
    let body: String
}

@MainActor
final class GuideCommentDrafts: ObservableObject {
    static let shared = GuideCommentDrafts()
    @Published var byVersion: [String: GuideCommentDraft] = [:]
}

extension CommentLayer {
    var guideDraft: GuideCommentDraft? {
        guideVersionId.flatMap { GuideCommentDrafts.shared.byVersion[$0] }
    }

    func saveGuideDraft(body: String) {
        guard let guideVersionId, !pendingQuotedText.isEmpty else { return }
        guard !body.isEmpty else { discardGuideDraft(); return }
        GuideCommentDrafts.shared.byVersion[guideVersionId] = GuideCommentDraft(
            seriesId: artifactId, quote: pendingQuotedText, occurrenceIndex: pendingOccurrenceIndex, body: body)
        objectWillChange.send()
    }

    func discardGuideDraft() {
        guard let guideVersionId else { return }
        GuideCommentDrafts.shared.byVersion.removeValue(forKey: guideVersionId)
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
