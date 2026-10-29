# Local API of the runner service — version 1.0

The interfaces reach the runner service (`core/`, binary `nestor-runner-service`) through this API. It follows the architecture contract of KCA-717 (§4). The macOS client is `LiveRunnerService`; the Rust server is `core/src/ipc.rs`.

## Transport and access

- **Unix domain socket**, one per service. Default path: `~/Library/Application Support/Nestor Runner Manager/live/run/service.sock` (`…/demo/run/…` for `--simulate`). No TCP port is opened.
- The socket's directory is created `0700` and must belong to the service's user; the socket is `0600`.
- On every connection the service asks the kernel for the peer's user id (`getpeereid`). Only the user running the service is served; anyone else gets `permissionDenied` and the connection is closed. There is no further token in this version: every client of that user may read and change the settings (contract §4.1 foresees a session token for non-native clients later).
- One service per data directory: an exclusive `flock` on `service.lock`. A second service refuses to start, and refuses to replace a socket that still answers.
- Framing: one JSON object per line (UTF-8, `\n`-terminated), at most 64 KiB per request. Requests on a connection are answered in order. At most 16 connections at a time.

## Requests and responses

```json
{"apiVersion": "1.0", "id": 7, "op": "setMode", "mode": "paused", "expectedRevision": 3}
```

```json
{"apiVersion": "1.0", "id": 7, "ok": true, "result": { … }}
{"apiVersion": "1.0", "id": 7, "ok": false, "error": {"code": "conflict", "message": "…", "retryable": true, "details": {"currentRevision": 4}}}
```

- `apiVersion` is required. A different **major** version gets `incompatibleVersion`; the client shows “service incompatible”. Minor versions only add fields; clients ignore unknown fields.
- `id` is echoed back.
- Times are milliseconds since the Unix epoch (`…Ms`).
- Resource figures are named `measured…`. **No field is a limit or a quota** (KCA-714): `maxConcurrentTasks` regulates admissions, not resources.

## Operations

| `op` | Arguments | Result |
| --- | --- | --- |
| `hello` | — | `{apiVersion, serviceMode}` |
| `machineInfo` | — | `MachineInfo` |
| `machineStatus` | — | `MachineStatus` |
| `connection` | — | `Connection` |
| `associate` | — | `Connection`. Demo service: simulated association with `demo.nestor.invalid`. Live service: `unavailable` until the Nestor API work exists (device authorization). |
| `disassociate` | — | `Connection`. Admissions stop; running tasks finish. |
| `setMode` | `mode`: `automatic` \| `paused`, `expectedRevision`? | `MachineStatus` |
| `setMaxConcurrentTasks` | `value` (1 … `maxConcurrentTasksUpperBound`), `expectedRevision`? | `MachineStatus` |
| `activeTasks` | — | `[ActiveTask]` |
| `events` | `since` (sequence, default 0) | `[Event]` with `seq > since` |
| `runners` *(diagnostics)* | — | `[Runner]` |
| `addRunner` *(diagnostics)* | `name`, `startNow`? | `Runner` |
| `start` *(diagnostics)* | `runnerId` | `Runner` |
| `stop` *(diagnostics)* | `runnerId`, `force`? | `Runner`. A `busy` runner needs `force: true`, which ends its task. |
| `remove` *(diagnostics)* | `runnerId` | `{}`. Only `stopped` or `failed` runners. |
| `logs` *(diagnostics)* | `runnerId`, `limit`? (default 200, max 1000) | `[LogEntry]` |

`expectedRevision` is the `configRevision` the change is based on. If it is stale the change is refused with `conflict` and nothing changes; the client reloads and tries again. Without it, the change applies to the current revision.

## Types

```jsonc
// MachineStatus
{
  "serviceMode": "live",                 // "live" | "demo" — the client shows its demo banner unless "live"
  "connection": {"state": "associated", "instance": "nestor.example", "sinceMs": 1793290000000},
  "mode": "automatic",                   // "automatic" | "paused"
  "admission": {"state": "open"},        // or {"state":"paused"} or {"state":"blocked","reason":"notAssociated"|"unreachable"|"revoked"|"reconciling"}
  "maxConcurrentTasks": 1,               // default 1
  "maxConcurrentTasksUpperBound": 18,    // number of cores: rejects absurd input, limits no resource
  "configRevision": 3,
  "busyRunners": 1,
  "activeTasks": [{"runnerId": "9f…", "runnerName": "runner-1", "taskRef": "…", "startedAtMs": 1793291000000}],
  "load": {"sampledAtMs": 1793291543411, "measuredCpuPercent": 7.8, "measuredMemoryUsedBytes": 19926319104,
           "measuredMemoryTotalBytes": 38654705664, "measuredMemoryPressure": "normal", "measuredDiskFreeBytes": 231194337280},
  "configError": null                    // set when the stored settings were unreadable: the service then runs paused with 1 task
}

// Connection
{"state": "notAssociated"} | {"state": "associated", "instance": "…", "sinceMs": 0}
| {"state": "unreachable", "instance": "…", "reason": "…"} | {"state": "revoked"}

// Runner
{"id": "32 hex digits", "name": "runner-1", "state": "busy", "generation": 3, "failureReason": null,
 "taskRef": "…", "createdAtMs": 0, "lastTaskAtMs": 0, "measuredCpuPercent": 250.0, "measuredMemoryBytes": 0}

// MachineInfo
{"name": "…", "model": "Mac17,14", "architecture": "arm64", "operatingSystem": "macOS 27.0.1",
 "cpuCores": 18, "memoryBytes": 38654705664, "diskFreeBytes": 231081906176}

// LogEntry
{"atMs": 0, "level": "info" | "warning" | "error", "message": "[demo] Task demo-task-1 started."}
```

