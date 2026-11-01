# Starting before login: the service as a LaunchDaemon

The service contract for starting `nestor-runner-service` when the Mac starts, before anyone logs in. It follows Brice's decisions of 2026-11-01 on KCA-716:

- **Start before login**: a LaunchDaemon, which Brice installs with `sudo`.
- **One account**: the service and its runners run as Brice's own macOS account. No second account, no account dedicated to the runners (B1(a) is not chosen).
- **No purchase**: no Developer ID certificate and no notarization for now. The builds used for the trials are signed ad hoc.

Nothing in this repository installs the daemon, and nothing of it was installed on the Mac Studio. This document is what the installation must satisfy and the procedure Brice runs; the points marked **Open** need a decision or code before a real installation.

## The contract

| # | Rule | Why |
| --- | --- | --- |
| D1 | One system job, label `fr.kocoa.nestor.runner-service`, plist `/Library/LaunchDaemons/fr.kocoa.nestor.runner-service.plist`. | Loaded by `launchd` at boot, in the system domain, without a login session. |
| D2 | `UserName` is Brice's short name and `GroupName` is `staff`: `launchd` starts the service as that account, never as root. | One account (decision). The service needs no privilege: no part of it runs as root. |
| D3 | The binaries (`nestor-runner-service`, `nestor-runner`) and the plist are owned by `root:wheel`, `0755` and `0644`, in `/Library/Application Support/Nestor Runner Manager/bin/` and `/Library/LaunchDaemons/`. No component of either path is writable by Brice's account. | §6.B.2: the account that runs the tasks must not be able to change what `launchd` starts. A task that escapes its confinement runs as Brice, so it cannot replace the binary or the plist without `sudo`. This is what the LaunchAgent could not give with one account (§6.B.3, N3). |
| D4 | `ProgramArguments` names the absolute path of the binary and `--data-dir "/Users/<brice>/Library/Application Support/Nestor Runner Manager/live"`. No other option; never `--simulate`, never `--allow-unsigned-clients`. | The data directory, its state and its socket stay where the app looks for them (`live/run/service.sock`). A release build rejects `--allow-unsigned-clients` anyway (M1). |
| D5 | `EnvironmentVariables` sets `HOME` to Brice's home, `PATH` to `/usr/bin:/bin:/usr/sbin:/sbin`, and `LANG` to `en_US.UTF-8`. | A daemon gets no session environment. The service clears the environment of each runner and passes its own short allow-list (`process.rs`), so nothing else reaches a task. |
| D6 | `RunAtLoad` true, `KeepAlive` `{ SuccessfulExit: false }`, `ThrottleInterval` 10, `ProcessType` `Standard`, `Umask` 63 (`077`). | Restarted when it crashes, not when it stops cleanly; files it creates are Brice's only. |
| D7 | `ExitTimeOut` 30. On `SIGTERM` the service stops cleanly and removes its socket; the runners it launched keep their jobs and are adopted by the next service (`supervisor`, adoption by pid and start time). | A restart or an update of the service never interrupts a task. To be checked on the real machine: `launchd` kills what is left in the job's process group after the timeout, and runners run in process groups of their own. |
| D8 | `StandardOutPath` and `StandardErrorPath` in `/Users/<brice>/Library/Logs/Nestor Runner Manager/`, a directory Brice owns. | The service masks secrets in what it logs (`mask`); the log stays readable by Brice and nobody else. |
| D9 | Before login, the service runs headless: admission, runners and the connection to Nestor work without the app. After login, the app connects to the same socket; `getpeereid` and the audit token are those of Brice's account, as with a service started in the session. | The app is a client, never a condition for running tasks. |
| D10 | Uninstalling removes the job and the binaries; it never deletes the data directory. Disassociating the machine stays an action of the app or of the Nestor web app. | Removing the service must not revoke the machine nor lose its state by surprise. |

The template is [`packaging/fr.kocoa.nestor.runner-service.plist`](../packaging/fr.kocoa.nestor.runner-service.plist): replace `BRICE` with the short name of the account (`id -un`) before installing it.

## What starting before login changes

These follow from D2 (one account) together with the decision on signing, and must be settled before a real installation.

### Open 1: where the machine key and the credential live

The live service keeps the machine credential in the data protection Keychain and the machine key in the Secure Enclave (`secrets`). Both need:

- a build signed with a `keychain-access-groups` entitlement, which an ad hoc signature cannot carry (A6): with the decision on signing, association is impossible as the code stands, before or after login;
- the user's session: before Brice logs in after a restart, the account's Keychain is not available to its processes, so even a signed build could not read the credential until Brice logs in once.

