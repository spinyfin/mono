import XCTest
@testable import Boss

@MainActor
final class WorkersWorkspaceModelSendTests: XCTestCase {
    func testAttachUsesTmuxClientCommandWithoutWorkerEnvironment() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        ))
        guard case .success = result else {
            XCTFail("expected tmux pane attach to succeed, got \(result)")
            return
        }

        let session = model.slots.first(where: { $0.slotId == 1 })?.session
        XCTAssertEqual(session?.launchSpec.initialInput, "exec tmux -S '/state/boss/tmux.sock' attach-session -t 'boss-1-run-tmux'\n")
        XCTAssertTrue(session?.launchSpec.env.isEmpty ?? false)
    }

    func testDetachRemovesTmuxViewerSurface() {
        let model = WorkersWorkspaceModel()
        _ = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        ))

        let result = model.detachWorkerPane(slotId: 1)
        guard case .success = result else {
            XCTFail("expected tmux pane detach to succeed, got \(result)")
            return
        }
        XCTAssertNil(model.slots.first(where: { $0.slotId == 1 })?.session)
    }

    func testAttachRejectsEmptyTmuxSocketPath() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "",
            summary: nil,
            taskTitle: nil
        ))
        guard case .failure(.internalFailure(let message)) = result else {
            XCTFail("expected .internalFailure for empty socket path, got \(result)")
            return
        }
        XCTAssertTrue(message.contains("tmux socket path"), "got \(message)")
    }

    func testAttachRejectsRelativeTmuxSocketPath() {
        let model = WorkersWorkspaceModel()
        let result = model.attachWorkerPane(EngineAttachRequest(
            runId: "run-tmux",
            slotId: 1,
            sessionName: "boss-1-run-tmux",
            tmuxSocketPath: "tmux.sock",
            summary: nil,
            taskTitle: nil
        ))
        guard case .failure(.internalFailure(let message)) = result else {
            XCTFail("expected .internalFailure for relative socket path, got \(result)")
            return
        }
        XCTAssertTrue(message.contains("tmux socket path"), "got \(message)")
    }

    func testSendToUnknownSlotReturnsUnknownSlot() {
        // Mirrors `focusWorkerPane` / `interruptWorkerPane`: a
        // `SendToPane` for a slot that the workers grid does not host
        // must surface `.unknownSlot` so the engine can decide whether
        // to requeue (probe injection) or surface a `WorkError` (the
        // `agents send` CLI path). Silently no-op'ing here was the
        // shape of the original intervene bug — a missing slot looked
        // like a successful injection, the engine moved on, and the
        // prompt was lost.
        let model = WorkersWorkspaceModel()
        let result = model.sendToPane(slotId: 99, text: "echo hello", expectedDriverBinary: "claude")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for nonexistent slot, got \(result)")
            return
        }
    }

    func testSendToIdleSlotReturnsUnknownSlot() {
        // An allocated slot with no session attached is the same
        // class of failure as a nonexistent slot — the app has no
        // surface to write to. Matches the equivalent
        // `focusWorkerPane` test so the engine's failure-handling
        // path stays uniform across the three pane verbs.
        let model = WorkersWorkspaceModel()
        let result = model.sendToPane(slotId: 1, text: "echo hello", expectedDriverBinary: "claude")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }
}

@MainActor
final class GhosttyTerminalHostSubmissionPlanTests: XCTestCase {
    func testPreservesBodyAndAlwaysSubmitsWhenNoTrailingNewline() {
        // The bug we are fixing: the prompt landed in the worker's
        // input buffer but was never submitted. The writer must
        // always follow the paste with a Return keystroke, regardless
        // of whether the caller bothered to terminate the text.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "echo hello")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "echo hello", sendReturn: true))
    }

    func testStripsSingleTrailingNewlineBeforeSubmitting() {
        // Earlier revisions of `bossctl agents send` appended `\n`
        // to the payload in the belief that libghostty's paste path
        // would treat it as Enter. It does not — the `\n` lands as a
        // literal newline character in the input field, leaving the
        // prompt with a trailing blank line when the writer adds its
        // own Return. Strip the trailing newline so the submitted
        // prompt matches what the human meant to type.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "echo hello\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "echo hello", sendReturn: true))
    }

