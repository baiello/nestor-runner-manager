import Foundation

/// How a `RunnerService` is backed. The interface shows a demo banner unless it is `.live`.
public enum ServiceMode: Equatable, Sendable {
    /// Simulated data and commands; nothing reaches Nestor or a real runner.
    case demo
    /// A real local service (the future shared Rust core).
    case live
}

public enum RunnerServiceError: Error, Equatable, Sendable, LocalizedError {
    case runnerNotFound
    case invalidName(String)
    case duplicateName(String)
    case invalidTransition(from: RunnerState, action: String)
    case notAssociated
    case unavailable(String)

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
        case .unavailable(let reason):
            "The runner service is unavailable: \(reason)"
        }
    }
}

/// Everything the interface needs from the local runner service.
///
/// The views only depend on this protocol. `DemoRunnerService` implements it with simulated
/// data today; an implementation talking to the shared Rust core over local IPC will replace it
/// without changes to the views.
public protocol RunnerService: Sendable {
    var mode: ServiceMode { get }

    func machineInfo() async throws -> MachineInfo
    func connection() async throws -> NestorConnection
    /// Associates the machine with a Nestor instance. The real flow happens in the browser
    /// (device authorization); the interface never asks for a secret.
    func associate() async throws -> NestorConnection

    func runners() async throws -> [Runner]
    func addRunner(_ request: NewRunnerRequest) async throws -> Runner
    func start(runnerID: Runner.ID) async throws -> Runner
    func stop(runnerID: Runner.ID) async throws -> Runner
    func remove(runnerID: Runner.ID) async throws
    func logs(runnerID: Runner.ID, limit: Int) async throws -> [LogEntry]
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
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_. "))
        if name.unicodeScalars.contains(where: { !allowed.contains($0) }) {
            throw RunnerServiceError.invalidName("Use letters, digits, spaces, “-”, “_” or “.”.")
        }
        return name
    }
}
