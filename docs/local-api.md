# Local API of the runner service — version 1.1

The interfaces reach the runner service (`core/`, binary `nestor-runner-service`) through this API. It follows the architecture contract (KCA-717 §4, revised by contract v2 in KCA-724 and the security review KCA-727). The macOS client is `LiveRunnerService`; the Rust server is `core/src/ipc.rs`. Security design and test evidence: [security.md](security.md).

Changes from 1.0: access levels by code signature, `confirmed` on sensitive operations, `cancelAssociation`, the `associating` connection, `liveRunners` and `activeScope` in the status, `scope` on runners, the `scope.violation` event, the `confirmationRequired` error, one-job runners (no admission token). Added in 1.1 without a version change (fields a client may ignore): `effectiveMaxConcurrentTasks` and `cappedByServer` in the status, the `leaseLost` and `released` outcomes, the places reserved for `full` access and the request delays.

## Transport and access

- **Unix domain socket**, one per service. Default path: `~/Library/Application Support/Nestor Runner Manager/live/run/service.sock` (`…/demo/run/…` for `--simulate`). No TCP port is opened.
- Before `bind`, the service refuses a shared temporary directory, requires the socket's directory to be a real directory (not a link) owned by its user, and makes it `0700`; it replaces an existing path only if it is its own socket and nothing answers there. The socket is `0600` and is removed when the service stops cleanly (SIGTERM, SIGINT, SIGHUP).
- Clients check with the kernel (`getpeereid`) that the process serving the socket runs as their own user, and refuse it otherwise.
- One service per data directory: an exclusive `flock` on `service.lock`, released by the kernel if the service crashes.

### Who may do what

Each connection is classified once, from the kernel, when it is accepted:

| Peer | Access |
| --- | --- |
| Another user, or a live runner or one of its descendants | refused (`permissionDenied`, connection closed) |
| The app, verified by **code signature from the audit token** (Developer ID certificate of the team), with the hardened runtime, without `get-task-allow` nor any `com.apple.security.cs.*` exception | `full` |
| The command line tool, verified the same way | `readOnly`: everything but changes and runner `logs` (A4) |
| Anything else of the same user | `basic`: `hello`, `machineInfo`, `machineStatus` (without task references), `connection` |
| Anything else, when a **development build** (Cargo feature `dev-unsigned`, never in a release build nor with `NESTOR_TEAM_ID`) runs with `--allow-unsigned-clients` | `full`, reported as `peerVerification: "unsignedFallback"` |

`hello` tells the client its access. The production code signing requirements (`ipc::developer_id_requirement`) name the app (`fr.kocoa.nestor.runner-manager`), the CLI (`….cli`), the Developer ID certificates and the team id given at build time (`NESTOR_TEAM_ID`); without a team id nothing can be verified by signature.
- Framing: one JSON object per line (UTF-8, `\n`-terminated), at most 64 KiB per request. Requests on a connection are answered in order. At most 16 connections at a time, of which 8 are reserved for `full` access: `basic` and `readOnly` peers get at most 8, so they cannot keep the app out. The first request, and every next one, must come within 10 s; only a `full` connection that asked for `events` may stay quiet up to 300 s.

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
| `hello` | — | `{apiVersion, serviceMode, access, peerVerification, keyProtection}`; `access`: `full` \| `readOnly` \| `basic`; `peerVerification`: `signature` \| `unsignedFallback` \| `none`; `keyProtection`: `secureEnclave` \| `software` \| `memory` |
| `machineInfo` | — | `MachineInfo` |
| `machineStatus` | — | `MachineStatus` |
| `connection` | — | `Connection` |
| `associate` | — | `Connection`, `associating` (device authorization started; the machine key was generated first). Demo service: the simulated Nestor approves at its first poll. Live service: `unavailable` until the Nestor API work exists (S1). |
| `cancelAssociation` | — | `Connection`. Abandons an association in progress; its key is erased. |
| `disassociate` | `confirmed: true` | `Connection`. **Ends running tasks**: runners are terminated, the credential and the key erased even if Nestor cannot be reached; the revocation is retried. |
| `setMode` | `mode`: `automatic` \| `paused`, `expectedRevision`? | `MachineStatus` |
| `setMaxConcurrentTasks` | `value` (1 … `maxConcurrentTasksUpperBound`), `expectedRevision`?, `confirmed` (required to raise it) | `MachineStatus` |
| `activeTasks` | — | `[ActiveTask]` |
| `events` | `since` (sequence, default 0) | `[Event]` with `seq > since` |
| `runners` *(diagnostics)* | — | `[Runner]` |
| `addRunner` *(diagnostics)* | `name`, `startNow`?, `confirmed: true` | `Runner` |
| `start` *(diagnostics)* | `runnerId` | `Runner`. Same regulation as automatic launches: refused while paused, blocked, at the ceiling, or next to a runner of unknown scope. |
| `stop` *(diagnostics)* | `runnerId`, `force`?, `confirmed` (with `force`) | `Runner`. A waiting runner is drained. A `busy` runner needs `force: true` and `confirmed: true`: it is terminated, which ends its task. |
| `remove` *(diagnostics)* | `runnerId` | `{}`. Only `stopped` or `failed` runners. |
| `logs` *(diagnostics)* | `runnerId`, `limit`? (default 200, max 1000) | `[LogEntry]` |

