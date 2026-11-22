import Foundation

/// Shared parser for the engine fixture's stdout bind announcement.
///
/// The fixture writes `boss-engine listening on <socket>` after bind. The
/// socket file existing is not enough: `connect()` can succeed before the
/// accept loop starts.
enum EngineFixtureReadiness {
    static func waitForListening(socket: String, output: FileHandle) throws {
        // Consume each available chunk: a fixed-length read can wait for more
        // bytes even after the engine has announced readiness and gone quiet.
        try waitForListening(socket: socket) { output.availableData }
    }

    static func waitForListening(socket: String, read: () throws -> Data) throws {
        let ready = Data("boss-engine listening on \(socket)".utf8)
        var pending = Data()
        while true {
            let chunk = try read()
            guard !chunk.isEmpty else {
                throw NSError(domain: "EngineFixture", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: "engine stdout closed before announcing \(socket)",
                ])
            }
            pending.append(chunk)
            while let newline = pending.firstIndex(of: 0x0A) {
                if pending[..<newline] == ready { return }
                pending.removeSubrange(...newline)
            }
        }
    }
}
