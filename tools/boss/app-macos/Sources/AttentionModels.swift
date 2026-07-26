import Foundation

// ===========================================================================
// Attentions (design: tools/boss/docs/designs/attentions.md). Swift mirrors of
// `boss_protocol::AttentionGroup` and `boss_protocol::Attention` — the
// agent-authored, human-actionable notification feature surfaced in the
// Notifications toolbar window. Distinct from `WorkAttentionItem`
// (Models+AttentionItems.swift), which is the legacy engine-raised
// *operational* alert store. Split out of Models.swift to keep that file under
// the repo's file-size check.
// ===========================================================================

/// Swift mirror of `boss_protocol::AttentionGroup`. The human-actionable
/// unit: related questions / followups collect into one group keyed by a
/// stable `grouping_key`, and actioning the group yields a single downstream
/// artifact (one revision/design task, or a batch task-create).
struct AttentionGroup: Identifiable, Codable, Hashable {
    var id: String
    var productID: String
    /// Per-product `A<n>` friendly id; `nil` until the engine assigns one.
    var shortID: Int?
    /// `"question"` | `"followup"` (extensible).
    var kind: String
    /// Exactly one of `associationProjectID` / `associationTaskID` is set.
    var associationProjectID: String?
    var associationTaskID: String?
    /// `"design_doc"` | `"task_transcript"` | `"manual"`.
    var sourceKind: String
    var sourceTaskID: String?
    var sourceRunID: String?
    var sourceDocPath: String?
    var sourceDocRepoRemoteURL: String?
    var sourceDocBranch: String?
    var groupingKey: String
    var generation: Int
    /// `"open"` | `"partially_answered"` | `"actioned"` | `"dismissed"`.
    var state: String
    /// `"revision"` | `"design_task"` | `"tasks"` — set once actioned.
    var producedArtifactKind: String?
    /// JSON ref: revision/design `{task_id, short_id}` or
    /// followup `{tasks:[{task_id, short_id, kind}]}`.
    var producedArtifactRef: String?
    var createdAt: String
    var actionedAt: String?
    var dismissedAt: String?

    enum CodingKeys: String, CodingKey {
        case id
        case productID = "product_id"
        case shortID = "short_id"
        case kind
        case associationProjectID = "association_project_id"
        case associationTaskID = "association_task_id"
        case sourceKind = "source_kind"
        case sourceTaskID = "source_task_id"
        case sourceRunID = "source_run_id"
        case sourceDocPath = "source_doc_path"
        case sourceDocRepoRemoteURL = "source_doc_repo_remote_url"
        case sourceDocBranch = "source_doc_branch"
        case groupingKey = "grouping_key"
        case generation
        case state
        case producedArtifactKind = "produced_artifact_kind"
        case producedArtifactRef = "produced_artifact_ref"
        case createdAt = "created_at"
        case actionedAt = "actioned_at"
        case dismissedAt = "dismissed_at"
    }
}

/// Swift mirror of `boss_protocol::Attention`. One member of an
/// [[AttentionGroup]] — a single question (with its type / prompt / choices /
/// answer) or a single proposed followup (with its `proposed_*` fields).
struct Attention: Identifiable, Codable, Hashable {
    var id: String
    var groupID: String
    /// Display order within the group (1-based).
    var ordinal: Int
    /// Doc heading slug (questions) or transcript offset hint (followups).
    var sourceAnchor: String?
    /// `"open"` | `"answered"` | `"skipped"` | `"dismissed"`.
    var answerState: String
    var createdAt: String
    var answeredAt: String?
    // --- question fields (populated when the group's kind is "question") ---
    /// `"yes_no"` | `"multiple_choice"` | `"prompt"`.
    var questionType: String?
    var promptText: String?
    /// JSON array of strings (`multiple_choice` only).
    var choiceOptions: String?
    /// Captured answer: `"yes"`/`"no"`, the chosen value, or free text.
    var answer: String?
    // --- followup fields (populated when the group's kind is "followup") ---
    var proposedName: String?
    var proposedDescription: String?
    var proposedEffort: String?
    var proposedWorkKind: String?
    var rationale: String?
    /// `"structured"` (manifest/sentinel) or `"extracted"` (model pass).
    var confidenceSource: String
    /// Count of independent reports folded into this item. `1` for a freshly
    /// created item; incremented each time a near-duplicate is reconciled
    /// into this canonical (design: notification-dedup-scoring.md). Drives
    /// the Notifications UI's priority badge and ordering.
    var score: Int64
    /// Id (`prp_…`) of the `worker_proposals` row this member was staged
    /// from, when it came in via `boss propose followup-task` rather than a
    /// detector/manifest. `nil` for members created any other way. Drives
    /// the Notifications-window proposal badge + provenance jump link
    /// (design: worker-proposal-api-replace-fragile-worker-to-engine-seams.md
    /// §"UI visibility and provenance").
    var sourceProposalID: String? = nil

