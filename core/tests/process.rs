//! The real launcher, with real processes (`examples/fake_one_job_runner.rs`): `argv` without a
//! shell, credential read once from an inherited pipe, allow-listed environment, drain versus
//! termination, process identity, masking (S8.2, S8.5, S8.6, B5, N5).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nestor_runner_core::mask::Redactor;
use nestor_runner_core::model::*;
use nestor_runner_core::persist::{MemoryStore, RunnerRecord};
use nestor_runner_core::process::{CredentialIssuer, ProcessConfig, ProcessLauncher};
use nestor_runner_core::secrets::Secret;
use nestor_runner_core::supervisor::{Clock, Launcher, Options, Supervisor};
use nestor_runner_core::sys;

const CREDENTIAL: &str = "nestor_runner_test_credential_0123456789";

struct Issuer(AtomicU32);

impl CredentialIssuer for Issuer {
    fn issue(&self, _runner: &RunnerRecord) -> Result<Secret, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Secret::new(CREDENTIAL))
    }
}

fn fake_runner() -> PathBuf {
    // Tests run from target/<profile>/deps; examples are built next to it.
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let path = deps.parent().unwrap().join("examples/fake_one_job_runner");
    assert!(path.exists(), "build the examples first (cargo test builds them): {}", path.display());
    path
}

struct Setup {
    supervisor: Arc<Supervisor>,
    launcher: Arc<ProcessLauncher>,
    issuer: Arc<Issuer>,
    _dir: tempfile::TempDir,
}

fn setup(base_args: &[&str], ceiling: u32) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let redactor = Redactor::new();
    let config = ProcessConfig {
        program: fake_runner(),
        base_args: base_args.iter().map(|s| s.to_string()).collect(),
        runners_root: dir.path().join("runners"),
    };
    let launcher = Arc::new(ProcessLauncher::new(config, redactor.clone()).unwrap());
    let clock: Clock = Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64);
    let mut options = Options::new(ServiceMode::Live, 8);
    options.kill_grace_ms = 300;
    let supervisor =
        Arc::new(Supervisor::open(Arc::new(MemoryStore::default()), launcher.clone(), clock, options, redactor));
    let issuer = Arc::new(Issuer(AtomicU32::new(0)));
    launcher.attach(&supervisor, issuer.clone());
    supervisor.reconcile();
    supervisor.set_machine_id(Some("m-test".into()));
    supervisor.set_connection(Connection::Associated { instance: "test.invalid".into(), since_ms: 0 });
    supervisor.set_max_concurrent_tasks(ceiling, None).unwrap();
    Setup { supervisor, launcher, issuer, _dir: dir }
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

#[test]
fn a_real_one_job_runner_gets_its_credential_safely_and_runs_one_task() {
    let setup = setup(&["--claim", "t-1", "--job-ms", "300"], 1);
    setup.supervisor.tick(&Queue::single("scope-a", 3));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim", || setup.supervisor.status().busy_runners == 1);
    let busy = setup.supervisor.runner(&runner.id).unwrap();
    assert_eq!(busy.task_ref.as_deref(), Some("t-1"));
    assert_eq!(busy.scope.as_deref(), Some("scope-a"));

    // The recorded process is recognized by pid, start time and generation marker.
    let record = RunnerRecord {
        id: runner.id.clone(),
        name: runner.name.clone(),
        generation: busy.generation,
        pid: setup.supervisor.runner_pids().first().copied(),
        start_time_us: setup.supervisor.runner_pids().first().and_then(|p| sys::process_start_time_us(*p)),
        scope: busy.scope.clone(),
        created_at_ms: 0,
        manual: false,
    };
    assert!(setup.launcher.is_alive(&record));
    assert!(!setup.launcher.is_alive(&RunnerRecord { generation: busy.generation + 1, ..record.clone() }));
    let arguments = sys::process_arguments(record.pid.unwrap()).unwrap();
    assert!(arguments.windows(2).any(|w| w[0] == "--generation" && w[1] == busy.generation.to_string()));
    assert!(!arguments.iter().any(|a| a.contains(CREDENTIAL)));

    wait_on(&setup, "the exit", || setup.supervisor.runner(&runner.id).unwrap().state == RunnerState::Stopped);
    assert!(finished_with(&setup, TaskOutcome::Succeeded));
    let text = logs(&setup, &runner.id);
    assert!(text.contains("credential-in-env=false credential-in-args=false"), "{text}");
    assert!(
        text.contains("env=") && !text.contains("CARGO") && !text.contains("RUST"),
        "environment allow-listed: {text}"
    );
    assert!(text.contains("credential-fd-closed=true"), "read once, then closed: {text}");
    assert!(!text.contains(CREDENTIAL), "masked at the source: {text}");
    assert!(text.contains("echo [masked]"), "{text}");
    let launches = setup.supervisor.runner(&runner.id).unwrap().generation;
    assert_eq!(setup.issuer.0.load(Ordering::SeqCst) as u64, launches, "one credential per launch");
}

