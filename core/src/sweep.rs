//! Stopping what a runner left behind after it was killed (M3, KCA-734 §4).
//!
//! A process group is not enough: the runner puts Claude Code in a group of its own, and a
//! process can leave its group (`setsid`). Walking the parents after the kill is useless too: the
//! orphans have been handed to `launchd` by then. So the service sweeps in two steps:
//!
//! 1. **Before** the `SIGKILL` (end of the grace period), a [`snapshot`]: the runner's
//!    descendants (by parent, recursively) and every process of the same user that is tied to the
//!    runner's directory: its current directory, an open file, or its `HOME` or `TMPDIR` is under
//!    it. Each with its pid, start time and process group. (Recent macOS versions no longer show
//!    another process's environment, so there `HOME`/`TMPDIR` cannot be read; the current
//!    directory and open files still can.)
//! 2. **After** it, [`sweep`]: each process of the snapshot is killed, then its group, but only if
//!    pid and start time still match (a reused pid is never signalled); plus every process of the
//!    same user found under the runner's directory again. Up to three passes; what survives is
//!    reported, never treated as a success.
//!
//! **Residual risk** (documented, accepted for this slice): a descendant that leaves the group
//! *and* moves its current directory out of the runner's directory without keeping a file open in
//! it (and, where the environment is readable, changes `HOME` and `TMPDIR`) before the snapshot
//! escapes. The real
//! barrier is a sandbox or a dedicated user account, out of this slice.

use std::path::Path;
use std::time::Duration;

use crate::sys::{self, ProcessInfo};

/// Passes of a sweep before what survives is reported.
pub const PASSES: u32 = 3;

/// The descendants of `root` (not `root` itself), by parent, recursively.
pub fn descendants(root: u32) -> Vec<ProcessInfo> {
    let all: Vec<ProcessInfo> = sys::all_pids().into_iter().filter_map(sys::process_info).collect();
    let mut found: Vec<ProcessInfo> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for process in all.iter().filter(|p| p.ppid == parent && p.pid != root) {
            if !found.iter().any(|f| f.pid == process.pid) {
                found.push(*process);
                frontier.push(process.pid);
            }
        }
    }
    found
}

/// Whether `path` is `directory` or under it.
fn is_under(path: &Path, directory: &Path) -> bool {
    path.starts_with(directory)
}

/// The live processes of this user, other than this service, whose current directory, an open
/// file, `HOME` or `TMPDIR` is under `directory`.
pub fn under_directory(directory: &Path) -> Vec<ProcessInfo> {
    let me = std::process::id();
    let uid = unsafe { libc::geteuid() };
    // Paths as the kernel reports them (`/private/var/…` rather than `/var/…` on macOS).
    let canonical = directory.canonicalize().ok();
    let directories: Vec<&Path> = [Some(directory), canonical.as_deref()].into_iter().flatten().collect();
    let inside = |path: &Path| directories.iter().any(|d| is_under(path, d));
    sys::all_pids()
        .into_iter()
        .filter(|pid| *pid != me)
        .filter_map(sys::process_info)
        .filter(|p| p.uid == uid && !p.zombie)
        .filter(|p| {
            sys::process_cwd(p.pid).is_some_and(|cwd| inside(&cwd))
                || sys::process_open_paths(p.pid).iter().any(|path| inside(path))
                || sys::process_environment(p.pid).is_some_and(|environment| {
                    environment
                        .iter()
                        .any(|(name, value)| (name == "HOME" || name == "TMPDIR") && inside(Path::new(value)))
                })
        })
        .collect()
}

/// Step 1, at the end of the grace period: the runner's descendants and what lives under its
/// directory.
pub fn snapshot(runner_pid: u32, directory: &Path) -> Vec<ProcessInfo> {
    let mut entries = descendants(runner_pid);
    for process in under_directory(directory) {
        if process.pid != runner_pid && !entries.iter().any(|e| e.pid == process.pid) {
            entries.push(process);
        }
    }
    entries
}

