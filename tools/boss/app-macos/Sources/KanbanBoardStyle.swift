import SwiftUI

/// Visual style for the kanban board. Persisted in UserDefaults and
/// switchable from View > Board Style in the menu bar.
///
/// The product default for users with no stored preference is
/// ``elevated``. The raw value for the former "Classic" style remains
/// `"classic"` so existing UserDefaults entries keep resolving; only
/// the user-facing label is ``Legacy``.
///
/// Four distinct takes on reducing "too many vertical lines":
///   - classic:  original appearance (column borders + card borders);
///               shown in the picker as "Legacy"
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

    /// Style used when `boss.kanban.boardStyle` is absent from
    /// UserDefaults (new installs and users who never opened the picker).
    /// Distinct from a stored `"classic"` value, which must keep resolving
    /// to ``classic`` / Legacy.
    static let productDefault: KanbanBoardStyle = .elevated

    /// UserDefaults key shared by `@AppStorage` call sites and any direct
    /// readers of the board-style preference.
    static let storageKey = "boss.kanban.boardStyle"

    var id: String { rawValue }

    var displayName: String {
        switch self {
        case .classic: return "Legacy"
        case .airy: return "Airy"
        case .elevated: return "Elevated"
        case .minimal: return "Minimal"
        }
    }

    /// Resolve the preference from a defaults store without rewriting it.
    /// Missing or unrecognized values yield ``productDefault``; a stored
    /// `"classic"` remains ``classic`` (Legacy).
    static func resolved(from defaults: UserDefaults) -> KanbanBoardStyle {
        guard let raw = defaults.string(forKey: storageKey),
              let style = KanbanBoardStyle(rawValue: raw)
        else {
            return productDefault
        }
        return style
    }
}

private struct KanbanBoardStyleKey: EnvironmentKey {
    static let defaultValue = KanbanBoardStyle.productDefault
}

extension EnvironmentValues {
    var kanbanBoardStyle: KanbanBoardStyle {
        get { self[KanbanBoardStyleKey.self] }
        set { self[KanbanBoardStyleKey.self] = newValue }
    }
}
