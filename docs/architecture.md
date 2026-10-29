# Architecture

Nestor Runner Manager has native interfaces per operating system on top of one shared core written in Rust. The machine is the main object: it is associated with Nestor once, and a local service manages its runners. Runners are a technical detail, shown in the diagnostics. The design contract is KCA-717; the local API is specified in [local-api.md](local-api.md).

```
┌──────────────────────────────┐   ┌──────────────┐   ┌──────────────┐
│ macOS app (SwiftUI)          │   │ Windows app  │   │ Linux app    │
│  machine view, diagnostics   │   │  (later)     │   │  (later)     │
│  RunnerManagerModel          │   └──────┬───────┘   └──────┬───────┘
│  RunnerService ◄─────────────┼──┐       │                  │
│   ├ DemoRunnerService (demo) │  │       │                  │
│   └ LiveRunnerService ───────┼──┤  local API: JSON lines on a Unix socket,
└──────────────────────────────┘  │  owner-only (docs/local-api.md)
                                  ▼
               ┌──────────────────────────────────────────┐
               │ core/ — nestor-runner-service (Rust)     │
               │  Regulator   admission tokens, pause,    │
               │              tasks-at-a-time ceiling     │
               │  Supervisor  runner state machines, pool │
               │              sizing, idle stop, crash    │
               │              restart, adoption on start  │
               │  FileStore   state.json (no secret)      │
               │  Launcher    fake (tests, --simulate) |  │
               │              real runner (not yet)       │
               └──────────────────────────────────────────┘
```

## Rules

- **Admission is not a resource quota.** The machine accepts at most `maxConcurrentTasks` tasks at the same time (1 by default, adjustable). No runner has a processor or memory limit (KCA-714); load figures are measurements.
- **One task per runner**, several runners per machine. A runner needs an admission token from the service before it claims a task from Nestor, and gives it back when the task ends.
- **Pause** holds back new tasks; **lowering the ceiling** too. Neither stops a running task. Idle runners above what is needed are stopped after 60 s.
- **No double launch.** Every supervisor operation runs under one lock; each runner has a generation, and reports from an older generation are ignored. One service per data directory (`flock`). After a restart nothing is admitted until the processes left running are adopted (counted busy until they report idle) or cleared.
- **Crashes** restart with an exponential backoff (1 s, 2 s, 4 s … up to 5 min); after five in a row the runner is `failed` and left for diagnosis. A task lost with its runner is reported `runnerLost`; Nestor's existing policy decides what happens to it.
- **Workspaces are assigned on the Nestor web app.** The service holds no right on workspaces; each runner will serve what Nestor grants its own credential.
- **Secrets** never go through the interface, the local API, the state file or the logs. Nestor issues each runner its own credential, to be kept in the Keychain (not in this version).
- **Demo is never presented as real.** Each service reports `serviceMode`; the interface shows the demo banner unless it is `live`. Demo logs start with `[demo]`. Demo and live keep their state apart.
- **Fail safe.** Unreadable settings start the service paused with one task at a time and show the error.

## The macOS app

`apps/macos` is a Swift package:

- **`RunnerManagerKit`** (no SwiftUI): models; `RunnerService`, the only way the interface reaches the machine (machine status, mode, ceiling, association; per-runner operations as diagnostics); `DemoRunnerService` (in memory); `LiveRunnerService` (local API client); `RunnerManagerModel` (observable state, tested without SwiftUI).
- **`NestorRunnerManager`**: the SwiftUI app. *This Mac* shows the connection, the tasks controls (automatic / paused, tasks at a time), the tasks running now, the load and the machine. *Diagnostics › Runners* lists the runners, their pages (state, measured usage, task, stop / start / remove, logs) and an advanced “add a runner by hand”.

`swift run NestorRunnerManager` uses the in-app demo; `--service` (default socket) or `--socket <path>` uses `LiveRunnerService`.

## The core

`core/` is a Rust crate (`nestor-runner-core`) with the binary `nestor-runner-service`, run in the user's session — no privileged installation. Modules: `admission`, `supervisor`, `persist`, `ipc`, `load`, `fake`, `demo`. See [core/README.md](../core/README.md).

## Not in this version

- The real runner: the live service has no launcher yet, so it provisions and starts nothing, and association with Nestor answers `unavailable`. Both need the Nestor API work framed in KCA-719 (device authorization, per-runner credentials, telemetry, admission against the existing claim protocol).
- Keychain storage, start at login (LaunchAgent), signing and notarization, installation on the Mac Studio — not authorized in this lot (KCA-717 §6.B).
- Load-driven admission (CPU, memory pressure): after real measurements on the Mac Studio (KCA-717 §9).
