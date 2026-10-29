import Foundation
import Testing
@testable import RunnerManagerKit

@MainActor
@Suite struct RunnerManagerModelTests {
    private func loadedModel(_ scenario: DemoRunnerService.Scenario = .populated) async -> RunnerManagerModel {
        let model = RunnerManagerModel(service: DemoRunnerService(scenario: scenario, latency: .zero))
        await model.load()
        return model
    }

    @Test func startsLoadingThenLoadsEverything() async {
        let model = RunnerManagerModel(service: DemoRunnerService(latency: .zero))
        #expect(model.runners == .loading)
        #expect(model.isDemo)
        await model.load()
        #expect(model.machine.value != nil)
        #expect(model.isAssociated)
        #expect(model.runners.value?.count == 3)
    }

    @Test func serviceFailureBecomesAnErrorState() async {
        let model = await loadedModel(.unavailable)
        guard case .failed(let message) = model.runners else {
            Issue.record("expected a failed state")
            return
        }
        #expect(message.contains("unavailable"))
        #expect(!model.isAssociated)
    }

    @Test func associatingUnlocksAddingRunners() async throws {
        let model = await loadedModel(.notAssociated)
        #expect(!model.isAssociated)
        await model.associate()
        #expect(model.isAssociated)
        let runner = try await model.addRunner(NewRunnerRequest(name: "new"))
        #expect(model.runner(runner.id) == runner)
    }

    @Test func commandsUpdateTheListAndRefreshOpenLogs() async throws {
        let model = await loadedModel()
        let running = try #require(model.runners.value?.first { $0.state == .running })
        await model.loadLogs(running.id)
        let before = model.logs[running.id]?.value?.count ?? 0

        await model.stop(running.id)
        #expect(model.runner(running.id)?.state == .stopped)
        #expect(model.logs[running.id]?.value?.count == before + 1)
        #expect(model.busyRunners.isEmpty)

        await model.remove(running.id)
        #expect(model.runner(running.id) == nil)
        #expect(model.logs[running.id] == nil)
    }

    @Test func refusedCommandSetsTheActionError() async throws {
        let model = await loadedModel()
        let running = try #require(model.runners.value?.first { $0.state == .running })
        await model.remove(running.id)
        #expect(model.actionError == "Cannot remove a runner that is running.")
        #expect(model.runner(running.id) != nil)
    }

    @Test func addingADuplicateThrowsWithoutChangingTheList() async {
        let model = await loadedModel()
        await #expect(throws: RunnerServiceError.self) {
            try await model.addRunner(NewRunnerRequest(name: "studio-build"))
        }
        #expect(model.runners.value?.count == 3)
    }
}
