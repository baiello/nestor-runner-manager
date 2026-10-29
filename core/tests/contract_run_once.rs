//! Cross-repository contract test (R2, KCA-734 §5): the service launches the **real**
//! `nestor-runner run-once`, built from nestor at the commit pinned in `contract/nestor.rev`,
//! against a fake Nestor, and the two sides of launch contract v1 are checked together.
//!
//! The binary comes from `scripts/nestor-runner.sh`, which prints the directory to give in
//! `NESTOR_RUNNER_DIR`:
//!
//! ```sh
//! NESTOR_RUNNER_DIR="$(../scripts/nestor-runner.sh)" cargo test --test contract_run_once -- --include-ignored
//! ```
//!
//! Without it the tests are ignored by a plain `cargo test`, and fail when asked for: they never
//! fall back to the double of `examples/run_once_double.rs`, and check that the process the
//! service follows is that binary, at that commit, with nestor's fixtures equal to ours.
//!
//! nestor's CI runs them the other way round (`.github/workflows/ci-contract.yml` there): it
//! builds the runner of its pull request, at a commit that is not pinned here, and sets
//! `NESTOR_RUNNER_UNPINNED=1`, which waives only the check of the commit; the fixtures must still
//! be ours byte for byte.
//!
//! The fake Nestor plays both sides of the server:
//! - for the **service** ([`NestorApi`]), the association, its confirmation (M1), the heartbeat,
//!   the queue and the runner tokens, each refused without a valid proof `nestor-machine-proof/v2`
//!   of the machine key, checked as nestor checks it ([`FakeNestor`]): signed by the key the
//!   machine associated with, for this API's origin, the request's method, path and query, the
//!   digest of its credential (or device code) and of its body, within 60 s of the server's
//!   clock, and strictly after the last proof accepted for the machine;
//! - for the **runner**, over HTTP on loopback, the routes of `/runner`: only tokens it issued to
//!   a machine through a verified proof are accepted, each good for one claim (S2), all of them
//!   refused once the machine is revoked.
//!
//! The service always passes `--server https://<instance>`; the real runner reaches such a server
//! over TLS only. A wrapper, the program the service launches here, rewrites that one value to
//! the fake Nestor's `http://127.0.0.1:<port>`, which the runner accepts for tests, puts the fake
//! Claude Code first on `PATH`, and `exec`s the runner: same pid, same start time, same
//! descriptors. Nothing else of the launch changes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nestor_runner_core::association::Associator;
use nestor_runner_core::mask::Redactor;
use nestor_runner_core::model::*;
use nestor_runner_core::nestor::{proof_header, proof_message, FakeNestor, MachineRequest, MachineResponse, NestorApi};
use nestor_runner_core::persist::{FileStore, MemoryStore, Store};
use nestor_runner_core::process::{ProcessConfig, ProcessLauncher};
use nestor_runner_core::secrets::{MachineKey, MachineKeyStore, MemoryKeyStore, MemorySecretStore, SecretStore};
use nestor_runner_core::supervisor::{Clock, Launcher, Options, Supervisor};
use nestor_runner_core::sys;
use serde_json::{json, Value};

const INSTANCE: &str = "nestor.contract.test";
const SCOPE: &str = "w-contract";
/// Job content that must never reach the service: its logs, events or reports.
const SECRET_PROMPT: &str = "secret-prompt-of-the-contract-job";
const SECRET_TITLE: &str = "secret-title-of-the-contract-job";
/// A fake Claude Code token, kept for the runner by the wrapper as `claude-token set` would.
const CLAUDE_TOKEN: &str = "sk-ant-oat01-contract-test-token-abcdefghijkl";
/// Not the contract's: the wrapper sets it so the runner never reads a keychain of this machine.
const TEST_ONLY_VARIABLE: &str = "NESTOR_RUNNER_TOKEN_STORE";

// MARK: The real runner

/// The binary built by `scripts/nestor-runner.sh`, checked before any launch.
struct Real {
    binary: PathBuf,
    rev: String,
}

fn real() -> Real {
    let directory = std::env::var_os("NESTOR_RUNNER_DIR").map(PathBuf::from).unwrap_or_else(|| {
        panic!("NESTOR_RUNNER_DIR is not set: build the real runner with scripts/nestor-runner.sh (never the double)")
    });
    let binary = fs::canonicalize(directory.join("nestor-runner")).expect("nestor-runner in NESTOR_RUNNER_DIR");
    let rev = fs::read_to_string(directory.join("rev")).expect("rev").trim().to_string();
    assert!(
        rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()),
        "rev in NESTOR_RUNNER_DIR is not a commit: '{rev}'"
    );
    // nestor's CI tests the runner of its own pull request against this service: the commit is
    // then the pull request's, not the pinned one, and the fixtures must still be ours.
    if std::env::var_os("NESTOR_RUNNER_UNPINNED").is_none_or(|v| v != "1") {
        let pinned = include_str!("../../contract/nestor.rev").trim();
        assert_eq!(rev, pinned, "the runner was built from nestor {rev}, not the pinned {pinned}");
    }
    assert_eq!(binary.file_name().unwrap(), "nestor-runner");
    let version = std::process::Command::new(&binary).arg("--version").output().expect("the runner runs");
    assert!(version.status.success(), "{version:?}");
    assert!(
        !String::from_utf8_lossy(&version.stdout).contains("double"),
        "the double of examples/run_once_double.rs is not the runner"
    );
    Real { binary, rev }
}

