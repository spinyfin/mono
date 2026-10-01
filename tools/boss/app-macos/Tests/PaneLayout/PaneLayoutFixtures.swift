import Foundation
@testable import PaneLayout

/// Shared builders. The default laptop geometry yields a 4x2 limit, so
/// capacity 8, matching the design doc's worked examples.
enum Fixture {
    /// 6pt x 13pt cells in a 1700 x 720pt area: 4 columns x 2 rows at
    /// `PaneCapacityPolicy.standard`.
    static let laptop = PaneGeometry(
        area: PaneSize(width: 1700, height: 720),
        cell: PaneSize(width: 6, height: 13)
    )

    static let epoch = Date(timeIntervalSince1970: 1_000_000)

    /// `run-NN`, started `n` seconds after `epoch`, so numeric order is sort order.
    static func runId(_ n: Int) -> String {
        "run-" + (n < 10 ? "0\(n)" : "\(n)")
    }

    static func member(
        _ n: Int,
        project: AgentProject = .unfiled,
        type: AgentType = .coding,
        waiting: Bool = false,
        viewer: Bool = true
    ) -> PaneMember {
        PaneMember(
            runId: runId(n),
            startedAt: epoch.addingTimeInterval(Double(n)),
            project: project,
            type: type,
            isWaiting: waiting,
            hasViewer: viewer
        )
    }

    static func members(_ range: ClosedRange<Int>) -> [PaneMember] {
        range.map { member($0) }
    }

    static func model(geometry: PaneGeometry = laptop, visible: Bool = true) -> PaneLayoutModel {
        PaneLayoutModel(geometry: geometry, isVisible: visible)
    }

    /// One token per rendered rectangle: the run id, `~run-NN` for a
    /// placeholder, `.` for an empty rectangle.
    static func tokens(_ page: PaneLayoutPage) -> [String] {
        page.cells.map { cell in
            switch cell.content {
            case .empty: return "."
            case .occupied(let member): return member.runId
            case .placeholder(let runId): return "~" + runId
            }
        }
    }

    /// Run ids on a page, in logical-cell order, skipping empties and placeholders.
    static func occupiedRunIds(_ page: PaneLayoutPage) -> [String] {
        tokens(page).filter { $0 != "." && !$0.hasPrefix("~") }
    }
}
