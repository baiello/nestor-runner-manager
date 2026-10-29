# Nestor Runner Manager

A desktop application and a local service to put a machine at Nestor's disposal. You associate the machine with Nestor once; the service then starts and stops the runners — the programs that take Nestor's tasks — to match the tasks the machine accepts. The machine is what you manage; runners are a detail shown in the diagnostics.

> **Status: prototype, demo and partial live.**
> - The macOS app runs by default on **simulated data** (yellow “Demo mode” banner on every screen).
> - The local service in `core/` is real — settings, admission rules, persistence, measurements, local API — but it is **not connected to Nestor yet** and starts **no real runner**: association answers “not available yet” and the machine takes no task. Started with `--simulate`, it runs a simulated queue and pretend runners, and says so (`serviceMode: demo`, banner, `[demo]` logs).
> - Nothing is installed, no secret is asked for or stored.

![This Mac](docs/screenshots/01-machine.png)

## What it does today

- **This Mac**: the association with Nestor; **Tasks** — *Automatic* or *Paused*, and how many **tasks at a time** the machine accepts (1 by default, adjustable); the tasks **running now**; the **load** (processor, memory, memory pressure, free disk — measurements); the machine.
- Pausing or lowering the number of tasks at a time never stops a running task: it only holds back new ones. The service starts a runner per accepted task and stops idle runners after a minute.
- **Diagnostics › Runners**: the runners, each with its state, measured usage, current task, start / stop / remove and logs; stopping a busy runner asks for a confirmation. Adding a runner by hand is an advanced diagnostic, not the normal path.

A runner takes **one task at a time**; that task may use several threads and processes. There is **no processor or memory limit** per runner: the number of tasks at a time regulates admissions, and the figures shown are measurements.

Workspaces are not assigned here: on the Nestor web app, a workspace is assigned to runners or pools. The interface never asks for a password or a token; in the real version the association is confirmed in the browser and Nestor issues each runner its own credential.

| Paused | Diagnostics | Busy runner | Failed runner |
| --- | --- | --- | --- |
| ![Paused](docs/screenshots/02-machine-paused.png) | ![Diagnostics](docs/screenshots/03-diagnostics.png) | ![Busy runner](docs/screenshots/04-runner-busy.png) | ![Failed runner](docs/screenshots/05-runner-failed.png) |

| Add a runner (advanced) | Not associated | Service unavailable | Loading |
| --- | --- | --- | --- |
| ![Add a runner](docs/screenshots/06-add-runner.png) | ![Not associated](docs/screenshots/07-not-associated.png) | ![Service unavailable](docs/screenshots/08-service-unavailable.png) | ![Loading](docs/screenshots/09-loading.png) |

Through the real local service (diagnostics):

| Live — no banner, not associated, no runner | `--simulate` — 2 tasks at a time, demo banner |
| --- | --- |
| ![Live service](docs/screenshots/10-live-service-diagnostics.png) | ![Simulated service](docs/screenshots/11-simulated-service-diagnostics.png) |

## Repository layout

```
apps/macos/      macOS app (Swift, SwiftUI), a Swift package
  Sources/RunnerManagerKit/     models, the RunnerService contract, demo and live services, the view model
  Sources/NestorRunnerManager/  the SwiftUI views
  Tests/RunnerManagerKitTests/  tests of the services, the wire format and the view model
apps/windows/    future Windows client (empty)
apps/linux/      future Linux client (empty)
core/            shared core in Rust: the local runner service (admission, supervision, persistence, local API)
docs/            architecture, local API specification and screenshots
```

See [docs/architecture.md](docs/architecture.md) for the design and [docs/local-api.md](docs/local-api.md) for the local API.

## Build and run (macOS)

Requirements: macOS 14 or later, Apple Silicon first (the target machine is a Mac Studio), and Xcode 16 or later — or the Command Line Tools with Swift 6.

```sh
cd apps/macos
swift build                                   # debug build
swift test                                    # tests
swift run NestorRunnerManager                 # launch the app
swift run NestorRunnerManager --demo-scenario notAssociated
swift build -c release --arch arm64           # Apple Silicon release build
lipo -archs "$(swift build -c release --arch arm64 --show-bin-path)/NestorRunnerManager"   # → arm64
```

Demo scenarios (`--demo-scenario`): `populated` (default), `empty`, `notAssociated`, `unavailable`.

With the local service (see [core/README.md](core/README.md)):

```sh
(cd ../../core && cargo run --bin nestor-runner-service)            # live, in another terminal
swift run NestorRunnerManager --service
(cd ../../core && cargo run --bin nestor-runner-service -- --simulate)
swift run NestorRunnerManager --socket "$HOME/Library/Application Support/Nestor Runner Manager/demo/run/service.sock"
```

To regenerate the screenshots: `swift run NestorRunnerManager --snapshot ../../docs/screenshots`, rendered off-screen from the demo service; add `--socket <path> --snapshot-prefix <name>` to render from a running service. Captures of a real service's machine view show the machine's name: do not commit them.

You can also open `apps/macos/Package.swift` in Xcode and run the `NestorRunnerManager` scheme.

### Command Line Tools only, on macOS 27

The macOS 27 SDK of the Command Line Tools declares SwiftUI's `@State` as a macro whose plugin ships only with Xcode, and the tools do not load the Swift Testing macros on their own. Without Xcode, build against the macOS 26 SDK the tools also install, and pass the testing plugin:

```sh
export DEVELOPER_DIR=/Library/Developer/CommandLineTools
export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk
swift build
swift test -Xswiftc -load-plugin-library -Xswiftc /Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing/libTestingMacros.dylib
```

## Continuous integration

`.github/workflows/macos.yml` builds, tests and makes an arm64 release build of the app, and tests and lints `core/`, on GitHub's macOS runner for each pull request and each push to `main`. GitHub-hosted runners are free for public repositories.

## Next steps

1. Nestor API work (KCA-719): association by device authorization, one credential per runner, telemetry, and admission against the runner's existing claim protocol. Then the real runner behind the service's launcher, on a test Nestor first.
2. Credentials in the Keychain; security review of the association and of the local API (KCA-720).
3. Start at login (LaunchAgent), signing, installation on the Mac Studio — each to be authorized first.
4. Admission driven by memory pressure and CPU, after real measurements on the Mac Studio.
5. Windows and Linux clients on the same core.

## License

No license has been chosen yet. Until one is added, the default copyright rules apply: the code is visible but not licensed for reuse.
