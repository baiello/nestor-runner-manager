//! The local runner service, run in the user's session (no privileged installation).
//!
//! ```text
//! nestor-runner-service [--data-dir DIR] [--socket PATH] [--simulate]
//! ```
//!
//! Without `--simulate` the service is `live`: real configuration, persistence, admission state,
//! machine measurements and local API, but no association with Nestor and no runner yet (that
//! needs the Nestor API work), so admissions stay blocked as `notAssociated`.
//! With `--simulate` it is `demo`: the association, the queue and the runner processes are
//! simulated, everything else is real. Its state is kept in a separate `demo` directory.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nestor_runner_core::demo::DemoQueue;
use nestor_runner_core::fake::{FakeLauncher, UnavailableLauncher};
use nestor_runner_core::ipc::{self, InstanceLock, Service};
use nestor_runner_core::model::ServiceMode;
use nestor_runner_core::persist::FileStore;
use nestor_runner_core::supervisor::{Clock, Launcher, Options, Supervisor};

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

fn run() -> Result<(), String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut simulate = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--data-dir" => data_dir = arguments.next().map(PathBuf::from),
            "--socket" => socket = arguments.next().map(PathBuf::from),
            "--simulate" => simulate = true,
            "--help" | "-h" => {
                println!("usage: nestor-runner-service [--data-dir DIR] [--socket PATH] [--simulate]");
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let mode = if simulate { ServiceMode::Demo } else { ServiceMode::Live };
    let data_dir = data_dir.unwrap_or_else(|| default_root().join(if simulate { "demo" } else { "live" }));
    let socket = socket.unwrap_or_else(|| data_dir.join("run/service.sock"));

    let _lock = InstanceLock::acquire(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let store = Arc::new(FileStore::new(&data_dir).map_err(|e| e.to_string())?);
    let clock: Clock =
        Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0));
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let fake = Arc::new(FakeLauncher::new());
    let launcher: Arc<dyn Launcher> = if simulate { fake.clone() } else { Arc::new(UnavailableLauncher) };

    let supervisor = Arc::new(Supervisor::open(store, launcher, clock.clone(), Options::new(mode, cores)));
    supervisor.reconcile();
    let service = Arc::new(Service::new(Arc::clone(&supervisor), clock.clone()));
    service.sample_load();

    let listener = ipc::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    eprintln!(
        "nestor-runner-service: {} mode, state in {}, listening on {}",
        if simulate { "DEMO (simulated association, queue and runners)" } else { "live" },
        data_dir.display(),
        socket.display()
    );

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
            let mut queue = simulate.then(|| DemoQueue::new(Arc::clone(&supervisor), fake));
            loop {
                std::thread::sleep(Duration::from_secs(1));
                match queue.as_mut() {
                    Some(queue) => queue.step(clock()),
                    // No Nestor connection yet: nothing is eligible, idle runners are stopped.
                    None => supervisor.tick(0),
                }
            }
        });
    }

    ipc::serve(listener, service);
    Ok(())
}
