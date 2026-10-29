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

Workspaces are not assigned here: on the Nestor web app, workspaces are assigned to the machine. The interface never asks for a password or a token: an administrator approves the machine on Nestor with a code the interface shows — never give that code to anyone — then the interface shows the Nestor account the machine would work for, and nothing runs until you confirm it is yours. Nestor issues a short-lived credential for each runner launch. Security design and evidence: [docs/security.md](docs/security.md).

### Claude Code on this Mac (one step, by the Mac's user)

The service never sends nor keeps a Claude Code token. Each runner uses the one its Mac's user keeps, once, in their login keychain:

```sh
claude setup-token                 # makes a long-lived token for Claude Code
nestor-runner claude-token set     # keeps it in the login keychain (macOS); paste it when asked
```

Run it in a terminal of that user's session (the login keychain is the session's). Until it is done, every task on the Mac fails `claude_not_signed_in`, and the app shows **Claude is not signed in** once a runner reports that reason (`run-once` does not report it yet: see docs/security.md).

| Paused | Diagnostics | Busy runner | Failed runner |
| --- | --- | --- | --- |
| ![Paused](docs/screenshots/02-machine-paused.png) | ![Diagnostics](docs/screenshots/03-diagnostics.png) | ![Busy runner](docs/screenshots/04-runner-busy.png) | ![Failed runner](docs/screenshots/05-runner-failed.png) |

| Add a runner (advanced) | Not associated | Service unavailable | Loading |
| --- | --- | --- | --- |
| ![Add a runner](docs/screenshots/06-add-runner.png) | ![Not associated](docs/screenshots/07-not-associated.png) | ![Service unavailable](docs/screenshots/08-service-unavailable.png) | ![Loading](docs/screenshots/09-loading.png) |

| Waiting for approval — never give the code | Confirm the Nestor account (M1) | Claude is not signed in (A7) |
| --- | --- | --- |
| ![Associating](docs/screenshots/12-associating.png) | ![Confirming](docs/screenshots/13-confirming.png) | ![Claude not signed in](docs/screenshots/14-claude-not-signed-in.png) |

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

Demo scenarios (`--demo-scenario`): `populated` (default), `empty`, `notAssociated`, `associating`, `confirming`, `claudeNotSignedIn`, `unavailable`. They apply to the in-app demo only: the app refuses `--demo-scenario` together with `--service` or `--socket`.

With the local service (see [core/README.md](core/README.md)):

```sh
(cd ../../core && cargo run --bin nestor-runner-service)            # live, in another terminal
swift run NestorRunnerManager --service                             # an unsigned build gets read-only access
(cd ../../core && cargo run --features dev-unsigned --bin nestor-runner-service -- --simulate --allow-unsigned-clients)
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

## Local verification

The checks run on the developer's Mac, by hand, before a push: `scripts/verify-local.sh` builds, tests and makes an arm64 release build of the app, formats, tests and lints `core/`, checks that a release build of the service refuses the development-only unsigned fallback (M1), and signs both binaries ad hoc with `scripts/sign-macos.sh` to check the signing configuration (hardened runtime, no `get-task-allow`). It installs nothing and needs no certificate nor privilege. Every pull request carries its result on its head commit: the command, the commit and the outcome.

```sh
scripts/verify-local.sh               # app and core
scripts/verify-local.sh --contract    # also the contract test with the real nestor-runner run-once
```

The contract test with the real `nestor-runner run-once` (`core/tests/contract_run_once.rs`) needs read access to nestor, which is private: `--contract` builds the runner with `scripts/nestor-runner.sh` (git's own credentials, or a local clone in `NESTOR_SOURCE`). nestor runs the same test from its side by hand, with `make contract-check`, against the commit of this repository it pins in `contract/runner-manager.rev`. See [contract/README.md](contract/README.md).

`.github/workflows/macos.yml`, which predates the local service, builds and tests the app only; no check of `core/` and no signing step runs on GitHub.

## Next steps

1. Nestor API work (contract v2, S1 to S7): device authorization, machine credential and runner credentials, eligible count per scope, lease ceiling, pause and scope enforced at claim (S6), the runner's one-job mode (S7). Then an HTTP client behind `NestorApi` and the real runner behind `ProcessLauncher`, on a test Nestor first. Security review before any real integration.
2. Signing with an Apple Developer certificate (A6, to be validated by Brice): it is required for the Secure Enclave key, the Keychain and the verification of the app by the service.
3. Start at login (LaunchAgent, acceptable only with dedicated runner accounts B1(a) or an explicit acceptance, see docs/security.md), installation on the Mac Studio — each to be authorized first.
4. Admission driven by memory pressure and CPU, after real measurements on the Mac Studio.
5. Windows and Linux clients on the same core.

## License

No license has been chosen yet. Until one is added, the default copyright rules apply: the code is visible but not licensed for reuse.
