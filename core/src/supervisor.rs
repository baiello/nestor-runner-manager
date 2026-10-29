//! The supervisor: the single local authority over this machine's runners.
//!
//! Managed runners are "one job" processes (contract v2 §3.4): launched, they claim at most one
//! job from Nestor, run it and exit. **Launching a runner is the admission**: the supervisor
//! launches one only when the [`Regulator`] allows it (automatic mode, not blocked, fewer live
//! runners than the ceiling, eligible jobs in the active scope). Every operation runs under one
//! lock, so two concurrent calls can never launch the same runner twice or go past the ceiling.
//!
//! Pausing, lowering the ceiling, losing the connection and idle timeouts **drain** runners that
//! have no job yet: they stop claiming and exit; a runner that holds a job finishes it. Nothing
//! is interrupted (principle 8). Only revocation, disassociation and a forced stop from the
//! diagnostics **terminate** runners (graceful signal, then kill after a grace period), because
//! the runner's `SIGTERM` abandons its job.
//!
//! None of this is a security boundary (principle 7): the server enforces leases, ceiling, pause
//! and scope at claim time. Until Nestor announces `capabilities.scope_filter` (S6), the ceiling
//! actually applied is 1 whatever the requested value (KCA-734 §3).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::admission::{Denied, LiveScope, Regulator};
use crate::mask::Redactor;
use crate::model::*;
use crate::persist::{Loaded, RunnerRecord, Store, StoredState, MAX_RUNNERS};
use crate::sys::ProcessIdentity;

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Where a runner works: the machine it acts for and the Nestor instance it talks to. Taken from
/// the association by the supervisor at each launch; never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTarget {
    pub machine_id: String,
    /// The instance's host (`nestor.example`); the runner gets `https://<instance>`.
    pub instance: String,
}

/// Starts and stops runner processes. [`crate::process::ProcessLauncher`] runs real ones;
/// [`crate::fake::FakeLauncher`] pretends, for the tests and the demo.
pub trait Launcher: Send + Sync {
    /// Launches the one-job process of `runner.generation` for `target`. The runner gets its
    /// credential from the launcher, never from the supervisor. `runner.scope` is what the
    /// supervisor planned; contract v1 does not pass it to the runner (KCA-734 §3).
    fn spawn(&self, runner: &RunnerRecord, target: &LaunchTarget) -> Result<ProcessIdentity, String>;
    /// Asks the process to stop claiming and exit once its job, if any, is done. Never
    /// interrupts a job.
    fn drain(&self, runner: &RunnerRecord);
    /// Ends the process now: a graceful signal, or a kill with `force`. Ends its job too; only
    /// for revocation, disassociation and forced stops. Must check the process identity first.
    fn terminate(&self, runner: &RunnerRecord, force: bool);
    /// Whether the recorded process (pid, start time and generation) is still alive.
    fn is_alive(&self, runner: &RunnerRecord) -> bool;
    /// Erases what the runner left locally (its state and work directories).
    fn forget(&self, runner_id: &str);
}

#[derive(Debug, Clone)]
pub struct Options {
    pub service_mode: ServiceMode,
    /// Highest ceiling accepted; a sanity check on the input (the number of cores by default).
    pub ceiling_upper_bound: u32,
    /// A runner waiting for a job longer than this, with nothing eligible in its scope, is
    /// drained. The one-job runner also exits by itself after its own idle delay.
    pub idle_timeout_ms: u64,
    /// Crashes in a row before a runner is left `failed`.
    pub max_crashes: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    /// After a graceful termination, how long before the process is killed.
    pub kill_grace_ms: u64,
    pub log_capacity: usize,
    pub event_capacity: usize,
}

impl Options {
    pub fn new(service_mode: ServiceMode, ceiling_upper_bound: u32) -> Self {
        Options {
            service_mode,
            ceiling_upper_bound: ceiling_upper_bound.max(1),
            idle_timeout_ms: 60_000,
            max_crashes: 5,
            backoff_base_ms: 1_000,
            backoff_max_ms: 300_000,
            kill_grace_ms: 10_000,
            log_capacity: 200,
            event_capacity: 1_024,
        }
    }
}

#[derive(Debug)]
struct Slot {
    record: RunnerRecord,
    state: RunnerState,
    failure: Option<String>,
    task_ref: Option<String>,
    task_started_ms: Option<u64>,
    idle_since_ms: Option<u64>,
    last_task_at_ms: Option<u64>,
    crashes: u32,
    /// Exits "Nestor unreachable" (75) in a row: they back off without counting as crashes.
    unreachable: u32,
    restart_at_ms: Option<u64>,
    /// Stopped by hand: the supervisor does not launch it again until it is started by hand.
    held: bool,
    /// Found alive after a restart of the service: it did not launch it in this run.
    adopted: bool,
    /// Asked to drain: it will exit after its job, if any.
    draining: bool,
    /// Terminated on purpose (not drained): its task, if any, ends as `stopped`.
    terminating: bool,
    /// Killed if still alive at this time.
    kill_at_ms: Option<u64>,
    /// Reported a job outside its scope.
    scope_violated: bool,
    /// Removed once its process has exited (disassociation, revocation).
    discard: bool,
    usage: (f64, u64),
    logs: VecDeque<LogEntry>,
}

struct Inner {
    stored: StoredState,
    slots: BTreeMap<RunnerId, Slot>,
    regulator: Regulator,
    events: VecDeque<Event>,
    next_seq: u64,
    reconciled: bool,
    config_error: Option<String>,
    load: Option<LoadSample>,
    last_admission: Admission,
    last_queue: Option<Queue>,
    /// Nestor announced `capabilities.scope_filter` (S6): the requested ceiling applies.
    scope_filter: bool,
    /// A runner exited 77 (credential refused): nothing is launched until the connection changes.
    credential_refused: bool,
    /// The last save failed (why); cleared by the next one that succeeds.
    save_error: Option<String>,
}

pub struct Supervisor {
    inner: Mutex<Inner>,
    launcher: Arc<dyn Launcher>,
    store: Arc<dyn Store>,
    clock: Clock,
    options: Options,
    redactor: Redactor,
}

