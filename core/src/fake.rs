//! Launchers that start no real runner.
//!
//! [`FakeLauncher`] keeps pretend processes in memory, for the tests and the `--simulate` demo of
//! the service. It records every launch so tests can check that a runner never has two live
//! processes, and every drain and termination so they can check that nothing busy was
//! interrupted. [`UnavailableLauncher`] is what the live service uses while no real runner can be
//! launched: it refuses to launch anything.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::persist::RunnerRecord;
use crate::supervisor::{LaunchTarget, Launcher};
use crate::sys::ProcessIdentity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeProcess {
    pub runner_id: String,
    pub generation: u64,
    pub pid: u32,
    pub scope: Option<String>,
    /// Asked to stop claiming and exit after its job.
    pub drained: bool,
    /// Asked to end now (graceful signal).
    pub terminated: bool,
    /// Killed.
    pub killed: bool,
}

#[derive(Default)]
pub struct FakeLauncher {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    next_pid: u32,
    live: BTreeMap<u32, FakeProcess>,
    spawns: u32,
    /// Set when a launch happened while the same runner still had a live process.
    duplicate_launches: u32,
    fail_spawns: bool,
    forgotten: Vec<String>,
}

impl FakeLauncher {
    pub fn new() -> Self {
        FakeLauncher { state: Mutex::new(FakeState { next_pid: 10_000, ..FakeState::default() }) }
    }

    pub fn live(&self) -> Vec<FakeProcess> {
        self.state.lock().unwrap().live.values().cloned().collect()
    }

    pub fn live_for(&self, runner_id: &str) -> Vec<FakeProcess> {
        self.live().into_iter().filter(|p| p.runner_id == runner_id).collect()
    }

    pub fn spawns(&self) -> u32 {
        self.state.lock().unwrap().spawns
    }

    pub fn duplicate_launches(&self) -> u32 {
        self.state.lock().unwrap().duplicate_launches
    }

    /// Makes the next launches fail, to exercise the crash path.
    pub fn set_fail_spawns(&self, fail: bool) {
        self.state.lock().unwrap().fail_spawns = fail;
    }

    /// Ends a pretend process, as if it exited (the caller reports it to the supervisor).
    pub fn end(&self, pid: u32) -> Option<FakeProcess> {
        self.state.lock().unwrap().live.remove(&pid)
    }

    pub fn drained(&self) -> Vec<FakeProcess> {
        self.live().into_iter().filter(|p| p.drained).collect()
    }

    pub fn terminated(&self) -> Vec<FakeProcess> {
        self.live().into_iter().filter(|p| p.terminated || p.killed).collect()
    }

    pub fn forgotten(&self) -> Vec<String> {
        self.state.lock().unwrap().forgotten.clone()
    }

    fn with_process(&self, runner: &RunnerRecord, change: impl FnOnce(&mut FakeProcess)) {
        let mut state = self.state.lock().unwrap();
        if let Some(pid) = runner.pid {
            if let Some(process) = state.live.get_mut(&pid) {
                if process.runner_id == runner.id && process.generation == runner.generation {
                    change(process);
                }
            }
        }
    }
}

impl Launcher for FakeLauncher {
    fn spawn(&self, runner: &RunnerRecord, _target: &LaunchTarget) -> Result<ProcessIdentity, String> {
        let mut state = self.state.lock().unwrap();
        if state.fail_spawns {
            return Err("simulated launch failure".into());
        }
        if state.live.values().any(|p| p.runner_id == runner.id) {
            state.duplicate_launches += 1;
        }
        state.next_pid += 1;
        state.spawns += 1;
        let pid = state.next_pid;
        state.live.insert(
            pid,
            FakeProcess {
                runner_id: runner.id.clone(),
                generation: runner.generation,
                pid,
                scope: runner.scope.clone(),
                drained: false,
                terminated: false,
                killed: false,
            },
        );
        Ok(ProcessIdentity { pid, start_time_us: pid as u64 * 1_000 })
    }

    fn drain(&self, runner: &RunnerRecord) {
        self.with_process(runner, |p| p.drained = true);
    }

    fn terminate(&self, runner: &RunnerRecord, force: bool) {
        self.with_process(runner, |p| {
            p.terminated = true;
            p.killed |= force;
        });
    }

    fn is_alive(&self, runner: &RunnerRecord) -> bool {
        let Some(pid) = runner.pid else { return false };
        self.state.lock().unwrap().live.get(&pid).is_some_and(|p| {
            p.runner_id == runner.id
                && p.generation == runner.generation
                && runner.start_time_us == Some(pid as u64 * 1_000)
        })
    }

    fn forget(&self, runner_id: &str) {
        self.state.lock().unwrap().forgotten.push(runner_id.to_string());
    }
}

/// The live service's launcher while no real runner can be launched (no association, or no
/// runner binary configured): nothing is ever launched.
pub struct UnavailableLauncher;

impl Launcher for UnavailableLauncher {
    fn spawn(&self, _runner: &RunnerRecord, _target: &LaunchTarget) -> Result<ProcessIdentity, String> {
        Err("no runner can be launched in this version".into())
    }

    fn drain(&self, _runner: &RunnerRecord) {}

    fn terminate(&self, _runner: &RunnerRecord, _force: bool) {
        // Nothing was launched, so there is nothing to stop. Never signal a recorded pid that
        // could have been reused by another process.
    }

    fn is_alive(&self, _runner: &RunnerRecord) -> bool {
        false
    }

    fn forget(&self, _runner_id: &str) {}
}
