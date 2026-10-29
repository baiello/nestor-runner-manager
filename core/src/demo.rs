//! The simulated Nestor queue behind `nestor-runner-service --simulate`.
//!
//! DEMO ONLY: the tasks, the association and the runner processes are invented. The supervisor,
//! the admission rules, the persistence and the local API are the real ones, so the interface can
//! be exercised end to end; the service reports `serviceMode: demo` and prefixes its logs with
//! `[demo]`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::admission::Token;
use crate::fake::FakeLauncher;
use crate::model::{RunnerState, TaskOutcome};
use crate::supervisor::Supervisor;

pub struct DemoQueue {
    supervisor: Arc<Supervisor>,
    launcher: Arc<FakeLauncher>,
    pending: u32,
    next_task: u64,
    running: BTreeMap<String, (Token, u64)>,
    ticks: u64,
}

impl DemoQueue {
    pub fn new(supervisor: Arc<Supervisor>, launcher: Arc<FakeLauncher>) -> Self {
        DemoQueue { supervisor, launcher, pending: 3, next_task: 1, running: BTreeMap::new(), ticks: 0 }
    }

    pub fn pending(&self) -> u32 {
        self.pending
    }

    /// One second of simulated life.
    pub fn step(&mut self, now_ms: u64) {
        self.ticks += 1;
        if self.ticks.is_multiple_of(20) {
            self.pending = (self.pending + 2).min(6);
        }

        // Pretend processes: stop those asked to stop, bring the starting ones up.
        for process in self.launcher.terminating() {
            self.launcher.end(process.pid);
            self.running.remove(&process.runner_id);
            self.supervisor.runner_exited(&process.runner_id, process.generation, "stopped");
        }
        for runner in self.supervisor.runners() {
            if runner.state == RunnerState::Starting {
                let _ = self.supervisor.runner_ready(&runner.id, runner.generation);
            }
        }

        // Finish the tasks that are due.
        let due: Vec<String> =
            self.running.iter().filter(|(_, (_, end))| *end <= now_ms).map(|(id, _)| id.clone()).collect();
        for runner_id in due {
            if let Some((token, _)) = self.running.remove(&runner_id) {
                let _ = self.supervisor.task_finished(&token, TaskOutcome::Succeeded);
                self.supervisor.set_runner_usage(&runner_id, 0.4, 180 << 20);
            }
        }

        // Ready runners ask for a token, then claim a simulated task.
        for runner in self.supervisor.runners() {
            if self.pending == 0 {
                break;
            }
            if runner.state != RunnerState::Ready {
                continue;
            }
            if let Ok(token) = self.supervisor.admit(&runner.id, runner.generation) {
                let task_ref = format!("demo-task-{}", self.next_task);
                let duration_ms = 15_000 + (self.next_task % 4) * 10_000;
                self.next_task += 1;
                self.pending -= 1;
                let _ = self.supervisor.task_started(&token, &task_ref);
                self.supervisor.set_runner_usage(
                    &runner.id,
                    180.0 + (self.next_task % 5) as f64 * 40.0,
                    (2 + self.next_task % 6) << 30,
                );
                self.running.insert(runner.id.clone(), (token, now_ms + duration_ms));
            }
        }

        self.supervisor.tick(self.pending);
    }
}