/// Whether `entry` is still the process it was (same pid and start time) and still running.
fn still_there(entry: &ProcessInfo) -> bool {
    sys::process_info(entry.pid).is_some_and(|now| now.start_time_us == entry.start_time_us && !now.zombie)
}

/// Kills what is still there; returns how many processes were signalled.
fn kill(entries: &[ProcessInfo]) -> usize {
    let own_group = unsafe { libc::getpgrp() } as u32;
    let mut signalled = 0;
    for entry in entries {
        if still_there(entry) {
            unsafe { libc::kill(entry.pid as libc::pid_t, libc::SIGKILL) };
            signalled += 1;
        }
    }
    // Then the groups led by a process of the snapshot. Never this service's own group, and never
    // a group whose leader's pid now names another process.
    for entry in entries {
        let group = entry.pgid;
        if group <= 1 || group == own_group {
            continue;
        }
        let Some(leader) = entries.iter().find(|e| e.pid == group) else { continue };
        let reused = sys::process_info(group).is_some_and(|now| now.start_time_us != leader.start_time_us);
        if !reused {
            unsafe { libc::killpg(group as libc::pid_t, libc::SIGKILL) };
        }
    }
    signalled
}

/// Step 2, after the `SIGKILL`: kills the snapshot (identity checked) and what lives under
/// `directory`, up to [`PASSES`] times, `interval` apart. Returns how many processes survived.
pub fn sweep(snapshot: &[ProcessInfo], directory: &Path, interval: Duration) -> usize {
    let remaining = |snapshot: &[ProcessInfo]| -> Vec<ProcessInfo> {
        let mut left: Vec<ProcessInfo> = snapshot.iter().filter(|e| still_there(e)).copied().collect();
        for process in under_directory(directory) {
            if !left.iter().any(|l| l.pid == process.pid) {
                left.push(process);
            }
        }
        left
    };
    let mut left = remaining(snapshot);
    for _ in 0..PASSES {
        if left.is_empty() {
            return 0;
        }
        kill(&left);
        // Wait for the kills to land, checking often.
        let deadline = std::time::Instant::now() + interval;
        loop {
            std::thread::sleep(Duration::from_millis(20).min(interval));
            left = remaining(&left);
            if left.is_empty() || std::time::Instant::now() >= deadline {
                break;
            }
        }
    }
    left.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_reused_pid_is_never_signalled() {
        let mut bystander = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let mut info = sys::process_info(bystander.id()).unwrap();
        // The snapshot saw "this pid" with another start time: another process.
        info.start_time_us -= 1;
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(sweep(&[info], directory.path(), Duration::from_millis(50)), 0);
        assert!(bystander.try_wait().unwrap().is_none(), "the bystander is still running");
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    #[test]
    fn descendants_and_processes_under_the_directory_are_found() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = Command::new("/bin/sleep").arg("30").current_dir(directory.path()).spawn().unwrap();
        let found = under_directory(directory.path());
        assert!(found.iter().any(|p| p.pid == child.id()), "{found:?}");
        assert!(descendants(std::process::id()).iter().any(|p| p.pid == child.id()));
        assert_eq!(sweep(&found, directory.path(), Duration::from_millis(200)), 0);
        child.wait().unwrap();
    }

    #[test]
    fn a_process_elsewhere_with_a_file_open_under_the_directory_is_found() {
        let directory = tempfile::tempdir().unwrap();
        let log = std::fs::File::create(directory.path().join("job.log")).unwrap();
        let mut child = Command::new("/bin/sleep").arg("30").current_dir("/").env_clear().stdout(log).spawn().unwrap();
        assert!(under_directory(directory.path()).iter().any(|p| p.pid == child.id()));
        assert_eq!(sweep(&[], directory.path(), Duration::from_millis(200)), 0);
        child.wait().unwrap();
    }
}
