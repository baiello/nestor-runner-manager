//! The launcher of real "one job" runner processes: `nestor-runner run-once`, launch contract v1.
//!
//! The contract is written in nestor (`docs/run-once-contract.md`); its fixtures are vendored in
//! `contract/run-once-v1.json` (KCA-734 §2). The service's side:
//!
//! ```text
//! <program> run-once --runner-id <hex> --generation <n> --machine-id <uuid> --server https://<instance>
//!                    --credential-fd 3 --report-fd 4
//! ```
//!
//! - **No shell**: the program and its arguments are an `argv` array; nothing is interpreted.
//!   `--runner-id` and `--generation` stay in the `argv`: they are how the service recognizes its
//!   process before adopting or signalling it. **`--scope` is never passed** (KCA-734 §3): the
//!   service also applies a ceiling of 1 until Nestor filters scopes at claim time (S6).
//! - **Credential (B5, A3)**: issued by Nestor for this launch only, written as one JSON object
//!   `{"runner_token":"…"}` to a pipe inherited as fd 3, which the service closes after writing.
//!   Written only once the process's pid and start time are saved (KCA-741): until then the
//!   runner waits on fd 3 and can claim nothing, so a process the service may lose track of in a
//!   crash never holds a job. See [`crate::supervisor::PendingLaunch`].
//!   Never in an argument, never in the environment, never in a file. Registered with the
//!   [`Redactor`] before anything is logged, forgotten 10 minutes after the runner stops (F4). The
//!   service holds no Claude Code token in this slice (A7): the field is left out.
//! - **Environment**: cleared, then a short allow-list (`PATH`, `LANG`, `HOME` and `TMPDIR` in the
//!   runner's own directory). Nothing is inherited from the service. Every other descriptor of
//!   the service is close-on-exec, so the runner gets 0 to 4 only.
//! - **Drain** = `drain\n` on the runner's standard input, which is then closed (end of input is a
//!   drain too). A service that dies closes it as well, so orphaned runners never claim again.
//! - **Reports** on fd 4, one JSON object per line, read as an **untrusted** channel (a process
//!   the runner started could forge lines): strict schema, 4 KiB a line, 20 a second, `hello`
//!   first and once, one `claimed` per process; anything else is ignored and counted. A forged
//!   report can at worst hold a slot until the process exits; the exit always frees it.
//! - **Exit status** decides how the runner ended: 0 normal, 64 usage (never relaunched), 65
//!   unreadable credential, 70 internal error (both crashes), 75 Nestor unreachable (backoff, not a
//!   crash), 77 credential refused (revoked: no relaunch), a signal is a crash unless expected.
//! - **Termination** (revocation, forced stop only): `SIGTERM` to the runner's own process group,
//!   then `SIGKILL` and a sweep of what it left behind ([`crate::sweep`], M3), and only after
//!   checking pid, start time and generation (B4).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::mask::{Redactor, Registration};
use crate::model::{is_runner_id, RunnerExit, TaskOutcome};
use crate::persist::RunnerRecord;
use crate::secrets::Secret;
use crate::supervisor::{LaunchTarget, Launcher, PendingLaunch, Supervisor};
use crate::sweep;
use crate::sys::{self, ProcessIdentity};

/// Version of the launch contract this service speaks.
pub const CONTRACT: u64 = 1;
pub const CREDENTIAL_FD: i32 = 3;
pub const REPORT_FD: i32 = 4;
pub const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;
pub const MAX_REPORT_BYTES: usize = 4 * 1024;
pub const MAX_REPORTS_PER_SECOND: u32 = 20;

pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 64;
pub const EXIT_CREDENTIAL_INVALID: i32 = 65;
pub const EXIT_INTERNAL: i32 = 70;
pub const EXIT_UNREACHABLE: i32 = 75;
pub const EXIT_REVOKED: i32 = 77;

