//! Acceptance tests of the admission and lifecycle rules (contract KCA-717 §11.2), on fake
//! runners with a hand-moved clock: deterministic, no real process.

mod common;

use std::sync::Arc;
use std::thread;

use common::Harness;
use nestor_runner_core::model::*;

#[test]
fn defaults_to_one_task_at_a_time_and_automatic() {
    let harness = Harness::new();
    let status = harness.supervisor.status();
    assert_eq!(status.max_concurrent_tasks, 1);
    assert_eq!(status.mode, Mode::Automatic);
    assert_eq!(
        status.admission,
        Admission::Blocked(BlockReason::Reconciling),
        "nothing is admitted before reconciliation"
    );
    harness.supervisor.reconcile();
    assert_eq!(harness.supervisor.status().admission, Admission::Blocked(BlockReason::NotAssociated));
    harness.associate();
    assert_eq!(harness.supervisor.status().admission, Admission::Open);
}

#[test]
fn ceiling_is_respected_under_load() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    for _ in 0..10 {
        harness.supervisor.tick(20);
        harness.bring_up();
        harness.admit_all();
        let status = harness.supervisor.status();
        assert!(status.busy_runners <= 3, "busy {} > ceiling 3", status.busy_runners);
    }
    assert_eq!(harness.supervisor.status().busy_runners, 3);
    assert_eq!(harness.supervisor.status().active_tasks.len(), 3);
    // Only as many runners as the ceiling were started, not one per queued task.
    assert_eq!(harness.launcher.live().len(), 3);
}

#[test]
fn ceiling_holds_with_concurrent_admissions() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    // Eight runners up by hand (diagnostics), all ready, then race for four slots.
    for n in 0..8 {
        harness.supervisor.add_runner(&format!("r{n}"), true).unwrap();
    }
    harness.bring_up();
    let runners = harness.supervisor.runners();
    let handles: Vec<_> = runners
        .into_iter()
        .map(|runner| {
            let supervisor = Arc::clone(&harness.supervisor);
            thread::spawn(move || supervisor.admit(&runner.id, runner.generation).is_ok())
        })
        .collect();
    let admitted = handles.into_iter().map(|h| h.join().unwrap()).filter(|ok| *ok).count();
    assert_eq!(admitted, 4);
    assert_eq!(harness.supervisor.status().busy_runners, 4);
}

#[test]
fn pause_stops_admissions_without_interrupting_tasks() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    harness.supervisor.tick(5);
    harness.bring_up();
    let tokens = harness.admit_all();
    assert_eq!(tokens.len(), 2);

    let status = harness.supervisor.set_mode(Mode::Paused, None).unwrap();
    assert_eq!(status.admission, Admission::Paused);
    // Nothing was asked to stop and both tasks are still there.
    assert!(harness.launcher.terminating().is_empty());
    assert_eq!(status.active_tasks.len(), 2);

    // A task ends: its runner becomes idle but gets no new task while paused.
    harness.supervisor.task_finished(&tokens[0], TaskOutcome::Succeeded).unwrap();
    let ready = harness.supervisor.runner(&tokens[0].runner_id).unwrap();
    assert_eq!(ready.state, RunnerState::Ready);
    assert!(harness.supervisor.admit(&ready.id, ready.generation).is_err());
    harness.supervisor.tick(5);
    assert_eq!(harness.launcher.spawns(), 2, "no runner is started while paused");

    // Idle runners are stopped once the idle delay has passed; the busy one is left alone.
    harness.advance(60_000);
    harness.supervisor.tick(5);
    let stopping: Vec<_> = harness.launcher.terminating().into_iter().map(|p| p.runner_id).collect();
    assert_eq!(stopping, vec![tokens[0].runner_id.clone()]);
    assert_eq!(harness.supervisor.runner(&tokens[1].runner_id).unwrap().state, RunnerState::Busy);

    // Resume: admissions open again.
    harness.complete_stops();
    harness.supervisor.set_mode(Mode::Automatic, None).unwrap();
    harness.supervisor.tick(5);
    harness.bring_up();
    assert_eq!(harness.admit_all().len(), 1, "one free slot next to the busy runner");
}

