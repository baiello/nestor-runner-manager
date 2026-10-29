//! Restart, validation of the stored state and crash recovery (contract v2 §3.3, §5, S8.2, S8.4).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use common::{options, queue, Harness, SCOPE};
use nestor_runner_core::fake::FakeLauncher;
use nestor_runner_core::model::*;
use nestor_runner_core::persist::{FileStore, Loaded, Store};

fn file_store(dir: &std::path::Path) -> Arc<dyn Store> {
    Arc::new(FileStore::new(dir).unwrap())
}

fn kept_aside(dir: &std::path::Path) -> bool {
    fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().contains("corrupt"))
}

#[test]
fn mode_and_ceiling_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let harness = Harness::with(file_store(dir.path()), Arc::new(FakeLauncher::new()), options());
        harness.supervisor.reconcile();
        harness.associate();
        harness.supervisor.set_max_concurrent_tasks(3, None).unwrap();
        harness.supervisor.set_mode(Mode::Paused, None).unwrap();
    }
    let harness = Harness::with(file_store(dir.path()), Arc::new(FakeLauncher::new()), options());
    let status = harness.supervisor.status();
    assert_eq!(status.mode, Mode::Paused);
    assert_eq!(status.max_concurrent_tasks, 3);
    assert_eq!(status.config_revision, 2);
    assert!(matches!(status.connection, Connection::Associated { .. }));
    assert_eq!(status.admission, Admission::Blocked(BlockReason::Reconciling));
    harness.supervisor.reconcile();
    assert_eq!(harness.supervisor.status().admission, Admission::Paused);
    assert!(status.config_error.is_none());
}

#[test]
fn state_file_is_private_and_holds_no_secret_or_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path()).unwrap();
    let harness = Harness::with(file_store(dir.path()), Arc::new(FakeLauncher::new()), options());
    harness.supervisor.reconcile();
    harness.associate();
    harness.supervisor.redactor().register("nestor_machine_0123456789abcdef");
    harness.round(&queue(1));

    assert_eq!(fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    let text = fs::read_to_string(store.path()).unwrap().to_lowercase();
    for word in ["token", "secret", "credential", "password", "nestor_machine", "limit", "quota", "cpu", "memory"] {
        assert!(!text.contains(word), "state file mentions {word}: {text}");
    }
    assert!(text.contains("starttimeus"), "the process start time is kept to recognize it later");
    assert!(!dir.path().join("state.json.tmp").exists(), "the temporary file is renamed away");
}

#[test]
fn corrupt_configuration_starts_paused_with_one_task() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("state.json"), b"{ not json").unwrap();
    let harness = Harness::with(file_store(dir.path()), Arc::new(FakeLauncher::new()), options());
    harness.supervisor.reconcile();
    harness.associate();
    let status = harness.supervisor.status();
    assert_eq!(status.mode, Mode::Paused);
    assert_eq!(status.max_concurrent_tasks, 1);
    assert_eq!(status.admission, Admission::Paused);
    assert!(status.config_error.as_deref().unwrap().contains("unreadable state file"));
    assert!(kept_aside(dir.path()));
    assert!(matches!(FileStore::new(dir.path()).unwrap().load(), Loaded::Restored(_)));
}

const VALID_RUNNER: &str =
    r#"{"id":"0123456789abcdef0123456789abcdef","name":"runner-1","generation":2,"pid":null,"createdAtMs":0}"#;

fn state(schema: u32, ceiling: u32, runners: &str, connection: &str) -> String {
    format!(
        r#"{{"schemaVersion":{schema},"machineId":"m1","connection":{connection},"mode":"automatic","maxConcurrentTasks":{ceiling},"configRevision":1,"runners":[{runners}]}}"#
    )
}