/// The executable of `pid`, from the kernel.
fn executable(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let length = unsafe { libc::proc_pidpath(pid as libc::c_int, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        (length > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buffer[..length as usize]).into_owned()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

// MARK: Fake Nestor, the service's side

/// The service's side of the server: [`FakeNestor`], which checks every proof exactly as nestor
/// does (`nestor-machine-proof/v2`), on the real clock, with a poll interval of one second, no
/// `scope_filter` announced, the jobs of [`Api`] as its queue, and the runner tokens it issues
/// handed to the runner's side.
struct ContractNestor {
    inner: FakeNestor,
    api: Arc<Api>,
}

impl ContractNestor {
    fn new(api: Arc<Api>) -> Arc<Self> {
        let inner = FakeNestor::new(INSTANCE).auto_approve();
        inner.set_now(now_ms());
        Arc::new(ContractNestor { inner, api })
    }

    /// An administrator revokes the machine on Nestor: its runner tokens die at once.
    fn revoke_machine(&self, machine_id: &str) {
        self.inner.revoke_machine(machine_id);
        self.api.revoke_machine(machine_id);
    }
}

impl NestorApi for ContractNestor {
    fn instance(&self) -> String {
        INSTANCE.into()
    }

    fn send(&self, request: &MachineRequest) -> Result<MachineResponse, String> {
        self.inner.set_now(now_ms());
        self.inner.set_queue(Queue::single(SCOPE, self.api.queued()));
        let mut response = self.inner.send(request)?;
        let mut body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        match request.target.as_str() {
            // Poll every second rather than every five: the test waits for real.
            "/machine/authorizations" if response.status == 201 => {
                body["interval"] = json!(1);
                response.body = body.to_string().into_bytes();
            }
            "/machine/disassociate" if response.status == 204 => {
                for machine in self.inner.revocations() {
                    self.api.revoke_machine(&machine);
                }
            }
            _ => {}
        }
        if let Some(token) = body.get("token").and_then(Value::as_str) {
            let machine = self.inner.machine_of_runner_token(token).expect("a token of a machine");
            self.api.accept_token(token, &machine);
        }
        Ok(response)
    }
}

// MARK: Fake Nestor, the runner's side

#[derive(Debug, Clone)]
struct Token {
    machine: String,
    /// The job its one claim got.
    job: Option<String>,
    revoked: bool,
}

#[derive(Default)]
struct ApiState {
    tokens: BTreeMap<String, Token>,
    queue: VecDeque<Value>,
    /// Jobs leased, by id, with the token that holds each.
    leases: BTreeMap<String, String>,
    most_leases: usize,
    /// Every request: path, body, and whether its token was accepted.
    requests: Vec<(String, String, bool)>,
    /// The descriptors the runner had open at its first request: number and type.
    descriptors_at_first_request: Option<Vec<(i32, String)>>,
}

struct Api {
    state: Mutex<ApiState>,
    /// The pid of the runner, written by the wrapper before it becomes the runner.
    pid_file: PathBuf,
}

impl Api {
    fn queued(&self) -> u32 {
        self.state.lock().unwrap().queue.len() as u32
    }

    fn accept_token(&self, token: &str, machine: &str) {
        let token_state = Token { machine: machine.into(), job: None, revoked: false };
        self.state.lock().unwrap().tokens.insert(token.into(), token_state);
    }

    fn revoke_machine(&self, machine: &str) {
        for token in self.state.lock().unwrap().tokens.values_mut().filter(|t| t.machine == machine) {
            token.revoked = true;
        }
    }

    fn add_jobs(&self, count: usize, scope: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        for _ in 0..count {
            let id = format!("0199d9a1-4c2e-7a01-8f3b-{:012x}", state.queue.len() + state.requests.len() + 1);
            state.queue.push_back(job(&id, scope));
        }
    }

    fn count(&self, suffix: &str) -> usize {
        self.state.lock().unwrap().requests.iter().filter(|(p, _, ok)| *ok && p.ends_with(suffix)).count()
    }

    fn bodies(&self, suffix: &str) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state.requests.iter().filter(|(p, _, _)| p.ends_with(suffix)).map(|(_, b, _)| b.clone()).collect()
    }

    fn refused_requests(&self) -> usize {
        self.state.lock().unwrap().requests.iter().filter(|(_, _, ok)| !ok).count()
    }

    fn answer(&self, path: &str, token: Option<&str>, body: &str) -> (&'static str, String) {
        let mut state = self.state.lock().unwrap();
        if state.descriptors_at_first_request.is_none() {
            let pid = fs::read_to_string(&self.pid_file).ok().and_then(|t| t.lines().last()?.trim().parse().ok());
            state.descriptors_at_first_request = pid.map(descriptors);
        }
        let held = token.and_then(|t| state.tokens.get(t).cloned()).filter(|t| !t.revoked);
        state.requests.push((path.into(), body.into(), held.is_some()));
        let (Some(token), Some(held)) = (token, held) else {
            return ("401 Unauthorized", r#"{"error":"invalid_runner_token"}"#.into());
        };
        match path {
            "/runner/heartbeat" => ("200 OK", r#"{"heartbeat_interval_seconds":15}"#.into()),
            "/runner/jobs/claim" => {
                // One claim per token (S2): once it got a job, a second claim is refused.
                if held.job.is_some() {
                    state.requests.last_mut().unwrap().2 = false;
                    return ("401 Unauthorized", r#"{"error":"invalid_runner_token"}"#.into());
                }
                match state.queue.pop_front() {
                    Some(job) => {
                        let id = job["id"].as_str().unwrap().to_string();
                        state.leases.insert(id.clone(), token.into());
                        state.most_leases = state.most_leases.max(state.leases.len());
                        state.tokens.get_mut(token).unwrap().job = Some(id);
                        ("200 OK", json!({ "job": job }).to_string())
                    }
                    None => ("200 OK", r#"{"job":null}"#.into()),
                }
            }
            _ => {
                let Some(id) = job_of(path) else { return ("404 Not Found", "{}".into()) };
                if state.leases.get(&id).map(String::as_str) != Some(token) {
                    return ("409 Conflict", r#"{"error":"lease_lost"}"#.into());
                }
                if path.ends_with("/renew") {
                    ("200 OK", r#"{"stop":null}"#.into())
                } else if path.ends_with("/finish") {
                    state.leases.remove(&id);
                    ("200 OK", r#"{"status":"succeeded"}"#.into())
                } else if path.ends_with("/release") {
                    // Back in the queue, without using one of its attempts.
                    state.leases.remove(&id);
                    let attempt = serde_json::from_str::<Value>(body).ok().and_then(|b| b["attempt"].as_u64());
                    let mut job = job(&id, None);
                    job["attempt"] = json!(attempt.unwrap_or(1));
                    state.queue.push_front(job);
                    ("204 No Content", String::new())
                } else {
                    ("204 No Content", String::new())
                }
            }
        }
    }
}

/// The job of a route `/runner/jobs/{id}/…`.
fn job_of(path: &str) -> Option<String> {
    path.strip_prefix("/runner/jobs/")?.split('/').next().map(str::to_string)
}

fn job(id: &str, scope: Option<&str>) -> Value {
    json!({
        "id": id,
        "attempt": 1,
        "title": SECRET_TITLE,
        "tool": "claude_code",
        "kind": "work",
        "repository_url": null,
        "branch": null,
        "prompt": SECRET_PROMPT,
        "timeout_seconds": 600,
        "first_sequence": 1,
        "renew_interval_seconds": 1,
        "scope": scope,
    })
}

/// The descriptors `pid` has open, with their type, from `lsof`.
fn descriptors(pid: u32) -> Vec<(i32, String)> {
    let output = std::process::Command::new("lsof")
        .args(["-n", "-P", "-a", "-p", &pid.to_string(), "-d", "0-255", "-F", "ft"])
        .output()
        .expect("lsof");
    let mut found = Vec::new();
    let mut fd = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(number) = line.strip_prefix('f') {
            fd = number.parse().ok();
        } else if let (Some(kind), Some(number)) = (line.strip_prefix('t'), fd) {
            found.push((number, kind.to_string()));
        }
    }
    found
}

fn serve(listener: TcpListener, api: Arc<Api>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let api = api.clone();
        std::thread::spawn(move || connection(stream, &api));
    }
}

fn connection(mut stream: TcpStream, api: &Api) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut request = String::new();
        if reader.read_line(&mut request).unwrap_or(0) == 0 {
            return;
        }
        let path = request.split(' ').nth(1).unwrap_or_default().to_string();
        let (mut length, mut token) = (0, None);
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            if header == "\r\n" {
                break;
            }
            let (name, value) = header.split_once(':').unwrap_or_default();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => token = value.trim().strip_prefix("Bearer ").map(str::to_string),
                _ => {}
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let (status, body) = api.answer(&path, token.as_deref(), &String::from_utf8_lossy(&body));
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

// MARK: The world of a test

/// What the wrapper changes beyond the server's address, to break the launch on purpose.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wrap {
    /// Nothing: the launch is the service's.
    Faithful,
    /// The runner is started as a child of the process the service launched, not in its
    /// place: its `hello` carries another pid than the service's.
    Fork,
    /// The runner gets `--scope`, which contract v1 never passes: the scope defence of S6.
    WithScope(&'static str),
    /// No Claude Code token kept for the runner: what a machine where no one ran
    /// `nestor-runner claude-token set` gives, since the service holds none (A7).
    NoClaudeToken,
}

/// The fake Nestor, the stores that outlive a service (Keychain, state file) and the files of
/// the test. Services come and go over it.
struct World {
    dir: tempfile::TempDir,
    real: Real,
    nestor: Arc<ContractNestor>,
    api: Arc<Api>,
    keys: Arc<MemoryKeyStore>,
    secrets: Arc<MemorySecretStore>,
    store: Arc<dyn Store>,
    wrapper: PathBuf,
    witness: PathBuf,
    body: PathBuf,
}

struct Service {
    supervisor: Arc<Supervisor>,
    launcher: Arc<ProcessLauncher>,
    associator: Arc<Associator>,
}

fn world(wrap: Wrap) -> World {
    world_with(wrap, None)
}

fn world_with(wrap: Wrap, store: Option<Arc<dyn Store>>) -> World {
    let real = real();
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let pid_file = base.join("pids");
    let api = Arc::new(Api { state: Mutex::default(), pid_file: pid_file.clone() });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = api.clone();
    std::thread::spawn(move || serve(listener, serving));

    // The fake Claude Code: what it inherited, its arguments and environment, then `body`.
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let (witness, body) = (base.join("witness"), base.join("body.sh"));
    fs::write(&body, "true\n").unwrap();
    let claude = bin.join("claude");
    fs::write(
        &claude,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "2.1.0 (Claude Code)"; exit 0; fi
for fd in 3 4 5 6 7 8 9; do
  if (: <&$fd) 2>/dev/null || (: >&$fd) 2>/dev/null; then echo "open $fd" >> '{witness}'; fi
done
echo "arguments $*" >> '{witness}'
env >> '{witness}'
echo "pid $$" >> '{witness}'
. '{body}'
echo '{{"type":"result","is_error":false,"result":"Done."}}'
"#,
            witness = witness.display(),
            body = body.display()
        ),
    )
    .unwrap();

    let claude_token = match wrap {
        Wrap::NoClaudeToken => String::new(),
        _ => format!("(umask 077; printf '%s' '{CLAUDE_TOKEN}' > claude-code-token)"),
    };
    let append = match wrap {
        Wrap::WithScope(scope) => format!("set -- \"$@\" --scope '{scope}'"),
        _ => String::new(),
    };
    let wrapper = base.join("launch-real-runner");
    fs::write(
        &wrapper,
        format!(
            r#"#!/bin/sh
# Test harness, not the service: the server's address on loopback, the fake Claude Code on PATH,
# then the real runner in place of this shell (same pid, same descriptors).
echo $$ >> '{pid_file}'
first=1
for argument do
  if [ "$first" = 1 ]; then set --; first=0; fi
  [ "$argument" = 'https://{INSTANCE}' ] && argument='http://127.0.0.1:{port}'
  set -- "$@" "$argument"
done
{append}
{claude_token}
# The service's environment, and one variable of the test: the service holds no Claude Code
# token (A7), so the runner uses the one `claude-token set` kept, here in its own directory (the
# file store), never in a keychain of this machine.
{launch} /usr/bin/env -i PATH='{bin}':"$PATH" LANG="$LANG" HOME="$HOME" TMPDIR="$TMPDIR" \
  NESTOR_RUNNER_TOKEN_STORE=file '{runner}' "$@" {after}
"#,
            pid_file = pid_file.display(),
            launch = if wrap == Wrap::Fork { "" } else { "exec" },
            after = if wrap == Wrap::Fork {
                format!("<&0 &\necho $! >> '{}'\nwait $!", pid_file.display())
            } else {
                String::new()
            },
            bin = bin.display(),
            runner = real.binary.display(),
        ),
    )
    .unwrap();
    for program in [&claude, &wrapper] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(program, fs::Permissions::from_mode(0o755)).unwrap();
    }

    World {
        dir,
        real,
        nestor: ContractNestor::new(api.clone()),
        api,
        keys: Arc::new(MemoryKeyStore::new()),
        secrets: Arc::new(MemorySecretStore::new()),
        store: store.unwrap_or_else(|| Arc::new(MemoryStore::default())),
        wrapper,
        witness,
        body,
    }
}

impl World {
    fn root(&self) -> PathBuf {
        fs::canonicalize(self.dir.path()).unwrap().join("runners")
    }

    /// A service as `nestor-runner-service` wires it: supervisor, real launcher, association.
    fn service(&self) -> Service {
        let redactor = Redactor::new();
        let mut config = ProcessConfig::new(self.wrapper.clone(), vec!["run-once".into()], self.root());
        config.sweep_interval = Duration::from_millis(300);
        let launcher = Arc::new(ProcessLauncher::new(config, redactor.clone()).unwrap());
        let clock: Clock = Arc::new(now_ms);
        let mut options = Options::new(ServiceMode::Live, 8);
        options.kill_grace_ms = 1_500;
        let supervisor =
            Arc::new(Supervisor::open(self.store.clone(), launcher.clone(), clock.clone(), options, redactor));
        let associator = Arc::new(Associator::new(
            Some(self.nestor.clone() as Arc<dyn NestorApi>),
            self.keys.clone(),
            self.secrets.clone(),
            supervisor.clone(),
            clock,
        ));
        launcher.attach(&supervisor, associator.clone());
        supervisor.reconcile();
        associator.startup();
        Service { supervisor, launcher, associator }
    }

    fn set_body(&self, body: &str) {
        fs::write(&self.body, format!("{body}\n")).unwrap();
    }

    fn witness(&self) -> String {
        fs::read_to_string(&self.witness).unwrap_or_default()
    }
}

fn machine() -> MachineInfo {
    MachineInfo {
        name: "Contract".into(),
        model: "Mac17,14".into(),
        architecture: "arm64".into(),
        operating_system: "macOS 27".into(),
        cpu_cores: 18,
        memory_bytes: 1,
        disk_free_bytes: 1,
    }
}

impl Service {
    /// One turn of the service's loop: the association and the queue, then the regulation.
    fn step(&self) {
        let queue = self.associator.tick();
        self.supervisor.tick(&queue);
    }

    fn wait(&self, what: &str, within: Duration, mut condition: impl FnMut(&Service) -> bool) {
        let start = Instant::now();
        loop {
            self.step();
            if condition(self) {
                return;
            }
            assert!(start.elapsed() < within, "timed out waiting for {what}:\n{}", self.dump());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn associate(&self) -> String {
        let connection = self.associator.associate(&machine()).unwrap();
        assert!(matches!(connection, Connection::Associating { .. }), "{connection:?}");
        self.wait("the approval", Duration::from_secs(10), |s| {
            matches!(s.supervisor.connection(), Connection::Confirming { .. })
        });
        // M1: nothing is created before the owner confirms the account on the machine.
        assert!(self.supervisor.runners().is_empty());
        self.associator.confirm(true).unwrap();
        assert!(self.supervisor.connection().is_associated());
        self.supervisor.machine_id().unwrap()
    }

    fn runner(&self) -> RunnerView {
        let runners = self.supervisor.runners();
        assert_eq!(runners.len(), 1, "{runners:#?}");
        runners.into_iter().next().unwrap()
    }

    fn logs(&self, runner_id: &str) -> String {
        let lines = self.supervisor.logs(runner_id, 500).unwrap_or_default();
        lines.iter().map(|l| l.message.clone()).collect::<Vec<_>>().join("\n")
    }

    fn events(&self) -> Vec<EventKind> {
        self.supervisor.events_since(0).unwrap().into_iter().map(|e| e.kind).collect()
    }

    fn outcomes(&self) -> Vec<TaskOutcome> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                EventKind::TaskFinished { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect()
    }

    fn dump(&self) -> String {
        let mut text = format!("{:#?}\n", self.supervisor.runners());
        for runner in self.supervisor.runners() {
            text += &self.logs(&runner.id);
        }
        text + &format!("\n{:?}", self.events())
    }

    /// Everything the service shows or keeps: status, events, logs, stored state.
    fn everything_visible(&self, store: &dyn Store) -> String {
        let mut text = serde_json::to_string(&self.supervisor.status()).unwrap();
        text += &serde_json::to_string(&self.supervisor.events_since(0).unwrap()).unwrap();
        for runner in self.supervisor.runners() {
            text += &self.logs(&runner.id);
        }
        text + &serde_json::to_string(store.load().state()).unwrap()
    }
}

/// The process the service follows for its one runner, once it is up: checked to be the real
/// binary, at the pinned commit, launched by contract v1.
fn followed(world: &World, service: &Service) -> (u32, u64) {
    let pid = service.supervisor.runner_pids()[0];
    let process = sys::process_info(pid).expect("alive");
    let program = executable(pid).expect("its executable");
    assert_eq!(
        fs::canonicalize(&program).unwrap(),
        world.real.binary,
        "the service follows the real nestor-runner at {}, never a double",
        world.real.rev
    );
    (pid, process.start_time_us)
}

fn gone(pid: u32, start_time_us: u64) -> bool {
    sys::process_info(pid).is_none_or(|p| p.start_time_us != start_time_us || p.zombie)
}

fn value_of(arguments: &[String], flag: &str) -> Option<String> {
    arguments.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

// MARK: Fixtures

/// The vendored fixtures are nestor's, byte for byte, at the pinned commit.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn the_vendored_fixtures_are_nestors_at_the_pinned_commit() {
    let real = real();
    let directory = real.binary.parent().unwrap().to_path_buf();
    let theirs = fs::read(directory.join("run-once-v1.json")).unwrap();
    let ours = include_bytes!("../../contract/run-once-v1.json");
    assert!(
        theirs == ours,
        "contract/run-once-v1.json differs from nestor's at {}: copy it unchanged, or pin the commit it comes from",
        real.rev
    );
}

// MARK: Scenarios of KCA-734 §5

/// 1, 2 and 3: the service launches the real runner by contract v1; its `hello` matches; the
/// credential is read and closed before the first request and reaches neither Claude Code, an
/// argument, an environment nor a log; the report pipe is not inherited; the runner claims one
/// job, the service takes and gives back the admission token, without `--scope` and one task at
/// a time; the job's content never reaches the service. Every call of the service to Nestor
/// carried a valid proof.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn the_service_runs_the_real_runner_by_contract_v1() {
    let world = world(Wrap::Faithful);
    let service = world.service();
    let machine_id = service.associate();
    // Three tasks at a time asked; without the scope filter of S6, one applies.
    service.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    world.set_body("sleep 2");
    world.api.add_jobs(3, None);

    service.wait("the first claim", Duration::from_secs(30), |s| s.supervisor.status().busy_runners == 1);
    let runner = service.runner();
    let (pid, _) = followed(&world, &service);
    let started = service.logs(&runner.id);
    assert!(service.supervisor.runners().len() == 1, "one runner at a time: {}", service.dump());

    // 1. The launch: the contract's options, the service's values, no --scope.
    let arguments = sys::process_arguments(pid).unwrap();
    assert_eq!(arguments.get(1).map(String::as_str), Some("run-once"), "{arguments:?}");
    assert_eq!(value_of(&arguments, "--runner-id").as_deref(), Some(runner.id.as_str()));
    assert_eq!(value_of(&arguments, "--generation"), Some(runner.generation.to_string()));
    assert_eq!(value_of(&arguments, "--machine-id").as_deref(), Some(machine_id.as_str()));
    assert_eq!(value_of(&arguments, "--credential-fd").as_deref(), Some("3"));
    assert_eq!(value_of(&arguments, "--report-fd").as_deref(), Some("4"));
    assert!(!arguments.iter().any(|a| a.starts_with("--scope")), "{arguments:?}");

    // 2. The credential: closed before the first request; only the report pipe kept.
    let open = world.api.state.lock().unwrap().descriptors_at_first_request.clone().expect("a request came");
    let pipes = open.iter().filter(|(fd, kind)| *fd > 2 && matches!(kind.as_str(), "PIPE" | "FIFO")).count();
    assert_eq!(pipes, 1, "the reports' pipe only, the credential's is closed: {open:?}");

    service.wait("the three jobs", Duration::from_secs(90), |s| {
        s.outcomes().len() == 3 && s.supervisor.status().busy_runners == 0
    });
    assert!(started.contains("contract") || !started.is_empty());
    let credentials = world.nestor.inner.runner_credentials(&machine_id);
    assert_eq!(credentials.len(), 3, "one credential per launch");
    assert_eq!(world.api.count("/claim"), 3, "one claim per runner: {}", service.dump());
    assert_eq!(world.api.count("/finish"), 3);
    assert_eq!(world.api.refused_requests(), 0, "no second claim, no unknown token");
    assert_eq!(world.api.state.lock().unwrap().most_leases, 1, "one task at a time");
    assert_eq!(service.outcomes(), vec![TaskOutcome::Succeeded; 3], "{}", service.dump());

    // Claude Code inherited 0, 1 and 2 only, and none of the credentials.
    let witness = world.witness();
    let inherited: Vec<&str> = witness.lines().filter(|l| l.starts_with("open ")).collect();
    assert!(inherited.is_empty(), "Claude Code inherited {inherited:?}");
    for credential in &credentials {
        assert!(!witness.contains(credential.as_str()), "Claude Code saw a credential");
    }
    // No credential and no job content anywhere the service shows or keeps.
    let visible = service.everything_visible(world.store.as_ref());
    for secret in credentials.iter().map(String::as_str).chain([SECRET_PROMPT, SECRET_TITLE]) {
        assert!(!visible.contains(secret), "{secret} is visible: {visible}");
    }
    assert_eq!(world.nestor.inner.rejected_proofs(), 0, "every proof was valid");
    assert_eq!(world.nestor.inner.stale_proofs(), 0, "every proof was signed after the last");
    let times = world.nestor.inner.accepted_proofs();
    assert!(times.windows(2).all(|w| w[0] < w[1]), "strictly increasing: {times:?}");
    for heartbeat in world.nestor.inner.heartbeats(&machine_id) {
        assert_eq!(heartbeat["service_mode"], "live");
        assert_eq!(heartbeat["max_concurrent_tasks"], 1, "no scope filter announced: one at a time");
    }
    assert!(!world.nestor.inner.heartbeats(&machine_id).is_empty(), "the service sent its heartbeat");
    for claim in world.api.bodies("/claim") {
        let claim: Value = serde_json::from_str(&claim).unwrap();
        assert_eq!(claim["machine_id"], machine_id.as_str());
        assert!(claim.get("scope").is_none_or(Value::is_null), "no scope claimed: {claim}");
    }
}

/// 2, while the job runs: neither the runner's nor Claude Code's arguments or environment hold
/// the credential (what `ps` shows), and the runner's environment is the contract's.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn the_credential_is_in_no_argument_and_no_environment() {
    let world = world(Wrap::Faithful);
    let service = world.service();
    let machine_id = service.associate();
    world.set_body("sleep 30");
    world.api.add_jobs(1, None);
    service.wait("Claude Code", Duration::from_secs(30), |_| world.witness().contains("pid "));
    let (pid, _) = followed(&world, &service);
    let claude: u32 =
        world.witness().lines().find_map(|l| l.strip_prefix("pid ")?.parse().ok()).expect("Claude Code's pid");
    let credential = world.nestor.inner.runner_credentials(&machine_id).pop().unwrap();
    for process in [pid, claude] {
        let arguments = sys::process_arguments(process).unwrap_or_default().join(" ");
        assert!(!arguments.contains(&credential), "in the arguments of {process}");
        let environment = sys::process_environment(process).unwrap_or_default();
        assert!(!environment.iter().any(|(_, v)| v.contains(&credential)), "in the environment of {process}");
    }
    let names: BTreeSet<String> = sys::process_environment(pid)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .filter(|n| n != TEST_ONLY_VARIABLE)
        .collect();
    let allowed: BTreeSet<String> = ["PATH", "LANG", "HOME", "TMPDIR"].map(String::from).into();
    assert!(names.is_subset(&allowed), "the runner's environment is the contract's: {names:?}");
    let runner = service.runner();
    service.supervisor.stop_runner(&runner.id, true).unwrap();
    service.wait("the stop", Duration::from_secs(20), |s| s.supervisor.status().busy_runners == 0);
}

/// 1: a `hello` that is not the launch's (here another generation) makes the service terminate
/// the runner, which is not launched again.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn a_hello_that_is_not_the_launchs_terminates_the_real_runner() {
    let world = world(Wrap::Fork);
    let service = world.service();
    service.associate();
    world.set_body("sleep 30");
    world.api.add_jobs(1, None);
    service.wait("the refusal", Duration::from_secs(30), |s| {
        s.supervisor.runners().iter().any(|r| r.failure_reason.as_deref().is_some_and(|f| f.contains("hello")))
    });
    // The process the service launched, then the real runner it started.
    let pids: Vec<u32> = fs::read_to_string(&world.api.pid_file).unwrap().lines().map(|l| l.parse().unwrap()).collect();
    assert_eq!(pids.len(), 2, "launched once: {pids:?}");
    // Possibly ended already: the service terminates it as soon as it reads the hello.
    let start = sys::process_info(pids[1]).map(|p| p.start_time_us).unwrap_or(0);
    service.wait("the end of the real runner", Duration::from_secs(20), |s| {
        gone(pids[1], start) && s.supervisor.status().busy_runners == 0 && s.runner().state == RunnerState::Stopped
    });
    assert_eq!(world.api.count("/finish"), 0, "its job is abandoned");
    // Held, with its reason, until started by hand: not launched again for the next job.
    world.api.add_jobs(1, None);
    for _ in 0..50 {
        service.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(&world.api.pid_file).unwrap().lines().count(), 2, "{}", service.dump());
    assert!(service.runner().failure_reason.unwrap().contains("hello"));
}

/// 4: drained while waiting for a job, the real runner exits 0 without claiming; drained while
/// running its job, it ends it, then exits 0, without a second claim.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn a_drained_runner_ends_without_claiming_or_after_its_job() {
    let world = world(Wrap::Faithful);
    let service = world.service();
    service.associate();

    // Idle: one job queued for the launch, taken back from the fake before the claim.
    world.api.add_jobs(1, None);
    service.wait("a runner up", Duration::from_secs(30), |s| {
        s.supervisor.runners().iter().any(|r| r.state == RunnerState::Ready)
    });
    world.api.state.lock().unwrap().queue.clear();
    let runner = service.runner();
    let (pid, start) = followed(&world, &service);
    service.supervisor.set_mode(Mode::Paused, None).unwrap();
    service.wait("the drained runner's exit", Duration::from_secs(20), |_| gone(pid, start));
    service.wait("its state", Duration::from_secs(5), |s| s.runner().state == RunnerState::Stopped);
    assert_eq!(world.api.count("/claim").min(1), world.api.count("/claim"), "at most the empty claim");
    assert!(service.outcomes().is_empty(), "no task: {}", service.dump());
    assert!(service.logs(&runner.id).contains("Exited."), "{}", service.dump());

    // Busy: the job ends, then the runner exits.
    service.supervisor.set_mode(Mode::Automatic, None).unwrap();
    world.set_body("sleep 3");
    world.api.add_jobs(1, None);
    service.wait("the claim", Duration::from_secs(30), |s| s.supervisor.status().busy_runners == 1);
    let (pid, start) = followed(&world, &service);
    world.api.add_jobs(1, None);
    service.supervisor.set_mode(Mode::Paused, None).unwrap();
    service.wait("the job's end", Duration::from_secs(30), |s| s.outcomes().len() == 1);
    service.wait("the exit", Duration::from_secs(10), |_| gone(pid, start));
    assert_eq!(service.outcomes(), vec![TaskOutcome::Succeeded], "{}", service.dump());
    assert_eq!(world.api.count("/finish"), 1);
    assert_eq!(world.api.queued(), 1, "nothing more claimed while paused");
}

/// 4: the end of standard input is a drain, as when the service dies; 5: a restarted service
/// adopts the real runner by pid, start time and generation, does not launch it again, and can
/// terminate it; a process with the same arguments but another generation is never signalled.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn a_restarted_service_adopts_the_real_runner_and_can_terminate_it() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn Store> = Arc::new(FileStore::new(&dir.path().join("state")).unwrap());
    let world = world_with(Wrap::Faithful, Some(store));
    let first = world.service();
    first.associate();
    world.set_body("sleep 60");
    world.api.add_jobs(1, None);
    first.wait("the claim", Duration::from_secs(30), |_| world.witness().contains("pid "));
    let runner = first.runner();
    let (pid, start) = followed(&world, &first);

    // A process with the runner's arguments, of another generation, outside its directory.
    let mut homonym = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 120; true", "run-once", "--runner-id", &runner.id, "--generation"])
        .arg((runner.generation + 1).to_string())
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    let homonym_start = sys::process_info(homonym.id()).unwrap().start_time_us;

    // The service dies: its end of standard input closes (a drain, never an abandon) and it
    // follows nothing any more.
    first.launcher.drain(&world.store.load().state().runners.iter().find(|r| r.id == runner.id).unwrap().clone());
    let Service { supervisor, launcher, associator } = first;
    drop((supervisor, associator, launcher));

    let second = world.service();
    let adopted = second.runner();
    assert_eq!(adopted.state, RunnerState::Busy, "adopted: {}", second.dump());
    assert!(second.logs(&runner.id).contains(&format!("Adopted process {pid}")), "{}", second.dump());
    assert!(!gone(pid, start), "the job goes on");
    for _ in 0..20 {
        second.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    let launches = fs::read_to_string(world.api.pid_file.clone()).unwrap().lines().count();
    assert_eq!(launches, 1, "not launched again");

    // Terminated by the service that adopted it: the real process ends, and only it.
    second.supervisor.stop_runner(&runner.id, true).unwrap();
    second.wait("the end of the adopted runner", Duration::from_secs(20), |_| gone(pid, start));
    assert!(!gone(homonym.id(), homonym_start), "the homonym of another generation is never signalled");
    homonym.kill().unwrap();
    homonym.wait().unwrap();
    assert_eq!(world.api.count("/finish"), 0, "SIGTERM abandons the job");
}

/// 6: `SIGTERM` ends the real runner and its Claude Code; the job is abandoned, not finished.
/// The kill and the sweep after a runner deaf to `SIGTERM` are R3's, tested with the double.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn terminate_ends_the_real_runner_and_its_claude_code() {
    let world = world(Wrap::Faithful);
    let service = world.service();
    service.associate();
    world.set_body("sleep 60");
    world.api.add_jobs(1, None);
    service.wait("Claude Code", Duration::from_secs(30), |_| world.witness().contains("pid "));
    let (pid, start) = followed(&world, &service);
    let claude: u32 = world.witness().lines().find_map(|l| l.strip_prefix("pid ")?.parse().ok()).unwrap();
    let claude_start = sys::process_info(claude).unwrap().start_time_us;
    let runner = service.runner();
    service.supervisor.stop_runner(&runner.id, true).unwrap();
    service.wait("the end", Duration::from_secs(20), |s| {
        gone(pid, start) && gone(claude, claude_start) && s.runner().state == RunnerState::Stopped
    });
    assert_eq!(world.api.count("/finish"), 0);
    assert_eq!(service.outcomes(), vec![TaskOutcome::Stopped], "{}", service.dump());
}

/// 7: Nestor refuses the credential (the machine is revoked): the real runner exits 77, the
/// service marks it and launches nothing more. Then, disassociation from the service ends the
/// real process that runs a job: the regression of É1.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn revocation_stops_the_real_runner_and_launches_nothing_more() {
    // Revoked on Nestor while a runner waits: its token is refused, it exits 77.
    let world = world(Wrap::Faithful);
    let service = world.service();
    let machine_id = service.associate();
    world.api.add_jobs(1, None);
    service.wait("a runner up", Duration::from_secs(30), |s| {
        s.supervisor.runners().iter().any(|r| r.state == RunnerState::Ready)
    });
    world.api.state.lock().unwrap().queue.clear();
    let (pid, start) = followed(&world, &service);
    world.api.revoke_machine(&machine_id);
    service.wait("exit 77", Duration::from_secs(60), |s| s.runner().state == RunnerState::Failed);
    assert!(gone(pid, start));
    let runner = service.runner();
    assert!(runner.failure_reason.as_deref().unwrap().contains("exit 77"), "{}", service.dump());
    world.api.add_jobs(1, None);
    for _ in 0..50 {
        service.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    let launches = fs::read_to_string(world.api.pid_file.clone()).unwrap().lines().count();
    assert_eq!(launches, 1, "no relaunch after 77: {}", service.dump());

    // Disassociated from the machine while a real runner runs its job: the process ends.
    let world = self::world(Wrap::Faithful);
    let service = world.service();
    let machine_id = service.associate();
    world.set_body("sleep 60");
    world.api.add_jobs(1, None);
    service.wait("Claude Code", Duration::from_secs(30), |_| world.witness().contains("pid "));
    let (pid, start) = followed(&world, &service);
    service.associator.disassociate();
    service.wait("the end of the real runner", Duration::from_secs(20), |s| {
        gone(pid, start) && s.supervisor.runners().is_empty()
    });
    assert!(world.nestor.inner.is_revoked(&machine_id), "the revocation reached Nestor");
    assert!(world.keys.load().unwrap().is_none() && world.secrets.get("machine-credential").unwrap().is_none());
}

/// 7, revoked on Nestor while a job runs: the service learns it from the queue's answer and
/// terminates the real runner.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn a_revocation_on_nestor_ends_the_real_runner_that_runs_a_job() {
    let world = world(Wrap::Faithful);
    let service = world.service();
    let machine_id = service.associate();
    world.set_body("sleep 60");
    world.api.add_jobs(1, None);
    service.wait("Claude Code", Duration::from_secs(30), |_| world.witness().contains("pid "));
    let (pid, start) = followed(&world, &service);
    world.nestor.revoke_machine(&machine_id);
    service.wait("the end of the real runner", Duration::from_secs(30), |s| {
        gone(pid, start) && s.supervisor.connection() == Connection::Revoked
    });
}

/// 8: hostile lines on the report channel can only come from the runner itself, whose
/// descriptor 4 its children never inherit (scenario 2). What the service makes of forged lines
/// is tested with the double (`process.rs`); here, a child of the real runner that tries to
/// write reports reaches nothing, and the service stays stable.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn a_child_of_the_real_runner_cannot_write_reports() {
    let world = world(Wrap::Faithful);
    let service = world.service();
    service.associate();
    let forged = r#"{"v":1,"type":"claimed","job_id":"forged"}"#;
    world.set_body(&format!(
        "for fd in 3 4 5 6 7 8 9; do echo '{forged}' >&$fd 2>/dev/null && echo \"wrote $fd\" >> '{}'; done; true",
        world.witness.display()
    ));
    world.api.add_jobs(2, None);
    service.wait("two jobs", Duration::from_secs(60), |s| s.outcomes().len() == 2);
    assert!(!world.witness().contains("wrote "), "{}", world.witness());
    let started: Vec<String> = service
        .events()
        .into_iter()
        .filter_map(|e| match e {
            EventKind::TaskStarted { task_ref, .. } => Some(task_ref),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 2);
    assert!(!started.iter().any(|t| t == "forged"));
}

/// 9, ahead of S6: once `--scope` is passed, a Nestor that ignores the filter gets the job back
/// through `release`: no failure, nothing run, nothing of the job in the service.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn a_job_of_another_scope_is_released_not_failed() {
    let world = world(Wrap::WithScope("ws_1"));
    let service = world.service();
    service.associate();
    world.api.add_jobs(1, Some("ws_2"));
    service
        .wait("the release", Duration::from_secs(30), |s| world.api.count("/release") == 1 && s.outcomes().len() == 1);
    service.supervisor.set_mode(Mode::Paused, None).unwrap();
    assert_eq!(service.outcomes(), vec![TaskOutcome::Released], "{}", service.dump());
    let release: Value = serde_json::from_str(&world.api.bodies("/release")[0]).unwrap();
    assert_eq!(release, json!({ "attempt": 1, "reason": "scope_mismatch" }));
    for never in ["/finish", "/events", "/renew"] {
        assert_eq!(world.api.count(never), 0, "{never}");
    }
    assert!(!world.witness.exists(), "Claude Code never ran");
    assert_eq!(world.api.queued(), 1, "back in the queue");
    let visible = service.everything_visible(world.store.as_ref());
    assert!(!visible.contains(SECRET_PROMPT) && !visible.contains(SECRET_TITLE), "{visible}");
}

