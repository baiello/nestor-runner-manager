//! The local API over a real Unix socket (contract v2 §4.1, S8.3, S8.6, A4, N5; QA KCA-726).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nestor_runner_core::association::Associator;
use nestor_runner_core::demo::DEMO_INSTANCE;
use nestor_runner_core::fake::{FakeLauncher, UnavailableLauncher};
use nestor_runner_core::ipc::{self, InstanceLock, PeerPolicy, Service};
use nestor_runner_core::mask::Redactor;
use nestor_runner_core::model::ServiceMode;
use nestor_runner_core::nestor::{FakeNestor, NestorApi};
use nestor_runner_core::persist::MemoryStore;
use nestor_runner_core::secrets::stores_for;
use nestor_runner_core::supervisor::{Clock, Launcher, Options, Supervisor};
use serde_json::{json, Value};

/// A temporary directory outside the shared temporary directories, which the service refuses.
fn private_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("t").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

struct Running {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    nestor: Arc<FakeNestor>,
    supervisor: Arc<Supervisor>,
    associator: Arc<Associator>,
    time: Arc<AtomicU64>,
}

fn start(mode: ServiceMode, policy: PeerPolicy) -> Running {
    let dir = private_dir();
    let socket = dir.path().join("r/s.sock");
    let time = Arc::new(AtomicU64::new(1_000));
    let clock_time = time.clone();
    let clock: Clock = Arc::new(move || clock_time.load(Ordering::SeqCst));
    let nestor = Arc::new(FakeNestor::new(DEMO_INSTANCE).auto_approve());
    let (launcher, api): (Arc<dyn Launcher>, Option<Arc<dyn NestorApi>>) = match mode {
        ServiceMode::Demo => (Arc::new(FakeLauncher::new()), Some(nestor.clone())),
        ServiceMode::Live => (Arc::new(UnavailableLauncher), None),
    };
    let supervisor = Arc::new(Supervisor::open(
        Arc::new(MemoryStore::default()),
        launcher,
        clock.clone(),
        Options::new(mode, 4),
        Redactor::new(),
    ));
    supervisor.reconcile();
    // Never the Keychain in tests: the demo stores, even for a "live" service under test.
    let (secrets, keys) = stores_for(ServiceMode::Demo, None);
    let associator = Arc::new(Associator::new(api, keys, secrets, supervisor.clone(), clock.clone()));
    let service = Arc::new(Service::new(supervisor.clone(), associator.clone(), clock, policy));
    let bound = ipc::bind(&socket).unwrap();
    std::thread::spawn(move || {
        ipc::serve(&bound.listener, service);
        drop(bound);
    });
    Running { _dir: dir, socket, nestor, supervisor, associator, time }
}

fn unsigned_allowed() -> PeerPolicy {
    PeerPolicy { allow_unsigned_clients: true, ..PeerPolicy::default() }
}

fn call(socket: &Path, op: &str, extra: Value) -> Value {
    let mut request = json!({ "apiVersion": "1.1", "id": 1, "op": op });
    request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    ipc::call(socket, &request).unwrap()
}

#[test]
fn socket_is_private_and_removed_when_the_service_stops() {
    let dir = private_dir();
    let socket = dir.path().join("r/s.sock");
    {
        let bound = ipc::bind(&socket).unwrap();
        assert_eq!(fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(dir.path().join("r")).unwrap().permissions().mode() & 0o777, 0o700);
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        assert_eq!(ipc::peer_uid(&stream), Some(unsafe { libc::geteuid() }));
        drop(bound);
    }
    assert!(!socket.exists(), "the socket is removed on a clean stop");
}

#[test]
fn the_socket_directory_is_checked_before_bind() {
    // Shared temporary directories are refused.
    assert_eq!(
        ipc::bind(Path::new("/tmp/nestor-test/service.sock")).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(ipc::bind(Path::new("relative/service.sock")).is_err());
    // A link in place of the directory is refused.
    let dir = private_dir();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
    assert_eq!(ipc::bind(&dir.path().join("link/s.sock")).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
    // An existing directory open to others is closed (it is ours).
    fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
    let _bound = ipc::bind(&real.join("s.sock")).unwrap();
    assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o700);
    // Something else than a socket at the path is never removed.
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("s.sock"), b"keep me").unwrap();
    assert!(ipc::bind(&other.join("s.sock")).is_err());
    assert_eq!(fs::read(other.join("s.sock")).unwrap(), b"keep me");
}

