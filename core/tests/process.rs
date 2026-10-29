//! The real launcher, with real processes: a double of `nestor-runner run-once` that checks
//! launch contract v1 against the vendored fixtures (`examples/run_once_double.rs`). `argv`
//! without a shell nor `--scope`, credential JSON read once from fd 3, drain line on stdin,
//! strict reading of the reports, exit codes, process identity, masking, and the sweep of what a
//! killed runner left behind (S8.2, S8.5, S8.6, B5, N5, KCA-734 R1 and R3).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nestor_runner_core::mask::Redactor;
use nestor_runner_core::model::*;
use nestor_runner_core::persist::{FileStore, Loaded, MemoryStore, RunnerRecord, Store, StoredState};
use nestor_runner_core::process::{CredentialIssuer, ProcessConfig, ProcessLauncher};
use nestor_runner_core::secrets::Secret;
use nestor_runner_core::supervisor::{Clock, LaunchTarget, Launcher, Options, Supervisor};
use nestor_runner_core::sys;

const CREDENTIAL: &str = "nestor_runner_test_credential_0123456789";
const MACHINE_ID: &str = "6f1c2a4e-9b3d-4c5e-8f70-1a2b3c4d5e6f";
const INSTANCE: &str = "nestor.test.invalid";

struct Issuer(AtomicU32);

impl CredentialIssuer for Issuer {
    fn issue(&self, _runner: &RunnerRecord) -> Result<Secret, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Secret::new(CREDENTIAL))
    }
}

fn double() -> PathBuf {
    // Tests run from target/<profile>/deps; examples are built next to it.
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let path = deps.parent().unwrap().join("examples/run_once_double");
    assert!(path.exists(), "build the examples first (cargo test builds them): {}", path.display());
    path
}

struct Setup {
    supervisor: Arc<Supervisor>,
    launcher: Arc<ProcessLauncher>,
    issuer: Arc<Issuer>,
    dir: tempfile::TempDir,
}

/// A supervisor on the real launcher; `double_args` are the double's own test options.
fn setup(double_args: &[&str]) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("runners");
    setup_in(dir, &root, Arc::new(MemoryStore::default()), double_args)
}

/// The same, on `store`, with runners under `root`; `dir` is kept as long as the setup.
fn setup_in(dir: tempfile::TempDir, root: &Path, store: Arc<dyn Store>, double_args: &[&str]) -> Setup {
    let redactor = Redactor::new();
    let mut base_args = vec!["run-once".to_string()];
    base_args.extend(double_args.iter().map(|s| s.to_string()));
    let config = ProcessConfig::new(double(), base_args, root.to_path_buf());
    let launcher = Arc::new(ProcessLauncher::new(config, redactor.clone()).unwrap());
    let clock: Clock = Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64);
    let mut options = Options::new(ServiceMode::Live, 8);
    options.kill_grace_ms = 300;
    let supervisor = Arc::new(Supervisor::open(store, launcher.clone(), clock, options, redactor));
    let issuer = Arc::new(Issuer(AtomicU32::new(0)));
    launcher.attach(&supervisor, issuer.clone());
    supervisor.reconcile();
    supervisor.set_machine_id(Some(MACHINE_ID.into()));
    supervisor.set_connection(Connection::Associated { instance: INSTANCE.into(), since_ms: 0 });
    Setup { supervisor, launcher, issuer, dir }
}

/// Waits as the service does: ticking every 20 ms. Until a runner has claimed its task, the
/// tick offers the queue again, so a launch that failed is retried after its backoff.
fn wait_on(setup: &Setup, what: &str, mut condition: impl FnMut() -> bool) {
    let mut ticking = || {
        let claimed = setup.supervisor.runners().iter().any(|r| r.last_task_at_ms.is_some());
        let paused = setup.supervisor.status().mode == Mode::Paused;
        setup.supervisor.tick(&if claimed || paused { Queue::empty() } else { Queue::single("scope-a", 1) });
        condition()
    };
    wait_or(what, &mut ticking, &|| dump(setup));
}

fn wait_or(what: &str, condition: &mut dyn FnMut() -> bool, dump: &dyn Fn() -> String) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}: {}", dump());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn dump(setup: &Setup) -> String {
    let mut text = format!("{:#?}\n", setup.supervisor.runners());
    for runner in setup.supervisor.runners() {
        text += &logs(setup, &runner.id);
    }
    text + &format!("\n{:?}", setup.supervisor.events_since(0).unwrap().iter().map(|e| &e.kind).collect::<Vec<_>>())
}

