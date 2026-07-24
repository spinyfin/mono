import XCTest
@testable import UpdateCore

// MARK: - DownloadProgressCoalescer tests

/// Covers the pure byte-count → `DownloadProgress` mapping used by the live
/// `AssetDownloader` to turn `URLSessionDownloadDelegate` callbacks (which can fire
/// many times per second) into whole-percent UI updates.
final class DownloadProgressCoalescerTests: XCTestCase {

    func testFirstSampleIsEmitted() {
        var coalescer = DownloadProgressCoalescer()
        let sample = coalescer.sample(totalBytesWritten: 0, totalBytesExpectedToWrite: 100)
        XCTAssertEqual(sample, .determinate(0))
    }

    func testRepeatedSampleAtSamePercentIsSuppressed() {
        var coalescer = DownloadProgressCoalescer()
        _ = coalescer.sample(totalBytesWritten: 10, totalBytesExpectedToWrite: 1000)
        let repeated = coalescer.sample(totalBytesWritten: 12, totalBytesExpectedToWrite: 1000)
        XCTAssertNil(repeated, "sub-percent-point deltas must be coalesced away")
    }

    func testSequenceIsMonotonicStartingAtZeroEndingAtOne() {
        var coalescer = DownloadProgressCoalescer()
        let totalBytes: Int64 = 1000
        var fractions: [Double] = []
        for written in stride(from: Int64(0), through: totalBytes, by: 100) {
            if case .determinate(let fraction) = coalescer.sample(totalBytesWritten: written, totalBytesExpectedToWrite: totalBytes) {
                fractions.append(fraction)
            }
        }
        XCTAssertEqual(fractions.first, 0)
        XCTAssertEqual(fractions.last, 1.0)
        XCTAssertEqual(fractions, fractions.sorted(), "fractions must never regress")
    }

    func testZeroExpectedBytesIsIndeterminate() {
        var coalescer = DownloadProgressCoalescer()
        let sample = coalescer.sample(totalBytesWritten: 512, totalBytesExpectedToWrite: 0)
        XCTAssertEqual(sample, .indeterminate)
    }

    func testNegativeExpectedBytesIsIndeterminate() {
        // URLSession reports -1 when the server sent no Content-Length header.
        var coalescer = DownloadProgressCoalescer()
        let sample = coalescer.sample(totalBytesWritten: 512, totalBytesExpectedToWrite: -1)
        XCTAssertEqual(sample, .indeterminate)
    }

    func testRepeatedIndeterminateSamplesAreSuppressedAfterTheFirst() {
        var coalescer = DownloadProgressCoalescer()
        let first = coalescer.sample(totalBytesWritten: 100, totalBytesExpectedToWrite: -1)
        let second = coalescer.sample(totalBytesWritten: 200, totalBytesExpectedToWrite: -1)
        XCTAssertEqual(first, .indeterminate)
        XCTAssertNil(second, "an unknown content length never changes mid-transfer; don't re-emit")
    }

    func testSwitchingFromIndeterminateToDeterminateEmitsImmediately() {
        // Not a realistic transfer (Content-Length can't appear mid-download), but
        // exercises the coalescer's state transition without relying on that.
        var coalescer = DownloadProgressCoalescer()
        _ = coalescer.sample(totalBytesWritten: 0, totalBytesExpectedToWrite: -1)
        let sample = coalescer.sample(totalBytesWritten: 50, totalBytesExpectedToWrite: 100)
        XCTAssertEqual(sample, .determinate(0.5))
    }
}
