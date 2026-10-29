//! Launchers that start no real runner.
//!
//! [`FakeLauncher`] keeps pretend processes in memory, for the tests and the `--simulate` demo of
//! the service. It records every launch so tests can check that a runner never has two live
//! processes. [`UnavailableLauncher`] is what the live service uses until the real runner is
//! integrated: it refuses to provision or launch anything.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::model::ServiceError;
use crate::persist::RunnerRecord;
use crate::supervisor::Launcher;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeProcess {
    pub runner_id: String,
    pub generation: u64,
    pub pid: u32,
    pub terminate_requested: bool,
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

    /// Processes the supervisor asked to stop.
    pub fn terminating(&self) -> Vec<FakeProcess> {
        self.live().into_iter().filter(|p| p.terminate_requested).collect()
    }
}

impl Launcher for FakeLauncher {
    fn provision(&self, _runner: &RunnerRecord) -> Result<(), ServiceError> {
        Ok(())
    }

    fn spawn(&self, runner: &RunnerRecord) -> Result<u32, String> {
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
                terminate_requested: false,
            },
        );
        Ok(pid)
    }

    fn terminate(&self, _runner_id: &str, _generation: u64, pid: u32) {
        if let Some(process) = self.state.lock().unwrap().live.get_mut(&pid) {
            process.terminate_requested = true;
        }
    }

    fn is_alive(&self, runner_id: &str, generation: u64, pid: u32) -> bool {
        self.state
            .lock()
            .unwrap()
            .live
            .get(&pid)
            .is_some_and(|p| p.runner_id == runner_id && p.generation == generation)
    }

    fn deprovision(&self, _runner_id: &str) {}
}

/// The live service's launcher until the real Nestor runner is integrated (needs the Nestor API
/// work of KCA-719): provisioning is refused, so nothing is ever launched.
pub struct UnavailableLauncher;

impl Launcher for UnavailableLauncher {
    fn provision(&self, _runner: &RunnerRecord) -> Result<(), ServiceError> {
        Err(ServiceError::Unavailable(
            "Runners cannot be provisioned yet: the real runner integration is not in this version.".into(),
        ))
    }

    fn spawn(&self, _runner: &RunnerRecord) -> Result<u32, String> {
        Err("the real runner integration is not in this version".into())
    }

    fn terminate(&self, _runner_id: &str, _generation: u64, _pid: u32) {
        // Nothing was launched, so there is nothing to stop. Never signal a recorded pid that
        // could have been reused by another process.
    }

    fn is_alive(&self, _runner_id: &str, _generation: u64, _pid: u32) -> bool {
        // Without a real runner, no recorded pid can be trusted to be one of ours.
        false
    }

    fn deprovision(&self, _runner_id: &str) {}
}
