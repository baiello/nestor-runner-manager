//! The local API: JSON lines over a Unix domain socket.
//!
//! See `docs/local-api.md`. Every request carries `apiVersion`; another major version gets
//! `incompatibleVersion`. There is no TCP listener.
//!
//! **Who may do what (B2, A4).** Each connection is classified once, from the kernel:
//!
//! 1. The peer's user id must be the service's (`getpeereid`); otherwise refused.
//! 2. A peer that is a live runner or one of its descendants is refused.
//! 3. The peer's **audit token** (not its pid, which can be reused) is checked against the code
//!    signing requirements of the app and of the command line tool, and must have the hardened
//!    runtime without `get-task-allow` (so nothing of the same user can inject code into it).
//!    The app gets `full` access. The command line tool gets `readOnly` access, without runner
//!    logs, in this version (A4: a task can run the signed CLI too).
//! 4. Anything else gets `basic` access (`hello`, `machineInfo`, `machineStatus` without task
//!    references, `connection`), unless the service was started with the explicit
//!    `--allow-unsigned-clients` flag for development, which gives such peers `full` access and
//!    is reported by `hello` (`peerVerification: "unsignedFallback"`) so the interface shows it.
//!
//! Sensitive operations (`disassociate`, raising the ceiling, `addRunner`, a forced `stop`) also
//! need `confirmed: true`, which the interface sends only after asking the user.
//!
//! The socket lives in a directory of the user's own (`~/Library/Application Support/…`, never a
//! temporary directory), whose owner and permissions are checked before `bind`; it is removed
//! when the service stops cleanly. Clients check that the process serving the socket runs as
//! their own user, so a fake service cannot pass for the real one.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::association::Associator;
use crate::load::LoadSampler;
use crate::model::*;
use crate::supervisor::Supervisor;
use crate::sys;

pub const API_VERSION: &str = "1.1";
pub const API_MAJOR: u32 = 1;
const MAX_LINE_BYTES: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 16;

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
    CancelAssociation,
    Disassociate {
        #[serde(default)]
        confirmed: bool,
    },
    Runners,
    #[serde(rename_all = "camelCase")]
    AddRunner {
        name: String,
        #[serde(default)]
        start_now: bool,
        #[serde(default)]
        confirmed: bool,
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
        #[serde(default)]
        confirmed: bool,
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
        #[serde(default)]
        confirmed: bool,
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

/// What a connection may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    Basic,
    ReadOnly,
    Full,
}

/// How the peer was identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerVerification {
    /// Code signature of the app or the CLI, hardened runtime, no `get-task-allow`.
    Signature,
    /// Not verified; given full access because the service runs with `--allow-unsigned-clients`.
    UnsignedFallback,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub access: Access,
    pub verification: PeerVerification,
}

#[derive(Debug, Clone, Default)]
pub struct PeerPolicy {
    /// Code signing requirement of the macOS app.
    pub app_requirement: Option<String>,
    /// Code signing requirement of the command line tool.
    pub cli_requirement: Option<String>,
    /// Development only, off by default: unverified peers of the same user get full access.
    pub allow_unsigned_clients: bool,
}

impl Request {
    fn needs(&self) -> Access {
        match self {
            Request::Hello | Request::MachineInfo | Request::MachineStatus | Request::Connection => Access::Basic,
            Request::Runners | Request::ActiveTasks | Request::Events { .. } => Access::ReadOnly,
            _ => Access::Full,
        }
    }
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

impl ErrorBody {
    fn from(error: &ServiceError, redactor: Option<&crate::mask::Redactor>) -> Self {
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
        let message = match redactor {
            Some(redactor) => redactor.mask(&error.to_string()),
            None => error.to_string(),
        };
        ErrorBody { code: error.code(), message, retryable: error.retryable(), details }
    }
}

/// What the IPC server exposes: the supervisor, the association and the machine description.
pub struct Service {
    pub supervisor: Arc<Supervisor>,
    pub associator: Arc<Associator>,
    sampler: Mutex<LoadSampler>,
    clock: crate::supervisor::Clock,
    pub policy: PeerPolicy,
}

impl Service {
    pub fn new(
        supervisor: Arc<Supervisor>,
        associator: Arc<Associator>,
        clock: crate::supervisor::Clock,
        policy: PeerPolicy,
    ) -> Self {
        Service { supervisor, associator, sampler: Mutex::new(LoadSampler::new()), clock, policy }
    }

    pub fn mode(&self) -> ServiceMode {
        self.supervisor.options().service_mode
    }

    /// Takes a load sample and hands it to the supervisor.
    pub fn sample_load(&self) {
        let sample = self.sampler.lock().unwrap().sample((self.clock)());
        self.supervisor.set_load(sample);
    }

    /// Classifies a connection (see the module documentation).
    pub fn classify(&self, stream: &UnixStream) -> Option<Peer> {
        let owner = unsafe { libc::geteuid() };
        if sys::peer_uid(stream) != Some(owner) {
            return None;
        }
        let token = sys::peer_audit_token(stream);
        if let Some(token) = &token {
            let pid = token.pid();
            if self.supervisor.runner_pids().into_iter().any(|runner| sys::descends_from(pid, runner)) {
                return None;
            }
        }
        Some(self.classify_signature(token.as_ref()))
    }

