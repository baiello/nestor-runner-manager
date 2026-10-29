import Foundation

/// A `RunnerService` backed by simulated data.
///
/// DEMO ONLY: no command reaches Nestor or a runner, nothing is installed, and no secret is
/// created or stored. State lives in memory and is lost when the app quits.
public actor DemoRunnerService: RunnerService {
    public enum Scenario: String, Sendable, CaseIterable {
        /// Associated with a demo Nestor instance, with a few runners.
        case populated
        /// Associated, no runner yet.
        case empty
        /// Not associated with Nestor yet.
        case notAssociated
        /// Every call fails, to show the error states.
        case unavailable
    }

    public nonisolated let mode: ServiceMode = .demo

    private let latency: Duration
    private let now: @Sendable () -> Date
    private var scenario: Scenario
    private var connectionState: NestorConnection
    private var runnerList: [Runner]
    private var logsByRunner: [Runner.ID: [LogEntry]]
    private var admissionMode: AdmissionMode = .automatic
    /// One task at a time by default, as a new machine starts.
    private var maxConcurrentTasks = 1
    private var configRevision = 0

    public static let demoInstance = "demo.nestor.invalid"
    public static let cpuCores = 24

    public init(
        scenario: Scenario = .populated,
        latency: Duration = .milliseconds(450),
        now: @escaping @Sendable () -> Date = { Date() }
    ) {
        self.scenario = scenario
        self.latency = latency
        self.now = now
        let start = now()
        switch scenario {
        case .populated:
            connectionState = .associated(instance: Self.demoInstance, since: start.addingTimeInterval(-86_400 * 3))
            let seeded = Self.seedRunners(at: start)
            runnerList = seeded.map(\.runner)
            logsByRunner = Dictionary(uniqueKeysWithValues: seeded.map { ($0.runner.id, $0.logs) })
        case .empty, .unavailable:
            connectionState = .associated(instance: Self.demoInstance, since: start.addingTimeInterval(-3_600))
            runnerList = []
            logsByRunner = [:]
        case .notAssociated:
            connectionState = .notAssociated
            runnerList = []
            logsByRunner = [:]
        }
    }

    // MARK: RunnerService

    public func machineInfo() async throws -> MachineInfo {
        try await simulateCall()
        return MachineInfo(
            name: "Demo Mac Studio",
            model: "Mac Studio (simulated)",
            architecture: "arm64",
            operatingSystem: "macOS",
            cpuCores: Self.cpuCores,
            memoryBytes: 128 * Self.gib,
            diskFreeBytes: 1_200 * Self.gib
        )
    }

    public func connection() async throws -> NestorConnection {
        try await simulateCall()
        return connectionState
    }

    public func associate() async throws -> NestorConnection {
        try await simulateCall()
        connectionState = .associated(instance: Self.demoInstance, since: now())
        return connectionState
    }

    public func machineStatus() async throws -> MachineStatus {
        try await simulateCall()
        return status()
    }

    public func setMode(_ mode: AdmissionMode, expectedRevision: Int) async throws -> MachineStatus {
        try await simulateCall()
        try checkRevision(expectedRevision)
        admissionMode = mode
        configRevision += 1
        return status()
    }

    public func setMaxConcurrentTasks(_ value: Int, expectedRevision: Int) async throws -> MachineStatus {
        try await simulateCall()
        try checkRevision(expectedRevision)
        guard (1...Self.cpuCores).contains(value) else {
            throw RunnerServiceError.limitOutOfRange(min: 1, max: Self.cpuCores)
        }
        // Lowering the ceiling interrupts nothing: busy runners keep their task.
        maxConcurrentTasks = value
        configRevision += 1
        return status()
    }

    public func runners() async throws -> [Runner] {
        try await simulateCall()
        return runnerList
    }

    public func addRunner(_ request: NewRunnerRequest) async throws -> Runner {
        try await simulateCall()
        guard case .associated = connectionState else { throw RunnerServiceError.notAssociated }
        let name = try RunnerNameRule.validate(request.name)
        if runnerList.contains(where: { $0.name.caseInsensitiveCompare(name) == .orderedSame }) {
            throw RunnerServiceError.duplicateName(name)
        }
        let created = now()
        let runner = Runner(
            name: name,
            state: request.startNow ? .ready : .stopped,
            createdAt: created
        )
        runnerList.append(runner)
        var logs = [LogEntry(date: created, level: .info, message: "[demo] Runner created (simulated, no credential issued).")]
        if request.startNow {
            logs.append(LogEntry(date: created, level: .info, message: "[demo] Runner started (simulated)."))
        }
        logsByRunner[runner.id] = logs
        return runner
    }

    public func start(runnerID: Runner.ID) async throws -> Runner {
        try await simulateCall()
        let index = try indexOf(runnerID)
        let state = runnerList[index].state
        guard state.canStart else { throw RunnerServiceError.invalidTransition(from: state, action: "start") }
        runnerList[index].state = .ready
        runnerList[index].failureReason = nil
        appendLog(runnerID, .info, "[demo] Runner started (simulated).")
        return runnerList[index]
    }

    public func stop(runnerID: Runner.ID, force: Bool) async throws -> Runner {
        try await simulateCall()
        let index = try indexOf(runnerID)
        let state = runnerList[index].state
        guard state.canStop, state != .busy || force else {
            throw RunnerServiceError.invalidTransition(from: state, action: "stop")
        }
        if let task = runnerList[index].currentTaskRef {
            appendLog(runnerID, .warning, "[demo] Task \(task) ended by the stop (simulated).")
        }
        runnerList[index].state = .stopped
        runnerList[index].usage = .idle
        runnerList[index].currentTaskRef = nil
        appendLog(runnerID, .info, "[demo] Runner stopped (simulated).")
        return runnerList[index]
    }

    public func remove(runnerID: Runner.ID) async throws {
        try await simulateCall()
        let index = try indexOf(runnerID)
        let state = runnerList[index].state
        guard state.canRemove else { throw RunnerServiceError.invalidTransition(from: state, action: "remove") }
        runnerList.remove(at: index)
        logsByRunner[runnerID] = nil
    }

    public func logs(runnerID: Runner.ID, limit: Int) async throws -> [LogEntry] {
        try await simulateCall()
        _ = try indexOf(runnerID)
        return Array((logsByRunner[runnerID] ?? []).suffix(max(0, limit)))
    }

    // MARK: Helpers

    private static let gib: UInt64 = 1 << 30

    private func simulateCall() async throws {
        if latency > .zero { try await Task.sleep(for: latency) }
        if scenario == .unavailable {
            throw RunnerServiceError.unavailable("the demo is simulating a stopped service.")
        }
    }

    private func status() -> MachineStatus {
        let admission: AdmissionState = switch connectionState {
        case .associated: admissionMode == .paused ? .paused : .open
        case .notAssociated: .blocked(.notAssociated)
        case .unreachable: .blocked(.unreachable)
        case .revoked: .blocked(.revoked)
        }
        let tasks = runnerList.compactMap { runner -> ActiveTask? in
            guard runner.state == .busy, let task = runner.currentTaskRef else { return nil }
            return ActiveTask(runnerID: runner.id, runnerName: runner.name, taskRef: task, startedAt: runner.lastJobAt ?? now())
        }
        let busyCPU = runnerList.filter { $0.state == .busy }.reduce(0) { $0 + $1.usage.cpuPercent }
        let busyMemory = runnerList.filter { $0.state == .busy }.reduce(UInt64(0)) { $0 + $1.usage.memoryBytes }
        return MachineStatus(
            serviceMode: .demo,
            mode: admissionMode,
            admission: admission,
            maxConcurrentTasks: maxConcurrentTasks,
            maxConcurrentTasksUpperBound: Self.cpuCores,
            configRevision: configRevision,
            busyRunners: tasks.count,
            activeTasks: tasks,
            load: MachineLoad(
                sampledAt: now(),
                cpuPercent: 4 + busyCPU / Double(Self.cpuCores),
                memoryUsedBytes: 22 * Self.gib + busyMemory,
                memoryTotalBytes: 128 * Self.gib,
                memoryPressure: .normal,
                diskFreeBytes: 1_200 * Self.gib
            )
        )
    }

    private func checkRevision(_ expected: Int) throws {
        guard expected == configRevision else {
            throw RunnerServiceError.conflict(currentRevision: configRevision)
        }
    }

    private func indexOf(_ id: Runner.ID) throws -> Int {
        guard let index = runnerList.firstIndex(where: { $0.id == id }) else {
            throw RunnerServiceError.runnerNotFound
        }
        return index
    }

    private func appendLog(_ id: Runner.ID, _ level: LogEntry.Level, _ message: String) {
        logsByRunner[id, default: []].append(LogEntry(date: now(), level: level, message: message))
    }

    private static func seedRunners(at date: Date) -> [(runner: Runner, logs: [LogEntry])] {
        func ago(_ seconds: TimeInterval) -> Date { date.addingTimeInterval(-seconds) }
        let build = Runner(
            name: "runner-1",
            state: .busy,
            usage: ResourceUsage(cpuPercent: 375, memoryBytes: 18 * gib, isRunningJob: true),
            createdAt: ago(86_400 * 3),
            lastJobAt: ago(120),
            currentTaskRef: "demo-task-128"
        )
        let agents = Runner(
            name: "runner-2",
            state: .stopped,
            createdAt: ago(86_400 * 2),
            lastJobAt: ago(7_200)
        )
        let gpu = Runner(
            name: "runner-3",
            state: .failed,
            failureReason: "Simulated failure: the runner exited with status 1.",
            createdAt: ago(86_400),
            lastJobAt: ago(5_400)
        )
        return [
            (build, [
                LogEntry(date: ago(86_400 * 3), level: .info, message: "[demo] Provisioned by the service (simulated)."),
                LogEntry(date: ago(600), level: .info, message: "[demo] Task demo-task-127 started (simulated)."),
                LogEntry(date: ago(540), level: .info, message: "[demo] Task demo-task-127 finished: succeeded (simulated)."),
                LogEntry(date: ago(120), level: .info, message: "[demo] Task demo-task-128 started (simulated)."),
            ]),
            (agents, [
                LogEntry(date: ago(86_400 * 2), level: .info, message: "[demo] Provisioned by the service (simulated)."),
                LogEntry(date: ago(7_000), level: .info, message: "[demo] Stopping: idle and not needed (simulated)."),
            ]),
            (gpu, [
                LogEntry(date: ago(86_400), level: .info, message: "[demo] Provisioned by the service (simulated)."),
                LogEntry(date: ago(5_500), level: .warning, message: "[demo] Job took longer than usual (simulated)."),
                LogEntry(date: ago(5_400), level: .error, message: "[demo] Runner exited with status 1 (simulated)."),
            ]),
        ]
    }
}
