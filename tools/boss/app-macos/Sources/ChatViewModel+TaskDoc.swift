import Foundation

extension ChatViewModel {
    /// Effective doc-link state for a work item, independent of `kind`.
    ///
    /// Design / design-postmortem cards with a project still prefer the
    /// **project-level** design-doc pointer when that pointer actually
    /// presents (resolved or broken) — that is a separate concept from
    /// the per-task `doc_*` columns. Otherwise the per-task
    /// `docLinkState` is used, so a chore, investigation, revision, or
    /// any other kind with an attached doc shows the same affordance.
    func workItemDocState(for task: WorkTask) -> ProjectDesignDocState? {
        let projectState: ProjectDesignDocState? = {
            guard task.kind == "design" || task.kind == "design_postmortem",
                  let projectID = task.projectID else { return nil }
            return designDocStateByProjectID[projectID] ?? .notSet
        }()
        if let projectState,
           ProjectDesignDocAffordancePresentation.from(state: projectState) != nil {
            return projectState
        }
        if let taskState = task.docLinkState,
           ProjectDesignDocAffordancePresentation.from(state: taskState) != nil {
            return taskState
        }
        return projectState ?? task.docLinkState
    }

    /// Open the doc attached to a work item. Prefers a presentable
    /// project-level design doc on design / design-postmortem cards;
    /// otherwise follows `task.docLinkState` via `openTaskDoc`.
    func openWorkItemDoc(_ task: WorkTask) {
        if (task.kind == "design" || task.kind == "design_postmortem"),
           let projectID = task.projectID,
           let project = project(withID: projectID) {
            let projectState = designDocStateByProjectID[project.id] ?? .notSet
            if ProjectDesignDocAffordancePresentation.from(state: projectState) != nil {
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
