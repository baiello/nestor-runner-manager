//! The service's state file: non-secret configuration and the runners it manages.
//!
//! Written atomically (temporary file, fsync, rename) with owner-only permissions. Secrets are
//! never written here; they belong in the operating system's secure storage (not in this lot).
//! An unreadable file never stops the service: it starts paused with a ceiling of 1 and reports
//! the error, and the bad file is kept aside for diagnosis.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Connection, Mode, RunnerId};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerRecord {
    pub id: RunnerId,
    pub name: String,
    pub generation: u64,
    /// The process of the current generation, kept so a restarted service adopts it instead of
    /// launching a second one.
    pub pid: Option<u32>,
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
        }
    }
}

impl StoredState {
    /// The safe fallback when the stored state cannot be trusted.
    pub fn safe_fallback() -> Self {
        StoredState { mode: Mode::Paused, max_concurrent_tasks: 1, ..StoredState::default() }
    }
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
        if state.schema_version != SCHEMA_VERSION {
            return Err(format!("unknown state schema version {}", state.schema_version));
        }
        if state.max_concurrent_tasks == 0 {
            return Err("stored ceiling of 0 tasks".into());
        }
        Ok(state)
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
            Err(error) => {
                let kept = self.path.with_extension(format!("json.corrupt-{}", std::process::id()));
                let kept_at = fs::rename(&self.path, &kept).ok().map(|_| kept);
                Loaded::Corrupt { state: StoredState::safe_fallback(), error, kept_at }
            }
        }
    }

    fn save(&self, state: &StoredState) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
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
