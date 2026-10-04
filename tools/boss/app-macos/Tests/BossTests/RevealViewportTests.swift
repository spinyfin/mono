import AppKit
import XCTest
@testable import Boss

@MainActor
final class RevealViewportTests: XCTestCase {
    func testRevealExpandsEveryDisclosurePreferenceWithoutChangingIt() {
        for defaultExpanded in [false, true] {
            for savedToggle in [nil, false, true] as [Bool?] {
                let sectionID = "reveal-test-\(UUID())"
                let key = WorkBoardSectionCollapse.storageKey(sectionID: sectionID)
                BossDefaults.store.set(savedToggle, forKey: key)
                defer { BossDefaults.store.removeObject(forKey: key) }
                let userToggled = WorkBoardSectionCollapse.userToggled(sectionID: sectionID)
                XCTAssertTrue(WorkBoardSectionCollapse.isExpanded(
                    defaultExpanded: defaultExpanded, userToggled: userToggled, revealExpanded: true
                ))
                XCTAssertEqual(WorkBoardSectionCollapse.isExpanded(
                    defaultExpanded: defaultExpanded, userToggled: userToggled, revealExpanded: false
                ), userToggled ? !defaultExpanded : defaultExpanded)
                XCTAssertEqual(BossDefaults.store.object(forKey: key) as? Bool, savedToggle)
            }
        }
    }

    func testMountedCardOutsideEitherBoardAxisIsNotVisible() {
        let viewport = NSRect(x: 0, y: 0, width: 600, height: 400)
        for origin in [NSPoint(x: 700, y: 100), NSPoint(x: 100, y: 500), NSPoint(x: 590, y: 100)] {
            XCTAssertFalse(RevealCardViewport.isVisible(
                card: NSRect(origin: origin, size: NSSize(width: 200, height: 100)), clippedTo: viewport
            ))
        }
        XCTAssertTrue(RevealCardViewport.isVisible(
            card: NSRect(x: 100, y: 100, width: 200, height: 100), clippedTo: viewport
        ))
        XCTAssertTrue(RevealCardViewport.isVisible(
            card: NSRect(x: 100, y: -100, width: 200, height: 600), clippedTo: viewport
        ))
    }
}