/// Why a launch was refused, in the terms of the local API.
impl From<Denied> for ServiceError {
    fn from(denied: Denied) -> Self {
        match denied {
            Denied::Paused => ServiceError::Unavailable("Tasks are paused.".into()),
            Denied::Blocked(BlockReason::NotAssociated) => ServiceError::NotAssociated,
            Denied::Blocked(BlockReason::Revoked) => ServiceError::Revoked,
            Denied::Blocked(BlockReason::Unreachable) => ServiceError::Unreachable,
            Denied::Blocked(BlockReason::Reconciling) => ServiceError::Unavailable("The service is starting.".into()),
            Denied::AtCapacity { live, ceiling } => {
                ServiceError::Unavailable(format!("{live} of {ceiling} runners already running."))
            }
            Denied::ScopeUnknown | Denied::ScopeViolation => ServiceError::Unavailable(
                "A running task's workspace is not known; no other task starts until it ends.".into(),
            ),
        }
    }
}

impl Supervisor {
    /// Loads and validates the stored state. Nothing is launched until [`Supervisor::reconcile`]
    /// has adopted or cleared the processes the previous service left behind.
    pub fn open(
        store: Arc<dyn Store>,
        launcher: Arc<dyn Launcher>,
        clock: Clock,
        options: Options,
        redactor: Redactor,
    ) -> Self {
        let loaded = store.load();
        let mut config_error = match &loaded {
            Loaded::Corrupt { error, kept_at, .. } => Some((error.clone(), kept_at.clone())),
            _ => None,
        };
        let mut stored = loaded.state().clone();
        if config_error.is_none() {
            if let Err(error) = stored.validate(options.ceiling_upper_bound) {
                // Parsed but out of bounds: same safe fallback as an unreadable file (N1).
                config_error = Some((error, store.set_aside()));
                stored = StoredState::safe_fallback();
            }
        }
        let config_error = config_error.map(|(error, kept_at)| match kept_at {
            Some(path) => format!("{error} (kept as {}); started paused with 1 task at a time", path.display()),
            None => format!("{error}; started paused with 1 task at a time"),
        });
        let mut regulator = Regulator::new(stored.mode, effective_ceiling(stored.max_concurrent_tasks, false));
        regulator.set_blocked(Some(BlockReason::Reconciling));
        let slots = stored
            .runners
            .iter()
            .map(|record| (record.id.clone(), Slot::new(record.clone(), RunnerState::Stopped)))
            .collect();
        let supervisor = Supervisor {
            inner: Mutex::new(Inner {
                stored,
                slots,
                regulator,
                events: VecDeque::new(),
                next_seq: 1,
                reconciled: false,
                config_error: config_error.clone(),
                load: None,
                last_admission: Admission::Blocked(BlockReason::Reconciling),
                last_queue: None,
                scope_filter: false,
                credential_refused: false,
                save_error: None,
            }),
            launcher,
            store,
            clock,
            options,
            redactor,
        };
        if let Some(message) = config_error {
            let mut inner = supervisor.lock();
            supervisor.emit_error(&mut inner, &message);
            // Write the safe configuration so the next start does not trip on the same file.
            supervisor.persist(&mut inner);
        }
        supervisor
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    // MARK: Startup

    /// Adopts the runner processes still alive from before a restart and clears the others.
    /// A pid is adopted only if the process behind it has the recorded start time and
    /// generation; otherwise it is forgotten, never signalled. Never launches a process over an
    /// existing one. An adopted runner lost its control pipe with the old service, so it is
    /// already draining: it finishes its job, if any, and exits.
    pub fn reconcile(&self) {
        let mut inner = self.lock();
        let ids: Vec<RunnerId> = inner.slots.keys().cloned().collect();
        for id in ids {
            let record = inner.slots[&id].record.clone();
            let Some(pid) = record.pid else { continue };
            if self.launcher.is_alive(&record) {
                {
                    let slot = inner.slots.get_mut(&id).unwrap();
                    slot.adopted = true;
                    slot.draining = true;
                }
                // Whether it holds a job is unknown: count it busy, which the ceiling does anyway.
                self.set_state(&mut inner, &id, RunnerState::Busy);
                self.log(
                    &mut inner,
                    &id,
                    LogLevel::Info,
                    format!("Adopted process {pid} after a restart of the service."),
                );
            } else {
                let slot = inner.slots.get_mut(&id).unwrap();
                slot.record.pid = None;
                slot.record.start_time_us = None;
                slot.record.scope = None;
                self.log(
                    &mut inner,
                    &id,
                    LogLevel::Warning,
                    format!("Process {pid} ended while the service was stopped."),
                );
            }
        }
        inner.reconciled = true;
        self.persist(&mut inner);
        self.refresh_admission(&mut inner);
    }

    // MARK: Regulation

    /// Launches runners for the jobs `queue` offers, within the ceiling and the active scope;
    /// drains the runners that are not needed; watches adopted processes and pending kills.
    pub fn tick(&self, queue: &Queue) {
        let mut inner = self.lock();
        if !inner.reconciled {
            return;
        }
        let now = self.now();
        inner.last_queue = Some(queue.clone());

        // Adopted processes are not our children: their exit is noticed by polling.
        let gone: Vec<(RunnerId, u64)> = inner
            .slots
            .values()
            .filter(|s| s.adopted && s.state.is_live() && !self.launcher.is_alive(&s.record))
            .map(|s| (s.record.id.clone(), s.record.generation))
            .collect();
        for (id, generation) in gone {
            self.exited(&mut inner, &id, generation, RunnerExit::Vanished);
        }

        // Terminated processes still alive after the grace period are killed.
        let due: Vec<RunnerId> = inner
            .slots
            .values()
            .filter(|s| s.state.is_live() && s.kill_at_ms.is_some_and(|at| at <= now))
            .map(|s| s.record.id.clone())
            .collect();
        for id in due {
            let record = {
                let slot = inner.slots.get_mut(&id).unwrap();
                slot.kill_at_ms = None;
                slot.record.clone()
            };
            self.log(&mut inner, &id, LogLevel::Warning, "Still running after the grace period: killed.".into());
            self.launcher.terminate(&record, true);
        }

        self.drain_surplus(&mut inner, Some(queue));

        let live = Self::live_scopes(&inner);
        let plan = match inner.regulator.plan(&live, queue) {
            Ok(plan) => plan,
            Err(_) => return,
        };
        for _ in 0..plan.count {
            let candidate = inner
                .slots
                .values()
                .filter(|s| {
                    s.state == RunnerState::Stopped
                        && !s.held
                        && !s.discard
                        && !s.record.manual
                        && s.restart_at_ms.is_none_or(|at| at <= now)
                })
                .map(|s| s.record.id.clone())
                .next();
            let id = match candidate {
                Some(id) => id,
                // A new runner only while the service manages fewer than the ceiling, so a
                // runner waiting out its restart delay is not replaced by a fresh one.
                None if Self::automatic(&inner) < inner.regulator.ceiling() && inner.slots.len() < MAX_RUNNERS => {
                    self.create(&mut inner, None, false)
                }
                None => break,
            };
            self.launch(&mut inner, &id, plan.scope.clone());
        }
    }

    /// Drains the runners without a job that are not wanted any more: all of them while paused
    /// or blocked, those above the ceiling, and those idle too long with nothing eligible in
    /// their scope. Busy runners are never touched.
    fn drain_surplus(&self, inner: &mut Inner, queue: Option<&Queue>) {
        let now = self.now();
        let open = inner.regulator.admission() == Admission::Open;
        let mut idle: Vec<(u64, RunnerId)> = inner
            .slots
            .values()
            .filter(|s| matches!(s.state, RunnerState::Starting | RunnerState::Ready) && !s.draining)
            .map(|s| (s.idle_since_ms.unwrap_or(now), s.record.id.clone()))
            .collect();
        // Newest first: the oldest idle runners are the likeliest to claim soon.
        idle.sort_by(|a, b| b.cmp(a));
        let live = inner.slots.values().filter(|s| s.state.is_live() && !s.draining).count() as u32;
        let excess = live.saturating_sub(inner.regulator.ceiling()) as usize;
        let mut to_drain: Vec<(RunnerId, &'static str)> = Vec::new();
        if !open {
            to_drain.extend(idle.iter().map(|(_, id)| (id.clone(), "paused or not connected")));
        } else {
            to_drain.extend(idle.iter().take(excess).map(|(_, id)| (id.clone(), "above the tasks-at-a-time setting")));
            for (since, id) in idle.iter().skip(excess) {
                let nothing_eligible = queue.is_some_and(|q| match &inner.slots[id].record.scope {
                    Some(scope) => q.in_scope(scope) == 0,
                    None => q.total() == 0,
                });
                if nothing_eligible && now.saturating_sub(*since) >= self.options.idle_timeout_ms {
                    to_drain.push((id.clone(), "idle and not needed"));
                }
            }
        }
        for (id, why) in to_drain {
            self.log(inner, &id, LogLevel::Info, format!("Draining: {why}. It will not claim a task."));
            self.drain(inner, &id);
        }
    }

    // MARK: Runner reports

    /// The runner is up and waiting for a job.
    pub fn runner_ready(&self, runner_id: &str, generation: u64) -> Result<(), ServiceError> {
        let mut inner = self.lock();
        let slot = Self::current(&inner, runner_id, generation)?;
        if slot.state != RunnerState::Starting {
            return Err(ServiceError::InvalidTransition { from: slot.state, action: "mark ready".into() });
        }
        let now = self.now();
        inner.slots.get_mut(runner_id).unwrap().idle_since_ms = Some(now);
        self.set_state(&mut inner, runner_id, RunnerState::Ready);
        Ok(())
    }

    /// The runner claimed its one job. A second claim is refused and logged: a one-job runner
    /// never claims twice. A job reported outside the runner's scope is a violation (B1(b)): it
    /// is reported and nothing more is launched until that runner ends. Contract v1 reports no
    /// scope (`scope` is `None`): only Nestor can tell, at claim time (S6).
    pub fn runner_claimed(
        &self,
        runner_id: &str,
        generation: u64,
        task_ref: &str,
        scope: Option<&str>,
    ) -> Result<(), ServiceError> {
        let mut inner = self.lock();
        let slot = Self::current(&inner, runner_id, generation)?;
        if !matches!(slot.state, RunnerState::Starting | RunnerState::Ready) || slot.task_ref.is_some() {
            let from = slot.state;
            self.log(&mut inner, runner_id, LogLevel::Error, "Reported a second claim; a runner takes one job.".into());
            return Err(ServiceError::InvalidTransition { from, action: "claim a second task on".into() });
        }
        let task_ref: String = task_ref.chars().filter(|c| c.is_ascii_graphic()).take(128).collect();
        if let Some(scope) = scope {
            validate_scope(scope)?;
        }
        let now = self.now();
        let violated = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            slot.task_ref = Some(task_ref.clone());
            slot.task_started_ms = Some(now);
            slot.last_task_at_ms = Some(now);
            slot.idle_since_ms = None;
            match (&slot.record.scope, scope) {
                (Some(pinned), Some(reported)) if pinned != reported => true,
                (Some(_), None) => false,
                (None, Some(reported)) => {
                    slot.record.scope = Some(reported.to_string());
                    false
                }
                _ => false,
            }
        };
        self.set_state(&mut inner, runner_id, RunnerState::Busy);
        self.log(&mut inner, runner_id, LogLevel::Info, format!("Task {task_ref} started."));
        self.emit(&mut inner, EventKind::TaskStarted { runner_id: runner_id.into(), task_ref });
        if violated {
            inner.slots.get_mut(runner_id).unwrap().scope_violated = true;
            self.log(
                &mut inner,
                runner_id,
                LogLevel::Error,
                "Nestor gave this runner a task outside its workspace scope. No other task starts until it ends."
                    .into(),
            );
            self.emit(&mut inner, EventKind::ScopeViolation { runner_id: runner_id.into() });
        }
        self.persist(&mut inner);
        Ok(())
    }

    /// The runner broke the launch contract (a `hello` that is not the one it was launched
    /// with): it is terminated like a forced stop, and not launched again until started by hand.
    pub fn runner_misbehaved(&self, runner_id: &str, generation: u64, reason: &str) {
        let mut inner = self.lock();
        let Ok(slot) = Self::current(&inner, runner_id, generation) else { return };
        if !slot.state.is_live() || slot.terminating {
            return;
        }
        let record = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            slot.held = true;
            slot.terminating = true;
            slot.kill_at_ms = Some(self.now() + self.options.kill_grace_ms);
            slot.failure = Some(capitalized(reason));
            slot.record.clone()
        };
        self.log(&mut inner, runner_id, LogLevel::Error, format!("{}: terminated.", capitalized(reason)));
        self.set_state(&mut inner, runner_id, RunnerState::Stopping);
        self.launcher.terminate(&record, false);
    }

