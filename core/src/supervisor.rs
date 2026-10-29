//! The supervisor: the single local authority over this machine's runners.
//!
//! It keeps the runners' state machines, sizes the pool to the admission ceiling and the work
//! available, admits tasks through the [`Regulator`], persists its configuration and records
//! events. Every operation runs under one lock, so two concurrent calls can never launch the same
//! runner twice or admit past the ceiling. Starting a process and admitting a task are separate:
//! a runner that is up still needs a token before it may claim a task from Nestor.
//!
//! Pausing and lowering the ceiling never stop a busy runner. Idle runners above the target are
//! stopped after `idle_timeout_ms`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::admission::{Denied, Regulator, Token};
use crate::model::*;
use crate::persist::{Loaded, RunnerRecord, Store, StoredState};

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Starts and stops runner processes. The fake launcher of the tests and the demo implements it
/// without processes; the real one will wrap the existing Nestor runner.
pub trait Launcher: Send + Sync {
    /// Obtains an identity for a new runner (in the real version, a credential issued by Nestor
    /// for this runner only).
    fn provision(&self, runner: &RunnerRecord) -> Result<(), ServiceError>;
    /// Launches the process of `runner.generation` and returns its pid.
    fn spawn(&self, runner: &RunnerRecord) -> Result<u32, String>;
    /// Asks the process to finish gracefully. The supervisor learns that it ended through
    /// [`Supervisor::runner_exited`].
    fn terminate(&self, runner_id: &str, generation: u64, pid: u32);
    /// Whether `pid` is still the process of this runner and generation.
    fn is_alive(&self, runner_id: &str, generation: u64, pid: u32) -> bool;
    /// Forgets the runner's identity (revocation in Nestor in the real version).
    fn deprovision(&self, runner_id: &str);
}

#[derive(Debug, Clone)]
pub struct Options {
    pub service_mode: ServiceMode,
    /// Highest ceiling accepted; a sanity check on the input (the number of cores by default).
    pub ceiling_upper_bound: u32,
    pub idle_timeout_ms: u64,
    /// Crashes in a row before a runner is left `failed`.
    pub max_crashes: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
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
    restart_at_ms: Option<u64>,
    /// Stopped by hand: the supervisor does not start it again until it is started by hand.
    held: bool,
    /// Found running after a restart of the service: busy until it reports idle.
    adopted: bool,
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
}

pub struct Supervisor {
    inner: Mutex<Inner>,
    launcher: Arc<dyn Launcher>,
    store: Arc<dyn Store>,
    clock: Clock,
    options: Options,
}

/// Why `admit` refused, in the terms of the local API.
impl From<Denied> for ServiceError {
    fn from(denied: Denied) -> Self {
        match denied {
            Denied::Paused => ServiceError::Unavailable("Admissions are paused.".into()),
            Denied::Blocked(BlockReason::NotAssociated) => ServiceError::NotAssociated,
            Denied::Blocked(BlockReason::Revoked) => ServiceError::Revoked,
            Denied::Blocked(BlockReason::Unreachable) => ServiceError::Unreachable,
            Denied::Blocked(BlockReason::Reconciling) => ServiceError::Unavailable("The service is starting.".into()),
            Denied::AtCapacity { busy, ceiling } => {
                ServiceError::Unavailable(format!("{busy} of {ceiling} tasks already running."))
            }
            Denied::AlreadyHolding => {
                ServiceError::InvalidTransition { from: RunnerState::Busy, action: "admit".into() }
            }
        }
    }
}

