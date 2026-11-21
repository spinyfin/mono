import AppKit
import Foundation
import SQLite3
import SwiftUI
import XCTest
@testable import Boss

/// The real engine owns projection and answers; the app consumes its socket
/// replies. Only worker/GitHub observations are injected into the private DB.
/// No worker, GitHub API, production socket or production database is used.
@MainActor
final class NavigatorLifecycleIntegrationTests: XCTestCase {
    func testRevisionQuestionReviewMergeKeepsRootAndDraft() async throws {
        let fixture = try NavigatorEngineFixture()
        defer { fixture.stop() }
        try fixture.sql("""
            INSERT INTO products(id,name,slug,status,created_at,updated_at,auto_pr_maintenance_enabled)
            VALUES ('prod_nav','Navigator lifecycle','navigator-lifecycle','active','100','100',0);
            INSERT INTO tasks(id,product_id,kind,name,description,status,autostart,created_at,updated_at)
            VALUES ('task_nav_root','prod_nav','chore','Preserve selection','Original brief','active',0,'100','100');
            INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at)
            VALUES ('nav-original','task_nav_root','task_implementation','running','','100');
            INSERT INTO work_runs(id,execution_id,agent_id,status,created_at)
            VALUES ('nav-original-run','nav-original','fixture','active','100');
            """)
        let defaults = UserDefaults(suiteName: "NavigatorLifecycle.\(UUID().uuidString)")!
        let model = ChatViewModel(socketPath: fixture.socket, defaults: defaults)
        model.selectedWorkProductID = "prod_nav"
        model.engine.start()
        defer { model.engine.stop() }
        try await wait("app connected") { model.isConnected }
        // connect() succeeds as soon as the socket is bound. The engine does
        // not accept until post-bind startup finishes, so wait for a real
        // reply before asserting navigator projection.
        try await wait("product listed") { model.products.contains { $0.id == "prod_nav" } }
        try await refresh(model, section: .working)
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("task_nav_root")
        try assertWorkerActivity(model, executionID: "nav-original", taskID: "task_nav_root")
        let original = try XCTUnwrap(model.navigatorSelectedTask)
        model.beginNavigatorBriefEdit(original)
        model.setNavigatorBriefText(original.id, text: "Unsaved original brief")

        try fixture.sql("""
            UPDATE work_executions SET status='completed' WHERE id='nav-original';
            UPDATE work_runs SET status='completed' WHERE id='nav-original-run';
            UPDATE tasks SET status='in_review',pr_url='https://github.com/spinyfin/mono/pull/7',
              pr_head_sha='fixture-head',ci_required_state='success',pr_mergeable_state='mergeable'
              WHERE id='task_nav_root';
            INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at)
            VALUES ('nav-review','task_nav_root','pr_review','completed','','101');
            INSERT INTO pr_review_verdicts(id,execution_id,work_item_id,head_sha,findings_count,revision_warranted,gate_outcome,created_at)
            VALUES ('nav-verdict','nav-review','task_nav_root','fixture-head',0,0,'completed_clean','101');
            """)
        try await refresh(model, section: .ready)
        XCTAssertTrue(model.navigator.tabs.contains(.review))

        try fixture.sql("""
            INSERT INTO tasks(id,product_id,kind,parent_task_id,name,description,status,created_via,autostart,created_at,updated_at)
            VALUES ('task_nav_revision','prod_nav','revision','task_nav_root','Repair CI','Current CI ask','active','ci-fix:fixture',0,'102','102');
            INSERT INTO work_executions(id,work_item_id,kind,status,repo_remote_url,created_at)
            VALUES ('nav-repair','task_nav_revision','revision_implementation','running','','102');
            INSERT INTO work_runs(id,execution_id,agent_id,status,created_at)
            VALUES ('nav-repair-run','nav-repair','fixture','active','102');
            """)
        try await refresh(model, section: .working)
        try assertWorkerActivity(model, executionID: "nav-repair", taskID: "task_nav_revision")
        XCTAssertEqual(model.navigator.askID, "task_nav_revision")
        model.setNavigatorDocument(.currentAsk, rootID: "task_nav_root")
        XCTAssertEqual(model.navigatorDocumentTask(for: try XCTUnwrap(model.navigatorSelectedTask)).description, "Current CI ask")
        XCTAssertEqual(model.navigatorSelectedActivity(for: try XCTUnwrap(model.navigatorSelectedTask))?.taskId, "task_nav_root",
                       "a new run must not redirect the previously selected terminal")
        model.setNavigatorActivity(executionID: "nav-repair", rootID: "task_nav_root")
        XCTAssertEqual(model.navigatorSelectedActivity(for: try XCTUnwrap(model.navigatorSelectedTask))?.taskId, "task_nav_revision")
        model.navigator.tab = .review
        let host = NSHostingView(rootView: WorkNavigatorView(model: model).frame(width: 1200, height: 800))
        host.frame = NSRect(x: 0, y: 0, width: 1200, height: 800)
        try await capture(host, name: "revision")

        try fixture.sql("""
            UPDATE tasks SET status='blocked',blocked_reason='awaiting_operator_answer',parked_at='103' WHERE id='task_nav_revision';
            UPDATE work_executions SET status='waiting_human' WHERE id='nav-repair';
            INSERT INTO operator_questions(id,work_item_id,execution_id,proposal_id,question_json,status,created_at)
            VALUES ('oq_nav','task_nav_revision','nav-repair','fixture-proposal',
              '{"text":"Keep the current keyboard shortcuts?","answer_type":{"kind":"yes_no"},"explanation":"The revision needs a choice."}','open','103');
            """)
        try await refresh(model, section: .needsAttention)
        let revision = try XCTUnwrap(model.task(withID: "task_nav_revision"))
        XCTAssertNotNil(revision.operatorQuestion)
        XCTAssertTrue(WorkNavigatorAttention.entries(model: model, root: try XCTUnwrap(model.navigatorSelectedTask))
            .contains { $0.id == "question:oq_nav" })
        XCTAssertEqual(model.navigator.tab, .review, "a new question must not replace the user's tab")
        model.navigator.tab = .attention
        try await capture(host, name: "question")
        XCTAssertTrue(model.answerOperatorQuestion(for: revision, answer: .yesNo(true)))
        try await wait("answer accepted") {
            model.operatorAnswerInFlightByTaskID[revision.id] == nil
        }
        XCTAssertNil(model.operatorAnswerErrorByTaskID[revision.id])
        XCTAssertEqual(try fixture.scalar("SELECT status FROM operator_questions WHERE id='oq_nav'"), "answered")

        try fixture.sql("""
            UPDATE tasks SET status='done',parked_at=NULL WHERE id='task_nav_revision';
            UPDATE work_executions SET status='completed' WHERE work_item_id='task_nav_revision';
            UPDATE work_runs SET status='completed' WHERE id='nav-repair-run';
            """)
        try await refresh(model, section: .ready)
        model.navigator.tab = .review
        try await capture(host, name: "review")
        try fixture.sql("UPDATE tasks SET merge_queue_state='queued' WHERE id='task_nav_root'")
        try await refresh(model, section: .backlog)
        XCTAssertFalse(try XCTUnwrap(model.navigatorSelectedTask).isMergeWhenReadyEligible)
        try await capture(host, name: "merging")

        // A persisted observation stands in for the external GitHub poller.
        try fixture.sql("""
            INSERT INTO pr_merge_evidence(root_task_id,canonical_pr_url,observed_at)
            VALUES ('task_nav_root','https://github.com/spinyfin/mono/pull/7','104');
            UPDATE tasks SET status='done',merge_queue_state=NULL WHERE id='task_nav_root';
            """)
        try await refresh(model, section: .landed)
        XCTAssertEqual(model.navigator.tab, .review)
        XCTAssertNotNil(model.navigatorSelectedTask?.navigator?.root?.mergeEvidence)
        XCTAssertEqual(model.navigatorBriefText(for: original), "Unsaved original brief")
        XCTAssertFalse(model.navigatorBriefCanEdit(try XCTUnwrap(model.navigatorSelectedTask)))
        XCTAssertEqual(model.navigatorFamily(of: try XCTUnwrap(model.navigatorSelectedTask)).map(\.id), ["task_nav_root", "task_nav_revision"])
        try await capture(host, name: "landed")
        // Exercise content reply correlation through the real transport too.
        model.loadNavigatorReviewGuide(for: try XCTUnwrap(model.navigatorSelectedTask), versionID: "unavailable-version")
        try await wait("identified missing-guide reply") {
            model.navigatorReviewGuides.readers["task_nav_root"]?.state == .failed("This review guide is no longer available.")
        }
    }

