//! Types shared by the supervisor, the persistence layer and the IPC protocol.
//!
//! Resource figures are measurements only: there is no processor or memory limit anywhere in
//! this model (KCA-714). The concurrency ceiling regulates how many tasks are *admitted*, which
//! is a different thing from a resource quota.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type RunnerId = String;

/// Nestor's opaque identifier of the scope (workspace) a job belongs to. The service never sees
/// a workspace's name or content, only this identifier, to keep tasks of different scopes apart
/// when more than one runs at a time (decision B1(b), contract v2).
pub type ScopeId = String;

/// What Nestor offers this machine right now: the number of eligible jobs per scope, capped by
/// the server, without any content (`GET /machine/queue`, S4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queue {
    pub eligible: BTreeMap<ScopeId, u32>,
}

impl Queue {
    pub fn empty() -> Self {
        Queue::default()
    }

    /// `count` eligible jobs in a single scope.
    pub fn single(scope: &str, count: u32) -> Self {
        let mut eligible = BTreeMap::new();
        if count > 0 {
            eligible.insert(scope.to_string(), count);
        }
        Queue { eligible }
    }

    pub fn total(&self) -> u32 {
        self.eligible.values().fold(0u32, |sum, n| sum.saturating_add(*n))
    }

    pub fn in_scope(&self, scope: &str) -> u32 {
        self.eligible.get(scope).copied().unwrap_or(0)
    }

    /// The scope with the most eligible jobs; ties go to the smallest identifier, so the choice
    /// is deterministic.
    pub fn busiest(&self) -> Option<(&ScopeId, u32)> {
        self.eligible
            .iter()
            .filter(|(_, n)| **n > 0)
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(scope, n)| (scope, *n))
    }
}

/// Whether the data behind the service is real. The interface shows its demo banner whenever
/// this is not `Live`, and must never infer `Live` from anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceMode {
    Demo,
    Live,
}

/// The admission mode chosen by the user. `Paused` stops new admissions; tasks already running
/// finish normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Automatic,
    Paused,
}

/// Why no task can be admitted right now, besides a pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockReason {
    NotAssociated,
    Unreachable,
    Revoked,
    /// The service has just started and is still adopting or clearing the runners it left.
    Reconciling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state", content = "reason")]
pub enum Admission {
    Open,
    Paused,
    Blocked(BlockReason),
}

/// The machine's association with a Nestor instance, as the service knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum Connection {
    NotAssociated,
    /// A device authorization is in progress: the user approves it on the Nestor web app with
    /// `user_code`. Nothing secret is shown: the user code alone grants nothing, the approval
    /// needs an administrator's session on the web (decision B6).
    #[serde(rename_all = "camelCase")]
    Associating {
        instance: String,
        user_code: String,
        verification_uri: String,
        expires_at_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    Associated {
        instance: String,
        since_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    Unreachable {
        instance: String,
        reason: String,
    },
    /// Nestor revoked the machine: runners were stopped and secrets erased. Associate again to
    /// take tasks.
    Revoked,
}

impl Connection {
    pub fn is_associated(&self) -> bool {
        matches!(self, Connection::Associated { .. } | Connection::Unreachable { .. })
    }
}

/// A runner's lifecycle. A managed runner is a "one job" process: it is launched (`Starting`),
/// comes up and asks Nestor for one job (`Ready`), runs it (`Busy`), then exits (`Stopped`).
/// Launching it *is* the admission: there is no local token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunnerState {
    Stopped,
    Starting,
    Ready,
    Busy,
    /// Asked to stop: drained (no new claim, its job if any finishes) or, on revocation and
    /// forced stops only, terminated.
    Stopping,
    Failed,
}

