import Darwin
import Foundation

/// A `RunnerService` backed by the local service of `core/` (`nestor-runner-service`), over its
/// local API: one JSON request per line on a Unix domain socket (`docs/local-api.md`).
///
/// Whether the data is real is decided by the service, not by this client: the service reports
/// `serviceMode` on every machine status, and a service started with `--simulate` reports `demo`.
///
/// Before sending anything, the client checks with the kernel that the process serving the
/// socket runs as the same user: a service of another user (or a fake one announcing `live`)
/// is refused. What the app may do is decided by the service from the app's code signature
/// (`access()`).
public final class LiveRunnerService: RunnerService {
    public let mode: ServiceMode = .live
    public let socketPath: String
    private let timeout: TimeInterval

    public static let apiVersion = "1.1"

    /// The socket of a live service started with its default settings.
    public static var defaultSocketPath: String {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Library/Application Support")
        return support.appendingPathComponent("Nestor Runner Manager/live/run/service.sock").path
    }

    public init(socketPath: String = LiveRunnerService.defaultSocketPath, timeout: TimeInterval = 10) {
        self.socketPath = socketPath
        self.timeout = timeout
    }

    // MARK: RunnerService

    public func access() async throws -> ServiceAccess {
        try await call("hello", as: Wire.Hello.self).model
    }

    public func machineInfo() async throws -> MachineInfo {
        let info = try await call("machineInfo", as: Wire.MachineInfo.self)
        return MachineInfo(
            name: info.name,
            model: info.model,
            architecture: info.architecture,
            operatingSystem: info.operatingSystem,
            cpuCores: info.cpuCores,
            memoryBytes: info.memoryBytes,
            diskFreeBytes: info.diskFreeBytes
        )
    }

    public func connection() async throws -> NestorConnection {
        try await call("connection", as: Wire.Connection.self).model
    }

    public func associate() async throws -> NestorConnection {
        try await call("associate", as: Wire.Connection.self).model
    }

    public func cancelAssociation() async throws -> NestorConnection {
        try await call("cancelAssociation", as: Wire.Connection.self).model
    }

    public func disassociate() async throws -> NestorConnection {
        // The interface asked the user first: running tasks end.
        try await call("disassociate", ["confirmed": true], as: Wire.Connection.self).model
    }

    public func machineStatus() async throws -> MachineStatus {
        try await call("machineStatus", as: Wire.Status.self).model
    }

    public func setMode(_ mode: AdmissionMode, expectedRevision: Int) async throws -> MachineStatus {
        try await call("setMode", ["mode": mode.rawValue, "expectedRevision": expectedRevision], as: Wire.Status.self).model
    }

    public func setMaxConcurrentTasks(_ value: Int, expectedRevision: Int, confirmed: Bool) async throws -> MachineStatus {
        try await call(
            "setMaxConcurrentTasks",
            ["value": value, "expectedRevision": expectedRevision, "confirmed": confirmed],
            as: Wire.Status.self
        ).model
    }

    public func runners() async throws -> [Runner] {
        try await call("runners", as: [Wire.Runner].self).map(\.model)
    }

    public func addRunner(_ request: NewRunnerRequest) async throws -> Runner {
        _ = try RunnerNameRule.validate(request.name)
        // The add sheet, an advanced action with its own explanation, is the confirmation.
        return try await call(
            "addRunner",
            ["name": request.name, "startNow": request.startNow, "confirmed": true],
            as: Wire.Runner.self
        ).model
    }

    public func start(runnerID: Runner.ID) async throws -> Runner {
        try await call("start", ["runnerId": Wire.serviceID(runnerID)], as: Wire.Runner.self).model
    }

    public func stop(runnerID: Runner.ID, force: Bool) async throws -> Runner {
        // A forced stop is only sent after the interface's confirmation.
        try await call(
            "stop",
            ["runnerId": Wire.serviceID(runnerID), "force": force, "confirmed": force],
            as: Wire.Runner.self
        ).model
    }

    public func remove(runnerID: Runner.ID) async throws {
        _ = try await call("remove", ["runnerId": Wire.serviceID(runnerID)], as: Wire.Empty.self)
    }

    public func logs(runnerID: Runner.ID, limit: Int) async throws -> [LogEntry] {
        guard limit > 0 else { return [] }
        let entries = try await call("logs", ["runnerId": Wire.serviceID(runnerID), "limit": limit], as: [Wire.Log].self)
        return entries.map(\.model)
    }

    // MARK: Transport

    func call<Result: Decodable>(_ op: String, _ arguments: [String: Any] = [:], as type: Result.Type) async throws -> Result {
        var request = arguments
        request["apiVersion"] = Self.apiVersion
        request["id"] = 1
        request["op"] = op
        let line = try JSONSerialization.data(withJSONObject: request)
        let path = socketPath
        let timeout = timeout
        let reply = try await Task.detached {
            try Self.exchange(line, socketPath: path, timeout: timeout)
        }.value
        return try Self.decode(reply, as: type)
    }

