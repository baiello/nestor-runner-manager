# Security of the runner service

How the local service (`core/`) meets the service criteria of contract v2 (KCA-724, S8 and S9), with the amendments of the security review (KCA-727, A1 to A7) and Brice's decisions B1 and B6 (KCA-716). What is real, what is simulated and what is still missing is listed at the end.

## Principles

1. **The local control is never a security boundary** (principle 7). Pause, ceiling, scope rule and anti-duplicate launches are good-faith regulation. Exclusive execution, the lease ceiling, the pause and the scope rule are enforced by Nestor at claim time (S6).
2. **"Never interrupt" applies to pause and to lowering the ceiling only** (principle 8). Revocation, disassociation and a forced stop from the diagnostics end running tasks.

## S8: the service

| Criterion | How | Tests |
| --- | --- | --- |
| S8.1 No admission token; one-job runners | Launching a runner *is* the admission: `Regulator::plan` allows a launch only when automatic, not blocked, fewer live runners than the ceiling, and eligible jobs in the active scope. A runner claims at most one job, runs it, exits. A second claim is refused and logged. | `admission` unit tests; `tests/supervisor.rs` (ceiling under load and with 32 concurrent calls, pause, lowering, raising, idle drain, crash backoff, one job per runner) |
| S8.2 `flock`; PID reconciliation with start time and generation | `InstanceLock` (`flock` on an open descriptor, released by the kernel on a crash). Each runner record keeps its pid, the process start time from the kernel and its generation; the runner's `argv` carries `--runner-id` and `--generation`. A pid is adopted or signalled only if all three match; otherwise it is forgotten, never signalled. | `sys` unit test (reused pid), `tests/persistence.rs` (adoption, reused pid never adopted nor signalled), `tests/process.rs` (generation marker on a real process) |
| S8.3 Peer authentication | Each connection is classified from the kernel: same user id; not a live runner nor one of its descendants; the **audit token** (not the pid) checked against the code signing requirement of the app (full access) and of the CLI (read-only); hardened runtime and no `get-task-allow` required. Everything else gets `basic` access (status only). The unsigned fallback exists for development only, **off by default** (`--allow-unsigned-clients`), and is reported to every client (`peerVerification: "unsignedFallback"`, orange banner). | `tests/ipc.rs` (copies of Apple's `nc` re-signed ad hoc: requirement + runtime → full or read-only; with `get-task-allow` or without runtime → basic; unsigned test process → basic; every write refused), `tests/process.rs` (a runner process is refused even with the fallback on) |
| S8.3 Socket | In `~/Library/Application Support/Nestor Runner Manager/<mode>/run/`. Before `bind`: refused in shared temporary directories; the directory must be a real directory (not a link) owned by the user, made `0700`; an existing path is replaced only if it is our socket and nothing answers. Socket `0600`. **Removed on a clean stop** (SIGTERM, SIGINT, SIGHUP), only if it is still the same inode (QA KCA-726). | `tests/ipc.rs`; manual run (below) |
| S8.3 Server UID checked by the client | The Rust client (`ipc::connect`) and the Swift client (`LiveRunnerService`) refuse a socket served by another user (`getpeereid`), so a fake service cannot announce `live`. | Code; a second macOS user was not available to test it (needs an admin) |
| S8.4 State validation | Schema *and* bounds: ceiling within 1…upper bound, known states (unknown fields refused), runner ids and ASCII names, scopes, pid only with its start time, no stored association in progress. Any invalid value → **paused, ceiling 1, visible `configError`**, file kept aside. No secret in the state file. | `tests/persistence.rs` (11 invalid variants) |
| S8.5 Disassociation and revocation | Every live runner is terminated (SIGTERM to its process group, SIGKILL after the grace period), runner records and their directories erased, the machine credential and key erased **even if Nestor cannot be reached**. A revocation request signed by the machine key *before* it is erased is kept (it holds no secret, it allows nothing but revoking this machine) and retried until Nestor confirms. A revocation from Nestor does the same. The interface asks for a confirmation that says running tasks end. | `tests/association.rs`, `tests/supervisor.rs`, `tests/process.rs` (real process group terminated) |
| S8.6 Demo never uses the Keychain | `secrets::stores_for(Demo)` returns memory-only stores; only `Live` gets the Keychain and the Secure Enclave. | `tests/association.rs` |
| S8.6 Live refuses demo scenarios | The demo is a separate mode (`--simulate`) with its own data directory; a live service has no simulated Nestor and answers `unavailable` to `associate`. | `tests/ipc.rs` |
| S8.6 Input revalidation, `argv`, limits | Names: 1 to 40 ASCII letters, digits, space, `-`, `_`, `.` (no look-alikes), checked by the service whatever the client did; runner ids 32 hex digits; scopes `[A-Za-z0-9_-]{1,64}`. Runners are launched with an `argv` array, never a shell. 64 KiB per request, 16 connections, 64 runners, 1000 log lines per read. | `tests/supervisor.rs`, `tests/ipc.rs`, `tests/process.rs` |
| S8.7 B1(b) | See below. | `admission` unit tests; `tests/supervisor.rs` |
| A4 | CLI read-only, without logs, in this version. Hardened runtime without `get-task-allow` in the build configuration (`signing/`, `scripts/sign-macos.sh`), checked by CI on ad hoc signatures and required by the service from its clients. | `tests/ipc.rs`; `scripts/verify-signature.sh` |
| N5 Masking at the source | Every runner log line, event and API error message goes through `mask::Redactor` before it is stored: the secrets the service handles (registered as soon as they exist: device code, machine credential, runner credentials), known token prefixes, `Bearer`/`Basic`, URL credentials, `token=`/`password:` values. The `logs` operation can only replay masked text. | `mask` unit tests; `tests/supervisor.rs`, `tests/ipc.rs`, `tests/process.rs` (a runner that prints its credential) |

### Credential handling (B5, A3)

The service never stores a runner credential. For each launch it asks Nestor for a fresh one (proof of possession of the machine key, one claim per credential per A3, scopes computed by the server) and writes it to a pipe the runner inherits as fd 3, reads once and closes. It is never in an argument, the environment or a file. The runner's environment is cleared, then given `PATH`, `LANG` and its own `HOME` and `TMPDIR`. Checked on real processes in `tests/process.rs`.

## B1(b): no tasks of different workspaces side by side

Brice's decision: concurrency above 1 is limited, for now, to tasks of the same scope (workspace). This is **not** an acceptance of the risk of running several workspaces under one macOS user (A1): what actually protects one task from another today is the confinement of the tools (Claude Code restricted, no shell, per-job `HOME` and `TMPDIR`), not the number of tasks. Any tool that runs commands requires B1(a), dedicated runner accounts.

**What the service does (regulation).** While more than one runner may be alive, every live runner is pinned to the same scope, and each runner is launched with that scope as a *restrictive filter* for its claim (`--scope`). With nothing alive, the scope with the most eligible jobs is chosen. A runner whose scope is unknown (adopted after a restart without a recorded scope, started by hand) blocks further launches until it ends. A runner that reports a job outside its scope raises `scope.violation`; its task is not interrupted, but nothing else starts until it ends.

**What the server must do (enforcement, S6 and A2).** The local rule cannot guarantee the property: Nestor picks the job at claim time, and the eligible count per scope is only a snapshot. Nestor must therefore **apply the scope at claim time**: honour the restrictive scope filter the runner sends, and refuse a claim of another scope while the machine holds an active lease in a different one. This belongs to S6 (lease with fencing, ceiling and pause on the server), with its concurrency test, and must be in place before any real use with more than one task at a time.

## S9: association

- **Key first.** `associate` generates the machine key before contacting Nestor and sends only its public part. Live: a P-256 key generated in the **Secure Enclave** (non-exportable), kept in the data protection Keychain.
- **Device authorization** (RFC 8628). The connection becomes `associating` with a user code and the address of the Nestor page; an **administrator** approves it there (B6). The device code is secret: in memory only, masked everywhere, never stored. Polls follow the server's interval and are signed with the machine key. Denied or expired: the key is erased. Cancel is possible.
- **Credential storage.** The machine credential goes to the **data protection Keychain**, whose access is bound to the code signature (team and access group), not to a path (N2).
- **States** `associating` and `revoked` are exposed to the interface.
- Tested against a fake Nestor (`nestor::FakeNestor`), which applies the server rules the service relies on: hashed single-use device code, expiry, approval on the web only, proof of possession on every call, revocation signature. `tests/association.rs`.

### A6: signing prerequisite and the software fallback

The Secure Enclave key and the Keychain access group need a **signed build** with a `keychain-access-groups` entitlement, so an Apple Developer certificate (a purchase, to be validated by Brice). An unsigned build gets an error (`errSecMissingEntitlement`, `-34018`, or `-25308` here) and the service reports it: **association is impossible until the signing is in place**. The weaker fallback of A6 — a software key kept in the Keychain — is coded behind the `MachineKeyStore` interface (`SoftwareKeyStore`, `KeyStoreWithFallback`) and **disabled**: the service binary never enables it, and enabling it is Brice's decision. Tested both ways in `secrets` unit tests and `tests/association.rs`.

## Preparing a real installation (contract §6.B), with the N3 correction

Nothing of this is authorized or done in this lot.

- **§6.B.2 (unchanged):** the binary and the plist must not be modifiable by the account that runs the tasks. With a LaunchAgent under the user's own id, a task that escapes its confinement can modify them: acceptable only with B1(a) or an explicit acceptance.
- **§6.B.3 (corrected, N3):** the LaunchAgent stays the first recommendation **only with B1(a)** (dedicated runner accounts, which cannot write the service's binary and plist) **or with Brice's explicit acceptance of that risk**. B1(b) does **not** make it acceptable: limiting concurrency to one scope protects neither the binary nor the plist. If B1(a) is chosen, a LaunchDaemon or a LaunchAgent of an account dedicated to the service is to be compared.
- **§6.B.5:** signing and notarization are a prerequisite (A6, above), not a convenience.

## Real, simulated, missing

| | Status |
| --- | --- |
| Admission regulation, scopes, drains, terminations, crash backoff, adoption, persistence, validation, local API, peer classification, masking | **Real**, tested with fake runners and with real processes (`examples/fake_one_job_runner.rs`) |
| Process launcher (`argv`, credential pipe, environment, process group, identity checks) | **Real**, tested with a stand-in runner; the Nestor runner's one-job mode (S7) does not exist yet |
| Association, runner credentials, queue, revocation | **Interface real, server simulated**: the Nestor API routes (S1, S2, S4) do not exist, so a live service answers `unavailable` to `associate` and launches nothing |
| Secure Enclave key, Keychain storage | **Coded, not exercised**: needs a signed build (A6) |
| Code signing classification | **Real** (tested with ad hoc signatures); the production requirements need the team id (`NESTOR_TEAM_ID` at build time) |
| Scope enforcement at claim, lease ceiling and pause on the server | **Server work (S6)**, not in this repository |

Observed while testing: when many runner processes are launched at once by the test suite, a freshly executed stand-in runner was occasionally killed (SIGKILL) before its first report. The service sends no SIGKILL at that point (checked by tracing every signal it sends); the cause was not identified. The supervisor treats it as a crash and launches again after the backoff, which is the intended behaviour; the tests wait the way the service does (ticking) so they cover it.
