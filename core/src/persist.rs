//! The service's state file: non-secret configuration and the runners it manages.
//!
//! Written atomically (temporary file, fsync, rename) with owner-only permissions. Secrets are
//! never written here; they belong in the operating system's secure storage ([`crate::secrets`]).
//! The file is validated when read, schema *and* bounds (N1): an unreadable file or any invalid
//! value never stops the service, it starts paused with a ceiling of 1 and reports the error,
//! and the bad file is kept aside for diagnosis.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{is_runner_id, validate_runner_name, validate_scope, Connection, Mode, RunnerId, ScopeId};

pub const SCHEMA_VERSION: u32 = 2;
/// Most runners the service keeps records for (a bound on input, not on resources).
pub const MAX_RUNNERS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerRecord {
    pub id: RunnerId,
    pub name: String,
    pub generation: u64,
    /// The process of the current generation, kept so a restarted service adopts it instead of
    /// launching a second one. Never trusted alone: see `start_time_us`.
    pub pid: Option<u32>,
    /// When that process started (microseconds since the epoch, from the kernel). A pid is only
    /// considered ours if the process behind it started at this exact time and carries this
    /// generation in its arguments (B4).
    #[serde(default)]
    pub start_time_us: Option<u64>,
    /// The scope the current process was launched for (B1(b)).
    #[serde(default)]
    pub scope: Option<ScopeId>,
    pub created_at_ms: u64,
    /// Added by hand from the diagnostics, rather than provisioned by the service.
    #[serde(default)]
    pub manual: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredState {
    pub schema_version: u32,
    pub machine_id: Option<String>,
    pub connection: Connection,
    pub mode: Mode,
    pub max_concurrent_tasks: u32,
    pub config_revision: u64,
    pub runners: Vec<RunnerRecord>,
    /// A revocation Nestor has not confirmed yet. It is signed by the machine key before the key
    /// is erased, so it can be retried after a local disassociation without keeping any secret:
    /// it allows nothing but revoking this machine (B7).
    #[serde(default)]
    pub pending_revocation: Option<RevocationRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RevocationRequest {
    pub instance: String,
    pub machine_id: String,
    pub issued_at_ms: u64,
    /// Base64 of the signature of [`RevocationRequest::message`] by the machine key.
    pub signature: String,
}

impl RevocationRequest {
    pub fn message(instance: &str, machine_id: &str, issued_at_ms: u64) -> String {
        format!("nestor-machine-revoke\n{instance}\n{machine_id}\n{issued_at_ms}")
    }
}

impl Default for StoredState {
    /// A fresh machine: not associated, automatic mode, one task at a time.
    fn default() -> Self {
        StoredState {
            schema_version: SCHEMA_VERSION,
            machine_id: None,
            connection: Connection::NotAssociated,
            mode: Mode::Automatic,
            max_concurrent_tasks: 1,
            config_revision: 0,
            runners: Vec::new(),
            pending_revocation: None,
        }
    }
}

impl StoredState {
    /// The safe fallback when the stored state cannot be trusted.
    pub fn safe_fallback() -> Self {
        StoredState { mode: Mode::Paused, max_concurrent_tasks: 1, ..StoredState::default() }
    }

    /// Checks every value against its bounds. `ceiling_upper_bound` is the service's own bound
    /// on the ceiling (number of cores by default).
    pub fn validate(&self, ceiling_upper_bound: u32) -> Result<(), String> {
        if !(1..=SCHEMA_VERSION).contains(&self.schema_version) {
            return Err(format!("unknown state schema version {}", self.schema_version));
        }
        if self.max_concurrent_tasks == 0 || self.max_concurrent_tasks > ceiling_upper_bound {
            return Err(format!(
                "stored ceiling of {} tasks is outside 1…{ceiling_upper_bound}",
                self.max_concurrent_tasks
            ));
        }
        if self.runners.len() > MAX_RUNNERS {
            return Err(format!("{} runners stored, at most {MAX_RUNNERS}", self.runners.len()));
        }
        if let Some(id) = &self.machine_id {
            if !valid_text(id, 128) {
                return Err("invalid machine id".into());
            }
        }
        match &self.connection {
            Connection::NotAssociated | Connection::Revoked => {}
            Connection::Associated { instance, .. } | Connection::Unreachable { instance, .. } => {
                if !valid_text(instance, 253) {
                    return Err("invalid Nestor instance".into());
                }
                if self.machine_id.is_none() {
                    return Err("associated without a machine id".into());
                }
            }
            Connection::Associating { .. } => return Err("an association in progress is never stored".into()),
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for runner in &self.runners {
            if !is_runner_id(&runner.id) || !ids.insert(runner.id.clone()) {
                return Err("invalid or repeated runner id".into());
            }
            let name = validate_runner_name(&runner.name).map_err(|e| format!("runner name: {e}"))?;
            if name != runner.name || !names.insert(name.to_ascii_lowercase()) {
                return Err("invalid or repeated runner name".into());
            }
            if runner.pid == Some(0) || (runner.pid.is_some() != runner.start_time_us.is_some()) {
                return Err("runner process recorded without its start time".into());
            }
            if let Some(scope) = &runner.scope {
                validate_scope(scope).map_err(|e| e.to_string())?;
            }
        }
        if let Some(revocation) = &self.pending_revocation {
            if !valid_text(&revocation.instance, 253)
                || !valid_text(&revocation.machine_id, 128)
                || !valid_text(&revocation.signature, 1024)
            {
                return Err("invalid pending revocation".into());
            }
        }
        Ok(())
    }
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.chars().all(|c| c.is_ascii_graphic())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    Fresh(StoredState),
    Restored(StoredState),
    /// The file was unreadable; `state` is the safe fallback and the file was moved to `kept_at`.
    Corrupt {
        state: StoredState,
        error: String,
        kept_at: Option<PathBuf>,
    },
}

impl Loaded {
    pub fn state(&self) -> &StoredState {
        match self {
            Loaded::Fresh(state) | Loaded::Restored(state) | Loaded::Corrupt { state, .. } => state,
        }
    }
}

pub trait Store: Send + Sync {
    fn load(&self) -> Loaded;
    fn save(&self, state: &StoredState) -> std::io::Result<()>;
    /// Moves the stored file aside for diagnosis, when it parsed but failed validation.
    fn set_aside(&self) -> Option<PathBuf> {
        None
    }
}

/// A store kept in memory, for tests and the demo.
#[derive(Default)]
pub struct MemoryStore {
    pub state: std::sync::Mutex<Option<StoredState>>,
}

impl Store for MemoryStore {
    fn load(&self) -> Loaded {
        match self.state.lock().unwrap().clone() {
            Some(state) => Loaded::Restored(state),
            None => Loaded::Fresh(StoredState::default()),
        }
    }

    fn save(&self, state: &StoredState) -> std::io::Result<()> {
        *self.state.lock().unwrap() = Some(state.clone());
        Ok(())
    }
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// `directory` is created with owner-only permissions if needed.
    pub fn new(directory: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        Ok(FileStore { path: directory.join("state.json") })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn parse(bytes: &[u8]) -> Result<StoredState, String> {
        let state: StoredState = serde_json::from_slice(bytes).map_err(|e| format!("unreadable state file: {e}"))?;
        // The bound on the ceiling is checked by the supervisor, which knows it.
        state.validate(u32::MAX)?;
        Ok(state)
    }

    fn keep_aside(&self) -> Option<PathBuf> {
        let kept = self.path.with_extension(format!("json.corrupt-{}", std::process::id()));
        fs::rename(&self.path, &kept).ok().map(|_| kept)
    }
}

impl Store for FileStore {
    fn load(&self) -> Loaded {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Fresh(StoredState::default()),
            Err(e) => {
                return Loaded::Corrupt {
                    state: StoredState::safe_fallback(),
                    error: format!("cannot read the state file: {e}"),
                    kept_at: None,
                }
            }
        };
        match Self::parse(&bytes) {
            Ok(state) => Loaded::Restored(state),
            Err(error) => Loaded::Corrupt { state: StoredState::safe_fallback(), error, kept_at: self.keep_aside() },
        }
    }

    fn set_aside(&self) -> Option<PathBuf> {
        self.keep_aside()
    }

    fn save(&self, state: &StoredState) -> std::io::Result<()> {
        let mut state = state.clone();
        state.schema_version = SCHEMA_VERSION;
        let bytes = serde_json::to_vec_pretty(&state).map_err(std::io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        {
            let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &self.path)?;
        if let Some(parent) = self.path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}
