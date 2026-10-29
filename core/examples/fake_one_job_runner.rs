//! A stand-in for the Nestor runner's "one job" mode (S7), used by the tests of the real
//! launcher. TEST ONLY: it claims nothing from Nestor.
//!
//! It follows the launch interface of `core/src/process.rs`: reads its credential once from
//! fd 3, reports on fd 4, treats the end of its standard input as "drain", exits 0 (or 10 when
//! its job fails). Options: `--claim TASK` (claim a task after 50 ms), `--job-ms N`, `--fail`,
//! `--idle-ms N`, `--connect SOCKET` (try the local API and print the answer).

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn value(arguments: &[String], flag: &str) -> Option<String> {
    arguments.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let mut credential = String::new();
    unsafe { File::from_raw_fd(3) }.read_to_string(&mut credential).unwrap();
    let mut report = unsafe { File::from_raw_fd(4) };

    let in_env = std::env::vars().any(|(_, v)| v.contains(&credential));
    let in_args = arguments.iter().any(|a| a.contains(&credential));
    let env_names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    eprintln!("credential-in-env={in_env} credential-in-args={in_args} env={}", env_names.join(","));
    eprintln!("echo {credential}");
    // The credential pipe was closed after one read: fd 3 is gone.
    eprintln!("credential-fd-closed={}", unsafe { libc::fcntl(3, libc::F_GETFD) } == -1);

    if let Some(socket) = value(&arguments, "--connect") {
        match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(mut stream) => {
                let _ = writeln!(stream, r#"{{"apiVersion":"1.1","id":1,"op":"hello"}}"#);
                let mut line = String::new();
                let _ = BufReader::new(stream).read_line(&mut line);
                eprintln!("api-answer {}", line.trim());
            }
            Err(error) => eprintln!("api-answer error {error}"),
        }
    }

    let drained = Arc::new(AtomicBool::new(false));
    {
        let drained = Arc::clone(&drained);
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut sink);
            drained.store(true, Ordering::SeqCst);
        });
    }
    writeln!(report, r#"{{"event":"ready"}}"#).unwrap();

    let idle = Duration::from_millis(value(&arguments, "--idle-ms").and_then(|v| v.parse().ok()).unwrap_or(30_000));
    let start = Instant::now();
    if let Some(task) = value(&arguments, "--claim") {
        std::thread::sleep(Duration::from_millis(50));
        if drained.load(Ordering::SeqCst) {
            return;
        }
        let scope = value(&arguments, "--scope").map(|s| format!(r#","scope":"{s}""#)).unwrap_or_default();
        writeln!(report, r#"{{"event":"claimed","taskRef":"{task}"{scope}}}"#).unwrap();
        let job = Duration::from_millis(value(&arguments, "--job-ms").and_then(|v| v.parse().ok()).unwrap_or(200));
        // The job runs to its end even if the input closes meanwhile.
        std::thread::sleep(job);
        eprintln!("job done, drained-meanwhile={}", drained.load(Ordering::SeqCst));
        if arguments.iter().any(|a| a == "--fail") {
            std::process::exit(10);
        }
        return;
    }
    while !drained.load(Ordering::SeqCst) && start.elapsed() < idle {
        std::thread::sleep(Duration::from_millis(20));
    }
}