impl Supervisor {
    /// Loads the stored state. Nothing is admitted until [`Supervisor::reconcile`] has adopted or
    /// cleared the processes the previous service left behind.
    pub fn open(store: Arc<dyn Store>, launcher: Arc<dyn Launcher>, clock: Clock, options: Options) -> Self {
        let loaded = store.load();
        let config_error = match &loaded {
            Loaded::Corrupt { error, kept_at, .. } => Some(match kept_at {
                Some(path) => format!("{error} (kept as {}); started paused with 1 task at a time", path.display()),
                None => format!("{error}; started paused with 1 task at a time"),
            }),
            _ => None,
        };
        let mut stored = loaded.state().clone();
        stored.max_concurrent_tasks = stored.max_concurrent_tasks.clamp(1, options.ceiling_upper_bound);
        let mut regulator = Regulator::new(stored.mode, stored.max_concurrent_tasks);
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
            }),
            launcher,
            store,
            clock,
            options,
        };
        if let Some(message) = config_error {
            let mut inner = supervisor.lock();
            supervisor.emit(&mut inner, EventKind::ServiceError { message });
            // Write the safe configuration so the next start does not trip on the same file.
            supervisor.persist(&mut inner);
        }
        supervisor
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    // MARK: Startup

    /// Adopts the runner processes still alive from before a restart (they count as busy until
    /// they report idle) and clears the others. Never launches a process over an existing one.
    pub fn reconcile(&self) {
        let mut inner = self.lock();
        let ids: Vec<RunnerId> = inner.slots.keys().cloned().collect();
        for id in ids {
            let (generation, pid) = {
                let slot = &inner.slots[&id];
                (slot.record.generation, slot.record.pid)
            };
            match pid {
                Some(pid) if self.launcher.is_alive(&id, generation, pid) => {
                    inner.regulator.adopt(&id, generation);
                    inner.slots.get_mut(&id).unwrap().adopted = true;
                    self.set_state(&mut inner, &id, RunnerState::Busy);
                    self.log(
                        &mut inner,
                        &id,
                        LogLevel::Info,
                        format!("Adopted the running process {pid} after a restart of the service."),
                    );
                }
                Some(pid) => {
                    inner.slots.get_mut(&id).unwrap().record.pid = None;
                    self.log(
                        &mut inner,
                        &id,
                        LogLevel::Warning,
                        format!("Process {pid} ended while the service was stopped."),
                    );
                }
                None => {}
            }
        }
        inner.reconciled = true;
        self.persist(&mut inner);
        self.refresh_admission(&mut inner);
    }

    // MARK: Pool sizing

    /// Sizes the pool: starts runners so that free admission slots can be used for the
    /// `eligible_tasks` Nestor offers this machine, and stops runners idle for longer than
    /// `idle_timeout_ms` beyond that target. Busy runners are never stopped here.
    pub fn tick(&self, eligible_tasks: u32) {
        let mut inner = self.lock();
        if !inner.reconciled {
            return;
        }
        let now = self.now();
        let wanted_idle = inner.regulator.free_slots().min(eligible_tasks) as usize;
        let idle_live =
            inner.slots.values().filter(|s| matches!(s.state, RunnerState::Starting | RunnerState::Ready)).count();

        if idle_live < wanted_idle {
            for _ in idle_live..wanted_idle {
                let candidate = inner
                    .slots
                    .values()
                    .filter(|s| {
                        s.state == RunnerState::Stopped && !s.held && s.restart_at_ms.is_none_or(|at| at <= now)
                    })
                    .map(|s| s.record.id.clone())
                    .next();
                let id = match candidate {
                    Some(id) => id,
                    // A new runner only while the service manages fewer than the ceiling, so a
                    // runner waiting out its restart delay is not replaced by a fresh one.
                    None if Self::provisioned(&inner) < inner.stored.max_concurrent_tasks => {
                        match self.provision(&mut inner, None, false) {
                            Ok(id) => id,
                            Err(error) => {
                                self.emit(&mut inner, EventKind::ServiceError { message: error.to_string() });
                                break;
                            }
                        }
                    }
                    None => break,
                };
                self.spawn(&mut inner, &id);
            }
        } else if idle_live > wanted_idle {
            let mut idle: Vec<(u64, RunnerId)> = inner
                .slots
                .values()
                .filter(|s| s.state == RunnerState::Ready)
                .filter_map(|s| s.idle_since_ms.map(|since| (since, s.record.id.clone())))
                .filter(|(since, _)| now.saturating_sub(*since) >= self.options.idle_timeout_ms)
                .collect();
            idle.sort();
            for (_, id) in idle.into_iter().take(idle_live - wanted_idle) {
                self.log(&mut inner, &id, LogLevel::Info, "Stopping: idle and not needed.".into());
                self.terminate(&mut inner, &id);
            }
        }
    }

    // MARK: Runner reports

    /// The runner is up and registered (or, after an adoption, reports that it is idle).
    pub fn runner_ready(&self, runner_id: &str, generation: u64) -> Result<(), ServiceError> {
        let mut inner = self.lock();
        let slot = Self::current(&inner, runner_id, generation)?;
        match slot.state {
            RunnerState::Starting => {}
            RunnerState::Busy if slot.adopted => {
                inner.regulator.release_runner(runner_id);
            }
            from => return Err(ServiceError::InvalidTransition { from, action: "mark ready".into() }),
        }
        let now = self.now();
        let slot = inner.slots.get_mut(runner_id).unwrap();
        slot.crashes = 0;
        slot.adopted = false;
        slot.idle_since_ms = Some(now);
        self.set_state(&mut inner, runner_id, RunnerState::Ready);
        self.refresh_admission(&mut inner);
        Ok(())
    }

    /// Asks for a token before claiming a task from Nestor. Refused while paused, blocked, at
    /// the ceiling, or if the runner is not ready.
    pub fn admit(&self, runner_id: &str, generation: u64) -> Result<Token, ServiceError> {
        let mut inner = self.lock();
        let slot = Self::current(&inner, runner_id, generation)?;
        if slot.state != RunnerState::Ready {
            return Err(ServiceError::InvalidTransition { from: slot.state, action: "admit a task on".into() });
        }
        let token = inner.regulator.try_acquire(runner_id, generation)?;
        self.set_state(&mut inner, runner_id, RunnerState::Busy);
        Ok(token)
    }

    /// The runner got a task from Nestor with its token.
    pub fn task_started(&self, token: &Token, task_ref: &str) -> Result<(), ServiceError> {
        let mut inner = self.lock();
        Self::holder(&inner, token)?;
        let now = self.now();
        {
            let slot = inner.slots.get_mut(&token.runner_id).unwrap();
            slot.task_ref = Some(task_ref.to_string());
            slot.task_started_ms = Some(now);
            slot.last_task_at_ms = Some(now);
        }
        self.log(&mut inner, &token.runner_id, LogLevel::Info, format!("Task {task_ref} started."));
        self.emit(&mut inner, EventKind::TaskStarted { runner_id: token.runner_id.clone(), task_ref: task_ref.into() });
        Ok(())
    }

    /// The task ended (or no task was claimed): the token goes back and the runner is idle again.
    pub fn task_finished(&self, token: &Token, outcome: TaskOutcome) -> Result<(), ServiceError> {
        let mut inner = self.lock();
        Self::holder(&inner, token)?;
        inner.regulator.release(token);
        let now = self.now();
        let task_ref = {
            let slot = inner.slots.get_mut(&token.runner_id).unwrap();
            slot.task_started_ms = None;
            slot.idle_since_ms = Some(now);
            slot.task_ref.take()
        };
        if let Some(task) = &task_ref {
            let level = if outcome == TaskOutcome::Succeeded { LogLevel::Info } else { LogLevel::Warning };
            self.log(&mut inner, &token.runner_id, level, format!("Task {task} finished: {}.", outcome_text(outcome)));
        }
        self.emit(&mut inner, EventKind::TaskFinished { runner_id: token.runner_id.clone(), task_ref, outcome });
        if inner.slots[&token.runner_id].state == RunnerState::Busy {
            self.set_state(&mut inner, &token.runner_id, RunnerState::Ready);
        }
        Ok(())
    }

    /// The runner's process ended. A stale generation is ignored. An exit nobody asked for counts
    /// as a crash: restart with backoff, then `failed` after `max_crashes` in a row.
    pub fn runner_exited(&self, runner_id: &str, generation: u64, reason: &str) {
        let mut inner = self.lock();
        if Self::current(&inner, runner_id, generation).is_err() {
            return;
        }
        let (state, task_ref) = {
            let slot = &inner.slots[runner_id];
            (slot.state, slot.task_ref.clone())
        };
        if !state.is_live() {
            return;
        }
        if state == RunnerState::Busy || inner.regulator.token_of(runner_id).is_some() {
            inner.regulator.release_runner(runner_id);
            self.emit(
                &mut inner,
                EventKind::TaskFinished { runner_id: runner_id.into(), task_ref, outcome: TaskOutcome::RunnerLost },
            );
        }
        let expected = state == RunnerState::Stopping;
        {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            slot.record.pid = None;
            slot.adopted = false;
            slot.task_ref = None;
            slot.task_started_ms = None;
            slot.idle_since_ms = None;
            slot.usage = (0.0, 0);
        }
        if expected {
            self.log(&mut inner, runner_id, LogLevel::Info, "Stopped.".into());
            self.set_state(&mut inner, runner_id, RunnerState::Stopped);
            self.persist(&mut inner);
        } else {
            self.crashed(&mut inner, runner_id, &format!("exited unexpectedly: {reason}"));
        }
        self.refresh_admission(&mut inner);
    }

    // MARK: Configuration

    pub fn set_mode(&self, mode: Mode, expected_revision: Option<u64>) -> Result<MachineStatus, ServiceError> {
        let mut inner = self.lock();
        Self::check_revision(&inner, expected_revision)?;
        inner.stored.mode = mode;
        inner.regulator.set_mode(mode);
        self.config_changed(&mut inner);
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
        inner.stored.max_concurrent_tasks = value;
        inner.regulator.set_ceiling(value);
        self.config_changed(&mut inner);
        Ok(self.status_locked(&inner))
    }

    pub fn set_connection(&self, connection: Connection) {
        let mut inner = self.lock();
        if inner.stored.connection == connection {
            return;
        }
        inner.stored.connection = connection.clone();
        self.emit(&mut inner, EventKind::ConnectionChanged { connection });
        self.persist(&mut inner);
        self.refresh_admission(&mut inner);
    }

    /// Removes the association: no more admissions, idle runners stop at the next tick, busy
    /// ones finish their task.
    pub fn disassociate(&self) {
        self.set_connection(Connection::NotAssociated);
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

    // MARK: Diagnostics (not the main path)

    pub fn add_runner(&self, name: &str, start_now: bool) -> Result<RunnerView, ServiceError> {
        let mut inner = self.lock();
        if !matches!(inner.stored.connection, Connection::Associated { .. }) {
            return Err(ServiceError::NotAssociated);
        }
        let name = validate_runner_name(name)?;
        if inner.slots.values().any(|s| s.record.name.to_lowercase() == name.to_lowercase()) {
            return Err(ServiceError::DuplicateName(name));
        }
        let id = self.provision(&mut inner, Some(name), true)?;
        if start_now {
            inner.slots.get_mut(&id).unwrap().held = false;
            self.spawn(&mut inner, &id);
        }
        Ok(self.view(&inner.slots[&id]))
    }

    pub fn start_runner(&self, runner_id: &str) -> Result<RunnerView, ServiceError> {
        let mut inner = self.lock();
        let slot = inner.slots.get_mut(runner_id).ok_or(ServiceError::RunnerNotFound)?;
        if !matches!(slot.state, RunnerState::Stopped | RunnerState::Failed) {
            return Err(ServiceError::InvalidTransition { from: slot.state, action: "start".into() });
        }
        slot.held = false;
        slot.crashes = 0;
        slot.failure = None;
        slot.restart_at_ms = None;
        self.spawn(&mut inner, runner_id);
        Ok(self.view(&inner.slots[runner_id]))
    }

    /// Stops a runner by hand. A busy runner is only stopped with `force` (the interface asks
    /// for a confirmation first); its task then follows Nestor's existing policy.
    pub fn stop_runner(&self, runner_id: &str, force: bool) -> Result<RunnerView, ServiceError> {
        let mut inner = self.lock();
        let slot = inner.slots.get_mut(runner_id).ok_or(ServiceError::RunnerNotFound)?;
        match slot.state {
            RunnerState::Starting | RunnerState::Ready => {}
            RunnerState::Busy if force => {}
            from => return Err(ServiceError::InvalidTransition { from, action: "stop".into() }),
        }
        slot.held = true;
        self.log(&mut inner, runner_id, LogLevel::Info, "Stop requested from the diagnostics.".into());
        self.terminate(&mut inner, runner_id);
        Ok(self.view(&inner.slots[runner_id]))
    }

    pub fn remove_runner(&self, runner_id: &str) -> Result<(), ServiceError> {
        let mut inner = self.lock();
        let slot = inner.slots.get(runner_id).ok_or(ServiceError::RunnerNotFound)?;
        if !matches!(slot.state, RunnerState::Stopped | RunnerState::Failed) {
            return Err(ServiceError::InvalidTransition { from: slot.state, action: "remove".into() });
        }
        inner.slots.remove(runner_id);
        self.launcher.deprovision(runner_id);
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

    fn provisioned(inner: &Inner) -> u32 {
        inner.slots.values().filter(|s| !s.record.manual).count() as u32
    }

    fn holder(inner: &Inner, token: &Token) -> Result<(), ServiceError> {
        match inner.regulator.token_of(&token.runner_id) {
            Some(held) if held == token => Ok(()),
            _ => Err(ServiceError::InvalidArgument { field: "token".into(), reason: "not held".into() }),
        }
    }

    fn check_revision(inner: &Inner, expected: Option<u64>) -> Result<(), ServiceError> {
        match expected {
            Some(revision) if revision != inner.stored.config_revision => {
                Err(ServiceError::Conflict { current_revision: inner.stored.config_revision })
            }
            _ => Ok(()),
        }
    }

    fn config_changed(&self, inner: &mut Inner) {
        inner.stored.config_revision += 1;
        let (revision, mode, max) =
            (inner.stored.config_revision, inner.stored.mode, inner.stored.max_concurrent_tasks);
        self.emit(inner, EventKind::ConfigChanged { revision, mode, max_concurrent_tasks: max });
        self.persist(inner);
        self.refresh_admission(inner);
    }

    fn provision(&self, inner: &mut Inner, name: Option<String>, manual: bool) -> Result<RunnerId, ServiceError> {
        let id = new_id();
        let name = name.unwrap_or_else(|| {
            (1..)
                .map(|n| format!("runner-{n}"))
                .find(|candidate| !inner.slots.values().any(|s| &s.record.name == candidate))
                .unwrap()
        });
        let record = RunnerRecord { id: id.clone(), name, generation: 0, pid: None, created_at_ms: self.now(), manual };
        self.launcher.provision(&record)?;
        let mut slot = Slot::new(record, RunnerState::Stopped);
        slot.held = manual;
        inner.slots.insert(id.clone(), slot);
        self.log(
            inner,
            &id,
            LogLevel::Info,
            if manual { "Added from the diagnostics.".into() } else { "Provisioned by the service.".into() },
        );
        self.persist(inner);
        Ok(id)
    }

    fn spawn(&self, inner: &mut Inner, runner_id: &str) {
        let record = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            debug_assert!(!slot.state.is_live(), "never launch over a live runner");
            slot.record.generation += 1;
            slot.restart_at_ms = None;
            slot.record.clone()
        };
        self.set_state(inner, runner_id, RunnerState::Starting);
        match self.launcher.spawn(&record) {
            Ok(pid) => {
                inner.slots.get_mut(runner_id).unwrap().record.pid = Some(pid);
                self.log(inner, runner_id, LogLevel::Info, format!("Started (generation {}).", record.generation));
                self.persist(inner);
            }
            Err(reason) => self.crashed(inner, runner_id, &format!("could not start: {reason}")),
        }
    }

    /// An exit nobody asked for: restart later with an exponential backoff, or leave the runner
    /// `failed` after `max_crashes` in a row.
    fn crashed(&self, inner: &mut Inner, runner_id: &str, reason: &str) {
        let now = self.now();
        let options = self.options.clone();
        let next = {
            let slot = inner.slots.get_mut(runner_id).unwrap();
            slot.record.pid = None;
            slot.crashes += 1;
            if slot.crashes >= options.max_crashes {
                slot.failure = Some(format!("Exited {} times in a row; last: {reason}", slot.crashes));
                RunnerState::Failed
            } else {
                let delay =
                    options.backoff_base_ms.saturating_mul(1 << (slot.crashes - 1).min(20)).min(options.backoff_max_ms);
                slot.restart_at_ms = Some(now + delay);
                RunnerState::Stopped
            }
        };
        self.log(inner, runner_id, LogLevel::Error, capitalized(reason));
        self.set_state(inner, runner_id, next);
        self.persist(inner);
    }

    fn terminate(&self, inner: &mut Inner, runner_id: &str) {
        let (generation, pid) = {
            let slot = &inner.slots[runner_id];
            (slot.record.generation, slot.record.pid)
        };
        self.set_state(inner, runner_id, RunnerState::Stopping);
        match pid {
            Some(pid) => self.launcher.terminate(runner_id, generation, pid),
            None => {
                // Nothing to wait for.
                self.set_state(inner, runner_id, RunnerState::Stopped);
                self.persist(inner);
            }
        }
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
                Connection::NotAssociated => Some(BlockReason::NotAssociated),
                Connection::Unreachable { .. } => Some(BlockReason::Unreachable),
                Connection::Revoked => Some(BlockReason::Revoked),
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
        inner.stored.runners = inner.slots.values().map(|s| s.record.clone()).collect();
        if let Err(error) = self.store.save(&inner.stored) {
            self.emit(inner, EventKind::ServiceError { message: format!("Could not save the configuration: {error}") });
        }
    }

    fn emit(&self, inner: &mut Inner, kind: EventKind) {
        let event = Event { seq: inner.next_seq, at_ms: self.now(), kind };
        inner.next_seq += 1;
        inner.events.push_back(event);
        while inner.events.len() > self.options.event_capacity {
            inner.events.pop_front();
        }
    }

    fn log(&self, inner: &mut Inner, runner_id: &str, level: LogLevel, message: String) {
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
            max_concurrent_tasks_upper_bound: self.options.ceiling_upper_bound,
            config_revision: inner.stored.config_revision,
            busy_runners: inner.regulator.busy(),
            active_tasks,
            load: inner.load.clone(),
            config_error: inner.config_error.clone(),
        }
    }
}

impl Slot {
    fn new(record: RunnerRecord, state: RunnerState) -> Self {
        Slot {
            held: record.manual,
            adopted: false,
            record,
            state,
            failure: None,
            task_ref: None,
            task_started_ms: None,
            idle_since_ms: None,
            last_task_at_ms: None,
            crashes: 0,
            restart_at_ms: None,
            usage: (0.0, 0),
            logs: VecDeque::new(),
        }
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
        TaskOutcome::NotClaimed => "not claimed",
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
