//! The local API: JSON lines over a Unix domain socket, owner-only.
//!
//! See `docs/local-api.md` for the specification. Every request carries `apiVersion`; a client
//! with another major version gets `incompatibleVersion`. Only processes of the user running the
//! service may connect: the peer's user id is checked with the kernel (`getpeereid`) on every
//! connection, and the socket lives in a 0700 directory with 0600 permissions. There is no TCP
//! listener.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::load::LoadSampler;
use crate::model::*;
use crate::supervisor::Supervisor;

pub const API_VERSION: &str = "1.0";
pub const API_MAJOR: u32 = 1;
const MAX_LINE_BYTES: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 16;
pub const DEMO_INSTANCE: &str = "demo.nestor.invalid";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub api_version: String,
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", tag = "op")]
pub enum Request {
    Hello,
    MachineInfo,
    MachineStatus,
    Connection,
    Associate,
    Disassociate,
    Runners,
    #[serde(rename_all = "camelCase")]
    AddRunner {
        name: String,
        #[serde(default)]
        start_now: bool,
    },
    #[serde(rename_all = "camelCase")]
    Start {
        runner_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Stop {
        runner_id: String,
        #[serde(default)]
        force: bool,
    },
    #[serde(rename_all = "camelCase")]
    Remove {
        runner_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Logs {
        runner_id: String,
        #[serde(default = "default_log_limit")]
        limit: usize,
    },
    #[serde(rename_all = "camelCase")]
    SetMode {
        mode: Mode,
        expected_revision: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    SetMaxConcurrentTasks {
        value: u32,
        expected_revision: Option<u64>,
    },
    ActiveTasks,
    Events {
        #[serde(default)]
        since: u64,
    },
}

fn default_log_limit() -> usize {
    200
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl From<&ServiceError> for ErrorBody {
    fn from(error: &ServiceError) -> Self {
        let details = match error {
            ServiceError::Conflict { current_revision } => Some(json!({ "currentRevision": current_revision })),
            ServiceError::InvalidArgument { field, .. } => Some(json!({ "field": field })),
            ServiceError::LimitOutOfRange { min, max } => Some(json!({ "min": min, "max": max })),
            ServiceError::InvalidTransition { from, action } => Some(json!({ "from": from, "action": action })),
            ServiceError::DuplicateName(name) => Some(json!({ "name": name })),
            ServiceError::IncompatibleVersion { supported } => Some(json!({ "supported": supported })),
            ServiceError::Internal(id) => Some(json!({ "correlationId": id })),
            _ => None,
        };
        ErrorBody { code: error.code(), message: error.to_string(), retryable: error.retryable(), details }
    }
}

/// What the IPC server exposes: the supervisor plus the machine description and the association
/// behavior of the current mode.
pub struct Service {
    pub supervisor: Arc<Supervisor>,
    sampler: Mutex<LoadSampler>,
    clock: crate::supervisor::Clock,
}

impl Service {
    pub fn new(supervisor: Arc<Supervisor>, clock: crate::supervisor::Clock) -> Self {
        Service { supervisor, sampler: Mutex::new(LoadSampler::new()), clock }
    }

    pub fn mode(&self) -> ServiceMode {
        self.supervisor.options().service_mode
    }

    /// Takes a load sample and hands it to the supervisor.
    pub fn sample_load(&self) {
        let sample = self.sampler.lock().unwrap().sample((self.clock)());
        self.supervisor.set_load(sample);
    }

    /// Handles one JSON request line and returns the response line.
    pub fn handle_line(&self, line: &str) -> String {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                let error = ServiceError::InvalidArgument { field: "request".into(), reason: e.to_string() };
                return respond(None, Err(error));
            }
        };
        let id = value.get("id").and_then(Value::as_u64);
        let major = value
            .get("apiVersion")
            .and_then(Value::as_str)
            .and_then(|v| v.split('.').next())
            .and_then(|m| m.parse::<u32>().ok());
        if major != Some(API_MAJOR) {
            return respond(id, Err(ServiceError::IncompatibleVersion { supported: API_VERSION.into() }));
        }
        let envelope: Envelope = match serde_json::from_value(value) {
            Ok(envelope) => envelope,
            Err(e) => {
                return respond(
                    id,
                    Err(ServiceError::InvalidArgument { field: "request".into(), reason: e.to_string() }),
                )
            }
        };
        respond(id, self.handle(envelope.request))
    }

    pub fn handle(&self, request: Request) -> Result<Value, ServiceError> {
        let supervisor = &self.supervisor;
        match request {
            Request::Hello => Ok(json!({ "apiVersion": API_VERSION, "serviceMode": self.mode() })),
            Request::MachineInfo => Ok(to_value(&self.sampler.lock().unwrap().machine_info())),
            Request::MachineStatus => Ok(to_value(&supervisor.status())),
            Request::Connection => Ok(to_value(&supervisor.connection())),
            Request::Associate => match self.mode() {
                ServiceMode::Demo => {
                    if !matches!(supervisor.connection(), Connection::Associated { .. }) {
                        supervisor.set_connection(Connection::Associated { instance: DEMO_INSTANCE.into(), since_ms: (self.clock)() });
                    }
                    Ok(to_value(&supervisor.connection()))
                }
                ServiceMode::Live => Err(ServiceError::Unavailable(
                    "Association with Nestor is not available in this version yet: it needs the Nestor API work (device authorization).".into(),
                )),
            },
            Request::Disassociate => {
                supervisor.disassociate();
                Ok(to_value(&supervisor.connection()))
            }
            Request::Runners => Ok(to_value(&supervisor.runners())),
            Request::AddRunner { name, start_now } => supervisor.add_runner(&name, start_now).map(|r| to_value(&r)),
            Request::Start { runner_id } => supervisor.start_runner(&runner_id).map(|r| to_value(&r)),
            Request::Stop { runner_id, force } => supervisor.stop_runner(&runner_id, force).map(|r| to_value(&r)),
            Request::Remove { runner_id } => supervisor.remove_runner(&runner_id).map(|_| json!({})),
            Request::Logs { runner_id, limit } => supervisor.logs(&runner_id, limit.min(1_000)).map(|l| to_value(&l)),
            Request::SetMode { mode, expected_revision } => supervisor.set_mode(mode, expected_revision).map(|s| to_value(&s)),
            Request::SetMaxConcurrentTasks { value, expected_revision } => {
                supervisor.set_max_concurrent_tasks(value, expected_revision).map(|s| to_value(&s))
            }
            Request::ActiveTasks => Ok(to_value(&supervisor.status().active_tasks)),
            Request::Events { since } => supervisor.events_since(since).map(|e| to_value(&e)),
        }
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn respond(id: Option<u64>, result: Result<Value, ServiceError>) -> String {
    let body = match result {
        Ok(result) => json!({ "apiVersion": API_VERSION, "id": id, "ok": true, "result": result }),
        Err(error) => json!({ "apiVersion": API_VERSION, "id": id, "ok": false, "error": ErrorBody::from(&error) }),
    };
    body.to_string()
}

// MARK: Socket

/// Prepares the socket's directory (created 0700, must belong to us) and refuses to replace the
/// socket of a service that still answers.
pub fn bind(socket: &Path) -> std::io::Result<UnixListener> {
    let directory = socket.parent().ok_or_else(|| std::io::Error::other("socket path has no directory"))?;
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::metadata(directory)?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the socket directory belongs to another user",
        ));
    }
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "another service answers on this socket"));
        }
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The user id of the process at the other end of the socket, from the kernel.
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (result == 0).then_some(uid)
}