    #[cfg(target_os = "macos")]
    fn classify_signature(&self, token: Option<&sys::AuditToken>) -> Peer {
        if let Some(token) = token {
            for (requirement, access) in
                [(&self.policy.app_requirement, Access::Full), (&self.policy.cli_requirement, Access::ReadOnly)]
            {
                let Some(requirement) = requirement else { continue };
                if let Ok(signature) = sys::check_peer_signature(token, requirement) {
                    if signature.satisfies && signature.hardened_runtime && !signature.get_task_allow {
                        return Peer { access, verification: PeerVerification::Signature };
                    }
                }
            }
        }
        self.unverified()
    }

    #[cfg(not(target_os = "macos"))]
    fn classify_signature(&self, _token: Option<&sys::AuditToken>) -> Peer {
        self.unverified()
    }

    fn unverified(&self) -> Peer {
        if self.policy.allow_unsigned_clients {
            Peer { access: Access::Full, verification: PeerVerification::UnsignedFallback }
        } else {
            Peer { access: Access::Basic, verification: PeerVerification::None }
        }
    }

    /// Handles one JSON request line from `peer` and returns the response line.
    pub fn handle_line(&self, line: &str, peer: Peer) -> String {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                let error = ServiceError::InvalidArgument { field: "request".into(), reason: e.to_string() };
                return self.respond(None, Err(error));
            }
        };
        let id = value.get("id").and_then(Value::as_u64);
        let major = value
            .get("apiVersion")
            .and_then(Value::as_str)
            .and_then(|v| v.split('.').next())
            .and_then(|m| m.parse::<u32>().ok());
        if major != Some(API_MAJOR) {
            return self.respond(id, Err(ServiceError::IncompatibleVersion { supported: API_VERSION.into() }));
        }
        let envelope: Envelope = match serde_json::from_value(value) {
            Ok(envelope) => envelope,
            Err(e) => {
                return self
                    .respond(id, Err(ServiceError::InvalidArgument { field: "request".into(), reason: e.to_string() }))
            }
        };
        if envelope.request.needs() > peer.access {
            return self.respond(id, Err(ServiceError::PermissionDenied));
        }
        self.respond(id, self.handle(envelope.request, peer))
    }

    pub fn handle(&self, request: Request, peer: Peer) -> Result<Value, ServiceError> {
        let supervisor = &self.supervisor;
        let confirm = |confirmed: bool, what: &str| {
            if confirmed {
                Ok(())
            } else {
                Err(ServiceError::ConfirmationRequired(what.into()))
            }
        };
        match request {
            Request::Hello => Ok(json!({
                "apiVersion": API_VERSION,
                "serviceMode": self.mode(),
                "access": peer.access,
                "peerVerification": peer.verification,
                "keyProtection": self.associator.key_protection(),
            })),
            Request::MachineInfo => Ok(to_value(&self.sampler.lock().unwrap().machine_info())),
            Request::MachineStatus => {
                let mut status = supervisor.status();
                if peer.access < Access::ReadOnly {
                    // Unverified peers see counts, not task references.
                    status.active_tasks.clear();
                }
                Ok(to_value(&status))
            }
            Request::Connection => Ok(to_value(&supervisor.connection())),
            Request::Associate => {
                let machine = self.sampler.lock().unwrap().machine_info();
                self.associator.associate(&machine).map(|c| to_value(&c))
            }
            Request::CancelAssociation => Ok(to_value(&self.associator.cancel())),
            Request::Disassociate { confirmed } => {
                confirm(confirmed, "disassociating ends the tasks running on this machine")?;
                Ok(to_value(&self.associator.disassociate()))
            }
            Request::Runners => Ok(to_value(&supervisor.runners())),
            Request::AddRunner { name, start_now, confirmed } => {
                confirm(confirmed, "adding a runner by hand")?;
                supervisor.add_runner(&name, start_now).map(|r| to_value(&r))
            }
            Request::Start { runner_id } => supervisor.start_runner(&checked_id(&runner_id)?).map(|r| to_value(&r)),
            Request::Stop { runner_id, force, confirmed } => {
                if force {
                    confirm(confirmed, "a forced stop ends the runner's task")?;
                }
                supervisor.stop_runner(&checked_id(&runner_id)?, force).map(|r| to_value(&r))
            }
            Request::Remove { runner_id } => supervisor.remove_runner(&checked_id(&runner_id)?).map(|_| json!({})),
            Request::Logs { runner_id, limit } => {
                supervisor.logs(&checked_id(&runner_id)?, limit.min(1_000)).map(|l| to_value(&l))
            }
            Request::SetMode { mode, expected_revision } => {
                supervisor.set_mode(mode, expected_revision).map(|s| to_value(&s))
            }
            Request::SetMaxConcurrentTasks { value, expected_revision, confirmed } => {
                if value > supervisor.max_concurrent_tasks() {
                    confirm(confirmed, "running more tasks at the same time")?;
                }
                supervisor.set_max_concurrent_tasks(value, expected_revision).map(|s| to_value(&s))
            }
            Request::ActiveTasks => Ok(to_value(&supervisor.status().active_tasks)),
            Request::Events { since } => supervisor.events_since(since).map(|e| to_value(&e)),
        }
    }

    fn respond(&self, id: Option<u64>, result: Result<Value, ServiceError>) -> String {
        respond_with(id, result, Some(self.supervisor.redactor()))
    }
}