So a service that starts before login, without a purchase, needs the secrets out of the Keychain: the software key of A6 (`SoftwareKeyStore`, coded, off) and a credential kept in a file of the data directory, `0600`, in a `0700` directory owned by Brice. That is weaker than today's design: with one account, a task that escapes its confinement runs as Brice and can read that file, so it can act as the machine (heartbeats, queue, runner tokens) until the machine is revoked. What still holds is on the server: the leases, the pause, the one-scope rule and the pools are enforced by Nestor at the claim (S6), so a stolen credential gets no job a runner of that machine could not get, and revoking the machine on the web app ends it.

Enabling the software fallback is Brice's decision (A6). **Recommendation**: accept it for the trials only, on the Mac Studio, with a security review of the file store before it is coded; move back to the Secure Enclave and the Keychain if a certificate is bought later. Not built in this lot.

### Open 2: how the service recognises the app

The service gives the app `full` access only when its signature meets the Developer ID requirement of the team (F1). An ad hoc signed app has no team: it gets `basic` access, which shows the status but cannot pause, change the ceiling or associate. The development fallback (`dev-unsigned`, `--allow-unsigned-clients`) is refused in a release build and must stay so (M1).

**Recommendation** for the trials: let the installation pin the exact build of the app, by its code directory hash, in a file of the root-owned installation directory (D3) that the service reads at start, and accept as `full` only a client whose audit token matches that hash, with the app itself installed root-owned in `/Applications`. A rebuilt or modified app then gets `basic` until Brice installs it again with `sudo`. Not built in this lot; needs a security review.

Until Open 1 and Open 2 are settled, a LaunchDaemon installed as above starts, keeps its state and answers `basic` clients, but cannot associate the machine nor run tasks.

## Procedure (Brice, with `sudo`)

Not authorized by this document: run it only once the installation on the Mac Studio is authorized.

```sh
# 0. Build and sign ad hoc, from a clean checkout of a merged commit.
scripts/release-service.sh                  # release build of core/, refuses dev-unsigned, prints its path
scripts/sign-macos.sh --identity - --kind service --identifier fr.kocoa.nestor.runner-service <path>
                                            # ad hoc: configuration check only (A6)

# 1. Install the binaries, owned by root, not writable by the account.
sudo install -d -o root -g wheel -m 0755 "/Library/Application Support/Nestor Runner Manager/bin"
sudo install -o root -g wheel -m 0755 <path of the release build> \
  "/Library/Application Support/Nestor Runner Manager/bin/"
sudo install -o root -g wheel -m 0755 <nestor-runner of the pinned nestor commit> \
  "/Library/Application Support/Nestor Runner Manager/bin/"

# 2. The plist, with the account's short name.
sed "s/BRICE/$(id -un)/g" packaging/fr.kocoa.nestor.runner-service.plist > /tmp/runner-service.plist
plutil -lint /tmp/runner-service.plist
sudo install -o root -g wheel -m 0644 /tmp/runner-service.plist \
  /Library/LaunchDaemons/fr.kocoa.nestor.runner-service.plist
mkdir -p "$HOME/Library/Logs/Nestor Runner Manager"

# 3. Load and start it.
sudo launchctl bootstrap system /Library/LaunchDaemons/fr.kocoa.nestor.runner-service.plist
sudo launchctl print system/fr.kocoa.nestor.runner-service | grep -E 'state|pid|last exit'
```

Checks after a restart, before logging in (from another machine over SSH): `sudo launchctl print system/fr.kocoa.nestor.runner-service` shows `state = running`, the log in `~/Library/Logs/Nestor Runner Manager/` shows the service listening, and the Nestor web app shows the machine online once it is associated.

Back out:

```sh
sudo launchctl bootout system/fr.kocoa.nestor.runner-service
sudo rm /Library/LaunchDaemons/fr.kocoa.nestor.runner-service.plist
sudo rm -r "/Library/Application Support/Nestor Runner Manager/bin"
# The data directory and the association are kept; disassociate from the app or revoke the
# machine on the Nestor web app to end them.
```

## What does not change

Admission (mode, ceiling, one scope at a time), one job per runner, no resource quota (KCA-714), the launch contract with `run-once`, the sweep after every exit, the pools and option A on Nestor (a client workspace lists no machine). Load-driven admission still comes after real measurements on the Mac Studio (KCA-717 §9).
