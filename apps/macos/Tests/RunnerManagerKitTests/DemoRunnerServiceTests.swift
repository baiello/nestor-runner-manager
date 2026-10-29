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
        #expect(states == [.running, .stopped, .failed])
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
        let runner = try await demo.addRunner(NewRunnerRequest(name: "first"))
        #expect(runner.state == .running)
    }

    @Test func addedRunnerKeepsItsLimitsAndCanStayStopped() async throws {
        let demo = service(.empty)
        let limits = ResourceLimits(cpuCores: 2, memoryBytes: 8 << 30)
        let runner = try await demo.addRunner(NewRunnerRequest(name: "  ci  ", limits: limits, startNow: false))
        #expect(runner.name == "ci")
        #expect(runner.limits == limits)
        #expect(runner.state == .stopped)
        #expect(try await demo.runners().map(\.id) == [runner.id])
        let logs = try await demo.logs(runnerID: runner.id, limit: 10)
        #expect(logs.allSatisfy { $0.message.hasPrefix("[demo]") })
    }

    @Test func rejectsDuplicateNamesWhateverTheCase() async throws {
        let demo = service()
        await #expect(throws: RunnerServiceError.duplicateName("Studio-Build")) {
            try await demo.addRunner(NewRunnerRequest(name: "Studio-Build"))
        }
    }

    @Test func startStopAndRemoveFollowTheStateRules() async throws {
        let demo = service(.empty)
        let runner = try await demo.addRunner(NewRunnerRequest(name: "a"))

        await #expect(throws: RunnerServiceError.invalidTransition(from: .running, action: "remove")) {
            try await demo.remove(runnerID: runner.id)
        }
        await #expect(throws: RunnerServiceError.invalidTransition(from: .running, action: "start")) {
            try await demo.start(runnerID: runner.id)
        }

        #expect(try await demo.stop(runnerID: runner.id).state == .stopped)
        await #expect(throws: RunnerServiceError.invalidTransition(from: .stopped, action: "stop")) {
            try await demo.stop(runnerID: runner.id)
        }
        #expect(try await demo.start(runnerID: runner.id).state == .running)
        _ = try await demo.stop(runnerID: runner.id)

        try await demo.remove(runnerID: runner.id)
        #expect(try await demo.runners().isEmpty)
        await #expect(throws: RunnerServiceError.runnerNotFound) {
            try await demo.logs(runnerID: runner.id, limit: 10)
        }
    }

    @Test func onlyARunningRunnerIsRunningAJob() async throws {
        let demo = service()
        let runners = try await demo.runners()
        #expect(runners.filter(\.usage.isRunningJob).map(\.state) == [.running])
        let busy = try #require(runners.first { $0.usage.isRunningJob })
        #expect(try await demo.stop(runnerID: busy.id).usage.isRunningJob == false)
    }

    @Test func startingAFailedRunnerClearsItsFailure() async throws {
        let demo = service()
        let failed = try #require(try await demo.runners().first { $0.state == .failed })
        let started = try await demo.start(runnerID: failed.id)
        #expect(started.state == .running)
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