impl RunnerState {
    pub fn is_live(self) -> bool {
        matches!(self, RunnerState::Starting | RunnerState::Ready | RunnerState::Busy | RunnerState::Stopping)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTask {
    pub runner_id: RunnerId,
    pub runner_name: String,
    /// Opaque reference given by Nestor. No task content is kept locally.
    pub task_ref: String,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerView {
    pub id: RunnerId,
    pub name: String,
    pub state: RunnerState,
    pub generation: u64,
    pub failure_reason: Option<String>,
    pub task_ref: Option<String>,
    /// The scope this runner may claim from, when more than one task may run at a time.
    pub scope: Option<ScopeId>,
    pub created_at_ms: u64,
    pub last_task_at_ms: Option<u64>,
    /// Measured, not capped.
    pub measured_cpu_percent: f64,
    pub measured_memory_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

/// One sample of the machine's load. Measurements only, never limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoadSample {
    pub sampled_at_ms: u64,
    pub measured_cpu_percent: f64,
    pub measured_memory_used_bytes: u64,
    pub measured_memory_total_bytes: u64,
    pub measured_memory_pressure: Option<MemoryPressure>,
    pub measured_disk_free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineInfo {
    pub name: String,
    pub model: String,
    pub architecture: String,
    pub operating_system: String,
    pub cpu_cores: u32,
    pub memory_bytes: u64,
    pub disk_free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineStatus {
    pub service_mode: ServiceMode,
    pub connection: Connection,
    pub mode: Mode,
    pub admission: Admission,
    pub max_concurrent_tasks: u32,
    /// What the service actually applies: `max_concurrent_tasks`, or 1 while Nestor does not
    /// announce `capabilities.scope_filter` (KCA-734 §3).
    pub effective_max_concurrent_tasks: u32,
    /// The requested value is above what Nestor allows for now (shown as "limited by the server").
    pub capped_by_server: bool,
    /// Upper bound accepted for `max_concurrent_tasks`; a sanity check on the input, not a limit
    /// on any runner's resources.
    pub max_concurrent_tasks_upper_bound: u32,
    pub config_revision: u64,
    pub busy_runners: u32,
    /// Runner processes alive (starting, waiting for a job, busy or stopping). Each may hold one
    /// job, so this is what the ceiling counts.
    pub live_runners: u32,
    /// Set while more than one task may run: the only scope new runners may claim from, so
    /// tasks of different workspaces never run side by side (B1(b)). Opaque.
    pub active_scope: Option<ScopeId>,
    pub active_tasks: Vec<ActiveTask>,
    pub load: Option<LoadSample>,
    /// Set when the stored configuration could not be read: the service then starts paused with
    /// a ceiling of 1.
    pub config_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub at_ms: u64,
    pub level: LogLevel,
    pub message: String,
}

/// How a task ended, from the service's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskOutcome {
    Succeeded,
    Failed,
    /// The runner died while running it; Nestor's existing policy decides what happens next.
    RunnerLost,
    /// The runner was stopped on purpose during its task (forced stop, revocation).
    Stopped,
    /// The runner was adopted after a restart and ended without the service knowing how.
    Unknown,
    /// The runner lost its lease (fencing): it stopped the job without finishing it.
    LeaseLost,
    /// The runner gave the job back to Nestor without running it (`scope_mismatch`).
    Released,
}

/// How a runner process ended, as its launcher reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerExit {
    /// It ran its one job and exited normally.
    Finished(TaskOutcome),
    /// It exited without a job: idle timeout, or drained before it claimed one.
    Idle,
    /// An adopted process is gone; the service did not launch it and cannot tell how it ended.
    Vanished,
    /// Anything else: a crash, a launch failure, a signal nobody sent.
    Crashed(String),
    /// Exit 64: the launch itself is wrong (a bug). Left `failed`, never launched again.
    Usage,
    /// Exit 75: Nestor could not be reached. Launched again after the backoff, not a crash.
    Unreachable,
    /// Exit 77: Nestor refused the credential (machine revoked). Left `failed`, no relaunch,
    /// and no runner is launched until the connection changes.
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum EventKind {
    #[serde(rename = "machine.connectionChanged")]
    ConnectionChanged { connection: Connection },
    #[serde(rename = "machine.configChanged", rename_all = "camelCase")]
    ConfigChanged { revision: u64, mode: Mode, max_concurrent_tasks: u32 },
    #[serde(rename = "machine.loadSample")]
    LoadSample { sample: LoadSample },
    #[serde(rename = "runner.stateChanged", rename_all = "camelCase")]
    RunnerStateChanged { runner_id: RunnerId, state: RunnerState, generation: u64 },
    #[serde(rename = "task.started", rename_all = "camelCase")]
    TaskStarted { runner_id: RunnerId, task_ref: String },
    #[serde(rename = "task.finished", rename_all = "camelCase")]
    TaskFinished { runner_id: RunnerId, task_ref: Option<String>, outcome: TaskOutcome },
    #[serde(rename = "admission.changed")]
    AdmissionChanged { admission: Admission },
    /// A runner reported a job from another scope than the one it was launched for: Nestor did
    /// not apply the scope filter. New launches stop until it ends (B1(b), A2).
    #[serde(rename = "scope.violation", rename_all = "camelCase")]
    ScopeViolation { runner_id: RunnerId },
    #[serde(rename = "service.error")]
    ServiceError { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: u64,
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Stable error codes of the local API (contract §4.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    NotAssociated,
    Revoked,
    Unreachable,
    Unavailable(String),
    Conflict {
        current_revision: u64,
    },
    InvalidArgument {
        field: String,
        reason: String,
    },
    LimitOutOfRange {
        min: u32,
        max: u32,
    },
    RunnerNotFound,
    InvalidTransition {
        from: RunnerState,
        action: String,
    },
    DuplicateName(String),
    PermissionDenied,
    /// A sensitive operation needs the interface's explicit confirmation (`confirmed: true`).
    ConfirmationRequired(String),
    IncompatibleVersion {
        supported: String,
    },
    SnapshotRequired,
    Internal(String),
}

impl ServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            ServiceError::NotAssociated => "notAssociated",
            ServiceError::Revoked => "revoked",
            ServiceError::Unreachable => "unreachable",
            ServiceError::Unavailable(_) => "unavailable",
            ServiceError::Conflict { .. } => "conflict",
            ServiceError::InvalidArgument { .. } => "invalidArgument",
            ServiceError::LimitOutOfRange { .. } => "limitOutOfRange",
            ServiceError::RunnerNotFound => "runnerNotFound",
            ServiceError::InvalidTransition { .. } => "invalidTransition",
            ServiceError::DuplicateName(_) => "duplicateName",
            ServiceError::PermissionDenied => "permissionDenied",
            ServiceError::ConfirmationRequired(_) => "confirmationRequired",
            ServiceError::IncompatibleVersion { .. } => "incompatibleVersion",
            ServiceError::SnapshotRequired => "snapshotRequired",
            ServiceError::Internal(_) => "internal",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ServiceError::Unreachable
                | ServiceError::Unavailable(_)
                | ServiceError::Conflict { .. }
                | ServiceError::Internal(_)
        )
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::NotAssociated => write!(f, "This machine is not associated with Nestor."),
            ServiceError::Revoked => write!(f, "This machine's association was revoked in Nestor."),
            ServiceError::Unreachable => write!(f, "Nestor cannot be reached."),
            ServiceError::Unavailable(reason) => write!(f, "{reason}"),
            ServiceError::Conflict { current_revision } => {
                write!(f, "The settings changed meanwhile (revision {current_revision}). Reload and try again.")
            }
            ServiceError::InvalidArgument { field, reason } => write!(f, "Invalid {field}: {reason}"),
            ServiceError::LimitOutOfRange { min, max } => write!(f, "Choose between {min} and {max} tasks at a time."),
            ServiceError::RunnerNotFound => write!(f, "This runner no longer exists."),
            ServiceError::InvalidTransition { from, action } => {
                write!(
                    f,
                    "Cannot {action} a runner that is {}.",
                    serde_json::to_value(from).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
                )
            }
            ServiceError::DuplicateName(name) => write!(f, "A runner named “{name}” already exists on this machine."),
            ServiceError::PermissionDenied => write!(f, "Permission denied."),
            ServiceError::ConfirmationRequired(what) => write!(f, "Confirm first: {what}"),
            ServiceError::IncompatibleVersion { supported } => {
                write!(f, "Incompatible service: it speaks API {supported}.")
            }
            ServiceError::SnapshotRequired => write!(f, "Events were dropped; reload the machine status."),
            ServiceError::Internal(id) => write!(f, "Internal error ({id})."),
        }
    }
}

impl std::error::Error for ServiceError {}

/// Same rule as the macOS interface: 1 to 40 ASCII letters, digits, spaces, `-`, `_` or `.`.
/// ASCII only, so two names that look the same are the same (no Unicode normalization to get
/// wrong); the service checks it again whatever the client did (N6).
pub fn validate_runner_name(raw: &str) -> Result<String, ServiceError> {
    let name = raw.trim();
    let invalid = |reason: &str| ServiceError::InvalidArgument { field: "name".into(), reason: reason.into() };
    if name.is_empty() {
        return Err(invalid("give the runner a name"));
    }
    if name.chars().count() > 40 {
        return Err(invalid("use at most 40 characters"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ')) {
        return Err(invalid("use letters (A to Z), digits, spaces, “-”, “_” or “.”"));
    }
    Ok(name.to_string())
}

/// A scope identifier comes from Nestor and is passed to a runner as one `argv` entry: 1 to 64
/// ASCII letters, digits, `-` or `_`, nothing else.
pub fn validate_scope(raw: &str) -> Result<(), ServiceError> {
    let valid =
        !raw.is_empty() && raw.len() <= 64 && raw.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ServiceError::InvalidArgument { field: "scope".into(), reason: "not an opaque scope identifier".into() })
    }
}

/// A runner identifier: 32 lowercase hexadecimal digits.
pub fn is_runner_id(raw: &str) -> bool {
    raw.len() == 32 && raw.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}