#[test]
fn a_second_service_cannot_take_the_socket_or_the_lock() {
    let running = start(ServiceMode::Live, PeerPolicy::default());
    let error = ipc::bind(&running.socket).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    let dir = private_dir();
    let _first = InstanceLock::acquire(dir.path()).unwrap();
    assert!(InstanceLock::acquire(dir.path()).is_err());
}

#[test]
fn an_unverified_client_gets_basic_read_access_only() {
    let running = start(ServiceMode::Demo, PeerPolicy::default());
    let hello = call(&running.socket, "hello", json!({}));
    assert_eq!(hello["result"]["access"], "basic");
    assert_eq!(hello["result"]["peerVerification"], "none");
    assert_eq!(call(&running.socket, "machineStatus", json!({}))["ok"], true);
    for (op, extra) in [
        ("setMode", json!({ "mode": "paused" })),
        ("setMaxConcurrentTasks", json!({ "value": 1 })),
        ("associate", json!({})),
        ("disassociate", json!({ "confirmed": true })),
        ("runners", json!({})),
        ("activeTasks", json!({})),
        ("logs", json!({ "runnerId": "0123456789abcdef0123456789abcdef" })),
    ] {
        let answer = call(&running.socket, op, extra);
        assert_eq!(answer["error"]["code"], "permissionDenied", "{op}: {answer}");
    }
    assert_eq!(running.supervisor.status().config_revision, 0, "nothing changed");
}

#[test]
fn the_unsigned_fallback_is_explicit_and_reported() {
    let running = start(ServiceMode::Demo, unsigned_allowed());
    let hello = call(&running.socket, "hello", json!({}));
    assert_eq!(hello["result"]["access"], "full");
    assert_eq!(hello["result"]["peerVerification"], "unsignedFallback");
    assert_eq!(hello["result"]["serviceMode"], "demo");
    assert_eq!(hello["result"]["keyProtection"], "memory");
}

#[test]
fn live_service_reports_live_and_refuses_to_pretend_an_association() {
    let running = start(ServiceMode::Live, unsigned_allowed());
    let status = call(&running.socket, "machineStatus", json!({}));
    assert_eq!(status["result"]["serviceMode"], "live");
    assert_eq!(status["result"]["maxConcurrentTasks"], 1);
    assert_eq!(status["result"]["admission"], json!({ "state": "blocked", "reason": "notAssociated" }));
    let text = status["result"].to_string().to_lowercase();
    assert!(!text.contains("limit") && !text.contains("quota"), "no limit or quota field: {text}");
    let associate = call(&running.socket, "associate", json!({}));
    assert_eq!(associate["error"]["code"], "unavailable");
    assert_eq!(call(&running.socket, "connection", json!({}))["result"]["state"], "notAssociated");
}

#[test]
fn sensitive_operations_need_a_confirmation() {
    let running = start(ServiceMode::Demo, unsigned_allowed());
    let associating = call(&running.socket, "associate", json!({}));
    assert_eq!(associating["result"]["state"], "associating");
    let code = associating["result"]["userCode"].as_str().unwrap().to_string();
    assert!(running.nestor.approve(&code));
    // The next poll, after the server's interval, gets the approval.
    running.time.fetch_add(6_000, Ordering::SeqCst);
    running.supervisor.tick(&running.associator.tick());
    // M1: the owner confirms the account Nestor gave, on the machine, before any runner.
    let confirming = call(&running.socket, "connection", json!({}))["result"].clone();
    assert_eq!(confirming["state"], "confirming");
    assert_eq!(confirming["accountEmail"], "o***r@example.com");
    assert_eq!(confirming["approvedByEmail"], "a***n@example.com");
    let accept = call(&running.socket, "confirmAssociation", json!({ "accept": true }));
    assert_eq!(accept["error"]["code"], "confirmationRequired");
    let accepted = call(&running.socket, "confirmAssociation", json!({ "accept": true, "confirmed": true }));
    assert_eq!(accepted["result"]["state"], "associated");
    assert_eq!(call(&running.socket, "connection", json!({}))["result"]["state"], "associated");

    let raise = call(&running.socket, "setMaxConcurrentTasks", json!({ "value": 3 }));
    assert_eq!(raise["error"]["code"], "confirmationRequired");
    assert_eq!(call(&running.socket, "setMaxConcurrentTasks", json!({ "value": 3, "confirmed": true }))["ok"], true);
    // Lowering needs no confirmation: it interrupts nothing.
    assert_eq!(call(&running.socket, "setMaxConcurrentTasks", json!({ "value": 2 }))["ok"], true);
    assert_eq!(call(&running.socket, "addRunner", json!({ "name": "diag" }))["error"]["code"], "confirmationRequired");
    let added = call(&running.socket, "addRunner", json!({ "name": "diag", "confirmed": true }));
    assert_eq!(added["result"]["state"], "stopped");
    assert_eq!(call(&running.socket, "disassociate", json!({}))["error"]["code"], "confirmationRequired");
    let disassociated = call(&running.socket, "disassociate", json!({ "confirmed": true }));
    assert_eq!(disassociated["result"]["state"], "notAssociated");
}