fn checked_id(raw: &str) -> Result<String, ServiceError> {
    if is_runner_id(raw) {
        Ok(raw.to_string())
    } else {
        Err(ServiceError::RunnerNotFound)
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn respond_with(
    id: Option<u64>,
    result: Result<Value, ServiceError>,
    redactor: Option<&crate::mask::Redactor>,
) -> String {
    let body = match result {
        Ok(result) => json!({ "apiVersion": API_VERSION, "id": id, "ok": true, "result": result }),
        Err(error) => {
            json!({ "apiVersion": API_VERSION, "id": id, "ok": false, "error": ErrorBody::from(&error, redactor) })
        }
    };
    body.to_string()
}

// MARK: Socket

/// The listening socket. Dropping it removes the socket file, if it is still the one it bound.
#[derive(Debug)]
pub struct BoundSocket {
    pub listener: UnixListener,
    pub path: PathBuf,
    inode: u64,
}

impl BoundSocket {
    /// Removes the socket file if it is still ours (same inode). Called on clean shutdown.
    pub fn remove_file(path: &Path, inode: u64) {
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket() && m.ino() == inode) {
            let _ = fs::remove_file(path);
        }
    }

    pub fn inode(&self) -> u64 {
        self.inode
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        Self::remove_file(&self.path, self.inode);
    }
}

fn denied(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message.to_string())
}

/// Whether `path` is in a shared temporary directory, where another user could prepare it.
fn in_shared_temp(path: &Path) -> bool {
    ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp"].iter().any(|t| path.starts_with(t))
}

/// Prepares the socket's directory and binds. The directory must be a real directory (not a
/// link) owned by this user and closed to others (it is made `0700` if needed, never taken over
/// from someone else); an existing socket is replaced only if it is ours and nothing answers on
/// it. Refuses shared temporary directories.
pub fn bind(socket: &Path) -> std::io::Result<BoundSocket> {
    let directory = socket.parent().ok_or_else(|| std::io::Error::other("socket path has no directory"))?;
    if !socket.is_absolute() || in_shared_temp(socket) {
        return Err(denied("the socket must be in a directory of the user's own, not a shared temporary one"));
    }
    if !directory.exists() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
    }
    let metadata = fs::symlink_metadata(directory)?;
    let me = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() {
        return Err(denied("the socket directory is not a directory"));
    }
    if metadata.uid() != me {
        return Err(denied("the socket directory belongs to another user"));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    if let Ok(existing) = fs::symlink_metadata(socket) {
        if !existing.file_type().is_socket() || existing.uid() != me {
            return Err(denied("something else is at the socket path"));
        }
        if UnixStream::connect(socket).is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "another service answers on this socket"));
        }
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    let inode = fs::symlink_metadata(socket)?.ino();
    Ok(BoundSocket { listener, path: socket.to_path_buf(), inode })
}

/// The user id of the process at the other end of the socket, from the kernel.
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
    sys::peer_uid(stream)
}

/// Accepts connections until the listener fails. Each connection is classified, then gets its
/// own thread; requests on a connection are answered in order.
pub fn serve(listener: &UnixListener, service: Arc<Service>) {
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let Some(peer) = service.classify(&stream) else {
            let _ = writeln!(stream, "{}", respond_with(None, Err(ServiceError::PermissionDenied), None));
            continue;
        };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            let _ = writeln!(
                stream,
                "{}",
                respond_with(None, Err(ServiceError::Unavailable("Too many clients.".into())), None)
            );
            continue;
        }
        let service = Arc::clone(&service);
        let open = Arc::clone(&open);
        std::thread::spawn(move || {
            let _ = handle_connection(stream, &service, peer);
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

pub fn handle_connection(stream: UnixStream, service: &Service, peer: Peer) -> std::io::Result<()> {
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
            writeln!(writer, "{}", respond_with(None, Err(error), None))?;
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        writeln!(writer, "{}", service.handle_line(line.trim_end(), peer))?;
    }
}

/// One service per data directory: an exclusive `flock` on an open descriptor, held for the life
/// of the process and released by the kernel if it crashes (B4).
pub struct InstanceLock {
    _file: File,
    pub path: PathBuf,
}

impl InstanceLock {
    pub fn acquire(directory: &Path) -> std::io::Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
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

/// Connects to the service, refusing a socket served by another user (a fake service could
/// otherwise announce `live`).
pub fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    if peer_uid(&stream) != Some(unsafe { libc::geteuid() }) {
        return Err(denied("the process serving this socket runs as another user"));
    }
    Ok(stream)
}

/// A minimal client, for tests and the command line.
pub fn call(socket: &Path, request: &Value) -> std::io::Result<Value> {
    let stream = connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}