/// A7: the service holds no Claude Code token, so without one kept by `claude-token set` the
/// real runner fails the job it claimed, says why to Nestor, and exits 0: a failed job, not a
/// crash, and the service launches again for the next job.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn without_a_kept_claude_code_token_the_job_fails_not_the_runner() {
    let world = world(Wrap::NoClaudeToken);
    let service = world.service();
    service.associate();
    world.api.add_jobs(2, None);
    service.wait("two jobs", Duration::from_secs(60), |s| s.outcomes().len() == 2);
    assert_eq!(service.outcomes(), vec![TaskOutcome::Failed; 2], "{}", service.dump());
    for finish in world.api.bodies("/finish") {
        let finish: Value = serde_json::from_str(&finish).unwrap();
        assert_eq!(finish["failure"], "claude_not_signed_in", "{finish}");
    }
    assert!(!world.witness.exists(), "Claude Code never ran");
    assert!(service.runner().failure_reason.is_none(), "{}", service.dump());
    // `run-once` at nestor.rev reports `failed` without why: the machine cannot tell Claude Code
    // is not signed in until the runner sends `finished.failure` (read ahead of the contract).
    assert_eq!(service.supervisor.status().claude, ClaudeSignIn::Unknown);
}

// MARK: The fake applies the proof of possession

