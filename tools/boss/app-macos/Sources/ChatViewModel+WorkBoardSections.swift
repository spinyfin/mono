import Foundation

extension ChatViewModel {
    /// Build the Done column's "Merging" section — tasks whose PR is either
    /// in GitHub's merge queue or has Merge When Ready armed
    /// (`WorkTask.isInMergingSection`), rendered collapsible above "Today".
    /// Returns `nil` when `items` is empty so the caller can omit the
    /// section entirely rather than render an empty collapsible header.
    ///
    /// Ordering is the engine-computed `section_order` from each task's
    /// `mergeQueueDetail` (queue position first, in queue order; then
    /// Merge-When-Ready tasks below) — the client never reconstructs that
    /// rule itself, only renders the key it's given. A task with a
    /// missing/unparseable `section_order` (only possible with a malformed
    /// or pre-migration payload) sorts last; ties break on task id for a
    /// stable order.
    static func mergingSection(items: [WorkTask]) -> WorkBoardSection? {
        guard !items.isEmpty else { return nil }
        let keyed = items.map { task in
            (task: task, detail: MergeQueueDetail.parse(task.mergeQueueDetail))
        }
        let sortedKeyed = keyed.sorted { lhs, rhs in
            let lhsOrder = lhs.detail?.sectionOrder ?? .max
            let rhsOrder = rhs.detail?.sectionOrder ?? .max
            if lhsOrder != rhsOrder { return lhsOrder < rhsOrder }
            return lhs.task.id < rhs.task.id
        }
        let sorted = sortedKeyed.map(\.task)
        // Queue administration lives in the Trunk web app, not Boss — this
        // just surfaces that a tracked queue is stalled, so it's clear why
        // the lane stopped moving. First match wins, read in the same
        // top-to-bottom lane order the user sees (`sortedKeyed`, not the
        // caller's `items` order): multiple distinct paused queues in one
        // Merging section is rare enough not to warrant enumerating them
        // all here. The banner text carries no product/repo name (single-
        // product v1); revisit if a second Trunk-queue product makes
        // "Trunk queue paused" alone ambiguous.
        let queueBanner = sortedKeyed.compactMap { $0.detail?.queueStateBanner }.first
        return WorkBoardSection(
            id: "done-merging",
            title: "Merging",
            items: sorted,
            isCollapsible: true,
            defaultExpanded: true,
            queueBannerText: queueBanner
        )
    }

    /// Bucket completed tasks by recency for the Done lane:
    ///   Today | Yesterday | <weekday names back to start of current week>
    ///   | Last Week | Earlier
    /// Bucketing uses `completed_at` (the time the task actually transitioned
    /// into a terminal status). Falls back to `updated_at` for rows that
    /// pre-date the `completed_at` migration (those have `completedAt == nil`).
    static func doneSections(
        items: [WorkTask],
        now: Date = Date(),
        calendar: Calendar = .current
    ) -> [WorkBoardSection] {
        let isoFractional = ISO8601DateFormatter()
        isoFractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let isoPlain = ISO8601DateFormatter()
        isoPlain.formatOptions = [.withInternetDateTime]

        let nowDay = calendar.startOfDay(for: now)
        guard let yesterdayDay = calendar.date(byAdding: .day, value: -1, to: nowDay) else {
            return [WorkBoardSection(id: "done-all", title: "Done", items: items)]
        }
        let weekday = calendar.component(.weekday, from: nowDay)
        let firstWeekday = calendar.firstWeekday
        let daysSinceStartOfWeek = (weekday - firstWeekday + 7) % 7
        guard let startOfWeek = calendar.date(byAdding: .day, value: -daysSinceStartOfWeek, to: nowDay),
              let startOfLastWeek = calendar.date(byAdding: .day, value: -7, to: startOfWeek)
        else {
            return [WorkBoardSection(id: "done-all", title: "Done", items: items)]
        }

        let weekdayFormatter = DateFormatter()
        weekdayFormatter.locale = .current
        weekdayFormatter.dateFormat = "EEEE"

        struct BucketSpec {
            let id: String
            let title: String
            let defaultExpanded: Bool
        }

        var bucketOrder: [BucketSpec] = [
            BucketSpec(id: "today", title: "Today", defaultExpanded: true),
            BucketSpec(id: "yesterday", title: "Yesterday", defaultExpanded: false),
        ]
        if daysSinceStartOfWeek >= 2 {
            for daysAgo in 2...daysSinceStartOfWeek {
                if let date = calendar.date(byAdding: .day, value: -daysAgo, to: nowDay) {
                    bucketOrder.append(
                        BucketSpec(
                            id: "weekday-\(daysAgo)",
                            title: weekdayFormatter.string(from: date),
                            defaultExpanded: false
                        )
                    )
                }
            }
        }
        bucketOrder.append(BucketSpec(id: "last-week", title: "Last Week", defaultExpanded: false))
        bucketOrder.append(BucketSpec(id: "earlier", title: "Earlier", defaultExpanded: false))

        var buckets: [String: [WorkTask]] = [:]
        for task in items {
            let bucketID = bucketID(
                for: task,
                nowDay: nowDay,
                yesterdayDay: yesterdayDay,
                startOfWeek: startOfWeek,
                startOfLastWeek: startOfLastWeek,
                calendar: calendar,
                isoFormatters: [isoFractional, isoPlain]
            )
            buckets[bucketID, default: []].append(task)
        }

        return bucketOrder.compactMap { spec -> WorkBoardSection? in
            guard let tasks = buckets[spec.id], !tasks.isEmpty else { return nil }
            let sorted = tasks.sorted { ($0.completedAt ?? $0.updatedAt) > ($1.completedAt ?? $1.updatedAt) }
            return WorkBoardSection(
                id: "done-\(spec.id)",
                title: spec.title,
                items: sorted,
                isCollapsible: true,
                defaultExpanded: spec.defaultExpanded
            )
        }
    }

