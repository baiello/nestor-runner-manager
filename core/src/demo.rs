//! The simulated Nestor behind `nestor-runner-service --simulate`.
//!
//! DEMO ONLY: the server ([`FakeNestor`], approving every association), the tasks and the runner
//! processes are invented. The supervisor, the admission rules, the association flow, the
//! persistence and the local API are the real ones, so the interface can be exercised end to
//! end. The service reports `serviceMode: demo`, prefixes its logs with `[demo]`, and keeps its
//! secrets in memory only: a demo service never touches the Keychain (N8).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::fake::FakeLauncher;
use crate::model::{Queue, RunnerExit, RunnerState, TaskOutcome};
use crate::nestor::FakeNestor;
use crate::supervisor::Supervisor;

pub const DEMO_INSTANCE: &str = "demo.nestor.invalid";
pub const DEMO_SCOPES: [&str; 2] = ["demo-workspace-a", "demo-workspace-b"];

pub struct DemoQueue {
    supervisor: Arc<Supervisor>,
    launcher: Arc<FakeLauncher>,
    nestor: Arc<FakeNestor>,
    pending: BTreeMap<String, u32>,
    next_task: u64,
    /// Runner id → (pid, generation, end of its task).
    running: BTreeMap<String, (u32, u64, u64)>,
    ticks: u64,
}

impl DemoQueue {
    pub fn new(supervisor: Arc<Supervisor>, launcher: Arc<FakeLauncher>, nestor: Arc<FakeNestor>) -> Self {
        let pending = BTreeMap::from([(DEMO_SCOPES[0].to_string(), 3), (DEMO_SCOPES[1].to_string(), 1)]);
        DemoQueue { supervisor, launcher, nestor, pending, next_task: 1, running: BTreeMap::new(), ticks: 0 }
    }

    pub fn queue(&self) -> Queue {
        Queue { eligible: self.pending.iter().filter(|(_, n)| **n > 0).map(|(s, n)| (s.clone(), *n)).collect() }
    }

    /// One second of simulated life.
    pub fn step(&mut self, now_ms: u64) {
        self.ticks += 1;
        self.nestor.set_now(now_ms);
        if self.ticks.is_multiple_of(20) {
            let scope = DEMO_SCOPES[(self.ticks / 20 % 2) as usize].to_string();
            let entry = self.pending.entry(scope).or_default();
            *entry = (*entry + 2).min(6);
        }

        // Pretend processes: terminated ones end now; drained ones end now unless they hold a task.
        for process in self.launcher.live() {
            let busy = self.running.contains_key(&process.runner_id);
            if process.terminated || process.killed {
                self.launcher.end(process.pid);
                self.running.remove(&process.runner_id);
                self.supervisor.runner_exited(
                    &process.runner_id,
                    process.generation,
                    RunnerExit::Crashed("terminated".into()),
                );
            } else if process.drained && !busy {
                self.launcher.end(process.pid);
                self.supervisor.runner_exited(&process.runner_id, process.generation, RunnerExit::Idle);
            }
        }
        for runner in self.supervisor.runners() {
            if runner.state == RunnerState::Starting {
                let _ = self.supervisor.runner_ready(&runner.id, runner.generation);
            }
        }

        // Tasks that are due end, and their one-job runner exits.
        let due: Vec<String> =
            self.running.iter().filter(|(_, (_, _, end))| *end <= now_ms).map(|(id, _)| id.clone()).collect();
        for runner_id in due {
            if let Some((pid, generation, _)) = self.running.remove(&runner_id) {
                self.launcher.end(pid);
                self.supervisor.runner_exited(&runner_id, generation, RunnerExit::Finished(TaskOutcome::Succeeded));
            }
        }

        // Waiting runners claim one simulated task of their scope.
        for runner in self.supervisor.runners() {
            if runner.state != RunnerState::Ready || self.running.contains_key(&runner.id) {
                continue;
            }
            let Some(process) = self.launcher.live_for(&runner.id).into_iter().find(|p| !p.drained) else { continue };
            let scope = process.scope.clone().unwrap_or_else(|| DEMO_SCOPES[0].to_string());
            let Some(count) = self.pending.get_mut(&scope).filter(|n| **n > 0) else { continue };
            *count -= 1;
            let task_ref = format!("demo-task-{}", self.next_task);
            let duration_ms = 15_000 + (self.next_task % 4) * 10_000;
            self.next_task += 1;
            if self.supervisor.runner_claimed(&runner.id, runner.generation, &task_ref, Some(&scope)).is_ok() {
                self.supervisor.set_runner_usage(
                    &runner.id,
                    180.0 + (self.next_task % 5) as f64 * 40.0,
                    (2 + self.next_task % 6) << 30,
                );
                self.running.insert(runner.id.clone(), (process.pid, runner.generation, now_ms + duration_ms));
            }
        }
        self.nestor.set_queue(self.queue());
        // A simulated server of nestor #157: scope filter announced, an administrator limit of 4.
        self.nestor.set_scope_filter(true);
        if let Some(machine_id) = self.supervisor.machine_id() {
            self.nestor.set_administrator_limits(&machine_id, 4, false);
        }
    }
}
