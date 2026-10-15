import Foundation

extension ChatViewModel {
    /// Design cards prefer their project's pointer, without resolving it.
    /// Other cards keep the engine-provided per-task doc presentation.
    func workItemDocPresentation(for task: WorkTask) -> ProjectDesignDocAffordancePresentation? {
        if (task.kind == "design" || task.kind == "design_postmortem"),
           let projectID = task.projectID,
           let project = project(withID: projectID),
           let presentation = ProjectDesignDocAffordancePresentation.from(project: project) {
            return presentation
        }
        return task.docLinkState.flatMap { ProjectDesignDocAffordancePresentation.from(state: $0) }
    }

    /// Open the doc attached to a work item. Prefers a presentable
    /// project-level design doc on design / design-postmortem cards;
    /// otherwise follows `task.docLinkState` via `openTaskDoc`.
    func openWorkItemDoc(_ task: WorkTask) {
        if (task.kind == "design" || task.kind == "design_postmortem"),
           let projectID = task.projectID,
           let project = project(withID: projectID) {
            if project.designDocPath != nil {
                openProjectDesignDoc(project)
                return
            }
        }
        if task.docLinkState != nil {
            openTaskDoc(task)
            return
        }
        if let projectID = task.projectID, let project = project(withID: projectID) {
            openProjectDesignDoc(project)
        }
    }

    /// Open the doc-link for a work item that carries a per-task
    /// `docLinkState`. The task-level analogue of `openProjectDesignDoc`:
    /// dispatch follows the engine-resolved `task.docLinkState` rather than a
    /// project's cached `ProjectDesignDocState`.
    ///
    /// Unlike the project path there is no workspace fast-path — the in-app
    /// design renderer is project-keyed, and an in-review doc lives on the
    /// PR head branch (not a leased workspace) anyway — so this resolves
    /// via the GitHub `rawContentURL` into the async markdown viewer,
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
                    asyncMarkdownViewerVM.state = .loading(title: displayName)
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
