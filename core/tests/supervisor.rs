//! Acceptance tests of the admission and lifecycle rules (contract v2 §2, §3, S8.1, S8.5, S8.7),
//! on fake "one job" runners with a hand-moved clock: deterministic, no real process.

mod common;

use std::sync::Arc;
use std::thread;

use common::{queue, Harness, SCOPE};
use nestor_runner_core::model::*;

#[test]
fn defaults_to_one_task_at_a_time_and_automatic() {
    let harness = Harness::new();
    let status = harness.supervisor.status();
    assert_eq!(status.max_concurrent_tasks, 1);
    assert_eq!(status.mode, Mode::Automatic);
    assert_eq!(status.admission, Admission::Blocked(BlockReason::Reconciling), "nothing before reconciliation");
    harness.supervisor.tick(&queue(5));
    assert_eq!(harness.launcher.spawns(), 0);
    harness.supervisor.reconcile();
    assert_eq!(harness.supervisor.status().admission, Admission::Blocked(BlockReason::NotAssociated));
    harness.supervisor.tick(&queue(5));
    assert_eq!(harness.launcher.spawns(), 0, "nothing is launched while not associated");
    harness.associate();
    assert_eq!(harness.supervisor.status().admission, Admission::Open);
    harness.supervisor.tick(&queue(5));
    assert_eq!(harness.launcher.spawns(), 1, "one runner for one task at a time");
}

#[test]
fn launching_is_the_admission_and_respects_the_ceiling_under_load() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    for _ in 0..10 {
        harness.round(&queue(20));
        let status = harness.supervisor.status();
        assert!(status.live_runners <= 3, "live {} > ceiling 3", status.live_runners);
    }
    assert_eq!(harness.busy(), 3);
    assert_eq!(harness.supervisor.status().active_tasks.len(), 3);
    // One runner per admitted task, not one per queued task.
    assert_eq!(harness.launcher.spawns(), 3);
    // Nothing eligible: nothing launched.
    let harness = Harness::associated();
    harness.supervisor.tick(&Queue::empty());
    assert_eq!(harness.launcher.spawns(), 0);
}

#[test]
fn a_one_job_runner_exits_after_its_task_and_is_launched_again_for_the_next() {
    let harness = Harness::associated();
    let claimed = harness.round(&queue(2));
    assert_eq!(claimed.len(), 1);
    let runner = harness.supervisor.runner(&claimed[0]).unwrap();
    // A second claim is refused: one job per runner.
    let error = harness.supervisor.runner_claimed(&runner.id, runner.generation, "other", Some(SCOPE)).unwrap_err();
    assert_eq!(error.code(), "invalidTransition");
    harness.finish(&runner.id);
    let stopped = harness.supervisor.runner(&runner.id).unwrap();
    assert_eq!(stopped.state, RunnerState::Stopped);
    assert!(stopped.failure_reason.is_none(), "exiting after its task is not a crash");
    // The next task: the same runner identity, a new generation; a stale report is ignored.
    harness.round(&queue(1));
    let again = harness.supervisor.runner(&runner.id).unwrap();
    assert_eq!(again.generation, runner.generation + 1);
    assert_eq!(again.state, RunnerState::Busy);
    harness.supervisor.runner_exited(&runner.id, runner.generation, RunnerExit::Crashed("late".into()));
    assert_eq!(harness.supervisor.runner(&runner.id).unwrap().state, RunnerState::Busy);
    assert_eq!(harness.supervisor.runners().len(), 1);
}

