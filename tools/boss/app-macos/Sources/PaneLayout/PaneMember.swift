import Foundation

/// Badge category for an agent. `unknown` stands for any missing or
/// unrecognised wire value; it is filterable and never dropped.
public enum AgentType: String, CaseIterable, Hashable, Sendable {
    case coding
    case design
    case review
    case automation
    case answer
    case unknown
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
/// can draw a terminal or a "Viewer not attached" card without a second join,
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
    public var hasViewer: Bool

    public var id: String { runId }

    public init(
        runId: String,
        startedAt: Date,
        project: AgentProject = .unfiled,
        type: AgentType = .coding,
        isWaiting: Bool = false,
        hasViewer: Bool = true
    ) {
        self.runId = runId
        self.startedAt = startedAt
        self.project = project
        self.type = type
        self.isWaiting = isWaiting
        self.hasViewer = hasViewer
    }
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

    public func admits(_ member: PaneMember) -> Bool {
        if let projects, !projects.contains(member.project) { return false }
        if let types, !types.contains(member.type) { return false }
        return true
    }
}
