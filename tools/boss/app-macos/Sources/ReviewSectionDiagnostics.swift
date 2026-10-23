import Foundation
import os.log

/// Where the board reports a Review-column card that arrived without an
/// engine section when one was owed.
///
/// The engine owns Review-column classification (`Task.review_section`) and
/// a section-aware engine sends it for every Review-lane row, on the work
/// tree and on every `work_item_updated` reply. The board files a sectionless
/// card under Pending so it is never hidden — but that fallback is only
/// legitimate in the two cases `ChatViewModel.missingReviewSectionIsExpected`
/// recognises. Any other sectionless Review card means an engine update path
/// dropped the projection, and silently showing it as Pending is exactly how
/// a Ready card ends up misfiled for hours. So it is reported here instead:
/// a fault-level log in the app, and — through `onMissingSection`, which the
/// test bundle's principal class binds to `XCTFail` — a failure of whichever
/// test produced it.
enum ReviewSectionDiagnostics {
    private static let log = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "review-section")

    /// Observer for reported defects. `nil` in the app, where the fault log
    /// is the report; the BossTests principal class installs one that fails
    /// the running test, so no code path can drop a Review card's section
    /// and still pass the suite. Tests that deliberately feed a sectionless
    /// card swap in their own observer for the duration and restore this.
    nonisolated(unsafe) static var onMissingSection: ((WorkTask) -> Void)?

    static func reportMissingSection(_ task: WorkTask) {
        log.fault(
            """
            Review card \(task.id, privacy: .public) "\(task.name, privacy: .public)" arrived without \
            review_section from a section-aware engine; showing it under Pending. \
            status=\(task.status, privacy: .public) \
            blocked_reason=\(task.blockedReason ?? "nil", privacy: .public) \
            ci_required_state=\(task.ciRequiredState ?? "nil", privacy: .public) \
            pr_mergeable_state=\(task.prMergeableState ?? "nil", privacy: .public) \
            has_in_progress_revision=\(task.hasInProgressRevision) \
            ready_for_review=\(task.readyForReview). \
            The engine owns this classification: an update path dropped it.
            """
        )
        onMissingSection?(task)
    }
}