#[test]
fn settings_round_trip_with_revisions_and_errors() {
    let running = start(ServiceMode::Demo, unsigned_allowed());
    let status = call(&running.socket, "machineStatus", json!({}));
    let revision = status["result"]["configRevision"].as_u64().unwrap();
    let paused = call(&running.socket, "setMode", json!({ "mode": "paused", "expectedRevision": revision }));
    assert_eq!(paused["result"]["mode"], "paused");
    let conflict = call(&running.socket, "setMaxConcurrentTasks", json!({ "value": 1, "expectedRevision": revision }));
    assert_eq!(conflict["error"]["code"], "conflict");
    assert_eq!(conflict["error"]["details"]["currentRevision"], revision + 1);
    let out_of_range = call(&running.socket, "setMaxConcurrentTasks", json!({ "value": 99, "confirmed": true }));
    assert_eq!(out_of_range["error"]["details"], json!({ "min": 1, "max": 4 }));
    assert_eq!(call(&running.socket, "logs", json!({ "runnerId": "../x" }))["error"]["code"], "runnerNotFound");
    let events = call(&running.socket, "events", json!({ "since": 0 }));
    assert!(events["result"].as_array().unwrap().iter().any(|e| e["type"] == "machine.configChanged"));
}

#[test]
fn malformed_and_incompatible_requests_are_refused() {
    let running = start(ServiceMode::Live, unsigned_allowed());
    let wrong = ipc::call(&running.socket, &json!({ "apiVersion": "2.0", "id": 5, "op": "machineStatus" })).unwrap();
    assert_eq!(wrong["error"]["code"], "incompatibleVersion");
    assert_eq!(wrong["id"], 5);
    assert_eq!(call(&running.socket, "format", json!({}))["error"]["code"], "invalidArgument");
    assert_eq!(call(&running.socket, "setMode", json!({ "mode": "turbo" }))["error"]["code"], "invalidArgument");
    // A request line over 64 KiB closes the connection with an error.
    let big = format!(r#"{{"apiVersion":"1.1","op":"hello","pad":"{}"}}"#, "x".repeat(70_000));
    let stream = ipc::connect(&running.socket).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let _ = writeln!(writer, "{big}");
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert!(line.contains("request too large"), "{line}");
}

#[test]
fn error_messages_are_masked() {
    let running = start(ServiceMode::Live, unsigned_allowed());
    running.supervisor.redactor().register("nestor_machine_zzzzzzzzzzzzzzzz");
    running.supervisor.report_error("upload failed with nestor_machine_zzzzzzzzzzzzzzzz");
    let events = call(&running.socket, "events", json!({ "since": 0 }));
    assert!(!events.to_string().contains("zzzzzzzz"), "{events}");
}

/// A copy of Apple's `nc`, re-signed ad hoc, as a client whose signature can be checked.
#[cfg(target_os = "macos")]
fn signed_client(dir: &Path, name: &str, runtime: bool, entitlement: Option<&str>) -> (PathBuf, String) {
    let path = dir.join(name);
    fs::copy("/usr/bin/nc", &path).unwrap();
    let mut command = Command::new("/usr/bin/codesign");
    command.args(["--force", "--sign", "-", "--identifier", name]);
    if runtime {
        command.args(["--options", "runtime"]);
    }
    if let Some(entitlement) = entitlement {
        let entitlements = dir.join(format!("{name}.plist"));
        fs::write(
            &entitlements,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>{entitlement}</key><true/></dict></plist>"#
            ),
        )
        .unwrap();
        command.arg("--entitlements").arg(&entitlements);
    }
    assert!(command.arg(&path).status().unwrap().success());
    let output = Command::new("/usr/bin/codesign").args(["-d", "-r-"]).arg(&path).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout).to_string() + &String::from_utf8_lossy(&output.stderr);
    let requirement =
        text.lines().find_map(|l| l.trim_start_matches("# ").strip_prefix("designated => ")).unwrap().to_string();
    (path, requirement)
}

