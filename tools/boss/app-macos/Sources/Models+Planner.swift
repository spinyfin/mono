import Foundation

// ===========================================================================
// Planner (design: tools/boss/docs/designs/
// auto-populate-project-tasks-on-design-pr-merge.md). Swift mirrors of
// `boss_protocol::PlannerRun` — one durable audit row per Planner
// invocation, also the per-project idempotency gate. Backs the app's
// review/release/undo surface (task 10 of that design).
// ===========================================================================

/// Swift mirror of `boss_protocol::PlannerRun`.
struct PlannerRun: Identifiable, Codable, Hashable {
    var id: String
    var projectID: String
    var productID: String
    /// The `kind = 'design'` task whose PR merge triggered this run.
    /// `nil` for some operator-initiated runs.
    var designTaskID: String?
    /// `"merge_trigger"` | `"operator"` | `"replan"`.
    var caller: String
    /// `"<repo_remote_url>|<ref>|<path>"` of the doc fetched.
    var docRef: String?
    /// Model slug used for the Planner call.
    var model: String?
    var inputSummary: String?
    /// Verbatim structured JSON returned by the model.
    var rawOutput: String?
    /// `[effort-classification] ...` lines, newline-joined.
    var effortAudit: String?
    /// Free-text rationale from the Planner.
    var notes: String?
    /// One of the `PLANNER_OUTCOME_*` constants — see `outcomeLabel`.
    var outcome: String
    var resultSummary: String?
    var createdAt: String
    var updatedAt: String

    enum CodingKeys: String, CodingKey {
        case id
        case projectID = "project_id"
        case productID = "product_id"
        case designTaskID = "design_task_id"
        case caller
        case docRef = "doc_ref"
        case model
        case inputSummary = "input_summary"
        case rawOutput = "raw_output"
        case effortAudit = "effort_audit"
        case notes
        case outcome
        case resultSummary = "result_summary"
        case createdAt = "created_at"
        case updatedAt = "updated_at"
    }
}

extension PlannerRun {
    /// `true` once this run has staged a batch awaiting operator release.
    var isStaged: Bool { outcome == "staged" }

    /// `true` once this run's batch has been released for dispatch.
    var isApplied: Bool { outcome == "applied" }

    /// Human-readable summary of the `PLANNER_OUTCOME_*` tag, matching the
    /// language the CLI and the engine's attention items use.
    var outcomeLabel: String {
        switch outcome {
        case "running": return "Planner is running…"
        case "staged": return "Staged tasks ready for review"
        case "applied": return "Released for dispatch"
        case "no_breakdown": return "No task breakdown found in the design doc"
        case "rejected_too_many": return "Rejected: too many proposed tasks"
        case "rejected_cycle": return "Rejected: proposed a cyclic dependency graph"
        case "fetch_failed": return "Failed to fetch the design doc"
        case "doc_missing": return "Design doc not found at the recorded path"
        case "planner_failed": return "Planner call failed"
        case "skipped_pre_seeded": return "Skipped — project already had tasks"
        case "skipped_already_populated": return "Skipped — already populated"
        default: return outcome
        }
    }

    /// Parsed `[effort-classification] ...` lines, one per proposed task.
    var effortAuditLines: [String] {
        guard let effortAudit else { return [] }
        return effortAudit
            .split(separator: "\n", omittingEmptySubsequences: true)
            .map(String.init)
    }

    /// Short, human-readable explanation of a `planner_failed` run, derived
    /// from the failure tag the engine writes as the `result_summary`
    /// prefix (`populator.rs`: `result_summary(format!("planner {}: {detail}",
    /// failure.tag()))`, tags from `PlannerOutcome::tag()` in `planner.rs`).
    /// `nil` for any other outcome, or when `result_summary` doesn't have
    /// the expected shape — callers fall back to the raw text alone rather
    /// than showing a blank or misleading headline.
    var plannerFailureHeadline: String? {
        guard outcome == "planner_failed", let resultSummary else { return nil }
        switch plannerFailureTag(from: resultSummary) {
        case "invalid_output":
            return "The planner returned output that did not match the expected schema."
        case "no_api_key":
            return "The planner call failed: no model API key is configured on the engine."
        case "api_error":
            return "The planner call failed: the model API returned an error."
        case "transport_error":
            return "The planner call failed: the request to the model could not complete."
        default:
            return "The planner call failed unexpectedly."
        }
    }

    private func plannerFailureTag(from resultSummary: String) -> String? {
        guard resultSummary.hasPrefix("planner ") else { return nil }
        let rest = resultSummary.dropFirst("planner ".count)
        guard let colonIndex = rest.firstIndex(of: ":") else { return nil }
        return String(rest[rest.startIndex..<colonIndex])
    }
}

extension String {
    /// Best-effort readability cleanup for diagnostic strings that carry
    /// multiple levels of backslash-escaping — e.g. a serde error whose
    /// message embeds the `Debug` form of a `Vec<String>`, itself embedding
    /// `Debug`-escaped quotes from the original values. This is display-only:
    /// it collapses `\"`, `\n`, and `\t` escape sequences repeatedly until
    /// the string stops changing (capped, so a pathological input can't loop
    /// forever), it does not attempt to parse or validate JSON.
    var unescapedForDisplay: String {
        var current = self
        for _ in 0..<6 {
            let next = current
                .replacingOccurrences(of: "\\\"", with: "\"")
                .replacingOccurrences(of: "\\n", with: "\n")
                .replacingOccurrences(of: "\\t", with: "\t")
            if next == current { break }
            current = next
        }
        return current
    }
}

/// Swift mirror of `boss_protocol::UnpopulatePreservedTask` — one task
/// `UnpopulateProject` left alone because it already had an execution
/// (released and dispatched), reported back so the operator decides.
struct UnpopulatePreservedTask: Identifiable, Codable, Hashable {
    var id: String
    var name: String
}

extension WorkTask {
    /// `true` for a task the auto-populate Planner created that is still
    /// awaiting operator release (`autostart == false`, never dispatched).
    /// Distinguishes "staged, review before it can start" from an ordinary
    /// manually-created backlog item. See design
    /// `auto-populate-project-tasks-on-design-pr-merge.md` task 10.
    var isPlannerStaged: Bool {
        createdVia == "engine_auto" && !autostart && status == "todo"
    }
}
