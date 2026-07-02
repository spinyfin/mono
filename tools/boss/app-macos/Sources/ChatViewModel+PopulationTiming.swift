import Foundation
import os

/// Population-timing hooks for the `@MainActor` apply + render segments of
/// the `GetWorkTree` task-population path (T2101 R1). Kept in an extension
/// so `ChatViewModel.swift` stays small; the request→reply and decode
/// segments live in [[EngineClient]] and [[PopulationTiming]].
extension ChatViewModel {
    /// Called at the end of `handle(.workTree)` once the buckets are
    /// rebuilt. Logs the apply burst (total + bucket-rebuild + sort
    /// sub-steps), then — for the currently-visible product only — times
    /// the render sort passes (`visibleWorkItems`, `workItems(in:)`) and
    /// the coarse post-apply → next-runloop-tick window during which
    /// SwiftUI rebuilds the lanes.
    ///
    /// Timing the render passes here (rather than inside the hot
    /// `computeVisibleWorkItems`/`workItems(in:)` getters) keeps those
    /// getters free of any always-on overhead: this eager read warms the
    /// exact caches SwiftUI reads on the next tick, so the sort work
    /// happens once — here, measured — instead of untimed at render.
    func recordPopulationApplyBurst(
        context: PopulationFetchContext,
        applyStartNanos: UInt64,
        bucketStartNanos: UInt64,
        bucketEndNanos: UInt64,
        sortEndNanos: UInt64
    ) {
        let applyEndNanos = PopulationTiming.now()
        PopulationTiming.shared.recordApply(
            context: context,
            applyStartNanos: applyStartNanos,
            bucketStartNanos: bucketStartNanos,
            bucketEndNanos: bucketEndNanos,
            sortEndNanos: sortEndNanos,
            applyEndNanos: applyEndNanos
        )
        PopulationSignpost.signposter.emitEvent(
            PopulationSignpost.Name.apply,
            "product=\(context.productId) items=\(context.items) seq=\(context.seq)"
        )

        // Render segments only make sense for the product actually on
        // screen. A background refetch (item_refetch of a non-selected
        // product) updates that product's buckets — the apply cost above
        // is real — but triggers no lane rebuild, so there is nothing to
        // time for render.
        guard context.productId == currentSelectedProductID else { return }

        // Sort passes SwiftUI would run on the next tick. Warming them now
        // fills the caches and lets us attribute their cost per column.
        let cvStart = PopulationTiming.now()
        _ = visibleWorkItems
        let cvEnd = PopulationTiming.now()
        PopulationTiming.shared.recordRenderSubstep(
            context: context,
            segment: PopulationSegment.renderComputeVisible,
            startNanos: cvStart,
            endNanos: cvEnd
        )
        for column in WorkBoardColumnKey.allCases {
            let colStart = PopulationTiming.now()
            _ = workItems(in: column)
            let colEnd = PopulationTiming.now()
            PopulationTiming.shared.recordRenderSubstep(
                context: context,
                segment: PopulationSegment.renderColumnBuild,
                startNanos: colStart,
                endNanos: colEnd,
                column: column.rawValue
            )
        }

        // Coarse render window: apply-end → the next main run-loop tick,
        // by which point SwiftUI has rebuilt the lanes. `os_signpost`
        // interval brackets the same window for Instruments attribution.
        let renderSignpost = PopulationSignpost.signposter.beginInterval(
            PopulationSignpost.Name.render
        )
        let renderStartNanos = applyEndNanos
        DispatchQueue.main.async {
            let renderEndNanos = PopulationTiming.now()
            PopulationSignpost.signposter.endInterval(
                PopulationSignpost.Name.render, renderSignpost
            )
            PopulationTiming.shared.recordRender(
                context: context,
                startNanos: renderStartNanos,
                endNanos: renderEndNanos
            )
        }
    }
}