fn logs(setup: &Setup, runner_id: &str) -> String {
    setup.supervisor.logs(runner_id, 200).unwrap().iter().map(|l| l.message.clone()).collect::<Vec<_>>().join("\n")
}

fn finished_with(setup: &Setup, outcome: TaskOutcome) -> bool {
    setup
        .supervisor
        .events_since(0)
        .unwrap()
        .iter()
        .any(|e| matches!(&e.kind, EventKind::TaskFinished { outcome: o, .. } if *o == outcome))
}

fn state(setup: &Setup, runner_id: &str) -> RunnerState {
    setup.supervisor.runner(runner_id).unwrap().state
}

/// Ticks for a while without a queue: nothing should be launched meanwhile.
fn idle_ticks(setup: &Setup, ms: u64) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        setup.supervisor.tick(&Queue::single("scope-a", 1));
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the process is gone (a zombie waiting for its parent counts as gone).
fn gone(pid: u32, start_time_us: u64) -> bool {
    sys::process_info(pid).is_none_or(|p| p.start_time_us != start_time_us || p.zombie)
}

#[test]
fn a_real_one_job_runner_is_launched_by_contract_v1_and_runs_one_task() {
    let setup = setup(&["--double-claim", "t-1", "--double-job-ms", "300"]);
    setup.supervisor.tick(&Queue::single("scope-a", 3));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim", || setup.supervisor.status().busy_runners == 1);
    let busy = setup.supervisor.runner(&runner.id).unwrap();
    assert_eq!(busy.task_ref.as_deref(), Some("t-1"));

    // The recorded process is recognized by pid, start time and generation marker.
    let pid = setup.supervisor.runner_pids()[0];
    let record = RunnerRecord {
        id: runner.id.clone(),
        name: runner.name.clone(),
        generation: busy.generation,
        pid: Some(pid),
        start_time_us: sys::process_start_time_us(pid),
        launching_since: None,
        scope: busy.scope.clone(),
        created_at_ms: 0,
        manual: false,
    };
    assert!(setup.launcher.is_alive(&record));
    assert!(!setup.launcher.is_alive(&RunnerRecord { generation: busy.generation + 1, ..record.clone() }));

    // The argv of contract v1: ids, machine, server, fds 3 and 4; never --scope, never a secret.
    let arguments = sys::process_arguments(pid).unwrap();
    let value_of = |flag: &str| arguments.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
    assert_eq!(arguments[1], "run-once");
    assert_eq!(value_of("--runner-id").as_deref(), Some(runner.id.as_str()));
    assert_eq!(value_of("--generation"), Some(busy.generation.to_string()));
    assert_eq!(value_of("--machine-id").as_deref(), Some(MACHINE_ID));
    assert_eq!(value_of("--server").as_deref(), Some("https://nestor.test.invalid"));
    assert_eq!(value_of("--credential-fd").as_deref(), Some("3"));
    assert_eq!(value_of("--report-fd").as_deref(), Some("4"));
    assert!(!arguments.iter().any(|a| a == "--scope"), "{arguments:?}");
    assert!(!arguments.iter().any(|a| a.contains(CREDENTIAL)));

    wait_on(&setup, "the exit", || state(&setup, &runner.id) == RunnerState::Stopped);
    assert!(finished_with(&setup, TaskOutcome::Succeeded));
    let text = logs(&setup, &runner.id);
    assert!(text.contains("contract-argv=ok"), "the double accepted the argv: {text}");
    assert!(text.contains("inherited-fds=0,1,2,3,4\n"), "nothing else of the service: {text}");
    assert!(text.contains("credential-in-env=false credential-in-args=false"), "{text}");
    assert!(
        text.contains("env=") && !text.contains("CARGO") && !text.contains("RUST"),
        "environment allow-listed: {text}"
    );
    assert!(text.contains("credential-fd-closed=true"), "read once, then closed: {text}");
    assert!(text.contains("report-fd-cloexec=true"), "{text}");
    assert!(!text.contains(CREDENTIAL), "masked at the source: {text}");
    assert!(text.contains("echo [masked]"), "{text}");
    assert!(!text.contains("Ignored"), "every report followed the contract: {text}");
    let launches = setup.supervisor.runner(&runner.id).unwrap().generation;
    assert_eq!(setup.issuer.0.load(Ordering::SeqCst) as u64, launches, "one credential per launch");
}

