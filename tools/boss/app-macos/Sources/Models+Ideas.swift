import Foundation

// ===========================================================================
// Idea models. Swift mirror of `boss_protocol::Idea` — a markdown draft
// authored over time in the Ideas surface, later graduatable into a chore
// or project. Deliberately NOT a work item: does not join
// `WorkItemPayload` (Models.swift) or `WorkCreateKind` — not dispatchable,
// no execution, no PR, not on the kanban. Own table, own `I<n>` short-id
// namespace, own top-level nav mode (`NavigationMode.ideas`).
// ===========================================================================

/// Swift mirror of `boss_protocol::Idea`.
struct WorkIdea: Codable, Identifiable, Equatable {
    let id: String
    var shortID: Int?
    var productID: String
    var name: String
    var body: String
    var status: IdeaStatus
    var createdAt: String
    var updatedAt: String
    var createdVia: String
    var graduatedToID: String?

    enum CodingKeys: String, CodingKey {
        case id
        case shortID = "short_id"
        case productID = "product_id"
        case name
        case body
        case status
        case createdAt = "created_at"
        case updatedAt = "updated_at"
        case createdVia = "created_via"
        case graduatedToID = "graduated_to_id"
    }

    /// Human-facing short label (`I12`), mirrors `Idea::display_label` on
    /// the Rust side — falls back to the canonical id when no short id has
    /// been allocated.
    var shortLabel: String {
        shortID.map { "I\($0)" } ?? id
    }
}

/// Swift mirror of `boss_protocol::IdeaStatus`.
enum IdeaStatus: String, Codable, CaseIterable {
    case draft
    case graduated
    case archived

    var label: String {
        switch self {
        case .draft: return "Draft"
        case .graduated: return "Graduated"
        case .archived: return "Archived"
        }
    }
}

/// Autosave status for the idea currently open in the editor, surfaced as
/// a small inline indicator (`IdeasView`) rather than the app's blocking
/// `workErrorMessage` alert — a background autosave hiccup must never
/// interrupt drafting. See `ChatViewModel+Ideas.swift`.
enum IdeaSaveStatus: Equatable {
    /// Nothing open, or the open idea has no unconfirmed edits.
    case idle
    /// An edit was just made; the local crash-floor write and/or the
    /// engine save are debounced and not yet sent.
    case pendingLocal
    /// The debounced `update_idea` request is in flight.
    case savingToEngine
    /// The engine has confirmed the exact edit currently in the editor.
    case savedToEngine
    /// The engine is unreachable; the edit is protected by the local
    /// crash-floor cache and will be pushed once the connection returns.
    case offlineSavedLocally
}
