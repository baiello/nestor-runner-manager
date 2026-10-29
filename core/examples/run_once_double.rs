//! A test double of `nestor-runner run-once` that follows launch contract v1 as written in the
//! vendored fixtures (`contract/run-once-v1.json`), for the tests of the real launcher. TEST
//! ONLY: it claims nothing from Nestor. The real runner is exercised by the cross-repository
//! contract test (R2).
//!
//! What it checks and does, as the contract asks of the runner: refuses an `argv` that breaks the
//! fixture (exit 64: unknown, repeated or forbidden option such as `--scope`, wrong format, fd
//! other than 3/4); marks fd 4 close-on-exec and closes every fd from 5 up before anything else;
//! reads the credential JSON on fd 3 to its end (exit 65 if it is not the fixture's shape) and
//! closes fd 3; reports `hello`, then `claimed`/`finished` or `idle` on fd 4; treats `drain\n` or
//! the end of its input as a drain.
//!
//! Test behaviour comes from `--double-*` options placed before the contract's options (never
//! used in production): `--double-claim JOB`, `--double-job-ms N`, `--double-finish OUTCOME`,
//! `--double-idle-ms N`, `--double-exit CODE`, `--double-hello-generation N` (lies in `hello`),
//! `--double-hostile` (forged and oversized reports), `--double-deaf` (ignores SIGTERM),
//! `--double-tree` (starts processes that leave its group), `--double-connect SOCKET`,
//! `--double-mark-credential` (creates `credential-received` in its directory as soon as a byte
//! arrives on fd 3).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

const FIXTURE: &str = include_str!("../../contract/run-once-v1.json");
const DOUBLE_VALUED: [&str; 8] = [
    "--double-failure",
    "--double-claim",
    "--double-job-ms",
    "--double-finish",
    "--double-idle-ms",
    "--double-exit",
    "--double-hello-generation",
    "--double-connect",
];
const DOUBLE_FLAGS: [&str; 5] =
    ["--double-hostile", "--double-deaf", "--double-tree", "--double-mark-credential", "--double-exec-sleep"];

fn usage(reason: &str) -> ! {
    eprintln!("contract-argv=refused {reason}");
    std::process::exit(64);
}

fn is_hex(text: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&text.len()) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The `formats` of the fixture, by option.
fn well_formed(option: &str, value: &str) -> bool {
    match option {
        "--runner-id" => is_hex(value, 8, 64),
        "--generation" => !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit()),
        "--machine-id" => {
            let groups: Vec<&str> = value.split('-').collect();
            groups.len() == 5 && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| is_hex(g, n, n))
        }
        "--server" => {
            value.strip_prefix("https://").is_some_and(|host| !host.is_empty() && !host.contains(['/', '@', '?', '#']))
        }
        "--idle-seconds" => value.parse::<u32>().is_ok_and(|n| (5..=600).contains(&n)),
        _ => true,
    }
}

/// Splits the `argv` into the double's own options and the contract's, checked against the
/// fixture.
fn parse(arguments: &[String], fixture: &Value) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let argv = &fixture["argv"];
    let list = |name: &str| -> Vec<String> {
        argv[name].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
    };
    let (required, optional, forbidden) = (list("required"), list("optional"), list("forbidden"));
    let subcommand = list("subcommand");
    if arguments.get(..subcommand.len()) != Some(&subcommand[..]) {
        usage("missing subcommand");
    }
    let mut double = BTreeMap::new();
    let mut contract = BTreeMap::new();
    let mut rest = arguments[subcommand.len()..].iter();
    while let Some(option) = rest.next() {
        if DOUBLE_FLAGS.contains(&option.as_str()) {
            double.insert(option.clone(), String::new());
            continue;
        }
        if DOUBLE_VALUED.contains(&option.as_str()) {
            double.insert(option.clone(), rest.next().cloned().unwrap_or_else(|| usage("double option without value")));
            continue;
        }
        if forbidden.contains(option) {
            usage(&format!("forbidden option {option}"));
        }
        if !required.contains(option) && !optional.contains(option) {
            usage(&format!("unknown option {option}"));
        }
        let value = rest.next().unwrap_or_else(|| usage(&format!("{option} without value")));
        if contract.insert(option.clone(), value.clone()).is_some() {
            usage(&format!("repeated option {option}"));
        }
        if !well_formed(option, value) {
            usage(&format!("malformed {option}"));
        }
    }
    for option in &required {
        if !contract.contains_key(option) {
            usage(&format!("missing {option}"));
        }
    }
    for (option, value) in argv["fixed"].as_object().unwrap() {
        if contract.get(option).map(String::as_str) != value.as_str() {
            usage(&format!("{option} must be {value}"));
        }
    }
    (double, contract)
}