    private static func bucketID(
        for task: WorkTask,
        nowDay: Date,
        yesterdayDay: Date,
        startOfWeek: Date,
        startOfLastWeek: Date,
        calendar: Calendar,
        isoFormatters: [ISO8601DateFormatter]
    ) -> String {
        guard let parsed = parseUpdatedAt(task.completedAt ?? task.updatedAt, isoFormatters: isoFormatters) else {
            return "earlier"
        }
        let day = calendar.startOfDay(for: parsed)
        if day >= nowDay {
            return "today"
        }
        if day >= yesterdayDay {
            return "yesterday"
        }
        if day >= startOfWeek {
            let delta = calendar.dateComponents([.day], from: day, to: nowDay).day ?? 0
            return "weekday-\(delta)"
        }
        if day >= startOfLastWeek {
            return "last-week"
        }
        return "earlier"
    }

    /// `boss chore list --json` currently emits `updated_at` in two
    /// shapes — Unix epoch seconds as a digit string for older rows
    /// and ISO 8601 for newer ones. The UI must handle both until the
    /// data-shape canonicalization chore lands; treat all-digit
    /// strings as Unix seconds, otherwise fall back to ISO parsing.
    static func parseUpdatedAt(
        _ string: String,
        isoFormatters: [ISO8601DateFormatter]
    ) -> Date? {
        let trimmed = string.trimmingCharacters(in: .whitespaces)
        if !trimmed.isEmpty,
           trimmed.allSatisfy({ $0.isASCII && $0.isNumber }),
           let seconds = TimeInterval(trimmed) {
            return Date(timeIntervalSince1970: seconds)
        }
        for formatter in isoFormatters {
            if let date = formatter.date(from: trimmed) {
                return date
            }
        }
        return nil
    }

    /// The board column's items, memoized in `cachedItemsByColumn` until the
    /// next `invalidateWorkCache()`.
    func workItems(in column: WorkBoardColumnKey) -> [WorkTask] {
        if let cached = cachedItemsByColumn[column] {
            return cached
        }
        // The Review column gets a dedicated ordering: newest by creation
        // time at the top, so the column is predictable and scannable (the
        // generic board sort keys on `ordinal`, which review-phase tasks
        // rarely carry, leaving them in an apparently-random order). See
        // boss issue #1250.
        let sort = column == .review ? reviewBoardSort : boardTaskSort
        var items = visibleWorkItems
            .filter { effectiveBoardColumn(for: $0) == column }
            .sorted(by: sort)
        // Revisions don't appear as standalone cards in Review or Done — they
        // roll up as single lines on the parent task's card in both lanes.
        // They are still visible in Backlog/Doing as distinct cards. An
        // `in_review` revision normally routes to Review, but one whose own
        // PR is in the merge queue / Merge When Ready
        // (`isInMergingSection`) routes to Done instead (`boardColumn`), so
        // the Done-column filter excludes `in_review` revisions too, not
        // just `done` ones.
        if column == .review {
            items = items.filter { !($0.kind == "revision" && $0.status == "in_review") }
        }
        if column == .done {
            items = items.filter { !($0.kind == "revision" && ($0.status == "done" || $0.status == "in_review")) }
        }
        cachedItemsByColumn[column] = items
        return items
    }