#[test]
fn the_launch_needs_a_machine_id_and_an_instance_of_the_right_form() {
    let setup = setup(&[]);
    let record = RunnerRecord {
        id: "0123456789abcdef0123456789abcdef".into(),
        name: "r".into(),
        generation: 1,
        pid: None,
        start_time_us: None,
        launching_since: None,
        scope: Some("scope-a".into()),
        created_at_ms: 0,
        manual: false,
    };
    let target =
        |machine_id: &str, instance: &str| LaunchTarget { machine_id: machine_id.into(), instance: instance.into() };
    let arguments = setup.launcher.arguments(&record, &target(MACHINE_ID, INSTANCE)).unwrap();
    assert!(!arguments.contains(&"--scope".to_string()), "a planned scope is never passed: {arguments:?}");
    assert!(setup.launcher.arguments(&record, &target("m000000000000001", INSTANCE)).is_err());
    assert!(setup.launcher.arguments(&record, &target(MACHINE_ID, "user@nestor.example")).is_err());
    assert!(setup.launcher.spawn(&record, &target(MACHINE_ID, "https://nestor.example")).is_err());
    assert_eq!(setup.issuer.0.load(Ordering::SeqCst), 0, "no credential issued for a refused launch");
}

#[test]
fn draining_never_interrupts_a_task_and_ends_an_idle_runner() {
    let setup = setup(&["--double-claim", "t-long", "--double-job-ms", "800"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim", || setup.supervisor.status().busy_runners == 1);
    setup.supervisor.set_mode(Mode::Paused, None).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(state(&setup, &runner.id), RunnerState::Busy, "pausing interrupts nothing");
    wait_on(&setup, "the end of the task", || state(&setup, &runner.id) == RunnerState::Stopped);
    assert!(finished_with(&setup, TaskOutcome::Succeeded));

    // An idle runner (no task to claim) gets the drain line and exits, not counted as a crash.
    let idle = self::setup(&["--double-idle-ms", "30000"]);
    idle.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = idle.supervisor.runners().remove(0);
    wait_on(&idle, "ready", || state(&idle, &runner.id) == RunnerState::Ready);
    idle.supervisor.set_mode(Mode::Paused, None).unwrap();
    wait_on(&idle, "the drained exit", || state(&idle, &runner.id) == RunnerState::Stopped);
    assert!(idle.supervisor.runner(&runner.id).unwrap().failure_reason.is_none());
    assert!(logs(&idle, &runner.id).contains("drain-line-received"), "{}", logs(&idle, &runner.id));
}

#[test]
fn ending_the_association_terminates_the_process_group() {
    let setup = setup(&["--double-claim", "t-1", "--double-job-ms", "60000"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim", || setup.supervisor.status().busy_runners == 1);
    let pid = setup.supervisor.runner_pids()[0];
    let identity = sys::ProcessIdentity::of(pid).unwrap();
    setup.supervisor.end_association(Connection::Revoked);
    wait_on(&setup, "the runner to be gone", || setup.supervisor.runner(&runner.id).is_none());
    assert!(!identity.is_current(), "the real process is gone");
    assert!(finished_with(&setup, TaskOutcome::Stopped));
}

#[test]
fn the_reported_outcome_is_kept_and_a_failed_job_is_not_a_crash() {
    let setup = setup(&["--double-claim", "t-1", "--double-job-ms", "50", "--double-finish", "failed"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the exit", || {
        state(&setup, &runner.id) == RunnerState::Stopped && finished_with(&setup, TaskOutcome::Failed)
    });
    assert!(setup.supervisor.runner(&runner.id).unwrap().failure_reason.is_none());

    let released = self::setup(&["--double-claim", "t-2", "--double-job-ms", "50", "--double-finish", "released"]);
    released.supervisor.tick(&Queue::single("scope-a", 1));
    wait_on(&released, "the release", || finished_with(&released, TaskOutcome::Released));
}

/// A7: a job that failed `claude_not_signed_in` says so in the machine's status; a job that
/// succeeds afterwards clears it. The failure is read ahead of contract v1 (`finished.failure`).
#[test]
fn a_job_without_claude_code_signed_in_shows_on_the_machine() {
    let setup = setup(&[
        "--double-claim",
        "t-1",
        "--double-job-ms",
        "50",
        "--double-finish",
        "failed",
        "--double-failure",
        "claude_not_signed_in",
    ]);
    assert_eq!(setup.supervisor.status().claude, ClaudeSignIn::Unknown);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the failed job", || finished_with(&setup, TaskOutcome::Failed));
    assert_eq!(setup.supervisor.status().claude, ClaudeSignIn::NotSignedIn);
    assert!(logs(&setup, &runner.id).contains("claude-token set"), "{}", dump(&setup));
    assert!(setup.supervisor.runner(&runner.id).unwrap().failure_reason.is_none(), "a failed job, not a crash");

    // Another failure is not Claude Code's.
    let other = self::setup(&[
        "--double-claim",
        "t-2",
        "--double-job-ms",
        "50",
        "--double-finish",
        "failed",
        "--double-failure",
        "invalid_commit_author",
    ]);
    other.supervisor.tick(&Queue::single("scope-a", 1));
    wait_on(&other, "the failed job", || finished_with(&other, TaskOutcome::Failed));
    assert_eq!(other.supervisor.status().claude, ClaudeSignIn::Unknown);

    let done = self::setup(&["--double-claim", "t-3", "--double-job-ms", "50"]);
    done.supervisor.tick(&Queue::single("scope-a", 1));
    wait_on(&done, "the job", || finished_with(&done, TaskOutcome::Succeeded));
    assert_eq!(done.supervisor.status().claude, ClaudeSignIn::SignedIn);
}

/// A child the launcher has not reaped is the runner whatever its argv became: `terminate`
/// reaches it (KCA-737 nº3).
#[test]
fn terminate_reaches_a_direct_child_whose_argv_changed() {
    let setup = setup(&["--double-exec-sleep"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    wait_on(&setup, "the hello", || setup.supervisor.runners().iter().any(|r| r.state == RunnerState::Ready));
    let runner = setup.supervisor.runners().remove(0);
    let pid = setup.supervisor.runner_pids()[0];
    let identity = sys::ProcessIdentity::of(pid).unwrap();
    wait_or(
        "the exec",
        &mut || sys::process_arguments(pid).is_some_and(|a| a.iter().any(|p| p.ends_with("sleep"))),
        &|| dump(&setup),
    );
    assert!(!sys::process_arguments(pid).unwrap().iter().any(|a| a == "--runner-id"), "the marker is gone");
    setup.supervisor.end_association(Connection::Revoked);
    wait_on(&setup, "the runner to be gone", || setup.supervisor.runner(&runner.id).is_none());
    assert!(!identity.is_current(), "the child was terminated");
}

#[test]
fn a_refused_credential_stops_launches_without_restarting() {
    let setup = setup(&["--double-exit", "77"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the refusal", || state(&setup, &runner.id) == RunnerState::Failed);
    let launches = setup.issuer.0.load(Ordering::SeqCst);
    idle_ticks(&setup, 300);
    assert_eq!(setup.issuer.0.load(Ordering::SeqCst), launches, "never launched again");
    assert_eq!(setup.supervisor.runners().len(), 1, "no new runner either");
    assert_eq!(setup.supervisor.status().admission, Admission::Blocked(BlockReason::Revoked));
    assert!(setup.supervisor.runner(&runner.id).unwrap().failure_reason.unwrap().contains("exit 77"));
}

#[test]
fn a_usage_error_is_not_relaunched_and_unreachable_is_not_a_crash() {
    let setup = setup(&["--double-exit", "64"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the usage error", || state(&setup, &runner.id) == RunnerState::Failed);
    let launches = setup.issuer.0.load(Ordering::SeqCst);
    idle_ticks(&setup, 300);
    assert_eq!(setup.issuer.0.load(Ordering::SeqCst), launches, "never launched again");

    let unreachable = self::setup(&["--double-exit", "75"]);
    unreachable.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = unreachable.supervisor.runners().remove(0);
    wait_on(&unreachable, "the exit", || logs(&unreachable, &runner.id).contains("could not be reached"));
    let view = unreachable.supervisor.runner(&runner.id).unwrap();
    assert_eq!(view.state, RunnerState::Stopped, "waits out its backoff");
    assert!(view.failure_reason.is_none());
    assert!(!logs(&unreachable, &runner.id).contains("unexpectedly"), "not a crash");
}

#[test]
fn a_hello_that_does_not_match_the_launch_terminates_the_runner() {
    let setup = setup(&["--double-hello-generation", "999", "--double-idle-ms", "30000"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the runner to be stopped", || {
        state(&setup, &runner.id) == RunnerState::Stopped
            && setup.supervisor.runner(&runner.id).unwrap().failure_reason.is_some_and(|f| f.contains("hello"))
    });
    let launches = setup.issuer.0.load(Ordering::SeqCst);
    idle_ticks(&setup, 300);
    assert_eq!(setup.issuer.0.load(Ordering::SeqCst), launches, "held: not launched again");
}

#[test]
fn hostile_reports_are_ignored_and_counted() {
    let setup = setup(&["--double-hostile", "--double-claim", "t-1", "--double-job-ms", "100"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the exit", || {
        setup.supervisor.runner(&runner.id).unwrap().last_task_at_ms.is_some()
            && state(&setup, &runner.id) == RunnerState::Stopped
    });
    let started: Vec<String> = setup
        .supervisor
        .events_since(0)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::TaskStarted { task_ref, .. } => Some(task_ref),
            _ => None,
        })
        .collect();
    assert_eq!(started, vec!["t-1".to_string()], "one claim, never the forged ones: {}", dump(&setup));
    let text = logs(&setup, &runner.id);
    let ignored: u32 = text
        .lines()
        .find_map(|l| l.strip_prefix("Ignored ")?.split(' ').next()?.parse().ok())
        .unwrap_or_else(|| panic!("{text}"));
    // Oversized, unknown type, extra field, old format, not JSON, second claim, over the rate.
    assert!(ignored >= 30, "{ignored}: {text}");
    assert!(!text.contains("example.invalid"), "no job content from a report reaches the logs: {text}");
}

#[test]
fn a_runner_deaf_to_sigterm_is_killed_and_what_it_started_is_swept() {
    let setup = setup(&["--double-deaf", "--double-tree", "--double-claim", "t-1", "--double-job-ms", "60000"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim and the tree", || {
        setup.supervisor.status().busy_runners == 1 && logs(&setup, &runner.id).contains("tree a=")
    });
    let text = logs(&setup, &runner.id);
    let pid_of = |name: &str| -> u32 {
        let line = text.lines().find(|l| l.starts_with("tree ")).unwrap();
        line.split(' ').find_map(|part| part.strip_prefix(&format!("{name}="))?.parse().ok()).unwrap()
    };
    let (a, b, c) = (pid_of("a"), pid_of("b"), pid_of("c"));
    assert!(a > 0 && b > 0 && c > 0, "{text}");
    let started = |pid: u32| sys::process_info(pid).unwrap().start_time_us;
    let (a_start, b_start, c_start) = (started(a), started(b), started(c));
    let runner_pid = setup.supervisor.runner_pids()[0];
    let runner_start = started(runner_pid);
    assert_ne!(sys::process_info(b).unwrap().ppid, runner_pid, "b left the runner's tree before the kill");
    assert_ne!(sys::process_info(c).unwrap().ppid, runner_pid, "so did c");

    let stop_at = Instant::now();
    setup.supervisor.stop_runner(&runner.id, true).unwrap();
    wait_on(&setup, "the kill and the sweep", || {
        gone(runner_pid, runner_start) && gone(a, a_start) && gone(b, b_start)
    });
    assert!(stop_at.elapsed() < Duration::from_secs(5), "within 5 s: {:?}", stop_at.elapsed());
    assert!(text.contains("contract-argv=ok"));
    wait_on(&setup, "the exit", || state(&setup, &runner.id) == RunnerState::Stopped);

    // (c) left the tree and the group, and has neither its directory, an open file nor its
    // environment in the runner's directory: the documented residual case.
    assert!(!gone(c, c_start), "the residual case escapes the sweep (KCA-734 §4)");
    unsafe { libc::kill(c as libc::pid_t, libc::SIGKILL) };
    assert!(setup.supervisor.runner(&runner.id).unwrap().failure_reason.is_none());
    drop(setup.dir);
}

/// The pid the double logged as `detached=<pid>`, once it has.
fn detached_child(setup: &Setup, runner_id: &str) -> Option<u32> {
    logs(setup, runner_id).lines().find_map(|l| l.strip_prefix("detached=")?.parse().ok())
}

/// Ticks until the runner is no longer live, checking at every tick that its slot is never
/// freed while its detached child still runs. Returns the state it ended in.
fn until_freed(setup: &Setup, runner_id: &str, child: u32, child_start: u64) -> RunnerState {
    let mut ended = None;
    wait_or(
        "the exit",
        &mut || {
            setup.supervisor.tick(&Queue::empty());
            let state = state(setup, runner_id);
            if !state.is_live() {
                assert!(gone(child, child_start), "the slot was freed ({state:?}) while the detached child runs");
                ended = Some(state);
            }
            ended.is_some()
        },
        &|| dump(setup),
    );
    ended.unwrap()
}

#[test]
fn a_runner_that_exits_143_on_sigterm_leaves_no_detached_child() {
    // As the real `run-once` does: SIGTERM handled, exit 143, not a death by signal (KCA-760).
    let setup = setup(&[
        "--double-sigterm-exit",
        "143",
        "--double-detach",
        "--double-claim",
        "t-1",
        "--double-job-ms",
        "60000",
    ]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim and the detached child", || {
        setup.supervisor.status().busy_runners == 1 && detached_child(&setup, &runner.id).is_some()
    });
    let child = detached_child(&setup, &runner.id).unwrap();
    let child_info = sys::process_info(child).unwrap();
    let runner_pid = setup.supervisor.runner_pids()[0];
    // In a new session: out of the runner's group, and no descendant once the runner exits.
    assert_eq!(child_info.ppid, runner_pid);
    assert_ne!(child_info.pgid, runner_pid, "out of the runner's group");

    setup.supervisor.stop_runner(&runner.id, true).unwrap();
    let ended = until_freed(&setup, &runner.id, child, child_info.start_time_us);
    let alive = !gone(child, child_info.start_time_us);
    if alive {
        unsafe { libc::kill(child as libc::pid_t, libc::SIGKILL) };
    }
    assert!(!alive, "no child survives the stop");
    assert_eq!(ended, RunnerState::Stopped, "{}", logs(&setup, &runner.id));
    assert!(logs(&setup, &runner.id).contains("status 143"), "it exited 143: {}", logs(&setup, &runner.id));
    assert!(setup.supervisor.runner(&runner.id).unwrap().failure_reason.is_none());
    assert!(finished_with(&setup, TaskOutcome::Stopped));
    drop(setup.dir);
}

#[test]
fn a_runner_that_exits_0_is_swept_before_its_slot_is_freed() {
    let setup = setup(&["--double-detach", "--double-claim", "t-1", "--double-job-ms", "300"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the detached child", || detached_child(&setup, &runner.id).is_some());
    let child = detached_child(&setup, &runner.id).unwrap();
    let child_start = sys::process_info(child).unwrap().start_time_us;
    let ended = until_freed(&setup, &runner.id, child, child_start);
    let alive = !gone(child, child_start);
    if alive {
        unsafe { libc::kill(child as libc::pid_t, libc::SIGKILL) };
    }
    assert!(!alive, "what a runner leaves after a normal exit is swept too");
    assert_eq!(ended, RunnerState::Stopped);
    assert!(finished_with(&setup, TaskOutcome::Succeeded), "{}", dump(&setup));
    assert!(setup.supervisor.runner(&runner.id).unwrap().failure_reason.is_none());
    drop(setup.dir);
}

#[test]
fn a_relative_program_path_is_refused() {
    let config = ProcessConfig::new("runner".into(), vec![], "/nonexistent".into());
    assert!(ProcessLauncher::new(config, Redactor::new()).is_err());
}

#[test]
fn a_runner_process_is_refused_by_the_local_api() {
    use nestor_runner_core::association::Associator;
    use nestor_runner_core::ipc::{self, PeerPolicy, Service};
    use nestor_runner_core::secrets::stores_for;

    let dir = tempfile::Builder::new().prefix("t").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let socket = dir.path().join("s.sock");
    let socket_text = socket.to_string_lossy().to_string();
    let setup = setup(&["--double-connect", &socket_text, "--double-idle-ms", "2000"]);
    let clock: Clock = Arc::new(|| 0);
    let (secrets, keys) = stores_for(ServiceMode::Demo, None);
    let associator = Arc::new(Associator::new(None, keys, secrets, setup.supervisor.clone(), clock.clone()));
    // Even with unsigned clients allowed, a runner and its descendants are refused.
    let policy = PeerPolicy { allow_unsigned_clients: true, ..PeerPolicy::default() };
    let service = Arc::new(Service::new(setup.supervisor.clone(), associator, clock, policy));
    let bound = ipc::bind(&socket).unwrap();
    std::thread::spawn(move || ipc::serve(&bound.listener, service));

    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the runner's answer", || logs(&setup, &runner.id).contains("api-answer"));
    let text = logs(&setup, &runner.id);
    assert!(text.contains("permissionDenied"), "{text}");
    // The test process itself, not a runner, is served.
    let hello = ipc::call(&socket, &serde_json::json!({ "apiVersion": "1.1", "op": "hello" })).unwrap();
    assert_eq!(hello["result"]["access"], "full");
}

// MARK: Recoverable launch (KCA-741)

/// What [`Gate`] does to the save that records a runner's pid (step 3 of a launch).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AtPidSave {
    /// Checks the process is alive and has not got its credential, then saves.
    Check,
    /// Fails, as on a full disk.
    Fail,
    /// Never returns until thawed, then fails: the service stops right there.
    Freeze,
}

/// A file store that watches the saves recording a pid, for the launch barrier tests.
struct Gate {
    inner: FileStore,
    runners_root: PathBuf,
    at_pid_save: AtPidSave,
    /// The processes seen at that save (pid, start time), and whether each had its credential.
    seen: std::sync::Mutex<Vec<(u32, u64, bool)>>,
    frozen: std::sync::atomic::AtomicBool,
    thawed: std::sync::atomic::AtomicBool,
}

impl Gate {
    fn new(dir: &Path, at_pid_save: AtPidSave) -> Arc<Gate> {
        Arc::new(Gate {
            inner: FileStore::new(&dir.join("state")).unwrap(),
            runners_root: dir.join("runners"),
            at_pid_save,
            seen: Default::default(),
            frozen: false.into(),
            thawed: false.into(),
        })
    }

    fn seen(&self) -> Vec<(u32, u64, bool)> {
        self.seen.lock().unwrap().clone()
    }
}

/// Whether the double of this runner got a byte of credential (`--double-mark-credential`).
fn credential_received(runners_root: &Path, runner_id: &str) -> bool {
    runners_root.join(runner_id).join("credential-received").exists()
}

impl Store for Gate {
    fn load(&self) -> Loaded {
        self.inner.load()
    }

    fn save(&self, state: &StoredState) -> std::io::Result<()> {
        if self.thawed.load(Ordering::SeqCst) {
            // The service that froze is "dead": it never writes again.
            return Err(std::io::Error::other("the service stopped"));
        }
        let previous = self.inner.load();
        let recorded = |id: &str| previous.state().runners.iter().any(|r| r.id == id && r.pid.is_some());
        for runner in state.runners.iter().filter(|r| r.pid.is_some() && !recorded(&r.id)) {
            let (pid, start) = (runner.pid.unwrap(), runner.start_time_us.unwrap());
            // Long enough for a credential written early to reach the double.
            std::thread::sleep(Duration::from_millis(150));
            assert!(!gone(pid, start), "the process waits for its credential while its pid is saved");
            let received = credential_received(&self.runners_root, &runner.id);
            self.seen.lock().unwrap().push((pid, start, received));
            match self.at_pid_save {
                AtPidSave::Check => {}
                AtPidSave::Fail => return Err(std::io::Error::other("no space left on device")),
                AtPidSave::Freeze => {
                    self.frozen.store(true, Ordering::SeqCst);
                    while !self.thawed.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    return Err(std::io::Error::other("the service stopped"));
                }
            }
        }
        self.inner.save(state)
    }
}

/// The live processes carrying this runner's id in their arguments, by generation.
fn processes_of(runner_id: &str) -> Vec<(u32, String)> {
    sys::all_pids()
        .into_iter()
        .filter(|pid| sys::process_info(*pid).is_some_and(|p| !p.zombie))
        .filter_map(|pid| {
            let arguments = sys::process_arguments(pid)?;
            let value_of = |flag: &str| arguments.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
            (value_of("--runner-id").as_deref() == Some(runner_id)).then_some(())?;
            Some((pid, value_of("--generation")?))
        })
        .collect()
}

#[test]
fn the_credential_is_written_only_after_the_pid_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(dir.path(), AtPidSave::Check);
    let root = dir.path().join("runners");
    let setup = setup_in(
        dir,
        &root,
        gate.clone(),
        &["--double-mark-credential", "--double-claim", "t-1", "--double-job-ms", "200"],
    );
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);

    let seen = gate.seen();
    assert_eq!(seen.len(), 1, "one launch, its pid saved once");
    assert!(!seen[0].2, "no credential before the pid was saved");
    // Saved, then released: the runner gets its credential and claims.
    wait_on(&setup, "the claim", || setup.supervisor.status().busy_runners == 1);
    assert!(credential_received(&root, &runner.id));
    let stored = gate.load().state().runners[0].clone();
    assert_eq!((stored.pid, stored.launching_since), (Some(seen[0].0), None), "pid recorded, intent cleared");
}

#[test]
fn a_pid_that_cannot_be_saved_kills_the_process_before_its_credential() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(dir.path(), AtPidSave::Fail);
    let root = dir.path().join("runners");
    let setup = setup_in(dir, &root, gate.clone(), &["--double-mark-credential", "--double-claim", "t-1"]);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);

    let seen = gate.seen();
    assert_eq!(seen.len(), 1);
    let (pid, start, received) = seen[0];
    assert!(!received);
    assert!(gone(pid, start), "killed before the launch returned");
    assert!(processes_of(&runner.id).is_empty());
    std::thread::sleep(Duration::from_millis(200));
    assert!(!credential_received(&root, &runner.id), "no credential was ever written");
    let text = logs(&setup, &runner.id);
    assert!(text.contains("could not be saved") && text.contains("before getting its credential"), "{text}");
    assert!(!text.contains("contract-argv=ok"), "the runner never got past its credential: {text}");
    assert!(!state(&setup, &runner.id).is_live());
    assert_eq!(setup.supervisor.runner_pids(), Vec::<u32>::new());
}

#[test]
fn a_launch_interrupted_before_its_pid_is_saved_is_ended_at_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let root = path.join("runners");
    let double = ["--double-mark-credential", "--double-claim", "t-1", "--double-job-ms", "200"];
    let frozen = Gate::new(&path, AtPidSave::Freeze);
    let first = setup_in(dir, &root, frozen.clone(), &double);

    // The service stops between the spawn and the save of the pid: the process lives on,
    // unknown to the state file but for the launch intent.
    let ticking = Arc::clone(&first.supervisor);
    let stuck = std::thread::spawn(move || ticking.tick(&Queue::single("scope-a", 1)));
    wait_or("the frozen save", &mut || frozen.frozen.load(Ordering::SeqCst), &|| String::new());
    let (orphan, orphan_start, received) = frozen.seen()[0];
    assert!(!received);
    let on_disk = FileStore::new(&path.join("state")).unwrap().load().state().runners[0].clone();
    assert_eq!(on_disk.pid, None, "the pid never reached the state file");
    let since = on_disk.launching_since.expect("the launch intent was saved before the spawn");
    assert!(orphan_start >= since, "launched after its intent");
    let generation = on_disk.generation.to_string();
    assert_eq!(processes_of(&on_disk.id), vec![(orphan, generation.clone())], "a real process survives");

    // A new service starts on the same state file. Nothing launches before `reconcile`, which
    // ends the orphan.
    let second = setup_in(tempfile::tempdir().unwrap(), &root, Gate::new(&path, AtPidSave::Check), &double);
    assert!(gone(orphan, orphan_start), "ended by reconcile, before anything else");
    assert!(processes_of(&on_disk.id).is_empty());
    assert!(!credential_received(&root, &on_disk.id), "it never got a credential");
    let text = logs(&second, &on_disk.id);
    assert!(text.contains("Ended 1 process(es) of a launch interrupted"), "{text}");
    let after = FileStore::new(&path.join("state")).unwrap().load().state().runners[0].clone();
    assert_eq!(after.launching_since, None, "intent cleared");

    // Then one runner, a new generation, and never two processes.
    second.supervisor.tick(&Queue::single("scope-a", 1));
    let live = processes_of(&on_disk.id);
    assert_eq!(live.len(), 1, "{live:?}");
    assert_ne!(live[0].1, generation);
    wait_on(&second, "the claim", || second.supervisor.status().busy_runners == 1);
    assert_eq!(processes_of(&on_disk.id).len(), 1);

    // The first service never comes back.
    frozen.thawed.store(true, Ordering::SeqCst);
    stuck.join().unwrap();
    drop(first);
}
