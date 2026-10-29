//! The local runner service, run in the user's session (no privileged installation).
//!
//! ```text
//! nestor-runner-service [--data-dir DIR] [--socket PATH] [--simulate] [--allow-unsigned-clients]
//! ```
//!
//! Without `--simulate` the service is `live`: real configuration, persistence, admission state,
//! machine measurements and local API, with secrets in the Keychain. Nestor's API for machines
//! does not exist yet, so `associate` answers `unavailable` and no runner is launched.
//! With `--simulate` it is `demo`: Nestor, its tasks and the runner processes are simulated,
//! secrets stay in memory, everything else is real. Its state is kept in a separate directory.
//!
//! `--allow-unsigned-clients` (development only, off by default) gives unsigned clients of the
//! same user full access to the local API; the service reports it to every client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nestor_runner_core::association::Associator;
use nestor_runner_core::demo::{DemoQueue, DEMO_INSTANCE};
use nestor_runner_core::fake::{FakeLauncher, UnavailableLauncher};
use nestor_runner_core::ipc::{self, BoundSocket, InstanceLock, PeerPolicy, Service};
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

fn main() {
    if let Err(message) = run() {
        eprintln!("nestor-runner-service: {message}");
        std::process::exit(1);
    }
}

fn default_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/Nestor Runner Manager")
}

fn requirement(identifier: &str) -> Option<String> {
    option_env!("NESTOR_TEAM_ID").map(|team| {
        format!("anchor apple generic and identifier \"{identifier}\" and certificate leaf[subject.OU] = \"{team}\"")
    })
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

fn run() -> Result<(), String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut simulate = false;
    let mut allow_unsigned_clients = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--data-dir" => data_dir = arguments.next().map(PathBuf::from),
            "--socket" => socket = arguments.next().map(PathBuf::from),
            "--simulate" => simulate = true,
            "--allow-unsigned-clients" => allow_unsigned_clients = true,
            "--help" | "-h" => {
                println!(
                    "usage: nestor-runner-service [--data-dir DIR] [--socket PATH] [--simulate] [--allow-unsigned-clients]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let mode = if simulate { ServiceMode::Demo } else { ServiceMode::Live };
    let data_dir = data_dir.unwrap_or_else(|| default_root().join(if simulate { "demo" } else { "live" }));
    let socket = socket.unwrap_or_else(|| data_dir.join("run/service.sock"));
    let signals = block_stop_signals();

    let _lock = InstanceLock::acquire(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let store = Arc::new(FileStore::new(&data_dir).map_err(|e| e.to_string())?);
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

    let bound = ipc::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
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