#[test]
fn ceiling_holds_with_concurrent_launches() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    // Eight runners added by hand, then a race of hand starts and automatic launches.
    let manual: Vec<String> =
        (0..8).map(|n| harness.supervisor.add_runner(&format!("r{n}"), false).unwrap().id).collect();
    let handles: Vec<_> = (0..32)
        .map(|n| {
            let supervisor = Arc::clone(&harness.supervisor);
            let id = manual[n % manual.len()].clone();
            thread::spawn(move || {
                if n % 2 == 0 {
                    supervisor.tick(&queue(10));
                } else {
                    let _ = supervisor.start_runner(&id);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(harness.launcher.duplicate_launches(), 0);
    assert!(harness.launcher.live().len() <= 4, "{} live > 4", harness.launcher.live().len());
    assert_eq!(harness.supervisor.status().live_runners, harness.launcher.live().len() as u32);
    for runner in harness.supervisor.runners() {
        assert!(harness.launcher.live_for(&runner.id).len() <= 1, "{} has two processes", runner.name);
    }
}

#[test]
fn pause_stops_launching_and_drains_idle_runners_without_interrupting_tasks() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    harness.supervisor.tick(&queue(3));
    harness.bring_up();
    // Two runners got a task; the third is up and waiting.
    let runners = harness.supervisor.runners();
    for runner in &runners[..2] {
        harness
            .supervisor
            .runner_claimed(&runner.id, runner.generation, &format!("t-{}", runner.name), Some(SCOPE))
            .unwrap();
    }
    let waiting = runners[2].id.clone();

    let status = harness.supervisor.set_mode(Mode::Paused, None).unwrap();
    assert_eq!(status.admission, Admission::Paused);
    // The waiting runner is drained (it will not claim); the busy ones are left alone.
    let drained: Vec<_> = harness.launcher.drained().into_iter().map(|p| p.runner_id).collect();
    assert_eq!(drained, vec![waiting.clone()]);
    assert!(harness.launcher.terminated().is_empty(), "pausing terminates nothing");
    assert_eq!(status.active_tasks.len(), 2);
    harness.complete_drains();
    assert_eq!(harness.supervisor.runner(&waiting).unwrap().state, RunnerState::Stopped);

    // A task ends: nothing new is launched while paused.
    harness.finish(&runners[0].id);
    harness.supervisor.tick(&queue(5));
    assert_eq!(harness.launcher.spawns(), 3, "no runner is launched while paused");
    assert_eq!(harness.busy(), 1);

    // Resume: launches again, within the ceiling, next to the busy runner.
    harness.supervisor.set_mode(Mode::Automatic, None).unwrap();
    assert_eq!(harness.round(&queue(5)).len(), 2);
    assert_eq!(harness.busy(), 3);
}

#[test]
fn lowering_the_ceiling_interrupts_nothing() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    let busy = harness.round(&queue(10));
    assert_eq!(busy.len(), 3);

    let status = harness.supervisor.set_max_concurrent_tasks(1, None).unwrap();
    assert_eq!(status.busy_runners, 3, "busy runners above the new ceiling keep their task");
    assert!(harness.launcher.drained().is_empty() && harness.launcher.terminated().is_empty());

    // Two tasks end: still at the ceiling (1 busy), nothing new is launched.
    harness.finish(&busy[0]);
    harness.finish(&busy[1]);
    assert!(harness.round(&queue(10)).is_empty());
    // The last one ends: one launch again.
    harness.finish(&busy[2]);
    assert_eq!(harness.round(&queue(10)).len(), 1);
    assert_eq!(harness.busy(), 1);
}

#[test]
fn lowering_the_ceiling_drains_waiting_runners_above_it() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    harness.supervisor.tick(&queue(3));
    harness.bring_up();
    harness.supervisor.set_max_concurrent_tasks(1, None).unwrap();
    assert_eq!(harness.launcher.drained().len(), 2, "two waiting runners above the new ceiling are drained");
    assert!(harness.launcher.terminated().is_empty());
}

#[test]
fn raising_the_ceiling_applies_at_the_next_launch() {
    let harness = Harness::associated();
    assert_eq!(harness.round(&queue(5)).len(), 1);
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    assert_eq!(harness.round(&queue(5)).len(), 2);
    assert_eq!(harness.busy(), 3);
}

#[test]
fn waiting_runners_are_drained_after_the_idle_delay() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    harness.supervisor.tick(&queue(2));
    harness.bring_up();
    assert_eq!(harness.count(RunnerState::Ready), 2);

    // The queue empties. Before the delay: kept.
    harness.supervisor.tick(&Queue::empty());
    harness.advance(59_999);
    harness.supervisor.tick(&Queue::empty());
    assert!(harness.launcher.drained().is_empty());

    harness.advance(1);
    harness.supervisor.tick(&Queue::empty());
    assert_eq!(harness.count(RunnerState::Stopping), 2);
    assert!(harness.launcher.terminated().is_empty(), "drained, not terminated");
    harness.complete_drains();
    assert_eq!(harness.count(RunnerState::Stopped), 2);
    assert!(harness.launcher.live().is_empty());
}