    func testStripsTrailingCRLFAndRepeatedNewlines() {
        // Heredoc-quoted prompts coming through shells can carry
        // `\r\n` line endings or a couple of trailing newlines.
        // Strip them all — they would otherwise pollute the input
        // field with stray blank lines before the Return keystroke
        // submits.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "first\nsecond\r\n\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "first\nsecond", sendReturn: true))
    }

    func testInternalNewlinesArePreserved() {
        // Multi-line prompts (e.g. a Stop-boundary probe asking the
        // worker to "explain what you're blocked on" across two
        // sentences) must keep their internal newlines so the paste
        // delivers the full body. Only the *trailing* newline gets
        // stripped before the Return submits.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "line one\nline two")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "line one\nline two", sendReturn: true))
    }

    func testEmptyPayloadStillSubmits() {
        // A degenerate "press enter" intervene (empty body) is rare
        // but well-defined: submit whatever the human had already
        // typed into the input field. The writer should still
        // synthesize Return — the body just has nothing to paste.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "", sendReturn: true))
    }

    func testWhitespaceOnlyPayloadKeepsLeadingSpaces() {
        // Trailing newlines come off; other whitespace stays. A
        // human who explicitly typed a leading space (e.g. quoting
        // shell input) should see that space preserved in the
        // submitted prompt.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "  spaced\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "  spaced", sendReturn: true))
    }
}

@MainActor
final class GhosttyTerminalHostSurfaceFailureDiagnosticTests: XCTestCase {
    func testDiagnosticReportsEveryControlledInput() {
        // When `ghostty_surface_new` returns NULL the host view no
        // longer `fatalError`s (issue #800 — a no-active-display
        // condition crashed the whole app). The NULL path is now a
        // logged, recoverable event, so the diagnostic block is the
        // only signal that survives into the dev log / os_log. Pin its
        // contract: every input we control must be reported, so a
        // future libghostty-rejection is still debuggable from the log
        // alone.
        let diagnostic = GhosttyTerminalHostView.surfaceFailureDiagnostic(
            appNonNil: true,
            workingDirectory: "/tmp/workdir",
            cwdExists: false,
            isDirectory: false,
            fontSize: 13,
            scaleFactor: 2.0,
            envVarCount: 3,
            envSummary: "PATH=/usr/bin, TERM=xterm",
            initialInputCount: 42
        )

        // Match label and value independently so the test pins the
        // contract (every field is reported) without being brittle to
        // the column-alignment whitespace.
        XCTAssertTrue(diagnostic.contains("ghostty_surface_new returned NULL"))
        XCTAssertTrue(diagnostic.contains("runtime.app != nil:"))
        XCTAssertTrue(diagnostic.contains("workingDirectory:"))
        XCTAssertTrue(diagnostic.contains("/tmp/workdir"))
        XCTAssertTrue(diagnostic.contains("env_var_count:"))
        XCTAssertTrue(diagnostic.contains("env (first 8):"))
        XCTAssertTrue(diagnostic.contains("PATH=/usr/bin, TERM=xterm"))
        XCTAssertTrue(diagnostic.contains("initialInput (chars):"))
        XCTAssertTrue(diagnostic.contains("42"))
    }
}

@MainActor
final class WorkersWorkspaceModelFocusTests: XCTestCase {
    func testFocusUnknownSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // Interactive grid is 1...16 (Bridge Crew + Lower Decks); 99 has no slot at all.
        let result = model.focusWorkerPane(slotId: 99)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for nonexistent slot, got \(result)")
            return
        }
    }

    func testFocusIdleSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // All slots start without a session attached. Focusing an
        // idle slot should fail the same way as an unknown one — the
        // app has nothing to raise. Mirrors the
        // `release_worker_pane` semantics for idle slots so the engine
        // can treat both cases the same way.
        let result = model.focusWorkerPane(slotId: 1)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }
}

