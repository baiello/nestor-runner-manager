# Architecture

Nestor Runner Manager is planned as native interfaces per operating system on top of one shared core written in Rust, with OS-specific integrations (services, keychains, resource limits). This repository starts with the macOS interface; the core and the other clients come next.

```
┌──────────────────────────┐   ┌──────────────┐   ┌──────────────┐
│ macOS app (SwiftUI)      │   │ Windows app  │   │ Linux app    │
│  views                   │   │  (later)     │   │  (later)     │
│  RunnerManagerModel      │   └──────┬───────┘   └──────┬───────┘
│  RunnerService ◄─────────┼──┐       │                  │
│   ├ DemoRunnerService    │  │       │                  │
│   └ (live service)       │  │       │                  │
└──────────────────────────┘  │  local authenticated IPC (later)
                              ▼       ▼                  ▼
                   ┌────────────────────────────────────────┐
                   │ core/ — shared Rust service (later)    │
                   │  supervises the runner processes,      │
                   │  talks to Nestor, keeps credentials    │
                   └────────────────────────────────────────┘
```

## The macOS app

`apps/macos` is a Swift package with two targets:

- **`RunnerManagerKit`** — no SwiftUI. It holds:
  - the models (`MachineInfo`, `NestorConnection`, `Runner`, `ResourceLimits`, `ResourceUsage`, `LogEntry`, `NewRunnerRequest`);
  - **`RunnerService`**, the only way the interface reaches the machine: machine information, connection, association, list, add, start, stop, remove and logs, all `async throws`. Its `mode` (`.demo` or `.live`) decides whether the demo banner is shown;
  - **`DemoRunnerService`**, the simulated implementation, kept in memory, with scenarios for each screen state;
  - **`RunnerManagerModel`**, the observable interface state (loading / loaded / failed per section, commands in flight, last error), tested without SwiftUI.
- **`NestorRunnerManager`** — the SwiftUI app. Views read `RunnerManagerModel` and never call a service directly.

Replacing the demo with the real core means writing one more `RunnerService` implementation (for example `CoreRunnerService`, talking to the Rust service) and choosing it at launch. The views and the model stay as they are.

## Rules the prototype keeps

- The interface never asks for or stores a secret. Association is meant to be confirmed in the browser; each runner's credential is issued by Nestor and kept by the service.
- No workspace is assigned locally: that happens on the Nestor web app.
- Every simulated command and log line says so (`[demo]`, “Simulated”), and the demo banner cannot be hidden.
- A runner must be stopped before it is removed.
