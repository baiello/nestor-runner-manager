import Foundation
import Testing
@testable import RunnerManagerKit

/// The local API's JSON, as `nestor-runner-service` writes it (`docs/local-api.md`).
@Suite struct LiveRunnerServiceTests {
    private func data(_ text: String) -> Data { Data(text.utf8) }

    @Test func decodesALiveStatusWithoutAnyLimitField() throws {
        let reply = data("""
        {"apiVersion":"1.0","id":1,"ok":true,"result":{"activeTasks":[],"admission":{"reason":"notAssociated","state":"blocked"},"busyRunners":0,"configError":null,"configRevision":0,"connection":{"state":"notAssociated"},"load":{"measuredCpuPercent":7.8,"measuredDiskFreeBytes":231194337280,"measuredMemoryPressure":"normal","measuredMemoryTotalBytes":38654705664,"measuredMemoryUsedBytes":19926319104,"sampledAtMs":1793291543411},"maxConcurrentTasks":1,"maxConcurrentTasksUpperBound":18,"mode":"automatic","serviceMode":"live"}}
        """)
        let status = try LiveRunnerService.decode(reply, as: Wire.Status.self).model
        #expect(status.serviceMode == .live)
        #expect(status.admission == .blocked(.notAssociated))
        #expect(status.maxConcurrentTasks == 1)
        #expect(status.maxConcurrentTasksUpperBound == 18)
        #expect(status.load?.memoryPressure == .normal)
        #expect(status.load?.memoryTotalBytes == 38_654_705_664)
    }

    @Test func anythingButLiveIsADemo() throws {
        let reply = data("""
        {"ok":true,"result":{"activeTasks":[{"runnerId":"0123456789abcdef0123456789abcdef","runnerName":"runner-1","taskRef":"demo-task-3","startedAtMs":1000}],"admission":{"state":"paused"},"busyRunners":1,"configError":"unreadable state file","configRevision":4,"connection":{"state":"associated","instance":"demo.nestor.invalid","sinceMs":5000},"load":null,"maxConcurrentTasks":2,"maxConcurrentTasksUpperBound":8,"mode":"paused","serviceMode":"somethingNew"}}
        """)
        let status = try LiveRunnerService.decode(reply, as: Wire.Status.self).model
        #expect(status.serviceMode == .demo)
        #expect(status.admission == .paused)
        #expect(status.configError == "unreadable state file")
        let task = try #require(status.activeTasks.first)
        #expect(task.taskRef == "demo-task-3")
        #expect(Wire.serviceID(task.runnerID) == "0123456789abcdef0123456789abcdef")
    }

    @Test func decodesRunnersAndTheirStates() throws {
        let reply = data("""
        {"ok":true,"result":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","name":"runner-1","state":"busy","generation":3,"failureReason":null,"taskRef":"t-9","createdAtMs":1000,"lastTaskAtMs":2000,"measuredCpuPercent":250.5,"measuredMemoryBytes":1073741824},{"id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","name":"runner-2","state":"ready","generation":1,"failureReason":null,"taskRef":null,"createdAtMs":1000,"lastTaskAtMs":null,"measuredCpuPercent":0,"measuredMemoryBytes":0}]}
        """)
        let runners = try LiveRunnerService.decode(reply, as: [Wire.Runner].self).map(\.model)
        #expect(runners.map(\.state) == [.busy, .ready])
        #expect(runners[0].usage.isRunningJob)
        #expect(runners[0].currentTaskRef == "t-9")
        #expect(runners[0].usage.cpuPercent == 250.5)
        #expect(!runners[1].usage.isRunningJob)
    }

    @Test(arguments: [
        (#"{"code":"conflict","message":"m","retryable":true,"details":{"currentRevision":7}}"#, RunnerServiceError.conflict(currentRevision: 7)),
        (#"{"code":"limitOutOfRange","message":"m","retryable":false,"details":{"min":1,"max":18}}"#, .limitOutOfRange(min: 1, max: 18)),
        (#"{"code":"notAssociated","message":"m","retryable":false}"#, .notAssociated),
        (#"{"code":"invalidTransition","message":"m","retryable":false,"details":{"from":"busy","action":"stop"}}"#, .invalidTransition(from: .busy, action: "stop")),
        (#"{"code":"duplicateName","message":"m","retryable":false,"details":{"name":"diag"}}"#, .duplicateName("diag")),
        (#"{"code":"incompatibleVersion","message":"m","retryable":false,"details":{"supported":"1.0"}}"#, .incompatibleVersion("1.0")),
        (#"{"code":"permissionDenied","message":"m","retryable":false}"#, .permissionDenied),
        (#"{"code":"unavailable","message":"Association is not available.","retryable":true}"#, .unavailable("Association is not available.")),
    ])
    func mapsErrorCodes(body: String, expected: RunnerServiceError) {
        let reply = data(#"{"apiVersion":"1.0","id":1,"ok":false,"error":"# + body + "}")
        #expect(throws: expected) { try LiveRunnerService.decode(reply, as: Wire.Status.self) }
    }

    @Test func unreachableServiceIsUnavailable() async {
        let service = LiveRunnerService(socketPath: "/nonexistent/service.sock", timeout: 1)
        await #expect(throws: RunnerServiceError.self) { try await service.machineStatus() }
    }
}