#[test]
fn a_crashing_runner_is_launched_again_with_backoff_then_fails() {
    let harness = Harness::associated();
    let runner_id = harness.round(&queue(1)).remove(0);

    // The process dies mid-task: the task is reported lost.
    let process = harness.launcher.live_for(&runner_id).remove(0);
    harness.launcher.end(process.pid);
    harness.supervisor.runner_exited(&runner_id, process.generation, RunnerExit::Crashed("status 1".into()));
    assert_eq!(harness.busy(), 0);
    assert!(harness.supervisor.events_since(0).unwrap().iter().any(|e| matches!(
        &e.kind,
        EventKind::TaskFinished { outcome: TaskOutcome::RunnerLost, task_ref: Some(_), .. }
    )));

    // Not launched again before the backoff (1 s).
    harness.supervisor.tick(&queue(1));
    assert_eq!(harness.launcher.spawns(), 1);
    harness.advance(1_000);
    harness.supervisor.tick(&queue(1));
    assert_eq!(harness.launcher.spawns(), 2);

    // Keeps crashing: failed after five in a row, not launched any more.
    for _ in 0..4 {
        let current = harness.launcher.live_for(&runner_id).remove(0);
        harness.launcher.end(current.pid);
        harness.supervisor.runner_exited(&runner_id, current.generation, RunnerExit::Crashed("status 1".into()));
        harness.advance(300_000);
        harness.supervisor.tick(&queue(1));
    }
    assert_eq!(harness.launcher.spawns(), 5);
    assert!(harness.launcher.live().is_empty(), "no replacement runner beyond the ceiling");
    let failed = harness.supervisor.runner(&runner_id).unwrap();
    assert_eq!(failed.state, RunnerState::Failed);
    assert!(failed.failure_reason.unwrap().contains("5 times"));
}

#[test]
fn stopping_a_busy_runner_needs_force_and_terminates_it() {
    let harness = Harness::associated();
    let runner_id = harness.round(&queue(1)).remove(0);
    let error = harness.supervisor.stop_runner(&runner_id, false).unwrap_err();
    assert_eq!(error.code(), "invalidTransition");
    harness.supervisor.stop_runner(&runner_id, true).unwrap();
    assert_eq!(harness.launcher.terminated().len(), 1, "a forced stop terminates");
    // Still alive after the grace period: killed.
    harness.advance(10_000);
    harness.supervisor.tick(&queue(1));
    assert!(harness.launcher.terminated()[0].killed);
    harness.complete_terminations();
    let runner = harness.supervisor.runner(&runner_id).unwrap();
    assert_eq!(runner.state, RunnerState::Stopped);
    assert!(runner.failure_reason.is_none(), "a requested stop is not a crash");
    assert!(harness
        .supervisor
        .events_since(0)
        .unwrap()
        .iter()
        .any(|e| matches!(&e.kind, EventKind::TaskFinished { outcome: TaskOutcome::Stopped, .. })));
    // Stopped by hand: not launched again on its own.
    harness.advance(120_000);
    harness.supervisor.tick(&queue(1));
    assert_eq!(harness.supervisor.runner(&runner_id).unwrap().state, RunnerState::Stopped);
}

#[test]
fn survivors_of_the_sweep_leave_the_runner_failed_and_keep_its_slot() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    let runner_id = harness.round(&queue(1)).remove(0);
    let process = harness.launcher.live_for(&runner_id).remove(0);
    harness.supervisor.stop_runner(&runner_id, true).unwrap();
    // It handled SIGTERM and exited 143; the sweep after its exit could not stop one process.
    harness.launcher.end(process.pid);
    harness.supervisor.runner_exited_after_sweep(
        &runner_id,
        process.generation,
        RunnerExit::Crashed("status 143".into()),
        1,
    );
    let runner = harness.supervisor.runner(&runner_id).unwrap();
    assert_eq!(runner.state, RunnerState::Failed, "never a silent success");
    assert!(runner.failure_reason.unwrap().contains("could not be stopped"));

    // Its slot is not freed: no runner of another scope starts beside what is left.
    harness.advance(600_000);
    harness.supervisor.tick(&Queue::single("scope-b", 3));
    assert_eq!(harness.launcher.spawns(), 1, "nothing launched for another scope");
    harness.supervisor.set_max_concurrent_tasks(1, None).unwrap();
    harness.supervisor.tick(&queue(3));
    assert_eq!(harness.launcher.spawns(), 1, "it still counts against the ceiling");
    let other = harness.supervisor.add_runner("other", false).unwrap();
    assert!(harness.supervisor.start_runner(&other.id).is_err());

    // Started or removed again only once a new sweep finds nothing left.
    harness.launcher.set_leftovers(1);
    assert!(harness.supervisor.start_runner(&runner_id).is_err());
    assert!(harness.supervisor.remove_runner(&runner_id).is_err());
    assert!(harness.supervisor.runner(&runner_id).is_some(), "its directory is kept");
    assert_eq!(harness.launcher.spawns(), 1);
    harness.launcher.set_leftovers(0);
    harness.supervisor.start_runner(&runner_id).unwrap();
    assert_eq!(harness.launcher.spawns(), 2);
    assert_eq!(harness.launcher.sweeps(), vec![runner_id.clone(); 3], "start, remove, start: each sweeps first");
}

