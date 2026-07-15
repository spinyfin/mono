import Foundation

enum NavigationMode: String, CaseIterable, Identifiable {
    case agents = "Agents"
    case work = "Work"
    case designs = "Designs"
    case automations = "Automations"

    var id: String { rawValue }
}
