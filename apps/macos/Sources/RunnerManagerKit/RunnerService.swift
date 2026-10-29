import Foundation

/// How a `RunnerService` is backed. The interface shows a demo banner unless it is `.live`.
public enum ServiceMode: Equatable, Sendable {
    /// Simulated data and commands; nothing reaches Nestor or a real runner.
    case demo
    /// The real local service (`core/`, reached over its local API).
    case live
}

public enum RunnerServiceError: Error, Equatable, Sendable, LocalizedError {
    case runnerNotFound
    case invalidName(String)
    case duplicateName(String)
    case invalidTransition(from: RunnerState, action: String)
    case notAssociated
    case revoked
    case unavailable(String)
    /// The settings changed meanwhile (another window or client); reload and try again.
    case conflict(currentRevision: Int)
    case limitOutOfRange(min: Int, max: Int)
    case invalidArgument(String)
    case permissionDenied
    case incompatibleVersion(String)
    /// A sensitive change the service accepts only once the user has confirmed it.
    case confirmationRequired(String)

    public var errorDescription: String? {
        switch self {
        case .runnerNotFound:
            "This runner no longer exists."
        case .invalidName(let reason):
            reason
        case .duplicateName(let name):
            "A runner named “\(name)” already exists on this machine."
        case .invalidTransition(let state, let action):
            "Cannot \(action) a runner that is \(state.rawValue)."
        case .notAssociated:
            "Associate this machine with Nestor before adding a runner."
        case .revoked:
            "This machine's association was revoked in Nestor."
        case .unavailable(let reason):
            "The runner service is unavailable: \(reason)"
        case .conflict:
            "The settings were changed elsewhere meanwhile. They have been reloaded; try again."
        case .limitOutOfRange(let min, let max):
            "Choose between \(min) and \(max) tasks at a time."
        case .invalidArgument(let reason):
            reason
        case .permissionDenied:
            "The runner service refused the request: it belongs to another user."
        case .incompatibleVersion(let supported):
            "The runner service is incompatible with this app (it speaks API \(supported))."
        case .confirmationRequired(let reason):
            reason
        }
    }
}

/// Everything the interface needs from the local runner service.
///
/// The views only depend on this protocol. `DemoRunnerService` implements it with simulated
/// data; `LiveRunnerService` talks to the service of `core/` over its local API
/// (`docs/local-api.md`).
///
/// The machine is the main object: its admission settings and status. Per-runner operations are
/// diagnostics; the service starts and stops runners by itself.
public protocol RunnerService: Sendable {
    /// How this client is backed. The service also reports its mode on every `machineStatus`,
    /// and that report wins.
    var mode: ServiceMode { get }

    /// What the service lets this app do.
    func access() async throws -> ServiceAccess
    func machineInfo() async throws -> MachineInfo
    func connection() async throws -> NestorConnection
    /// Starts associating the machine with Nestor (device authorization). The connection
    /// becomes `.associating` with a code an administrator approves on the Nestor web app; the
    /// interface never asks for a secret.
    func associate() async throws -> NestorConnection
    /// Abandons an association in progress.
    func cancelAssociation() async throws -> NestorConnection
    /// The owner's answer to the account Nestor approved the machine for (`.confirming`):
    /// accepted, the machine takes tasks; refused, it disassociates itself.
    func confirmAssociation(accept: Bool) async throws -> NestorConnection
    /// Ends the association. Call it only after the user confirmed: it ends the tasks running
    /// on this machine and erases its credentials, even if Nestor cannot be reached.
    func disassociate() async throws -> NestorConnection

    func machineStatus() async throws -> MachineStatus
    /// Pauses or resumes admissions. `expectedRevision` is the `configRevision` the change is
    /// based on; a stale one fails with `.conflict`.
    func setMode(_ mode: AdmissionMode, expectedRevision: Int) async throws -> MachineStatus
    /// Sets how many tasks may run at the same time. Lowering it interrupts no task. Raising it
    /// needs `confirmed` (the user confirmed it), or fails with `.confirmationRequired`.
    func setMaxConcurrentTasks(_ value: Int, expectedRevision: Int, confirmed: Bool) async throws -> MachineStatus

    // Diagnostics.
    func runners() async throws -> [Runner]
    func addRunner(_ request: NewRunnerRequest) async throws -> Runner
    func start(runnerID: Runner.ID) async throws -> Runner
    /// Stops a runner. A busy runner is only stopped with `force`, which ends its task.
    func stop(runnerID: Runner.ID, force: Bool) async throws -> Runner
    func remove(runnerID: Runner.ID) async throws
    func logs(runnerID: Runner.ID, limit: Int) async throws -> [LogEntry]
}

extension RunnerService {
    public func stop(runnerID: Runner.ID) async throws -> Runner {
        try await stop(runnerID: runnerID, force: false)
    }

    public func setMaxConcurrentTasks(_ value: Int, expectedRevision: Int) async throws -> MachineStatus {
        try await setMaxConcurrentTasks(value, expectedRevision: expectedRevision, confirmed: false)
    }
}

public enum RunnerNameRule {
    public static let maxLength = 40

    /// Returns the trimmed name, or throws why it is not acceptable.
    public static func validate(_ raw: String) throws -> String {
        let name = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if name.isEmpty {
            throw RunnerServiceError.invalidName("Give the runner a name.")
        }
        if name.count > maxLength {
            throw RunnerServiceError.invalidName("Use at most \(maxLength) characters.")
        }
        // ASCII only, as the service checks it again (no look-alike names).
        let allowed = Set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_. ".unicodeScalars)
        if name.unicodeScalars.contains(where: { !allowed.contains($0) }) {
            throw RunnerServiceError.invalidName("Use letters (A to Z), digits, spaces, “-”, “_” or “.”.")
        }
        return name
    }
}