#[test]
fn survivors_keep_a_revoked_runner_until_they_are_stopped() {
    let harness = Harness::associated();
    let runner_id = harness.round(&queue(1)).remove(0);
    harness.supervisor.end_association(Connection::Revoked);
    let process = harness.launcher.live_for(&runner_id).remove(0);
    harness.launcher.end(process.pid);
    harness.supervisor.runner_exited_after_sweep(
        &runner_id,
        process.generation,
        RunnerExit::Crashed("status 143".into()),
        2,
    );
    let runner = harness.supervisor.runner(&runner_id).expect("not erased while something it started runs");
    assert_eq!(runner.state, RunnerState::Failed);
    assert!(!harness.launcher.forgotten().contains(&runner_id), "its directory is kept");
    // Ending the association again erases stopped runners, not this one.
    harness.supervisor.end_association(Connection::NotAssociated);
    assert!(harness.supervisor.runner(&runner_id).is_some());
    assert!(!harness.launcher.forgotten().contains(&runner_id));
}

#[test]
fn configuration_changes_are_versioned() {
    let harness = Harness::associated();
    let revision = harness.supervisor.status().config_revision;
    let status = harness.supervisor.set_max_concurrent_tasks(2, Some(revision)).unwrap();
    assert_eq!(status.config_revision, revision + 1);
    let error = harness.supervisor.set_mode(Mode::Paused, Some(revision)).unwrap_err();
    assert_eq!(error, ServiceError::Conflict { current_revision: revision + 1 });
    assert!(error.retryable());
    assert_eq!(harness.supervisor.status().mode, Mode::Automatic);
    for value in [0, 9] {
        assert_eq!(
            harness.supervisor.set_max_concurrent_tasks(value, None).unwrap_err(),
            ServiceError::LimitOutOfRange { min: 1, max: 8 }
        );
    }
}

#[test]
fn losing_the_connection_stops_launching_but_not_running_tasks() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    harness.supervisor.tick(&queue(2));
    harness.bring_up();
    let runner = harness.supervisor.runners().remove(0);
    harness.supervisor.runner_claimed(&runner.id, runner.generation, "t1", Some(SCOPE)).unwrap();
    harness
        .supervisor
        .set_connection(Connection::Unreachable { instance: "test.invalid".into(), reason: "offline".into() });
    assert_eq!(harness.supervisor.status().admission, Admission::Blocked(BlockReason::Unreachable));
    assert_eq!(harness.supervisor.status().active_tasks.len(), 1);
    assert!(harness.launcher.terminated().is_empty());
    assert_eq!(harness.launcher.drained().len(), 1, "the waiting runner is drained");
    harness.finish(&runner.id);
    harness.supervisor.tick(&queue(5));
    assert_eq!(harness.launcher.spawns(), 2);
}

#[test]
fn tasks_of_different_scopes_never_run_side_by_side() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    let mut mixed = Queue::single("scope-a", 2);
    mixed.eligible.insert("scope-b".into(), 1);
    let first = harness.round(&mixed);
    assert_eq!(first.len(), 2, "only the jobs of the chosen scope");
    assert_eq!(harness.supervisor.status().active_scope.as_deref(), Some("scope-a"));
    for process in harness.launcher.live() {
        assert_eq!(process.scope.as_deref(), Some("scope-a"), "launched with a restrictive scope filter");
    }
    // Only scope-b has work: nothing is launched while scope-a tasks run.
    assert!(harness.round(&Queue::single("scope-b", 5)).is_empty());
    assert_eq!(harness.launcher.spawns(), 2);
    harness.finish(&first[0]);
    assert!(harness.round(&Queue::single("scope-b", 5)).is_empty());
    harness.finish(&first[1]);
    // All scope-a tasks ended: scope-b may start.
    assert_eq!(harness.round(&Queue::single("scope-b", 5)).len(), 4);
    assert_eq!(harness.supervisor.status().active_scope.as_deref(), Some("scope-b"));
}

