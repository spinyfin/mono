import XCTest
@testable import Boss

/// Instantiated by XCTest before any test runs — it is the test bundle's
/// `NSPrincipalClass` (`Tests/BossTests/Info.plist`, merged into every
/// BossTests shard by `infoplists` in `BUILD.bazel`). Its job is to turn
/// `ReviewSectionDiagnostics`' fault log into a test failure: a Review card
/// that reaches the board without an engine section, from a section-aware
/// engine and not as a just-dragged card, fails whichever test produced it
/// instead of being quietly filed under Pending. Tests that feed such a card
/// on purpose replace the observer for their own duration and restore it.
///
/// The ObjC name is fixed so the plist entry resolves in every shard's
/// module (`BossTestsShard1`, `BossTestsShard2`, ...).
@objc(BossTestsPrincipal)
final class BossTestsPrincipal: NSObject {
    override init() {
        super.init()
        ReviewSectionDiagnostics.onMissingSection = { task in
            XCTFail(
                "Review card \(task.id) (\(task.name)) reached the board without a review_section from a "
                    + "section-aware engine. The engine owns that classification; an engine update path dropped "
                    + "it, or the fixture set reviewSectionReported without a section. "
                    + "status=\(task.status) ci=\(task.ciRequiredState ?? "nil") "
                    + "mergeable=\(task.prMergeableState ?? "nil")"
            )
        }
    }
}