    /// Processes a runner left behind could not be stopped after the sweep (KCA-734 §4): the
    /// runner is left `failed` (never a silent success) and the machine raises an error.
    pub fn sweep_failed(&self, runner_id: &str, generation: u64, remaining: usize) {
        let mut inner = self.lock();
        let message = format!("{remaining} process(es) started by runner {runner_id} could not be stopped.");
        self.emit_error(&mut inner, &message);
        let current = Self::current(&inner, runner_id, generation).is_ok_and(|s| !s.state.is_live());
        if current {
            {
                let slot = inner.slots.get_mut(runner_id).unwrap();
                slot.failure = Some(message.clone());
                slot.held = true;
            }
            self.log(&mut inner, runner_id, LogLevel::Error, message);
            self.set_state(&mut inner, runner_id, RunnerState::Failed);
            self.persist(&mut inner);
        }
    }

    /// The runner's process ended. A stale generation is ignored.
    pub fn runner_exited(&self, runner_id: &str, generation: u64, exit: RunnerExit) {
        let mut inner = self.lock();
        self.exited(&mut inner, runner_id, generation, exit);
    }

    fn exited(&self, inner: &mut Inner, runner_id: &str, generation: u64, exit: RunnerExit) {
        if Self::current(inner, runner_id, generation).is_err() {
            return;
        }
        let (state, task_ref, terminated, discard) = {
            let slot = &inner.slots[runner_id];
            (slot.state, slot.task_ref.clone(), slot.terminating || slot.discard, slot.discard)
        };
        if !state.is_live() {
            return;
        }
        if task_ref.is_some() || state == RunnerState::Busy {
            let outcome = match &exit {
                _ if terminated => TaskOutcome::Stopped,
                RunnerExit::Finished(outcome) => *outcome,
                RunnerExit::Crashed(_) | RunnerExit::Usage | RunnerExit::Unreachable | RunnerExit::Revoked => {
                    TaskOutcome::RunnerLost
                }
                RunnerExit::Idle | RunnerExit::Vanished => TaskOutcome::Unknown,
            };
            if let Some(task) = &task_ref {
                let level = if outcome == TaskOutcome::Succeeded { LogLevel::Info } else { LogLevel::Warning };
                self.log(inner, runner_id, level, format!("Task {task} ended: {}.", outcome_text(outcome)));
            }
            self.emit(inner, EventKind::TaskFinished { runner_id: runner_id.into(), task_ref, outcome });
        }
        let was_draining = inner.slots[runner_id].draining;
        {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            slot.record.pid = None;
            slot.record.start_time_us = None;
            slot.record.scope = None;
            slot.adopted = false;
            slot.draining = false;
            slot.terminating = false;
            slot.kill_at_ms = None;
            slot.scope_violated = false;
            slot.task_ref = None;
            slot.task_started_ms = None;
            slot.idle_since_ms = None;
            slot.usage = (0.0, 0);
        }
        if discard {
            inner.slots.remove(runner_id);
            self.launcher.forget(runner_id);
        } else {
            match exit {
                RunnerExit::Crashed(reason) if state != RunnerState::Stopping && !was_draining && !terminated => {
                    self.crashed(inner, runner_id, &format!("exited unexpectedly: {reason}"));
                }
                RunnerExit::Crashed(reason) => {
                    self.log(inner, runner_id, LogLevel::Warning, format!("Stopped: {reason}."));
                    self.set_state(inner, runner_id, RunnerState::Stopped);
                }
                RunnerExit::Finished(_) | RunnerExit::Idle | RunnerExit::Vanished => {
                    let slot = inner.slots.get_mut(runner_id).unwrap();
                    slot.crashes = 0;
                    slot.unreachable = 0;
                    self.log(inner, runner_id, LogLevel::Info, "Exited.".into());
                    self.set_state(inner, runner_id, RunnerState::Stopped);
                }
                RunnerExit::Usage => {
                    let reason = "The runner refused its launch (usage error, exit 64); not launched again.";
                    self.fail(inner, runner_id, reason);
                }
                RunnerExit::Revoked => {
                    let reason = "Nestor refused the runner's credential (exit 77): the machine may be revoked. \
                                  No runner is launched until the connection changes.";
                    self.fail(inner, runner_id, reason);
                    inner.credential_refused = true;
                    self.emit_error(inner, reason);
                }
                RunnerExit::Unreachable => {
                    let now = self.now();
                    let delay = {
                        let slot = inner.slots.get_mut(runner_id).unwrap();
                        slot.unreachable += 1;
                        let delay = self.backoff(slot.unreachable);
                        slot.restart_at_ms = Some(now + delay);
                        delay
                    };
                    self.log(
                        inner,
                        runner_id,
                        LogLevel::Warning,
                        format!("Nestor could not be reached (exit 75); next launch in {} s.", delay / 1_000),
                    );
                    self.set_state(inner, runner_id, RunnerState::Stopped);
                }
            }
        }
        self.persist(inner);
        self.refresh_admission(inner);
    }

