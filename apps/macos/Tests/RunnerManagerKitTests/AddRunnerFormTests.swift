import Foundation
import Testing
@testable import RunnerManagerKit

/// Holds `addRunner` until the test releases it, to look at the form while a creation is in flight.
private actor GatedRunnerService: RunnerService {
    nonisolated let mode: ServiceMode = .demo
    private let demo = DemoRunnerService(scenario: .empty, latency: .zero)
    private var waiting: CheckedContinuation<Void, Never>?
    private var released = false

    func release() {
        released = true
        waiting?.resume()
        waiting = nil
    }

    func machineInfo() async throws -> MachineInfo { try await demo.machineInfo() }
    func connection() async throws -> NestorConnection { try await demo.connection() }
    func associate() async throws -> NestorConnection { try await demo.associate() }
    func machineStatus() async throws -> MachineStatus { try await demo.machineStatus() }
    func setMode(_ mode: AdmissionMode, expectedRevision: Int) async throws -> MachineStatus {
        try await demo.setMode(mode, expectedRevision: expectedRevision)
    }
    func setMaxConcurrentTasks(_ value: Int, expectedRevision: Int) async throws -> MachineStatus {
        try await demo.setMaxConcurrentTasks(value, expectedRevision: expectedRevision)
    }
    func runners() async throws -> [Runner] { try await demo.runners() }
    func addRunner(_ request: NewRunnerRequest) async throws -> Runner {
        if !released {
            await withCheckedContinuation { waiting = $0 }
        }
        return try await demo.addRunner(request)
    }
    func start(runnerID: Runner.ID) async throws -> Runner { try await demo.start(runnerID: runnerID) }
    func stop(runnerID: Runner.ID, force: Bool) async throws -> Runner { try await demo.stop(runnerID: runnerID, force: force) }
    func remove(runnerID: Runner.ID) async throws { try await demo.remove(runnerID: runnerID) }
    func logs(runnerID: Runner.ID, limit: Int) async throws -> [LogEntry] { try await demo.logs(runnerID: runnerID, limit: limit) }
}

@MainActor
@Suite struct AddRunnerFormTests {
    @Test func cannotBeCancelledWhileTheCreationIsInFlight() async throws {
        let service = GatedRunnerService()
        let model = RunnerManagerModel(service: service)
        await model.load()
        let form = AddRunnerForm()
        form.name = "studio-ci"
        #expect(form.canDismiss)

        let submission = Task { await form.submit(to: model) }
        while !form.isSubmitting { await Task.yield() }

        // Add → Cancel while waiting: Cancel, Escape and the other dismissals are refused,
        // and a second submission is not sent.
        #expect(!form.canDismiss)
        #expect(!form.canSubmit)
        #expect(await form.submit(to: model) == nil)

        await service.release()
        let runner = try #require(await submission.value)

        #expect(form.canDismiss)
        #expect(form.created == runner)
        #expect(model.runners.value?.map(\.name) == ["studio-ci"])
    }

    @Test func failedCreationLetsTheUserCancelOrRetry() async {
        let model = RunnerManagerModel(service: DemoRunnerService(latency: .zero))
        await model.load()
        let form = AddRunnerForm()
        form.name = "runner-1"

        #expect(await form.submit(to: model) == nil)
        #expect(form.error == "A runner named “runner-1” already exists on this machine.")
        #expect(form.canDismiss)
        #expect(form.canSubmit)
        #expect(model.runners.value?.count == 3)
    }

    @Test func buildsTheRequestFromTheNameAndStartChoice() {
        let form = AddRunnerForm()
        form.name = "ci"
        form.startNow = false
        #expect(form.request == NewRunnerRequest(name: "ci", startNow: false))
    }

    @Test func invalidNameBlocksSubmission() {
        let form = AddRunnerForm()
        #expect(!form.canSubmit)
        #expect(form.nameError == nil)
        form.name = "a/b"
        #expect(form.nameError != nil)
        #expect(!form.canSubmit)
    }
}