Operations need the access level of the table above: `hello`, `machineInfo`, `machineStatus`, `connection` need `basic`; `runners`, `activeTasks`, `events` need `readOnly`; everything else needs `full`. Otherwise: `permissionDenied`.

`confirmed: true` is sent by the interface only after asking the user; without it a sensitive operation fails with `confirmationRequired`. Lowering the ceiling needs no confirmation: it interrupts nothing.

`expectedRevision` is the `configRevision` the change is based on. If it is stale the change is refused with `conflict` and nothing changes; the client reloads and tries again. Without it, the change applies to the current revision.

## Types

```jsonc
// MachineStatus
{
  "serviceMode": "live",                 // "live" | "demo" — the client shows its demo banner unless "live"
  "connection": {"state": "associated", "instance": "nestor.example", "sinceMs": 1793290000000},
  "mode": "automatic",                   // "automatic" | "paused"
  "admission": {"state": "open"},        // or {"state":"paused"} or {"state":"blocked","reason":"notAssociated"|"unreachable"|"revoked"|"reconciling"}
  "maxConcurrentTasks": 1,               // default 1: what the user asked for
  "effectiveMaxConcurrentTasks": 1,      // what applies: 1 until Nestor announces its scope filter (S6)
  "cappedByServer": false,               // true when the requested value is above it: "limited by the server"
  "maxConcurrentTasksUpperBound": 18,    // number of cores: rejects absurd input, limits no resource
  "configRevision": 3,
  "busyRunners": 1,                      // runners holding a task
  "liveRunners": 1,                      // runner processes alive: what the ceiling counts
  "activeScope": "w-7f3a",               // set while runners are alive: the only scope new runners may claim from (opaque)
  "activeTasks": [{"runnerId": "9f…", "runnerName": "runner-1", "taskRef": "…", "startedAtMs": 1793291000000}],
  "load": {"sampledAtMs": 1793291543411, "measuredCpuPercent": 7.8, "measuredMemoryUsedBytes": 19926319104,
           "measuredMemoryTotalBytes": 38654705664, "measuredMemoryPressure": "normal", "measuredDiskFreeBytes": 231194337280},
  "configError": null                    // set when the stored settings were unreadable (the service then runs paused with 1 task), or when they could not be saved
}

// Connection
{"state": "notAssociated"}
| {"state": "associating", "instance": "…", "userCode": "WDJB-MJHT", "verificationUri": "https://…/machines/approve", "expiresAtMs": 0}
| {"state": "associated", "instance": "…", "sinceMs": 0}
| {"state": "unreachable", "instance": "…", "reason": "…"} | {"state": "revoked"}

// Runner
{"id": "32 hex digits", "name": "runner-1", "state": "busy", "generation": 3, "failureReason": null,
 "taskRef": "…", "scope": "w-7f3a", "createdAtMs": 0, "lastTaskAtMs": 0, "measuredCpuPercent": 250.0, "measuredMemoryBytes": 0}

// MachineInfo
{"name": "…", "model": "Mac17,14", "architecture": "arm64", "operatingSystem": "macOS 27.0.1",
 "cpuCores": 18, "memoryBytes": 38654705664, "diskFreeBytes": 231081906176}

// LogEntry
{"atMs": 0, "level": "info" | "warning" | "error", "message": "[demo] Task demo-task-1 started."}
```

Runner states. A managed runner is a **one-job** process: `stopped` → `starting` (launched) → `ready` (waiting to claim) → `busy` (one task) → exits → `stopped`; the next task launches it again with a new generation. `stopping`: drained (no new claim; its task, if any, finishes) or terminated. `failed` after five unexpected exits in a row, or at once when the runner refuses its launch (exit 64), when Nestor refuses its credential (exit 77: no runner is launched until the connection changes) or when what it started could not be stopped. `taskRef` and `scope` are Nestor's opaque references; no task content is kept or shown locally. Names are 1 to 40 ASCII letters, digits, spaces, `-`, `_` or `.`.

