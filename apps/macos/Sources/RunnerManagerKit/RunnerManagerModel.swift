import Foundation
import Observation

/// A value being fetched from the service.
public enum Loadable<Value: Equatable & Sendable>: Equatable, Sendable {
    case loading
    case loaded(Value)
    case failed(String)

    public var value: Value? {
        if case .loaded(let value) = self { value } else { nil }
    }
}

/// The interface state, kept apart from the views so it can be tested without SwiftUI.
@MainActor
@Observable
public final class RunnerManagerModel {
    public let service: any RunnerService

    public private(set) var machine: Loadable<MachineInfo> = .loading
    public private(set) var connection: Loadable<NestorConnection> = .loading
    public private(set) var runners: Loadable<[Runner]> = .loading
    public private(set) var logs: [Runner.ID: Loadable<[LogEntry]>] = [:]
    /// Runners with a command in flight; their actions are disabled meanwhile.
    public private(set) var busyRunners: Set<Runner.ID> = []
    public private(set) var isAssociating = false
    /// The last failed command, shown as an alert.
    public var actionError: String?

    public static let logLimit = 200

    public init(service: any RunnerService) {
        self.service = service
    }

    public var isDemo: Bool { service.mode == .demo }

    public var isAssociated: Bool {
        if case .loaded(.associated) = connection { true } else { false }
    }

    public func runner(_ id: Runner.ID) -> Runner? {
        runners.value?.first { $0.id == id }
    }

    /// Loads the machine, the connection and the runners together.
    public func load() async {
        machine = .loading
        connection = .loading
        runners = .loading
        async let machineResult = capture { try await self.service.machineInfo() }
        async let connectionResult = capture { try await self.service.connection() }
        async let runnersResult = capture { try await self.service.runners() }
        machine = await machineResult
        connection = await connectionResult
        runners = await runnersResult
    }

    public func associate() async {
        isAssociating = true
        defer { isAssociating = false }
        do {
            connection = .loaded(try await service.associate())
        } catch {
            actionError = Self.message(for: error)
        }
    }

    /// Adds a runner. Throws so the add sheet can show the error next to the form.
    @discardableResult
    public func addRunner(_ request: NewRunnerRequest) async throws -> Runner {
        let runner = try await service.addRunner(request)
        if case .loaded(var list) = runners {
            list.append(runner)
            runners = .loaded(list)
        } else {
            runners = await capture { try await self.service.runners() }
        }
        return runner
    }

    public func start(_ id: Runner.ID) async {
        await perform(on: id) { try await self.service.start(runnerID: id) }
    }

    public func stop(_ id: Runner.ID) async {
        await perform(on: id) { try await self.service.stop(runnerID: id) }
    }

    public func remove(_ id: Runner.ID) async {
        guard !busyRunners.contains(id) else { return }
        busyRunners.insert(id)
        defer { busyRunners.remove(id) }
        do {
            try await service.remove(runnerID: id)
            if case .loaded(var list) = runners {
                list.removeAll { $0.id == id }
                runners = .loaded(list)
            }
            logs[id] = nil
        } catch {
            actionError = Self.message(for: error)
        }
    }

    public func loadLogs(_ id: Runner.ID) async {
        if logs[id] == nil { logs[id] = .loading }
        logs[id] = await capture { try await self.service.logs(runnerID: id, limit: Self.logLimit) }
    }

    private func perform(on id: Runner.ID, _ command: @escaping () async throws -> Runner) async {
        guard !busyRunners.contains(id) else { return }
        busyRunners.insert(id)
        defer { busyRunners.remove(id) }
        do {
            let updated = try await command()
            replace(updated)
            if logs[id] != nil { await loadLogs(id) }
        } catch {
            actionError = Self.message(for: error)
        }
    }

    private func replace(_ runner: Runner) {
        guard case .loaded(var list) = runners, let index = list.firstIndex(where: { $0.id == runner.id }) else { return }
        list[index] = runner
        runners = .loaded(list)
    }

    private func capture<Value>(_ call: () async throws -> Value) async -> Loadable<Value> {
        do {
            return .loaded(try await call())
        } catch {
            return .failed(Self.message(for: error))
        }
    }

    static func message(for error: any Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }
}
