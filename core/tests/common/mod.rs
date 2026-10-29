#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nestor_runner_core::admission::Token;
use nestor_runner_core::fake::FakeLauncher;
use nestor_runner_core::model::{Connection, RunnerState, ServiceMode};
use nestor_runner_core::persist::{MemoryStore, Store};
use nestor_runner_core::supervisor::{Clock, Options, Supervisor};

/// A supervisor on fake runners, a memory store and a clock the test moves by hand.
pub struct Harness {
    pub supervisor: Arc<Supervisor>,
    pub launcher: Arc<FakeLauncher>,
    pub store: Arc<dyn Store>,
    pub time: Arc<AtomicU64>,
}

pub fn options() -> Options {
    let mut options = Options::new(ServiceMode::Demo, 8);
    options.idle_timeout_ms = 60_000;
    options
}

impl Harness {
    pub fn new() -> Self {
        Self::with(Arc::new(MemoryStore::default()), Arc::new(FakeLauncher::new()), options())
    }

    pub fn with(store: Arc<dyn Store>, launcher: Arc<FakeLauncher>, options: Options) -> Self {
        let time = Arc::new(AtomicU64::new(1_000_000));
        let clock_time = Arc::clone(&time);
        let clock: Clock = Arc::new(move || clock_time.load(Ordering::SeqCst));
        let supervisor = Arc::new(Supervisor::open(store.clone(), launcher.clone(), clock, options));
        Harness { supervisor, launcher, store, time }
    }

    /// Reconciled and associated: ready to admit.
    pub fn associated() -> Self {
        let harness = Self::new();
        harness.supervisor.reconcile();
        harness.associate();
        harness
    }

    pub fn associate(&self) {
        self.supervisor.set_connection(Connection::Associated { instance: "test.invalid".into(), since_ms: 0 });
    }

    pub fn advance(&self, ms: u64) {
        self.time.fetch_add(ms, Ordering::SeqCst);
    }

    /// Brings every starting runner up, as the runner would report it.
    pub fn bring_up(&self) {
        for runner in self.supervisor.runners() {
            if runner.state == RunnerState::Starting {
                self.supervisor.runner_ready(&runner.id, runner.generation).unwrap();
            }
        }
    }

    /// Ends the pretend processes the supervisor asked to stop and reports their exit.
    pub fn complete_stops(&self) {
        for process in self.launcher.terminating() {
            self.launcher.end(process.pid);
            self.supervisor.runner_exited(&process.runner_id, process.generation, "stopped");
        }
    }

    /// Asks a token for every ready runner and starts a task with it.
    pub fn admit_all(&self) -> Vec<Token> {
        let mut tokens = Vec::new();
        for runner in self.supervisor.runners() {
            if runner.state == RunnerState::Ready {
                if let Ok(token) = self.supervisor.admit(&runner.id, runner.generation) {
                    self.supervisor.task_started(&token, &format!("task-{}", token.id)).unwrap();
                    tokens.push(token);
                }
            }
        }
        tokens
    }

    pub fn count(&self, state: RunnerState) -> usize {
        self.supervisor.runners().iter().filter(|r| r.state == state).count()
    }
}