#[test]
fn a_task_outside_the_runner_scope_is_reported_and_blocks_further_launches() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    harness.supervisor.tick(&Queue::single("scope-a", 1));
    harness.bring_up();
    let runner = harness.supervisor.runners().remove(0);
    // Nestor did not apply the filter: the runner reports a job of scope-b.
    harness.supervisor.runner_claimed(&runner.id, runner.generation, "t1", Some("scope-b")).unwrap();
    assert!(harness
        .supervisor
        .events_since(0)
        .unwrap()
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ScopeViolation { runner_id } if *runner_id == runner.id)));
    assert!(harness.supervisor.logs(&runner.id, 10).unwrap().iter().any(|l| l.level == LogLevel::Error));
    // Its task is not interrupted, but nothing else starts until it ends.
    assert!(harness.launcher.terminated().is_empty());
    harness.supervisor.tick(&Queue::single("scope-a", 5));
    harness.supervisor.tick(&Queue::single("scope-b", 5));
    assert_eq!(harness.launcher.spawns(), 1);
    assert!(harness.supervisor.start_runner(&harness.supervisor.add_runner("manual", false).unwrap().id).is_err());
    harness.finish(&runner.id);
    harness.supervisor.tick(&Queue::single("scope-a", 5));
    assert!(harness.launcher.spawns() > 1);
}

#[test]
fn ending_the_association_terminates_runners_and_erases_them() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    harness.supervisor.tick(&queue(2));
    harness.bring_up();
    let runner = harness.supervisor.runners().remove(0);
    harness.supervisor.runner_claimed(&runner.id, runner.generation, "t1", Some(SCOPE)).unwrap();
    let stopped = harness.supervisor.add_runner("spare", false).unwrap();

    harness.supervisor.end_association(Connection::Revoked);
    let status = harness.supervisor.status();
    assert_eq!(status.admission, Admission::Blocked(BlockReason::Revoked));
    // Revocation is the exception to "never interrupt": every live runner is terminated.
    assert_eq!(harness.launcher.terminated().len(), 2);
    assert!(harness.supervisor.runner(&stopped.id).is_none(), "a stopped runner is erased at once");
    assert!(harness.launcher.forgotten().contains(&stopped.id));
    harness.advance(10_000);
    harness.supervisor.tick(&queue(5));
    assert!(harness.launcher.terminated().iter().all(|p| p.killed), "killed after the grace period");
    harness.complete_terminations();
    assert!(harness.supervisor.runners().is_empty());
    assert!(harness.launcher.forgotten().contains(&runner.id));
    assert_eq!(harness.launcher.spawns(), 2, "nothing launched after a revocation");
    assert!(harness.supervisor.machine_id().is_none());
}

#[test]
fn hand_starts_follow_the_same_regulation() {
    let harness = Harness::associated();
    let manual = harness.supervisor.add_runner("by-hand", false).unwrap();
    harness.supervisor.set_mode(Mode::Paused, None).unwrap();
    assert_eq!(harness.supervisor.start_runner(&manual.id).unwrap_err().code(), "unavailable");
    harness.supervisor.set_mode(Mode::Automatic, None).unwrap();
    harness.round(&queue(1));
    assert_eq!(
        harness.supervisor.start_runner(&manual.id).unwrap_err().code(),
        "unavailable",
        "the ceiling of 1 is taken"
    );
    assert_eq!(harness.supervisor.add_runner("By-Hand", false).unwrap_err().code(), "duplicateName");
    assert_eq!(harness.supervisor.add_runner("caf\u{e9}", false).unwrap_err().code(), "invalidArgument", "ASCII only");
}

#[test]
fn events_can_be_resumed_by_sequence() {
    let harness = Harness::associated();
    let events = harness.supervisor.events_since(0).unwrap();
    let last = events.last().unwrap().seq;
    harness.supervisor.set_mode(Mode::Paused, None).unwrap();
    let newer = harness.supervisor.events_since(last).unwrap();
    assert!(newer.iter().all(|e| e.seq > last));
    assert!(newer.iter().any(|e| matches!(e.kind, EventKind::ConfigChanged { mode: Mode::Paused, .. })));
    assert!(newer.iter().any(|e| matches!(e.kind, EventKind::AdmissionChanged { admission: Admission::Paused })));
}