## Admission rules

- **Launching a runner is the admission.** There is no local token. A one-job runner is launched only when the mode is `automatic`, the service is not blocked (not associated, unreachable, revoked, reconciling), fewer runners are alive than `effectiveMaxConcurrentTasks`, and Nestor reports eligible jobs in the active scope. Until Nestor announces `capabilities.scope_filter` (S6), `effectiveMaxConcurrentTasks` is 1 whatever `maxConcurrentTasks` says (KCA-734 §3).
- **Scopes (B1(b)).** While runners are alive they all share one scope (`activeScope`). Launch contract v1 does **not** pass the scope to the runner (`--scope` comes back with S6): until then only Nestor can keep workspaces apart, hence the ceiling of 1. A runner of unknown scope blocks other launches until it ends; a task reported outside its runner's scope raises `scope.violation` and blocks launches until it ends. Nestor enforces the same rule at claim time (S6).
- **Pause**, **lowering the ceiling** and losing the connection only stop launching, and **drain** the runners that have no task yet (they stop claiming and exit). No running task is stopped. Runners waiting 60 s with nothing eligible in their scope are drained too.
- **Termination** (revocation, disassociation, forced stop) is the only thing that ends a running task: SIGTERM to the runner's process group, SIGKILL after 10 s, then a sweep of what the runner started (M3, see security.md).
- After a restart, the service is `blocked: reconciling` until it has adopted the runner processes still alive (only if pid, start time and generation all match; they count toward the ceiling and drain) and cleared the others. It never launches a runner over a live one, and never signals a pid it cannot recognize.
- None of this is a security control: Nestor enforces the lease ceiling, the pause and the scope at claim time.

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
{"seq": 20, "atMs": 1793291576004, "type": "scope.violation", "runnerId": "9f…"}
```

`outcome` is `succeeded`, `failed`, `runnerLost` (the runner died during the task; Nestor's existing policy decides what happens to it), `stopped` (terminated on purpose, or abandoned by the runner), `leaseLost` (the runner lost its lease and stopped the job), `released` (the runner gave the job back to Nestor without running it) or `unknown` (an adopted runner ended, or the runner exited without saying how its job ended). The outcome comes from the runner's `finished` report; the report channel is not trusted for anything else.

Every message — logs, events, error messages — is masked at the source: secrets the service handles and anything that looks like a credential are replaced by `[masked]`.

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
| `confirmationRequired` | no | — | Confirm first: running more tasks at the same time |
| `incompatibleVersion` | no | `{"supported": "1.0"}` | Incompatible service: it speaks API 1.0. |
| `snapshotRequired` | no | — | Events were dropped; reload the machine status. |
| `internal` | yes | `{"correlationId": "…"}` | Internal error (…). |

## Demo and live

| | Demo (`--simulate`, or the in-app `DemoRunnerService`) | Live (`nestor-runner-service`) |
| --- | --- | --- |
| `serviceMode` | `demo` → banner shown | `live` → no banner |
| Association | real flow against a simulated Nestor (`demo.nestor.invalid`) that approves at its first poll | not available yet (`unavailable`): the Nestor API routes do not exist |
| Tasks and runner processes | simulated queue in two scopes, pretend processes | none: admissions stay `blocked: notAssociated` |
| Secrets and machine key | memory only; **never the Keychain** | Keychain and Secure Enclave (need a signed build) |
| Admission rules, persistence, local API, peer check | real | real |
| Load and machine description | real for `--simulate`; simulated in the in-app demo | real |
| Logs | prefixed `[demo]` | not prefixed |

The demo and live services keep their state in separate directories, so demo data can never be read by a live service.

## Persistence

`state.json` in the data directory (`0600`, written to a temporary file, synced, then renamed), schema 2: association, machine id, mode, ceiling, revision, the runners (id, name, generation, pid, process start time, scope) and a pending revocation (signed, no secret). **No secret** — the machine credential and key are in the Keychain and the Secure Enclave; runner credentials are never stored. The file is validated when read, schema and bounds; an unreadable file or **any invalid value** starts the service paused with one task at a time, reports `configError`, keeps the bad file as `state.json.corrupt-<pid>` and writes a safe one. Schema 1 files are read.
