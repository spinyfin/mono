import AppKit
import Foundation
import SwiftUI
import Textual

/// Source the renderer is rendering. The `project-design-doc-pointer.md`
/// Q9 + chore #12 framing names `case designTask` and `case projectPointer`
/// — only `.projectPointer` is wired up today because the design-task
/// surface (`GetDesignDoc(task_id)` RPC, Approve / Revoke buttons) lands
/// with `design-producing-tasks` Q6. When that ships, the additional case
/// is added here and the view branches on it; the Approve button is
/// rendered for `.designTask` only, satisfying chore #12's
/// "Approve button hidden in project-pointer mode" acceptance.
enum DesignRendererSource: Hashable {
    case projectPointer(projectID: String, resolved: ResolvedDesignDoc)
}

/// Payload handed to the `"design-renderer"` `WindowGroup`. The scene
/// keys windows by this struct so re-clicking the same project's icon
/// brings an existing window forward rather than stacking a duplicate
/// (Hashable + the `WindowGroup(for:)` initializer). `filePath` is
/// already composed (workspacePath + repo-relative `path`) so the view
/// is purely a disk reader.
struct DesignRendererContent: Codable, Hashable {
    /// Title shown in the window's header row — typically the project
    /// name so the user can tell two open renderer windows apart.
    let title: String
    /// Absolute path to the markdown file on disk, inside a leased cube
    /// workspace. Resolved by [[ChatViewModel.openProjectDesignDoc(_:)]]
    /// before the window is opened; the view does not re-resolve.
    let filePath: String
    /// GitHub web URL for the doc. Surfaced as an "Open on GitHub ↗"
    /// affordance and used as the fallback if the on-disk read fails
    /// (file deleted, workspace evicted between resolve and click).
    let webURL: String
    /// `<owner>/<repo>` rendered next to the title so a glance tells
    /// the reader which repo the doc lives in. Empty string when the
    /// caller couldn't derive one.
    let repoLabel: String
    /// Project id and resolved doc kind discriminator. Persisted so a
    /// state-restored window survives a restart without re-querying
    /// the engine. Unused by the project-pointer surface today; lives
    /// on the payload so the future design-task case can carry its
    /// `task_id` alongside.
    let projectID: String

    // The resolved doc's repo/branch/path — the pieces of the engine's
    // `pr_doc:<repo_remote_url>:<branch>:<path>` comment-artifact id. Optional so
    // an old state-restored payload (or the local-file open path) decodes without
    // them; comments are engine-backed only when all three are present.
    var repoRemoteURL: String? = nil
    var branch: String? = nil
    var path: String? = nil

    /// The comment artifact this doc corresponds to (`pr_doc:*`), or `nil` when
    /// the payload lacks the repo/branch/path needed to key it.
    var commentArtifact: CommentArtifactRef? {
        guard let repoRemoteURL, let branch, let path,
              !repoRemoteURL.isEmpty, !branch.isEmpty, !path.isEmpty
        else { return nil }
        return .prDoc(repoRemoteURL: repoRemoteURL, branch: branch, path: path)
    }

    /// Payload for opening a plain local markdown file directly (not a
    /// project's pointed-at design doc) — shared by File ▸ Open
    /// (`OpenMarkdownFileCommand`) and the engine-pushed `open_document`
    /// RPC that backs `bossctl open`
    /// (`ChatViewModel.handleEngineRequest`), so both routes render
    /// through the exact same window and view rather than growing a
    /// second markdown-rendering surface. No repo/comment metadata —
    /// `commentArtifact` is nil for both callers.
    static func forLocalFile(path: String) -> DesignRendererContent {
        let url = URL(fileURLWithPath: path)
        return DesignRendererContent(
            title: url.deletingPathExtension().lastPathComponent,
            filePath: path,
            webURL: "",
            repoLabel: "",
            projectID: ""
        )
    }