@MainActor
final class WorkersWorkspaceModelPaneInputTests: XCTestCase {
    func testDriverInputRefusesWhenForegroundProcessIsTheShell() {
        let error = WorkersWorkspaceModel.driverInputError(
            expectedDriverBinary: "grok",
            foregroundProcess: "zsh"
        )
        guard case .driverExited(let expected, let observed) = error else {
            XCTFail("expected the shell foreground process to refuse agent input, got \(String(describing: error))")
            return
        }
        XCTAssertEqual(expected, "grok")
        XCTAssertEqual(observed, "zsh")
    }

    func testDriverInputAllowsTheLiveForegroundDriver() {
        XCTAssertNil(
            WorkersWorkspaceModel.driverInputError(
                expectedDriverBinary: "grok",
                foregroundProcess: "grok"
            )
        )
    }
}

@MainActor
final class WorkersWorkspaceModelSpawnTests: XCTestCase {
    private func makeRequest(slot: Int, runId: String = "run-test") -> EngineSpawnRequest {
        EngineSpawnRequest(
            runId: runId,
            workspacePath: "/tmp/ws",
            slotId: slot,
            initialInput: "claude\n",
            env: [],
            summary: nil,
            taskTitle: nil,
            paneMonitor: nil
        )
    }

    func testSpawnHonorsEngineClaimedSlot() {
        // Engine asked for slot 5. The app must host the pane in
        // slot 5 — not the lowest free slot, not a random one. This
        // is the contract that replaces the old firstIndex(where:)
        // heuristic.
        let model = WorkersWorkspaceModel()
        let result = model.spawnWorkerPane(makeRequest(slot: 5))
        guard case .success(let slotId, _) = result else {
            XCTFail("expected .success, got \(result)")
            return
        }
        XCTAssertEqual(slotId, 5, "app must honor the engine-supplied slot")
        XCTAssertNotNil(
            model.slots.first(where: { $0.slotId == 5 })?.session,
            "slot 5 should now host a session"
        )
        XCTAssertNil(
            model.slots.first(where: { $0.slotId == 1 })?.session,
            "no other slot should be touched when the engine asked for slot 5"
        )
    }

    func testSpawnIntoOccupiedSlotReturnsSlotBusy() {
        // Engine and app disagree about whether slot 3 is free. The
        // app must surface .slotBusy rather than silently picking a
        // different slot — that would re-introduce the dual
        // allocator the engine-owns-slots refactor exists to remove.
        let model = WorkersWorkspaceModel()
        _ = model.spawnWorkerPane(makeRequest(slot: 3, runId: "run-first"))
        let result = model.spawnWorkerPane(makeRequest(slot: 3, runId: "run-second"))
        guard case .failure(.slotBusy(let occupyingRunId)) = result else {
            XCTFail("expected .slotBusy when engine asks for an occupied slot, got \(result)")
            return
        }
        XCTAssertEqual(
            occupyingRunId,
            "run-first",
            "slotBusy should report the run already hosted in the slot"
        )
    }

    func testSpawnRejectsOutOfRangeSlot() {
        let model = WorkersWorkspaceModel()
        let zeroResult = model.spawnWorkerPane(makeRequest(slot: 0))
        guard case .failure(.internalFailure) = zeroResult else {
            XCTFail("expected .internalFailure for slot 0, got \(zeroResult)")
            return
        }
        let highResult = model.spawnWorkerPane(makeRequest(slot: 99))
        guard case .failure(.internalFailure) = highResult else {
            XCTFail("expected .internalFailure for slot 99, got \(highResult)")
            return
        }
    }

    func testSurfaceFailureReportsSpawnFailureNotPaneDeath() {
        // The pane's libghostty surface never attached — no shell process
        // ever started for this run, so nothing died. It must be reported
        // as a SPAWN failure (`onSpawnFailed`, which the engine reaps via
        // the never-started-spawn path that feeds the cross-work-item
        // spawn-capability breaker) and must NOT also fire `onPaneDied`
        // (whose engine reap does not feed that breaker). Reporting both
        // is what raced the diagnosis out of the record and left the
        // breaker starved through the 2026-07 no-active-display churn.
        let model = WorkersWorkspaceModel()
        var diedRunIds: [String] = []
        var reasons: [WorkerPaneDeathReason] = []
        model.onPaneDied = { runId, reason in
            diedRunIds.append(runId)
            reasons.append(reason)
        }
        var spawnFailures: [(String, String)] = []
        model.onSpawnFailed = { spawnFailures.append(($0, $1)) }

        _ = model.spawnWorkerPane(makeRequest(slot: 5, runId: "exec-surface-failed"))
        let session = model.slots.first(where: { $0.slotId == 5 })?.session
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            screenCount: 1,
            nsScreenMainNonNil: true
        )
        session?.onSurfaceCreationFailed?("no active display", host, "diagnostic")

