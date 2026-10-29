//! Restart and crash recovery (contract KCA-717 §5 and §11.3).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use common::{options, Harness};
use nestor_runner_core::fake::FakeLauncher;
use nestor_runner_core::model::*;
use nestor_runner_core::persist::{FileStore, Loaded, Store};

fn file_store(dir: &std::path::Path) -> Arc<dyn Store> {
    Arc::new(FileStore::new(dir).unwrap())
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
}

#[test]
fn state_file_is_private_and_holds_no_secret_or_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path()).unwrap();
    let harness =
        Harness::with(Arc::new(FileStore::new(dir.path()).unwrap()), Arc::new(FakeLauncher::new()), options());
    harness.supervisor.reconcile();
    harness.associate();
    harness.supervisor.add_runner("studio", true).unwrap();

    let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    let text = fs::read_to_string(store.path()).unwrap().to_lowercase();
    for word in ["token", "secret", "credential", "password", "limit", "quota", "cpu", "memory"] {
        assert!(!text.contains(word), "state file mentions {word}: {text}");
    }
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
    // The bad file is kept for diagnosis and a safe one written instead.
    let kept = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
    assert!(kept);
    assert!(matches!(FileStore::new(dir.path()).unwrap().load(), Loaded::Restored(_)));
}

#[test]
fn unknown_schema_or_zero_ceiling_is_treated_as_corrupt() {
    for content in [
        r#"{"schemaVersion":99,"machineId":null,"connection":{"state":"notAssociated"},"mode":"automatic","maxConcurrentTasks":2,"configRevision":1,"runners":[]}"#,
        r#"{"schemaVersion":1,"machineId":null,"connection":{"state":"notAssociated"},"mode":"automatic","maxConcurrentTasks":0,"configRevision":1,"runners":[]}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("state.json"), content).unwrap();
        assert!(matches!(FileStore::new(dir.path()).unwrap().load(), Loaded::Corrupt { .. }));
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
    first.supervisor.tick(2);
    first.bring_up();
    assert_eq!(first.admit_all().len(), 2);
    // One of the runners dies while the service is down.
    let lost = launcher.live().remove(0);
    drop(first); // the service crashes; its runner processes do not
    launcher.end(lost.pid);

    let second = Harness::with(file_store(dir.path()), launcher.clone(), options());
    // Before reconciliation nothing is started or admitted.
    second.supervisor.tick(10);
    assert_eq!(launcher.spawns(), 2);
    second.supervisor.reconcile();

    let survivor = launcher.live().remove(0);
    let adopted = second.supervisor.runner(&survivor.runner_id).unwrap();
    assert_eq!(adopted.state, RunnerState::Busy, "an adopted runner counts as busy until it reports idle");
    assert_eq!(second.supervisor.runner(&lost.runner_id).unwrap().state, RunnerState::Stopped);
    assert_eq!(second.supervisor.status().busy_runners, 1);

    // The dead one is started again (a new generation); the adopted one is not launched twice.
    second.supervisor.tick(10);
    assert_eq!(launcher.duplicate_launches(), 0);
    assert_eq!(launcher.live_for(&survivor.runner_id).len(), 1);
    assert_eq!(launcher.live_for(&lost.runner_id).len(), 1);

    // The adopted runner reports idle: it may take a task again, within the ceiling.
    second.supervisor.runner_ready(&survivor.runner_id, survivor.generation).unwrap();
    second.bring_up();
    assert_eq!(second.admit_all().len(), 2);
    assert_eq!(second.supervisor.status().busy_runners, 2);
}