#[test]
fn runner_output_is_masked_at_the_source() {
    let harness = Harness::associated();
    harness.supervisor.redactor().register("nestor_runner_secret_value_1234");
    let runner_id = harness.round(&queue(1)).remove(0);
    let generation = harness.supervisor.runner(&runner_id).unwrap().generation;
    harness.supervisor.runner_output(
        &runner_id,
        generation,
        "using nestor_runner_secret_value_1234 and token=abcdef123",
    );
    harness.supervisor.report_error("failed with Bearer abcdefghijklmnop");
    let logs = harness.supervisor.logs(&runner_id, 50).unwrap();
    let text = serde_json::to_string(&logs).unwrap()
        + &serde_json::to_string(&harness.supervisor.events_since(0).unwrap()).unwrap();
    assert!(
        !text.contains("secret_value") && !text.contains("abcdef123") && !text.contains("abcdefghijklmnop"),
        "{text}"
    );
    assert!(text.contains("[masked]"));
}

#[test]
fn without_the_server_scope_filter_one_task_runs_at_a_time() {
    let harness = Harness::new();
    harness.supervisor.reconcile();
    harness.associate_without_scope_filter();
    harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    let status = harness.supervisor.status();
    assert_eq!(status.max_concurrent_tasks, 4, "the requested value is kept and shown");
    assert_eq!(status.effective_max_concurrent_tasks, 1);
    assert!(status.capped_by_server);
    assert_eq!(harness.round(&queue(4)).len(), 1);
    harness.round(&queue(4));
    assert_eq!(harness.supervisor.status().live_runners, 1, "never more than one (KCA-734 §3)");

    // Nestor announces its scope filter (S6): the requested ceiling applies.
    harness.supervisor.set_scope_filter(true);
    let status = harness.supervisor.status();
    assert_eq!((status.effective_max_concurrent_tasks, status.capped_by_server), (4, false));
    harness.round(&queue(4));
    assert_eq!(harness.busy(), 4);

    // It stops announcing it: nothing running is interrupted, nothing new starts until one remains.
    harness.supervisor.set_scope_filter(false);
    assert_eq!(harness.busy(), 4);
    let busy: Vec<String> =
        harness.supervisor.runners().into_iter().filter(|r| r.task_ref.is_some()).map(|r| r.id).collect();
    harness.finish(&busy[0]);
    harness.round(&queue(4));
    assert_eq!(harness.supervisor.status().live_runners, 3, "no launch while above the ceiling");
}

#[test]
fn a_refused_credential_blocks_launches_until_the_connection_changes() {
    let harness = Harness::associated();
    harness.round(&queue(1));
    let runner = harness.supervisor.runners().remove(0);
    let process = harness.launcher.live_for(&runner.id).remove(0);
    harness.launcher.end(process.pid);
    harness.supervisor.runner_exited(&runner.id, process.generation, RunnerExit::Revoked);
    let view = harness.supervisor.runner(&runner.id).unwrap();
    assert_eq!(view.state, RunnerState::Failed);
    assert_eq!(harness.supervisor.status().admission, Admission::Blocked(BlockReason::Revoked));
    let spawns = harness.launcher.spawns();
    harness.advance(600_000);
    harness.supervisor.tick(&queue(3));
    assert_eq!(harness.launcher.spawns(), spawns, "no launch, no new runner");

    // The association answers again (or ends and starts over): launches resume.
    harness.supervisor.set_connection(Connection::Unreachable { instance: "test.invalid".into(), reason: "x".into() });
    harness.associate();
    assert_eq!(harness.supervisor.status().admission, Admission::Open);
}

#[test]
fn unreachable_backs_off_without_counting_as_a_crash_and_usage_is_final() {
    let harness = Harness::associated();
    harness.supervisor.tick(&queue(1));
    let runner = harness.supervisor.runners().remove(0);
    for _ in 0..10 {
        let process = harness.launcher.live_for(&runner.id).remove(0);
        harness.launcher.end(process.pid);
        harness.supervisor.runner_exited(&runner.id, process.generation, RunnerExit::Unreachable);
        assert_eq!(harness.supervisor.runner(&runner.id).unwrap().state, RunnerState::Stopped, "never failed");
        harness.advance(600_000);
        harness.supervisor.tick(&queue(1));
    }
    let process = harness.launcher.live_for(&runner.id).remove(0);
    harness.launcher.end(process.pid);
    harness.supervisor.runner_exited(&runner.id, process.generation, RunnerExit::Usage);
    assert_eq!(harness.supervisor.runner(&runner.id).unwrap().state, RunnerState::Failed);
    let spawns = harness.launcher.spawns();
    harness.advance(600_000);
    harness.supervisor.tick(&queue(1));
    assert_eq!(harness.launcher.spawns(), spawns);
}