/// Only the user running the service may use it. `None` (the kernel could not tell) is refused.
pub fn authorize(peer: Option<u32>, owner: u32) -> bool {
    peer == Some(owner)
}

/// Accepts connections until the listener fails. Each connection gets its own thread; requests on
/// a connection are answered in order.
pub fn serve(listener: UnixListener, service: Arc<Service>) {
    let open = Arc::new(AtomicUsize::new(0));
    let owner = unsafe { libc::geteuid() };
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if !authorize(peer_uid(&stream), owner) {
            let _ = writeln!(stream, "{}", respond(None, Err(ServiceError::PermissionDenied)));
            continue;
        }
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            let _ = writeln!(stream, "{}", respond(None, Err(ServiceError::Unavailable("Too many clients.".into()))));
            continue;
        }
        let service = Arc::clone(&service);
        let open = Arc::clone(&open);
        std::thread::spawn(move || {
            let _ = handle_connection(stream, &service);
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

pub fn handle_connection(stream: UnixStream, service: &Service) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        let read = (&mut reader).take(MAX_LINE_BYTES).read_line(&mut line)?;
        if read == 0 {
            return Ok(());
        }
        if !line.ends_with('\n') && read as u64 >= MAX_LINE_BYTES {
            let error = ServiceError::InvalidArgument { field: "request".into(), reason: "request too large".into() };
            writeln!(writer, "{}", respond(None, Err(error)))?;
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        writeln!(writer, "{}", service.handle_line(line.trim_end()))?;
    }
}

/// One service per data directory: an exclusive `flock` held for the life of the process.
pub struct InstanceLock {
    _file: File,
    pub path: PathBuf,
}

impl InstanceLock {
    pub fn acquire(directory: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join("service.lock");
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "another service already runs for this data directory",
            ));
        }
        Ok(InstanceLock { _file: file, path })
    }
}

/// A minimal client, for tests and the command line.
pub fn call(socket: &Path, request: &Value) -> std::io::Result<Value> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}