    /// Convenience for tests and the wiring layer in
    /// [[ChatViewModel.openProjectDesignDoc(_:)]] — builds the payload
    /// from a [[ResolvedDesignDoc]] + workspace path. Returns nil when
    /// the resolved kind is `.external` (no workspace path to read
    /// from) so the caller can fall back to the web URL the same way
    /// the existing dispatcher does.
    static func from(
        projectID: String,
        projectName: String,
        resolved: ResolvedDesignDoc,
        workspacePath: String,
        webURL: String
    ) -> DesignRendererContent? {
        switch resolved.kind {
        case .sameProduct, .otherProduct:
            break
        case .external:
            return nil
        }
        let absolute = (workspacePath as NSString)
            .appendingPathComponent(resolved.path)
        return DesignRendererContent(
            title: projectName.isEmpty ? resolved.path : projectName,
            filePath: absolute,
            webURL: webURL,
            repoLabel: repoOwnerSlash(repoURL: resolved.repoRemoteURL),
            projectID: projectID,
            repoRemoteURL: resolved.repoRemoteURL,
            branch: resolved.branch,
            path: resolved.path
        )
    }

    /// Lift `<owner>/<repo>` out of a GitHub URL for the header chip.
    /// Mirrors `ProjectDesignDocAffordancePresentation.repoBasename`
    /// so the kanban tooltip and the renderer's header label stay in
    /// sync. Returns the trimmed URL verbatim when nothing parses,
    /// rather than guessing — the caller renders whatever it gets.
    private static func repoOwnerSlash(repoURL: String) -> String {
        if let url = URL(string: repoURL), url.host != nil {
            let parts = url.path
                .split(separator: "/", omittingEmptySubsequences: true)
                .map(String.init)
            if parts.count >= 2 {
                let owner = parts[0]
                let repo = parts[1].hasSuffix(".git")
                    ? String(parts[1].dropLast(4))
                    : parts[1]
                return "\(owner)/\(repo)"
            }
        }
        if let scpRange = repoURL.range(of: ":") {
            let path = String(repoURL[scpRange.upperBound...])
            return path.hasSuffix(".git") ? String(path.dropLast(4)) : path
        }
        return repoURL
    }
}

/// In-app markdown viewer for a project's pointed-at design doc, and the File ▸
/// Open / `open -a Boss` local-file surface. Reads the file from disk and renders
/// it through the shared [[MarkdownDocumentChrome]] — the same chrome the kanban
/// "Read full description", async design-doc, and Designs-tab viewers use, so a
/// doc looks identical however it is opened. Read-only: `design-producing-tasks`
/// Q6 owns the Approve / Revoke affordances and lands them on its own case of
/// [[DesignRendererSource]].
///
/// This view keeps only the disk-read concern (the 5 MB guard + async load);
/// every visual and interactive concern (background, header, ⌘F, comments,
/// questions panel) lives in the chrome.
struct DesignRendererView: View {
    let content: DesignRendererContent

    @EnvironmentObject private var model: ChatViewModel
    @State private var source: String = ""
    @State private var loadError: String?

    private var questionGroups: [AttentionGroup] {
        model.openQuestionGroupsForDocPath(content.filePath)
    }

    var body: some View {
        MarkdownDocumentChrome(
            title: content.title,
            repoLabel: content.repoLabel.isEmpty ? nil : content.repoLabel,
            subtitle: content.filePath,
            webURL: content.webURL.isEmpty ? nil : content.webURL,
            source: source,
            loadError: loadError,
            baseURL: URL(fileURLWithPath: content.filePath).deletingLastPathComponent(),
            artifact: content.commentArtifact,
            questionGroups: questionGroups
        )
        .task(id: content.filePath) {
            await load()
        }
    }

    private func load() async {
        let path = content.filePath
        let result: Result<String, Error> = await Task.detached {
            do {
                let url = URL(fileURLWithPath: path)
                let maxBytes = 5 * 1024 * 1024
                if let attrs = try? FileManager.default.attributesOfItem(atPath: path),
                   let size = attrs[.size] as? Int, size > maxBytes {
                    let mb = size / (1024 * 1024)
                    throw NSError(
                        domain: NSCocoaErrorDomain,
                        code: NSFileReadTooLargeError,
                        userInfo: [NSLocalizedDescriptionKey: "File is \(mb) MB, which exceeds the 5 MB display limit."]
                    )
                }
                let raw = try String(
                    contentsOf: url,
                    encoding: .utf8
                )
                return .success(raw)
            } catch {
                return .failure(error)
            }
        }.value

        switch result {
        case .success(let text):
            self.loadError = nil
            self.source = text
        case .failure(let error):
            self.loadError = "Failed to read \(path): \(error.localizedDescription)"
            self.source = ""
        }
    }
}
