# core

Shared core of Nestor Runner Manager, in Rust: the local runner service the interfaces talk to (contract KCA-717, API in [../docs/local-api.md](../docs/local-api.md)).

| Module | Role |
| --- | --- |
| `admission` | Admission tokens: pause, ceiling of tasks at a time, one task per runner. No resource limit. |
| `supervisor` | Runner state machines, pool sizing, idle stop, crash restart with backoff, adoption after a restart, events, logs. |
| `persist` | `state.json`: atomic, `0600`, no secret; unreadable → paused with 1 task. |
| `ipc` | The local API on a Unix socket, peer user check, instance lock. |
| `load` | Machine description and load measurements (CPU, memory, memory pressure, free disk). |
| `fake` | Launchers that start nothing: `FakeLauncher` (tests, `--simulate`), `UnavailableLauncher` (live, until the real runner is integrated). |
| `demo` | The simulated Nestor queue behind `--simulate`. |

## Build, test, run

```sh
cd core
cargo test                                   # unit and acceptance tests (fake runners, real socket)
cargo run --bin nestor-runner-service        # live: real settings, measurements and API; not associated, no runner
cargo run --bin nestor-runner-service -- --simulate   # demo: simulated association, queue and runners
```

Options: `--data-dir DIR` (default `~/Library/Application Support/Nestor Runner Manager/live` or `…/demo`), `--socket PATH` (default `DIR/run/service.sock`).

Then, from `apps/macos`: `swift run NestorRunnerManager --service` (live) or `--socket "$HOME/Library/Application Support/Nestor Runner Manager/demo/run/service.sock"` (simulated).

The service runs in your session and stops with it; nothing is installed. Start at login and the real runner come later.