/// Issues the credential of one launch: short-lived, bound to the machine and to this runner,
/// good for one claim (S2, A3). In the live service, Nestor issues it to the machine.
pub trait CredentialIssuer: Send + Sync {
    fn issue(&self, runner: &RunnerRecord) -> Result<Secret, String>;
}

#[derive(Debug, Clone)]
pub struct ProcessConfig {
    /// Absolute path of the runner program.
    pub program: PathBuf,
    /// Before the contract's options: `["run-once"]` for the Nestor runner.
    pub base_args: Vec<String>,
    /// Where each runner gets its own directory (`<root>/<runner id>`), `0700`.
    pub runners_root: PathBuf,
    /// Between two passes of the sweep after a kill (1 s in the service).
    pub sweep_interval: Duration,
}

impl ProcessConfig {
    pub fn new(program: PathBuf, base_args: Vec<String>, runners_root: PathBuf) -> Self {
        ProcessConfig { program, base_args, runners_root, sweep_interval: Duration::from_secs(1) }
    }
}

struct Running {
    generation: u64,
    pid: u32,
    /// The drain line is written here; dropping it closes the runner's standard input.
    control: Option<ChildStdin>,
}

pub struct ProcessLauncher {
    config: ProcessConfig,
    issuer: OnceLock<Arc<dyn CredentialIssuer>>,
    redactor: Redactor,
    supervisor: OnceLock<Weak<Supervisor>>,
    running: Arc<Mutex<BTreeMap<String, Running>>>,
}

// MARK: Reports

/// How the runner says its job ended (`finished`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    Done,
    Failed,
    Abandoned,
    LeaseLost,
    Released,
}

impl Finished {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "done" => Finished::Done,
            "failed" => Finished::Failed,
            "abandoned" => Finished::Abandoned,
            "lease_lost" => Finished::LeaseLost,
            "released" => Finished::Released,
            _ => return None,
        })
    }

    pub fn outcome(self) -> TaskOutcome {
        match self {
            Finished::Done => TaskOutcome::Succeeded,
            Finished::Failed => TaskOutcome::Failed,
            Finished::Abandoned => TaskOutcome::Stopped,
            Finished::LeaseLost => TaskOutcome::LeaseLost,
            Finished::Released => TaskOutcome::Released,
        }
    }
}

/// One line of the report channel, once checked against the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Hello {
        contract: u64,
        runner_id: String,
        generation: u64,
        pid: u32,
    },
    /// The job id is opaque; the report carries no job content.
    Claimed {
        job_id: String,
    },
    Finished(Finished),
    /// `finished` `failed`, with why (`failure`, such as `claude_not_signed_in`). Read ahead of
    /// the contract: `run-once` at `contract/nestor.rev` does not send it yet (A7).
    FailedWith {
        failure: String,
    },
    Idle,
}