    static func decode<Result: Decodable>(_ reply: Data, as type: Result.Type) throws -> Result {
        let envelope: Wire.Envelope<Result>
        do {
            envelope = try JSONDecoder().decode(Wire.Envelope<Result>.self, from: reply)
        } catch {
            throw RunnerServiceError.unavailable("unreadable answer from the service.")
        }
        if envelope.ok, let result = envelope.result {
            return result
        }
        throw envelope.error?.serviceError ?? RunnerServiceError.unavailable("empty answer from the service.")
    }

    /// Connects, sends one line, reads one line. Blocking; run off the main actor.
    private static func exchange(_ request: Data, socketPath: String, timeout: TimeInterval) throws -> Data {
        let descriptor = socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw RunnerServiceError.unavailable("cannot open a socket.") }
        defer { close(descriptor) }

        var noSigPipe: Int32 = 1
        setsockopt(descriptor, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        var time = timeval(tv_sec: Int(timeout), tv_usec: 0)
        setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO, &time, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, &time, socklen_t(MemoryLayout<timeval>.size))

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(socketPath.utf8)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard pathBytes.count < capacity else { throw RunnerServiceError.unavailable("the socket path is too long.") }
        withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            buffer.copyBytes(from: pathBytes)
            buffer[pathBytes.count] = 0
        }
        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(descriptor, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else {
            throw RunnerServiceError.unavailable("the runner service is not running (no answer on \(socketPath)).")
        }
        // Only talk to a service of our own user.
        var peerUser: uid_t = 0
        var peerGroup: gid_t = 0
        guard getpeereid(descriptor, &peerUser, &peerGroup) == 0, peerUser == geteuid() else {
            throw RunnerServiceError.unavailable("the process serving \(socketPath) runs as another user; it was not used.")
        }

        var payload = request
        payload.append(0x0A)
        try payload.withUnsafeBytes { buffer in
            var offset = 0
            while offset < buffer.count {
                let written = write(descriptor, buffer.baseAddress! + offset, buffer.count - offset)
                guard written > 0 else { throw RunnerServiceError.unavailable("the service closed the connection.") }
                offset += written
            }
        }

        var reply = Data()
        var chunk = [UInt8](repeating: 0, count: 16 * 1024)
        while !reply.contains(0x0A) {
            let count = read(descriptor, &chunk, chunk.count)
            guard count > 0 else {
                if reply.isEmpty { throw RunnerServiceError.unavailable("no answer from the service.") }
                break
            }
            reply.append(contentsOf: chunk[0..<count])
            if reply.count > 4 * 1024 * 1024 { throw RunnerServiceError.unavailable("answer too large.") }
        }
        if let newline = reply.firstIndex(of: 0x0A) { reply = reply[..<newline] }
        return reply
    }
}

/// The JSON shapes of the local API (`docs/local-api.md`).
enum Wire {
    struct Empty: Decodable {}

    struct Envelope<Result: Decodable>: Decodable {
        var ok: Bool
        var result: Result?
        var error: ErrorBody?
    }

    struct ErrorBody: Decodable {
        var code: String
        var message: String
        var details: [String: JSONValue]?

        var serviceError: RunnerServiceError {
            switch code {
            case "notAssociated": .notAssociated
            case "revoked": .revoked
            case "runnerNotFound": .runnerNotFound
            case "duplicateName": .duplicateName(details?["name"]?.string ?? "")
            case "invalidTransition":
                if let from = details?["from"]?.string.flatMap(RunnerState.init(rawValue:)), let action = details?["action"]?.string {
                    .invalidTransition(from: from, action: action)
                } else {
                    .invalidArgument(message)
                }
            case "conflict": .conflict(currentRevision: details?["currentRevision"]?.int ?? -1)
            case "limitOutOfRange": .limitOutOfRange(min: details?["min"]?.int ?? 1, max: details?["max"]?.int ?? 1)
            case "invalidArgument": .invalidArgument(message)
            case "permissionDenied": .permissionDenied
            case "incompatibleVersion": .incompatibleVersion(details?["supported"]?.string ?? "?")
            case "confirmationRequired": .confirmationRequired(message)
            default: .unavailable(message)
            }
        }
    }

    enum JSONValue: Decodable {
        case string(String), number(Double), bool(Bool), other

        init(from decoder: any Decoder) throws {
            let container = try decoder.singleValueContainer()
            if let value = try? container.decode(String.self) { self = .string(value) }
            else if let value = try? container.decode(Double.self) { self = .number(value) }
            else if let value = try? container.decode(Bool.self) { self = .bool(value) }
            else { self = .other }
        }

        var string: String? { if case .string(let value) = self { value } else { nil } }
        var int: Int? { if case .number(let value) = self { Int(value) } else { nil } }
    }

    static func date(_ milliseconds: UInt64) -> Date {
        Date(timeIntervalSince1970: TimeInterval(milliseconds) / 1000)
    }

    /// The service names runners with 32 hexadecimal digits; the interface uses `UUID`s.
    static func runnerID(_ serviceID: String) -> UUID {
        let hex = Array(serviceID.lowercased())
        guard hex.count == 32 else { return UUID() }
        let parts = [0..<8, 8..<12, 12..<16, 16..<20, 20..<32].map { String(hex[$0]) }
        return UUID(uuidString: parts.joined(separator: "-")) ?? UUID()
    }

