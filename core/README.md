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
cargo test                                   # unit and acceptance tests (fake runners, run-once double, real socket)
cargo run --bin nestor-runner-service        # live: real settings, measurements and API; not associated, no runner
cargo run --features dev-unsigned --bin nestor-runner-service -- --simulate --allow-unsigned-clients
                                             # demo, usable by an unsigned dev build of the app
../scripts/release-service.sh                # release build; refuses the dev-unsigned feature (M1)
```

Options: `--data-dir DIR` (default `~/Library/Application Support/Nestor Runner Manager/live` or `…/demo`), `--socket PATH` (default `DIR/run/service.sock`; never a shared temporary directory), `--allow-unsigned-clients` (only in a development build with the `dev-unsigned` Cargo feature and without `NESTOR_TEAM_ID`: gives unsigned clients full access, and says so to every client; any other build rejects it with exit 64). Without it, an unsigned build of the app only gets read access to the status. Usage errors exit 64.

`examples/run_once_double.rs` is a double of `nestor-runner run-once` that checks launch contract v1 against `contract/run-once-v1.json` (argv, fds 3 and 4, credential JSON, reports, drain), used by `tests/process.rs`; it claims nothing from Nestor. `tests/contract_fixtures.rs` checks the launcher and the report reader against the same fixtures.

Then, from `apps/macos`: `swift run NestorRunnerManager --service` (live) or `--socket "$HOME/Library/Application Support/Nestor Runner Manager/demo/run/service.sock"` (simulated).

The service runs in your session and stops with it; nothing is installed. Start at login and the real runner come later.