/// Checks one report line against contract v1: a JSON object with `"v":1`, a known `type` and
/// exactly that type's fields, with the right types. Anything else is refused.
pub fn parse_report(line: &str) -> Result<Report, String> {
    if line.len() > MAX_REPORT_BYTES {
        return Err("line too long".into());
    }
    let value: Value = serde_json::from_str(line).map_err(|_| "not JSON".to_string())?;
    let object = value.as_object().ok_or("not an object")?;
    if object.get("v").and_then(Value::as_u64) != Some(1) {
        return Err("unknown version".into());
    }
    let kind = object.get("type").and_then(Value::as_str).ok_or("no type")?;
    let fields: &[&str] = match kind {
        "hello" => &["contract", "runner_id", "generation", "pid"],
        "claimed" => &["job_id"],
        "finished" => &["outcome"],
        "idle" => &[],
        _ => return Err("unknown type".into()),
    };
    // `failure`, on a failed job only, is read ahead of the contract (A7).
    let failure = match object.get("failure") {
        Some(failure) if kind == "finished" && object.get("outcome").and_then(Value::as_str) == Some("failed") => {
            let failure = failure.as_str().ok_or("failure is not a string")?;
            if failure.is_empty() || failure.len() > 64 || !failure.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            {
                return Err("invalid failure".into());
            }
            Some(failure.to_string())
        }
        _ => None,
    };
    let extra = usize::from(failure.is_some());
    if object.len() != fields.len() + 2 + extra || !fields.iter().all(|f| object.contains_key(*f)) {
        return Err(format!("fields of {kind} do not match"));
    }
    if let Some(failure) = failure {
        return Ok(Report::FailedWith { failure });
    }
    let text = |name: &str| object[name].as_str().ok_or_else(|| format!("{name} is not a string"));
    let number = |name: &str| object[name].as_u64().ok_or_else(|| format!("{name} is not a number"));
    Ok(match kind {
        "hello" => {
            let runner_id = text("runner_id")?;
            if !(8..=64).contains(&runner_id.len())
                || !runner_id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err("invalid runner_id".into());
            }
            let pid = u32::try_from(number("pid")?).map_err(|_| "invalid pid")?;
            Report::Hello {
                contract: number("contract")?,
                runner_id: runner_id.into(),
                generation: number("generation")?,
                pid,
            }
        }
        "claimed" => {
            let job_id = text("job_id")?;
            if job_id.is_empty() || job_id.len() > 128 || !job_id.bytes().all(|b| b.is_ascii_graphic()) {
                return Err("invalid job_id".into());
            }
            Report::Claimed { job_id: job_id.into() }
        }
        "finished" => Report::Finished(Finished::parse(text("outcome")?).ok_or("unknown outcome")?),
        _ => Report::Idle,
    })
}

/// What an exit status means under the contract (the names of `contract/run-once-v1.json`).
pub fn exit_meaning(code: i32) -> Option<&'static str> {
    Some(match code {
        EXIT_OK => "ok",
        EXIT_USAGE => "usage",
        EXIT_CREDENTIAL_INVALID => "credential_invalid",
        EXIT_INTERNAL => "internal",
        EXIT_UNREACHABLE => "unreachable",
        EXIT_REVOKED => "revoked",
        _ => return None,
    })
}

/// How the runner ended, from its exit status and what it reported.
pub fn classify_exit(status: &ExitStatus, claimed: bool, finished: Option<Finished>) -> RunnerExit {
    match status.code() {
        Some(EXIT_OK) if claimed => RunnerExit::Finished(finished.map_or(TaskOutcome::Unknown, Finished::outcome)),
        Some(EXIT_OK) => RunnerExit::Idle,
        Some(EXIT_USAGE) => RunnerExit::Usage,
        Some(EXIT_UNREACHABLE) => RunnerExit::Unreachable,
        Some(EXIT_REVOKED) => RunnerExit::Revoked,
        Some(EXIT_CREDENTIAL_INVALID) => RunnerExit::Crashed("unreadable credential (exit 65)".into()),
        Some(EXIT_INTERNAL) => RunnerExit::Crashed("internal error (exit 70)".into()),
        Some(code) => RunnerExit::Crashed(format!("status {code}")),
        None => RunnerExit::Crashed(format!("signal {}", status.signal().unwrap_or(0))),
    }
}

/// What the report reader learned, shared with the thread that waits for the exit.
#[derive(Default)]
struct Reported {
    claimed: AtomicBool,
    finished: Mutex<Option<Finished>>,
}

/// Reads one line, keeping at most `limit` bytes of it: the rest of a longer line is read and
/// dropped, so memory stays bounded whatever the process writes (KCA-732 nº4). `Ok(None)` at
/// the end of the stream; otherwise the bytes kept and whether the line was cut.
fn read_capped_line(reader: &mut impl BufRead, limit: usize) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::new();
    let mut cut = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((!line.is_empty() || cut).then_some((line, cut)));
        }
        let (chunk, done) = match buffer.iter().position(|b| *b == b'\n') {
            Some(end) => (&buffer[..end], Some(end + 1)),
            None => (buffer, None),
        };
        let room = limit.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..chunk.len().min(room)]);
        cut |= chunk.len() > room;
        let consumed = done.unwrap_or(buffer.len());
        reader.consume(consumed);
        if done.is_some() {
            return Ok(Some((line, cut)));
        }
    }
}

