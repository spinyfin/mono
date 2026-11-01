import Foundation

/// The single definition of a worker kind: each case is one verb a pane
/// badge can show, and the Agents view Type filter lists exactly
/// [`allCases`] under the same [`badgeLabel`]. `unknown` stands for any
/// missing or unrecognised wire value; it is filterable and never dropped.
///
/// `guide` and `consolidation` are never wire `agent_type` values: they are
/// refinements of `review` resolved by [`resolve`] from the execution kind
/// and review stage, so a filter selects exactly the panes showing the verb.
public enum AgentType: String, CaseIterable, Hashable, Sendable {
    case coding
    case design
    case review
    case guide
    case consolidation
    case automation
    case answer
    case unknown

    /// Map a wire `agent_type` to a base kind. Missing and unrecognised
    /// values become `unknown` so they stay visible and filterable.
    public static func fromWire(_ value: String?) -> AgentType {
        guard let value, !value.isEmpty else { return .unknown }
        switch value {
        case "coding": return .coding
        case "design": return .design
        case "review": return .review
        case "automation": return .automation
        case "answer": return .answer
        default: return .unknown
        }
    }

    /// The kind a pane displays: the wire `agent_type` refined by the
    /// engine-reported execution kind and review stage. Review-guide
    /// generation is `guide`; the consolidating agent of an AI review is
    /// `consolidation` rather than one more reviewer.
    public static func resolve(wire: String?, reviewStage: String? = nil, executionKind: String? = nil) -> AgentType {
        let base = fromWire(wire)
        guard base == .review else { return base }
        if executionKind == AgentExecutionKind.prReviewGuide { return .guide }
        if reviewStage == AgentReviewStage.consolidation { return .consolidation }
        return base
    }

    /// Title-case badge verb, also the Type filter label. `unknown` stays
    /// "Unknown" even when the raw wire value is shown beside it.
    public var badgeLabel: String {
        switch self {
        case .coding: "Coding"
        case .design: "Design"
        case .review: "Reviewing"
        case .guide: "Guiding"
        case .consolidation: "Consolidating"
        case .automation: "Automation"
        case .answer: "Answer"
        case .unknown: "Unknown"
        }
    }

    /// Type filter menu labels, in menu order.
    public static var filterLabels: [String] { allCases.map(\.badgeLabel) }
}

/// Wire values of the engine's execution `kind` that refine a badge.
public enum AgentExecutionKind {
    public static let prReviewGuide = "pr_review_guide"
}

/// Wire values of the engine's `review_stage`. The engine owns the state; the
/// app only maps it to text.
public enum AgentReviewStage {
    public static let consolidation = "consolidation"
    public static let applying = "applying"
}

/// Project attribution. Work with no project is `unfiled`; it is never
/// inferred from the pool a worker happens to occupy.
public enum AgentProject: Hashable, Sendable {
    case project(id: String)
    case unfiled
}

/// One engine-reported local worker, as far as layout is concerned.
///
/// Identity is the run id. Layout inputs are deliberately limited to what
/// ordering, filtering, and counting need: the model never sees slot, persona,
/// or screen position. `hasViewer` is carried through to the cells so the view
/// can draw a terminal or a viewer-missing card without a second join,
/// but it has no effect on layout: a worker without a viewer occupies a cell,
/// consumes capacity, and is filtered and counted like any other member.
public struct PaneMember: Equatable, Sendable, Identifiable {
    public var runId: String
    /// Execution start time; the primary sort key, oldest first.
    public var startedAt: Date
    public var project: AgentProject
    public var type: AgentType
    /// The worker is waiting for input. Feeds waiting counts and selector dots.
    public var isWaiting: Bool
    /// The worker declared a bounded wait on a job; the owner is not needed.
    public var isWaitingOnJob: Bool
    public var hasViewer: Bool

    public var id: String { runId }

    public init(
        runId: String,
        startedAt: Date,
        project: AgentProject = .unfiled,
        type: AgentType = .coding,
        isWaiting: Bool = false,
        isWaitingOnJob: Bool = false,
        hasViewer: Bool = true
    ) {
        self.runId = runId
        self.startedAt = startedAt
        self.project = project
        self.type = type
        self.isWaiting = isWaiting
        self.isWaitingOnJob = isWaitingOnJob
        self.hasViewer = hasViewer
    }
}

/// Result of an explicit request to bring a run into view.
public enum PaneRevealOutcome: Equatable, Sendable {
    /// The run is not an engine-reported local member.
    case notMember
    /// The run was already admitted by the filter; its page is now selected.
    case revealed
    /// A filter was hiding the run; it was cleared and the page selected.
    case revealedClearingFilter
}

/// Project and type multi-selects. `nil` means "all"; a non-nil set admits only
/// its members, so an empty set admits nothing. The UI should map "nothing
/// checked" back to `nil`.
public struct PaneFilter: Equatable, Sendable {
    public static let all = PaneFilter()

    public var projects: Set<AgentProject>?
    public var types: Set<AgentType>?

    public init(projects: Set<AgentProject>? = nil, types: Set<AgentType>? = nil) {
        self.projects = projects
        self.types = types
    }

    public var isActive: Bool { projects != nil || types != nil }

    /// Checks or unchecks one project. Unchecking the last one returns the
    /// dimension to "all".
    public mutating func toggle(project: AgentProject) {
        var set = projects ?? []
        if set.contains(project) { set.remove(project) } else { set.insert(project) }
        projects = set.isEmpty ? nil : set
    }

    /// Checks or unchecks one type. Unchecking the last one returns the
    /// dimension to "all".
    public mutating func toggle(type: AgentType) {
        var set = types ?? []
        if set.contains(type) { set.remove(type) } else { set.insert(type) }
        types = set.isEmpty ? nil : set
    }

    public func admits(_ member: PaneMember) -> Bool {
        if let projects, !projects.contains(member.project) { return false }
        if let types, !types.contains(member.type) { return false }
        return true
    }
}
