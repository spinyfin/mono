import Foundation
import os

extension EngineClient {
    /// Separate JSON syntax errors from valid JSON with an invalid wire shape.
    static func decodeEnvelope(_ data: Data) throws -> ([String: Any], [String: Any], String) {
        let object = try JSONSerialization.jsonObject(with: data)
        guard let envelope = object as? [String: Any],
              let payload = envelope["payload"] as? [String: Any],
              let type = payload["type"] as? String else {
            throw NSError(domain: "Boss.EngineClient.Envelope", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "expected object with payload.type string",
            ])
        }
        return (envelope, payload, type)
    }

    /// Base64 preserves invalid UTF-8 and control bytes without log injection.
    /// Bound each sample independently of the size of a work-tree response.
    static func frameDiagnostic(_ data: Data, error: String) -> [String: Any] {
        [
            "error": error,
            "length_bytes": data.count,
            "prefix_base64": Data(data.prefix(128)).base64EncodedString(),
            "suffix_base64": Data(data.suffix(128)).base64EncodedString(),
        ]
    }

    static func logInvalidFrame(_ data: Data, error: String) {
        Logger(subsystem: "com.boss.app", category: "EngineClient")
            .error("engine frame rejected: \(error, privacy: .public); bytes=\(data.count)")
        IpcLog.shared.log(
            requestId: "",
            direction: "engine→app",
            kind: "invalid_frame",
            body: frameDiagnostic(data, error: error)
        )
    }
}
