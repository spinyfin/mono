import Foundation

final class EngineProcessController: @unchecked Sendable {
    private var process: Process?
    private var stdoutPipe: Pipe?
    private var stderrPipe: Pipe?

    var onOutputLine: (@MainActor @Sendable (String) -> Void)?

    func start(socketPath: String) throws {
        guard process == nil else {
            return
        }

        let command = ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"]
            ?? "bazel run //tools/boss/engine:engine -- --mode=server --socket-path \(socketPath)"

        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/zsh")
        proc.arguments = ["-c", command]
        proc.environment = ProcessInfo.processInfo.environment

        let stdout = Pipe()
        let stderr = Pipe()
        proc.standardOutput = stdout
        proc.standardError = stderr

        observe(pipe: stdout)
        observe(pipe: stderr)

        proc.terminationHandler = { [weak self] process in
            guard let self else { return }
            Task { @MainActor in
                self.onOutputLine?("[engine exited] status=\(process.terminationStatus)")
            }
            self.process = nil
            self.stdoutPipe = nil
            self.stderrPipe = nil
        }

        try proc.run()

        process = proc
        stdoutPipe = stdout
        stderrPipe = stderr

        Task { @MainActor in
            onOutputLine?("[engine launch] \(command)")
        }
    }

    func stop() {
        guard let process else {
            return
        }

        if process.isRunning {
            process.terminate()
        }

        self.process = nil
        stdoutPipe = nil
        stderrPipe = nil
    }

    private func observe(pipe: Pipe) {
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            guard let self else { return }
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else {
                return
            }

            for line in text.split(whereSeparator: \ .isNewline) {
                Task { @MainActor in
                    self.onOutputLine?("[engine] \(line)")
                }
            }
        }
    }
}
