import Foundation

extension ChatViewModel {
    /// Open the doc-link for a project-less docs-backed work item (an
    /// investigation). The task-level analogue of `openProjectDesignDoc`:
    /// dispatch follows the engine-resolved `task.docLinkState` rather than a
    /// project's cached `ProjectDesignDocState`.
    ///
    /// Unlike the project path there is no workspace fast-path — the in-app
    /// design renderer is project-keyed, and an in-review investigation's doc
    /// lives on the PR head branch (not a leased workspace) anyway — so this
    /// resolves via the GitHub `rawContentURL` into the async markdown viewer,
    /// falling back to the GitHub web URL. Mirrors the `.resolved` dispatch in
    /// `openProjectDesignDoc` so both doc-link icons behave identically.
    func openTaskDoc(_ task: WorkTask) {
        let shortID = task.shortID.map { "\($0)" } ?? task.id
        guard let state = task.docLinkState else { return }
        switch state {
        case .notSet:
            return
        case .broken(let reason):
            workErrorMessage = "Doc pointer is broken: \(reason)"
        case .resolved(let resolved, _, let webURL, let rawContentURL):
            // Prefer fetching via rawContentURL (GitHub API): correct for both
            // the in-review (PR head branch) and merged (main) cases, because
            // the ref is baked into the URL.
            if rawContentURL != nil {
                let displayName = task.name
                if let opener = asyncMarkdownViewerOpener {
                    // Open the window immediately, then resolve through
                    // the engine (cache + revalidate). Parity with the
                    // project path; no app-side `gh api`.
                    asyncMarkdownViewerVM.state = .loading
                    asyncMarkdownViewerVM.clickStartTime = Date()
                    opener()
                    openDesignDocViaEngine(
                        ref: DesignDocRef(
                            repoRemoteURL: resolved.repoRemoteURL,
                            path: resolved.path,
                            gitRef: resolved.branch
                        ),
                        title: displayName,
                        artifact: resolved.commentArtifact,
                        projectShortID: shortID
                    )
                } else {
                    // Headless / test path: no in-app viewer wired.
                    openDesignDocFallback(webURL: webURL)
                }
                return
            }
            // rawContentURL absent (non-GitHub repo or older engine): fall back
            // to the GitHub web URL.
            guard let url = URL(string: webURL) else {
                workErrorMessage = "Doc URL could not be parsed: \(webURL)"
                return
            }
            urlOpener(url)
        }
    }
}
