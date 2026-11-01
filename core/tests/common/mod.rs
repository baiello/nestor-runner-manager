#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nestor_runner_core::fake::FakeLauncher;
use nestor_runner_core::mask::Redactor;
use nestor_runner_core::model::{Connection, Queue, RunnerExit, RunnerState, ServiceMode, TaskOutcome};
use nestor_runner_core::persist::{MemoryStore, Store};
use nestor_runner_core::supervisor::{Clock, Options, Supervisor};

pub const SCOPE: &str = "scope-a";

/// A supervisor on fake runners, a memory store and a clock the test moves by hand.
pub struct Harness {
    pub supervisor: Arc<Supervisor>,
    pub launcher: Arc<FakeLauncher>,
    pub store: Arc<dyn Store>,
    pub time: Arc<AtomicU64>,
    next_task: AtomicU64,
}

pub fn options() -> Options {
    let mut options = Options::new(ServiceMode::Demo, 8);
    options.idle_timeout_ms = 60_000;
    options
}

pub fn queue(count: u32) -> Queue {
    Queue::single(SCOPE, count)
}

impl Harness {
    pub fn new() -> Self {
        Self::with(Arc::new(MemoryStore::default()), Arc::new(FakeLauncher::new()), options())
    }

    pub fn with(store: Arc<dyn Store>, launcher: Arc<FakeLauncher>, options: Options) -> Self {
        let time = Arc::new(AtomicU64::new(1_000_000));
        let clock_time = Arc::clone(&time);
        let clock: Clock = Arc::new(move || clock_time.load(Ordering::SeqCst));
        let supervisor = Arc::new(Supervisor::open(store.clone(), launcher.clone(), clock, options, Redactor::new()));
        Harness { supervisor, launcher, store, time, next_task: AtomicU64::new(1) }
    }

    /// Reconciled and associated: ready to launch.
    pub fn associated() -> Self {
        let harness = Self::new();
        harness.supervisor.reconcile();
        harness.associate();
        harness
    }

    /// Associated with a Nestor that filters scopes at claim time (S6), so the requested ceiling
    /// applies; [`Harness::associate_without_scope_filter`] for today's servers.
    pub fn associate(&self) {
        self.associate_without_scope_filter();
        self.supervisor.set_scope_filter(true);
    }

    pub fn associate_without_scope_filter(&self) {
        self.supervisor.set_machine_id(Some("m-test".into()));
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

    /// Every waiting runner that was not drained claims a task of its scope. Returns the runner
    /// ids that got one.
    pub fn claim_all(&self) -> Vec<String> {
        let mut claimed = Vec::new();
        for runner in self.supervisor.runners() {
            if runner.state != RunnerState::Ready {
                continue;
            }
            let Some(process) = self.launcher.live_for(&runner.id).into_iter().find(|p| !p.drained) else { continue };
            let scope = process.scope.clone().unwrap_or_else(|| SCOPE.to_string());
            let task = format!("task-{}", self.next_task.fetch_add(1, Ordering::SeqCst));
            if self.supervisor.runner_claimed(&runner.id, runner.generation, &task, Some(&scope)).is_ok() {
                claimed.push(runner.id.clone());
            }
        }
        claimed
    }

    /// Launch for `queue`, bring up, claim: one full round.
    pub fn round(&self, queue: &Queue) -> Vec<String> {
        self.supervisor.tick(queue);
        self.bring_up();
        self.claim_all()
    }

    /// The runner's task ends and its one-job process exits.
    pub fn finish(&self, runner_id: &str) {
        let process = self.launcher.live_for(runner_id).remove(0);
        self.launcher.end(process.pid);
        self.supervisor.runner_exited(runner_id, process.generation, RunnerExit::Finished(TaskOutcome::Succeeded));
    }

    /// Drained processes without a task exit, as a one-job runner does when its input closes.
    pub fn complete_drains(&self) {
        for process in self.launcher.drained() {
            let busy = self.supervisor.runner(&process.runner_id).is_some_and(|r| r.task_ref.is_some());
            if !busy {
                self.launcher.end(process.pid);
                self.supervisor.runner_exited(&process.runner_id, process.generation, RunnerExit::Idle);
            }
        }
    }

    /// Terminated processes exit.
    pub fn complete_terminations(&self) {
        for process in self.launcher.terminated() {
            self.launcher.end(process.pid);
            self.supervisor.runner_exited(
                &process.runner_id,
                process.generation,
                RunnerExit::Crashed("signal 15".into()),
            );
        }
    }

    pub fn count(&self, state: RunnerState) -> usize {
        self.supervisor.runners().iter().filter(|r| r.state == state).count()
    }

    pub fn busy(&self) -> u32 {
        self.supervisor.status().busy_runners
    }
}