    private func assertWorkerActivity(_ model: ChatViewModel, executionID: String, taskID: String) throws {
        let root = try XCTUnwrap(model.navigatorSelectedTask)
        let projection = try XCTUnwrap(root.navigator?.root)
        let activity = try XCTUnwrap(projection.activities.first { $0.executionId == executionID })
        XCTAssertEqual(activity.taskId, taskID)
        XCTAssertEqual(activity.runId, executionID + "-run")
        XCTAssertNotEqual(activity.runId, activity.executionId)
        let row = NavigatorRow(task: root, projection: projection, matchingAskID: nil)
        let observed = NavigatorRowLiveState(model: model, row: row)
        // Worker registration uses execution.id, while work_runs has its own spawn id.
        // Feed the wire parser and observable store, not a hand-keyed lookup dictionary.
        for (workerID, status, expected) in [
            (executionID, "working", true), (executionID, "idle", false),
            (executionID, "waiting_for_input", false), ("unrelated-execution", "working", false),
            (executionID, "working", true),
        ] {
            let state = try XCTUnwrap(model.engine.parseWorkerLiveState([
                "slot_id": 1, "run_id": workerID, "model": "fixture",
                "activity": status, "work_item_id": taskID,
            ]))
            model.liveWorkerStates.update(states: [state])
            model.presentationUpdates.flush()
            XCTAssertEqual(observed.isWorkerActive, expected, "\(taskID): \(workerID) \(status)")
        }
        model.liveWorkerStates.update(states: [])
        model.presentationUpdates.flush()
        XCTAssertFalse(observed.isWorkerActive)
    }

