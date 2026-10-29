# core

Shared core of Nestor Runner Manager, in Rust: the local runner service the interfaces talk to (contract KCA-717, API in [../docs/local-api.md](../docs/local-api.md)).

| Module | Role |
| --- | --- |
| `admission` | When a one-job runner may be launched: mode, ceiling, eligible jobs, one scope at a time. No resource limit. |
| `supervisor` | Runner state machines, launches, drains, terminations, crash backoff, adoption after a restart, events, masked logs. |
| `process` | `ProcessLauncher`: real runner processes (`argv`, credential on fd 3, reports on fd 4, drain on stdin, process group, identity checks). |
| `association` | Device authorization, machine key, disassociation and revocation, runner credentials per launch. |
| `nestor` | `NestorApi` (what the service needs from Nestor) and `FakeNestor` (tests, `--simulate`). |
| `secrets` | `Secret`, Keychain, Secure Enclave key, the A6 software fallback (off). |
| `persist` | `state.json`: atomic, `0600`, no secret, validated when read; anything invalid → paused with 1 task. |
| `ipc` | The local API on a Unix socket: peer classification by code signature, socket checks, instance lock. |
| `mask` | Masking of secrets in everything the service logs. |
| `sys` | Process identity (pid + start time, arguments), socket peers (audit token), code signing, keys, Keychain. |
| `load` | Machine description and load measurements (CPU, memory, memory pressure, free disk). |
| `fake` | Launchers that start nothing: `FakeLauncher` (tests, `--simulate`), `UnavailableLauncher` (live, until the real runner is integrated). |
| `demo` | The simulated Nestor queue behind `--simulate`. |

## Build, test, run

```sh
cd core
cargo test                                   # unit and acceptance tests (fake and stand-in runners, real socket)
cargo run --bin nestor-runner-service        # live: real settings, measurements and API; not associated, no runner
cargo run --bin nestor-runner-service -- --simulate --allow-unsigned-clients   # demo, usable by an unsigned dev build of the app
```

Options: `--data-dir DIR` (default `~/Library/Application Support/Nestor Runner Manager/live` or `…/demo`), `--socket PATH` (default `DIR/run/service.sock`; never a shared temporary directory), `--allow-unsigned-clients` (development only: gives unsigned clients full access, and says so to every client). Without it, an unsigned build of the app only gets read access to the status.

`examples/fake_one_job_runner.rs` is a stand-in for the Nestor runner's one-job mode, used by `tests/process.rs`; it claims nothing from Nestor.

Then, from `apps/macos`: `swift run NestorRunnerManager --service` (live) or `--socket "$HOME/Library/Application Support/Nestor Runner Manager/demo/run/service.sock"` (simulated).

The service runs in your session and stops with it; nothing is installed. Start at login and the real runner come later.