    enum CodingKeys: String, CodingKey {
        case id
        case groupID = "group_id"
        case ordinal
        case sourceAnchor = "source_anchor"
        case answerState = "answer_state"
        case createdAt = "created_at"
        case answeredAt = "answered_at"
        case questionType = "question_type"
        case promptText = "prompt_text"
        case choiceOptions = "choice_options"
        case answer
        case proposedName = "proposed_name"
        case proposedDescription = "proposed_description"
        case proposedEffort = "proposed_effort"
        case proposedWorkKind = "proposed_work_kind"
        case rationale
        case confidenceSource = "confidence_source"
        case score
        case sourceProposalID = "source_proposal_id"
    }
}

/// Swift mirror of `boss_protocol::AttentionMerge`. One row of the
/// `attention_merges` provenance ledger — a fold event recorded when a
/// near-duplicate was reconciled into this canonical `Attention`. Fetched
/// on demand for the Notifications UI's merge-provenance affordance.
struct AttentionMerge: Identifiable, Codable, Hashable {
    var id: String
    var candidateSummary: String
    var createdAt: String
    var model: String
    var productID: String
    /// `"creation"` | `"sweep"` | `"sensibility"`.
    var trigger: String
    var canonicalAttentionID: String?
    var canonicalWorkItemID: String?
    var candidateSource: String?
    var decisionRationale: String?
    var duplicateAttentionID: String?
    /// JSON `[{field, before, after}]` when the fold edited the canonical.
    var editsApplied: String?

    enum CodingKeys: String, CodingKey {
        case id
        case candidateSummary = "candidate_summary"
        case createdAt = "created_at"
        case model
        case productID = "product_id"
        case trigger
        case canonicalAttentionID = "canonical_attention_id"
        case canonicalWorkItemID = "canonical_work_item_id"
        case candidateSource = "candidate_source"
        case decisionRationale = "decision_rationale"
        case duplicateAttentionID = "duplicate_attention_id"
        case editsApplied = "edits_applied"
    }
}

extension AttentionMerge {
    /// `true` when this fold applied a bounded edit to the canonical, vs. a
    /// plain `score += 1` with no content change.
    var editedCanonical: Bool {
        guard let editsApplied else { return false }
        return !editsApplied.isEmpty
    }
}

extension AttentionGroup {
    /// A group is actionable (shows in the Notifications open list and counts
    /// toward the toolbar badge) while it still awaits a human gesture.
    var isOpen: Bool { state == "open" || state == "partially_answered" }

    /// `true` once the group has produced its downstream artifact.
    var isActioned: Bool { state == "actioned" }

    /// Human-readable kind chip label.
    var kindLabel: String {
        switch kind {
        case "question": return "Question"
        case "followup": return "Followup"
        default: return kind.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }

    /// The produced work items recorded on `produced_artifact_ref`, decoded
    /// from either the single-task (revision / design) or the batch
    /// (followup `tasks`) shape. Empty until the group is actioned.
    var producedArtifacts: [ProducedArtifactRef] {
        guard let producedArtifactRef,
              let data = producedArtifactRef.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return [] }
        if let tasks = obj["tasks"] as? [[String: Any]] {
            return tasks.compactMap(ProducedArtifactRef.init(dict:))
        }
        if let single = ProducedArtifactRef(dict: obj) {
            return [single]
        }
        return []
    }
}

/// One produced work item referenced from an actioned group's
/// `produced_artifact_ref`. Used to render "jump to the produced revision /
/// task" links on a resolved card.
struct ProducedArtifactRef: Hashable {
    var taskID: String
    var shortID: Int?
    /// `"task"` | `"chore"` for followup-created items; `nil` for revisions.
    var kind: String?

    init?(dict: [String: Any]) {
        guard let taskID = dict["task_id"] as? String, !taskID.isEmpty else { return nil }
        self.taskID = taskID
        self.shortID = (dict["short_id"] as? NSNumber)?.intValue
        self.kind = dict["kind"] as? String
    }
}

extension Attention {
    /// Decoded `choice_options` JSON array. Empty when absent or unparseable.
    var choices: [String] {
        guard let choiceOptions,
              let data = choiceOptions.data(using: .utf8),
              let arr = try? JSONDecoder().decode([String].self, from: data)
        else { return [] }
        return arr
    }

    /// A member is resolved once it has left the `open` answer-state.
    var isResolved: Bool { answerState != "open" }
    var isAnswered: Bool { answerState == "answered" }
    var isSkipped: Bool { answerState == "skipped" }
    var isDismissed: Bool { answerState == "dismissed" }
}