    /// The board column's sections, memoized in `cachedSectionsByColumn`
    /// until the next `invalidateWorkCache()`.
    func workSections(in column: WorkBoardColumnKey) -> [WorkBoardSection] {
        if let cached = cachedSectionsByColumn[column] {
            return cached
        }
        let sections = computeWorkSections(in: column)
        cachedSectionsByColumn[column] = sections
        return sections
    }

    private func computeWorkSections(in column: WorkBoardColumnKey) -> [WorkBoardSection] {
        let items = workItems(in: column)
        if column == .done {
            // `isInMergingSection` in_review tasks route into `.done` via
            // `boardColumn` so they render in the Merging section; split
            // them out here so `doneSections`'s recency bucketing only ever
            // sees genuinely completed (`status == "done"`) tasks.
            let merging = items.filter(\.isInMergingSection)
            let completed = items.filter { !$0.isInMergingSection }
            var sections: [WorkBoardSection] = []
            if let mergingSection = Self.mergingSection(items: merging) {
                sections.append(mergingSection)
            }
            sections.append(contentsOf: Self.doneSections(items: completed))
            return sections
        }
        guard workBoardGrouping == .project else {
            return [WorkBoardSection(id: column.rawValue, title: column.title, items: items)]
        }

        let grouped = Dictionary(grouping: items) { task in
            if task.isChore { return "Chores" }
            // Chore-parented revisions inherit nil projectID from the chain
            // root (a chore). Group them with chores so they don't land in
            // a confusing "No Project" section — they are logically part of
            // the chore world.
            if task.kind == "revision", task.projectID == nil { return "Chores" }
            return projectName(for: task.projectID) ?? "No Project"
        }

        return grouped.keys.sorted().compactMap { key in
            guard let sectionItems = grouped[key], !sectionItems.isEmpty else { return nil }
            let projectID = sectionItems.first(where: { !$0.isChore })?.projectID
            return WorkBoardSection(
                id: "\(column.rawValue)-\(key)",
                title: key,
                items: sectionItems,
                isCollapsible: true,
                defaultExpanded: true,
                projectID: projectID
            )
        }
    }

    func isTaskVisible(_ task: WorkTask) -> Bool {
        workItems(in: effectiveBoardColumn(for: task)).contains(where: { $0.id == task.id })
    }

    /// Replace this product's slice of [[taskRuntimesByID]] with `runtimes`.
    /// Removes only the keys actually being replaced instead of filtering
    /// the whole dictionary — `taskRuntimesByID` accumulates every product
    /// ever viewed this session, so a full-dictionary filter here cost
    /// O(every product's items) on every single work-tree refresh, not
    /// O(this product's items). This was the dominant unattributed cost in
    /// the `apply` population-timing segment (see the ui-stall diagnostics
    /// this fix responds to).
    ///
    /// Mutations are staged on a local `var merged` and published with a
    /// single assignment at the end, not one `taskRuntimesByID[id] = ...`
    /// per element. `@Published`'s setter has observable side effects
    /// (`objectWillChange.send()`), so the compiler cannot synthesize an
    /// in-place `_modify` accessor for it the way it would for a plain
    /// stored property — every subscript write on the property directly
    /// desugars to a get (temp = taskRuntimesByID), mutate, set
    /// (taskRuntimesByID = temp) sequence. That get leaves the dictionary
    /// with two owners (the property's storage and `temp`) for the
    /// duration of the mutate step, so each write forces a full
    /// copy-on-write of the *entire accumulated dictionary* — the
    /// `WorkTaskRuntimeVwcp`/`VWOc` value-witness-copy frames dominating the
    /// captured 250-300ms stalls — not just the handful of keys actually
    /// changing. Staging on a local var keeps the dictionary uniquely
    /// referenced across every mutation (in-place, O(1) amortized each) and
    /// triggers exactly one CoW copy and one `objectWillChange` for the
    /// whole merge, regardless of how many runtimes changed.
    func mergeTaskRuntimes(
        _ runtimes: [WorkTaskRuntime],
        for productID: String,
        tasks: [WorkTask],
        chores: [WorkTask]
    ) {
        let productItemIDs = Set(tasks.map(\.id) + chores.map(\.id))
        let freshRuntimeIDs = Set(runtimes.map(\.workItemID))
        var merged = taskRuntimesByID
        for id in productItemIDs where !freshRuntimeIDs.contains(id) {
            merged.removeValue(forKey: id)
        }
        for runtime in runtimes {
            merged[runtime.workItemID] = runtime
        }
        taskRuntimesByID = merged
    }

    func taskRuntime(for taskID: String) -> WorkTaskRuntime? {
        taskRuntimesByID[taskID]
    }
}