/// One report line of at most `limit` bytes, `\n`-terminated UTF-8; anything else is `Err`
/// (ignored by the caller).
fn read_bounded_line(reader: &mut impl BufRead, limit: usize) -> std::io::Result<Option<Result<String, ()>>> {
    Ok(read_capped_line(reader, limit)?.map(|(line, cut)| match cut {
        true => Err(()),
        false => String::from_utf8(line).map_err(|_| ()),
    }))
}

/// Longest line of a runner's output kept in its log; the rest of the line is dropped.
const MAX_OUTPUT_LINE_BYTES: usize = 8 * 1024;

// MARK: Launcher

impl ProcessLauncher {
    pub fn new(config: ProcessConfig, redactor: Redactor) -> Result<Self, String> {
        if !config.program.is_absolute() {
            return Err("the runner program must be an absolute path".into());
        }
        Ok(ProcessLauncher {
            config,
            issuer: OnceLock::new(),
            redactor,
            supervisor: OnceLock::new(),
            running: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Where the launcher sends what runners report, and who issues their credentials. Set
    /// once, right after the supervisor is created (they need the launcher themselves).
    pub fn attach(&self, supervisor: &Arc<Supervisor>, issuer: Arc<dyn CredentialIssuer>) {
        let _ = self.supervisor.set(Arc::downgrade(supervisor));
        let _ = self.issuer.set(issuer);
    }

    fn runner_dir(&self, runner_id: &str) -> Option<PathBuf> {
        is_runner_id(runner_id).then(|| self.config.runners_root.join(runner_id))
    }

    /// The `argv` of a launch (contract v1 §2). Never `--scope`, never a secret.
    pub fn arguments(&self, runner: &RunnerRecord, target: &LaunchTarget) -> Result<Vec<String>, String> {
        if !is_machine_id(&target.machine_id) {
            return Err("the machine id is not a lowercase UUID".into());
        }
        if !is_instance(&target.instance) {
            return Err("the Nestor instance is not a plain host name".into());
        }
        let mut arguments = self.config.base_args.clone();
        arguments.extend([
            "--runner-id".into(),
            runner.id.clone(),
            "--generation".into(),
            runner.generation.to_string(),
            "--machine-id".into(),
            target.machine_id.clone(),
            "--server".into(),
            format!("https://{}", target.instance),
            "--credential-fd".into(),
            CREDENTIAL_FD.to_string(),
            "--report-fd".into(),
            REPORT_FD.to_string(),
        ]);
        Ok(arguments)
    }

    /// Whether the arguments of `pid` carry this runner's id and generation: the generation
    /// marker checked before any adoption or signal.
    fn carries_marker(pid: u32, runner: &RunnerRecord) -> bool {
        let Some(arguments) = sys::process_arguments(pid) else { return false };
        let value_of = |flag: &str| arguments.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
        value_of("--runner-id").as_deref() == Some(runner.id.as_str())
            && value_of("--generation") == Some(runner.generation.to_string())
    }

    /// A child this launcher spawned and has not reaped yet: its pid cannot be anyone else's, so
    /// it is the runner whatever its argv became. Adopted runners are not children, and need the
    /// marker.
    fn is_unreaped_child(&self, runner: &RunnerRecord, pid: u32) -> bool {
        self.running
            .lock()
            .unwrap()
            .get(&runner.id)
            .is_some_and(|running| running.generation == runner.generation && running.pid == pid)
    }

    /// Sweeps what the runner left behind, then reports what survived (M3).
    fn sweep_and_report(
        supervisor: &Weak<Supervisor>,
        snapshot: &[sys::ProcessInfo],
        directory: &std::path::Path,
        interval: Duration,
        runner_id: &str,
        generation: u64,
    ) -> usize {
        let remaining = sweep::sweep(snapshot, directory, interval);
        if remaining > 0 {
            if let Some(supervisor) = supervisor.upgrade() {
                supervisor.sweep_failed(runner_id, generation, remaining);
            }
        }
        remaining
    }
}

/// A lowercase UUID (`8-4-4-4-12` hexadecimal digits).
pub fn is_machine_id(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// A host name, with an optional port: no scheme, user, path, query or fragment.
fn is_instance(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 253
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
        && !text.starts_with(['.', '-', ':'])
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((read, write))
}

impl Launcher for ProcessLauncher {
    fn spawn(&self, runner: &RunnerRecord, target: &LaunchTarget) -> Result<Box<dyn PendingLaunch>, String> {
        let arguments = self.arguments(runner, target)?;
        let directory = self.runner_dir(&runner.id).ok_or("invalid runner id")?;
        let home = directory.join("home");
        let tmp = directory.join("tmp");
        for path in [&self.config.runners_root, &directory, &home, &tmp] {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(path).map_err(|e| e.to_string())?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        }

        let credential = self.issuer.get().ok_or("no credential issuer")?.issue(runner)?;
        let registration = self.redactor.register_transient(credential.expose());
        // A7: the service holds no Claude Code token in this slice; the field is left out.
        let payload = Secret::new(serde_json::json!({ "runner_token": credential.expose() }).to_string());
        drop(credential);
        if payload.expose().len() > MAX_CREDENTIAL_BYTES {
            self.redactor.release(registration);
            return Err("the runner credential is too large".into());
        }

        let (credential_read, credential_write) = pipe().map_err(|e| e.to_string())?;
        let (report_read, report_write) = pipe().map_err(|e| e.to_string())?;
        let (credential_fd, report_fd) = (credential_read.as_raw_fd(), report_write.as_raw_fd());

        let mut command = Command::new(&self.config.program);
        command
            .args(&arguments)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("LANG", "en_US.UTF-8")
            .env("HOME", &home)
            .env("TMPDIR", &tmp)
            .current_dir(&directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a termination reaches what it started too.
            .process_group(0);
        unsafe {
            command.pre_exec(move || {
                // Move both ends above 4 first, in case they already sit on 3 or 4.
                let credential = libc::fcntl(credential_fd, libc::F_DUPFD, 10);
                let report = libc::fcntl(report_fd, libc::F_DUPFD, 10);
                if credential < 0
                    || report < 0
                    || libc::dup2(credential, CREDENTIAL_FD) < 0
                    || libc::dup2(report, REPORT_FD) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(credential);
                libc::close(report);
                Ok(())
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.redactor.release(registration);
                return Err(format!("cannot launch the runner: {error}"));
            }
        };
        drop(credential_read);
        drop(report_write);

        let pid = child.id();
        // The kernel's record of a process that has just been executed can lag a little.
        let identity = (0..100).find_map(|attempt| {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
            ProcessIdentity::of(pid)
        });
        let Some(identity) = identity else {
            // Never leave a process the service cannot recognize later.
            let early = child.try_wait().ok().flatten();
            let _ = child.kill();
            let _ = child.wait();
            self.redactor.release(registration);
            return Err(match early {
                Some(status) => format!("the runner ended at once ({status})"),
                None => "cannot read the runner's process identity".into(),
            });
        };

        Ok(Box::new(Pending {
            identity,
            child: Some(child),
            credential: Some((fs::File::from(credential_write), payload)),
            reports: Some(fs::File::from(report_read)),
            registration: Some(registration),
            directory,
            id: runner.id.clone(),
            generation: runner.generation,
            supervisor: self.supervisor.get().cloned().unwrap_or_default(),
            running: Arc::clone(&self.running),
            redactor: self.redactor.clone(),
            interval: self.config.sweep_interval,
        }))
    }

    fn clock_now(&self) -> u64 {
        sys::process_clock_now()
    }

    fn end_unrecorded(&self, runner: &RunnerRecord, since: u64) -> (usize, usize) {
        let processes: Vec<ProcessIdentity> = sys::all_pids()
            .into_iter()
            .filter(|pid| *pid != std::process::id())
            .filter_map(sys::process_info)
            .filter(|p| !p.zombie && p.start_time_us >= since && Self::carries_marker(p.pid, runner))
            .map(|p| p.identity())
            .collect();
        for process in &processes {
            // Its own group leader since its launch: the group goes too. Identity checked just
            // before, so a reused pid is never signalled.
            if process.is_current() {
                unsafe {
                    libc::killpg(process.pid as libc::pid_t, libc::SIGKILL);
                    libc::kill(process.pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let remaining = loop {
            let left = processes
                .iter()
                .filter(|p| {
                    sys::process_info(p.pid).is_some_and(|now| now.start_time_us == p.start_time_us && !now.zombie)
                })
                .count();
            if left == 0 || Instant::now() >= deadline {
                break left;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        (processes.len() - remaining, remaining)
    }

    fn drain(&self, runner: &RunnerRecord) {
        if let Some(running) = self.running.lock().unwrap().get_mut(&runner.id) {
            if running.generation == runner.generation {
                // `drain\n`, then the end of input (also a drain). A runner that already exited
                // makes the write fail, which changes nothing.
                if let Some(mut control) = running.control.take() {
                    let _ = control.write_all(b"drain\n");
                }
            }
        }
    }

    fn terminate(&self, runner: &RunnerRecord, force: bool) {
        if !self.is_alive(runner) {
            return;
        }
        let Some(pid) = runner.pid else { return };
        self.drain(runner);
        if !force {
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
            return;
        }
        // End of the grace period: what it started is noted before the kill, then swept (M3).
        let Some(directory) = self.runner_dir(&runner.id) else { return };
        let snapshot = sweep::snapshot(pid, &directory);
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
        let supervisor = self.supervisor.get().cloned().unwrap_or_default();
        let (id, generation, interval) = (runner.id.clone(), runner.generation, self.config.sweep_interval);
        std::thread::spawn(move || {
            Self::sweep_and_report(&supervisor, &snapshot, &directory, interval, &id, generation);
        });
    }

    fn is_alive(&self, runner: &RunnerRecord) -> bool {
        let (Some(pid), Some(start_time_us)) = (runner.pid, runner.start_time_us) else { return false };
        ProcessIdentity { pid, start_time_us }.is_current()
            && (self.is_unreaped_child(runner, pid) || Self::carries_marker(pid, runner))
    }

    fn forget(&self, runner_id: &str) {
        if let Some(directory) = self.runner_dir(runner_id) {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

/// A runner spawned and blocked on fd 3: it has not got its credential, so it can claim
/// nothing (KCA-741). Nothing follows it yet: its output and reports wait in their pipes.
struct Pending {
    identity: ProcessIdentity,
    child: Option<Child>,
    credential: Option<(fs::File, Secret)>,
    reports: Option<fs::File>,
    registration: Option<Registration>,
    directory: PathBuf,
    id: String,
    generation: u64,
    supervisor: Weak<Supervisor>,
    running: Arc<Mutex<BTreeMap<String, Running>>>,
    redactor: Redactor,
    interval: Duration,
}

impl PendingLaunch for Pending {
    fn identity(&self) -> ProcessIdentity {
        self.identity
    }

    fn release(mut self: Box<Self>) {
        let mut child = self.child.take().expect("a pending launch has its process");
        let registration = self.registration.take().expect("a pending launch has its registration");
        let reports = self.reports.take();
        // The credential goes through the pipe once, then the pipe is closed.
        if let Some((mut writer, payload)) = self.credential.take() {
            let _ = writer.write_all(payload.expose().as_bytes());
        }
        let (supervisor, directory, interval) = (self.supervisor.clone(), self.directory.clone(), self.interval);
        let (id, generation, pid) = (self.id.clone(), self.generation, self.identity.pid);
        let reported = Arc::new(Reported::default());
        // The readers signal when their pipe ends, so the exit is reported after the last line
        // and the last report (a claim must be known before the exit is classified).
        let mut readers_done = Vec::new();

        for stream in [
            child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
            child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let (supervisor, id) = (supervisor.clone(), id.clone());
            let (done, finished) = std::sync::mpsc::channel::<()>();
            readers_done.push(finished);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                while let Ok(Some((line, cut))) = read_capped_line(&mut reader, MAX_OUTPUT_LINE_BYTES) {
                    let mut line = String::from_utf8_lossy(&line).into_owned();
                    if cut {
                        line.push_str(" [line cut]");
                    }
                    if let Some(supervisor) = supervisor.upgrade() {
                        supervisor.runner_output(&id, generation, &line);
                    }
                }
                let _ = done.send(());
            });
        }
        if let Some(reports) = reports {
            let (supervisor, id, reported) = (supervisor.clone(), id.clone(), Arc::clone(&reported));
            let (done, finished) = std::sync::mpsc::channel::<()>();
            readers_done.push(finished);
            std::thread::spawn(move || {
                read_reports(reports, &supervisor, &id, generation, pid, &reported);
                let _ = done.send(());
            });
        }
        let control = child.stdin.take();
        self.running.lock().unwrap().insert(id.clone(), Running { generation, pid, control });
        let running = Arc::clone(&self.running);
        let redactor = self.redactor.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            // A process the runner started may keep a pipe open: wait a little, not forever.
            for finished in readers_done {
                let _ = finished.recv_timeout(Duration::from_secs(2));
            }
            {
                let mut running = running.lock().unwrap();
                if running.get(&id).is_some_and(|r| r.generation == generation) {
                    running.remove(&id);
                }
            }
            redactor.release(registration);
            let claimed = reported.claimed.load(Ordering::SeqCst);
            let finished = *reported.finished.lock().unwrap();
            let exit = match &status {
                Ok(status) => classify_exit(status, claimed, finished),
                Err(error) => RunnerExit::Crashed(error.to_string()),
            };
            // Killed: what it started may still run (M3). Swept before its directory can be
            // erased, reported after the exit.
            let killed = matches!(&status, Ok(status) if status.signal().is_some());
            let remaining = if killed { sweep::sweep(&[], &directory, interval) } else { 0 };
            if let Some(supervisor) = supervisor.upgrade() {
                supervisor.runner_exited(&id, generation, exit);
                if remaining > 0 {
                    supervisor.sweep_failed(&id, generation, remaining);
                }
            }
        });
    }

    fn abort(self: Box<Self>) {
        // Dropping does it.
    }
}

impl Drop for Pending {
    /// Not released: the process is killed with its group and collected, its credential is
    /// never written (the pipe closes unwritten).
    fn drop(&mut self) {
        self.credential = None;
        if let Some(mut child) = self.child.take() {
            if self.identity.is_current() {
                unsafe { libc::killpg(self.identity.pid as libc::pid_t, libc::SIGKILL) };
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(registration) = self.registration.take() {
            self.redactor.release(registration);
        }
    }
}

/// Reads the report channel of one runner as untrusted input (contract v1 §2).
fn read_reports(
    reports: fs::File,
    supervisor: &Weak<Supervisor>,
    runner_id: &str,
    generation: u64,
    pid: u32,
    reported: &Reported,
) {
    let mut reader = BufReader::new(reports);
    let (mut hello, mut ignored) = (false, 0u64);
    let (mut window, mut in_window) = (Instant::now(), 0u32);
    while let Ok(Some(line)) = read_bounded_line(&mut reader, MAX_REPORT_BYTES) {
        if window.elapsed() >= Duration::from_secs(1) {
            (window, in_window) = (Instant::now(), 0);
        }
        in_window += 1;
        let report = match line {
            Ok(line) if in_window <= MAX_REPORTS_PER_SECOND => parse_report(&line).ok(),
            _ => None,
        };
        let Some(supervisor) = supervisor.upgrade() else { break };
        match report {
            Some(Report::Hello {
                contract,
                runner_id: reported_id,
                generation: reported_generation,
                pid: reported_pid,
            }) if !hello => {
                hello = true;
                if contract != CONTRACT
                    || reported_id != runner_id
                    || reported_generation != generation
                    || reported_pid != pid
                {
                    supervisor.runner_misbehaved(
                        runner_id,
                        generation,
                        "the runner's hello does not match its launch (contract, id, generation or pid)",
                    );
                } else {
                    let _ = supervisor.runner_ready(runner_id, generation);
                }
            }
            Some(Report::Claimed { job_id }) if hello && !reported.claimed.load(Ordering::SeqCst) => {
                reported.claimed.store(true, Ordering::SeqCst);
                let _ = supervisor.runner_claimed(runner_id, generation, &job_id, None);
            }
            Some(Report::Finished(finished)) if hello && reported.claimed.load(Ordering::SeqCst) => {
                reported.finished.lock().unwrap().get_or_insert(finished);
            }
            Some(Report::FailedWith { failure }) if hello && reported.claimed.load(Ordering::SeqCst) => {
                let first = {
                    let mut finished = reported.finished.lock().unwrap();
                    let first = finished.is_none();
                    finished.get_or_insert(Finished::Failed);
                    first
                };
                if first {
                    supervisor.runner_failed_job(runner_id, generation, &failure);
                }
            }
            Some(Report::Idle) if hello => {}
            _ => ignored += 1,
        }
    }
    if ignored > 0 {
        if let Some(supervisor) = supervisor.upgrade() {
            supervisor.runner_output(
                runner_id,
                generation,
                &format!("Ignored {ignored} report(s) that do not follow the launch contract."),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_lines_are_cut_without_reading_them_whole() {
        // 50 MiB without a newline: only the first bytes are kept.
        let mut source = std::io::Read::chain(std::io::repeat(b'x').take(50 * 1024 * 1024), &b"\nnext\n"[..]);
        let mut reader = BufReader::with_capacity(4096, &mut source);
        let (line, cut) = read_capped_line(&mut reader, 100).unwrap().unwrap();
        assert_eq!((line.len(), cut), (100, true));
        assert_eq!(read_capped_line(&mut reader, 100).unwrap(), Some((b"next".to_vec(), false)));
        assert_eq!(read_capped_line(&mut reader, 100).unwrap(), None);
    }

    #[test]
    fn long_lines_are_skipped_whole() {
        let input = format!("{}\nshort\n{}", "x".repeat(10_000), "tail-without-newline");
        let mut reader = BufReader::with_capacity(64, input.as_bytes());
        assert_eq!(read_bounded_line(&mut reader, 100).unwrap(), Some(Err(())));
        assert_eq!(read_bounded_line(&mut reader, 100).unwrap(), Some(Ok("short".into())));
        assert_eq!(
            read_bounded_line(&mut reader, 100).unwrap(),
            Some(Ok("tail-without-newline".into())),
            "a last line without newline is still checked like any other"
        );
        assert_eq!(read_bounded_line(&mut reader, 100).unwrap(), None);
    }

    #[test]
    fn machine_ids_and_instances_are_checked() {
        assert!(is_machine_id("6f1c2a4e-9b3d-4c5e-8f70-1a2b3c4d5e6f"));
        assert!(!is_machine_id("6F1C2A4E-9B3D-4C5E-8F70-1A2B3C4D5E6F"));
        assert!(!is_machine_id("m000000000000001"));
        assert!(is_instance("nestor.example"));
        assert!(is_instance("localhost:8443"));
        for bad in ["https://nestor.example", "user@nestor.example", "nestor.example/x", "", "-x"] {
            assert!(!is_instance(bad), "{bad}");
        }
    }
}
