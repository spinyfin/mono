import Foundation
import CryptoKit

struct GuideCommentDraftCacheEntry: Codable {
    let versionId: String
    let position: Int
    let draft: GuideCommentDraft
    let submitted: GuideCommentDraft?
}

/// Mirrors IdeaDraftCache's atomic, best-effort writes. Guide drafts need a
/// composite key and the submitted anchor as well as the editable buffer.
enum GuideCommentDraftCache {
    static func readAll(in directory: URL) -> [GuideCommentDraftCacheEntry] {
        let files = (try? FileManager.default.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: nil)) ?? []
        return files.filter { $0.pathExtension == "json" }.compactMap { url in
            guard let data = try? Data(contentsOf: url) else { return nil }
            return try? JSONDecoder().decode(GuideCommentDraftCacheEntry.self, from: data)
        }.sorted { $0.position < $1.position }
    }

    static func write(
        byVersion: [String: [GuideCommentDraft]],
        submitted: [UUID: GuideCommentDraft], in directory: URL
    ) {
        let fm = FileManager.default
        try? fm.createDirectory(at: directory, withIntermediateDirectories: true)
        var retained = Set<String>()
        for (version, drafts) in byVersion {
            for (position, draft) in drafts.enumerated() {
                // Encode arbitrary series/version identifiers without path separators.
                let key = [draft.seriesId, version, draft.composerId.uuidString]
                guard let keyData = try? JSONEncoder().encode(key) else { continue }
                let name = SHA256.hash(data: keyData).map { String(format: "%02x", $0) }.joined() + ".json"
                retained.insert(name)
                let entry = GuideCommentDraftCacheEntry(
                    versionId: version, position: position, draft: draft,
                    submitted: submitted[draft.composerId])
                guard let data = try? JSONEncoder().encode(entry) else { continue }
                try? data.write(to: directory.appendingPathComponent(name), options: .atomic)
            }
        }
        let files = (try? fm.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? []
        for file in files where file.pathExtension == "json" && !retained.contains(file.lastPathComponent) {
            try? fm.removeItem(at: file)
        }
    }
}
