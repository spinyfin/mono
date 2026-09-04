import Foundation

/// Idea (markdown draft) authoring: selection, autosave, and
/// send-to-coordinator. See the Ideas project description for why the
/// crash floor exists — a coordinator session once died mid-composition
/// and the unsent buffer was unrecoverable.
///
/// Autosave has two debounce tiers plus an always-on local crash floor:
///
/// 1. Every keystroke (`noteIdeaDraftEdited`) resets a short local-disk
///    write debounce (`ideaLocalCacheDebounce`) — a crash-floor write to
///    `IdeaDraftCache` that survives an app crash or force-quit no matter
///    what the engine connection is doing.
/// 2. The same keystroke resets a longer engine-save debounce
///    (`ideaEngineSaveDebounce`) that sends `update_idea` — skipped
///    entirely while disconnected (see `sendIdeaEngineSave`), since there
///    is nothing to send to.
/// 3. `flushIdeaDraft` short-circuits both debounces for an immediate,
///    synchronous save-on-blur / save-on-window-close / switch-idea flush.
///
/// When the engine is unreachable mid-edit, the draft is never dropped:
/// the local cache write still happens on schedule, `ideaSaveStatus` shows
/// `.offlineSavedLocally` instead of failing silently, and the next time
/// this idea is opened for editing (`loadIdeaDraft`) the cached body wins
/// over the engine's last-known snapshot and is immediately re-sent.
extension ChatViewModel {
    /// Local crash-floor writes fire this soon after the last keystroke —
    /// short enough that a crash loses at most a fraction of a second of
    /// typing, cheap enough (a few-KB JSON write) to run on every pause.
    static let ideaLocalCacheDebounce: TimeInterval = 0.5
    /// Engine `update_idea` sends debounce longer than the local write:
    /// this is a network round trip, and coalescing keystrokes into one
    /// request per pause in typing — not one per character — is the
    /// entire point of a debounce.
    static let ideaEngineSaveDebounce: TimeInterval = 1.5

    /// Ideas for the currently selected product (see `selectedProduct`),
    /// newest first, as the engine returns them.
    var ideasForSelectedProduct: [WorkIdea] {
        guard let productID = currentSelectedProductID else { return [] }
        return ideasByProductID[productID] ?? []
    }

    /// The idea backing the open editor, looked up from the per-product
    /// list so it always reflects the latest engine-confirmed row.
    var selectedIdea: WorkIdea? {
        guard let id = selectedIdeaID else { return nil }
        return ideasForSelectedProduct.first { $0.id == id }
    }

    /// Whether an idea has an unsynced local draft sitting in the crash
    /// floor. Drives the sidebar's "unsaved" indicator so a draft that
    /// never got reconciled (a crash, then the operator opened a
    /// different idea) stays visible instead of waiting to be
    /// rediscovered.
    func ideaHasPendingLocalDraft(_ ideaID: String) -> Bool {
        IdeaDraftCache.hasPendingDraft(ideaID: ideaID)
    }

    /// Create a new idea in the selected product and open it for editing.
    /// No-op when disconnected or no product is selected — mirrors every
    /// other "New …" affordance in the app (`ContentView`'s New Product /
    /// Project / Task / Chore, all gated on `isConnected`). An idea's id is
    /// minted by the engine, so there is no offline-create path: unlike an
    /// in-progress edit, a not-yet-created idea has nothing durable on
    /// disk to protect.
    func createIdea() {
        guard isConnected, let productID = currentSelectedProductID else { return }
        engine.sendCreateIdea(productId: productID, name: "New idea", body: nil)
    }

    /// Switch the editor to a different idea (or `nil` to close it),
    /// flushing any unconfirmed edit on the idea being left first — see
    /// `flushIdeaDraft` — so a fast switch never drops a save that was
    /// still sitting in the debounce window.
    func selectIdea(_ ideaID: String?) {
        guard selectedIdeaID != ideaID else { return }
        flushIdeaDraft()
        selectedIdeaID = ideaID
        loadIdeaDraft(ideaID)
    }

    /// Populate the editor's draft state for `ideaID`. Prefers an unsynced
    /// local cache entry over the engine's last-known snapshot — the cache
    /// exists precisely because it can be ahead of what the engine has
    /// confirmed (a crash before the debounced save fired, or an edit made
    /// while offline) — and immediately re-attempts the engine save, so
    /// the reconciliation the design calls for happens the moment the
    /// idea is revisited rather than waiting on the next keystroke.
    private func loadIdeaDraft(_ ideaID: String?) {
        guard let ideaID, let idea = ideasByProductID.values.flatMap({ $0 }).first(where: { $0.id == ideaID }) else {
            ideaDraftName = ""
            ideaDraftBody = ""
            ideaSaveStatus = .idle
            return
        }
        if let cached = IdeaDraftCache.read(ideaID: ideaID), cached.productID == idea.productID {
            ideaDraftName = cached.name
            ideaDraftBody = cached.body
            ideaSaveStatus = .pendingLocal
            scheduleIdeaEngineSave(immediate: true)
        } else {
            ideaDraftName = idea.name
            ideaDraftBody = idea.body
            ideaSaveStatus = .savedToEngine
        }
    }

    /// Called on every editor keystroke (`IdeasView`'s `.onChange` on the
    /// published `ideaDraftName` / `ideaDraftBody`). Resets both debounce
    /// timers.
    func noteIdeaDraftEdited() {
        guard selectedIdeaID != nil else { return }
        ideaSaveStatus = .pendingLocal
        scheduleIdeaLocalCacheWrite()
        scheduleIdeaEngineSave(immediate: false)
    }