    // MARK: Configuration

    /// A setting that cannot be saved is never acknowledged as a success (KCA-732 nº1): pausing
    /// still applies at once (the safe direction) but answers an error saying it would not
    /// survive a restart; resuming is not applied at all.
    pub fn set_mode(&self, mode: Mode, expected_revision: Option<u64>) -> Result<MachineStatus, ServiceError> {
        let mut inner = self.lock();
        Self::check_revision(&inner, expected_revision)?;
        let previous = inner.stored.mode;
        inner.stored.mode = mode;
        inner.regulator.set_mode(mode);
        let safe = mode == Mode::Paused;
        if let Err(error) = self.config_changed(&mut inner, safe) {
            if !safe {
                inner.stored.mode = previous;
                inner.regulator.set_mode(previous);
            }
            return Err(not_saved(&error, safe));
        }
        Ok(self.status_locked(&inner))
    }

    pub fn set_max_concurrent_tasks(
        &self,
        value: u32,
        expected_revision: Option<u64>,
    ) -> Result<MachineStatus, ServiceError> {
        let mut inner = self.lock();
        Self::check_revision(&inner, expected_revision)?;
        if value < 1 || value > self.options.ceiling_upper_bound {
            return Err(ServiceError::LimitOutOfRange { min: 1, max: self.options.ceiling_upper_bound });
        }
        let previous = inner.stored.max_concurrent_tasks;
        inner.stored.max_concurrent_tasks = value;
        let effective = effective_ceiling(value, inner.scope_filter);
        inner.regulator.set_ceiling(effective);
        // Lowering is the safe direction: applied even if it cannot be saved.
        let safe = value <= previous;
        if let Err(error) = self.config_changed(&mut inner, safe) {
            if !safe {
                inner.stored.max_concurrent_tasks = previous;
                let effective = effective_ceiling(previous, inner.scope_filter);
                inner.regulator.set_ceiling(effective);
            }
            return Err(not_saved(&error, safe));
        }
        Ok(self.status_locked(&inner))
    }

