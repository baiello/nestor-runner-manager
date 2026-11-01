import Foundation
import Testing
@testable import RunnerManagerKit

@Suite struct DemoRunnerServiceTests {
    private func service(_ scenario: DemoRunnerService.Scenario = .populated) -> DemoRunnerService {
        DemoRunnerService(scenario: scenario, latency: .zero)
    }

    @Test func declaresItselfAsDemo() {
        #expect(service().mode == .demo)
    }

    @Test func populatedScenarioHasRunnersInEveryStableState() async throws {
        let states = Set(try await service().runners().map(\.state))
        #expect(states == [.busy, .stopped, .failed])
    }

    @Test func emptyScenarioIsAssociatedWithoutRunners() async throws {
        let demo = service(.empty)
        #expect(try await demo.runners().isEmpty)
        guard case .associated = try await demo.connection() else {
            Issue.record("expected an associated machine")
            return
        }
    }

    @Test func addingRequiresAnAssociation() async throws {
        let demo = service(.notAssociated)
        await #expect(throws: RunnerServiceError.notAssociated) {
            try await demo.addRunner(NewRunnerRequest(name: "first"))
        }
        _ = try await demo.associate()
        // Approved, not confirmed: still nothing (M1).
        await #expect(throws: RunnerServiceError.self) {
            try await demo.addRunner(NewRunnerRequest(name: "first"))
        }
        _ = try await demo.confirmAssociation(accept: true)
        let runner = try await demo.addRunner(NewRunnerRequest(name: "first"))
        #expect(runner.state == .ready)
    }

    @Test func theOwnerConfirmsTheAccountOrTheMachineIsDisassociated() async throws {
        let demo = service(.notAssociated)
        guard case .confirming(_, let account, let approver) = try await demo.associate() else {
            Issue.record("expected the account to confirm")
            return
        }
        #expect(account == "o***r@example.com" && approver == "a***n@example.com")
        #expect(try await demo.machineStatus().admission == .blocked(.awaitingConfirmation))
        #expect(try await demo.confirmAssociation(accept: false) == .notAssociated)
        await #expect(throws: RunnerServiceError.self) { try await demo.confirmAssociation(accept: true) }
    }

    @Test func aJobWithoutClaudeSignedInShowsOnTheMachine() async throws {
        #expect(try await service(.claudeNotSignedIn).machineStatus().claude == .notSignedIn)
        #expect(try await service().machineStatus().claude == .unknown)
    }

    @Test func addedRunnerCanStayStopped() async throws {
        let demo = service(.empty)
        let runner = try await demo.addRunner(NewRunnerRequest(name: "  ci  ", startNow: false))
        #expect(runner.name == "ci")
        #expect(runner.usage == .idle)
        #expect(runner.state == .stopped)
        #expect(try await demo.runners().map(\.id) == [runner.id])
        let logs = try await demo.logs(runnerID: runner.id, limit: 10)
        #expect(logs.allSatisfy { $0.message.hasPrefix("[demo]") })
    }

    @Test func rejectsDuplicateNamesWhateverTheCase() async throws {
        let demo = service()
        await #expect(throws: RunnerServiceError.duplicateName("Runner-1")) {
            try await demo.addRunner(NewRunnerRequest(name: "Runner-1"))
        }
    }

    @Test func startStopAndRemoveFollowTheStateRules() async throws {
        let demo = service(.empty)
        let runner = try await demo.addRunner(NewRunnerRequest(name: "a"))

        await #expect(throws: RunnerServiceError.invalidTransition(from: .ready, action: "remove")) {
            try await demo.remove(runnerID: runner.id)
        }
        await #expect(throws: RunnerServiceError.invalidTransition(from: .ready, action: "start")) {
            try await demo.start(runnerID: runner.id)
        }

        #expect(try await demo.stop(runnerID: runner.id).state == .stopped)
        await #expect(throws: RunnerServiceError.invalidTransition(from: .stopped, action: "stop")) {
            try await demo.stop(runnerID: runner.id)
        }
        #expect(try await demo.start(runnerID: runner.id).state == .ready)
        _ = try await demo.stop(runnerID: runner.id)

        try await demo.remove(runnerID: runner.id)
        #expect(try await demo.runners().isEmpty)
        await #expect(throws: RunnerServiceError.runnerNotFound) {
            try await demo.logs(runnerID: runner.id, limit: 10)
        }
    }

    @Test func onlyABusyRunnerIsRunningATaskAndStoppingItNeedsForce() async throws {
        let demo = service()
        let runners = try await demo.runners()
        #expect(runners.filter(\.usage.isRunningJob).map(\.state) == [.busy])
        let busy = try #require(runners.first { $0.usage.isRunningJob })
        await #expect(throws: RunnerServiceError.invalidTransition(from: .busy, action: "stop")) {
            try await demo.stop(runnerID: busy.id)
        }
        let stopped = try await demo.stop(runnerID: busy.id, force: true)
        #expect(stopped.usage.isRunningJob == false)
        #expect(stopped.currentTaskRef == nil)
        #expect(try await demo.machineStatus().activeTasks.isEmpty)
    }

    @Test func machineStartsWithOneTaskAtATimeAndNoLimitField() async throws {
        let status = try await service().machineStatus()
        #expect(status.serviceMode == .demo)
        #expect(status.maxConcurrentTasks == 1)
        #expect(status.mode == .automatic)
        #expect(status.admission == .open)
        #expect(status.activeTasks.map(\.taskRef) == ["demo-task-128"])
        let notAssociated = try await service(.notAssociated).machineStatus()
        #expect(notAssociated.admission == .blocked(.notAssociated))
    }

    @Test func pauseAndLowerCeilingKeepRunningTasks() async throws {
        let demo = service()
        await #expect(throws: RunnerServiceError.self) {
            try await demo.setMaxConcurrentTasks(3, expectedRevision: 0)
        }
        var status = try await demo.setMaxConcurrentTasks(3, expectedRevision: 0, confirmed: true)
        #expect(status.maxConcurrentTasks == 3)
        status = try await demo.setMode(.paused, expectedRevision: status.configRevision)
        #expect(status.admission == .paused)
        #expect(status.activeTasks.count == 1)
        status = try await demo.setMaxConcurrentTasks(1, expectedRevision: status.configRevision)
        #expect(status.activeTasks.count == 1)
        #expect(try await demo.runners().contains { $0.state == .busy })
    }

    @Test func settingsRejectStaleRevisionsAndOutOfRangeValues() async throws {
        let demo = service()
        _ = try await demo.setMode(.paused, expectedRevision: 0)
        await #expect(throws: RunnerServiceError.conflict(currentRevision: 1)) {
            try await demo.setMode(.automatic, expectedRevision: 0)
        }
        await #expect(throws: RunnerServiceError.limitOutOfRange(min: 1, max: 24)) {
            try await demo.setMaxConcurrentTasks(0, expectedRevision: 1)
        }
        await #expect(throws: RunnerServiceError.limitOutOfRange(min: 1, max: 24)) {
            try await demo.setMaxConcurrentTasks(25, expectedRevision: 1, confirmed: true)
        }
    }

    @Test func startingAFailedRunnerClearsItsFailure() async throws {
        let demo = service()
        let failed = try #require(try await demo.runners().first { $0.state == .failed })
        let started = try await demo.start(runnerID: failed.id)
        #expect(started.state == .ready)
        #expect(started.failureReason == nil)
    }

    @Test func logsAreCappedToTheMostRecent() async throws {
        let demo = service()
        let runner = try #require(try await demo.runners().first)
        let all = try await demo.logs(runnerID: runner.id, limit: 100)
        let last = try await demo.logs(runnerID: runner.id, limit: 2)
        #expect(last == Array(all.suffix(2)))
        #expect(try await demo.logs(runnerID: runner.id, limit: -1).isEmpty)
    }

    @Test func unavailableScenarioFailsEveryCall() async {
        let demo = service(.unavailable)
        await #expect(throws: RunnerServiceError.self) { try await demo.machineInfo() }
        await #expect(throws: RunnerServiceError.self) { try await demo.runners() }
    }
}

@Suite struct RunnerNameRuleTests {
    @Test(arguments: ["studio-build", "Mac Studio 1", "ci_runner.2"])
    func acceptsValidNames(_ name: String) throws {
        #expect(try RunnerNameRule.validate(name) == name)
    }

    @Test(arguments: ["", "   ", "a/b", "rm -rf ~", String(repeating: "x", count: 41)])
    func rejectsInvalidNames(_ name: String) {
        #expect(throws: RunnerServiceError.self) { try RunnerNameRule.validate(name) }
    }
}
