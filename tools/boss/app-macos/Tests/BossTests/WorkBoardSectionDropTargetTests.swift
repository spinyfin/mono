import XCTest
@testable import Boss

/// `WorkBoardSection.dropGroupKey` — which group, if any, a drop landing on a
/// section reports to the engine.
///
/// A group-qualified drop is how the engine tells a reorder inside
/// Done ▸ Merging (no-op) from a deliberate Merging → completed transition.
/// That distinction only holds when the user can see what they dropped onto:
/// a collapsed section is a header strip with no cards under it, so it names
/// the column alone and the engine treats it as the unqualified drop it
/// visually was.
@MainActor
final class WorkBoardSectionDropTargetTests: XCTestCase {

    /// Section ids are the defaults keys, so every test uses its own.
    private var sectionID: String = ""

    override func setUp() {
        super.setUp()
        sectionID = "test-section-\(UUID().uuidString)"
    }

    override func tearDown() {
        BossDefaults.store.removeObject(
            forKey: WorkBoardSectionCollapse.storageKey(sectionID: sectionID)
        )
        super.tearDown()
    }

    private func section(
        defaultExpanded: Bool,
        isCollapsible: Bool = true,
        groupKey: WorkBoardGroupKey? = .completed
    ) -> WorkBoardSection {
        WorkBoardSection(
            id: sectionID,
            title: "Monday",
            items: [],
            isCollapsible: isCollapsible,
            defaultExpanded: defaultExpanded,
            groupKey: groupKey
        )
    }

    private func setUserToggled(_ value: Bool) {
        BossDefaults.store.set(
            value, forKey: WorkBoardSectionCollapse.storageKey(sectionID: sectionID)
        )
    }

    func testExpandedSectionReportsItsGroup() {
        XCTAssertEqual(section(defaultExpanded: true).dropGroupKey, .completed)
    }

    /// The reported gesture: Done holds an expanded "Merging" group with a
    /// collapsed completion group ("Monday (1)") stacked directly beneath it.
    /// A drag inside Merging that overshoots by a few points lands on that
    /// header strip. Reporting the column alone makes it a reorder rather
    /// than a completion the operator cannot cleanly undo.
    func testCollapsedByDefaultSectionReportsNoGroup() {
        XCTAssertNil(section(defaultExpanded: false).dropGroupKey)
    }

    /// The persisted bit is "the user flipped this away from its default",
    /// not the expanded state itself — so both directions must be honoured.
    func testUserToggleFlipsTheReportedGroupInBothDirections() {
        setUserToggled(true)
        XCTAssertNil(
            section(defaultExpanded: true).dropGroupKey,
            "a section the user collapsed reports no group"
        )
        XCTAssertEqual(
            section(defaultExpanded: false).dropGroupKey,
            .completed,
            "a section the user expanded reports its group again"
        )
    }

    /// The narrowing must not disable the deliberate gesture: an expanded
    /// completion group still completes an in-flight merge dropped onto it.
    func testExpandedCompletionGroupStillCarriesTheCompletionKey() {
        setUserToggled(false)
        XCTAssertEqual(section(defaultExpanded: true, groupKey: .completed).dropGroupKey, .completed)
    }

    /// Merging renders expanded by default, so the drop target the reorder
    /// happens inside is group-qualified and resolves as a reorder rather
    /// than falling back to the column.
    func testMergingSectionIsGroupQualifiedByDefault() {
        let merging = ChatViewModel.mergingSection(items: [makeTask()])
        XCTAssertEqual(merging?.dropGroupKey, .merging)
    }

    /// Non-collapsible sections are always fully visible, so collapse state
    /// never applies to them — including the stale-defaults case where a key
    /// survives from a section that used to be collapsible.
    func testNonCollapsibleSectionIgnoresCollapseState() {
        setUserToggled(true)
        XCTAssertEqual(
            section(defaultExpanded: true, isCollapsible: false).dropGroupKey,
            .completed
        )
    }

    /// Sections with no group of their own (project rollups, flat columns)
    /// report nothing either way.
    func testGrouplessSectionReportsNilRegardlessOfCollapseState() {
        XCTAssertNil(section(defaultExpanded: true, groupKey: nil).dropGroupKey)
        XCTAssertNil(section(defaultExpanded: false, groupKey: nil).dropGroupKey)
    }

    private func makeTask() -> WorkTask {
        var task = WorkTask(
            id: "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
        task.mergeQueueState = "queued"
        return task
    }
}
