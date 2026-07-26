import SwiftUI

struct FlowLayout: Layout {
    /// Horizontal gap between chips on the same line.
    var horizontalSpacing: CGFloat = 6
    /// Vertical gap between wrapped lines.
    var verticalSpacing: CGFloat = 3

    /// Layout cache: per-subview sizes plus the last row layout for a given max width.
    /// `sizeThatFits` fills sizes (and rows for its proposal); `placeSubviews` reuses
    /// sizes always and rows when the width still matches — avoiding a second full
    /// pass of `subviews[i].sizeThatFits(.unspecified)` on every chip.
    struct Cache {
        var sizes: [CGSize] = []
        var rows: [Row] = []
        /// Max width the cached `rows` were computed for; `nil` means rows are invalid.
        var rowsMaxWidth: CGFloat? = nil
    }

    func makeCache(subviews: Subviews) -> Cache {
        Cache(sizes: measureSizes(subviews))
    }

    func updateCache(_ cache: inout Cache, subviews: Subviews) {
        cache.sizes = measureSizes(subviews)
        cache.rows = []
        cache.rowsMaxWidth = nil
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Cache) -> CGSize {
        ensureSizes(subviews: subviews, cache: &cache)
        let maxWidth = proposal.width ?? .infinity
        let rows = rows(for: maxWidth, cache: &cache)
        let width = rows.map(\.width).max() ?? 0
        let height = rows.reduce(0) { $0 + $1.height }
            + verticalSpacing * CGFloat(max(0, rows.count - 1))
        return CGSize(width: min(width, maxWidth), height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Cache) {
        ensureSizes(subviews: subviews, cache: &cache)
        let rows = rows(for: bounds.width, cache: &cache)
        var y = bounds.minY
        for row in rows {
            var x = bounds.minX
            for index in row.indices {
                let size = cache.sizes[index]
                subviews[index].place(
                    at: CGPoint(x: x, y: y),
                    anchor: .topLeading,
                    proposal: ProposedViewSize(size)
                )
                x += size.width + horizontalSpacing
            }
            y += row.height + verticalSpacing
        }
    }

    struct Row {
        var indices: [Int] = []
        var width: CGFloat = 0
        var height: CGFloat = 0
    }

    // MARK: - Cache helpers

    private func measureSizes(_ subviews: Subviews) -> [CGSize] {
        subviews.map { $0.sizeThatFits(.unspecified) }
    }

    /// Defensive refresh if the cache is empty or out of sync with the subview list
    /// (e.g. first call before SwiftUI has driven `makeCache`/`updateCache`).
    private func ensureSizes(subviews: Subviews, cache: inout Cache) {
        if cache.sizes.count != subviews.count {
            cache.sizes = measureSizes(subviews)
            cache.rows = []
            cache.rowsMaxWidth = nil
        }
    }

    /// Return rows for `maxWidth`, recomputing from cached sizes only when the
    /// width differs from the last computation (no re-measure of chips).
    private func rows(for maxWidth: CGFloat, cache: inout Cache) -> [Row] {
        if cache.rowsMaxWidth == maxWidth {
            return cache.rows
        }
        let computed = computeRows(maxWidth: maxWidth, sizes: cache.sizes)
        cache.rows = computed
        cache.rowsMaxWidth = maxWidth
        return computed
    }

    private func computeRows(maxWidth: CGFloat, sizes: [CGSize]) -> [Row] {
        var rows: [Row] = []
        var current = Row()
        for index in sizes.indices {
            let size = sizes[index]
            let lead = current.indices.isEmpty ? 0 : horizontalSpacing
            // Wrap when the chip would overflow — but never strand a chip on an
            // empty line, so the first chip of a row always fits even if it is
            // itself wider than the lane (it will clip, but that is degenerate).
            if !current.indices.isEmpty && current.width + lead + size.width > maxWidth {
                rows.append(current)
                current = Row()
            }
            let gap = current.indices.isEmpty ? 0 : horizontalSpacing
            current.width += gap + size.width
            current.height = max(current.height, size.height)
            current.indices.append(index)
        }
        if !current.indices.isEmpty {
            rows.append(current)
        }
        return rows
    }
}