    pub fn max_concurrent_tasks(&self) -> u32 {
        self.lock().stored.max_concurrent_tasks
    }

    /// What Nestor announced it enforces. Without `scope_filter` (S6) the ceiling applied is 1:
    /// several tasks side by side need the server to keep workspaces apart at claim time.
    pub fn set_scope_filter(&self, scope_filter: bool) {
        let mut inner = self.lock();
        if inner.scope_filter == scope_filter {
            return;
        }
        inner.scope_filter = scope_filter;
        let effective = effective_ceiling(inner.stored.max_concurrent_tasks, scope_filter);
        inner.regulator.set_ceiling(effective);
        self.refresh_admission(&mut inner);
        self.drain_surplus(&mut inner, None);
    }

    pub fn set_connection(&self, connection: Connection) {
        let mut inner = self.lock();
        self.set_connection_locked(&mut inner, connection);
    }

    fn set_connection_locked(&self, inner: &mut Inner, connection: Connection) {
        if inner.stored.connection == connection {
            return;
        }
        inner.stored.connection = connection.clone();
        inner.credential_refused = false;
        self.emit(inner, EventKind::ConnectionChanged { connection });
        self.persist(inner);
        self.refresh_admission(inner);
        self.drain_surplus(inner, None);
    }

    pub fn set_machine_id(&self, machine_id: Option<String>) {
        let mut inner = self.lock();
        inner.stored.machine_id = machine_id;
        self.persist(&mut inner);
    }

    pub fn machine_id(&self) -> Option<String> {
        self.lock().stored.machine_id.clone()
    }

    pub fn set_pending_revocation(&self, request: Option<crate::persist::RevocationRequest>) {
        let mut inner = self.lock();
        inner.stored.pending_revocation = request;
        self.persist(&mut inner);
    }

    pub fn pending_revocation(&self) -> Option<crate::persist::RevocationRequest> {
        self.lock().stored.pending_revocation.clone()
    }

    /// Disassociation or revocation (B7, an exception to "never interrupt"): no more launches,
    /// every live runner is terminated (graceful signal, then killed after the grace period),
    /// and every runner record is erased with its local files once its process is gone.
    pub fn end_association(&self, connection: Connection) {
        let mut inner = self.lock();
        self.set_connection_locked(&mut inner, connection);
        inner.stored.machine_id = None;
        let ids: Vec<RunnerId> = inner.slots.keys().cloned().collect();
        let now = self.now();
        for id in ids {
            let live = inner.slots[&id].state.is_live();
            if live {
                let record = {
                    let slot = inner.slots.get_mut(&id).unwrap();
                    slot.discard = true;
                    slot.terminating = true;
                    slot.kill_at_ms = Some(now + self.options.kill_grace_ms);
                    slot.record.clone()
                };
                self.log(&mut inner, &id, LogLevel::Warning, "Stopping: the machine is no longer associated.".into());
                self.set_state(&mut inner, &id, RunnerState::Stopping);
                self.launcher.terminate(&record, false);
            } else {
                inner.slots.remove(&id);
                self.launcher.forget(&id);
            }
        }
        self.persist(&mut inner);
    }

    pub fn set_load(&self, sample: LoadSample) {
        let mut inner = self.lock();
        inner.load = Some(sample.clone());
        self.emit(&mut inner, EventKind::LoadSample { sample });
    }

    pub fn set_runner_usage(&self, runner_id: &str, cpu_percent: f64, memory_bytes: u64) {
        if let Some(slot) = self.lock().slots.get_mut(runner_id) {
            slot.usage = (cpu_percent, memory_bytes);
        }
    }

    /// Records a line a runner wrote, masked first.
    pub fn runner_output(&self, runner_id: &str, generation: u64, line: &str) {
        let mut inner = self.lock();
        if Self::current(&inner, runner_id, generation).is_ok() {
            let line: String = line.chars().take(2_000).collect();
            self.log(&mut inner, runner_id, LogLevel::Info, line);
        }
    }

    /// Records a service-level error, masked.
    pub fn report_error(&self, message: &str) {
        let mut inner = self.lock();
        self.emit_error(&mut inner, message);
    }

    // MARK: Diagnostics (not the main path)

    pub fn add_runner(&self, name: &str, start_now: bool) -> Result<RunnerView, ServiceError> {
        let mut inner = self.lock();
        if !matches!(inner.stored.connection, Connection::Associated { .. }) {
            return Err(ServiceError::NotAssociated);
        }
        let name = validate_runner_name(name)?;
        if inner.slots.values().any(|s| s.record.name.eq_ignore_ascii_case(&name)) {
            return Err(ServiceError::DuplicateName(name));
        }
        if inner.slots.len() >= MAX_RUNNERS {
            return Err(ServiceError::InvalidArgument {
                field: "name".into(),
                reason: format!("at most {MAX_RUNNERS} runners"),
            });
        }
        let id = self.create(&mut inner, Some(name), true);
        if start_now {
            self.start_locked(&mut inner, &id)?;
        }
        Ok(self.view(&inner.slots[&id]))
    }

