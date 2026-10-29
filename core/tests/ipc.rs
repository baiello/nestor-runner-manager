//! The local API over a real Unix socket (contract KCA-717 §4 and §11.4).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nestor_runner_core::fake::{FakeLauncher, UnavailableLauncher};
use nestor_runner_core::ipc::{self, authorize, InstanceLock, Service};
use nestor_runner_core::model::ServiceMode;
use nestor_runner_core::persist::MemoryStore;
use nestor_runner_core::supervisor::{Clock, Launcher, Options, Supervisor};
use serde_json::{json, Value};

fn start(mode: ServiceMode) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run/service.sock");
    let time = Arc::new(AtomicU64::new(1_000));
    let clock: Clock = Arc::new(move || time.load(Ordering::SeqCst));
    let launcher: Arc<dyn Launcher> = match mode {
        ServiceMode::Demo => Arc::new(FakeLauncher::new()),
        ServiceMode::Live => Arc::new(UnavailableLauncher),
    };
    let supervisor =
        Arc::new(Supervisor::open(Arc::new(MemoryStore::default()), launcher, clock.clone(), Options::new(mode, 4)));
    supervisor.reconcile();
    let service = Arc::new(Service::new(supervisor, clock));
    let listener = ipc::bind(&socket).unwrap();
    std::thread::spawn(move || ipc::serve(listener, service));
    (dir, socket)
}

fn call(socket: &std::path::Path, op: &str, extra: Value) -> Value {
    let mut request = json!({ "apiVersion": "1.0", "id": 1, "op": op });
    request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    ipc::call(socket, &request).unwrap()
}

#[test]
fn socket_is_private_to_its_owner() {
    let (dir, socket) = start(ServiceMode::Live);
    assert_eq!(fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(dir.path().join("run")).unwrap().permissions().mode() & 0o777, 0o700);
    // The peer check: only the owner's uid passes; an unknown peer is refused.
    let me = unsafe { libc::geteuid() };
    assert!(authorize(Some(me), me));
    assert!(!authorize(Some(me + 1), me));
    assert!(!authorize(None, me));
    // A connection from this user is accepted and identified by the kernel.
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    assert_eq!(ipc::peer_uid(&stream), Some(me));
}

#[test]
fn a_second_service_cannot_take_the_socket_or_the_lock() {
    let (dir, socket) = start(ServiceMode::Live);
    let error = ipc::bind(&socket).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    let _first = InstanceLock::acquire(dir.path()).unwrap();
    assert!(InstanceLock::acquire(dir.path()).is_err());
}

#[test]
fn live_service_reports_live_and_refuses_to_pretend_an_association() {
    let (_dir, socket) = start(ServiceMode::Live);
    let hello = call(&socket, "hello", json!({}));
    assert_eq!(hello["result"]["serviceMode"], "live");
    let status = call(&socket, "machineStatus", json!({}));
    assert_eq!(status["ok"], true);
    assert_eq!(status["result"]["serviceMode"], "live");
    assert_eq!(status["result"]["maxConcurrentTasks"], 1);
    assert_eq!(status["result"]["admission"], json!({ "state": "blocked", "reason": "notAssociated" }));
    let text = status["result"].to_string().to_lowercase();
    assert!(!text.contains("limit") && !text.contains("quota"), "no limit or quota field: {text}");

    let associate = call(&socket, "associate", json!({}));
    assert_eq!(associate["ok"], false);
    assert_eq!(associate["error"]["code"], "unavailable");
}

#[test]
fn settings_round_trip_with_revisions_and_errors() {
    let (_dir, socket) = start(ServiceMode::Demo);
    let status = call(&socket, "machineStatus", json!({}));
    assert_eq!(status["result"]["serviceMode"], "demo");
    let revision = status["result"]["configRevision"].as_u64().unwrap();

    let paused = call(&socket, "setMode", json!({ "mode": "paused", "expectedRevision": revision }));
    assert_eq!(paused["result"]["mode"], "paused");
    let conflict = call(&socket, "setMaxConcurrentTasks", json!({ "value": 2, "expectedRevision": revision }));
    assert_eq!(conflict["error"]["code"], "conflict");
    assert_eq!(conflict["error"]["retryable"], true);
    assert_eq!(conflict["error"]["details"]["currentRevision"], revision + 1);
    let out_of_range = call(&socket, "setMaxConcurrentTasks", json!({ "value": 99 }));
    assert_eq!(out_of_range["error"]["code"], "limitOutOfRange");
    assert_eq!(out_of_range["error"]["details"], json!({ "min": 1, "max": 4 }));

    let associated = call(&socket, "associate", json!({}));
    assert_eq!(associated["result"]["instance"], "demo.nestor.invalid");
    let added = call(&socket, "addRunner", json!({ "name": "diag", "startNow": false }));
    assert_eq!(added["result"]["state"], "stopped");
    let duplicate = call(&socket, "addRunner", json!({ "name": "DIAG" }));
    assert_eq!(duplicate["error"]["code"], "duplicateName");
    let runner_id = added["result"]["id"].as_str().unwrap().to_string();
    let logs = call(&socket, "logs", json!({ "runnerId": runner_id, "limit": 10 }));
    assert!(logs["result"][0]["message"].as_str().unwrap().starts_with("[demo]"));
    let removed = call(&socket, "remove", json!({ "runnerId": runner_id }));
    assert_eq!(removed["ok"], true);
    assert_eq!(call(&socket, "runners", json!({}))["result"], json!([]));
    let events = call(&socket, "events", json!({ "since": 0 }));
    assert!(events["result"].as_array().unwrap().iter().any(|e| e["type"] == "machine.configChanged"));
}

#[test]
fn malformed_and_incompatible_requests_are_refused() {
    let (_dir, socket) = start(ServiceMode::Live);
    let wrong_version = ipc::call(&socket, &json!({ "apiVersion": "2.0", "id": 5, "op": "machineStatus" })).unwrap();
    assert_eq!(wrong_version["error"]["code"], "incompatibleVersion");
    assert_eq!(wrong_version["id"], 5);
    let unknown = call(&socket, "format", json!({}));
    assert_eq!(unknown["error"]["code"], "invalidArgument");
    let bad_mode = call(&socket, "setMode", json!({ "mode": "turbo" }));
    assert_eq!(bad_mode["error"]["code"], "invalidArgument");
}