#[test]
fn any_invalid_value_is_treated_as_corrupt() {
    let not_associated = r#"{"state":"notAssociated"}"#;
    let cases = [
        ("unknown schema", state(99, 2, "", not_associated)),
        ("ceiling of 0", state(2, 0, "", not_associated)),
        ("ceiling above the upper bound", state(2, 9, "", not_associated)),
        ("unknown mode", state(2, 1, "", not_associated).replace("automatic", "turbo")),
        (
            "unknown runner state field",
            state(2, 1, &VALID_RUNNER.replace("\"pid\"", "\"quota\":1,\"pid\""), not_associated),
        ),
        (
            "bad runner id",
            state(2, 1, &VALID_RUNNER.replace("0123456789abcdef0123456789abcdef", "../../etc"), not_associated),
        ),
        ("bad runner name", state(2, 1, &VALID_RUNNER.replace("runner-1", "rm -rf /;"), not_associated)),
        ("pid without start time", state(2, 1, &VALID_RUNNER.replace("\"pid\":null", "\"pid\":42"), not_associated)),
        (
            "bad scope",
            state(2, 1, &VALID_RUNNER.replace("\"pid\":null", "\"pid\":null,\"scope\":\"a b\""), not_associated),
        ),
        (
            "stored association in progress",
            state(
                2,
                1,
                "",
                r#"{"state":"associating","instance":"x","userCode":"A","verificationUri":"u","expiresAtMs":1}"#,
            ),
        ),
        ("bad instance", state(2, 1, "", r#"{"state":"associated","instance":"","sinceMs":1}"#)),
    ];
    for (what, content) in cases {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state.json"), &content).unwrap();
        let harness = Harness::with(file_store(dir.path()), Arc::new(FakeLauncher::new()), options());
        let status = harness.supervisor.status();
        assert_eq!(status.mode, Mode::Paused, "{what}");
        assert_eq!(status.max_concurrent_tasks, 1, "{what}");
        assert!(status.config_error.is_some(), "{what}: the error is visible");
        assert!(kept_aside(dir.path()), "{what}: the file is kept for diagnosis");
    }
    // The valid variant of the same file is accepted, and a schema 1 file still reads.
    for schema in [1, 2] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state.json"), state(schema, 2, VALID_RUNNER, not_associated)).unwrap();
        let harness = Harness::with(file_store(dir.path()), Arc::new(FakeLauncher::new()), options());
        assert!(harness.supervisor.status().config_error.is_none());
        assert_eq!(harness.supervisor.status().max_concurrent_tasks, 2);
    }
}

#[test]
fn a_restarted_service_adopts_live_runners_instead_of_launching_again() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new());
    let first = Harness::with(file_store(dir.path()), launcher.clone(), options());
    first.supervisor.reconcile();
    first.associate();
    first.supervisor.set_max_concurrent_tasks(2, None).unwrap();
    assert_eq!(first.round(&queue(2)).len(), 2);
    // One of the runners dies while the service is down.
    let lost = launcher.live().remove(0);
    drop(first); // the service crashes; its runner processes do not
    launcher.end(lost.pid);

    let second = Harness::with(file_store(dir.path()), launcher.clone(), options());
    // Before reconciliation nothing is launched.
    second.supervisor.tick(&queue(10));
    assert_eq!(launcher.spawns(), 2);
    second.supervisor.reconcile();

    let survivor = launcher.live().remove(0);
    let adopted = second.supervisor.runner(&survivor.runner_id).unwrap();
    assert_eq!(adopted.state, RunnerState::Busy, "an adopted runner counts toward the ceiling");
    assert_eq!(adopted.scope.as_deref(), Some(SCOPE), "its scope was kept");
    assert_eq!(second.supervisor.runner(&lost.runner_id).unwrap().state, RunnerState::Stopped);

    // One free slot in the same scope: one launch, never over the adopted process.
    second.round(&queue(10));
    assert_eq!(launcher.duplicate_launches(), 0);
    assert_eq!(launcher.live_for(&survivor.runner_id).len(), 1);
    assert_eq!(second.supervisor.status().live_runners, 2);

    // The adopted process ends: noticed by polling, its slot is free again.
    launcher.end(survivor.pid);
    second.supervisor.tick(&Queue::empty());
    assert_eq!(second.supervisor.runner(&survivor.runner_id).unwrap().state, RunnerState::Stopped);
    assert!(second
        .supervisor
        .events_since(0)
        .unwrap()
        .iter()
        .any(|e| matches!(&e.kind, EventKind::TaskFinished { outcome: TaskOutcome::Unknown, .. })));
}

#[test]
fn a_reused_pid_is_never_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new());
    let first = Harness::with(file_store(dir.path()), launcher.clone(), options());
    first.supervisor.reconcile();
    first.associate();
    first.round(&queue(1));
    drop(first);
    // Rewrite the stored start time: same pid, another process.
    let path = dir.path().join("state.json");
    let mut stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let start = stored["runners"][0]["startTimeUs"].as_u64().unwrap();
    stored["runners"][0]["startTimeUs"] = (start + 1).into();
    fs::write(&path, stored.to_string()).unwrap();

    let second = Harness::with(file_store(dir.path()), launcher.clone(), options());
    second.supervisor.reconcile();
    let runner = second.supervisor.runners().remove(0);
    assert_eq!(runner.state, RunnerState::Stopped, "not adopted");
    assert!(launcher.terminated().is_empty() && launcher.drained().is_empty(), "and never signalled");
}

#[test]
fn an_adopted_runner_without_a_known_scope_blocks_other_launches() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new());
    let first = Harness::with(file_store(dir.path()), launcher.clone(), options());
    first.supervisor.reconcile();
    first.associate();
    first.supervisor.set_max_concurrent_tasks(3, None).unwrap();
    first.supervisor.tick(&queue(1));
    drop(first);
    let path = dir.path().join("state.json");
    let mut stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    stored["runners"][0]["scope"] = serde_json::Value::Null;
    fs::write(&path, stored.to_string()).unwrap();

    let second = Harness::with(file_store(dir.path()), launcher.clone(), options());
    second.supervisor.reconcile();
    second.supervisor.tick(&queue(5));
    assert_eq!(launcher.spawns(), 1, "scope unknown: no other task starts next to it");
    assert!(second.supervisor.status().active_scope.is_none());
}
