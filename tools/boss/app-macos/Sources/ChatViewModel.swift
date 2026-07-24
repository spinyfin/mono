import Foundation
import os
#if canImport(AppKit)
import AppKit
#endif

private let designDocTimingLog = Logger(subsystem: "com.boss.app", category: "DesignDocTiming")
private let markdownOpenLog = Logger(subsystem: "com.boss.app", category: "MarkdownOpen")

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var navigationMode: NavigationMode = .agents
    @Published var isConnected: Bool = false
    /// Full product list as reported by the engine, including archived
    /// rows. Keep the full set so id-based lookups (`product(withID:)`,
    /// work-tree merges) still resolve when a product was archived in
    /// another session; surfaces that let the user *select* a product
    /// should read [[activeProducts]] instead.
    @Published var products: [WorkProduct] = []

    /// Non-archived subset of [[products]], in the same sort order.
    /// This is what the sidebar Product picker, the Designs picker, and
    /// any other "products I work in actively" surface should bind to —
    /// archived products are history, not selection targets. Mirrors the
    /// CLI split: `boss product list` shows everything; the picker is
    /// for live products only.
    var activeProducts: [WorkProduct] {
        products.filter { $0.status != "archived" }
    }
    @Published var projectsByProductID: [String: [WorkProject]] = [:] {
        didSet { invalidateWorkCache() }
    }
    @Published var tasksByProjectID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    @Published var choresByProductID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    /// Revisions whose chain root is a chore. A revision inherits its
    /// `project_id` from the chain root (`insert_revision_in_tx`), so a
    /// chore-parented revision has none and cannot live in
    /// `tasksByProjectID`. Keyed by product so these rows still render as
    /// standalone Backlog/Doing cards and roll up under the parent chore's
    /// Review card. Without this bucket they were silently dropped at
    /// work-tree reception and invisible in the kanban (issue #789).
    @Published var productLevelRevisionsByProductID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    /// Product-level work items (`project_id IS NULL`) that are neither
    /// chores nor revisions — `kind == "investigation"` today, and any
    /// future product-level kind the engine emits. The work-tree handler
    /// used to drop every non-revision product-level row on the floor,
    /// so an investigation with no project was invisible on the board even
    /// while a live worker produced against it (issue #886). Routing the
    /// catch-all here makes the omission impossible by construction: a new
    /// kind lands in a real bucket and renders instead of vanishing.
    @Published var productLevelTasksByProductID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    @Published var taskRuntimesByID: [String: WorkTaskRuntime] = [:]
    /// Project-bucket keys in [[tasksByProjectID]] currently populated for
    /// each product, maintained incrementally by `applyWorkTree`. Lets a
    /// work-tree refresh evict exactly the stale buckets it's about to
    /// replace instead of scanning every product's buckets — without this,
    /// `applyWorkTree` cost grew with the total tasks/chores across every
    /// product ever viewed this session, not just the product being
    /// refreshed (see `ChatViewModel+WorkItemEvents.applyWorkTree`).
    var trackedProjectIDsByProductID: [String: Set<String>] = [:]
    /// Debounce handles for [[scheduleWorkTreeRefetch]], keyed by product
    /// id. A burst of invalidation-style events for the same product
    /// (bulk deletes, reorders, planner actions) collapses into one
    /// `GetWorkTree` request — and one full-tree apply — instead of one
    /// per event; see the "refetch storm" evidence in
    /// docs/investigations/task-population-latency-on-start-and-product-switch.md §10.2.
    var pendingWorkTreeRefetchTasks: [String: Task<Void, Never>] = [:]
    /// Dependency edges keyed by product. Refreshed whenever the engine
    /// pushes a fresh `WorkTree` for that product. The kanban joins
    /// these against the task/chore/project name maps to render
    /// "Blocked by <prereq title>" on gated cards.
    @Published var dependenciesByProductID: [String: [WorkItemDependency]] = [:] { didSet { invalidateWorkCache() } }
    /// Attention items keyed by work-item id (product id for external-tracker
    /// items). Populated on product selection and on every workTree refresh.
    @Published var attentionItemsByWorkItemID: [String: [WorkAttentionItem]] = [:]
    /// Open `deferred_scope` attention items keyed by product id. See
    /// `ChatViewModel+DeferredScope.swift`.
    @Published var deferredScopeAttentionsByProductID: [String: [DeferredScopeAttention]] = [:]
    /// Attention *groups* keyed by product id — the agent-authored
    /// notification feature (attentions.md), distinct from the operational
    /// `attentionItemsByWorkItemID` store above. Loaded on product selection /
    /// work-tree refresh and kept live via `AttentionCreated` /
    /// `AttentionGroupUpdated` / `AttentionGroupActioned` pushes. Holds open
    /// groups plus any that flipped to actioned/dismissed this session (so the
    /// produced-artifact link lingers until the next full reload).
    @Published var attentionGroupsByProductID: [String: [AttentionGroup]] = [:]
    /// Attention group *members* keyed by `AttentionGroup.id`, in display
    /// order. Populated alongside [[attentionGroupsByProductID]].
    @Published var attentionMembersByGroupID: [String: [Attention]] = [:]
    /// `attention_merges` provenance rows keyed by canonical `Attention.id`,
    /// fetched on demand for the merge-provenance affordance (score badge
    /// detail). Absent key means "not yet fetched", not "no merges".
    @Published var attentionMergesByAttentionID: [String: [AttentionMerge]] = [:]
    /// Planner audit rows (`planner_runs`) keyed by project id, newest
    /// first — as returned by `list_planner_runs`. Backs the Planner
    /// review/release/undo surface (design auto-populate-project-tasks-on-
    /// design-pr-merge.md task 10).
    @Published var plannerRunsByProjectID: [String: [PlannerRun]] = [:]
    /// Project ids with an in-flight `release_project` or
    /// `unpopulate_project` request — disables the action buttons until
    /// the engine replies (or `workError` clears it on failure).
    @Published var plannerActionInFlightProjectIDs: Set<String> = []
    /// Project id whose Planner Run inspector sheet is presented, or `nil`.
    @Published var plannerInspectorProjectID: String? = nil
    /// Historical execution rows keyed by task id. Populated on demand when
    /// the transcript viewer window sends `list_executions`. Cleared per-task
    /// before each fresh fetch so the viewer never shows stale rows.
    @Published var executionsByTaskID: [String: [ExecutionVM]] = [:]
    /// Transcript load state keyed by execution id. Populated on demand when
    /// the transcript viewer selects an execution (`execution_transcript`
    /// RPC). A `nil` (absent) entry means "not requested yet"; live
    /// executions can be re-fetched via [[refreshTranscript(executionId:)]].
    @Published var transcriptsByExecutionID: [String: TranscriptLoadState] = [:]
    /// Automations keyed by product id. Loaded when the Automations tab is
    /// entered or the selected product changes while the tab is active.
    @Published var automationsByProductID: [String: [AppAutomation]] = [:]
    /// Fetch state for the automations list keyed by product id. `nil` (absent)
    /// means no fetch has been issued yet; `.loading` means a request is in
    /// flight; `.loaded` means the response arrived; `.failed` means the fetch
    /// failed (connection dropped while in flight).
    @Published var automationsFetchStateByProductID: [String: AutomationsFetchState] = [:]
    /// Open-task counts keyed by automation id. Refreshed alongside the list.
    @Published var openTaskCountByAutomationID: [String: Int] = [:]
    /// Run history keyed by automation id. Fetched on selection and refreshed
    /// when the automation's state changes (outcome updated, etc.).
    @Published var automationRunsByID: [String: [AppAutomationRun]] = [:]
    /// The automation currently selected in the Automations tab detail pane.
    @Published var selectedAutomationID: String?
    /// Editorial-action audit rows keyed by product id. Populated on demand
    /// when the Editorial Controls sheet is opened for a product.
    @Published var editorialActionsByProductID: [String: [EditorialAction]] = [:]
    /// Fetch state for the editorial-actions list keyed by product id.
    @Published var editorialActionsFetchStateByProductID: [String: AutomationsFetchState] = [:]
    /// When non-nil, the Editorial Controls sheet is presented for this product id.
    @Published var editorialControlsProductID: String?
    @Published var selectedWorkProductID: String? {
        didSet { invalidateWorkCache() }
    }
    @Published var selectedProjectFilterIDs: Set<String> = [] {
        didSet { invalidateWorkCache() }
    }
    /// When true, the board shows all project-less work items (chores,
    /// investigation tasks, etc.) and their revisions. Mutually exclusive
    /// with `selectedProjectFilterIDs`.
    @Published var filterToChoresOnly: Bool = false {
        didSet { invalidateWorkCache() }
    }
    @Published var includeChores: Bool = true {
        didSet { invalidateWorkCache() }
    }
    @Published var showBlockedOnly: Bool = false {
        didSet { invalidateWorkCache() }
    }
    @Published var showArchivedProjects: Bool = false {
        didSet { invalidateWorkCache() }
    }
    @Published var selectedWorkCardID: String?
    /// Task id that the reveal animation is currently highlighting.
    /// Set by `revealWorkCard`; cleared after 1.5 s. Views observe
    /// this to apply a transient border-glow overlay on the matching
    /// card.
    @Published var revealHighlightID: String?
    /// Set of task IDs that should be highlighted as the actionable
    /// prerequisite frontier when the pointer is over a Dependency
    /// badge. Computed by `setDepBadgeHover`; cleared when the pointer
    /// leaves the badge. Views observe this to apply a transient
    /// amber border on every frontier card.
    @Published var depFrontierHighlightIDs: Set<String> = []
    /// Set of revision task IDs to highlight when the pointer is over an
    /// "In revision" badge. Computed by `setRevisionBadgeHover`; cleared
    /// on pointer exit. Uses the same green-border overlay as dep frontier.
    @Published var revisionHighlightIDs: Set<String> = []
    /// Task id that scroll views should bring into the visible area.
    /// Set by `revealWorkCard`; cleared after a short delay once the
    /// scroll has been triggered. Views observe this via `.onChange`
    /// on their `ScrollViewReader` proxies.
    @Published var revealScrollTarget: String?
    /// Task id whose card should be scrolled to once its product's
    /// work tree arrives. Used when a reveal crosses a product
    /// boundary — `revealWorkCard` sets this and the `workTree`
    /// event handler promotes it to `revealScrollTarget`.
    var pendingRevealScrollID: String?
    @Published var workBoardGrouping: WorkBoardGrouping = .none {
        didSet { invalidateWorkCache() }
    }
    @Published var selectedWorkNodeID: WorkNodeID?
    @Published var pendingWorkCreateRequest: WorkCreateRequest?
    @Published var pendingWorkEditRequest: WorkEditRequest?
    @Published var workErrorMessage: String?
    /// Current state of an in-flight `evaluate_editorial_rules` RPC.
    @Published var editorialEvaluationState: EditorialEvaluationState = .idle
    @Published var workSearchText: String = "" {
        didSet { invalidateWorkCache() }
    }
    @Published var isBossPanelCollapsed: Bool = false
    @Published var bossPanelWidth: CGFloat = 380
    /// Live runtime state for every active worker, sourced from the
    /// engine's LiveWorkerState snapshot (`worker_live_states_list`
    /// event) and refreshed on each push from the `worker.live_states`
    /// topic. Drives the kanban Doing icon (working / waiting / idle
    /// / errored) and the per-pane titlebar pill — replaces the
    /// screen-scrape-only signal that always rendered "Claude
    /// Unknown".
    ///
    /// Held on its own `ObservableObject` so the high-rate hook
    /// traffic that drives this snapshot doesn't invalidate every
    /// view that observes `ChatViewModel` (toolbar, sidebar, Boss
    /// panel, ContentView root). Only the views that actually read
    /// live state subscribe to the store.
    let liveWorkerStates = LiveWorkerStateStore()

    /// Slot ids whose live-status summarizer has been manually
    /// disabled by the human via the Agents-tab toggle. Sourced from
    /// `list_live_status_disabled_slots` at session start and kept
    /// in sync via `live_status_enabled_set` echoes. Persisted on
    /// the engine side so this is purely a UI mirror.
    @Published var liveStatusDisabledSlotIDs: Set<Int> = []

    /// Per-installation settings snapshot, sourced from `get_settings`
    /// on Settings window open and kept in sync via `setting_set`
    /// echoes after every toggle. Empty until the Settings window is
    /// first opened in this session.
    @Published var engineSettings: [EngineSetting] = []

    /// All registered hosts, populated by `list_hosts` on Settings-pane
    /// appear and updated in-place by `host_result`, `host_updated`, and
    /// `host_removed` responses.
    @Published var registeredHosts: [EngineHost] = []

    /// Engine-side configuration health issues sourced from
    /// `get_engine_health` at session start. Empty means the engine
    /// is healthy. Non-empty drives the top-of-window
    /// `EngineHealthBanner` and the Settings-pane warning so a
    /// missing `ANTHROPIC_API_KEY` (or any later "missing config"
    /// surface) is impossible to miss (#699).
    @Published var engineHealthIssues: [EngineHealthIssue] = []
    /// Top-level mirror of the same `get_engine_health` reply. Surfaced
    /// in the Settings pane next to the (future) API-key field so
    /// "key set" / "key missing" is legible without parsing the
    /// `issues` list. `true` until the engine answers at least once,
    /// so the banner doesn't flash on a transient reconnect.
    @Published var engineAnthropicApiKeyPresent: Bool = true

    /// Engine metrics snapshot — every registered counter and gauge —
    /// sourced from `metrics_list_live` on Metrics pane open and
    /// refreshed by the pane's 5-second polling timer. Empty until the
    /// pane has been opened in this session.
    @Published var engineMetrics: [EngineMetric] = []

    /// Engine feature-flag snapshot, sourced from `list_feature_flags`
    /// on debug-pane open and kept in sync via `feature_flag_set`
    /// echoes after every toggle. Backs the Feature Flags window
    /// (incident 001 AI #5). Empty when the pane has never been opened
    /// in this session.
    @Published var featureFlags: [FeatureFlag] = []

    /// Current GitHub OAuth auth state for github.com (OAuth device-flow
    /// design §3/§4). The engine owns a single per-host state; the app
    /// subscribes to the `github.auth` topic and refreshes this on every
    /// `git_hub_auth_state` push as the device flow advances. Backs the
    /// "GitHub account" subsection of the external-tracker settings.
    /// Defaults to `.disconnected` until the engine's first reply lands.
    @Published var gitHubAuthState: GitHubAuthState = .disconnected

    /// Resolved design-doc pointer state per project. Populated lazily
    /// when a project surface (kanban project header, future detail
    /// view) calls `resolveProjectDesignDoc(_:)`; refreshed whenever
    /// the engine pushes a fresh `WorkTree` so a re-pointing or unset
    /// from another session lands in the icon without a manual reload.
    /// A missing entry means "we haven't asked yet" — the affordance
    /// stays hidden until the engine replies.
    @Published var designDocStateByProjectID: [String: ProjectDesignDocState] = [:]
    /// Designs-tab markdown listings, keyed by product id. A missing
    /// entry means "not asked yet"; the value is the engine's classified
    /// outcome (loaded / no repo / unreachable / rate-limited / empty).
    /// Read and written by [[ChatViewModel+DesignDocs.swift]].
    @Published var designDocTreeByProductID: [String: DesignDocTreeState] = [:]
    /// Products with an outstanding listing request, so the tab can show
    /// a spinner without losing the previously-loaded listing underneath.
    @Published var designDocsLoadingProductIDs: Set<String> = []
    /// Fetched document bodies keyed by their full `(repo, path, ref)`
    /// triple. Keyed by the triple rather than held in a single
    /// "current document" slot so a slow fetch landing after the
    /// operator clicked elsewhere cannot overwrite the visible document.
    @Published var designDocContentByRef: [DesignDocRef: DesignDocContent] = [:]
    /// The document the Designs tab reader pane is showing, if any.
    @Published var selectedDesignDocRef: DesignDocRef?
    /// In-flight resolve-RPC batch. The engine resolves design-doc
    /// pointers in lock-step (responses arrive back-to-back regardless of
    /// per-project work), so stamping each project with its own
    /// start-to-response delta produces N near-identical numbers and
    /// destroys per-project attribution. Instead we track one batch per
    /// `refreshDesignDocStates` call and emit a single
    /// `phase=resolve project=batch count=<n>` summary when the last
    /// pending response arrives. Stray responses for projects outside the
    /// current batch (a refresh that landed mid-flight) still update
    /// state — they just don't drive timing.
    private struct DesignDocResolveBatch {
        var startDate: Date
        var pendingProjectIDs: Set<String>
        let initialCount: Int
    }
    private var currentDesignDocResolveBatch: DesignDocResolveBatch?

    /// Engine-tab attempt list, freshest first. Refreshed on Engine-tab
    /// entry, on `conflict_resolution_*` topic pushes, and on `Refresh`
    /// button taps. Phase 5 #14 of the merge-conflict design.
    @Published var conflictResolutions: [WorkConflictResolution] = [] {
        didSet { invalidateWorkCache() }
    }

    /// Engine-tab CI-remediation attempt list, freshest first.
    /// Mirror of [[conflictResolutions]]; refreshed on Engine-tab
    /// entry, on `ci_remediation_*` topic pushes, and on `Refresh`
    /// button taps. Phase 11 #37 of the merge-conflict design.
    @Published var ciRemediations: [WorkCiRemediation] = [] {
        didSet { invalidateWorkCache() }
    }

    /// PR URLs whose most recent CI-remediation attempt succeeded,
    /// with the wall-clock timestamp the engine reported (or the local
    /// observation time as a fallback). Drives the `"✅ ci auto-fixed"`
    /// PR-card chip per design Q11; cards whose PR sits in this map
    /// with an age under [[badgeFreshnessWindow]] render the chip.
    @Published var recentlyClearedCIPRs: [String: Date] = [:]

    /// Per-PR snapshot of the most recent observed CI exhaustion event.
    /// Carries the (used, budget) pair the engine sent so the kanban
    /// card can render `🟧 ci failing (used/budget)` or
    /// `🛑 ci failing (exhausted)` chips per design Q11. Cleared from
    /// the front of the map when the matching PR returns to
    /// `in_review` (observed via `ciRemediationSucceeded`).
    @Published var ciFailureBadges: [String: CiFailureBadge] = [:]

    /// `true` when this PR has a CI auto-fix that landed inside the
    /// badge window. Cards bind to this on the `Identifiable` task
    /// id; non-PR cards always return `false`.
    func showsCIAutoFixedBadge(forPR prURL: String?) -> Bool {
        guard let prURL,
              let clearedAt = recentlyClearedCIPRs[prURL] else {
            return false
        }
        return Date().timeIntervalSince(clearedAt) < badgeFreshnessWindow
    }

    /// CI-fail / exhausted chip for a PR card. `nil` when no active CI
    /// remediation is in flight (or budget exhaustion has not been
    /// observed). Cards bind to this on the `Identifiable` task id.
    func ciFailureBadge(forPR prURL: String?) -> CiFailureBadge? {
        guard let prURL else { return nil }
        return ciFailureBadges[prURL]
    }

    /// PR URLs whose most recent conflict-resolution attempt succeeded,
    /// with the wall-clock timestamp the engine reported (or the local
    /// observation time as a fallback). Drives the
    /// `"🔧 conflict cleared"` PR-card badge: cards whose PR sits in
    /// this map with an age under [[badgeFreshnessWindow]] render the
    /// chip. Phase 5 #15.
    @Published var recentlyClearedConflictPRs: [String: Date] = [:]

    /// 24-hour rolling window for the PR-card "🔧 conflict cleared"
    /// chip. Matches the auto-rebase-stacked-prs.md Q7 cadence so the
    /// two surfaces feel symmetric.
    static let conflictBadgeFreshnessWindow: TimeInterval = 24 * 60 * 60

    var badgeFreshnessWindow: TimeInterval { Self.conflictBadgeFreshnessWindow }

    /// `true` when this PR's most recent successful conflict-resolution
    /// landed inside the badge window. Cards bind to this on the
    /// `Identifiable` task id; non-PR cards always return `false`.
    func showsConflictClearedBadge(forPR prURL: String?) -> Bool {
        guard let prURL,
              let clearedAt = recentlyClearedConflictPRs[prURL] else {
            return false
        }
        return Date().timeIntervalSince(clearedAt) < badgeFreshnessWindow
    }

    /// Indirection for the OS URL opener used by [[openProjectDesignDoc(_:)]].
    /// Production defaults to `NSWorkspace.shared.open`; tests inject a
    /// recording stub so a `.resolved` click never hands a real GitHub
    /// blob URL to the OS during `swift test`. A test that fires the
    /// resolved branch without overriding this *will* pop the user's
    /// browser — see `ProjectDesignDocAffordanceTests` for the stub
    /// pattern.
    var urlOpener: (URL) -> Void = { url in
        #if canImport(AppKit)
        NSWorkspace.shared.open(url)
        #endif
    }

    /// Indirection for opening the in-app `DesignRendererView` window.
    /// Installed by [[ContentView]] using `@Environment(\.openWindow)`
    /// — the view model can't reach the SwiftUI environment directly,
    /// so the closure crosses the boundary. `nil` (the default for
    /// tests and headless contexts) falls the dispatcher back to the
    /// legacy `urlOpener(fileURL)` path that hands the file to the
    /// OS-registered `.md` handler.
    ///
    /// Wiring this closure is what swaps the project-card click
    /// affordance from `$EDITOR` to the in-app Textual renderer —
    /// chore #12 of [[project-design-doc-pointer.md]] and Q9's
    /// renderer-reuse acceptance.
    ///
    /// `didSet` notifies [[onDesignRendererWired]] the moment this
    /// becomes non-nil so observers (namely `AppDelegate`'s pending
    /// open-document buffer) can gate on "the renderer is actually
    /// wired" rather than on an unrelated signal like `chatModel`
    /// merely existing. `ContentView`'s `.task` and the `.task` that
    /// assigns `AppDelegate.chatModel` are two independent SwiftUI
    /// tasks with no ordering guarantee between them.
    var designRendererOpener: ((DesignRendererContent) -> Void)? {
        didSet {
            if designRendererOpener != nil {
                onDesignRendererWired?()
            }
        }
    }

    /// Fired once [[designRendererOpener]] is first wired to a non-nil
    /// closure. `AppDelegate` observes this to flush its pending
    /// markdown-open buffer at the correct time instead of racing on
    /// `chatModel` assignment.
    var onDesignRendererWired: (() -> Void)?

    /// Indirection for opening the markdown-viewer window with fetched
    /// content. Installed by [[ContentView]] using
    /// `@Environment(\.openWindow)` — same boundary-crossing pattern as
    /// [[designRendererOpener]]. Used when the design doc lives on a PR
    /// branch (not yet on `main`) and no leased workspace is available:
    /// the dispatcher fetches the raw content via [[rawContentFetcher]]
    /// and hands the rendered string to this opener. `nil` (tests and
    /// headless contexts) falls back to `urlOpener`.
    var markdownViewerOpener: ((MarkdownViewerContent) -> Void)?

    /// Indirection for opening the `"async-markdown-viewer"` Window
    /// immediately, before the design doc has been fetched. Installed by
    /// [[ContentView]] via `@Environment(\.openWindow)`. When set, the
    /// raw-content path opens the window first (loading state) then
    /// resolves content into [[asyncMarkdownViewerVM]]. `nil` (tests and
    /// headless) falls back to the legacy fetch-then-open path via
    /// [[markdownViewerOpener]].
    var asyncMarkdownViewerOpener: (() -> Void)?

    /// Shared state for the `"async-markdown-viewer"` Window scene.
    /// The window observes this object to transition from loading →
    /// loaded/failed without needing to pass content through the
    /// `openWindow` value type.
    let asyncMarkdownViewerVM = AsyncMarkdownViewerViewModel()

    /// Indirection for fetching raw markdown content from a URL.
    /// Production default routes through [[GitHubContentFetcher]] so
    /// the request authenticates as the user's active `gh` session and
    /// works for private repos. An unauthenticated `URLSession` fetch
    /// against `raw.githubusercontent.com` returns 404 for any private
    /// repo (issue #732), so this path must never reach `URLSession`.
    /// Tests inject a stub so the affordance tests never shell out.
    var rawContentFetcher: (URL) async throws -> String = { url in
        try await GitHubContentFetcher.fetch(url)
    }

    /// Indirection for opening the review-terminal window. Installed by
    /// [[ContentView]] using `@Environment(\.openWindow)`. Called on
    /// click (before the engine responds) so the window opens immediately
    /// in a loading state. `nil` in tests and headless contexts.
    var reviewTerminalOpener: (() -> Void)?

    /// Shared state for the `"review-terminal"` Window scene. Owned here
    /// and injected via EnvironmentObject so the window can observe the
    /// loading → ready transition without going through a value-type
    /// openWindow payload (which can't be updated after the window opens).
    let reviewTerminalVM = ReviewTerminalViewModel()

    /// Work item IDs for which `open_review_terminal` has been sent but
    /// `review_terminal_ready` (or `work_error`) has not yet arrived.
    /// Guards against a second click while the engine is still leasing.
    var openingReviewTerminalIDs: Set<String> = []

    /// Work item IDs for which `open_live_workspace_terminal` has been
    /// sent but `live_workspace_terminal_ready` (or `work_error`) has not
    /// yet arrived. Guards against a second click while the engine looks
    /// up the live execution's workspace.
    var openingLiveWorkspaceTerminalIDs: Set<String> = []

    /// Work item IDs for which `merge_when_ready` has been sent but
    /// `merge_when_ready_accepted` (or `work_error`) has not yet arrived.
    /// Guards against a duplicate tap while the engine is running the merge.
    var mergingWhenReadyIDs: Set<String> = []

    /// Inline confirmation banner shown next to a card whose
    /// `merge_when_ready_accepted` reply just arrived (e.g. "Submitted to
    /// Trunk merge queue"), keyed by the wire `action` value in
    /// `ChatViewModel+EventHandling`. Single-slot and auto-dismissed —
    /// mirrors `dragRefusalNotice`.
    struct MergeFeedbackNotice: Equatable {
        let taskID: String
        let message: String
    }

    /// Ask the engine to merge (or queue for merging) the PR for the given
    /// Review-column task. Guards against a duplicate tap while the RPC is
    /// in flight. The engine runs `gh pr merge --auto --squash` and kicks
    /// the PR-reconciler so the kanban state updates promptly on success.
    func mergeWhenReady(for task: WorkTask) {
        guard let prURL = task.prURL, !prURL.isEmpty else { return }
        _ = prURL  // consumed by the engine; kept here for the guard above
        guard !mergingWhenReadyIDs.contains(task.id) else { return }
        mergingWhenReadyIDs.insert(task.id)
        engine.sendMergeWhenReady(workItemID: task.id)
    }

    /// Ask the engine to lease a workspace for the given Review-column
    /// task's PR branch and open a terminal there. Opens the window
    /// immediately with a loading spinner; the terminal becomes live once
    /// the engine sends back `ReviewTerminalReady`.
    func openReviewTerminal(for task: WorkTask) {
        guard let prURL = task.prURL, !prURL.isEmpty else { return }
        guard !openingReviewTerminalIDs.contains(task.id) else {
            // Same task still loading — just re-focus the window.
            reviewTerminalOpener?()
            return
        }
        reviewTerminalVM.state = .loading(taskName: task.name)
        reviewTerminalOpener?()
        openingReviewTerminalIDs.insert(task.id)
        engine.sendOpenReviewTerminal(workItemID: task.id)
    }

    /// Notify the engine that the review terminal for `leaseID` has
    /// closed so the workspace lease can be released. Called from the
    /// `ReviewTerminalView.onDisappear` handler.
    func releaseReviewTerminal(leaseID: String) {
        engine.sendReleaseReviewTerminal(leaseID: leaseID)
    }

    /// Ask the engine for a terminal into a Doing-column task's already-
    /// live execution workspace — no new lease, just the path the running
    /// worker is already using. Opens the same window as
    /// `openReviewTerminal` with a loading spinner; becomes live once the
    /// engine sends back `LiveWorkspaceTerminalReady`. Unlike the review
    /// flow, the window's `onDisappear` never releases a lease, since the
    /// worker owns it for the lifetime of its run.
    func openLiveWorkspaceTerminal(for task: WorkTask) {
        guard !openingLiveWorkspaceTerminalIDs.contains(task.id) else {
            // Same task still loading — just re-focus the window.
            reviewTerminalOpener?()
            return
        }
        reviewTerminalVM.state = .loading(taskName: task.name)
        reviewTerminalOpener?()
        openingLiveWorkspaceTerminalIDs.insert(task.id)
        engine.sendOpenLiveWorkspaceTerminal(workItemID: task.id)
    }

    /// Fetch the execution history for `taskId` from the engine.
    /// Clears any cached rows first so the viewer shows a loading state.
    /// The engine replies with an `executions_list` event that populates
    /// [[executionsByTaskID]].
    func loadExecutions(taskId: String) {
        executionsByTaskID[taskId] = nil
        engine.sendListExecutions(taskId: taskId)
    }

    /// Fetch the rendered transcript for `executionId` the first time it is
    /// requested. Selecting an execution in the viewer calls this; an
    /// already-loaded, in-flight, or unavailable transcript is left
    /// untouched so re-selecting a row doesn't re-hit the engine. Use
    /// [[refreshTranscript(executionId:)]] to force a re-fetch.
    func loadTranscript(executionId: String) {
        if transcriptsByExecutionID[executionId] != nil { return }
        transcriptsByExecutionID[executionId] = .loading
        engine.sendExecutionTranscript(executionId: executionId)
    }

    /// Force a re-fetch of `executionId`'s transcript — the "Refresh"
    /// affordance on a still-running (live) execution, and the periodic
    /// poll while a live transcript's view is open. Deliberately leaves an
    /// already-`.loaded` doc in place while the fetch is in flight instead
    /// of flipping to `.loading`: swapping to the loading placeholder and
    /// back tears down and remounts `TranscriptView`, which resets its
    /// scroll position and per-segment expansion state on every refresh.
    /// Only [[loadTranscript(executionId:)]]'s first fetch (nothing loaded
    /// yet) needs the loading placeholder.
    func refreshTranscript(executionId: String) {
        if transcriptsByExecutionID[executionId] == nil {
            transcriptsByExecutionID[executionId] = .loading
        }
        engine.sendExecutionTranscript(executionId: executionId)
    }

    /// Toggle the live-status summarizer for `slotId`. Sends the
    /// RPC and optimistically updates local state; the engine echo
    /// brings the two back in sync.
    func setLiveStatusEnabled(slotId: Int, enabled: Bool) {
        if enabled {
            liveStatusDisabledSlotIDs.remove(slotId)
        } else {
            liveStatusDisabledSlotIDs.insert(slotId)
        }
        engine.sendSetLiveStatusEnabled(slotId: slotId, enabled: enabled)
    }

    /// `true` if the live-status summarizer is currently enabled for
    /// `slotId`. Defaults to enabled — the disabled set is the
    /// minority case.
    func isLiveStatusEnabled(slotId: Int) -> Bool {
        !liveStatusDisabledSlotIDs.contains(slotId)
    }

    /// Ask the engine for the current per-installation settings
    /// snapshot. Called by the Settings window on appear.
    func refreshSettings() {
        engine.sendGetSettings()
    }

    /// Ask the engine for a fresh engine-health snapshot. Also called
    /// on every reconnect from the `.connected` arm of `handle`; this
    /// wrapper exists so the Settings pane can re-poll on appear
    /// without exposing the private `engine` field.
    func refreshEngineHealth() {
        engine.sendGetEngineHealth()
    }

    /// User-initiated resume from the `dispatch_paused` health-banner
    /// issue. Drives the same `SetDispatchPaused { paused: false }`
    /// RPC `bossctl dispatch resume` uses; the engine owns the actual
    /// state change, this is a thin trigger. The engine has no push
    /// event for a health-state change, so this re-polls
    /// `get_engine_health` right behind the resume request (requests
    /// on one socket are processed in order) so the banner clears
    /// without waiting for the next reconnect.
    func resumeDispatch() {
        engine.sendSetDispatchPaused(paused: false)
        engine.sendGetEngineHealth()
    }

    /// Toggle one per-installation setting. Optimistically patches the
    /// cached snapshot so the UI feels instantaneous; the engine's
    /// `setting_set` echo reconciles state once the on-disk write
    /// returns.
    func setEngineSetting(key: String, enabled: Bool) {
        if let idx = engineSettings.firstIndex(where: { $0.key == key }) {
            let prior = engineSettings[idx]
            engineSettings[idx] = EngineSetting(
                key: prior.key,
                description: prior.description,
                defaultEnabled: prior.defaultEnabled,
                enabled: enabled
            )
        }
        engine.sendSetSetting(key: key, enabled: enabled)
    }


    var selectedProduct: WorkProduct? {
        guard let productID = currentSelectedProductID else { return nil }
        return product(withID: productID)
    }

    /// Automations for the currently selected product, ordered by `created_at`.
    var automationsForSelectedProduct: [AppAutomation] {
        guard let productID = currentSelectedProductID else { return [] }
        return automationsByProductID[productID] ?? []
    }

    /// Fetch state for the currently selected product's automations list.
    /// `nil` means no fetch has been issued for this product yet (treat like loading).
    var automationsFetchStateForSelectedProduct: AutomationsFetchState? {
        guard let productID = currentSelectedProductID else { return nil }
        return automationsFetchStateByProductID[productID]
    }

    /// The currently selected automation, looked up from the per-product list.
    var selectedAutomation: AppAutomation? {
        guard let id = selectedAutomationID else { return nil }
        return automationsForSelectedProduct.first { $0.id == id }
    }

    /// Unresolved attention items for the currently selected product.
    var selectedProductOpenAttentionItems: [WorkAttentionItem] {
        guard let productID = currentSelectedProductID else { return [] }
        return (attentionItemsByWorkItemID[productID] ?? []).filter { $0.resolvedAt == nil }
    }

    /// All known attention groups for the selected product (open plus any
    /// recently actioned/dismissed this session), newest-first.
    var selectedProductAttentionGroups: [AttentionGroup] {
        guard let productID = currentSelectedProductID else { return [] }
        return (attentionGroupsByProductID[productID] ?? [])
            .sorted { $0.createdAt > $1.createdAt }
    }

    /// Open (actionable) attention groups for the selected product — the
    /// Notifications window's primary list and the toolbar badge source.
    /// Ordered max-item-score-desc, then created-at-desc, so cards holding
    /// the most-corroborated items (design: notification-dedup-scoring.md
    /// §8) rise to the top; groups with no scored items keep today's
    /// newest-first order.
    var selectedProductOpenAttentionGroups: [AttentionGroup] {
        selectedProductAttentionGroups
            .filter(\.isOpen)
            .sorted { lhs, rhs in
                let lhsScore = maxItemScore(forGroup: lhs.id)
                let rhsScore = maxItemScore(forGroup: rhs.id)
                if lhsScore != rhsScore { return lhsScore > rhsScore }
                return lhs.createdAt > rhs.createdAt
            }
    }

    /// Count of open attention groups for the selected product. Drives the
    /// Notifications toolbar bell badge (hidden when 0).
    var openAttentionGroupCount: Int {
        selectedProductOpenAttentionGroups.count
    }

    /// Members of a group, in display order.
    func attentionMembers(forGroup groupID: String) -> [Attention] {
        (attentionMembersByGroupID[groupID] ?? []).sorted { $0.ordinal < $1.ordinal }
    }

    /// Highest `score` among a group's members — the priority signal used to
    /// badge and order cards. `1` (the default) for a group with no members
    /// loaded yet or no folds recorded against any of them.
    func maxItemScore(forGroup groupID: String) -> Int64 {
        (attentionMembersByGroupID[groupID] ?? []).map(\.score).max() ?? 1
    }

    var selectedProject: WorkProject? {
        guard selectedProjectFilterIDs.count == 1,
              let projectID = selectedProjectFilterIDs.first else { return nil }
        return project(withID: projectID)
    }

    var projectFilterDescription: String {
        if filterToChoresOnly { return "No Project" }
        let visibleSelected = visibleSelectedProjectFilterIDs
        switch visibleSelected.count {
        case 0:
            return "All projects"
        case 1:
            if let id = visibleSelected.first, let project = self.project(withID: id) {
                return project.name
            }
            return "1 project"
        case let count:
            return "\(count) projects"
        }
    }

    var hasProjectFilters: Bool {
        !visibleSelectedProjectFilterIDs.isEmpty || filterToChoresOnly
    }

    /// Subset of `selectedProjectFilterIDs` whose projects are currently
    /// visible in the sidebar. When archived projects are hidden, their
    /// IDs may still be in the filter set (so toggling Show Archived
    /// back on restores the prior selection), but counts and badges
    /// must only reflect what the user can see.
    private var visibleSelectedProjectFilterIDs: Set<String> {
        guard !selectedProjectFilterIDs.isEmpty else { return [] }
        let visibleIDs = Set(projectsForSelectedProduct.map(\.id))
        return selectedProjectFilterIDs.intersection(visibleIDs)
    }

    var selectedTask: WorkTask? {
        guard let taskID = selectedWorkCardID else { return nil }
        return task(withID: taskID)
    }

    var projectsForSelectedProduct: [WorkProject] {
        let all = allProjectsForSelectedProduct
        guard !showArchivedProjects else { return all }
        return all.filter { $0.status != "archived" }
    }

    /// Unfiltered project list for the selected product, used by code
    /// paths that need full visibility regardless of the sidebar's
    /// Show Archived toggle (e.g. boss-agent context where the LLM
    /// must know archived projects exist so it doesn't recreate them).
    var allProjectsForSelectedProduct: [WorkProject] {
        guard let productID = currentSelectedProductID else { return [] }
        return (projectsByProductID[productID] ?? []).sorted(by: projectSort)
    }

    var visibleWorkItems: [WorkTask] {
        if let cached = cachedVisibleItems {
            return cached
        }
        let computed = computeVisibleWorkItems()
        cachedVisibleItems = computed
        return computed
    }

    /// Repo names (lowercased) that resolve to more than one org across
    /// the currently visible card set's PR URLs. Drives the board-local
    /// disambiguation rule for kanban PR-link labels: a repo name in
    /// this set must render as `org/repo#n`; everything else can drop
    /// to the bare `repo#n`. Cached on the same lifetime as
    /// [[visibleWorkItems]] — invalidated by [[invalidateWorkCache]].
    var ambiguousVisibleRepoNames: Set<String> {
        if let cached = cachedAmbiguousRepoNames {
            return cached
        }
        let computed = ambiguousPRRepoNames(in: visibleWorkItems)
        cachedAmbiguousRepoNames = computed
        return computed
    }

    /// The active board search query with surrounding whitespace removed,
    /// or `nil` when no search filter is in effect. Single source of truth
    /// for both the filter logic below and the persistent "filtered view"
    /// banner so the two can never disagree about whether the board is
    /// showing a subset (issue #1248).
    var activeWorkSearchQuery: String? {
        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)
        return query.isEmpty ? nil : query
    }

    /// True while a free-text search is hiding non-matching cards. Drives
    /// the kanban filter banner so a stale search can't be mistaken for an
    /// empty or complete board.
    var isWorkSearchActive: Bool { activeWorkSearchQuery != nil }

    private func computeVisibleWorkItems() -> [WorkTask] {
        guard let productID = currentSelectedProductID else { return [] }

        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)

        var items: [WorkTask] = []
        if filterToChoresOnly {
            items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
            items.append(contentsOf: (productLevelTasksByProductID[productID] ?? []).sorted(by: taskSort))
            items.append(contentsOf: (productLevelRevisionsByProductID[productID] ?? []).sorted(by: taskSort))
        } else {
            let projectFilter = visibleSelectedProjectFilterIDs
            for project in projectsForSelectedProduct {
                guard projectFilter.isEmpty || projectFilter.contains(project.id) else { continue }
                items.append(contentsOf: (tasksByProjectID[project.id] ?? []).sorted(by: taskSort))
            }
            // Product-level work items (investigations, etc.) have no project, so a
            // project filter legitimately excludes them; otherwise they always
            // render. They are first-class work — not gated by the chores toggle,
            // which would otherwise hide an investigation a live worker is
            // producing against (issue #886).
            if projectFilter.isEmpty {
                items.append(contentsOf: (productLevelTasksByProductID[productID] ?? []).sorted(by: taskSort))
            }
            if includeChores && projectFilter.isEmpty {
                items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
                // Chore-parented revisions have no project of their own; surface
                // them with the chores so their Backlog/Doing cards appear. The
                // in-review ones are rolled up under the parent and filtered out
                // of the Review column by `workItems(in:)`.
                items.append(contentsOf: (productLevelRevisionsByProductID[productID] ?? []).sorted(by: taskSort))
            }
        }

        // Automation-sourced chores are real work items that need human review.
        // They appear on the kanban like any other chore — the card detail view
        // marks them with a purple wand icon to indicate automation provenance.
        // Do NOT filter them out here: a chore in in_review status needs to
        // be visible so the operator can review and merge the PR.

        if showBlockedOnly {
            items = items.filter { $0.status == "blocked" }
        }

        guard !query.isEmpty else {
            return items
        }

        let matched = items.filter { item in
            item.name.localizedCaseInsensitiveContains(query)
                || item.description.localizedCaseInsensitiveContains(query)
                || (item.prURL?.localizedCaseInsensitiveContains(query) ?? false)
                || (projectName(for: item.projectID)?.localizedCaseInsensitiveContains(query) ?? false)
                || item.status.localizedCaseInsensitiveContains(query)
                || (item.shortID.map { "T\($0)" }?.localizedCaseInsensitiveContains(query) ?? false)
        }

        // A bare or "T"-prefixed number (e.g. "2801", "T2801", "t2801") is a
        // short id lookup. The substring match above already surfaces ids
        // containing that number anywhere (prefix search), but the id it
        // names exactly should always be the one the user actually finds —
        // pull it to the front rather than leaving it wherever taskSort put it.
        guard let exactShortID = Self.parseShortIDQuery(query) else {
            return matched
        }
        var exact: [WorkTask] = []
        var rest: [WorkTask] = []
        for item in matched {
            if item.shortID == exactShortID {
                exact.append(item)
            } else {
                rest.append(item)
            }
        }
        return exact + rest
    }

    /// Parses a search query as a short-id lookup: bare digits ("2801") or a
    /// case-insensitive "T"-prefixed number ("T2801", "t2801"). Returns `nil`
    /// for anything else so plain text search is unaffected.
    static func parseShortIDQuery(_ query: String) -> Int? {
        var digits = Substring(query)
        if let first = digits.first, first == "T" || first == "t" {
            digits = digits.dropFirst()
        }
        guard !digits.isEmpty, digits.allSatisfy({ $0.isNumber }) else { return nil }
        return Int(digits)
    }

    let engine: EngineClient
    /// Routes engine comment RPC replies + `comments.artifact.*` invalidations
    /// to the open [`CommentLayer`]s (P529 Phase 2). Injected into the markdown
    /// viewers via the `@EnvironmentObject` `ChatViewModel`.
    let commentBridge: CommentEngineBridge
    /// Test-only hook: forwarded to `EngineClient.outboundRecorder`
    /// so an XCTest can assert that the form's submit lands the
    /// expected `repo_remote_url` on the wire. The real socket write
    /// still runs (against a stub path that fails harmlessly in
    /// tests).
    var outboundRecorder: (([String: Any]) -> Void)? {
        get { engine.outboundRecorder }
        set { engine.outboundRecorder = newValue }
    }
    private let processController: EngineProcessController
    private let paths: BossEnginePaths
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    /// Becomes `true` the first time the socket reaches `.ready`, so the
    /// Disconnected banner stays hidden during the initial-connect window.
    @Published private(set) var hasConnectedOnce = false
    @Published var showConnectionLostBanner = false // see ChatViewModel+Connection.swift
    static let connectionLostBannerDelay: TimeInterval = 2.0 // grace period before a disconnect may raise the banner
    var connectionGeneration = 0 // bumped on connect/disconnect; supersedes a stale banner-reveal
    var subscribedWorkTopics: Set<String> = []
    private let defaults = UserDefaults.standard

    /// Notification manager for Review-lane transitions. Fires a system
    /// banner when a task reaches `in_review` while the app is backgrounded.
    let reviewNotifier = ReviewNotificationCenter()
    #if canImport(AppKit)
    private var appActivationObserver: NSObjectProtocol?
    #endif

    /// Task IDs currently known to be in `in_review`. Populated from
    /// work-tree snapshots (without firing) on load/reconnect, and
    /// updated incrementally on `workItemUpdated` events. Guards against
    /// re-notifying for a task that was already in Review when the app
    /// launched or re-subscribed.
    var knownReviewTaskIDs: Set<String> = []

    private let navigationModeDefaultsKey = "boss.navigationMode"
    private let selectedWorkProductDefaultsKey = "boss.work.selectedProductID"
    private let selectedProjectFilterIDsDefaultsKey = "boss.work.projectFilterIDs"
    private let filterToChoresOnlyDefaultsKey = "boss.work.filterToChoresOnly"
    private let includeChoresDefaultsKey = "boss.work.includeChores"
    private let showBlockedOnlyDefaultsKey = "boss.work.showBlockedOnly"
    private let showArchivedProjectsDefaultsKey = "boss.work.showArchivedProjects"
    private let workBoardGroupingDefaultsKey = "boss.work.grouping"
    private let bossPanelCollapsedDefaultsKey = "boss.work.bossPanelCollapsed"
    private let bossPanelWidthDefaultsKey = "boss.work.bossPanelWidth"

    init(paths: BossEnginePaths) {
        self.paths = paths
        self.socketPath = paths.socketPath
        self.processController = EngineProcessController(paths: paths)
        let showSystem = ProcessInfo.processInfo.environment["BOSS_SHOW_SYSTEM_MESSAGES"] ?? ""
        showSystemMessages = showSystem == "1" || showSystem.lowercased() == "true"
        engine = EngineClient(socketPath: paths.socketPath)
        commentBridge = CommentEngineBridge(engine: engine)

        commonInit()
    }

    /// Test-only convenience: build a `ChatViewModel` whose engine
    /// paths are all derived from a single per-test `socketPath` so a
    /// test never touches the production pid file or control token.
    /// Mirrors the call shape `ChatViewModel(socketPath:)` that
    /// pre-issue-#705 tests used, but routes through
    /// `BossEnginePaths.forTest(...)` so the test-context refusal in
    /// `BossEnginePaths.production*()` still applies to anything that
    /// reaches for the canonical paths.
    convenience init(socketPath: String) {
        let paths = BossEnginePaths.forTest(
            socketPath: socketPath,
            pidPath: "\(socketPath).pid",
            controlTokenPath: "\(socketPath).token"
        )
        self.init(paths: paths)
    }

    private func commonInit() {

        if let rawMode = defaults.string(forKey: navigationModeDefaultsKey),
           let persistedMode = NavigationMode(rawValue: rawMode) {
            navigationMode = persistedMode
        }
        selectedWorkProductID = defaults.string(forKey: selectedWorkProductDefaultsKey)
        if let storedFilters = defaults.array(forKey: selectedProjectFilterIDsDefaultsKey) as? [String] {
            selectedProjectFilterIDs = Set(storedFilters)
        }
        filterToChoresOnly = defaults.bool(forKey: filterToChoresOnlyDefaultsKey)
        if defaults.object(forKey: includeChoresDefaultsKey) != nil {
            includeChores = defaults.bool(forKey: includeChoresDefaultsKey)
        }
        showBlockedOnly = defaults.bool(forKey: showBlockedOnlyDefaultsKey)
        showArchivedProjects = defaults.bool(forKey: showArchivedProjectsDefaultsKey)
        if let groupingRaw = defaults.string(forKey: workBoardGroupingDefaultsKey),
           let grouping = WorkBoardGrouping(rawValue: groupingRaw) {
            workBoardGrouping = grouping
        }
        isBossPanelCollapsed = defaults.bool(forKey: bossPanelCollapsedDefaultsKey)
        let savedWidth = defaults.double(forKey: bossPanelWidthDefaultsKey)
        if savedWidth > 0 {
            bossPanelWidth = savedWidth
        }

        processController.onOutputLine = { [weak self] line in
            self?.appendSystemMessage(line)
        }

        bindEngineEventStream()

        reviewNotifier.configure()
        reviewNotifier.onSelectWorkItem = { [weak self] taskID in
            self?.setNavigationMode(.work)
            self?.selectWorkCard(taskID)
        }

        // In the AppKit-hosted macOS shell, the root SwiftUI `.task` can be
        // missed on some launches. Schedule the normal startup path here too so
        // the engine connection still comes up reliably.
        DispatchQueue.main.async { [weak self] in
            self?.startIfNeeded()
        }

        #if canImport(AppKit)
        // Kick PR-state reconcilers immediately when the user returns to Boss
        // from another app (e.g. after reviewing or merging a PR on GitHub).
        // The engine quiesces repeated kicks within a 15 s window so rapid
        // focus-toggle events don't hammer the GitHub API.
        //
        // `MainActor.assumeIsolated` is safe here because we pass `queue: .main`
        // — the closure always runs on the main queue, which is the main actor's
        // executor.
        appActivationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.isConnected else { return }
                self.engine.sendKickPrReconcilers()
            }
        }
        #endif
    }

    deinit {
        processController.stop()
        engine.stop()
    }

    func toggleBossPanelCollapsed() {
        isBossPanelCollapsed.toggle()
        defaults.set(isBossPanelCollapsed, forKey: bossPanelCollapsedDefaultsKey)
    }

    func setBossPanelWidth(_ width: CGFloat) {
        bossPanelWidth = width
        defaults.set(width, forKey: bossPanelWidthDefaultsKey)
    }

    func setNavigationMode(_ mode: NavigationMode) {
        // Instrument the tab switch so the pane-grid relayout it provokes
        // (how many panes rebuild, the settle wall-time, any unexpected
        // teardown) is measurable for the high-CPU investigation. See
        // [[TerminalLoopMonitor]].
        if navigationMode != mode {
            TerminalLoopMonitor.shared.noteTabSwitch(
                from: navigationMode.rawValue,
                to: mode.rawValue
            )
        }
        navigationMode = mode
        defaults.set(mode.rawValue, forKey: navigationModeDefaultsKey)
        if mode == .work {
            refreshWork()
        }
        if mode == .automations {
            refreshAutomations()
        }
    }

    func selectWorkProduct(_ productID: String) {
        let isAlreadyShowingProductBoard =
            selectedWorkProductID == productID
            && selectedProjectFilterIDs.isEmpty
            && selectedWorkCardID == nil
        guard !isAlreadyShowingProductBoard else { return }
        selectedWorkProductID = productID
        selectedProjectFilterIDs = []
        selectedWorkCardID = nil
        workErrorMessage = nil
        persistSelectedProductID(productID)
        persistProjectFilterIDs()
        refreshWorkSubscriptions()
        if isConnected {
            engine.sendGetWorkTree(productId: productID, flow: .productSwitch)
            engine.sendListAttentionItemsForWorkItem(workItemID: productID)
            engine.sendListAttentionGroups(productId: productID)
            engine.sendListDeferredScopeAttentions(productId: productID)
        }
    }

    func toggleProjectFilter(_ projectID: String) {
        if filterToChoresOnly {
            filterToChoresOnly = false
            defaults.set(false, forKey: filterToChoresOnlyDefaultsKey)
        }
        if selectedProjectFilterIDs.contains(projectID) {
            selectedProjectFilterIDs.remove(projectID)
        } else {
            selectedProjectFilterIDs.insert(projectID)
        }
        selectedWorkCardID = nil
        persistProjectFilterIDs()
    }

    func clearProjectFilters() {
        guard !selectedProjectFilterIDs.isEmpty || filterToChoresOnly else { return }
        selectedProjectFilterIDs = []
        filterToChoresOnly = false
        defaults.set(false, forKey: filterToChoresOnlyDefaultsKey)
        selectedWorkCardID = nil
        persistProjectFilterIDs()
    }

    func setFilterToChoresOnly(_ value: Bool) {
        guard filterToChoresOnly != value else { return }
        filterToChoresOnly = value
        defaults.set(value, forKey: filterToChoresOnlyDefaultsKey)
        if value {
            selectedProjectFilterIDs = []
            persistProjectFilterIDs()
        }
        selectedWorkCardID = nil
    }

    func archiveProject(id: String) {
        engine.sendUpdateWorkItem(id: id, patch: ["status": "archived"])
    }

    func setIncludeChores(_ value: Bool) {
        guard includeChores != value else { return }
        includeChores = value
        defaults.set(value, forKey: includeChoresDefaultsKey)
    }

    func setShowBlockedOnly(_ value: Bool) {
        guard showBlockedOnly != value else { return }
        showBlockedOnly = value
        defaults.set(value, forKey: showBlockedOnlyDefaultsKey)
    }

    func setShowArchivedProjects(_ value: Bool) {
        guard showArchivedProjects != value else { return }
        showArchivedProjects = value
        defaults.set(value, forKey: showArchivedProjectsDefaultsKey)
    }

    /// Persist (or clear, when `nil`) the selected product so the next
    /// launch restores the board the operator left open.
    func persistSelectedProductID(_ productID: String?) {
        if let productID {
            defaults.set(productID, forKey: selectedWorkProductDefaultsKey)
        } else {
            defaults.removeObject(forKey: selectedWorkProductDefaultsKey)
        }
    }

    func persistProjectFilterIDs() {
        if selectedProjectFilterIDs.isEmpty {
            defaults.removeObject(forKey: selectedProjectFilterIDsDefaultsKey)
        } else {
            defaults.set(Array(selectedProjectFilterIDs).sorted(), forKey: selectedProjectFilterIDsDefaultsKey)
        }
    }

    func selectWorkCard(_ taskID: String?) {
        selectedWorkCardID = taskID
        guard let taskID, let task = task(withID: taskID) else { return }
        selectedWorkProductID = task.productID
    }

    /// Navigate the kanban to `taskID` and play a 1.5 s highlight.
    /// Switches to the Work tab, selects the task's product, clears
    /// every active board filter, and queues a scroll. If the task's
    /// product is not the one currently loaded, the scroll is deferred
    /// until the `workTree` event for that product arrives.
    ///
    /// Reveal's contract is "show me this card", so it must override any
    /// filter that would hide the target — a stale search query, a
    /// blocked-only / chores-only toggle, a project filter, or chores
    /// being hidden — all of which can exclude the card and make the
    /// scroll silently land on nothing (#1249). We reset the board to its
    /// unfiltered state before scrolling so the revealed card is
    /// guaranteed visible.
    ///
    /// `taskID` itself is not always the card that gets scrolled to/
    /// highlighted — see `revealCardTarget(for:)`: a revision rolled up
    /// onto its parent's card redirects to the parent. The returned
    /// `RevealCardResult` tells the caller (the `reveal_work_item` IPC
    /// handler) whether a real card was reached, deferred pending a
    /// product-tree fetch, or unreachable — so it can answer bossctl
    /// truthfully instead of always claiming success.
    @discardableResult
    func revealWorkCard(_ taskID: String, productID: String) -> RevealCardResult {
        let outcome = revealCardTarget(for: taskID)
        let hostCardID: String
        switch outcome {
        case .revealed(let cardID):
            hostCardID = cardID
        case .deferred:
            hostCardID = taskID
        case .unreachable:
            return outcome
        }
        setNavigationMode(.work)
        clearWorkFiltersForReveal()
        selectedWorkCardID = hostCardID
        let isProductSwitch = currentSelectedProductID != productID
        if isProductSwitch {
            selectWorkProduct(productID)
            pendingRevealScrollID = hostCardID
        } else {
            triggerRevealScroll(hostCardID)
        }
        revealHighlightID = hostCardID
        let capturedID = hostCardID
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
            if self?.revealHighlightID == capturedID {
                self?.revealHighlightID = nil
            }
        }
        return outcome
    }

    /// Reset every board filter that could hide a reveal target so the
    /// full work board for the product is shown. Each assignment is a
    /// no-op when the filter is already in its neutral state, so this is
    /// cheap to call unconditionally. Keep this in sync with
    /// `computeVisibleWorkItems` — any new narrowing filter added there
    /// must be neutralized here too, or reveal can silently fail again.
    private func clearWorkFiltersForReveal() {
        selectedProjectFilterIDs = []
        workSearchText = ""
        showBlockedOnly = false
        filterToChoresOnly = false
        includeChores = true
    }

    func triggerRevealScroll(_ taskID: String) {
        revealScrollTarget = taskID
        let capturedID = taskID
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            if self?.revealScrollTarget == capturedID {
                self?.revealScrollTarget = nil
            }
        }
    }

    func setWorkBoardGrouping(_ grouping: WorkBoardGrouping) {
        workBoardGrouping = grouping
        defaults.set(grouping.rawValue, forKey: workBoardGroupingDefaultsKey)
    }

    func presentCreateProduct() {
        pendingWorkCreateRequest = WorkCreateRequest(kind: .product)
    }

    func presentCreateProject() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .project(productID: productID))
    }

    func presentCreateTask() {
        guard let project = taskCreationProject else { return }
        pendingWorkCreateRequest = WorkCreateRequest(
            kind: .task(productID: project.productID, projectID: project.id)
        )
    }

    func presentCreateChore() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .chore(productID: productID))
    }

    func dismissWorkCreateRequest() {
        pendingWorkCreateRequest = nil
    }

    func presentEditSelectedWorkItem() {
        if let task = selectedTask {
            pendingWorkEditRequest = WorkEditRequest(item: task.isChore ? .chore(task) : .task(task))
        } else if let project = selectedProject {
            pendingWorkEditRequest = WorkEditRequest(item: .project(project))
        } else if let product = selectedProduct {
            pendingWorkEditRequest = WorkEditRequest(item: .product(product))
        }
    }

    func presentEditSelectedProduct() {
        guard let product = selectedProduct else { return }
        pendingWorkEditRequest = WorkEditRequest(item: .product(product))
    }

    func dismissWorkEditRequest() {
        pendingWorkEditRequest = nil
    }

    func evaluateEditorialRules(productId: String, body: String, title: String?) {
        editorialEvaluationState = .loading
        engine.sendEvaluateEditorialRules(productId: productId, body: body, title: title?.isEmpty == true ? nil : title)
    }

    func submitWorkCreateRequest(
        _ request: WorkCreateRequest,
        name: String,
        description: String,
        repoRemoteURL: String = "",
        goal: String = "",
        setAsProductDefault: Bool = false
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        workErrorMessage = nil
        let repoOverride = repoRemoteURL.trimmingCharacters(in: .whitespacesAndNewlines)
        switch request.kind {
        case .product:
            engine.sendCreateProduct(
                name: trimmedName,
                description: description,
                repoRemoteURL: repoRemoteURL
            )
        case .project(let productID):
            engine.sendCreateProject(
                productId: productID,
                name: trimmedName,
                description: description,
                goal: goal
            )
        case .task(let productID, let projectID):
            engine.sendCreateTask(
                productId: productID,
                projectId: projectID,
                name: trimmedName,
                description: description,
                repoRemoteURL: repoOverride.isEmpty ? nil : repoOverride
            )
            if setAsProductDefault && !repoOverride.isEmpty {
                engine.sendUpdateWorkItem(
                    id: productID,
                    patch: ["repo_remote_url": repoOverride]
                )
            }
        case .chore(let productID):
            engine.sendCreateChore(
                productId: productID,
                name: trimmedName,
                description: description,
                repoRemoteURL: repoOverride.isEmpty ? nil : repoOverride
            )
            if setAsProductDefault && !repoOverride.isEmpty {
                engine.sendUpdateWorkItem(
                    id: productID,
                    patch: ["repo_remote_url": repoOverride]
                )
            }
        }

        pendingWorkCreateRequest = nil
    }

    /// Empirical known-repo set for `productID`, mirroring the CLI's
    /// `known_repos_for_product` (multi-repo design Q4). Returns the
    /// distinct, non-empty `repo_remote_url` values across the
    /// product's tasks and chores, plus the product's own default if
    /// set. Sorted by short-name for stable picker ordering, with the
    /// product default first when present so the picker leads with
    /// the "obvious" choice.
    ///
    /// All inputs come from the work tree the model already has on
    /// hand; no engine RPC. Returns an empty array when the product
    /// is unknown.
    func knownReposForProduct(_ productID: String) -> [String] {
        guard products.contains(where: { $0.id == productID }) else {
            return []
        }
        var seen: Set<String> = []
        var result: [String] = []
        let productDefault = products
            .first(where: { $0.id == productID })?
            .repoRemoteURL?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let productDefault, !productDefault.isEmpty {
            seen.insert(productDefault)
            result.append(productDefault)
        }
        var rest: [String] = []
        let projects = projectsByProductID[productID] ?? []
        for project in projects {
            for task in tasksByProjectID[project.id] ?? [] {
                if let url = task.repoRemoteURL?.trimmingCharacters(in: .whitespacesAndNewlines),
                   !url.isEmpty, !seen.contains(url) {
                    seen.insert(url)
                    rest.append(url)
                }
            }
        }
        for chore in choresByProductID[productID] ?? [] {
            if let url = chore.repoRemoteURL?.trimmingCharacters(in: .whitespacesAndNewlines),
               !url.isEmpty, !seen.contains(url) {
                seen.insert(url)
                rest.append(url)
            }
        }
        rest.sort { shortRepoName(for: $0) < shortRepoName(for: $1) }
        result.append(contentsOf: rest)
        return result
    }

    /// Product default repo URL, looked up by id. Used by
    /// `WorkCreateSheet` to construct a `WorkCreateRepoFormState`
    /// without reaching into `products` itself. `nil` for an unknown
    /// product or one whose URL is empty / whitespace.
    func productDefaultRepoURL(_ productID: String) -> String? {
        let raw = products.first(where: { $0.id == productID })?.repoRemoteURL
        let trimmed = raw?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let trimmed, !trimmed.isEmpty { return trimmed }
        return nil
    }

    func submitWorkEditRequest(
        _ request: WorkEditRequest,
        name: String,
        description: String,
        status: String,
        repoRemoteURL: String = "",
        goal: String = "",
        priority: String = "",
        prURL: String = "",
        workerBranchPrefix: String = "",
        docsRepo: String = ""
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        var patch: [String: Any] = [
            "name": trimmedName,
            "description": description,
            "status": status,
        ]

        let id: String
        switch request.item {
        case .product(let product):
            id = product.id
            patch["repo_remote_url"] = repoRemoteURL
            patch["worker_branch_prefix"] = workerBranchPrefix
            patch["docs_repo"] = docsRepo
        case .project(let project):
            id = project.id
            patch["goal"] = goal
            patch["priority"] = priority
        case .task(let task), .chore(let task):
            id = task.id
            patch["pr_url"] = prURL
            // Only send a priority patch when the user actually
            // touched the picker — keeps unrelated edits from
            // bouncing the field through serde-validation noise.
            if !priority.isEmpty, priority != task.priority {
                patch["priority"] = priority
            }
        }

        engine.sendUpdateWorkItem(id: id, patch: patch)
        pendingWorkEditRequest = nil
    }

    func setProductExternalTracker(
        productId: String,
        kind: String,
        org: String,
        repo: String,
        projectNumber: Int,
        reverseClose: Bool
    ) {
        let config: [String: Any] = [
            "org": org,
            "repo": repo,
            "project_number": projectNumber,
            "reverse_close": reverseClose,
        ]
        engine.sendSetProductExternalTracker(productId: productId, kind: kind, config: config)
    }

    func unsetProductExternalTracker(productId: String) {
        engine.sendUnsetProductExternalTracker(productId: productId)
    }

    func deleteSelectedWorkItem() {
        guard let task = selectedTask else { return }
        engine.sendDeleteWorkItem(id: task.id)
    }

    func deleteWorkItem(id: String) {
        engine.sendDeleteWorkItem(id: id)
    }

    func moveSelectedTask(offset: Int) {
        guard let task = selectedTask,
              !task.isChore,
              let projectID = task.projectID,
              var tasks = tasksByProjectID[projectID]?.sorted(by: taskSort),
              let currentIndex = tasks.firstIndex(where: { $0.id == task.id })
        else {
            return
        }

        let destination = currentIndex + offset
        guard tasks.indices.contains(destination) else { return }

        tasks.swapAt(currentIndex, destination)
        engine.sendReorderProjectTasks(projectId: projectID, taskIds: tasks.map(\.id))
    }

    /// Move a card between kanban columns. Two extra concerns vs. a
    /// pure status edit, both per `tools/boss/docs/designs/work-kanban.md`:
    ///
    /// - Drop into Doing (target status `active`) also fires
    ///   `RequestExecution` so the engine schedules a worker. The
    ///   engine is idempotent — a non-terminal execution already
    ///   running for this work item won't get a duplicate.
    /// - Move OUT of Doing while a live worker is attached is
    ///   blocked — except for two intentional gestures:
    ///   (a) Dragging back to Backlog (`todo`): engine stops the worker,
    ///       releases the lease, and parks the card — no autostart.
    ///   (b) Terminal transitions (`done`, `archived`): these mirror the
    ///       engine's own lifecycle resolutions and are always allowed.
    func moveTask(_ taskID: String, to column: WorkBoardColumnKey) {
        guard let task = task(withID: taskID) else { return }
        let targetStatus = column.targetStatus
        guard task.status != targetStatus else { return }

        if task.status == "active"
            && !Self.terminalKanbanStatuses.contains(targetStatus)
            && column != .backlog  // backlog drag = stop+park: engine handles teardown
            && hasLiveWorker(forTaskID: taskID)
        {
            appendSystemMessage(
                "\(task.name) is being worked on by a live worker. Stop the worker before moving the card out of Doing.",
                alwaysShow: true
            )
            return
        }

        // Optimistic update: move the card to the destination column immediately
        // before the RPC completes. The engine remains the authority — on failure
        // we bounce back via bounceBackOptimisticMoves.
        let originColumn = effectiveBoardColumn(for: task)
        pendingMoveOriginByTaskID[taskID] = originColumn
        optimisticColumnByTaskID[taskID] = column
        invalidateWorkCache()

        engine.sendUpdateWorkItem(id: task.id, patch: ["status": targetStatus])

        if targetStatus == "active" {
            engine.sendRequestExecution(workItemId: task.id)
        }
    }

    /// Statuses that the engine itself can drive a chore into at run
    /// completion. The kanban must allow the human to mirror those
    /// transitions even from `active` so a successful PR-merge flow
    /// can move a card to Done without first stopping the worker.
    private static let terminalKanbanStatuses: Set<String> = [
        "done",
        "archived",
    ]

    /// True iff the work item has a non-terminal worker currently
    /// attached (running, paused on input, or idle between turns).
    /// `WorkerActivity.terminated` and `.errored` count as "no live
    /// worker" — the slot is no longer holding the run open.
    private func hasLiveWorker(forTaskID taskID: String) -> Bool {
        guard let live = workerLiveState(forTaskID: taskID) else {
            return false
        }
        switch live.activity {
        case .terminated, .errored:
            return false
        case .spawning, .working, .waitingForInput, .idle:
            return true
        }
    }

    func toggleBlocked(for taskID: String) {
        guard let task = task(withID: taskID) else { return }
        let nextStatus: String
        switch task.status {
        case "blocked":
            nextStatus = "active"
        case "active":
            nextStatus = "blocked"
        default:
            return
        }
        engine.sendUpdateWorkItem(id: task.id, patch: ["status": nextStatus])
    }

    /// Update a task or chore's priority via the inline picker on the
    /// detail popover. No-ops when the new value matches the current
    /// one so an idle picker tap doesn't generate write traffic.
    func setPriority(for taskID: String, to priority: WorkPriority) {
        guard let task = task(withID: taskID) else { return }
        guard task.priority != priority.rawValue else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["priority": priority.rawValue])
    }

    func startIfNeeded() {
        guard !didStart else { return }

        // Swap-on-startup fallback (design doc §4): if a staged update is ready and
        // the user is in automatic mode, replace the bundle *before* the engine
        // launches (so the new engine binary is what gets spawned), then hand off to
        // the detached relaunch helper and exit — it relaunches us into the new
        // version. If no swap applies, this returns false and we continue normally.
        // Placed here because this is the single chokepoint guaranteed to run before
        // `processController.start()`. See [[UpdateLifecycle]].
        if UpdateLifecycle.applyStartupSwapIfNeeded() {
            exit(0)
        }

        didStart = true

        let autostart = ProcessInfo.processInfo.environment["BOSS_ENGINE_AUTOSTART"] != "0"
        if autostart {
            let processController = self.processController
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                do {
                    try processController.start()
                    DispatchQueue.main.async {
                        self?.startEngineIfNeeded()
                    }
                } catch {
                    DispatchQueue.main.async {
                        self?.appendSystemMessage(
                            "Failed to launch engine: \(error.localizedDescription)",
                            alwaysShow: true
                        )
                    }
                }
            }
        } else {
            startEngineIfNeeded()
        }
    }

    /// `true` while a user-initiated engine restart is running. The
    /// unreachable banner binds its "Restart engine" button to the
    /// inverse so a second click can't queue another terminate +
    /// relaunch on top of the first one (issue #697).
    @Published private(set) var isRestartingEngine = false

    /// User-initiated recovery from the unreachable banner. Terminates
    /// the engine the pid file points at (token-auth shutdown RPC
    /// first, then SIGTERM/SIGKILL — same path `stop()` uses) and
    /// relaunches it. The `EngineClient` reconnect loop picks the new
    /// socket up automatically once it accepts.
    ///
    /// Routes the terminate+launch through the same background queue
    /// `startIfNeeded()` uses so the main thread never blocks on
    /// `terminateEngine`'s up-to-5s SIGKILL wait. `isRestartingEngine`
    /// drives the banner button's `.disabled` state.
    func restartEngine() {
        guard !isRestartingEngine else { return }
        isRestartingEngine = true

        let processController = self.processController
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var restartError: Error?
            do {
                try processController.restart()
            } catch {
                restartError = error
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.isRestartingEngine = false
                if let restartError {
                    self.appendSystemMessage(
                        "Failed to restart engine: \(restartError.localizedDescription)",
                        alwaysShow: true
                    )
                }
                // Make sure the EngineClient is started even if the
                // very first `startIfNeeded()` failed before launching
                // it (autostart=0 paths also flow through here).
                self.startEngineIfNeeded()
            }
        }
    }

    func refreshWork() {
        guard isConnected else { return }
        engine.sendListProducts()
        if let productID = currentSelectedProductID {
            engine.sendGetWorkTree(productId: productID, flow: .manualRefresh)
        }
    }

    /// Ask the engine to resolve the design-doc pointer for every
    /// project whose row carries a non-nil `designDocPath`. Projects
    /// with no pointer set are skipped so the engine doesn't burn an
    /// RPC just to be told `not_set` — the affordance is hidden in
    /// that case anyway. Re-issued on every `WorkTree` so a re-point
    /// landed in another session flows through to the icon.
    func refreshDesignDocStates(for projects: [WorkProject]) {
        guard isConnected else { return }
        let pending = projects.filter { $0.designDocPath != nil }
        guard !pending.isEmpty else { return }
        currentDesignDocResolveBatch = DesignDocResolveBatch(
            startDate: Date(),
            pendingProjectIDs: Set(pending.map(\.id)),
            initialCount: pending.count
        )
        for project in pending {
            engine.sendResolveProjectDesignDoc(projectID: project.id)
        }
    }

    /// Open the design-doc pointer for `project`. Dispatch follows
    /// `ProjectDesignDocState`:
    ///
    /// - `.notSet` — affordance shouldn't have been clickable. No-op.
    /// - `.broken` — surface the engine's reason as a work error so
    ///   the user can re-point. The re-point sheet is tracked
    ///   separately (design Q5).
    /// - `.resolved` — dispatch priority:
    ///   1. `rawContentURL` present: fetch from GitHub via [[rawContentFetcher]]
    ///      and open in the async markdown viewer. This is correct for both
    ///      merged (main) and in-review (PR branch) docs — the GitHub ref in
    ///      the URL is the authoritative source regardless of cube workspace
    ///      state. A leased workspace may be on a different task's branch even
    ///      when `resolved.branch == "main"`, so reading from disk is not safe.
    ///   2. `rawContentURL` absent (non-GitHub repo or older engine) AND a
    ///      workspace is leased for the resolved repo AND branch is `main`:
    ///      render via [[designRendererOpener]] (in-app renderer) when wired,
    ///      otherwise hand the `file://` URL to [[urlOpener]].
    ///   3. Fall through to [[urlOpener]] with the web URL.
    func openProjectDesignDoc(_ project: WorkProject) {
        let shortID = project.shortID.map { "\($0)" } ?? project.id
        let state = designDocStateByProjectID[project.id] ?? .notSet
        switch state {
        case .notSet:
            return
        case .broken(let reason):
            workErrorMessage = "Design doc pointer is broken: \(reason)"
        case .resolved(let resolved, let workspacePath, let webURL, let rawContentURL):
            // Prefer fetching via rawContentURL (GitHub API). This is correct
            // regardless of cube workspace state — the workspace may be on a
            // different branch even when resolved.branch == "main".
            if let rawContentURL, let rawURL = URL(string: rawContentURL) {
                let projectName = project.name
                let clickStart = Date()
                designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=rawContentURL")
                if let opener = asyncMarkdownViewerOpener {
                    // Open the window immediately in a loading state, then
                    // resolve the content asynchronously — the user sees a
                    // window within one frame of the click (T-open-immediately).
                    asyncMarkdownViewerVM.state = .loading
                    asyncMarkdownViewerVM.clickStartTime = clickStart
                    let openWindowStart = Date()
                    opener()
                    let openWindowMs = Int(Date().timeIntervalSince(openWindowStart) * 1000)
                    designDocTimingLog.info("phase=open_window project=\(shortID, privacy: .public) duration_ms=\(openWindowMs, privacy: .public)")
                    Task { @MainActor in
                        await self.fetchAndUpdateAsyncMarkdownViewerVM(
                            projectName: projectName,
                            rawURL: rawURL,
                            projectShortID: shortID,
                            artifact: resolved.commentArtifact
                        )
                    }
                } else {
                    // Headless / test path: fetch first, then open via the
                    // legacy markdownViewerOpener (or fall back to urlOpener).
                    Task { @MainActor in
                        await self.fetchAndOpenDesignDoc(
                            projectName: projectName,
                            rawURL: rawURL,
                            webURL: webURL,
                            projectShortID: shortID
                        )
                    }
                }
                return
            }
            // rawContentURL absent (non-GitHub repo or older engine): fall back
            // to the workspace fast-path for merged docs when a workspace is
            // available. Only safe for branch == "main" designs where we can
            // reasonably assume the workspace holds the merged file.
            if let workspacePath, isWorkspaceFastPathEligible(kind: resolved.kind),
               resolved.branch == "main" {
                designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=workspace")
                if let opener = designRendererOpener,
                   let content = DesignRendererContent.from(
                       projectID: project.id,
                       projectName: project.name,
                       resolved: resolved,
                       workspacePath: workspacePath,
                       webURL: webURL
                   ) {
                    opener(content)
                    return
                }
                let absolute = (workspacePath as NSString)
                    .appendingPathComponent(resolved.path)
                urlOpener(URL(fileURLWithPath: absolute))
                return
            }
            guard let url = URL(string: webURL) else {
                workErrorMessage = "Design doc URL could not be parsed: \(webURL)"
                return
            }
            designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=webURL")
            urlOpener(url)
        }
    }

    /// Open a local `.md`/`.markdown` file in the in-app design renderer,
    /// reusing [[designRendererOpener]] — the same window and rendering
    /// path as [[openProjectDesignDoc]] and File ▸ Open (⌘O). This is the
    /// shared entry point for every "open a markdown file" surface: the
    /// File ▸ Open panel, `open -a Boss foo.md` from the shell, and
    /// Finder's "Open With ▸ Boss" (both routed through the app's
    /// `application(_:open:)` delegate callback, which calls this after
    /// [[designRendererOpener]] is wired).
    ///
    /// `allowOSFallback` controls what happens when the renderer isn't
    /// wired: the File ▸ Open panel path (the default, `true`) falls
    /// back to `urlOpener` (the OS-registered handler) — safe there
    /// because the user explicitly picked the file from within Boss, not
    /// because the OS handed it to Boss. The OS open-document path
    /// (`AppDelegate.application(_:open:)`) passes `false`: an event
    /// that arrived *from* LaunchServices must never be handed back to
    /// `NSWorkspace.shared.open`, since Boss can itself be the
    /// OS-registered `.md` handler after this change — falling back
    /// would re-dispatch the file to Boss's own open-document handler
    /// (or silently bounce it to a different app). When the fallback is
    /// disallowed and the renderer isn't wired, the open is dropped with
    /// a log line rather than silently lost.
    func openLocalMarkdownFile(url: URL, allowOSFallback: Bool = true) {
        let content = DesignRendererContent.forLocalFile(path: url.path)
        if let opener = designRendererOpener {
            opener(content)
        } else if allowOSFallback {
            urlOpener(url)
        } else {
            markdownOpenLog.warning(
                "Dropped OS-delivered markdown open for \(url.path, privacy: .public) — design renderer not wired yet"
            )
        }
    }

    /// Fetch raw markdown from `rawURL` and open it in the
    /// [[markdownViewerOpener]] window. Falls back to `urlOpener(webURL)`
    /// if the fetch fails or [[markdownViewerOpener]] is not wired.
    @MainActor
    func fetchAndOpenDesignDoc(
        projectName: String,
        rawURL: URL,
        webURL: String,
        projectShortID: String
    ) async {
        do {
            let fetchStart = Date()
            designDocTimingLog.info("phase=fetch_start project=\(projectShortID, privacy: .public) url=\(rawURL.absoluteString, privacy: .public)")
            let markdown = try await rawContentFetcher(rawURL)
            let fetchMs = Int(Date().timeIntervalSince(fetchStart) * 1000)
            designDocTimingLog.info("phase=fetch_end project=\(projectShortID, privacy: .public) duration_ms=\(fetchMs, privacy: .public) bytes=\(markdown.utf8.count, privacy: .public)")
            if let opener = markdownViewerOpener {
                let title = projectName.isEmpty ? rawURL.lastPathComponent : projectName
                opener(MarkdownViewerContent(title: title, markdown: markdown))
            } else if let url = URL(string: webURL) {
                urlOpener(url)
            }
        } catch {
            if let url = URL(string: webURL) {
                urlOpener(url)
            } else {
                workErrorMessage = "Failed to fetch design doc: \(error.localizedDescription)"
            }
        }
    }

    /// Fetch raw markdown from `rawURL` and update [[asyncMarkdownViewerVM]]
    /// state. Called after the viewer window is already open in `.loading`
    /// state. Transitions to `.loaded` on success or `.failed` on error so
    /// the window always resolves to a terminal state. `artifact` (built by
    /// the caller from the resolved doc's repo/branch/path, mirroring
    /// `DesignRendererContent.commentArtifact`) is carried into `.loaded` so
    /// comments on this viewer are engine-backed instead of in-memory.
    @MainActor
    func fetchAndUpdateAsyncMarkdownViewerVM(
        projectName: String,
        rawURL: URL,
        projectShortID: String,
        artifact: CommentArtifactRef? = nil
    ) async {
        let title = projectName.isEmpty ? rawURL.lastPathComponent : projectName
        do {
            let fetchStart = Date()
            designDocTimingLog.info("phase=fetch_start project=\(projectShortID, privacy: .public) url=\(rawURL.absoluteString, privacy: .public)")
            let markdown = try await rawContentFetcher(rawURL)
            let fetchMs = Int(Date().timeIntervalSince(fetchStart) * 1000)
            designDocTimingLog.info("phase=fetch_end project=\(projectShortID, privacy: .public) duration_ms=\(fetchMs, privacy: .public) bytes=\(markdown.utf8.count, privacy: .public)")
            asyncMarkdownViewerVM.pendingRenderProjectShortID = projectShortID
            asyncMarkdownViewerVM.renderStartTime = Date()
            asyncMarkdownViewerVM.renderContentID = UUID()
            asyncMarkdownViewerVM.state = .loaded(title: title, markdown: markdown, artifact: artifact)
        } catch {
            asyncMarkdownViewerVM.state = .failed(
                title: title,
                message: error.localizedDescription
            )
        }
    }

    /// Apply a resolve response and close out the in-flight batch's timing
    /// summary once its last project reports. Stray responses for projects
    /// outside the current batch (a refresh that landed mid-flight) still
    /// update state — they just don't drive timing. Called from the
    /// `.projectDesignDocResolved` arm in [[ChatViewModel+EventHandling.swift]].
    func applyResolvedProjectDesignDoc(_ output: ResolveProjectDesignDocOutput) {
        if var batch = currentDesignDocResolveBatch,
           batch.pendingProjectIDs.remove(output.projectID) != nil {
            if batch.pendingProjectIDs.isEmpty {
                let ms = Int(Date().timeIntervalSince(batch.startDate) * 1000)
                designDocTimingLog.info("phase=resolve project=batch count=\(batch.initialCount, privacy: .public) duration_ms=\(ms, privacy: .public)")
                currentDesignDocResolveBatch = nil
            } else {
                currentDesignDocResolveBatch = batch
            }
        }
        designDocStateByProjectID[output.projectID] = output.state
    }

    /// Kanban open-affordance fast-path predicate: a `ResolvedDesignDocKind`
    /// is editor-eligible exactly when the doc lives in a repo Boss
    /// tracks as a Product (same- or other-product). External pointers
    /// always fall through to the web URL because cube can't lease
    /// untracked repos.
    private func isWorkspaceFastPathEligible(kind: ResolvedDesignDocKind) -> Bool {
        switch kind {
        case .sameProduct, .otherProduct:
            return true
        case .external:
            return false
        }
    }

    // MARK: - Pane bridge

    /// Handlers ContentView installs so the engine can drive libghostty panes
    /// through this model. The `engine_request` arms that call them live in
    /// [[ChatViewModel+EventHandling.swift]]; a build without GhosttyKit
    /// leaves them `nil` and those arms answer with a failure.
    var paneSpawnHandler: ((EngineSpawnRequest) -> EngineSpawnResult)?
    var paneReleaseHandler: ((Int, UInt32) -> EngineReleaseResult)?
    var paneSendHandler: ((Int, String) -> EngineSendResult)?
    var paneFocusHandler: ((Int) -> EngineFocusResult)?
    var paneInterruptHandler: ((Int) -> EngineInterruptResult)?
    /// Enumerates every slot the app currently hosts a session in,
    /// regardless of whether the engine has a live-tracked run for it.
    /// Backs `bossctl agents list --all`. `nil` build (Bazel without
    /// GhosttyKit) replies with an empty list — there is no pane
    /// allocator to enumerate.
    var paneListHostedHandler: (() -> [EngineHostedPaneEntry])?
    /// Invoked when the engine pushes `engine_pool_config`: forwards pool sizes to
    /// `WorkersWorkspaceModel` and coordinator model to `BossPaneModel`.
    /// Parameters: workerSlots, automationSlots, reviewSlots, coordinatorModel.
    var panePoolConfigHandler: ((Int, Int, Int, String) -> Void)?

    /// Whether the engine has confirmed this client is the registered app session.
    /// Reset on disconnect (see [[ChatViewModel+EventHandling.swift]]); set when
    /// `appSessionRegistered` is received.
    var isAppSessionRegistered = false
    /// Returns the Boss pane's current shell pid from
    /// `ghostty_surface_foreground_pid`. Injected by ContentView (GhosttyKit
    /// build only). Returns 0 when the surface is not yet live.
    var bossPaneShellPidProvider: (() -> Int32)?

    // MARK: - Lookups and shared helpers

    var currentSelectedProductID: String? {
        selectedWorkProductID
    }

    private var taskCreationProject: WorkProject? {
        if let selectedProject {
            return selectedProject
        }
        if let selectedTask, let projectID = selectedTask.projectID {
            return project(withID: projectID)
        }
        return nil
    }

    func workTopic(forProductID productID: String) -> String {
        "work.product.\(productID)"
    }

    private var desiredWorkTopics: Set<String> {
        // `github.auth` is a global (per-host, not per-product) topic
        // carrying GitHub OAuth auth-state pushes; the engine fans every
        // device-flow transition out on it. We stay subscribed for the
        // whole session so the "GitHub account" settings subsection
        // re-renders live (OAuth device-flow design §4, TOPIC_GITHUB_AUTH).
        // `engine.health` carries health-state changes (dispatch pause/resume,
        // etc.) so the banner updates live without polling or restarting.
        var topics: Set<String> = ["work.products", "worker.live_states", "github.auth", "engine.health"]
        if let productID = currentSelectedProductID {
            topics.insert(workTopic(forProductID: productID))
        }
        return topics
    }

    /// Records a successful (re)connect. Exists so `hasConnectedOnce` can stay
    /// `private(set)` — `private` is file-scoped in Swift, and the `.connected`
    /// arm that flips it lives in [[ChatViewModel+EventHandling.swift]].
    func markConnected() {
        isConnected = true
        hasConnectedOnce = true
    }

    func refreshWorkSubscriptions() {
        guard isConnected else { return }
        let desired = desiredWorkTopics
        let toSubscribe = desired.subtracting(subscribedWorkTopics)
        let toUnsubscribe = subscribedWorkTopics.subtracting(desired)

        if !toUnsubscribe.isEmpty {
            engine.sendUnsubscribe(topics: Array(toUnsubscribe).sorted())
        }
        if !toSubscribe.isEmpty {
            engine.sendSubscribe(topics: Array(toSubscribe).sorted())
        }

        subscribedWorkTopics = desired
    }

    private func startEngineIfNeeded() {
        guard !didStartEngine else { return }
        didStartEngine = true
        engine.start()
    }


    func appendSystemMessage(_ text: String, alwaysShow: Bool = false) {
        guard alwaysShow || showSystemMessages else { return }
        FileHandle.standardError.write(Data("\(text)\n".utf8))
    }

    /// Non-private: [[ChatViewModel+BoardHelpers.swift]] resolves a task's
    /// owning product across several repo/badge helpers.
    func product(withID id: String) -> WorkProduct? {
        products.first { $0.id == id }
    }

    /// Lookup a project row by id across every product the model has
    /// loaded. Non-private so view code (the kanban project-card
    /// affordance) can resolve a section's `projectID` to a full
    /// `WorkProject` without re-walking the projects map itself.
    func project(withID id: String) -> WorkProject? {
        for projects in projectsByProductID.values {
            if let project = projects.first(where: { $0.id == id }) {
                return project
            }
        }
        return nil
    }

    func task(withID id: String) -> WorkTask? {
        if taskIndexByID == nil { rebuildTaskIndex() }
        return taskIndexByID?[id]
    }

    /// Look up any task or chore by id. Used by the kanban to resolve
    /// the parent task for revision card chrome.
    func workTask(withID id: String) -> WorkTask? {
        task(withID: id)
    }

    /// Cached output of `visibleWorkItems`. Filled lazily on read; reset to
    /// `nil` whenever a published input changes (see `invalidateWorkCache`).
    /// Keeps engine pushes that don't touch the work tree (e.g.
    /// `worker.live_states`) from re-walking the projects/tasks/chores trees.
    private var cachedVisibleItems: [WorkTask]?
    var cachedItemsByColumn: [WorkBoardColumnKey: [WorkTask]] = [:]
    var cachedSectionsByColumn: [WorkBoardColumnKey: [WorkBoardSection]] = [:]
    private var cachedAmbiguousRepoNames: Set<String>?
    /// O(1) id → work-item index over every task/chore/revision bucket.
    /// Built lazily on first lookup after any change (see `rebuildTaskIndex`);
    /// replaces a linear scan of all four buckets per `task(withID:)` call.
    /// Invalidated alongside the other caches whenever a published input
    /// changes (every bucket `didSet` routes through `invalidateWorkCache`).
    var taskIndexByID: [String: WorkTask]?
    /// Backing storage for the `dependencyPrereqsByTaskID` / `gatingPrereqsByTaskID`
    /// accessors (in ChatViewModel+Dependencies). `nil` means "invalidated —
    /// rebuild on next read". Rebuilt lazily so a burst of engine events during
    /// startup coalesces into a single rebuild at the next render instead of one
    /// full graph walk per event.
    var cachedDependencyPrereqs: [String: [WorkDependencyRow]]?
    var cachedGatingPrereqs: [String: [WorkDependencyRow]]?
    /// Backing storage for `inReviewRevisions(forParentTaskID:)` /
    /// `doneRevisions(forParentTaskID:)` (ChatViewModel+BoardHelpers). `nil`
    /// means "invalidated — rebuild on next read". Before this cache
    /// existed, both accessors re-scanned every project's tasks and every
    /// product's revisions on EVERY call, and the kanban calls both once
    /// per visible card on every render. Hover-highlight state
    /// (`revisionHighlightIDs` et al) lives on this `@Published` view model,
    /// so a single badge hover during a scroll re-renders the whole board
    /// and re-ran that O(total tasks) scan per card — a measured
    /// main-thread hot leaf during hover-while-scroll jank. Rebuilt lazily,
    /// same pattern as `cachedGatingPrereqs`.
    var cachedInReviewRevisionsByParentID: [String: [WorkTask]]?
    var cachedDoneRevisionsByParentID: [String: [WorkTask]]?

    func invalidateWorkCache() {
        cachedVisibleItems = nil
        cachedItemsByColumn.removeAll(keepingCapacity: true)
        cachedSectionsByColumn.removeAll(keepingCapacity: true)
        cachedAmbiguousRepoNames = nil
        taskIndexByID = nil
        cachedDependencyPrereqs = nil
        cachedGatingPrereqs = nil
        cachedInReviewRevisionsByParentID = nil
        cachedDoneRevisionsByParentID = nil
    }

    /// Inline drag-refusal banner shown next to the source card when a
    /// drag from Blocked → Doing is rejected because the row still has
    /// unsatisfied gating prereqs (design item 11). Single-slot — the
    /// previous notice is replaced when a new refusal fires.
    @Published var dragRefusalNotice: DragRefusalNotice?

    /// Inline confirmation banner shown on the card whose
    /// `merge_when_ready_accepted` reply just arrived (`MergeFeedbackNotice`)
    /// — for `trunk_enqueued`/`enqueued` the engine's optimistic
    /// `merge_queue_state` write routes the card into the Merging section in
    /// the same handler that emits the reply, so the banner typically shows
    /// on a Merging-section card, not a Review-lane one. If the Merging
    /// section is collapsed, the banner is not visible for those actions and
    /// the 5s auto-dismiss expires unseen — acceptable for now since the
    /// section defaults to expanded. Set and auto-dismissed from
    /// `ChatViewModel+EventHandling`.
    @Published var mergeFeedbackNotice: MergeFeedbackNotice?

    // MARK: - Optimistic kanban moves

    /// Optimistic column override for a card whose drop has been accepted
    /// in the UI but not yet confirmed by the engine. `effectiveBoardColumn`
    /// consults this before falling back to the real task status, giving an
    /// instant visual response on drop.
    var optimisticColumnByTaskID: [String: WorkBoardColumnKey] = [:]
    /// Origin column for each in-flight optimistic move. Kept until the
    /// engine's `workItemUpdated` event confirms the transition (at which
    /// point it is removed without clearing the override). If `work_error`
    /// arrives while entries remain here, the card bounces back.
    var pendingMoveOriginByTaskID: [String: WorkBoardColumnKey] = [:]

    // MARK: - Live worker state

    /// Resolve a task to its current LiveWorkerState by joining
    /// `task → execution_id → run_id`. Returns `nil` when the task
    /// has no active execution or the engine has not yet seen any
    /// hook events for the run (so the live state map is empty).
    func workerLiveState(forTaskID taskID: String) -> WorkerLiveState? {
        guard let runtime = taskRuntimesByID[taskID],
              let executionID = runtime.executionID
        else {
            return nil
        }
        return liveWorkerStates.byRunID[executionID]
    }
}