    private func scheduleIdeaLocalCacheWrite() {
        ideaLocalCacheTask?.cancel()
        ideaLocalCacheTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(ChatViewModel.ideaLocalCacheDebounce * 1_000_000_000))
            guard !Task.isCancelled, let self else { return }
            self.writeIdeaDraftToLocalCache()
        }
    }

    private func writeIdeaDraftToLocalCache() {
        guard let ideaID = selectedIdeaID else { return }
        let productID = selectedIdea?.productID ?? currentSelectedProductID
        guard let productID else { return }
        IdeaDraftCache.write(IdeaDraftCacheEntry(
            ideaID: ideaID,
            productID: productID,
            name: ideaDraftName,
            body: ideaDraftBody,
            savedAt: Date()
        ))
    }

    private func scheduleIdeaEngineSave(immediate: Bool) {
        ideaEngineSaveTask?.cancel()
        let delay = immediate ? 0 : ChatViewModel.ideaEngineSaveDebounce
        ideaEngineSaveTask = Task { [weak self] in
            if delay > 0 {
                try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            }
            guard !Task.isCancelled, let self else { return }
            self.sendIdeaEngineSave()
        }
    }

    private func sendIdeaEngineSave() {
        guard let ideaID = selectedIdeaID else { return }
        guard isConnected else {
            // Engine unreachable mid-edit: the local crash-floor write
            // (already scheduled/completed independently of this) is the
            // only protection right now, and that is by design — never
            // "show an error and drop the buffer". `loadIdeaDraft` re-sends
            // this exact edit the next time the idea is opened, and a
            // future `.connected` reconnect leaves it to that same path
            // rather than polling.
            ideaSaveStatus = .offlineSavedLocally
            return
        }
        ideaSaveStatus = .savingToEngine
        engine.sendUpdateIdea(id: ideaID, name: ideaDraftName, body: ideaDraftBody)
    }

    /// Synchronous, immediate flush of any pending idea edit: cancels both
    /// debounce timers, writes the local crash-floor cache right now, and
    /// fires the engine save right now if connected. Called before
    /// switching to a different idea, when `IdeasView` disappears (a
    /// nav-mode switch tears the view down — see `ContentView`'s
    /// structural-conditional placement note), and on app termination.
    func flushIdeaDraft() {
        guard selectedIdeaID != nil else { return }
        switch ideaSaveStatus {
        case .savedToEngine, .idle:
            return
        case .pendingLocal, .savingToEngine, .offlineSavedLocally:
            break
        }
        ideaLocalCacheTask?.cancel()
        ideaEngineSaveTask?.cancel()
        writeIdeaDraftToLocalCache()
        sendIdeaEngineSave()
    }

    /// Reception of `idea_created` — the reply to `createIdea()`. Inserts
    /// the new row and opens it for editing.
    func handleIdeaCreated(_ idea: WorkIdea) {
        upsertIdea(idea)
        selectedIdeaID = idea.id
        ideaDraftName = idea.name
        ideaDraftBody = idea.body
        ideaSaveStatus = .savedToEngine
    }

    /// Reception of `idea_updated`. Always refreshes the store entry so
    /// every list observer (including a stale one for a different
    /// session's edit to the same idea) sees the current row. Clears the
    /// local crash-floor cache and flips the status to "saved" only when
    /// this reply confirms the exact body currently in the editor — a
    /// stale reply for an earlier debounce cycle (the operator kept typing
    /// after the request went out) must not mark a newer, still-unsent
    /// edit as safe.
    func handleIdeaUpdated(_ idea: WorkIdea) {
        upsertIdea(idea)
        guard idea.id == selectedIdeaID else { return }
        if idea.body == ideaDraftBody, idea.name == ideaDraftName {
            IdeaDraftCache.clear(ideaID: idea.id)
            ideaSaveStatus = .savedToEngine
        }
    }

    private func upsertIdea(_ idea: WorkIdea) {
        var list = ideasByProductID[idea.productID] ?? []
        if let index = list.firstIndex(where: { $0.id == idea.id }) {
            list[index] = idea
        } else {
            list.insert(idea, at: 0)
        }
        ideasByProductID[idea.productID] = list
    }

    // MARK: - Send to coordinator

    /// A draft whose first line reads as a Claude Code slash command
    /// (`/…`) would be swallowed as a command instead of landing as prompt
    /// text — a property of the coordinator's own input surface (any first
    /// line starting with `/`, typed or pasted, trips it), not something
    /// the Ideas feature can fix upstream. Defeat it by prefixing a single
    /// space: the pasted text no longer starts with `/`, and a leading
    /// space is invisible once it lands in the prompt.
    nonisolated static func coordinatorSubmissionText(for draft: String) -> String {
        guard let firstLine = draft.split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: false).first,
              firstLine.hasPrefix("/")
        else {
            return draft
        }
        return " " + draft
    }

    /// Paste `ideaDraftBody` into the coordinator pane and submit it, as if
    /// it had been typed there, then switch to the Work tab — the
    /// coordinator pane is mounted only inside the Work surface
    /// (`ContentView.workBossPanel`), so sending without switching would
    /// land the text in a pane the operator can't see. Returns `false`
    /// when no coordinator pane is attached yet (e.g. a Bazel build
    /// without GhosttyKit, or the coordinator session hasn't attached).
    @discardableResult
    func sendIdeaDraftToCoordinator() -> Bool {
        guard let handler = sendToCoordinatorHandler else { return false }
        let text = Self.coordinatorSubmissionText(for: ideaDraftBody)
        let sent = handler(text)
        if sent {
            setNavigationMode(.work)
        }
        return sent
    }
}
