//! The local runner service, run in the user's session (no privileged installation).
//!
//! ```text
//! nestor-runner-service [--data-dir DIR] [--socket PATH] [--simulate]
//! ```
//!
//! Without `--simulate` the service is `live`: real configuration, persistence, admission state,
//! machine measurements and local API, with secrets in the Keychain. Nestor's API for machines
//! does not exist yet, so `associate` answers `unavailable` and no runner is launched.
//! With `--simulate` it is `demo`: Nestor, its tasks and the runner processes are simulated,
//! secrets stay in memory, everything else is real. Its state is kept in a separate directory.
//!
//! `--allow-unsigned-clients` gives unsigned clients of the same user full access to the local
//! API, which the service reports to every client. It exists **only** in a development build with
//! the Cargo feature `dev-unsigned` (M1): a release build cannot be compiled with it, a build
//! without it rejects the flag as an unknown option (exit 64), and even a development build
//! rejects it when `NESTOR_TEAM_ID` was set at build time (a build meant to be signed). Otherwise
//! a task that escaped its confinement could restart the signed service with the flag and get
//! full access.
//!
//! Usage errors exit 64.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nestor_runner_core::association::Associator;
use nestor_runner_core::demo::{DemoQueue, DEMO_INSTANCE};
use nestor_runner_core::fake::{FakeLauncher, UnavailableLauncher};
use nestor_runner_core::ipc::{self, developer_id_requirement, BoundSocket, InstanceLock, PeerPolicy, Service};
use nestor_runner_core::mask::Redactor;
use nestor_runner_core::model::ServiceMode;
use nestor_runner_core::nestor::{FakeNestor, NestorApi};
use nestor_runner_core::persist::FileStore;
use nestor_runner_core::secrets::stores_for;
use nestor_runner_core::supervisor::{Clock, Launcher, Options, Supervisor};

/// Bundle identifiers of the signed app and command line tool. The team identifier is set at
/// build time (`NESTOR_TEAM_ID`); without it no client can be verified by signature.
const APP_IDENTIFIER: &str = "fr.kocoa.nestor.runner-manager";
const CLI_IDENTIFIER: &str = "fr.kocoa.nestor.runner-manager.cli";

#[cfg(all(feature = "dev-unsigned", not(debug_assertions)))]
compile_error!("the dev-unsigned feature is for development builds only; never in a release build (M1)");

/// Exit status of a usage error.
const EXIT_USAGE: i32 = 64;

enum Failure {
    Usage(String),
    Runtime(String),
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(Failure::Usage(message)) => {
            eprintln!("nestor-runner-service: {message}");
            eprintln!("usage: {}", usage());
            std::process::exit(EXIT_USAGE);
        }
        Err(Failure::Runtime(message)) => {
            eprintln!("nestor-runner-service: {message}");
            std::process::exit(1);
        }
    }
}

fn usage() -> &'static str {
    if cfg!(feature = "dev-unsigned") {
        "nestor-runner-service [--data-dir DIR] [--socket PATH] [--simulate] [--allow-unsigned-clients]"
    } else {
        "nestor-runner-service [--data-dir DIR] [--socket PATH] [--simulate]"
    }
}

/// `--allow-unsigned-clients`, accepted only in a development build without a team id (M1).
#[cfg(feature = "dev-unsigned")]
fn unsigned_clients_flag() -> Result<bool, Failure> {
    if option_env!("NESTOR_TEAM_ID").is_some() {
        return Err(Failure::Usage("--allow-unsigned-clients is refused in a build with a team id".into()));
    }
    Ok(true)
}

#[cfg(not(feature = "dev-unsigned"))]
fn unsigned_clients_flag() -> Result<bool, Failure> {
    Err(Failure::Usage("unknown argument --allow-unsigned-clients".into()))
}

fn default_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/Nestor Runner Manager")
}

fn requirement(identifier: &str) -> Option<String> {
    option_env!("NESTOR_TEAM_ID").map(|team| developer_id_requirement(identifier, team))
}

