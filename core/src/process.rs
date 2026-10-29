//! The launcher of real "one job" runner processes.
//!
//! How a runner is launched (the interface S7 implements on the runner side):
//!
//! ```text
//! <program> <base args…> --runner-id <id> --generation <n> --credential-fd 3 --report-fd 4 [--scope <scope>]
//! ```
//!
//! - **No shell**: the program and its arguments are an `argv` array; nothing is interpreted.
//! - **Credential (B5, A3)**: issued by Nestor for this launch only, written to a pipe inherited
//!   as fd 3, which the runner reads once and closes. Never in an argument, never in the
//!   environment, never in a file. Registered with the [`Redactor`] before anything is logged.
//! - **Environment**: cleared, then a short allow-list (`PATH`, `LANG`, `HOME` and `TMPDIR` in the
//!   runner's own directory). Nothing is inherited from the service.
//! - **Reports** on fd 4, one JSON object per line: `{"event":"ready"}`, then
//!   `{"event":"claimed","taskRef":"…","scope":"…"}`.
//! - **Drain** = the service closes the runner's standard input: the runner claims nothing more
//!   and exits after its job, if any. A service that dies closes it too, so orphaned runners
//!   never claim again.
//! - **Exit status**: 0 after a job that succeeded or with no job, 10 after a job that failed;
//!   anything else is a crash.
//! - **Termination** (revocation, forced stop only): `SIGTERM`, then `SIGKILL`, to the runner's own
//!   process group, and only after checking pid, start time and generation (B4).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::Deserialize;

use crate::mask::Redactor;
use crate::model::{is_runner_id, RunnerExit, TaskOutcome};
use crate::persist::RunnerRecord;
use crate::secrets::Secret;
use crate::supervisor::{Launcher, Supervisor};
use crate::sys::{self, ProcessIdentity};

/// Exit status of a runner whose job failed.
pub const EXIT_JOB_FAILED: i32 = 10;

/// Issues the credential of one launch: short-lived, bound to the machine and to this runner,
/// good for one claim (S2, A3). In the live service, Nestor issues it to the machine.
pub trait CredentialIssuer: Send + Sync {
    fn issue(&self, runner: &RunnerRecord) -> Result<Secret, String>;
}

#[derive(Debug, Clone)]
pub struct ProcessConfig {
    /// Absolute path of the runner program.
    pub program: PathBuf,
    pub base_args: Vec<String>,
    /// Where each runner gets its own directory (`<root>/<runner id>`), `0700`.
    pub runners_root: PathBuf,
}

struct Running {
    generation: u64,
    /// Dropping it closes the runner's standard input: that is the drain signal.
    control: Option<ChildStdin>,
}

pub struct ProcessLauncher {
    config: ProcessConfig,
    issuer: OnceLock<Arc<dyn CredentialIssuer>>,
    redactor: Redactor,
    supervisor: OnceLock<Weak<Supervisor>>,
    running: Arc<Mutex<BTreeMap<String, Running>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", tag = "event")]
enum Report {
    Ready,
    #[serde(rename_all = "camelCase")]
    Claimed {
        task_ref: String,
        #[serde(default)]
        scope: Option<String>,
    },
}

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

    fn arguments(&self, runner: &RunnerRecord) -> Vec<String> {
        let mut arguments = self.config.base_args.clone();
        arguments.extend([
            "--runner-id".into(),
            runner.id.clone(),
            "--generation".into(),
            runner.generation.to_string(),
            "--credential-fd".into(),
            "3".into(),
            "--report-fd".into(),
            "4".into(),
        ]);
        if let Some(scope) = &runner.scope {
            arguments.extend(["--scope".into(), scope.clone()]);
        }
        arguments
    }