        XCTAssertEqual(spawnFailures.map(\.0), ["exec-surface-failed"])
        XCTAssertEqual(spawnFailures.map(\.1), ["no active display"])
        XCTAssertTrue(
            diedRunIds.isEmpty,
            "a surface that never came up hosted no shell and must not be reported as a pane death"
        )
        XCTAssertTrue(reasons.isEmpty)
    }

    func testSurfaceFailureReasonNamesTheDisplayStateActuallyObserved() {
        // The reason string is the human-facing explanation the engine
        // stores as the orphan reason. A reason that names display
        // availability whatever the real display state is makes the
        // recoverable #800 condition and a genuine non-transient rejection
        // (env pollution, bad cwd, version mismatch) indistinguishable in
        // the record, so each branch must name what it actually observed.
        // Measured via HostDisplaySnapshot (CG active count), not NSScreen.main.
        // Realistic lock-screen shape: active=0, online=1, nsScreenMainNonNil=true.
        let noDisplay = GhosttyTerminalHostView.surfaceFailureReason(
            host: .make(
                activeDisplayCount: 0,
                onlineDisplayCount: 1,
                mainDisplayAsleep: true,
                sessionLocked: true,
                screenCount: 1,
                nsScreenMainNonNil: true
            )
        )
        XCTAssertTrue(
            noDisplay.contains("no active CG displays"),
            "the no-display case must name it as the cause; got: \(noDisplay)"
        )

        let withDisplay = GhosttyTerminalHostView.surfaceFailureReason(
            host: .make(
                activeDisplayCount: 1,
                onlineDisplayCount: 1,
                screenCount: 1,
                nsScreenMainNonNil: true
            )
        )
        XCTAssertTrue(
            withDisplay.contains("active CG displays present"),
            "a failure with a display present must not blame display availability; got: \(withDisplay)"
        )
        XCTAssertTrue(
            withDisplay.contains("bossctl logs spawn"),
            "active-display branch must point at retrievable spawn logs; got: \(withDisplay)"
        )
        XCTAssertFalse(
            withDisplay.lowercased().contains("stderr"),
            "must not point at unreadable stderr; got: \(withDisplay)"
        )
        XCTAssertNotEqual(
            noDisplay,
            withDisplay,
            "the two causes must be distinguishable from the reason string alone"
        )
    }

    func testChildExitedReportsPaneDied() {
        // The pane's shell process exited. Worker panes (unlike the
        // Boss pane, which restarts itself) must report this to the
        // engine via `onPaneDied` so the backing execution is reaped
        // immediately.
        let model = WorkersWorkspaceModel()
        var diedRunIds: [String] = []
        var reasons: [WorkerPaneDeathReason] = []
        model.onPaneDied = { runId, reason in
            diedRunIds.append(runId)
            reasons.append(reason)
        }

        _ = model.spawnWorkerPane(makeRequest(slot: 5, runId: "exec-child-exited"))
        let session = model.slots.first(where: { $0.slotId == 5 })?.session
        session?.terminalReady = true
        session?.onChildExited?()
        session?.onChildExited?()

        XCTAssertEqual(diedRunIds, ["exec-child-exited"])
        XCTAssertEqual(reasons, [.childProcessExited])
    }

    func testChildExitBeforeSurfaceAttachIsNotReported() {
        let model = WorkersWorkspaceModel()
        var reports = 0
        model.onPaneDied = { _, _ in reports += 1 }

        _ = model.spawnWorkerPane(makeRequest(slot: 5, runId: "exec-not-attached"))
        let session = model.slots.first(where: { $0.slotId == 5 })?.session
        XCTAssertFalse(session?.terminalReady ?? true)

        session?.onChildExited?()

        XCTAssertEqual(reports, 0, "a pre-attach close callback is not a child-process death")
    }

    func testCloseCallbackRequiresActualCurrentChildExit() {
        XCTAssertFalse(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: true,
                isReleased: false,
                processExited: false
            ),
            "a no-confirm close request does not mean the child exited"
        )
        XCTAssertFalse(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: false,
                isReleased: false,
                processExited: true
            ),
            "a stale or not-yet-attached surface cannot report this session dead"
        )
        XCTAssertFalse(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: true,
                isReleased: true,
                processExited: true
            ),
            "engine-driven release must not echo a new pane-death report"
        )
        XCTAssertTrue(
            GhosttyRuntime.shouldReportChildExit(
                needsConfirmation: false,
                isCurrentAttachedSurface: true,
                isReleased: false,
                processExited: true
            )
        )
    }

    func testSurfaceCreationFailureForwardsNackWithRunId() {
        // The post-sleep false-live spawn: `spawnWorkerPane` wires the
        // session's `onSurfaceCreationFailed` to the model's `onSpawnFailed`
        // so the app can NACK the engine (fail-fast) instead of leaving the
        // spawn to the engine's 60s spawn-ack timeout. Simulate the surface
        // failing and assert the NACK carries the raw run id and the reason.
        let model = WorkersWorkspaceModel()
        var captured: (runId: String, reason: String)?
        model.onSpawnFailed = { runId, reason in captured = (runId, reason) }

        let result = model.spawnWorkerPane(makeRequest(slot: 2, runId: "exec-nack"))
        guard case .success = result else {
            XCTFail("spawn precondition failed: \(result)")
            return
        }
        let session = model.slots.first(where: { $0.slotId == 2 })?.session
        XCTAssertNotNil(
            session?.onSurfaceCreationFailed,
            "spawn must wire the surface-creation-failure callback so a no-display spawn can NACK"
        )

        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            screenCount: 1,
            nsScreenMainNonNil: true
        )
        session?.onSurfaceCreationFailed?("no active display", host, "diagnostic")

        XCTAssertEqual(captured?.runId, "exec-nack", "NACK must carry the raw execution id")
        XCTAssertEqual(captured?.reason, "no active display")
    }
}

