import Foundation
import Combine

/// Inline readers are keyed by root and PR binding, independently of the
/// singleton document window. A pinned version survives row and tab changes.
@MainActor
final class NavigatorReviewGuideStore: ObservableObject {
    enum State: Equatable {
        case loading
        case loaded(ReviewGuideVersionContent)
        case failed(String)
    }

    struct Reader {
        let prURL: String?
        let versionID: String
        var requestID: String?
        var state: State
        var versions: [ReviewGuideVersionContent]

        var content: ReviewGuideVersionContent? {
            if case .loaded(let content) = state { return content }
            return nil
        }
    }

    @Published private(set) var readers: [String: Reader] = [:]

    func load(root: WorkTask, versionID: String? = nil, refresh: Bool = false,
              send: (String) -> String?) {
        let previous = readers[root.id].flatMap { $0.prURL == root.prURL ? $0 : nil }
        guard let versionID = versionID ?? previous?.versionID ?? root.reviewGuideReadableVersionId else {
            readers.removeValue(forKey: root.id)
            return
        }
        if !refresh, previous?.versionID == versionID { return }
        let versions = previous?.versions ?? []
        if !refresh, let cached = versions.first(where: { $0.id == versionID }) {
            readers[root.id] = Reader(prURL: root.prURL, versionID: versionID,
                                     state: .loaded(cached), versions: versions)
            return
        }
        let requestID = send(versionID)
        readers[root.id] = Reader(
            prURL: root.prURL, versionID: versionID, requestID: requestID,
            state: requestID == nil ? .failed("Guide request could not be sent. Reconnect and retry.") : .loading,
            versions: versions
        )
    }

    func receive(versionID: String, content: ReviewGuideVersionContent?, requestID: String?) {
        guard let requestID, let rootID = readers.first(where: {
            $0.value.requestID == requestID && $0.value.versionID == versionID
        })?.key, var reader = readers[rootID] else { return }
        reader.requestID = nil
        if let content, content.id == versionID {
            reader.state = .loaded(content)
            reader.versions.removeAll { $0.id == content.id }
            reader.versions.append(content)
        } else {
            reader.state = .failed("This review guide is no longer available.")
        }
        readers[rootID] = reader
    }

    @discardableResult
    func fail(message: String, requestID: String?) -> Bool {
        guard let requestID, let rootID = readers.first(where: { $0.value.requestID == requestID })?.key else {
            return false
        }
        readers[rootID]?.requestID = nil
        readers[rootID]?.state = .failed(message)
        return true
    }

    func disconnect() {
        for reader in readers.values {
            fail(message: "Connection lost while loading this guide.", requestID: reader.requestID)
        }
    }
}
