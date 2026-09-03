import Foundation
#if canImport(AppKit)
import AppKit
#endif

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var navigationMode: NavigationMode = .agents
    @Published var isConnected: Bool = false
    /// Full product list as reported by the engine, including archived
    /// rows. Keep the full set so id-based lookups (`product(withID:)`,
    /// work-tree merges) still resolve when a product was archived in
    /// another session; surfaces that let the user *select* a product
    /// should read [[activeProducts]] instead.
    @Published var products: [WorkProduct] = [] {
        didSet { notePublishedWorkInputChanged() }
    }

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
        didSet { notePublishedWorkInputChanged() }
    }
    @Published var tasksByProjectID: [String: [WorkTask]] = [:] {
        didSet { notePublishedWorkInputChanged() }
    }
    @Published var choresByProductID: [String: [WorkTask]] = [:] {
        didSet { notePublishedWorkInputChanged() }
    }
    /// Revisions whose chain root is a chore. A revision inherits its
    /// `project_id` from the chain root (`insert_revision_in_tx`), so a
    /// chore-parented revision has none and cannot live in
    /// `tasksByProjectID`. Keyed by product so these rows still render as
    /// standalone Backlog/Doing cards and roll up under the parent chore's
    /// Review card. Without this bucket they were silently dropped at
    /// work-tree reception and invisible in the kanban (issue #789).
    @Published var productLevelRevisionsByProductID: [String: [WorkTask]] = [:] {
        didSet { notePublishedWorkInputChanged() }
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
        didSet { notePublishedWorkInputChanged() }
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
    @Published var dependenciesByProductID: [String: [WorkItemDependency]] = [:] {
        didSet { notePublishedWorkInputChanged() }
    }
    /// Attention items keyed by work-item id (product id for external-tracker
    /// items). Populated on product selection and on every workTree refresh.
    @Published var attentionItemsByWorkItemID: [String: [WorkAttentionItem]] = [:]
    /// Open `deferred_scope` attention items keyed by product id. See
    /// `ChatViewModel+DeferredScope.swift`.
    @Published var deferredScopeAttentionsByProductID: [String: [DeferredScopeAttention]] = [:]
    /// Attention item ids for which `accept_deferred_scope_attention` or
    /// `create_task_from_deferred_scope_attention` has been sent but neither
    /// the success push (`attentionItemUpdated`/`attentionItemConverted`) nor
    /// `work_error` has arrived yet. Drives the popover row's disabled
    /// "acting" state — published (unlike `mergingWhenReadyIDs`) because the
    /// row reads it directly to know when to re-enable. Cleared wholesale on
    /// any `work_error` and on `.disconnected` (see
    /// `ChatViewModel+EventHandling.swift`) so a failed request or a dropped
    /// connection never leaves a row stuck disabled.
    @Published var deferredScopeActionInFlightIDs: Set<String> = []
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
    /// Task ids with an in-flight `list_executions` request — tracked
    /// separately from [[executionsByTaskID]] because a Swift dictionary
    /// drops a key entirely on `= nil`, so the dictionary alone can't tell
    /// "still loading" apart from "a `workError` arrived while loading".
    /// Cleared on success ([[executionsByTaskID]] gets its rows) or failure
    /// (see [[executionsLoadFailureByTaskID]]).
    var executionsInFlightTaskIDs: Set<String> = []
    /// Failure reason keyed by task id, set when a `workError` arrives
    /// while that task's `list_executions` request was in flight. Cleared
    /// at the start of the next [[loadExecutions(taskId:)]] call. The
    /// transcript viewer renders this instead of spinning forever — see
    /// [[TranscriptViewerView.executionList]].
    @Published var executionsLoadFailureByTaskID: [String: String] = [:]
    /// Screenshot-evidence rows keyed by task id. Populated on demand when
    /// the attachment viewer window sends `list_attachments_for_work_item`.
    /// Cleared per-task before each fresh fetch, mirroring
    /// [[executionsByTaskID]].
    @Published var attachmentsByTaskID: [String: [AttachmentVM]] = [:]
    /// Task ids with an in-flight `list_attachments_for_work_item` request.
    /// Mirrors [[executionsInFlightTaskIDs]] for the same reason.
    var attachmentsInFlightTaskIDs: Set<String> = []
    /// Failure reason keyed by task id, set when a `workError` arrives
    /// while that task's attachment request was in flight. Mirrors
    /// [[executionsLoadFailureByTaskID]] — see
    /// [[AttachmentViewerView.attachmentList]].
    @Published var attachmentsLoadFailureByTaskID: [String: String] = [:]
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
        didSet {
            notePublishedWorkInputChanged()
            // Every path that changes the chooser funnels through this
            // property — explicit selection, card reveal, the archived-product
            // fallback — so reporting here is what keeps the engine's copy
            // from drifting, rather than having each call site remember.
            if oldValue != selectedWorkProductID {
                reportSelectedProductToEngine()
            }
        }
    }
    @Published var selectedProjectFilterIDs: Set<String> = [] {
        didSet { notePublishedWorkInputChanged() }
    }
    /// When true, the board shows all project-less work items (chores,
    /// investigation tasks, etc.) and their revisions. Mutually exclusive
    /// with `selectedProjectFilterIDs`.
    @Published var filterToChoresOnly: Bool = false {
        didSet { notePublishedWorkInputChanged() }
    }
    @Published var includeChores: Bool = true {
        didSet { notePublishedWorkInputChanged() }
    }
    @Published var showBlockedOnly: Bool = false {
        didSet { notePublishedWorkInputChanged() }
    }
    @Published var showArchivedProjects: Bool = false {
        didSet { notePublishedWorkInputChanged() }
    }
    /// When true, the Review column shows only `readyForReview` cards —
    /// waiting on the operator and nothing else: no block, no in-progress
    /// revision, CI green, no merge conflict. Sticky across app restarts
    /// (persisted like the other board filters below), scoped to the
    /// Review column only.
    @Published var reviewReadyOnly: Bool = false {
        didSet { notePublishedWorkInputChanged() }
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
    /// Revision-hover state lives outside this broad `ObservableObject`.
    /// Each mounted card observes one keyed cell from the store, so hovering
    /// an "In revision" badge does not invalidate every board section.
    let revisionHighlightStore = WorkBoardRevisionHighlightStore()
    /// Last parent id delivered to `setRevisionBadgeHover`. A repeated enter
    /// for the same parent is a no-op string compare before any revision scan.
    var lastRevisionHoverParentID: String?

    /// Read-only aggregate used by tests; views observe keyed cells and must
    /// not read this. Intentionally not `@Published`.
    var revisionHighlightIDs: Set<String> {
        revisionHighlightStore.highlightedIDs
    }
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
        didSet { notePublishedWorkInputChanged() }
    }
    @Published var selectedWorkNodeID: WorkNodeID?
    @Published var pendingWorkCreateRequest: WorkCreateRequest?
    @Published var pendingWorkEditRequest: WorkEditRequest?
    @Published var workErrorMessage: String?
    /// Current state of an in-flight `evaluate_editorial_rules` RPC.
    @Published var editorialEvaluationState: EditorialEvaluationState = .idle
    @Published var workSearchText: String = "" {
        didSet { notePublishedWorkInputChanged() }
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
    /// `get_engine_health` at session start and re-broadcast when
    /// pause/resume changes land. Empty means the engine is healthy.
    /// Banner kinds drive `EngineHealthBanner`; `automation_paused`
    /// is handled by the toolbar toggle instead. Settings still lists
    /// the full snapshot so a missing `ANTHROPIC_API_KEY` (or any later
    /// "missing config" surface) is impossible to miss (#699).
    @Published var engineHealthIssues: [EngineHealthIssue] = []

    /// Top-level mirror of the same `get_engine_health` reply. Surfaced
    /// in the Settings pane next to the (future) API-key field so
    /// "key set" / "key missing" is legible without parsing the
    /// `issues` list. `true` until the engine answers at least once,
    /// so the banner doesn't flash on a transient reconnect.
    @Published var engineAnthropicApiKeyPresent: Bool = true

    /// Current driver traffic split: how eligible, `standard`-reasoning
    /// implementation work is allocated between the `grok`, `claude`, and
    /// `codex` drivers. Sourced from `driver_traffic_split_result`, fetched
    /// on Settings-pane appear and confirmed after every
    /// `setDriverTrafficShare` call.
    @Published var driverTrafficSplit: DriverTrafficSplit = .engineDefault

    /// Per-driver provider quota usage, sourced from
    /// `driver_quota_usage_result`. Starts empty (`neverChecked`), which the
    /// Engine settings pane renders as "not checked yet" — deliberately not
    /// as three zeroes, which would read as full headroom before the engine
    /// has said anything at all.
    @Published var driverQuota: DriverQuotaSnapshot = .empty

    /// Whether a quota refresh request is outstanding. Drives the Refresh
    /// button's disabled state only; the pane itself never blocks on a
    /// fetch, and Settings opens regardless of whether one is in flight.
    @Published var isRefreshingDriverQuota: Bool = false

    /// Whether a Trunk org API token is currently configured (env override
    /// or Keychain), sourced from `trunk_status` — on Settings-pane appear,
    /// after a `TrunkSetToken` save, and on product-settings appear so the
    /// merge-mechanism control can warn when Trunk queue has no token.
    /// `nil` until the engine has answered at least once this session.
    @Published var trunkTokenConfigured: Bool?
    /// `"env"` / `"keychain"` when `trunkTokenConfigured` is `true`, mirrors
    /// `TrunkStatus.source`.
    @Published var trunkTokenSource: String?
    /// Live `getQueue` smoke-check outcome, mirrors `TrunkStatus.queueCheck`.
    /// `nil` when no token is configured, or when there is no
    /// `trunk_queue`-mechanism product yet to probe — see `trunkTokenNote`.
    @Published var trunkTokenQueueCheck: TrunkQueueCheck?
    /// Explains why `trunkTokenQueueCheck` is `nil`, mirrors
    /// `TrunkStatus.note`.
    @Published var trunkTokenNote: String?

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
    struct DesignDocResolveBatch {
        var startDate: Date
        var pendingProjectIDs: Set<String>
        let initialCount: Int
    }
    var currentDesignDocResolveBatch: DesignDocResolveBatch?

    /// Engine-tab attempt list, freshest first. Refreshed on Engine-tab
    /// entry, on `conflict_resolution_*` topic pushes, and on `Refresh`
    /// button taps. Phase 5 #14 of the merge-conflict design.
    @Published var conflictResolutions: [WorkConflictResolution] = [] {
        didSet { notePublishedWorkInputChanged() }
    }

    /// Engine-tab CI-remediation attempt list, freshest first.
    /// Mirror of [[conflictResolutions]]; refreshed on Engine-tab
    /// entry, on `ci_remediation_*` topic pushes, and on `Refresh`
    /// button taps. Phase 11 #37 of the merge-conflict design.
    @Published var ciRemediations: [WorkCiRemediation] = [] {
        didSet { notePublishedWorkInputChanged() }
    }

    /// The authoritative, merged attempt feed for Activity. Unlike the
    /// source-specific arrays above, these entries deliberately contain only
    /// shared list fields; a selected row loads its detailed record on demand.
    @Published var engineAttempts: [EngineAttemptListEntry] = []

    /// Engine-owned snapshot for the background-work toolbar affordance.
    /// Replaced atomically from `ListEngineAttempts` responses; the app
    /// must not filter or re-count by kind. Cleared on disconnect.
    @Published var backgroundWork: [BackgroundWorkItem] = []

    /// Canonical five-second cadence for the connection-scoped background
    /// snapshot poll. Tests shorten the instance property.
    nonisolated static let backgroundWorkPollInterval: TimeInterval = 5

    /// Polling interval used by `startBackgroundWorkPolling()`. Defaults
    /// to ``ChatViewModel.backgroundWorkPollInterval``; tests assign a shorter value.
    var backgroundWorkPollInterval: TimeInterval = ChatViewModel.backgroundWorkPollInterval

    /// Cancellable connection-scoped poller. Non-nil while connected.
    var backgroundWorkPollTask: Task<Void, Never>?

    /// Monotonic generation for in-flight snapshot requests so a late
    /// `limit = 0` poll cannot overwrite a newer event-triggered refresh.
    var backgroundWorkSendGeneration: UInt64 = 0
    var backgroundWorkAppliedGeneration: UInt64 = 0
    /// Independent of `backgroundWorkAppliedGeneration` so a history
    /// refresh that loses the snapshot race can still replace Activity.
    var attemptsAppliedGeneration: UInt64 = 0
    var backgroundWorkPending: [String: BackgroundWorkPendingRequest] = [:]

    /// Source-specific records requested by selected Activity rows, keyed by
    /// their shared-list attempt id.
    @Published var engineAttemptDetails: [String: EngineAttemptRow] = [:]

    /// Detail-fetch errors keyed by the selected shared-list attempt id.
    @Published var engineAttemptDetailErrors: [String: String] = [:]

    /// The source-specific detail request currently awaiting an engine reply.
    var engineAttemptDetailRequestID: String?

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

    /// Indirection for opening the `"async-markdown-viewer"` Window
    /// immediately, before the design doc has been fetched. Installed by
    /// [[ContentView]] via `@Environment(\.openWindow)`. When set, the
    /// raw-content path opens the window first (loading state) then
    /// resolves content into [[asyncMarkdownViewerVM]]. `nil` (tests and
    /// headless) falls back to [[openDesignDocFallback]].
    var asyncMarkdownViewerOpener: (() -> Void)?

    /// Shared state for the `"async-markdown-viewer"` Window scene.
    /// The window observes this object to transition from loading →
    /// loaded/failed without needing to pass content through the
    /// `openWindow` value type.
    let asyncMarkdownViewerVM = AsyncMarkdownViewerViewModel()

    /// In-flight engine fetch for the async markdown viewer (project /
    /// investigation doc icon). `applyProductDesignDocContent` updates
    /// the window only when the reply's triple matches this.
    var pendingAsyncViewerRef: DesignDocRef?
    var pendingAsyncViewerTitle: String = ""
    var pendingAsyncViewerArtifact: CommentArtifactRef?

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

    let engine: EngineClient
    /// Routes engine comment RPC replies + `comments.artifact.*` invalidations
    /// to the open [`CommentLayer`]s. Injected into the markdown
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
    let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    /// Becomes `true` the first time the socket reaches `.ready`, so the
    /// Disconnected banner stays hidden during the initial-connect window.
    @Published private(set) var hasConnectedOnce = false
    @Published var showConnectionLostBanner = false // see ChatViewModel+Connection.swift
    static let connectionLostBannerDelay: TimeInterval = 2.0 // grace period before a disconnect may raise the banner
    var connectionGeneration = 0 // bumped on connect/disconnect; supersedes a stale banner-reveal
    var subscribedWorkTopics: Set<String> = []
    let defaults = BossDefaults.store

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

    let navigationModeDefaultsKey = "boss.navigationMode"
    let selectedWorkProductDefaultsKey = "boss.work.selectedProductID"
    let selectedProjectFilterIDsDefaultsKey = "boss.work.projectFilterIDs"
    let filterToChoresOnlyDefaultsKey = "boss.work.filterToChoresOnly"
    let includeChoresDefaultsKey = "boss.work.includeChores"
    let showBlockedOnlyDefaultsKey = "boss.work.showBlockedOnly"
    let showArchivedProjectsDefaultsKey = "boss.work.showArchivedProjects"
    let reviewReadyOnlyDefaultsKey = "boss.work.reviewReadyOnly"
    let workBoardGroupingDefaultsKey = "boss.work.grouping"
    let bossPanelCollapsedDefaultsKey = "boss.work.bossPanelCollapsed"
    let bossPanelWidthDefaultsKey = "boss.work.bossPanelWidth"

    init(paths: BossEnginePaths) {
        self.paths = paths
        self.socketPath = paths.socketPath
        self.processController = EngineProcessController(paths: paths)
        let showSystem = ProcessInfo.processInfo.environment["BOSS_SHOW_SYSTEM_MESSAGES"] ?? ""
        showSystemMessages = showSystem == "1" || showSystem.lowercased() == "true"
        engine = EngineClient(socketPaths: paths.socketPaths)
        commentBridge = CommentEngineBridge(engine: engine)
        // Reuse the app's existing long-lived connection as the supervision
        // liveness signal instead of opening a fresh probe socket to the
        // engine every poll tick (see EngineProcessController.livenessProbe).
        processController.livenessProbe = { [weak engine] in engine?.isReachable ?? false }

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
        reviewReadyOnly = defaults.bool(forKey: reviewReadyOnlyDefaultsKey)
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
        processController.onSupervisionStateChange = { [weak self] state in
            guard let self, self.engineSupervisionState != state else { return }
            self.engineSupervisionState = state
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

        // UI tests drive explicit fixture clients and events directly. The
        // `commonInit` fallback schedules this method automatically, so do not
        // start either a real process or a reconnecting client from XCTest.
        // Controller startup has dedicated injected tests.
        guard !BossEnginePaths.isRunningInTestContext else { return }

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
    /// App-managed engine recovery state. This is distinct from the socket
    /// connection state: the latter says whether a listener is reachable,
    /// while this says whether the app is actively replacing a dead listener
    /// or has exhausted its bounded retry policy.
    @Published private(set) var engineSupervisionState: EngineSupervisionState = .running

    /// User-initiated recovery from the unreachable banner. Discovers the
    /// reachable engine by socket (token-auth shutdown RPC first, then a
    /// validated peer/pid-file SIGTERM/SIGKILL fallback) and
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

    // MARK: - Pane bridge

    /// Handlers ContentView installs so the engine can drive libghostty panes
    /// through this model. The `engine_request` arms that call them live in
    /// [[ChatViewModel+EventHandling.swift]]; a build without GhosttyKit
    /// leaves them `nil` and those arms answer with a failure.
    var paneSpawnHandler: ((EngineSpawnRequest) -> EngineSpawnResult)?
    var paneReleaseHandler: ((Int, UInt32) -> EngineReleaseResult)?
    var paneAttachHandler: ((EngineAttachRequest) -> EngineAttachResult)?
    var coordinatorPaneAttachHandler: ((EngineCoordinatorAttachRequest) -> EngineCoordinatorAttachResult)?
    var paneDetachHandler: ((Int) -> EngineReleaseResult)?
    var paneSendHandler: ((Int, String, String) -> EngineSendResult)?
    var paneFocusHandler: ((Int) -> EngineFocusResult)?
    var paneInterruptHandler: ((Int) -> EngineInterruptResult)?
    /// Enumerates every slot the app currently hosts a session in,
    /// regardless of whether the engine has a live-tracked run for it.
    /// Backs `bossctl agents list --all`. `nil` build (Bazel without
    /// GhosttyKit) replies with an empty list — there is no pane
    /// allocator to enumerate.
    var paneListHostedHandler: (() -> [EngineHostedPaneEntry])?
    /// Invoked when the engine pushes `engine_pool_config`: forwards pool sizes to
    /// `WorkersWorkspaceModel` and records the engine's desired coordinator model.
    /// Parameters: workerSlots, automationSlots, reviewSlots, coordinatorModel.
    var panePoolConfigHandler: ((Int, Int, Int, String) -> Void)?

    /// The model the engine would use for the next coordinator creation.
    /// Paired with `attachedCoordinatorModel` to make a replacement explicitly
    /// destructive instead of silently restarting a live conversation.
    var requestedCoordinatorModel: String?
    var attachedCoordinatorModel: String?
    var attachedCoordinatorSpawnToken: String?
    var declinedCoordinatorRecreateToken: String?
    @Published var coordinatorModelRecreateConfirmation: CoordinatorModelRecreateConfirmation?

    /// The installed `claude` version, set only while the engine reports it
    /// as newer than what the running coordinator session actually launched
    /// with. Drives `CoordinatorUpdateBanner`; `nil` renders nothing. Clears
    /// itself the moment a reset makes the versions match again — there is
    /// no separate dismiss, since a wrong "up to date" reading is worse than
    /// the banner persisting until the operator acts.
    @Published var coordinatorUpdateAvailable: String?

    /// Whether the engine has confirmed this client is the registered app session.
    /// Reset on disconnect (see [[ChatViewModel+EventHandling.swift]]); set when
    /// `appSessionRegistered` is received.
    var isAppSessionRegistered = false

    private func startEngineIfNeeded() {
        guard !didStartEngine else { return }
        didStartEngine = true
        engine.start()
    }

    /// Records a successful (re)connect. Exists so `hasConnectedOnce` can stay
    /// `private(set)` — `private` is file-scoped in Swift, and the `.connected`
    /// arm that flips it lives in [[ChatViewModel+EventHandling.swift]].
    func markConnected() {
        isConnected = true
        hasConnectedOnce = true
    }

    /// Cached output of `visibleWorkItems`. Filled lazily on read; reset to
    /// `nil` whenever a published input changes (see `invalidateWorkCache`).
    /// Keeps engine pushes that don't touch the work tree (e.g.
    /// `worker.live_states`) from re-walking the projects/tasks/chores trees.
    var cachedVisibleItems: [WorkTask]?
    var cachedItemsByColumn: [WorkBoardColumnKey: [WorkTask]] = [:]
    var cachedSectionsByColumn: [WorkBoardColumnKey: [WorkBoardSection]] = [:]
    var cachedAmbiguousRepoNames: Set<String>?
    /// O(1) id → work-item index over every task/chore/revision bucket.
    /// Built lazily on first lookup after any change (see `rebuildTaskIndex`);
    /// replaces a linear scan of all four buckets per `task(withID:)` call.
    /// Full invalidation drops this; a same-bucket single-item update patches
    /// the entry in place via `patchTaskIndex(with:)` instead.
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
    /// per visible card on every render. Any broad `@Published` change on
    /// this view model re-renders the whole board, so without this cache
    /// each such change re-runs an O(total tasks) scan once per card — a
    /// measured main-thread hot leaf during hover-while-scroll jank.
    /// Rebuilt lazily, same pattern as `cachedGatingPrereqs`.
    var cachedInReviewRevisionsByParentID: [String: [WorkTask]]?
    var cachedDoneRevisionsByParentID: [String: [WorkTask]]?
    /// Backing storage for `workBoardRepoMode` (ChatViewModel+BoardHelpers).
    /// `nil` means "invalidated — rebuild on next read". `WorkBoardRepoMode.compute`
    /// scans every visible card and used to re-run on each kanban render/scroll
    /// frame as a top-of-stack main-thread leaf; same pattern as
    /// `cachedInReviewRevisionsByParentID`.
    var cachedWorkBoardRepoMode: WorkBoardRepoMode?

    /// When true, published work-input `didSet` observers skip the blanket
    /// `invalidateWorkCache()` so a caller can mutate buckets and then apply
    /// keyed invalidation itself (see `applyIncrementalTaskUpdate`).
    var suppressWorkCacheInvalidation = false

    /// Which derived work caches to drop. A single-item update only clears
    /// the subsets that item can affect; bulk / filter / edge changes still
    /// use `.all`.
    struct WorkCacheInvalidation: OptionSet {
        let rawValue: Int

        /// `cachedVisibleItems`, per-column items/sections, ambiguous-repo
        /// names, and `workBoardRepoMode`.
        static let boardLayout = WorkCacheInvalidation(rawValue: 1 << 0)
        /// O(1) id → task index (`taskIndexByID`).
        static let taskIndex = WorkCacheInvalidation(rawValue: 1 << 1)
        /// Dependency / gating prereq graphs.
        static let dependencies = WorkCacheInvalidation(rawValue: 1 << 2)
        /// In-review / done revision rollup caches.
        static let revisions = WorkCacheInvalidation(rawValue: 1 << 3)

        static let all: WorkCacheInvalidation = [
            .boardLayout, .taskIndex, .dependencies, .revisions,
        ]
    }

    /// Default entry point for published work-input changes: drop every
    /// derived cache. Prefer `invalidateWorkCache(_:)` from paths that know
    /// which subsets are affected.
    func invalidateWorkCache() {
        invalidateWorkCache(.all)
    }

    /// Drop only the selected derived caches. Used by keyed single-item
    /// updates so a status flip does not rebuild the dependency graph or
    /// the full id index when bucket membership is unchanged.
    func invalidateWorkCache(_ keys: WorkCacheInvalidation) {
        if keys.contains(.boardLayout) {
            cachedVisibleItems = nil
            cachedItemsByColumn.removeAll(keepingCapacity: true)
            cachedSectionsByColumn.removeAll(keepingCapacity: true)
            cachedAmbiguousRepoNames = nil
            cachedWorkBoardRepoMode = nil
        }
        if keys.contains(.taskIndex) {
            taskIndexByID = nil
        }
        if keys.contains(.dependencies) {
            cachedDependencyPrereqs = nil
            cachedGatingPrereqs = nil
        }
        if keys.contains(.revisions) {
            cachedInReviewRevisionsByParentID = nil
            cachedDoneRevisionsByParentID = nil
        }
    }

    /// Patch one entry of the live id index without dropping it. No-op when
    /// the index has not been built yet (lazy rebuild will pick up the new
    /// row). Call only when bucket membership is unchanged.
    func patchTaskIndex(with task: WorkTask) {
        guard taskIndexByID != nil else { return }
        taskIndexByID?[task.id] = task
    }

    /// Published work-input `didSet` hook. Full invalidate unless a caller
    /// is mid-keyed mutation under `suppressWorkCacheInvalidation`.
    func notePublishedWorkInputChanged() {
        guard !suppressWorkCacheInvalidation else { return }
        invalidateWorkCache()
    }

    /// Run `body` without the bucket `didSet` observers firing a full cache
    /// drop, so the caller can finish with keyed invalidation instead.
    func withSuppressedWorkCacheInvalidation(_ body: () -> Void) {
        let wasSuppressed = suppressWorkCacheInvalidation
        suppressWorkCacheInvalidation = true
        defer { suppressWorkCacheInvalidation = wasSuppressed }
        body()
    }

    /// Inline drag-refusal banner shown next to the source card when a
    /// drag from Blocked → Doing is rejected because the row still has
    /// unsatisfied gating prereqs (design item 11). Single-slot — the
    /// previous notice is replaced when a new refusal fires.
    @Published var dragRefusalNotice: DragRefusalNotice?

    /// A drag-to-Doing whose `EvaluateDispatchAdmission` reply is still
    /// in flight. Single-slot, like `dragRefusalNotice` — a second drag
    /// landing before the first's reply arrives simply supersedes it; the
    /// stale reply is dropped by `taskID` mismatch in the event handler.
    var pendingDragAdmissionCheck: PendingDragAdmissionCheck?

    /// Set once `EvaluateDispatchAdmission` reports an active, overridable
    /// pause and no other blocker — the confirmation dialog binds to this.
    /// `nil` means no confirmation is showing.
    @Published var pendingPauseOverrideConfirmation: PauseOverrideConfirmation?

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
}
