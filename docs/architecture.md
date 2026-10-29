# Architecture

Nestor Runner Manager has native interfaces per operating system on top of one shared core written in Rust. The machine is the main object: it is associated with Nestor once, and a local service manages its runners. Runners are a technical detail, shown in the diagnostics. The design contract is KCA-717, revised by contract v2 (KCA-724) and the security review KCA-727; the local API is specified in [local-api.md](local-api.md), the security design and its evidence in [security.md](security.md).

```
┌──────────────────────────────┐   ┌──────────────┐   ┌──────────────┐
│ macOS app (SwiftUI)          │   │ Windows app  │   │ Linux app    │
│  machine view, diagnostics   │   │  (later)     │   │  (later)     │
│  RunnerManagerModel          │   └──────┬───────┘   └──────┬───────┘
│  RunnerService ◄─────────────┼──┐       │                  │
│   ├ DemoRunnerService (demo) │  │       │                  │
│   └ LiveRunnerService ───────┼──┤  local API: JSON lines on a Unix socket; access
└──────────────────────────────┘  │  by code signature (local-api.md, security.md)
                                  ▼
               ┌──────────────────────────────────────────┐
               │ core/ — nestor-runner-service (Rust)     │
               │  Regulator   one-job launches: pause,    │
               │              ceiling, one scope at once  │
               │  Supervisor  runner state machines,      │
               │              drains, terminations, crash │
               │              backoff, adoption on start  │
               │  Associator  device authorization, key,  │
               │              revocation  ──► NestorApi   │
               │  FileStore   state.json (no secret)      │
               │  Secrets     Keychain, Secure Enclave    │
               │  Launcher    ProcessLauncher (argv, fd 3 │
               │              credential) | fake          │
               └──────────────────────────────────────────┘
```

## Rules

- **Admission is not a resource quota.** The machine accepts at most `maxConcurrentTasks` tasks at the same time (1 by default, adjustable). No runner has a processor or memory limit (KCA-714); load figures are measurements.
- **One job per runner process.** Launching a "one job" runner *is* the admission: it claims at most one task, runs it and exits. Several runners per machine; each keeps a stable identity and a new generation per launch.
- **One scope at a time** (B1(b)): runners alive together all claim from the same workspace scope; Nestor enforces it at claim time (S6).
- **Pause** holds back new tasks; **lowering the ceiling** too. Neither stops a running task: runners without a task are drained. Only revocation, disassociation and a forced stop end a running task.
- **No double launch.** Every supervisor operation runs under one lock; reports from an older generation are ignored. One service per data directory (`flock`). After a restart nothing is launched until the processes left running are adopted (pid, start time and generation must all match) or cleared.
- **Crashes** relaunch with an exponential backoff (1 s, 2 s, 4 s … up to 5 min); after five in a row the runner is `failed` and left for diagnosis. A task lost with its runner is reported `runnerLost`; Nestor's existing policy decides what happens to it.
- **Workspaces are assigned to the machine on the Nestor web app.** The service never asks for a scope; Nestor computes what each runner credential may claim.
- **Secrets** never go through the interface, the local API, the state file or the logs (masked at the source). The machine credential is in the Keychain, the machine key in the Secure Enclave; runner credentials are issued per launch and passed on an inherited pipe, never stored.
- **The local control is regulation, not a security boundary**: Nestor enforces leases, ceiling, pause and scope.
- **Demo is never presented as real.** Each service reports `serviceMode`; the interface shows the demo banner unless it is `live`. Demo logs start with `[demo]`. Demo and live keep their state apart.
- **Fail safe.** Unreadable settings start the service paused with one task at a time and show the error.

## The macOS app

`apps/macos` is a Swift package:

- **`RunnerManagerKit`** (no SwiftUI): models; `RunnerService`, the only way the interface reaches the machine (machine status, mode, ceiling, association; per-runner operations as diagnostics); `DemoRunnerService` (in memory); `LiveRunnerService` (local API client); `RunnerManagerModel` (observable state, tested without SwiftUI).
- **`NestorRunnerManager`**: the SwiftUI app. *This Mac* shows the connection, the tasks controls (automatic / paused, tasks at a time), the tasks running now, the load and the machine. *Diagnostics › Runners* lists the runners, their pages (state, measured usage, task, stop / start / remove, logs) and an advanced “add a runner by hand”.

`swift run NestorRunnerManager` uses the in-app demo; `--service` (default socket) or `--socket <path>` uses `LiveRunnerService`.

## The core

`core/` is a Rust crate (`nestor-runner-core`) with the binary `nestor-runner-service`, run in the user's session — no privileged installation. Modules: `admission`, `supervisor`, `process`, `sweep`, `association`, `nestor`, `secrets`, `persist`, `ipc`, `mask`, `load`, `sys`, `fake`, `demo`. The launcher speaks launch contract v1 of `nestor-runner run-once`, whose fixtures are vendored in `contract/run-once-v1.json`. See [core/README.md](../core/README.md).

## Not in this version

- The Nestor API for machines (S1 to S6): until it exists, the live service has no `NestorApi`, answers `unavailable` to `associate` and launches nothing. The runner's one-job mode exists (`nestor-runner run-once`, S7); the launcher follows its contract v1, is tested against a double that checks the vendored fixtures, and against the real binary built at the nestor commit pinned in `contract/nestor.rev` (`core/tests/contract_run_once.rs`, R2 of KCA-737, the `contract` job of the CI).
- Signing (A6): the Secure Enclave key, the Keychain and the verification of the app by the service need it. Start at login (LaunchAgent), notarization, installation on the Mac Studio — not authorized in this lot (contract §6.B, see security.md).
- Load-driven admission (CPU, memory pressure): after real measurements on the Mac Studio (KCA-717 §9).