    /// Whether the arguments of `pid` carry this runner's id and generation: the generation
    /// marker checked before any adoption or signal.
    fn carries_marker(pid: u32, runner: &RunnerRecord) -> bool {
        let Some(arguments) = sys::process_arguments(pid) else { return false };
        let value_of = |flag: &str| arguments.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
        value_of("--runner-id").as_deref() == Some(runner.id.as_str())
            && value_of("--generation") == Some(runner.generation.to_string())
    }
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
    fn spawn(&self, runner: &RunnerRecord) -> Result<ProcessIdentity, String> {
        let directory = self.runner_dir(&runner.id).ok_or("invalid runner id")?;
        let home = directory.join("home");
        let tmp = directory.join("tmp");
        for path in [&self.config.runners_root, &directory, &home, &tmp] {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(path).map_err(|e| e.to_string())?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        }

        let credential = self.issuer.get().ok_or("no credential issuer")?.issue(runner)?;
        self.redactor.register(credential.expose());

        let (credential_read, credential_write) = pipe().map_err(|e| e.to_string())?;
        let (report_read, report_write) = pipe().map_err(|e| e.to_string())?;
        let (credential_fd, report_fd) = (credential_read.as_raw_fd(), report_write.as_raw_fd());

        let mut command = Command::new(&self.config.program);
        command
            .args(self.arguments(runner))
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
                if credential < 0 || report < 0 || libc::dup2(credential, 3) < 0 || libc::dup2(report, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(credential);
                libc::close(report);
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| format!("cannot launch the runner: {e}"))?;
        drop(credential_read);
        drop(report_write);

        let pid = child.id();
        // The kernel's record of a process that has just been executed can lag a little.
        let identity = (0..100).find_map(|attempt| {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            ProcessIdentity::of(pid)
        });
        let Some(identity) = identity else {
            // Never leave a process the service cannot recognize later.
            let early = child.try_wait().ok().flatten();
            let _ = child.kill();
            let _ = child.wait();
            return Err(match early {
                Some(status) => format!("the runner ended at once ({status})"),
                None => "cannot read the runner's process identity".into(),
            });
        };

        // The credential goes through the pipe once, then the pipe is closed.
        {
            let mut writer = fs::File::from(credential_write);
            let _ = writer.write_all(credential.expose().as_bytes());
        }
        drop(credential);

        let supervisor = self.supervisor.get().cloned().unwrap_or_default();
        let (id, generation) = (runner.id.clone(), runner.generation);
        let claimed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The readers signal when their pipe ends, so the exit is reported after the last line
        // and the last report (a claim must be known before the exit is classified).
        let mut readers_done = Vec::new();

        for stream in [
            child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let (supervisor, id) = (supervisor.clone(), id.clone());
            let (done, finished) = std::sync::mpsc::channel::<()>();
            readers_done.push(finished);
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if let Some(supervisor) = supervisor.upgrade() {
                        supervisor.runner_output(&id, generation, &line);
                    }
                }
                let _ = done.send(());
            });
        }
        {
            let (supervisor, id, claimed) = (supervisor.clone(), id.clone(), Arc::clone(&claimed));
            let reports = fs::File::from(report_read);
            let (done, finished) = std::sync::mpsc::channel::<()>();
            readers_done.push(finished);
            std::thread::spawn(move || {
                // At most a few reports matter; the rest is read and ignored.
                for (index, line) in BufReader::new(reports).lines().map_while(Result::ok).enumerate() {
                    let Some(supervisor) = supervisor.upgrade().filter(|_| index < 16) else { continue };
                    match serde_json::from_str::<Report>(&line) {
                        Ok(Report::Ready) => {
                            let _ = supervisor.runner_ready(&id, generation);
                        }
                        Ok(Report::Claimed { task_ref, scope }) => {
                            claimed.store(true, std::sync::atomic::Ordering::SeqCst);
                            let _ = supervisor.runner_claimed(&id, generation, &task_ref, scope.as_deref());
                        }
                        Err(_) => supervisor.runner_output(&id, generation, "Unreadable report from the runner."),
                    }
                }
                let _ = done.send(());
            });
        }
        let control = child.stdin.take();
        self.running.lock().unwrap().insert(id.clone(), Running { generation, control });
        {
            let running = Arc::clone(&self.running);
            std::thread::spawn(move || {
                let status = child.wait();
                // A process the runner started may keep a pipe open: wait a little, not forever.
                for finished in readers_done {
                    let _ = finished.recv_timeout(std::time::Duration::from_secs(2));
                }
                {
                    let mut running = running.lock().unwrap();
                    if running.get(&id).is_some_and(|r| r.generation == generation) {
                        running.remove(&id);
                    }
                }
                let claimed = claimed.load(std::sync::atomic::Ordering::SeqCst);
                let exit = match status {
                    Ok(status) if status.code() == Some(0) && claimed => RunnerExit::Finished(TaskOutcome::Succeeded),
                    Ok(status) if status.code() == Some(0) => RunnerExit::Idle,
                    Ok(status) if status.code() == Some(EXIT_JOB_FAILED) && claimed => {
                        RunnerExit::Finished(TaskOutcome::Failed)
                    }
                    Ok(status) => match status.signal() {
                        Some(signal) => RunnerExit::Crashed(format!("signal {signal}")),
                        None => RunnerExit::Crashed(format!("status {}", status.code().unwrap_or(-1))),
                    },
                    Err(error) => RunnerExit::Crashed(error.to_string()),
                };
                if let Some(supervisor) = supervisor.upgrade() {
                    supervisor.runner_exited(&id, generation, exit);
                }
            });
        }
        Ok(identity)
    }

    fn drain(&self, runner: &RunnerRecord) {
        if let Some(running) = self.running.lock().unwrap().get_mut(&runner.id) {
            if running.generation == runner.generation {
                running.control = None;
            }
        }
    }

    fn terminate(&self, runner: &RunnerRecord, force: bool) {
        if !self.is_alive(runner) {
            return;
        }
        let Some(pid) = runner.pid else { return };
        self.drain(runner);
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        unsafe { libc::killpg(pid as libc::pid_t, signal) };
    }

    fn is_alive(&self, runner: &RunnerRecord) -> bool {
        let (Some(pid), Some(start_time_us)) = (runner.pid, runner.start_time_us) else { return false };
        ProcessIdentity { pid, start_time_us }.is_current() && Self::carries_marker(pid, runner)
    }

    fn forget(&self, runner_id: &str) {
        if let Some(directory) = self.runner_dir(runner_id) {
            let _ = fs::remove_dir_all(directory);
        }
    }
}