/// Without this, the scenarios would prove nothing about the service's proofs: the fake refuses,
/// as nestor does, a proof replayed or signed no later than the last, out of the window, for
/// another audience, method, target, credential or body, or by another key; and the service's
/// own proofs pass.
#[test]
#[ignore = "needs the real runner: NESTOR_RUNNER_DIR=\"$(../scripts/nestor-runner.sh)\""]
fn the_fake_nestor_applies_the_proof_of_possession() {
    let world = world(Wrap::Faithful);
    let service = world.service();
    let machine_id = service.associate();
    assert_eq!(world.nestor.inner.rejected_proofs() + world.nestor.inner.stale_proofs(), 0);
    let credential = world.secrets.get("machine-credential").unwrap().expect("credential");
    let key = world.keys.load().unwrap().expect("key");
    let stranger = MemoryKeyStore::new().create().unwrap();
    let audience = format!("https://{INSTANCE}");
    let body = b"{}".to_vec();
    // Signs with `signer` what is given, then sends the request to /machine/runners.
    let send =
        |signer: &dyn MachineKey, audience: &str, method: &str, target: &str, secret: &str, signed: &[u8], at: u64| {
            let message = proof_message(audience, method, target, at, Some(secret), signed);
            let request = MachineRequest {
                method: "POST",
                target: "/machine/runners".into(),
                body: body.clone(),
                credential: Some(credential.clone()),
                proof: proof_header(at, &signer.sign(&message).unwrap()),
            };
            let response = world.nestor.send(&request).unwrap();
            (response.status, response.code())
        };
    let secret = credential.expose();
    let invalid = (401, Some("invalid_machine_proof".to_string()));
    let stale = (401, Some("stale_machine_proof".to_string()));
    let t = now_ms();
    assert_eq!(send(key.as_ref(), &audience, "POST", "/machine/runners", secret, &body, t).0, 201, "a fresh proof");
    assert_eq!(send(key.as_ref(), &audience, "POST", "/machine/runners", secret, &body, t), stale, "replayed");
    assert_eq!(send(key.as_ref(), &audience, "POST", "/machine/runners", secret, &body, t - 1), stale, "earlier");
    for (what, response) in [
        (
            "another audience",
            send(key.as_ref(), "https://other.test", "POST", "/machine/runners", secret, &body, t + 1),
        ),
        ("another method", send(key.as_ref(), &audience, "PUT", "/machine/runners", secret, &body, t + 2)),
        ("another target", send(key.as_ref(), &audience, "POST", "/machine/queue", secret, &body, t + 3)),
        ("another query", send(key.as_ref(), &audience, "POST", "/machine/runners?x=1", secret, &body, t + 4)),
        (
            "another credential",
            send(key.as_ref(), &audience, "POST", "/machine/runners", "nestor_machine_other", &body, t + 5),
        ),
        ("another body", send(key.as_ref(), &audience, "POST", "/machine/runners", secret, b"{\"a\":1}", t + 6)),
        ("another key", send(stranger.as_ref(), &audience, "POST", "/machine/runners", secret, &body, t + 7)),
    ] {
        assert_eq!(response, invalid, "{what}");
    }
    assert_eq!(send(key.as_ref(), &audience, "POST", "/machine/runners", secret, &body, now_ms() - 61_000), stale);
    assert_eq!(send(key.as_ref(), &audience, "POST", "/machine/runners", secret, &body, now_ms() + 61_000), stale);
    // A time far ahead recorded nothing; one just after the last still passes.
    assert_eq!(send(key.as_ref(), &audience, "POST", "/machine/runners", secret, &body, t + 8).0, 201);
    assert_eq!(world.nestor.inner.rejected_proofs(), 7);
    assert_eq!(world.nestor.inner.stale_proofs(), 4);
    assert_eq!(world.nestor.inner.runners_of(&machine_id).len(), 2);
    // The service, right after: its own proofs pass, signed after the ones above.
    std::thread::sleep(Duration::from_millis(50));
    world.api.add_jobs(1, None);
    service.wait("the job", Duration::from_secs(60), |s| s.outcomes().len() == 1);
    assert_eq!(world.nestor.inner.rejected_proofs(), 7, "none of the service's was refused");
}
