//! Types shared by the supervisor, the persistence layer and the IPC protocol.
//!
//! Resource figures are measurements only: there is no processor or memory limit anywhere in
//! this model (KCA-714). The concurrency ceiling regulates how many tasks are *admitted*, which
//! is a different thing from a resource quota.

use serde::{Deserialize, Serialize};

pub type RunnerId = String;

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
    Revoked,
}

/// A runner's lifecycle. A runner takes one task at a time: `Busy` means it holds an admission
/// token and is claiming or running its task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunnerState {
    Stopped,
    Starting,
    Ready,
    Busy,
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
    /// Upper bound accepted for `max_concurrent_tasks`; a sanity check on the input, not a limit
    /// on any runner's resources.
    pub max_concurrent_tasks_upper_bound: u32,
    pub config_revision: u64,
    pub busy_runners: u32,
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
    /// The runner had a token but did not get a task from Nestor.
    NotClaimed,
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
    Conflict { current_revision: u64 },
    InvalidArgument { field: String, reason: String },
    LimitOutOfRange { min: u32, max: u32 },
    RunnerNotFound,
    InvalidTransition { from: RunnerState, action: String },
    DuplicateName(String),
    PermissionDenied,
    IncompatibleVersion { supported: String },
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
            ServiceError::IncompatibleVersion { supported } => {
                write!(f, "Incompatible service: it speaks API {supported}.")
            }
            ServiceError::SnapshotRequired => write!(f, "Events were dropped; reload the machine status."),
            ServiceError::Internal(id) => write!(f, "Internal error ({id})."),
        }
    }
}

impl std::error::Error for ServiceError {}

/// Same rule as the macOS interface: 1 to 40 letters, digits, spaces, `-`, `_` or `.`.
pub fn validate_runner_name(raw: &str) -> Result<String, ServiceError> {
    let name = raw.trim();
    let invalid = |reason: &str| ServiceError::InvalidArgument { field: "name".into(), reason: reason.into() };
    if name.is_empty() {
        return Err(invalid("give the runner a name"));
    }
    if name.chars().count() > 40 {
        return Err(invalid("use at most 40 characters"));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ')) {
        return Err(invalid("use letters, digits, spaces, “-”, “_” or “.”"));
    }
    Ok(name.to_string())
}