#[test]
fn draining_never_interrupts_a_task_and_ends_an_idle_runner() {
    let setup = setup(&["--claim", "t-long", "--job-ms", "800"], 1);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim", || setup.supervisor.status().busy_runners == 1);
    setup.supervisor.set_mode(Mode::Paused, None).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(setup.supervisor.runner(&runner.id).unwrap().state, RunnerState::Busy, "pausing interrupts nothing");
    wait_on(&setup, "the end of the task", || {
        setup.supervisor.runner(&runner.id).unwrap().state == RunnerState::Stopped
    });
    assert!(finished_with(&setup, TaskOutcome::Succeeded));

    // An idle runner (no task to claim) exits when drained, without being counted as a crash.
    let idle = self::setup(&["--idle-ms", "30000"], 1);
    idle.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = idle.supervisor.runners().remove(0);
    wait_on(&idle, "ready", || idle.supervisor.runner(&runner.id).unwrap().state == RunnerState::Ready);
    idle.supervisor.set_mode(Mode::Paused, None).unwrap();
    wait_on(&idle, "the drained exit", || idle.supervisor.runner(&runner.id).unwrap().state == RunnerState::Stopped);
    assert!(idle.supervisor.runner(&runner.id).unwrap().failure_reason.is_none());
}

#[test]
fn ending_the_association_terminates_the_process_group() {
    let setup = setup(&["--claim", "t-1", "--job-ms", "60000"], 1);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the claim", || setup.supervisor.status().busy_runners == 1);
    let pid = setup.supervisor.runner_pids()[0];
    let identity = sys::ProcessIdentity::of(pid).unwrap();
    setup.supervisor.end_association(Connection::Revoked);
    wait_on(&setup, "the runner to be gone", || setup.supervisor.runner(&runner.id).is_none());
    assert!(!identity.is_current(), "the process is gone");
    assert!(finished_with(&setup, TaskOutcome::Stopped));
}

#[test]
fn a_failed_job_is_not_a_crash() {
    let setup = setup(&["--claim", "t-1", "--job-ms", "50", "--fail"], 1);
    setup.supervisor.tick(&Queue::single("scope-a", 1));
    let runner = setup.supervisor.runners().remove(0);
    wait_on(&setup, "the exit", || {
        setup.supervisor.runner(&runner.id).unwrap().state == RunnerState::Stopped
            && finished_with(&setup, TaskOutcome::Failed)
    });
    assert!(setup.supervisor.runner(&runner.id).unwrap().failure_reason.is_none());
}

#[test]
fn a_relative_program_path_is_refused() {
    let config = ProcessConfig { program: "runner".into(), base_args: vec![], runners_root: "/nonexistent".into() };
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
    let setup = setup(&["--connect", &socket_text, "--idle-ms", "2000"], 1);
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