#[test]
fn lowering_the_ceiling_interrupts_nothing() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    harness.supervisor.tick(10);
    harness.bring_up();
    let tokens = harness.admit_all();
    assert_eq!(tokens.len(), 3);

    let status = harness.supervisor.set_max_concurrent_tasks(1, None).unwrap();
    assert_eq!(status.busy_runners, 3, "busy runners above the new ceiling keep their task");
    assert!(harness.launcher.terminating().is_empty());

    // Two tasks end: still at the ceiling (1 busy), nothing new is admitted.
    harness.supervisor.task_finished(&tokens[0], TaskOutcome::Succeeded).unwrap();
    harness.supervisor.task_finished(&tokens[1], TaskOutcome::Succeeded).unwrap();
    assert!(harness.admit_all().is_empty());
    assert_eq!(harness.supervisor.status().busy_runners, 1);

    // The last one ends: one admission again.
    harness.supervisor.task_finished(&tokens[2], TaskOutcome::Succeeded).unwrap();
    assert_eq!(harness.admit_all().len(), 1);
    assert_eq!(harness.supervisor.status().busy_runners, 1);
}

#[test]
fn raising_the_ceiling_applies_at_the_next_admission() {
    let harness = Harness::associated();
    harness.supervisor.tick(5);
    harness.bring_up();
    assert_eq!(harness.admit_all().len(), 1);
    harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    harness.supervisor.tick(5);
    harness.bring_up();
    assert_eq!(harness.admit_all().len(), 2);
    assert_eq!(harness.supervisor.status().busy_runners, 3);
}