    private func refresh(_ model: ChatViewModel, section: NavigatorSection) async throws {
        let before = Set(model.boardReadFences["prod_nav"]?.trees.keys.map { $0 } ?? [])
        model.engine.sendGetWorkTree(productId: "prod_nav", flow: .manualRefresh)
        let after = Set(model.boardReadFences["prod_nav"]?.trees.keys.map { $0 } ?? [])
        let requestID = try XCTUnwrap(after.subtracting(before).first)
        try await wait("engine projected \(section)") {
            model.boardReadFences["prod_nav"]?.trees[requestID] == nil
                && model.task(withID: "task_nav_root")?.navigator?.root?.knownSection == section
        }
        if model.navigator.selectedRootID != nil { XCTAssertEqual(model.navigator.selectedRootID, "task_nav_root") }
    }

    private func wait(_ label: String, until predicate: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(60)
        while !predicate(), Date() < deadline { try await Task.sleep(for: .milliseconds(20)) }
        guard predicate() else { throw NSError(domain: "NavigatorLifecycle", code: 1, userInfo: [NSLocalizedDescriptionKey: label]) }
    }

    private func capture(_ host: NSView, name: String) async throws {
        host.appearance = NSAppearance(named: .aqua)
        for _ in 0..<3 {
            host.layoutSubtreeIfNeeded()
            try await Task.sleep(for: .milliseconds(20))
        }
        host.layoutSubtreeIfNeeded()
        let bitmap = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: bitmap)
        let data = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
        XCTAssertGreaterThan(data.count, 100)
        let output = try XCTUnwrap(ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"])
        try data.write(to: URL(fileURLWithPath: "\(output)/navigator-lifecycle-\(name).png"))
    }
}

private final class NavigatorEngineFixture {
    let socket: String
    private let db: String
    private let process = Process()
    private let log: FileHandle
    private let stdout: FileHandle

    init() throws {
        let env = ProcessInfo.processInfo.environment
        let root = try XCTUnwrap(env["TEST_TMPDIR"])
        let runfiles = try XCTUnwrap(env["TEST_SRCDIR"])
        socket = "\(root)/navigator.sock"
        db = "\(root)/navigator.db"
        let logPath = "\(root)/navigator-engine.log"
        FileManager.default.createFile(atPath: logPath, contents: nil)
        log = try FileHandle(forWritingTo: URL(fileURLWithPath: logPath))
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: "\(runfiles)/_main/tools/boss/engine/core/engine_fixture_test")
        process.arguments = ["--socket-path", socket]
        var environment = env
        for key in ["BOSS_EVENTS_SOCKET", "BOSS_DB_PATH", "BOSS_PID_PATH", "BOSS_CONTROL_TOKEN_PATH"] {
            environment.removeValue(forKey: key)
        }
        process.environment = environment
        process.standardOutput = output
        process.standardError = log
        try process.run()
        stdout = output.fileHandleForReading
        try output.fileHandleForWriting.close()
        do {
            try EngineFixtureReadiness.waitForListening(socket: socket, output: stdout)
        } catch {
            // init throws, so the caller never gets a fixture whose defer can
            // stop the process. Tear down here before wrapping the error.
            stop()
            let logText = (try? String(contentsOfFile: logPath, encoding: .utf8)) ?? "engine log unavailable"
            throw NSError(domain: "NavigatorFixture", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "\(error.localizedDescription): \(logText)",
            ])
        }
    }

    func stop() {
        if process.isRunning { process.terminate() }
        process.waitUntilExit()
        try? stdout.close()
        try? log.close()
    }

    func sql(_ statement: String) throws {
        try connection { connection in
            guard sqlite3_exec(connection, statement, nil, nil, nil) == SQLITE_OK else {
                throw NSError(domain: "NavigatorFixtureSQL", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: String(cString: sqlite3_errmsg(connection)),
                ])
            }
        }
    }

    func scalar(_ sql: String) throws -> String? {
        try connection { connection in
            var statement: OpaquePointer?
            guard sqlite3_prepare_v2(connection, sql, -1, &statement, nil) == SQLITE_OK else {
                throw NSError(domain: "NavigatorFixtureSQL", code: 2)
            }
            defer { sqlite3_finalize(statement) }
            guard sqlite3_step(statement) == SQLITE_ROW, let text = sqlite3_column_text(statement, 0) else { return nil }
            return String(cString: text)
        }
    }

    private func connection<T>(_ body: (OpaquePointer) throws -> T) throws -> T {
        var connection: OpaquePointer?
        guard sqlite3_open_v2(db, &connection, SQLITE_OPEN_READWRITE, nil) == SQLITE_OK, let connection else {
            throw NSError(domain: "NavigatorFixtureSQL", code: 3)
        }
        defer { sqlite3_close(connection) }
        sqlite3_busy_timeout(connection, 5000)
        return try body(connection)
    }
}