fn open_fds() -> Vec<i32> {
    (0..256).filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1).collect()
}

/// Reads the credential like the runner: to the end, bounded, strict shape.
fn read_credential(fixture: &Value, mark: bool) -> String {
    let limit = fixture["credential"]["max_bytes"].as_u64().unwrap();
    let mut raw = Vec::new();
    let mut source = unsafe { File::from_raw_fd(3) };
    if mark {
        // Blocks until the service writes or closes the pipe.
        let mut first = [0u8; 1];
        if source.read(&mut first).unwrap_or(0) == 1 {
            let _ = File::create("credential-received");
            raw.push(first[0]);
        }
    }
    let _ = (&mut source).take(limit + 1 - raw.len() as u64).read_to_end(&mut raw);
    drop(source); // Closes fd 3 before any other work.
    let allowed: Vec<&String> = fixture["credential"]["fields"].as_object().unwrap().keys().collect();
    let parsed: Option<String> = (|| {
        if raw.len() as u64 > limit {
            return None;
        }
        let value: Value = serde_json::from_slice(&raw).ok()?;
        let object = value.as_object()?;
        if !object.keys().all(|k| allowed.contains(&k)) {
            return None;
        }
        if object.get("claude_code_token").is_some_and(|v| !v.is_string()) {
            return None;
        }
        object.get("runner_token")?.as_str().map(String::from)
    })();
    parsed.unwrap_or_else(|| {
        eprintln!("credential=refused");
        std::process::exit(65);
    })
}

fn report(out: &mut File, line: &str) {
    let _ = writeln!(out, "{line}");
}

fn spawn_tree() {
    // (a) in another process group; (b) double fork with setsid, so it is no descendant any more
    // when the runner is killed, but still in the runner's directory; (c) double fork with setsid,
    // current directory, open files and environment elsewhere: the residual case the sweep
    // cannot see.
    let a = unsafe {
        Command::new("/bin/sleep")
            .arg("120")
            .pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            })
            .spawn()
            .map(|c| c.id())
            .unwrap_or(0)
    };
    let b = unsafe {
        Command::new("/bin/sh")
            .args(["-c", "/bin/sleep 121 >/dev/null 2>&1 & echo $!"])
            .stdout(Stdio::piped())
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok())
            .unwrap_or(0)
    };
    let c = unsafe {
        Command::new("/bin/sh")
            .args(["-c", "/bin/sleep 122 </dev/null >/dev/null 2>&1 & echo $!"])
            .env_clear()
            .current_dir("/")
            .stdout(Stdio::piped())
            .pre_exec(|| {
                libc::setsid();
                Ok(())
            })
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok())
            .unwrap_or(0)
    };
    eprintln!("tree a={a} b={b} c={c}");
}