    /// Starts a runner by hand. Same regulation as automatic launches: never past the ceiling,
    /// never while paused or blocked, never in another scope than the live runners.
    pub fn start_runner(&self, runner_id: &str) -> Result<RunnerView, ServiceError> {
        let mut inner = self.lock();
        self.start_locked(&mut inner, runner_id)?;
        Ok(self.view(&inner.slots[runner_id]))
    }

    fn start_locked(&self, inner: &mut Inner, runner_id: &str) -> Result<(), ServiceError> {
        let slot = inner.slots.get(runner_id).ok_or(ServiceError::RunnerNotFound)?;
        if !matches!(slot.state, RunnerState::Stopped | RunnerState::Failed) || slot.discard {
            return Err(ServiceError::InvalidTransition { from: slot.state, action: "start".into() });
        }
        let live = Self::live_scopes(inner);
        let scope = inner.regulator.may_launch_one(&live, inner.last_queue.as_ref())?;
        let slot = inner.slots.get_mut(runner_id).unwrap();
        slot.held = false;
        slot.crashes = 0;
        slot.failure = None;
        slot.restart_at_ms = None;
        self.launch(inner, runner_id, scope);
        Ok(())
    }

    /// Stops a runner by hand. Without a job it is drained. A busy runner is only stopped with
    /// `force` (the interface asks for a confirmation first): it is terminated, which ends its
    /// task; Nestor's existing policy decides what happens to it.
    pub fn stop_runner(&self, runner_id: &str, force: bool) -> Result<RunnerView, ServiceError> {
        let mut inner = self.lock();
        let slot = inner.slots.get_mut(runner_id).ok_or(ServiceError::RunnerNotFound)?;
        match slot.state {
            RunnerState::Starting | RunnerState::Ready => {
                slot.held = true;
                self.log(&mut inner, runner_id, LogLevel::Info, "Stop requested from the diagnostics.".into());
                self.drain(&mut inner, runner_id);
            }
            RunnerState::Busy | RunnerState::Stopping if force => {
                slot.held = true;
                slot.terminating = true;
                slot.kill_at_ms = Some(self.now() + self.options.kill_grace_ms);
                let record = slot.record.clone();
                self.log(&mut inner, runner_id, LogLevel::Warning, "Forced stop from the diagnostics.".into());
                self.set_state(&mut inner, runner_id, RunnerState::Stopping);
                self.launcher.terminate(&record, false);
            }
            from => return Err(ServiceError::InvalidTransition { from, action: "stop".into() }),
        }
        Ok(self.view(&inner.slots[runner_id]))
    }

    pub fn remove_runner(&self, runner_id: &str) -> Result<(), ServiceError> {
        let mut inner = self.lock();
        let slot = inner.slots.get(runner_id).ok_or(ServiceError::RunnerNotFound)?;
        if !matches!(slot.state, RunnerState::Stopped | RunnerState::Failed) {
            return Err(ServiceError::InvalidTransition { from: slot.state, action: "remove".into() });
        }
        inner.slots.remove(runner_id);
        self.launcher.forget(runner_id);
        self.persist(&mut inner);
        Ok(())
    }

    // MARK: Reads

    pub fn status(&self) -> MachineStatus {
        self.status_locked(&self.lock())
    }

    pub fn runners(&self) -> Vec<RunnerView> {
        let inner = self.lock();
        inner.slots.values().map(|s| self.view(s)).collect()
    }

    pub fn runner(&self, runner_id: &str) -> Option<RunnerView> {
        let inner = self.lock();
        inner.slots.get(runner_id).map(|s| self.view(s))
    }

    /// Process ids of the live runners, to refuse them on the local API.
    pub fn runner_pids(&self) -> Vec<u32> {
        let inner = self.lock();
        inner.slots.values().filter(|s| s.state.is_live()).filter_map(|s| s.record.pid).collect()
    }

    /// The records of the live runners (for the launcher's shutdown).
    pub fn live_records(&self) -> Vec<RunnerRecord> {
        let inner = self.lock();
        inner.slots.values().filter(|s| s.state.is_live()).map(|s| s.record.clone()).collect()
    }

    pub fn logs(&self, runner_id: &str, limit: usize) -> Result<Vec<LogEntry>, ServiceError> {
        let inner = self.lock();
        let slot = inner.slots.get(runner_id).ok_or(ServiceError::RunnerNotFound)?;
        let skip = slot.logs.len().saturating_sub(limit);
        Ok(slot.logs.iter().skip(skip).cloned().collect())
    }

    /// Events after `since`. If some were dropped meanwhile, the client must reload a snapshot.
    pub fn events_since(&self, since: u64) -> Result<Vec<Event>, ServiceError> {
        let inner = self.lock();
        if let Some(first) = inner.events.front() {
            if since + 1 < first.seq {
                return Err(ServiceError::SnapshotRequired);
            }
        }
        Ok(inner.events.iter().filter(|e| e.seq > since).cloned().collect())
    }

    pub fn connection(&self) -> Connection {
        self.lock().stored.connection.clone()
    }

    // MARK: Internals

