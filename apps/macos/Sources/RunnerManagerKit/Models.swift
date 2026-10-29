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
    /// Waiting for an administrator to approve this machine on the Nestor web app with
    /// `userCode`. The code alone grants nothing; nothing secret is shown or typed here.
    case associating(instance: String, userCode: String, verificationURL: String, expiresAt: Date)
    case associated(instance: String, since: Date)
    case unreachable(instance: String, reason: String)
    /// The association was revoked in Nestor; associate again to take tasks.
    case revoked
}

/// A runner's lifecycle. The service starts and stops runners itself; a runner takes one task at
/// a time and is `busy` while it does.
public enum RunnerState: String, Equatable, Sendable, CaseIterable {
    case busy
    case ready
    case starting
    case stopping
    case stopped
    case failed

    /// Whether `start` makes sense from this state.
    public var canStart: Bool { self == .stopped || self == .failed }
    /// Whether `stop` makes sense from this state. Stopping a busy runner ends its task, so the
    /// interface asks for a confirmation first.
    public var canStop: Bool { self == .ready || self == .starting || self == .busy }
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
    /// Nestor's reference of the task the runner is running, if any.
    public var currentTaskRef: String?

    public init(
        id: UUID = UUID(),
        name: String,
        state: RunnerState,
        failureReason: String? = nil,
        usage: ResourceUsage = .idle,
        createdAt: Date,
        lastJobAt: Date? = nil,
        currentTaskRef: String? = nil
    ) {
        self.id = id
        self.name = name
        self.state = state
        self.failureReason = failureReason
        self.usage = usage
        self.createdAt = createdAt
        self.lastJobAt = lastJobAt
        self.currentTaskRef = currentTaskRef
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

// MARK: Machine status

/// The user's choice: take tasks automatically, or pause new admissions. A pause never stops a
/// task that is already running.
public enum AdmissionMode: String, Equatable, Sendable, CaseIterable {
    case automatic
    case paused
}

/// Why no task can be admitted besides a pause.
public enum AdmissionBlock: String, Equatable, Sendable {
    case notAssociated
    case unreachable
    case revoked
    /// The service has just started and is still checking the runners it left running.
    case reconciling
}

/// Whether the service accepts new tasks right now.
public enum AdmissionState: Equatable, Sendable {
    case open
    case paused
    case blocked(AdmissionBlock)
}

public enum MemoryPressure: String, Equatable, Sendable {
    case normal, warning, critical
}

/// One sample of the machine's load. Measurements, never limits.
public struct MachineLoad: Equatable, Sendable {
    public var sampledAt: Date
    public var cpuPercent: Double
    public var memoryUsedBytes: UInt64
    public var memoryTotalBytes: UInt64
    public var memoryPressure: MemoryPressure?
    public var diskFreeBytes: UInt64

    public init(
        sampledAt: Date,
        cpuPercent: Double,
        memoryUsedBytes: UInt64,
        memoryTotalBytes: UInt64,
        memoryPressure: MemoryPressure?,
        diskFreeBytes: UInt64
    ) {
        self.sampledAt = sampledAt
        self.cpuPercent = cpuPercent
        self.memoryUsedBytes = memoryUsedBytes
        self.memoryTotalBytes = memoryTotalBytes
        self.memoryPressure = memoryPressure
        self.diskFreeBytes = diskFreeBytes
    }
}

/// A task running on this machine. `taskRef` is Nestor's opaque reference; no task content is
/// kept locally.
public struct ActiveTask: Identifiable, Equatable, Sendable {
    public var id: String { taskRef }
    public var runnerID: Runner.ID
    public var runnerName: String
    public var taskRef: String
    public var startedAt: Date

    public init(runnerID: Runner.ID, runnerName: String, taskRef: String, startedAt: Date) {
        self.runnerID = runnerID
        self.runnerName = runnerName
        self.taskRef = taskRef
        self.startedAt = startedAt
    }
}

/// The machine as the service runs it: admission settings, load and active tasks.
///
/// `maxConcurrentTasks` regulates how many tasks are admitted at the same time. It is not a
/// processor or memory limit: no runner has one.
public struct MachineStatus: Equatable, Sendable {
    /// Reported by the service on every status. The interface shows the demo banner unless this
    /// is `.live`.
    public var serviceMode: ServiceMode
    public var mode: AdmissionMode
    public var admission: AdmissionState
    public var maxConcurrentTasks: Int
    /// Highest value accepted for `maxConcurrentTasks` (a sanity check on the input).
    public var maxConcurrentTasksUpperBound: Int
    /// Increases with every settings change; sent back with a change so two windows cannot
    /// overwrite each other silently.
    public var configRevision: Int
    public var busyRunners: Int
    public var activeTasks: [ActiveTask]
    public var load: MachineLoad?
    /// Set when the service could not read its stored settings and started paused instead.
    public var configError: String?

    public init(
        serviceMode: ServiceMode,
        mode: AdmissionMode,
        admission: AdmissionState,
        maxConcurrentTasks: Int,
        maxConcurrentTasksUpperBound: Int,
        configRevision: Int,
        busyRunners: Int,
        activeTasks: [ActiveTask],
        load: MachineLoad?,
        configError: String? = nil
    ) {
        self.serviceMode = serviceMode
        self.mode = mode
        self.admission = admission
        self.maxConcurrentTasks = maxConcurrentTasks
        self.maxConcurrentTasksUpperBound = maxConcurrentTasksUpperBound
        self.configRevision = configRevision
        self.busyRunners = busyRunners
        self.activeTasks = activeTasks
        self.load = load
        self.configError = configError
    }
}

// MARK: Access to the service

/// What the local service lets this app do, decided by the service from the app's code
/// signature (docs/local-api.md, "Who may do what").
public struct ServiceAccess: Equatable, Sendable {
    public enum Level: String, Equatable, Sendable {
        /// Read and change everything: the signed app.
        case full
        /// Read only, without runner logs: the signed command line tool.
        case readOnly
        /// Status only: an app the service could not verify.
        case basic
    }

    public enum Verification: String, Equatable, Sendable {
        /// The service checked the app's code signature.
        case signature
        /// Not verified; the service was started with `--allow-unsigned-clients` (development).
        case unsignedFallback
        case none
        /// The in-app demo: there is no service.
        case notApplicable
    }

    public var level: Level
    public var verification: Verification

    public init(level: Level, verification: Verification) {
        self.level = level
        self.verification = verification
    }

    public static let demo = ServiceAccess(level: .full, verification: .notApplicable)
}