#[cfg(target_os = "macos")]
fn hello_from(client: &Path, socket: &Path) -> Value {
    let mut child =
        Command::new(client).arg("-U").arg(socket).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    writeln!(child.stdin.as_mut().unwrap(), r#"{{"apiVersion":"1.1","id":1,"op":"hello"}}"#).unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    serde_json::from_str(&line).unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn clients_are_identified_by_code_signature_from_the_audit_token() {
    let dir = private_dir();
    let (app, app_requirement) = signed_client(dir.path(), "app-client", true, None);
    let (cli, cli_requirement) = signed_client(dir.path(), "cli-client", true, None);
    let (debuggable, debuggable_requirement) =
        signed_client(dir.path(), "debuggable-client", true, Some("com.apple.security.get-task-allow"));
    let (no_runtime, no_runtime_requirement) = signed_client(dir.path(), "plain-client", false, None);
    let (exception, exception_requirement) =
        signed_client(dir.path(), "exception-client", true, Some("com.apple.security.cs.disable-library-validation"));

    let policy = |app: &str, cli: &str| PeerPolicy {
        app_requirement: Some(app.to_string()),
        cli_requirement: Some(cli.to_string()),
        allow_unsigned_clients: false,
    };
    let running = start(ServiceMode::Demo, policy(&app_requirement, &cli_requirement));
    let answer = hello_from(&app, &running.socket);
    assert_eq!(answer["result"]["access"], "full", "{answer}");
    assert_eq!(answer["result"]["peerVerification"], "signature");
    assert_eq!(hello_from(&cli, &running.socket)["result"]["access"], "readOnly", "the CLI is read-only (A4)");
    // The test process itself is not signed for either requirement.
    assert_eq!(call(&running.socket, "hello", json!({}))["result"]["access"], "basic");

    // Satisfying the requirement is not enough without the hardened runtime, or with
    // get-task-allow: a process of the same user could inject code into it.
    let running = start(ServiceMode::Demo, policy(&debuggable_requirement, &no_runtime_requirement));
    assert_eq!(hello_from(&debuggable, &running.socket)["result"]["access"], "basic");
    assert_eq!(hello_from(&no_runtime, &running.socket)["result"]["access"], "basic");

    // Nor with a hardened runtime exception (F1): code could be loaded into it.
    let running = start(ServiceMode::Demo, policy(&exception_requirement, &cli_requirement));
    assert_eq!(hello_from(&exception, &running.socket)["result"]["access"], "basic");
}

#[cfg(target_os = "macos")]
#[test]
fn an_ad_hoc_signature_does_not_meet_the_developer_id_requirement() {
    let dir = private_dir();
    let (app, _) = signed_client(dir.path(), "fr.kocoa.nestor.runner-manager", true, None);
    let policy = PeerPolicy {
        app_requirement: Some(ipc::developer_id_requirement("fr.kocoa.nestor.runner-manager", "TEAMID1234")),
        cli_requirement: None,
        allow_unsigned_clients: false,
    };
    let running = start(ServiceMode::Demo, policy);
    let answer = hello_from(&app, &running.socket);
    assert_eq!(answer["result"]["access"], "basic", "the requirement compiles and an ad hoc copy fails it: {answer}");
}

#[cfg(target_os = "macos")]
#[test]
fn basic_clients_cannot_keep_the_app_out() {
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let dir = private_dir();
    let (app, app_requirement) = signed_client(dir.path(), "app-client", true, None);
    let policy =
        PeerPolicy { app_requirement: Some(app_requirement), cli_requirement: None, allow_unsigned_clients: false };
    let running = start(ServiceMode::Demo, policy);
    // The test process is unsigned: basic. Eight of them hold every place open to basic peers.
    // The service gives each 10 s from its opening: timed from there, not from after the app's
    // own connection, which takes seconds to check (KCA-737).
    let opened = Instant::now();
    let idle: Vec<UnixStream> =
        (0..ipc::MAX_CONNECTIONS - ipc::RESERVED_FOR_FULL).map(|_| ipc::connect(&running.socket).unwrap()).collect();
    assert_eq!(call(&running.socket, "hello", json!({}))["error"]["message"], "Too many clients.");
    // The app still gets in.
    let answer = hello_from(&app, &running.socket);
    assert_eq!(answer["result"]["access"], "full", "{answer}");

    // A connection that sends nothing is closed 10 s after it opened, which frees its place.
    let mut first = idle.into_iter().next().unwrap();
    first.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut buffer = [0u8; 1];
    assert_eq!(std::io::Read::read(&mut first, &mut buffer).unwrap_or(0), 0, "closed by the service");
    let waited = opened.elapsed();
    assert!(waited >= Duration::from_secs(9) && waited < Duration::from_secs(15), "{waited:?}");
}