    fn current<'a>(inner: &'a Inner, runner_id: &str, generation: u64) -> Result<&'a Slot, ServiceError> {
        match inner.slots.get(runner_id) {
            Some(slot) if slot.record.generation == generation => Ok(slot),
            Some(_) => {
                Err(ServiceError::InvalidArgument { field: "generation".into(), reason: "stale generation".into() })
            }
            None => Err(ServiceError::RunnerNotFound),
        }
    }

    fn live_scopes(inner: &Inner) -> Vec<LiveScope> {
        inner
            .slots
            .values()
            .filter(|s| s.state.is_live())
            .map(|s| {
                if s.scope_violated {
                    LiveScope::Violated
                } else {
                    match &s.record.scope {
                        Some(scope) => LiveScope::Pinned(scope.clone()),
                        None => LiveScope::Unknown,
                    }
                }
            })
            .collect()
    }

    fn automatic(inner: &Inner) -> u32 {
        inner.slots.values().filter(|s| !s.record.manual).count() as u32
    }

    fn check_revision(inner: &Inner, expected: Option<u64>) -> Result<(), ServiceError> {
        match expected {
            Some(revision) if revision != inner.stored.config_revision => {
                Err(ServiceError::Conflict { current_revision: inner.stored.config_revision })
            }
            _ => Ok(()),
        }
    }

    /// Saves a settings change first. If it cannot be saved and is not `safe`, nothing is
    /// applied (the caller restores its fields); a `safe` change is applied in memory anyway.
    fn config_changed(&self, inner: &mut Inner, safe: bool) -> Result<(), String> {
        inner.stored.config_revision += 1;
        let saved = self.try_persist(inner);
        if saved.is_err() && !safe {
            inner.stored.config_revision -= 1;
            return saved;
        }
        let (revision, mode, max) =
            (inner.stored.config_revision, inner.stored.mode, inner.stored.max_concurrent_tasks);
        self.emit(inner, EventKind::ConfigChanged { revision, mode, max_concurrent_tasks: max });
        self.refresh_admission(inner);
        self.drain_surplus(inner, None);
        saved
    }

    fn create(&self, inner: &mut Inner, name: Option<String>, manual: bool) -> RunnerId {
        let id = new_id();
        let name = name.unwrap_or_else(|| {
            (1..)
                .map(|n| format!("runner-{n}"))
                .find(|candidate| !inner.slots.values().any(|s| &s.record.name == candidate))
                .unwrap()
        });
        let record = RunnerRecord {
            id: id.clone(),
            name,
            generation: 0,
            pid: None,
            start_time_us: None,
            scope: None,
            created_at_ms: self.now(),
            manual,
        };
        let mut slot = Slot::new(record, RunnerState::Stopped);
        slot.held = manual;
        inner.slots.insert(id.clone(), slot);
        self.log(
            inner,
            &id,
            LogLevel::Info,
            if manual { "Added from the diagnostics.".into() } else { "Created by the service.".into() },
        );
        self.persist(inner);
        id
    }

    fn launch(&self, inner: &mut Inner, runner_id: &str, scope: Option<ScopeId>) {
        let record = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            debug_assert!(!slot.state.is_live(), "never launch over a live runner");
            slot.record.generation += 1;
            slot.record.scope = scope;
            slot.restart_at_ms = None;
            slot.draining = false;
            slot.idle_since_ms = Some(self.now());
            slot.record.clone()
        };
        self.set_state(inner, runner_id, RunnerState::Starting);
        let target = match (&inner.stored.machine_id, &inner.stored.connection) {
            (Some(machine_id), Connection::Associated { instance, .. }) => {
                Ok(LaunchTarget { machine_id: machine_id.clone(), instance: instance.clone() })
            }
            _ => Err("the machine is not associated".to_string()),
        };
        match target.and_then(|target| self.launcher.spawn(&record, &target)) {
            Ok(identity) => {
                {
                    let slot = inner.slots.get_mut(runner_id).unwrap();
                    slot.record.pid = Some(identity.pid);
                    slot.record.start_time_us = Some(identity.start_time_us);
                }
                self.log(
                    inner,
                    runner_id,
                    LogLevel::Info,
                    format!("Launched for one task (generation {}).", record.generation),
                );
                self.persist(inner);
            }
            Err(reason) => {
                inner.slots.get_mut(runner_id).unwrap().record.scope = None;
                self.crashed(inner, runner_id, &format!("could not start: {reason}"));
            }
        }
    }

    fn drain(&self, inner: &mut Inner, runner_id: &str) {
        let record = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            slot.draining = true;
            slot.record.clone()
        };
        if record.pid.is_some() {
            self.set_state(inner, runner_id, RunnerState::Stopping);
            self.launcher.drain(&record);
        } else {
            inner.slots.get_mut(runner_id).unwrap().draining = false;
            self.set_state(inner, runner_id, RunnerState::Stopped);
            self.persist(inner);
        }
    }

    /// An exit nobody asked for: may launch again after an exponential backoff, or the runner
    /// is left `failed` after `max_crashes` in a row.
    fn crashed(&self, inner: &mut Inner, runner_id: &str, reason: &str) {
        let now = self.now();
        let options = self.options.clone();
        let next = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            slot.record.pid = None;
            slot.record.start_time_us = None;
            slot.crashes += 1;
            if slot.crashes >= options.max_crashes {
                slot.failure = Some(format!("Exited {} times in a row; last: {reason}", slot.crashes));
                RunnerState::Failed
            } else {
                slot.restart_at_ms = Some(now + self.backoff(slot.crashes));
                RunnerState::Stopped
            }
        };
        self.log(inner, runner_id, LogLevel::Error, capitalized(reason));
        self.set_state(inner, runner_id, next);
        self.persist(inner);
    }

    /// Exponential delay after `attempts` failed launches in a row (1 or more).
    fn backoff(&self, attempts: u32) -> u64 {
        let options = &self.options;
        options.backoff_base_ms.saturating_mul(1 << attempts.saturating_sub(1).min(20)).min(options.backoff_max_ms)
    }

    /// Left `failed` for a reason that relaunching would not fix.
    fn fail(&self, inner: &mut Inner, runner_id: &str, reason: &str) {
        inner.slots.get_mut(runner_id).unwrap().failure = Some(reason.to_string());
        self.log(inner, runner_id, LogLevel::Error, reason.to_string());
        self.set_state(inner, runner_id, RunnerState::Failed);
    }

    fn set_state(&self, inner: &mut Inner, runner_id: &str, state: RunnerState) {
        let generation = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            if slot.state == state {
                return;
            }
            slot.state = state;
            if matches!(state, RunnerState::Starting | RunnerState::Ready | RunnerState::Busy) {
                slot.failure = None;
            }
            slot.record.generation
        };
        self.emit(inner, EventKind::RunnerStateChanged { runner_id: runner_id.into(), state, generation });
    }

    fn refresh_admission(&self, inner: &mut Inner) {
        let blocked = if !inner.reconciled {
            Some(BlockReason::Reconciling)
        } else {
            match inner.stored.connection {
                Connection::NotAssociated | Connection::Associating { .. } => Some(BlockReason::NotAssociated),
                Connection::Unreachable { .. } => Some(BlockReason::Unreachable),
                Connection::Revoked => Some(BlockReason::Revoked),
                Connection::Associated { .. } if inner.credential_refused => Some(BlockReason::Revoked),
                Connection::Associated { .. } => None,
            }
        };
        inner.regulator.set_blocked(blocked);
        let admission = inner.regulator.admission();
        if admission != inner.last_admission {
            inner.last_admission = admission;
            self.emit(inner, EventKind::AdmissionChanged { admission });
        }
    }

    fn persist(&self, inner: &mut Inner) {
        let _ = self.try_persist(inner);
    }

    /// Writes the state. A failure is kept visible in the status (`configError`) until a later
    /// save succeeds.
    fn try_persist(&self, inner: &mut Inner) -> Result<(), String> {
        inner.stored.runners = inner.slots.values().map(|s| s.record.clone()).collect();
        let mut stored = inner.stored.clone();
        // An association in progress is never stored: its device code lives in memory only.
        if matches!(stored.connection, Connection::Associating { .. }) {
            stored.connection = Connection::NotAssociated;
        }
        match self.store.save(&stored) {
            Ok(()) => {
                inner.save_error = None;
                Ok(())
            }
            Err(error) => {
                let error = error.to_string();
                if inner.save_error.is_none() {
                    self.emit_error(inner, &format!("Could not save the configuration: {error}"));
                }
                inner.save_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn emit_error(&self, inner: &mut Inner, message: &str) {
        let message = self.redactor.mask(message);
        self.emit(inner, EventKind::ServiceError { message });
    }

    fn emit(&self, inner: &mut Inner, kind: EventKind) {
        let event = Event { seq: inner.next_seq, at_ms: self.now(), kind };
        inner.next_seq += 1;
        inner.events.push_back(event);
        while inner.events.len() > self.options.event_capacity {
            inner.events.pop_front();
        }
    }

    /// Every runner log line is masked here, at the source (N5).
    fn log(&self, inner: &mut Inner, runner_id: &str, level: LogLevel, message: String) {
        let message = self.redactor.mask(&message);
        let message = match self.options.service_mode {
            ServiceMode::Demo => format!("[demo] {message}"),
            ServiceMode::Live => message,
        };
        let at_ms = self.now();
        if let Some(slot) = inner.slots.get_mut(runner_id) {
            slot.logs.push_back(LogEntry { at_ms, level, message });
            while slot.logs.len() > self.options.log_capacity {
                slot.logs.pop_front();
            }
        }
    }

    fn view(&self, slot: &Slot) -> RunnerView {
        RunnerView {
            id: slot.record.id.clone(),
            name: slot.record.name.clone(),
            state: slot.state,
            generation: slot.record.generation,
            failure_reason: slot.failure.clone(),
            task_ref: slot.task_ref.clone(),
            scope: slot.record.scope.clone(),
            created_at_ms: slot.record.created_at_ms,
            last_task_at_ms: slot.last_task_at_ms,
            measured_cpu_percent: slot.usage.0,
            measured_memory_bytes: slot.usage.1,
        }
    }

    fn status_locked(&self, inner: &Inner) -> MachineStatus {
        let active_tasks = inner
            .slots
            .values()
            .filter_map(|s| {
                Some(ActiveTask {
                    runner_id: s.record.id.clone(),
                    runner_name: s.record.name.clone(),
                    task_ref: s.task_ref.clone()?,
                    started_at_ms: s.task_started_ms?,
                })
            })
            .collect();
        MachineStatus {
            service_mode: self.options.service_mode,
            connection: inner.stored.connection.clone(),
            mode: inner.stored.mode,
            admission: inner.regulator.admission(),
            max_concurrent_tasks: inner.stored.max_concurrent_tasks,
            effective_max_concurrent_tasks: inner.regulator.ceiling(),
            capped_by_server: inner.regulator.ceiling() < inner.stored.max_concurrent_tasks,
            max_concurrent_tasks_upper_bound: self.options.ceiling_upper_bound,
            config_revision: inner.stored.config_revision,
            busy_runners: inner.slots.values().filter(|s| s.state == RunnerState::Busy).count() as u32,
            live_runners: inner.slots.values().filter(|s| s.state.is_live()).count() as u32,
            active_scope: Regulator::active_scope(&Self::live_scopes(inner)).ok().flatten(),
            active_tasks,
            load: inner.load.clone(),
            config_error: match &inner.save_error {
                Some(error) => Some(self.redactor.mask(&format!(
                    "The settings could not be saved ({error}); changes since then will be lost if the service restarts."
                ))),
                None => inner.config_error.clone(),
            },
        }
    }
}

impl Slot {
    fn new(record: RunnerRecord, state: RunnerState) -> Self {
        Slot {
            held: record.manual,
            adopted: false,
            draining: false,
            terminating: false,
            kill_at_ms: None,
            scope_violated: false,
            discard: false,
            record,
            state,
            failure: None,
            task_ref: None,
            task_started_ms: None,
            idle_since_ms: None,
            last_task_at_ms: None,
            crashes: 0,
            unreachable: 0,
            restart_at_ms: None,
            usage: (0.0, 0),
            logs: VecDeque::new(),
        }
    }
}

fn not_saved(error: &str, applied: bool) -> ServiceError {
    ServiceError::Unavailable(if applied {
        format!(
            "Applied for now, but the setting could not be saved ({error}): it will be lost if the service restarts."
        )
    } else {
        format!("The setting could not be saved ({error}); nothing changed.")
    })
}

/// The ceiling actually applied: the requested one only once Nestor filters scopes at claim
/// time (`capabilities.scope_filter`, S6); 1 until then (KCA-734 §3).
pub fn effective_ceiling(requested: u32, scope_filter: bool) -> u32 {
    if scope_filter {
        requested.max(1)
    } else {
        1
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn outcome_text(outcome: TaskOutcome) -> &'static str {
    match outcome {
        TaskOutcome::Succeeded => "succeeded",
        TaskOutcome::Failed => "failed",
        TaskOutcome::RunnerLost => "runner lost",
        TaskOutcome::Stopped => "stopped",
        TaskOutcome::Unknown => "outcome unknown",
        TaskOutcome::LeaseLost => "lease lost",
        TaskOutcome::Released => "given back to Nestor",
    }
}

/// A random identifier for a runner (128 bits from the system's random source).
pub fn new_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