fn main() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (double, contract) = parse(&arguments, &fixture);
    let option = |name: &str| double.get(name).cloned();
    let number = |name: &str, default: u64| option(name).and_then(|v| v.parse().ok()).unwrap_or(default);

    // Before anything else: fd 4 close-on-exec, every fd from 5 up closed.
    let inherited = open_fds();
    unsafe { libc::fcntl(4, libc::F_SETFD, libc::FD_CLOEXEC) };
    for fd in inherited.iter().filter(|fd| **fd >= 5) {
        unsafe { libc::close(*fd) };
    }
    let credential = read_credential(&fixture, double.contains_key("--double-mark-credential"));
    let mut out = unsafe { File::from_raw_fd(4) };

    eprintln!("contract-argv=ok");
    eprintln!("inherited-fds={}", inherited.iter().map(|fd| fd.to_string()).collect::<Vec<_>>().join(","));
    let in_env = std::env::vars().any(|(_, v)| v.contains(&credential));
    let in_args = arguments.iter().any(|a| a.contains(&credential));
    let env_names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    eprintln!("credential-in-env={in_env} credential-in-args={in_args} env={}", env_names.join(","));
    eprintln!("echo {credential}");
    eprintln!("credential-fd-closed={}", unsafe { libc::fcntl(3, libc::F_GETFD) } == -1);
    eprintln!("report-fd-cloexec={}", unsafe { libc::fcntl(4, libc::F_GETFD) } & libc::FD_CLOEXEC != 0);

    if double.contains_key("--double-deaf") {
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    if let Some(socket) = option("--double-connect") {
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
            let mut line = String::new();
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            while input.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "drain\n" {
                    eprintln!("drain-line-received");
                    break;
                }
                line.clear();
            }
            drained.store(true, Ordering::SeqCst);
        });
    }

    let generation = option("--double-hello-generation").unwrap_or_else(|| contract["--generation"].clone());
    report(
        &mut out,
        &format!(
            r#"{{"v":1,"type":"hello","contract":1,"runner_id":"{}","generation":{generation},"pid":{}}}"#,
            contract["--runner-id"],
            std::process::id()
        ),
    );
    if let Some(code) = option("--double-exit") {
        std::process::exit(code.parse().unwrap_or(70));
    }
    if double.contains_key("--double-tree") {
        spawn_tree();
    }
    if double.contains_key("--double-exec-sleep") {
        // Becomes another program: same pid, an argv without the launch's marker.
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new("/bin/sleep").arg("30").exec();
        usage(&format!("exec: {error}"));
    }
    if double.contains_key("--double-hostile") {
        report(&mut out, &format!(r#"{{"v":1,"type":"idle","pad":"{}"}}"#, "x".repeat(10_000)));
        report(&mut out, r#"{"v":1,"type":"scope","scope":"other"}"#);
        report(&mut out, r#"{"v":1,"type":"claimed","job_id":"forged","repository":"https://example.invalid/r.git"}"#);
        report(&mut out, r#"{"event":"claimed","taskRef":"old-format"}"#);
        report(&mut out, "\u{0}\u{1} not json");
    }

    let idle = Duration::from_millis(number("--double-idle-ms", 30_000));
    let start = Instant::now();
    if let Some(job) = option("--double-claim") {
        std::thread::sleep(Duration::from_millis(50));
        if drained.load(Ordering::SeqCst) {
            report(&mut out, r#"{"v":1,"type":"idle"}"#);
            return;
        }
        report(&mut out, &format!(r#"{{"v":1,"type":"claimed","job_id":"{job}"}}"#));
        if double.contains_key("--double-hostile") {
            report(&mut out, r#"{"v":1,"type":"claimed","job_id":"second-claim"}"#);
            for _ in 0..40 {
                report(&mut out, r#"{"v":1,"type":"finished","outcome":"failed"}"#);
            }
        }
        // The job runs to its end even if the input closes meanwhile.
        std::thread::sleep(Duration::from_millis(number("--double-job-ms", 200)));
        eprintln!("job done, drained-meanwhile={}", drained.load(Ordering::SeqCst));
        if !double.contains_key("--double-hostile") {
            let outcome = option("--double-finish").unwrap_or_else(|| "done".into());
            match option("--double-failure") {
                Some(failure) => report(
                    &mut out,
                    &format!(r#"{{"v":1,"type":"finished","outcome":"{outcome}","failure":"{failure}"}}"#),
                ),
                None => report(&mut out, &format!(r#"{{"v":1,"type":"finished","outcome":"{outcome}"}}"#)),
            }
        }
        return;
    }
    while !drained.load(Ordering::SeqCst) && start.elapsed() < idle {
        std::thread::sleep(Duration::from_millis(20));
    }
    report(&mut out, r#"{"v":1,"type":"idle"}"#);
}
