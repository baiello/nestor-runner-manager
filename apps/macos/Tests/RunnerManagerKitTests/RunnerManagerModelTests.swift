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

    @Test func associatingUnlocksAddingRunnersOnceTheAccountIsConfirmed() async throws {
        let model = await loadedModel(.notAssociated)
        #expect(!model.isAssociated)
        await model.associate()
        // Approved on Nestor: the owner confirms the account first (M1).
        #expect(!model.isAssociated)
        guard case .loaded(.confirming) = model.connection else {
            Issue.record("expected the account to confirm")
            return
        }
        await model.confirmAssociation(accept: true)
        #expect(model.isAssociated)
        let runner = try await model.addRunner(NewRunnerRequest(name: "new"))
        #expect(model.runner(runner.id) == runner)
    }

    @Test func commandsUpdateTheListAndRefreshOpenLogs() async throws {
        let model = await loadedModel()
        let running = try #require(model.runners.value?.first { $0.state == .busy })
        await model.loadLogs(running.id)
        let before = model.logs[running.id]?.value?.count ?? 0

        await model.stop(running.id, force: true)
        #expect(model.runner(running.id)?.state == .stopped)
        #expect(model.logs[running.id]?.value?.count == before + 2)
        #expect(model.busyRunners.isEmpty)

        await model.remove(running.id)
        #expect(model.runner(running.id) == nil)
        #expect(model.logs[running.id] == nil)
    }

    @Test func refusedCommandSetsTheActionError() async throws {
        let model = await loadedModel()
        let running = try #require(model.runners.value?.first { $0.state == .busy })
        await model.remove(running.id)
        #expect(model.actionError == "Cannot remove a runner that is busy.")
        #expect(model.runner(running.id) != nil)
    }

    @Test func addingADuplicateThrowsWithoutChangingTheList() async {
        let model = await loadedModel()
        await #expect(throws: RunnerServiceError.self) {
            try await model.addRunner(NewRunnerRequest(name: "runner-1"))
        }
        #expect(model.runners.value?.count == 3)
    }

    @Test func loadsTheMachineStatusAndChangesSettings() async throws {
        let model = await loadedModel()
        #expect(model.status.value?.maxConcurrentTasks == 1)
        // Raising waits for the user's confirmation; nothing changes before it.
        await model.requestMaxConcurrentTasks(3)
        #expect(model.pendingCeilingRaise == 3)
        #expect(model.status.value?.maxConcurrentTasks == 1)
        await model.confirmCeilingRaise()
        #expect(model.pendingCeilingRaise == nil)
        #expect(model.status.value?.maxConcurrentTasks == 3)
        // Lowering applies at once: it interrupts nothing.
        await model.requestMaxConcurrentTasks(2)
        #expect(model.pendingCeilingRaise == nil)
        #expect(model.status.value?.maxConcurrentTasks == 2)
        await model.setMode(.paused)
        #expect(model.status.value?.admission == .paused)
        #expect(model.status.value?.activeTasks.count == 1, "pausing interrupts nothing")
        #expect(model.actionError == nil)
        #expect(!model.isUpdatingSettings)
    }

    @Test func aConflictReloadsTheStatusAndReportsIt() async throws {
        let service = DemoRunnerService(latency: .zero)
        let model = RunnerManagerModel(service: service)
        await model.load()
        // Another client changes the settings meanwhile.
        _ = try await service.setMaxConcurrentTasks(4, expectedRevision: 0, confirmed: true)
        await model.setMode(.paused)
        #expect(model.actionError?.contains("changed elsewhere") == true)
        #expect(model.status.value?.maxConcurrentTasks == 4)
        #expect(model.status.value?.mode == .automatic)
    }

    @Test func disassociatingEndsTheAssociationAfterConfirmation() async throws {
        let model = await loadedModel()
        #expect(model.isAssociated)
        #expect(model.access == .demo)
        #expect(model.canChange)
        await model.disassociate()
        #expect(model.connection == .loaded(.notAssociated))
        #expect(model.runners.value?.isEmpty == true)
        #expect(model.status.value?.admission == .blocked(.notAssociated))
    }

    @Test func demoBannerFollowsTheServiceReport() async {
        let model = await loadedModel()
        #expect(model.isDemo)
        // A live client that cannot reach its service has no report: no banner, but nothing
        // loads either.
        let live = RunnerManagerModel(service: LiveRunnerService(socketPath: "/nonexistent/service.sock", timeout: 1))
        await live.load()
        #expect(live.status.value == nil)
        #expect(!live.isDemo)
    }
}
