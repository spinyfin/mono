import SwiftUI

/// Visual style for the kanban board. Persisted in UserDefaults and
/// switchable from View > Board Style in the menu bar.
///
/// Four distinct takes on reducing "too many vertical lines":
///   - classic:  current appearance (column borders + card borders)
///   - airy:     soft column panels, borderless cards with a drop shadow
///   - elevated: airy's spacing/layout, but cards use a surface color
///               clearly distinct from the column background (plus a
///               faint border) so card boundaries stay legible even when
///               the drop shadow alone doesn't read well (e.g. dark mode)
///   - minimal:  flat cards, tinted column panels, no borders anywhere
enum KanbanBoardStyle: String, CaseIterable, Identifiable {
    case classic
    case airy
    case elevated
    case minimal

    var id: String { rawValue }

    var displayName: String {
        switch self {
        case .classic: return "Classic"
        case .airy: return "Airy"
        case .elevated: return "Elevated"
        case .minimal: return "Minimal"
        }
    }
}

private struct KanbanBoardStyleKey: EnvironmentKey {
    static let defaultValue = KanbanBoardStyle.classic
}

extension EnvironmentValues {
    var kanbanBoardStyle: KanbanBoardStyle {
        get { self[KanbanBoardStyleKey.self] }
        set { self[KanbanBoardStyleKey.self] = newValue }
    }
}