/// Blocks the stop signals in every thread; one thread waits for them and stops cleanly.
fn block_stop_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn run() -> Result<(), Failure> {
    let mut data_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut simulate = false;
    let mut allow_unsigned_clients = false;
    let value_of = |arguments: &mut std::iter::Skip<std::env::Args>, flag: &str| {
        arguments.next().ok_or_else(|| Failure::Usage(format!("{flag} needs a value")))
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value_of(&mut arguments, "--data-dir")?)),
            "--socket" => socket = Some(PathBuf::from(value_of(&mut arguments, "--socket")?)),
            "--simulate" => simulate = true,
            "--allow-unsigned-clients" => allow_unsigned_clients = unsigned_clients_flag()?,
            "--help" | "-h" => {
                println!("usage: {}", usage());
                return Ok(());
            }
            other => return Err(Failure::Usage(format!("unknown argument {other}"))),
        }
    }
    let mode = if simulate { ServiceMode::Demo } else { ServiceMode::Live };
    let data_dir = data_dir.unwrap_or_else(|| default_root().join(if simulate { "demo" } else { "live" }));
    let socket = socket.unwrap_or_else(|| data_dir.join("run/service.sock"));
    let signals = block_stop_signals();

    let _lock =
        InstanceLock::acquire(&data_dir).map_err(|e| Failure::Runtime(format!("{}: {e}", data_dir.display())))?;
    let store = Arc::new(FileStore::new(&data_dir).map_err(|e| Failure::Runtime(e.to_string()))?);
    let clock: Clock =
        Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0));
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let redactor = Redactor::new();

    // Demo: a simulated Nestor and pretend runners. Live: no Nestor API yet, so no runner.
    let fake = Arc::new(FakeLauncher::new());
    let nestor = Arc::new(FakeNestor::new(DEMO_INSTANCE).auto_approve());
    let (launcher, api): (Arc<dyn Launcher>, Option<Arc<dyn NestorApi>>) =
        if simulate { (fake.clone(), Some(nestor.clone())) } else { (Arc::new(UnavailableLauncher), None) };
    let (secrets, keys) = stores_for(mode, option_env!("NESTOR_KEYCHAIN_GROUP").map(String::from));

    let supervisor =
        Arc::new(Supervisor::open(store, launcher, clock.clone(), Options::new(mode, cores), redactor.clone()));
    supervisor.reconcile();
    let associator = Arc::new(Associator::new(api, keys, secrets, Arc::clone(&supervisor), clock.clone()));
    associator.startup();
    let policy = PeerPolicy {
        app_requirement: requirement(APP_IDENTIFIER),
        cli_requirement: requirement(CLI_IDENTIFIER),
        allow_unsigned_clients,
    };
    let service = Arc::new(Service::new(Arc::clone(&supervisor), Arc::clone(&associator), clock.clone(), policy));
    service.sample_load();

    let bound = ipc::bind(&socket).map_err(|e| Failure::Runtime(format!("{}: {e}", socket.display())))?;
    eprintln!(
        "nestor-runner-service: {} mode, state in {}, listening on {}{}",
        if simulate { "DEMO (simulated Nestor, tasks and runners)" } else { "live" },
        data_dir.display(),
        socket.display(),
        if allow_unsigned_clients { "; UNSIGNED CLIENTS ALLOWED (development only)" } else { "" }
    );

    // Clean stop: remove the socket, then exit. Runners lose their control pipe with this
    // process and drain by themselves: no task is interrupted.
    {
        let (path, inode) = (bound.path.clone(), bound.inode());
        std::thread::spawn(move || {
            let mut signal: libc::c_int = 0;
            unsafe { libc::sigwait(&signals, &mut signal) };
            BoundSocket::remove_file(&path, inode);
            eprintln!("nestor-runner-service: stopped (signal {signal}).");
            std::process::exit(0);
        });
    }
    {
        let service = Arc::clone(&service);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            service.sample_load();
        });
    }
    {
        let supervisor = Arc::clone(&supervisor);
        let clock = clock.clone();
        std::thread::spawn(move || {
            let mut demo = simulate.then(|| DemoQueue::new(Arc::clone(&supervisor), fake, nestor));
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if let Some(demo) = demo.as_mut() {
                    demo.step(clock());
                }
                let queue = associator.tick();
                supervisor.tick(&queue);
            }
        });
    }

    ipc::serve(&bound.listener, service);
    Ok(())
}