@MainActor
final class WorkersWorkspaceModelReleaseTests: XCTestCase {
    private func makeSpawn(slot: Int) -> EngineSpawnRequest {
        EngineSpawnRequest(
            runId: "run-release-\(slot)",
            workspacePath: "/tmp/ws",
            slotId: slot,
            initialInput: "claude\n",
            env: [],
            summary: nil,
            taskTitle: nil,
            paneMonitor: nil
        )
    }

    func testReleaseUnknownSlotReturnsUnknownSlot() {
        // Engine asked the app to release slot 99 but the interactive
        // grid is 1...16 — there's nothing to release. Mirrors the
        // `sendToPane` / `focusWorkerPane` shape so the engine's
        // failure-handling stays uniform across pane verbs.
        let model = WorkersWorkspaceModel()
        let result = model.releaseWorkerPane(slotId: 99, killGraceSeconds: 0)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for slot outside 1...16, got \(result)")
            return
        }
    }

    func testReleaseIdleSlotReturnsUnknownSlot() {
        // An allocated slot with no session is the same class of
        // failure as a nonexistent one — there's no live pty to
        // reap. The engine relies on this to make
        // `release_worker_pane` idempotent across the redundant
        // chore-done / completion-detection / `bossctl agents stop`
        // paths.
        let model = WorkersWorkspaceModel()
        let result = model.releaseWorkerPane(slotId: 1, killGraceSeconds: 5)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }

    func testReleaseLiveSlotClearsSessionAndSucceeds() {
        // After a real spawn the slot hosts a session. Releasing the
        // slot must (a) return `.success` synchronously (the engine's
        // 5s timeout fires otherwise) and (b) drop the session,
        // runId, and summary fields so the kanban / pane titlebar
        // stop showing the worker as attached. The kill-ladder side
        // effects are covered by `WorkerProcessKillerTests`; here we
        // only assert the slot-state half so a regression on the
        // session-clearing wouldn't masquerade as success.
        let model = WorkersWorkspaceModel()
        let spawn = model.spawnWorkerPane(makeSpawn(slot: 4))
        guard case .success = spawn else {
            XCTFail("spawn precondition failed: \(spawn)")
            return
        }
        XCTAssertNotNil(model.slots.first(where: { $0.slotId == 4 })?.session)

        let result = model.releaseWorkerPane(slotId: 4, killGraceSeconds: 0)
        guard case .success = result else {
            XCTFail("expected .success releasing a live slot, got \(result)")
            return
        }
        XCTAssertNil(
            model.slots.first(where: { $0.slotId == 4 })?.session,
            "session must be cleared so SwiftUI tears down the libghostty surface"
        )
        XCTAssertNil(
            model.slots.first(where: { $0.slotId == 4 })?.runId,
            "runId must be cleared so the kanban stops attributing the slot to the run"
        )
    }
}