    static func serviceID(_ id: UUID) -> String {
        id.uuidString.lowercased().replacingOccurrences(of: "-", with: "")
    }

    struct MachineInfo: Decodable {
        var name: String
        var model: String
        var architecture: String
        var operatingSystem: String
        var cpuCores: Int
        var memoryBytes: UInt64
        var diskFreeBytes: UInt64
    }

    struct Hello: Decodable {
        var access: String?
        var peerVerification: String?

        var model: ServiceAccess {
            ServiceAccess(
                level: access.flatMap(ServiceAccess.Level.init(rawValue:)) ?? .basic,
                verification: peerVerification.flatMap(ServiceAccess.Verification.init(rawValue:)) ?? .none
            )
        }
    }

    struct Connection: Decodable {
        var state: String
        var instance: String?
        var sinceMs: UInt64?
        var reason: String?
        var userCode: String?
        var verificationUri: String?
        var expiresAtMs: UInt64?

        var model: NestorConnection {
            switch state {
            case "associating":
                .associating(
                    instance: instance ?? "?",
                    userCode: userCode ?? "",
                    verificationURL: verificationUri ?? "",
                    expiresAt: Wire.date(expiresAtMs ?? 0)
                )
            case "associated": .associated(instance: instance ?? "?", since: Wire.date(sinceMs ?? 0))
            case "unreachable": .unreachable(instance: instance ?? "?", reason: reason ?? "")
            case "revoked": .revoked
            default: .notAssociated
            }
        }
    }

    struct Admission: Decodable {
        var state: String
        var reason: String?

        var model: AdmissionState {
            switch state {
            case "open": .open
            case "paused": .paused
            default: .blocked(reason.flatMap(AdmissionBlock.init(rawValue:)) ?? .reconciling)
            }
        }
    }

    struct Load: Decodable {
        var sampledAtMs: UInt64
        var measuredCpuPercent: Double
        var measuredMemoryUsedBytes: UInt64
        var measuredMemoryTotalBytes: UInt64
        var measuredMemoryPressure: String?
        var measuredDiskFreeBytes: UInt64
    }

    struct TaskBody: Decodable {
        var runnerId: String
        var runnerName: String
        var taskRef: String
        var startedAtMs: UInt64
    }

    struct Status: Decodable {
        var serviceMode: String
        var connection: Connection
        var mode: String
        var admission: Admission
        var maxConcurrentTasks: Int
        var maxConcurrentTasksUpperBound: Int
        var configRevision: Int
        var busyRunners: Int
        var activeTasks: [TaskBody]
        var load: Load?
        var configError: String?

        var model: MachineStatus {
            MachineStatus(
                // Anything but an explicit "live" is shown as a demo.
                serviceMode: serviceMode == "live" ? .live : .demo,
                mode: AdmissionMode(rawValue: mode) ?? .paused,
                admission: admission.model,
                maxConcurrentTasks: maxConcurrentTasks,
                maxConcurrentTasksUpperBound: maxConcurrentTasksUpperBound,
                configRevision: configRevision,
                busyRunners: busyRunners,
                activeTasks: activeTasks.map {
                    ActiveTask(runnerID: Wire.runnerID($0.runnerId), runnerName: $0.runnerName, taskRef: $0.taskRef, startedAt: Wire.date($0.startedAtMs))
                },
                load: load.map {
                    MachineLoad(
                        sampledAt: Wire.date($0.sampledAtMs),
                        cpuPercent: $0.measuredCpuPercent,
                        memoryUsedBytes: $0.measuredMemoryUsedBytes,
                        memoryTotalBytes: $0.measuredMemoryTotalBytes,
                        memoryPressure: $0.measuredMemoryPressure.flatMap(MemoryPressure.init(rawValue:)),
                        diskFreeBytes: $0.measuredDiskFreeBytes
                    )
                },
                configError: configError
            )
        }
    }

    struct Runner: Decodable {
        var id: String
        var name: String
        var state: String
        var failureReason: String?
        var taskRef: String?
        var createdAtMs: UInt64
        var lastTaskAtMs: UInt64?
        var measuredCpuPercent: Double
        var measuredMemoryBytes: UInt64

        var model: RunnerManagerKit.Runner {
            let state = RunnerState(rawValue: state) ?? .failed
            return RunnerManagerKit.Runner(
                id: Wire.runnerID(id),
                name: name,
                state: state,
                failureReason: failureReason,
                usage: ResourceUsage(cpuPercent: measuredCpuPercent, memoryBytes: measuredMemoryBytes, isRunningJob: state == .busy),
                createdAt: Wire.date(createdAtMs),
                lastJobAt: lastTaskAtMs.map(Wire.date),
                currentTaskRef: taskRef
            )
        }
    }

    struct Log: Decodable {
        var atMs: UInt64
        var level: String
        var message: String

        var model: LogEntry {
            LogEntry(date: Wire.date(atMs), level: LogEntry.Level(rawValue: level) ?? .info, message: message)
        }
    }
}
