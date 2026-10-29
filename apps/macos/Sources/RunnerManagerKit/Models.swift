import Foundation

/// The machine the manager runs on, as reported by the service.
public struct MachineInfo: Equatable, Sendable {
    public var name: String
    public var model: String
    public var architecture: String
    public var operatingSystem: String
    public var cpuCores: Int
    public var memoryBytes: UInt64
    public var diskFreeBytes: UInt64

    public init(
        name: String,
        model: String,
        architecture: String,
        operatingSystem: String,
        cpuCores: Int,
        memoryBytes: UInt64,
        diskFreeBytes: UInt64
    ) {
        self.name = name
        self.model = model
        self.architecture = architecture
        self.operatingSystem = operatingSystem
        self.cpuCores = cpuCores
        self.memoryBytes = memoryBytes
        self.diskFreeBytes = diskFreeBytes
    }
}

/// Whether this manager is associated with a Nestor instance.
///
/// Association happens once per machine; each runner then gets its own identity from Nestor.
/// Workspaces are assigned to runners on the Nestor web app, never here.
public enum NestorConnection: Equatable, Sendable {
    case notAssociated
    case associated(instance: String, since: Date)
    case unreachable(instance: String, reason: String)
}

public enum RunnerState: String, Equatable, Sendable, CaseIterable {
    case running
    case starting
    case stopping
    case stopped
    case failed

    /// Whether `start` makes sense from this state.
    public var canStart: Bool { self == .stopped || self == .failed }
    /// Whether `stop` makes sense from this state.
    public var canStop: Bool { self == .running || self == .starting }
    /// A runner must be stopped before it is removed.
    public var canRemove: Bool { self == .stopped || self == .failed }
}

/// What a runner is using right now, as measured. These are measurements, not caps: a runner has
/// no processor or memory limit.
///
/// A runner takes one job at a time, as the Nestor runner does, and that job may use several
/// threads and processes. To run jobs in parallel, a machine runs several runners.
public struct ResourceUsage: Equatable, Sendable {
    public var cpuPercent: Double
    public var memoryBytes: UInt64
    /// Whether the runner is running its job; it never runs more than one.
    public var isRunningJob: Bool

    public init(cpuPercent: Double = 0, memoryBytes: UInt64 = 0, isRunningJob: Bool = false) {
        self.cpuPercent = cpuPercent
        self.memoryBytes = memoryBytes
        self.isRunningJob = isRunningJob
    }

    public static let idle = ResourceUsage()
}

public struct Runner: Identifiable, Equatable, Sendable {
    public let id: UUID
    public var name: String
    public var state: RunnerState
    /// The last failure, shown while `state == .failed`.
    public var failureReason: String?
    public var usage: ResourceUsage
    public var createdAt: Date
    public var lastJobAt: Date?

    public init(
        id: UUID = UUID(),
        name: String,
        state: RunnerState,
        failureReason: String? = nil,
        usage: ResourceUsage = .idle,
        createdAt: Date,
        lastJobAt: Date? = nil
    ) {
        self.id = id
        self.name = name
        self.state = state
        self.failureReason = failureReason
        self.usage = usage
        self.createdAt = createdAt
        self.lastJobAt = lastJobAt
    }
}

/// What the add-runner flow collects. It holds no secret: the runner's credential is issued
/// by Nestor and stored by the service, never typed into the interface.
public struct NewRunnerRequest: Equatable, Sendable {
    public var name: String
    public var startNow: Bool

    public init(name: String, startNow: Bool = true) {
        self.name = name
        self.startNow = startNow
    }
}

public struct LogEntry: Identifiable, Equatable, Sendable {
    public enum Level: String, Sendable, CaseIterable {
        case info, warning, error
    }

    public let id: UUID
    public var date: Date
    public var level: Level
    public var message: String

    public init(id: UUID = UUID(), date: Date, level: Level, message: String) {
        self.id = id
        self.date = date
        self.level = level
        self.message = message
    }
}