#[test]
fn no_double_launch_under_concurrent_calls() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    let manual = harness.supervisor.add_runner("manual", false).unwrap();
    let handles: Vec<_> = (0..32)
        .map(|n| {
            let supervisor = Arc::clone(&harness.supervisor);
            let manual_id = manual.id.clone();
            thread::spawn(move || {
                if n % 2 == 0 {
                    supervisor.tick(10);
                } else {
                    let _ = supervisor.start_runner(&manual_id);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(harness.launcher.duplicate_launches(), 0);
    for runner in harness.supervisor.runners() {
        assert!(harness.launcher.live_for(&runner.id).len() <= 1, "{} has two processes", runner.name);
    }
    // At most the ceiling of automatic runners, plus the manual one.
    assert!(harness.launcher.live().len() <= 5);
    assert_eq!(harness.launcher.live_for(&manual.id).len(), 1);
}

#[test]
fn one_task_per_runner() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(4, None).unwrap();
    harness.supervisor.tick(1);
    harness.bring_up();
    let runner = harness.supervisor.runners().remove(0);
    harness.supervisor.admit(&runner.id, runner.generation).unwrap();
    assert!(harness.supervisor.admit(&runner.id, runner.generation).is_err());
    // A late "ready" report while it claims its task does not give its token back.
    assert!(harness.supervisor.runner_ready(&runner.id, runner.generation).is_err());
    assert_eq!(harness.supervisor.status().busy_runners, 1);
}

#[test]
fn idle_runners_are_stopped_after_the_idle_delay() {
    let harness = Harness::associated();
    harness.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    harness.supervisor.tick(2);
    harness.bring_up();
    assert_eq!(harness.count(RunnerState::Ready), 2);

    // The queue empties. Before the delay: kept warm.
    harness.supervisor.tick(0);
    assert!(harness.launcher.terminating().is_empty());
    harness.advance(59_999);
    harness.supervisor.tick(0);
    assert!(harness.launcher.terminating().is_empty());

    harness.advance(1);
    harness.supervisor.tick(0);
    assert_eq!(harness.count(RunnerState::Stopping), 2);
    harness.complete_stops();
    assert_eq!(harness.count(RunnerState::Stopped), 2);
    assert!(harness.launcher.live().is_empty());
}

#[test]
fn a_crashing_runner_restarts_with_backoff_then_fails() {
    let harness = Harness::associated();
    harness.supervisor.tick(1);
    let runner = harness.supervisor.runners().remove(0);
    harness.bring_up();
    let token = harness.supervisor.admit(&runner.id, runner.generation).unwrap();
    harness.supervisor.task_started(&token, "t1").unwrap();

    // The process dies mid-task: the token comes back and the task is reported lost.
    let process = harness.launcher.live_for(&runner.id).remove(0);
    harness.launcher.end(process.pid);
    harness.supervisor.runner_exited(&runner.id, process.generation, "status 1");
    let status = harness.supervisor.status();
    assert_eq!(status.busy_runners, 0);
    assert!(harness.supervisor.events_since(0).unwrap().iter().any(|e| matches!(
        &e.kind,
        EventKind::TaskFinished { outcome: TaskOutcome::RunnerLost, task_ref: Some(t), .. } if t == "t1"
    )));

    // Not restarted before the backoff (1 s).
    harness.supervisor.tick(1);
    assert_eq!(harness.launcher.spawns(), 1);
    harness.advance(1_000);
    harness.supervisor.tick(1);
    assert_eq!(harness.launcher.spawns(), 2);

    // A late exit report from the old generation is ignored.
    harness.supervisor.runner_exited(&runner.id, process.generation, "late");
    assert_eq!(harness.supervisor.runner(&runner.id).unwrap().state, RunnerState::Starting);

    // Keeps crashing: failed after five in a row, not restarted any more.
    for _ in 0..4 {
        let current = harness.launcher.live_for(&runner.id).remove(0);
        harness.launcher.end(current.pid);
        harness.supervisor.runner_exited(&runner.id, current.generation, "status 1");
        harness.advance(300_000);
        harness.supervisor.tick(1);
    }
    assert_eq!(harness.launcher.spawns(), 5);
    assert!(harness.launcher.live().is_empty(), "no replacement runner beyond the ceiling");
    let failed = harness.supervisor.runner(&runner.id).unwrap();
    assert_eq!(failed.state, RunnerState::Failed);
    assert!(failed.failure_reason.unwrap().contains("5 times"));
}

#[test]
fn stopping_a_busy_runner_needs_force() {
    let harness = Harness::associated();
    harness.supervisor.tick(1);
    harness.bring_up();
    let token = harness.admit_all().remove(0);
    let error = harness.supervisor.stop_runner(&token.runner_id, false).unwrap_err();
    assert_eq!(error.code(), "invalidTransition");
    harness.supervisor.stop_runner(&token.runner_id, true).unwrap();
    harness.complete_stops();
    let runner = harness.supervisor.runner(&token.runner_id).unwrap();
    assert_eq!(runner.state, RunnerState::Stopped);
    assert_eq!(harness.supervisor.status().busy_runners, 0);
    // Stopped by hand: the supervisor does not start it again on its own.
    harness.advance(120_000);
    harness.supervisor.tick(1);
    assert_eq!(harness.supervisor.runner(&token.runner_id).unwrap().state, RunnerState::Stopped);
}

#[test]
fn configuration_changes_are_versioned() {
    let harness = Harness::associated();
    let revision = harness.supervisor.status().config_revision;
    let status = harness.supervisor.set_max_concurrent_tasks(2, Some(revision)).unwrap();
    assert_eq!(status.config_revision, revision + 1);
    // A second client still on the old revision gets a conflict.
    let error = harness.supervisor.set_mode(Mode::Paused, Some(revision)).unwrap_err();
    assert_eq!(error, ServiceError::Conflict { current_revision: revision + 1 });
    assert!(error.retryable());
    assert_eq!(harness.supervisor.status().mode, Mode::Automatic);

    assert_eq!(
        harness.supervisor.set_max_concurrent_tasks(0, None).unwrap_err(),
        ServiceError::LimitOutOfRange { min: 1, max: 8 }
    );
    assert_eq!(
        harness.supervisor.set_max_concurrent_tasks(9, None).unwrap_err(),
        ServiceError::LimitOutOfRange { min: 1, max: 8 }
    );
}

#[test]
fn losing_the_association_blocks_admissions_but_not_running_tasks() {
    let harness = Harness::associated();
    harness.supervisor.tick(1);
    harness.bring_up();
    let token = harness.admit_all().remove(0);
    harness
        .supervisor
        .set_connection(Connection::Unreachable { instance: "test.invalid".into(), reason: "offline".into() });
    assert_eq!(harness.supervisor.status().admission, Admission::Blocked(BlockReason::Unreachable));
    assert_eq!(harness.supervisor.status().active_tasks.len(), 1);
    assert!(harness.launcher.terminating().is_empty());
    harness.supervisor.task_finished(&token, TaskOutcome::Succeeded).unwrap();
    assert!(harness.admit_all().is_empty());
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
