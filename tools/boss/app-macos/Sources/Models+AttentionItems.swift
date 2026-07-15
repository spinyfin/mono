import Foundation

// ===========================================================================
// Operational attention items. `WorkAttentionItem` is the engine-raised
// operational alert store — distinct from the agent-authored `Attention` /
// `AttentionGroup` notification feature in AttentionModels.swift (design:
// tools/boss/docs/designs/attentions.md). Also holds the deferred-scope join and
// the external-tracker attention presentation model. Split out of Models.swift
// to keep that file under the repo's file-size check.
// ===========================================================================

/// Swift mirror of `boss_protocol::WorkAttentionItem`. One attention-item
/// row from `work_attention_items`, attached to either an execution or a
/// work item (product / task / chore).
struct WorkAttentionItem: Identifiable, Codable, Hashable {
    var id: String
    var executionID: String?
    var workItemID: String?
    var kind: String
    var status: String
    var title: String
    var bodyMarkdown: String
    var createdAt: String
    var resolvedAt: String?
    /// Set only when this item was closed via "create task" (currently only
    /// `deferred_scope` items support that path) — the id of the followup
    /// task the conversion produced.
    var convertedTaskID: String?

    enum CodingKeys: String, CodingKey {
        case id
        case executionID = "execution_id"
        case workItemID = "work_item_id"
        case kind
        case status
        case title
        case bodyMarkdown = "body_markdown"
        case createdAt = "created_at"
        case resolvedAt = "resolved_at"
        case convertedTaskID = "converted_task_id"
    }
}

/// Swift mirror of `boss_protocol::DeferredScopeAttention` — one open
/// `deferred_scope` attention item paired with the id of the work item
/// whose execution recorded it. `WorkAttentionItem.workItemID` is always
/// `nil` for this kind (it carries only `executionID` — see
/// [`WorkAttentionItem.workItemID`]), so this join is what lets the kanban
/// place the item on the right card.
struct DeferredScopeAttention: Identifiable, Hashable {
    var item: WorkAttentionItem
    var sourceWorkItemID: String

    var id: String { item.id }
}

/// Pure-data presentation model for an external-tracker attention item.
/// Derived from a `WorkAttentionItem` whose `kind` starts with
/// `"external_tracker_"`. `forItem` returns `nil` for unrecognised kinds
/// so callers can filter to only the items they know how to render.
///
/// Four reasons are defined in the design doc (chore 16):
/// - `external_tracker_auth_failed`
/// - `external_tracker_transient_errors`
/// - `external_tracker_removed_upstream`
/// - `external_tracker_permission_denied`
struct ExternalTrackerAttentionPresentation: Equatable {
    /// Short reason code extracted from the kind, e.g. `"auth_failed"`.
    let reasonCode: String
    /// Human-readable title shown in the attention list.
    let displayTitle: String
    /// One-line summary of the remediation action.
    let remediationHint: String
    /// SF Symbol name for the attention icon.
    let iconName: String
    /// Whether the item is still open (not resolved).
    let isOpen: Bool

    /// Build a presentation from a raw attention item. Returns `nil` when
    /// the kind is not a recognised external-tracker kind.
    static func forItem(_ item: WorkAttentionItem) -> ExternalTrackerAttentionPresentation? {
        let prefix = "external_tracker_"
        guard item.kind.hasPrefix(prefix) else { return nil }
        let reasonCode = String(item.kind.dropFirst(prefix.count))
        let (displayTitle, remediationHint, iconName) = metadata(for: reasonCode, item: item)
        return ExternalTrackerAttentionPresentation(
            reasonCode: reasonCode,
            displayTitle: displayTitle,
            remediationHint: remediationHint,
            iconName: iconName,
            isOpen: item.status == "open"
        )
    }

    private static func metadata(
        for reasonCode: String,
        item: WorkAttentionItem
    ) -> (String, String, String) {
        switch reasonCode {
        case "auth_failed":
            return (
                item.title,
                "Run `gh auth login` to refresh credentials.",
                "lock.trianglebadge.exclamationmark"
            )
        case "transient_errors":
            return (
                item.title,
                "Boss will retry automatically. Check network connectivity if this persists.",
                "exclamationmark.icloud"
            )
        case "removed_upstream":
            return (
                item.title,
                "Re-bind manually with `boss chore link-external` if this was unintended.",
                "link.badge.plus"
            )
        case "permission_denied":
            return (
                item.title,
                "Run `gh auth login --scopes repo` to grant write permission.",
                "exclamationmark.shield"
            )
        default:
            return (
                item.title,
                "See the engine log for details.",
                "exclamationmark.triangle"
            )
        }
    }
}