Runner states: `stopped` → `starting` → `ready` → `busy` → `ready` …, `stopping` → `stopped`; `failed` after five unexpected exits in a row. `taskRef` is Nestor's opaque reference; no task content is kept or shown locally.

## Admission rules

- A runner holds a local **admission token** before it claims a task from Nestor and gives it back when the task ends (or when it dies).
- Tokens are refused when the service is paused, blocked (not associated, unreachable, revoked, reconciling), at `maxConcurrentTasks`, or when the runner already holds one (one task per runner).
- **Pause** and **lowering the ceiling** refuse new tokens only. No running task is stopped. Runners idle for 60 s beyond what is needed are stopped.
- After a restart, the service is `blocked: reconciling` until it has adopted the runner processes still alive (counted as busy until they report idle) and cleared the others. It never launches a runner over a live one.

## Events

Every event has `seq` (increasing) and `atMs`, plus a `type`:

```json
{"seq": 12, "atMs": 1793291543411, "type": "machine.connectionChanged", "connection": {"state": "unreachable", "instance": "nestor.example", "reason": "timeout"}}
{"seq": 13, "atMs": 1793291543412, "type": "machine.configChanged", "revision": 4, "mode": "paused", "maxConcurrentTasks": 2}
{"seq": 14, "atMs": 1793291545000, "type": "machine.loadSample", "sample": {"sampledAtMs": 1793291545000, "measuredCpuPercent": 12.5, "measuredMemoryUsedBytes": 1, "measuredMemoryTotalBytes": 2, "measuredMemoryPressure": "normal", "measuredDiskFreeBytes": 3}}
{"seq": 15, "atMs": 1793291546000, "type": "runner.stateChanged", "runnerId": "9f…", "state": "busy", "generation": 3}
{"seq": 16, "atMs": 1793291546001, "type": "task.started", "runnerId": "9f…", "taskRef": "t-1"}
{"seq": 17, "atMs": 1793291576001, "type": "task.finished", "runnerId": "9f…", "taskRef": "t-1", "outcome": "succeeded"}
{"seq": 18, "atMs": 1793291576002, "type": "admission.changed", "admission": {"state": "paused"}}
{"seq": 19, "atMs": 1793291576003, "type": "service.error", "message": "Could not save the configuration: …"}
```

`outcome` is `succeeded`, `failed`, `runnerLost` (the runner died during the task; Nestor's existing policy decides what happens to it) or `notClaimed`.

The service keeps the last 1024 events. `events` with a `since` older than that fails with `snapshotRequired`: reload `machineStatus` and continue from the newest `seq`. In this version events are **polled**; the macOS client refreshes the status every 3 s. A push subscription on the same connection is a later minor version.

## Errors

| `code` | `retryable` | `details` | Example `message` |
| --- | --- | --- | --- |
| `notAssociated` | no | — | This machine is not associated with Nestor. |
| `revoked` | no | — | This machine's association was revoked in Nestor. |
| `unreachable` | yes | — | Nestor cannot be reached. |
| `unavailable` | yes | — | Association with Nestor is not available in this version yet: … |
| `conflict` | yes | `{"currentRevision": 4}` | The settings changed meanwhile (revision 4). Reload and try again. |
| `invalidArgument` | no | `{"field": "request"}` | Invalid request: unknown variant `format` … |
| `limitOutOfRange` | no | `{"min": 1, "max": 18}` | Choose between 1 and 18 tasks at a time. |
| `runnerNotFound` | no | — | This runner no longer exists. |
| `invalidTransition` | no | `{"from": "busy", "action": "stop"}` | Cannot stop a runner that is busy. |
| `duplicateName` | no | `{"name": "diag"}` | A runner named “diag” already exists on this machine. |
| `permissionDenied` | no | — | Permission denied. |
| `incompatibleVersion` | no | `{"supported": "1.0"}` | Incompatible service: it speaks API 1.0. |
| `snapshotRequired` | no | — | Events were dropped; reload the machine status. |
| `internal` | yes | `{"correlationId": "…"}` | Internal error (…). |

## Demo and live

| | Demo (`--simulate`, or the in-app `DemoRunnerService`) | Live (`nestor-runner-service`) |
| --- | --- | --- |
| `serviceMode` | `demo` → banner shown | `live` → no banner |
| Association | simulated, `demo.nestor.invalid` | not available yet (`unavailable`) |
| Tasks and runner processes | simulated queue, pretend processes | none: admissions stay `blocked: notAssociated` |
| Admission rules, persistence, local API, peer check | real | real |
| Load and machine description | real for `--simulate`; simulated in the in-app demo | real |
| Logs | prefixed `[demo]` | not prefixed |

The demo and live services keep their state in separate directories, so demo data can never be read by a live service.

## Persistence

`state.json` in the data directory (`0600`, written to a temporary file, synced, then renamed): schema version, association, mode, ceiling, revision, and the runners (id, name, generation, pid of the current process). **No secret** — credentials belong in the Keychain, which this version does not use yet. An unreadable file, an unknown schema or a ceiling of 0 starts the service paused with one task at a time, reports `configError`, keeps the bad file as `state.json.corrupt-<pid>` and writes a safe one.