@MainActor
final class WorkersWorkspaceModelPageTests: XCTestCase {
    private func makeRequest(slot: Int, runId: String = "run-page") -> EngineSpawnRequest {
        EngineSpawnRequest(
            runId: runId,
            workspacePath: "/tmp/ws",
            slotId: slot,
            initialInput: "claude\n",
            env: [],
            summary: nil,
            taskTitle: nil,
            paneMonitor: nil
        )
    }

    func testInteractivePoolIsSixteenSlotsSplitIntoTwoPages() {
        // The interactive pool is now two pages of 8: Bridge Crew (slots
        // 1...8) and Lower Decks (slots 9...16). Both are drawn from the flat
        // `slots` array; the pages must be disjoint and cover it exactly.
        let model = WorkersWorkspaceModel()
        XCTAssertEqual(model.slots.count, 16, "main pool must span both pages")
        XCTAssertEqual(model.bridgeCrewSlots.map(\.slotId), Array(1...8))
        XCTAssertEqual(model.lowerDecksSlots.map(\.slotId), Array(9...16))
        // Namespace agreement with the engine: automation floats immediately
        // above the interactive pool (worker 16 → automation base 17).
        XCTAssertEqual(WorkersWorkspaceModel.automationSlotBase, 17)
    }

    func testSpawnIntoLowerDecksSlotSucceedsAndRoutesToMainPool() {
        // Slot 9 is Lower Decks slot 1 — the first spillover slot. Before the
        // second page existed it was the automation pool and would not host a
        // main worker; now it must spawn into the main `slots` array and show
        // up under `lowerDecksSlots`, indistinguishable from a Bridge Crew pane.
        let model = WorkersWorkspaceModel()
        let result = model.spawnWorkerPane(makeRequest(slot: 9, runId: "run-ld1"))
        guard case .success(let slotId, _) = result else {
            XCTFail("expected .success spawning Lower Decks slot 9, got \(result)")
            return
        }
        XCTAssertEqual(slotId, 9)
        XCTAssertNotNil(model.slots.first(where: { $0.slotId == 9 })?.session)
        XCTAssertNotNil(
            model.lowerDecksSlots.first(where: { $0.slotId == 9 })?.session,
            "the spawned pane must appear on the Lower Decks page"
        )
        XCTAssertTrue(
            model.bridgeCrewSlots.allSatisfy { $0.session == nil },
            "spawning Lower Decks must not touch any Bridge Crew slot"
        )
    }

    func testSpawnIntoTopLowerDecksSlotSucceeds() {
        // Slot 16 is the last interactive slot. It must be a valid spawn
        // target (it was out of range when the pool capped at 8).
        let model = WorkersWorkspaceModel()
        let result = model.spawnWorkerPane(makeRequest(slot: 16, runId: "run-ld8"))
        guard case .success(let slotId, _) = result else {
            XCTFail("expected .success spawning Lower Decks slot 16, got \(result)")
            return
        }
        XCTAssertEqual(slotId, 16)
    }
}
